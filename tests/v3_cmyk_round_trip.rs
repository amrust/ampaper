// v3 CMY codec integration tests (M12 / Phase 6).
//
// Pin the 3-channel codec end-to-end: synthetic round-trip,
// density check (multi-page on high-entropy data only), and
// composability with the offset/larger-canvas behavior the
// underlying B&W codec gets through Phase 2.5a.
//
// Phase 6 first slice — synthetic only. Real-paper validation
// (CMY ink fade, scanner color separation, calibration) is
// the next slice.

use ampaper::v3::{
    CmyDecodeError, PageGeometry, RgbPageBitmap, decode_pages_cmyk, encode_pages_cmyk,
};

fn lcg_bytes(count: u32, seed: u32) -> Vec<u8> {
    let mut x = seed;
    (0..count)
        .map(|_| {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (x >> 16) as u8
        })
        .collect()
}

fn small_geometry() -> PageGeometry {
    PageGeometry { nx: 5, ny: 5, pixels_per_dot: 1 }
}

#[test]
fn round_trips_short_payload() {
    let plaintext = b"Phase 6 ships color: 3 bits per dot, 3x density.";
    let pages = encode_pages_cmyk(plaintext, &small_geometry(), 25).unwrap();
    assert_eq!(pages.len(), 1, "short payload should fit on 1 CMY page");
    let recovered = decode_pages_cmyk(&pages, &small_geometry()).unwrap();
    assert_eq!(recovered, plaintext);
}

#[test]
fn round_trips_medium_high_entropy_payload() {
    // Enough data that the 3-channel density gain shows up in
    // page count (must span ≥ 2 pages on small_geometry).
    let plaintext = lcg_bytes(15_000, 0xFADE_FADE);
    let pages = encode_pages_cmyk(&plaintext, &small_geometry(), 25).unwrap();
    assert!(pages.len() >= 2, "expected ≥ 2 pages for 15 KB, got {}", pages.len());
    let recovered = decode_pages_cmyk(&pages, &small_geometry()).unwrap();
    assert_eq!(recovered, plaintext);
}

#[test]
fn round_trips_one_megabyte_high_entropy() {
    // Stress test: 1 MB pseudo-random. Exercises RaptorQ's
    // multi-source-block codepath AND the 3-channel packet
    // distribution at scale.
    let plaintext = lcg_bytes(1_048_576, 0xDEAD_C0DE);
    let pages = encode_pages_cmyk(&plaintext, &small_geometry(), 25).unwrap();
    let recovered = decode_pages_cmyk(&pages, &small_geometry()).unwrap();
    assert_eq!(recovered, plaintext);
}

#[test]
fn density_gain_versus_bw() {
    // For the same input + geometry, CMY should produce ~1/3
    // the page count of B&W. With a 6 KB high-entropy payload
    // on small_geometry (5×5 = 25 cells/page, 24 data slots/page
    // for B&W → ~6 pages; 72 data slots for CMY → ~2 pages).
    let plaintext = lcg_bytes(6_000, 0xCAFE_F00D);
    let bw_pages = ampaper::v3::encode_pages(&plaintext, &small_geometry(), 25).unwrap();
    let cmy_pages = encode_pages_cmyk(&plaintext, &small_geometry(), 25).unwrap();
    assert!(
        cmy_pages.len() < bw_pages.len(),
        "CMY should fit in fewer pages than B&W (CMY={}, BW={})",
        cmy_pages.len(),
        bw_pages.len()
    );
    // Sanity: the 3× capacity factor should hold roughly.
    // Allow a wide margin (1.5×-4× ratio) to absorb the constant
    // overhead per page (anchors, finder margins).
    let ratio = bw_pages.len() as f32 / cmy_pages.len() as f32;
    assert!(
        ratio >= 1.5,
        "CMY density gain too small: BW {} pages / CMY {} pages = {ratio:.2}",
        bw_pages.len(),
        cmy_pages.len()
    );
}

#[test]
fn empty_input_rejected() {
    let err = encode_pages_cmyk(b"", &small_geometry(), 25).unwrap_err();
    assert!(matches!(err, ampaper::v3::CmyEncodeError::EmptyInput));
}

#[test]
fn highly_compressible_payload_fits_on_one_page() {
    // 200 KB of repetitive text → after zstd ~few KB → 1 CMY page
    // even at small_geometry. Pins that the compression layer
    // works for CMY encode just like B&W.
    let mut plaintext = Vec::with_capacity(200_000);
    let line = b"The quick brown fox jumps over the lazy dog. ";
    while plaintext.len() < 200_000 {
        plaintext.extend_from_slice(line);
    }
    plaintext.truncate(200_000);
    let pages = encode_pages_cmyk(&plaintext, &small_geometry(), 25).unwrap();
    assert_eq!(
        pages.len(),
        1,
        "200 KB repetitive text should fit on 1 CMY page after zstd, got {}",
        pages.len()
    );
    let recovered = decode_pages_cmyk(&pages, &small_geometry()).unwrap();
    assert_eq!(recovered, plaintext);
}

#[test]
fn yellow_channel_loss_recovers_via_pooled_packets() {
    // Phase 6's resilience promise: each color channel carries a
    // disjoint set of RaptorQ packets, but the decoder pools them
    // ALL together. So if yellow ink fades catastrophically (the
    // common real-paper failure mode for archived CMYK prints),
    // the surviving 2 channels should still carry > K packets
    // and the decode succeeds.
    //
    // Simulate yellow loss by stomping the B channel of every
    // pixel back to 255 (no yellow ink) before decoding.
    let plaintext = lcg_bytes(2_000, 0xBABE_BEEF);
    // Crank the repair budget so 2-of-3 channels carries enough.
    let pages = encode_pages_cmyk(&plaintext, &small_geometry(), 200).unwrap();
    assert!(!pages.is_empty());

    let damaged: Vec<RgbPageBitmap> = pages
        .iter()
        .map(|rgb| {
            let mut pixels = rgb.pixels.clone();
            // Reset every pixel's B (yellow channel) to white.
            // The C and M layers stay intact.
            for i in (2..pixels.len()).step_by(3) {
                pixels[i] = 255;
            }
            RgbPageBitmap { pixels, width: rgb.width, height: rgb.height }
        })
        .collect();

    let recovered = decode_pages_cmyk(&damaged, &small_geometry()).unwrap();
    assert_eq!(recovered, plaintext);
}

#[test]
fn decoder_rejects_blank_canvas() {
    let geom = small_geometry();
    let blank = RgbPageBitmap {
        pixels: vec![255u8; (geom.pixel_width() * geom.pixel_height() * 3) as usize],
        width: geom.pixel_width(),
        height: geom.pixel_height(),
    };
    let err = decode_pages_cmyk(&[blank], &geom).unwrap_err();
    // Blank input → finder detection fails on every channel →
    // PageParse(FinderDetection(InsufficientFinders)).
    assert!(
        matches!(err, CmyDecodeError::PageParse(_)),
        "expected PageParse, got {err:?}"
    );
}
