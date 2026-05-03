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

/// Standard LCG (Numerical Recipes glibc parameters) producing
/// high-entropy bytes — defeats zstd's compression layer so
/// per-page-count assertions in these tests stay deterministic.
/// Without this, low-entropy input (e.g. a cyclic mod-256
/// pattern) collapses to a single page after Phase 3 compression.
fn lcg_bytes(count: u32, seed: u32) -> Vec<u8> {
    let mut x = seed;
    (0..count)
        .map(|_| {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (x >> 16) as u8
        })
        .collect()
}

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
    // High-entropy input — zstd compression in encode_pages can't
    // shrink LCG output below ~95% of original, so the page-count
    // assertion stays valid.
    let plaintext = lcg_bytes(5000, 0xDEAD_BEEF);
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
    let plaintext = lcg_bytes(4000, 0xC0DE_C0DE);
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
    let plaintext = lcg_bytes(3000, 0xBEEF_BEEF);
    let geom = small_geometry();
    let pages = encode_pages(&plaintext, &geom, 30).unwrap();
    assert!(pages.len() >= 3, "test setup needs ≥ 3 pages");

    let mut survivors: Vec<PageBitmap> = pages.to_vec();
    let _dropped = survivors.remove(1);

    let recovered = decode_pages(&survivors, &geom).unwrap();
    assert_eq!(recovered, plaintext);
}

#[test]
fn highly_compressible_payload_fits_on_one_page() {
    // The Phase 3 compression payoff: 100 KB of repetitive text
    // would normally span ~5 pages at small_geometry without
    // compression. With zstd at level 22 the bytes shrink by ~50×
    // and the encoded output fits on a single page. Pins the
    // encoder's "use compression when it helps" decision.
    let mut plaintext = Vec::with_capacity(100_000);
    let line = b"PaperBack 1.10 archives bytes onto paper. ampaper v3 picks up where it left off. ";
    while plaintext.len() < 100_000 {
        plaintext.extend_from_slice(line);
    }
    plaintext.truncate(100_000);

    let geom = small_geometry();
    let pages = encode_pages(&plaintext, &geom, 5).unwrap();
    assert_eq!(
        pages.len(),
        1,
        "100 KB of repetitive text should compress to <1 page, got {}",
        pages.len()
    );
    let recovered = decode_pages(&pages, &geom).unwrap();
    assert_eq!(recovered, plaintext);
}

