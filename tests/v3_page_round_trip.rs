// v3 page-level codec round-trip tests (M12 Phase 2 first slice).
//
// Pins the cell + page + RaptorQ pipeline as one unit:
//   bytes → encode_pages → bitmaps → decode_pages → bytes
//
// Phase 2 first slice — synthetic round-trip only. The bitmaps live
// in memory, parsed pixel-perfectly by a decoder that's told the
// exact geometry. Real-scanner round-trip (with calibration, finder
// patterns, noise tolerance) is the next slice.

use ampaper::v3::{PageBitmap, PageGeometry, decode_pages, encode_pages, pad_with_white};

fn letter_geometry() -> PageGeometry {
    // 12×18 cells = 216 cells/page. At RAPTORQ_MTU=124 → T=120, each
    // data cell carries ~120 bytes, so one page holds ~25 KB after
    // anchor overhead. 12×32×6 = 2304 px wide, 18×32×6 = 3456 px tall
    // — well within Letter-at-600-DPI (5100×6600).
    PageGeometry { nx: 12, ny: 18, pixels_per_dot: 6 }
}

fn small_geometry() -> PageGeometry {
    // 4×4 cells = 16 cells/page. 1 anchor + 15 data slots ≈ 1.8 KB
    // per page. Useful for forcing multi-page output on small
    // payloads in tests without burning gigabytes of RAM rendering
    // big bitmaps.
    PageGeometry { nx: 4, ny: 4, pixels_per_dot: 1 }
}

#[test]
fn round_trips_short_payload_one_page() {
    let plaintext = b"Phase 2 ships the cell + page layer.";
    let pages = encode_pages(plaintext, &letter_geometry(), 5).unwrap();
    assert_eq!(pages.len(), 1, "short payload should fit on one page");
    let recovered = decode_pages(&pages, &letter_geometry()).unwrap();
    assert_eq!(recovered, plaintext);
}

#[test]
fn round_trips_payload_that_spans_three_pages() {
    let plaintext: Vec<u8> = (0u32..5000)
        .map(|i| (i.wrapping_mul(13).wrapping_add(37) & 0xFF) as u8)
        .collect();
    let geom = small_geometry();
    let pages = encode_pages(&plaintext, &geom, 10).unwrap();
    assert!(
        pages.len() >= 3,
        "5 KB at 15 data slots/page must span ≥ 3 pages, got {}",
        pages.len()
    );
    let recovered = decode_pages(&pages, &geom).unwrap();
    assert_eq!(recovered, plaintext);
}

#[test]
fn round_trips_with_letter_scale_geometry() {
    // Realistic geometry — 6 device pixels per data dot (matches
    // PB-1.10's calibration of 600-DPI printer × 100-dot/inch
    // encoding). Heavier on RAM than scale=1 but the most realistic
    // synthetic test before real scanner round-trip.
    let plaintext: Vec<u8> = (0u32..2048)
        .map(|i| (i.wrapping_mul(7).wrapping_add(11) & 0xFF) as u8)
        .collect();
    let geom = letter_geometry();
    let pages = encode_pages(&plaintext, &geom, 5).unwrap();
    assert_eq!(pages.len(), 1, "2 KB should fit on one Letter-scale page");

    // Each bitmap is exactly geometry.pixel_width × pixel_height.
    assert_eq!(pages[0].width, geom.pixel_width());
    assert_eq!(pages[0].height, geom.pixel_height());
    assert_eq!(pages[0].pixels.len(), (geom.pixel_width() * geom.pixel_height()) as usize);

    let recovered = decode_pages(&pages, &geom).unwrap();
    assert_eq!(recovered, plaintext);
}

#[test]
fn round_trips_one_megabyte() {
    // The big stress test — 1 MB exercises RaptorQ's multi-source-
    // block path AND many-page output. ~150 pages at small_geometry.
    let mut plaintext = Vec::with_capacity(1_048_576);
    let mut x: u32 = 0xCAFE_BABE;
    for _ in 0..1_048_576 {
        x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
        plaintext.push((x >> 16) as u8);
    }
    let geom = small_geometry();
    let pages = encode_pages(&plaintext, &geom, 50).unwrap();
    let recovered = decode_pages(&pages, &geom).unwrap();
    assert_eq!(recovered, plaintext);
}

#[test]
fn anchor_loss_on_first_pages_recovers_via_later_anchor() {
    // Every page carries an anchor — the decoder picks the first
    // valid one it finds. This pins that property: corrupt the
    // anchors on the first half of the pages and confirm decode
    // still works using anchors from the surviving pages.
    let plaintext: Vec<u8> = (0u32..4000)
        .map(|i| (i.wrapping_mul(17) & 0xFF) as u8)
        .collect();
    let geom = small_geometry();
    let mut pages = encode_pages(&plaintext, &geom, 30).unwrap();
    assert!(pages.len() >= 4, "test setup needs ≥ 4 pages");

    // Corrupt the anchor cell on the first half of the pages.
    // After Phase 2.5, the anchor sits at the top-left of the
    // DATA GRID, which is offset by FINDER_MARGIN_DOTS (= 8) from
    // the bitmap origin to leave room for the corner finder
    // pattern. At pixels_per_dot=1 the anchor cell occupies pixels
    // [(8, 8), (40, 40)).
    let half = pages.len() / 2;
    for page in pages.iter_mut().take(half) {
        for y in 8..40 {
            let row_start = (y * page.width) as usize;
            for x in 8..40 {
                page.pixels[row_start + x] = ampaper::v3::page::WHITE;
            }
        }
    }

    let recovered = decode_pages(&pages, &geom).unwrap();
    assert_eq!(recovered, plaintext);
}

