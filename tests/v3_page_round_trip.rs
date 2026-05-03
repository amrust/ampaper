// v3 page-level codec round-trip tests (M12 Phase 2 first slice).
//
// Pins the cell + page + RaptorQ pipeline as one unit:
//   bytes → encode_pages → bitmaps → decode_pages → bytes
//
// Phase 2 first slice — synthetic round-trip only. The bitmaps live
// in memory, parsed pixel-perfectly by a decoder that's told the
// exact geometry. Real-scanner round-trip (with calibration, finder
// patterns, noise tolerance) is the next slice.

use ampaper::v3::{PageBitmap, PageGeometry, decode_pages, encode_pages};

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
    // Anchor sits at cell 0, top-left of the bitmap, occupying
    // 32×32 dot pixels at scale=1.
    let half = pages.len() / 2;
    for page in pages.iter_mut().take(half) {
        // Smash the anchor's pixel block to all-white — the parser
        // will read all-zero cell bytes there, fail CRC, and skip.
        for y in 0..32 {
            let row_start = (y * page.width) as usize;
            for x in 0..32 {
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