#[test]
fn already_compressed_input_falls_back_to_raw() {
    // Truly random / high-entropy input that zstd can't shrink.
    // The encoder must detect this case and store raw bytes
    // (compression flag = None on the anchor) rather than waste
    // ~14 bytes of zstd frame overhead per page. Page count
    // should match the raw-encoding count.
    let plaintext = lcg_bytes(2000, 0xABBA_ABBA);
    let geom = small_geometry();
    let pages = encode_pages(&plaintext, &geom, 5).unwrap();

    // Decode anchor of page 0 to confirm compression == None.
    let cells = ampaper::v3::page::parse_page(&pages[0], &geom).unwrap();
    let anchor = match ampaper::v3::cell::decode_cell(&cells[0]).unwrap() {
        ampaper::v3::cell::DecodedCell::Anchor(a) => a,
        _ => panic!("cell 0 must be an anchor"),
    };
    assert_eq!(
        anchor.compression,
        ampaper::v3::cell::Compression::None,
        "incompressible input should ship raw, not pay zstd's overhead"
    );

    let recovered = decode_pages(&pages, &geom).unwrap();
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

/// Rotate a bitmap by `angle_deg` clockwise into a larger
/// canvas (with white margins) using nearest-neighbor sampling.
/// Used by Phase 2.5b rotation tests — nearest-neighbor matches
/// the high-contrast bimodal nature of v3 page bitmaps without
/// introducing intermediate gray levels that the fixed-128
/// threshold would have to cope with.
fn rotate_into_larger_canvas(src: &PageBitmap, angle_deg: f32) -> PageBitmap {
    let theta = angle_deg.to_radians();
    let cos_t = theta.cos();
    let sin_t = theta.sin();
    // New canvas: enough margin that rotation doesn't clip the page.
    // A square that fits the rotated rectangle has side = w·|cos| + h·|sin|.
    let w = src.width as f32;
    let h = src.height as f32;
    let new_w = (w * cos_t.abs() + h * sin_t.abs()).ceil() as u32 + 20;
    let new_h = (w * sin_t.abs() + h * cos_t.abs()).ceil() as u32 + 20;
    let mut pixels = vec![ampaper::v3::page::WHITE; (new_w * new_h) as usize];
    let cx_old = w / 2.0;
    let cy_old = h / 2.0;
    let cx_new = new_w as f32 / 2.0;
    let cy_new = new_h as f32 / 2.0;
    for ny in 0..new_h {
        for nx in 0..new_w {
            let dx = nx as f32 - cx_new;
            let dy = ny as f32 - cy_new;
            // Inverse rotation: rotate (dx, dy) by -theta to find source pixel.
            let ox = cos_t * dx + sin_t * dy + cx_old;
            let oy = -sin_t * dx + cos_t * dy + cy_old;
            let oxi = ox as i64;
            let oyi = oy as i64;
            if oxi < 0 || oyi < 0 || oxi >= src.width as i64 || oyi >= src.height as i64 {
                continue;
            }
            pixels[(ny * new_w + nx) as usize] =
                src.pixels[(oyi as usize) * (src.width as usize) + oxi as usize];
        }
    }
    PageBitmap { pixels, width: new_w, height: new_h }
}

#[test]
fn pages_decode_when_rotated_a_few_degrees() {
    // The Phase 2.5b unlock: pages tilted by a small angle (the
    // typical condition of any flatbed scan that's not perfectly
    // straight) still decode. The decoder uses the three corner
    // finders to compute an affine transform from page-dot space
    // to bitmap-pixel space, so rotation, modest skew, and scale
    // drift all fall out of the same math.
    let plaintext: Vec<u8> = (0u32..1500)
        .map(|i| (i.wrapping_mul(11).wrapping_add(3) & 0xFF) as u8)
        .collect();
    // Use a generous pixels_per_dot so the rotation's
    // nearest-neighbor sampling artefacts don't blur the dot
    // pattern past the threshold detector. Real high-DPI scans
    // give us this same property.
    let geom = PageGeometry { nx: 5, ny: 5, pixels_per_dot: 6 };
    let pages = encode_pages(&plaintext, &geom, 10).unwrap();

    // Try a few small rotation angles. ±5° is the typical
    // worst-case for a flatbed scan when the page isn't aligned.
    for angle in [-5.0, -2.0, 1.5, 4.0] {
        let rotated: Vec<PageBitmap> =
            pages.iter().map(|p| rotate_into_larger_canvas(p, angle)).collect();
        let recovered = decode_pages(&rotated, &geom)
            .unwrap_or_else(|e| panic!("decode at {angle}° failed: {e}"));
        assert_eq!(
            recovered, plaintext,
            "round-trip at {angle}° rotation must recover bytes exactly"
        );
    }
}

/// Lighten every pixel by `fade_amount` (saturating at 255) to
/// simulate paper fade or scanner gamma drift. Used by the Phase
/// 2.5c Otsu test — if `fade_amount` exceeds 128, every "black"
/// pixel value sits ABOVE the old fixed-128 threshold, and a
/// non-adaptive threshold would silently lose all data.
fn fade_pixels(bitmap: &PageBitmap, fade_amount: u8) -> PageBitmap {
    let mut pixels = bitmap.pixels.clone();
    for p in pixels.iter_mut() {
        *p = p.saturating_add(fade_amount);
    }
    PageBitmap { pixels, width: bitmap.width, height: bitmap.height }
}

/// Bilinear rotation into a larger canvas. Unlike the
/// nearest-neighbor rotate used by the Phase 2.5b tests, bilinear
/// interpolation introduces gray pixels at black/white transitions
/// — closer to how a real scanner's anti-aliased capture looks.
/// Phase 2.5c's 5-point sub-pixel averaging is the load-bearing
/// upgrade for handling this.
fn rotate_bilinear_into_larger_canvas(src: &PageBitmap, angle_deg: f32) -> PageBitmap {
    let theta = angle_deg.to_radians();
    let cos_t = theta.cos();
    let sin_t = theta.sin();
    let w = src.width as f32;
    let h = src.height as f32;
    let new_w = (w * cos_t.abs() + h * sin_t.abs()).ceil() as u32 + 20;
    let new_h = (w * sin_t.abs() + h * cos_t.abs()).ceil() as u32 + 20;
    let mut pixels = vec![ampaper::v3::page::WHITE; (new_w * new_h) as usize];
    let cx_old = w / 2.0;
    let cy_old = h / 2.0;
    let cx_new = new_w as f32 / 2.0;
    let cy_new = new_h as f32 / 2.0;
    for ny in 0..new_h {
        for nx in 0..new_w {
            let dx = nx as f32 - cx_new;
            let dy = ny as f32 - cy_new;
            let ox = cos_t * dx + sin_t * dy + cx_old;
            let oy = -sin_t * dx + cos_t * dy + cy_old;
            // Bilinear sample at (ox, oy) from src.
            if ox < 0.0 || oy < 0.0 || ox >= w - 1.0 || oy >= h - 1.0 {
                continue;
            }
            let x0 = ox as u32;
            let y0 = oy as u32;
            let fx = ox - x0 as f32;
            let fy = oy - y0 as f32;
            let stride = src.width as usize;
            let p00 = src.pixels[(y0 as usize) * stride + x0 as usize] as f32;
            let p10 = src.pixels[(y0 as usize) * stride + x0 as usize + 1] as f32;
            let p01 = src.pixels[(y0 as usize + 1) * stride + x0 as usize] as f32;
            let p11 = src.pixels[(y0 as usize + 1) * stride + x0 as usize + 1] as f32;
            let blended = p00 * (1.0 - fx) * (1.0 - fy)
                + p10 * fx * (1.0 - fy)
                + p01 * (1.0 - fx) * fy
                + p11 * fx * fy;
            pixels[(ny * new_w + nx) as usize] = blended.round().clamp(0.0, 255.0) as u8;
        }
    }
    PageBitmap { pixels, width: new_w, height: new_h }
}

/// Add deterministic pseudo-Gaussian noise with the given
/// standard deviation (in pixel units) to every pixel. Uses a
/// linear-congruential RNG seeded by pixel position so the test
/// is bit-reproducible. Approximates Gaussian via the sum of two
/// uniform draws (central limit theorem with N=2 is very rough,
/// but adequate for "is the parse path noise-tolerant" testing).
fn add_pseudo_noise(bitmap: &PageBitmap, std_dev: f32) -> PageBitmap {
    let mut pixels = bitmap.pixels.clone();
    let w = bitmap.width as usize;
    for (i, p) in pixels.iter_mut().enumerate() {
        // Deterministic per-position seed.
        let seed = (i as u32).wrapping_mul(2_654_435_761).wrapping_add(0xDEAD_BEEF);
        let r1 = ((seed >> 16) & 0xFFFF) as f32 / 65536.0; // [0, 1)
        let r2 = ((seed.wrapping_mul(48271)) >> 16 & 0xFFFF) as f32 / 65536.0;
        // Two uniform draws, sum minus 1 → roughly mean=0, scaled to std_dev.
        let n = (r1 + r2 - 1.0) * std_dev * 2.0;
        let v = (*p as f32 + n).round().clamp(0.0, 255.0);
        *p = v as u8;
        let _ = w; // silence lint; w is unused but useful for later 2D-noise extensions
    }
    PageBitmap { pixels, width: bitmap.width, height: bitmap.height }
}

#[test]
fn pages_decode_through_paper_fade() {
    // Phase 2.5c Otsu test. Lighten every pixel by 100 — the old
    // fixed-128 threshold would now treat (former-black) pixels
    // of value 100 as "white" and lose all data. Otsu adapts to
    // the shifted histogram and picks a threshold around 200.
    let plaintext = b"Phase 2.5c handles paper fade via Otsu.".to_vec();
    let geom = PageGeometry { nx: 4, ny: 4, pixels_per_dot: 4 };
    let pages = encode_pages(&plaintext, &geom, 5).unwrap();
    let faded: Vec<PageBitmap> = pages.iter().map(|p| fade_pixels(p, 100)).collect();
    let recovered = decode_pages(&faded, &geom).unwrap();
    assert_eq!(recovered, plaintext);
}

#[test]
fn pages_decode_through_bilinear_rotation() {
    // Phase 2.5c sub-pixel sampling test. Bilinear rotation
    // introduces gray pixels at every black/white transition;
    // single-pixel sampling at the geometric center can land
    // on one of those grays and read the wrong bit. The 5-point
    // average over each dot's footprint smooths past these
    // edge artefacts.
    let plaintext = b"Phase 2.5c handles bilinear-interpolated rotation.".to_vec();
    let geom = PageGeometry { nx: 4, ny: 4, pixels_per_dot: 6 };
    let pages = encode_pages(&plaintext, &geom, 5).unwrap();
    let rotated: Vec<PageBitmap> =
        pages.iter().map(|p| rotate_bilinear_into_larger_canvas(p, 4.0)).collect();
    let recovered = decode_pages(&rotated, &geom).unwrap();
    assert_eq!(recovered, plaintext);
}

#[test]
fn pages_decode_through_combined_real_scanner_distortions() {
    // The Phase 2.5c finale: combine all three distortions a
    // real flatbed scanner introduces — paper fade (Otsu), small
    // rotation with bilinear interpolation (sub-pixel sampling),
    // and additive sensor noise (both helping). If this round-
    // trips, the codec is finally ready for actual print + scan
    // experiments. Up to this slice the parse path was synthetic-
    // only; this test pins it as scanner-grade.
    let plaintext: Vec<u8> = (0u32..1500)
        .map(|i| (i.wrapping_mul(7).wrapping_add(11) & 0xFF) as u8)
        .collect();
    let geom = PageGeometry { nx: 5, ny: 5, pixels_per_dot: 6 };
    let pages = encode_pages(&plaintext, &geom, 10).unwrap();

    let processed: Vec<PageBitmap> = pages
        .iter()
        .map(|p| {
            // 1. Bilinear rotation by 3°.
            let rotated = rotate_bilinear_into_larger_canvas(p, 3.0);
            // 2. Paper fade — lighten by 60.
            let faded = fade_pixels(&rotated, 60);
            // 3. Sensor noise with σ ≈ 12.
            add_pseudo_noise(&faded, 12.0)
        })
        .collect();

    let recovered = decode_pages(&processed, &geom).unwrap();
    assert_eq!(recovered, plaintext);
}

#[test]
fn pages_decode_when_rotated_and_offset() {
    // Combine Phase 2.5a (offset) and Phase 2.5b (rotation): pad
    // the rendered page with asymmetric white margins, then
    // rotate the whole thing. Both transforms should compose
    // through the affine fit.
    let plaintext = b"Phase 2.5b composes with Phase 2.5a's offset handling.".to_vec();
    let geom = PageGeometry { nx: 4, ny: 4, pixels_per_dot: 5 };
    let pages = encode_pages(&plaintext, &geom, 5).unwrap();
    assert_eq!(pages.len(), 1);

    let padded = pad_with_white(&pages[0], 30, 50, 70, 25);
    let rotated = rotate_into_larger_canvas(&padded, 3.5);

    let recovered = decode_pages(&[rotated], &geom).unwrap();
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