#[test]
fn dropped_pages_recover_via_rateless_ecc() {
    // Drop the second page entirely — the surviving pages must
    // carry enough RaptorQ packets (source + repair) for the
    // decoder to converge. Pins the rateless-ECC promise at the
    // page level.
    let plaintext: Vec<u8> = (0u32..3000)
        .map(|i| (i.wrapping_mul(23) & 0xFF) as u8)
        .collect();
    let geom = small_geometry();
    let pages = encode_pages(&plaintext, &geom, 30).unwrap();
    assert!(pages.len() >= 3, "test setup needs ≥ 3 pages");

    let mut survivors: Vec<PageBitmap> = pages.to_vec();
    let _dropped = survivors.remove(1);

    let recovered = decode_pages(&survivors, &geom).unwrap();
    assert_eq!(recovered, plaintext);
}

#[test]
fn empty_input_rejected() {
    let err = encode_pages(b"", &small_geometry(), 5).unwrap_err();
    assert!(matches!(err, ampaper::v3::PageEncodeError::EmptyInput));
}

#[test]
fn pages_decode_when_embedded_in_larger_white_canvas() {
    // The Phase 2.5 unlock: pages don't have to be cropped to the
    // exact rendered size. A flatbed scanner captures the entire
    // sheet of paper; the actual data area sits in the middle with
    // arbitrary white margins around it. The QR-style corner
    // finders make the page locatable inside that larger canvas.
    let plaintext: Vec<u8> = (0u32..2000)
        .map(|i| (i.wrapping_mul(19).wrapping_add(5) & 0xFF) as u8)
        .collect();
    let geom = small_geometry();
    let pages = encode_pages(&plaintext, &geom, 10).unwrap();

    // Pad each page asymmetrically — different paddings on each
    // edge, simulating real scanner behavior where the page is
    // rarely centered exactly.
    let padded: Vec<PageBitmap> = pages
        .iter()
        .enumerate()
        .map(|(i, page)| {
            // Rotate paddings per page so different pages get
            // different layouts — proves the detector isn't
            // exploiting any fixed-position assumption.
            let rotation = i as u32 * 7;
            pad_with_white(
                page,
                40 + rotation,
                30 + rotation * 2,
                60 - rotation,
                25 + rotation,
            )
        })
        .collect();

    let recovered = decode_pages(&padded, &geom).unwrap();
    assert_eq!(recovered, plaintext);
}

#[test]
fn pages_decode_with_modest_scale_drift() {
    // Phase 2.5 derives `pixels_per_dot` from the measured finder
    // distances, not from the geometry's stored value. So a page
    // rendered at scale=4 then upscaled to scale=5 (simulating a
    // print-scan resolution mismatch) should still decode — the
    // parser sees larger finder distances and adjusts dx/dy
    // accordingly. Without this property the encode-time DPI and
    // decode-time DPI would have to match exactly.
    let plaintext = b"Phase 2.5 tolerates scale drift between print and scan.";
    let encode_geom = PageGeometry { nx: 4, ny: 4, pixels_per_dot: 4 };
    let pages = encode_pages(plaintext, &encode_geom, 5).unwrap();
    assert_eq!(pages.len(), 1);

    // Upscale the bitmap by 1.25× via nearest-neighbor: every pixel
    // becomes a 5×4 block (alternating to get the 1.25× ratio).
    // This simulates a print-scan path where the scanner captures
    // at 1.25× the encode resolution.
    let src = &pages[0];
    let new_w = src.width * 5 / 4;
    let new_h = src.height * 5 / 4;
    let mut upscaled = vec![ampaper::v3::page::WHITE; (new_w * new_h) as usize];
    for ny in 0..new_h {
        let sy = (ny * 4 / 5).min(src.height - 1);
        for nx in 0..new_w {
            let sx = (nx * 4 / 5).min(src.width - 1);
            upscaled[(ny * new_w + nx) as usize] =
                src.pixels[(sy * src.width + sx) as usize];
        }
    }
    let scaled_page = PageBitmap {
        pixels: upscaled,
        width: new_w,
        height: new_h,
    };

    // Decode using the SAME geometry the encoder used. The parser
    // measures the actual scale from finder positions and uses it
    // for sampling; the geometry's `pixels_per_dot` field is now
    // a hint, not a hard requirement.
    let recovered = decode_pages(&[scaled_page], &encode_geom).unwrap();
    assert_eq!(recovered, plaintext);
}

#[test]
fn decoder_rejects_geometry_mismatch() {
    let plaintext = b"hello";
    let encode_geom = PageGeometry { nx: 4, ny: 4, pixels_per_dot: 1 };
    let pages = encode_pages(plaintext, &encode_geom, 5).unwrap();

    // Decode with the WRONG geometry — bitmap dimensions don't match.
    let wrong_geom = PageGeometry { nx: 8, ny: 8, pixels_per_dot: 1 };
    let err = decode_pages(&pages, &wrong_geom).unwrap_err();
    assert!(
        matches!(err, ampaper::v3::PageDecodeError::PageParse(_)),
        "expected PageParse, got {err:?}"
    );
}
