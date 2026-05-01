// Integration tests against PaperBack 1.10 golden vectors.
//
// Each vector under tests/golden/v1-paperbak/ is a (.input, .bmp)
// pair where the .bmp is what PB 1.10's encoder produced when fed
// the .input bytes. This test loads the .bmp via the `image` crate,
// hands the resulting grayscale pixel buffer to `ampaper::scan::scan_decode`,
// and asserts the recovered bytes equal the .input file byte-for-byte.
//
// This is the level-2 leg of the three-way cross-check from the
// memory `feedback_three_way_crosscheck.md`: PaperBack 1.10's
// encoder output round-trips through ampaper's decoder. Combined
// with the existing self round-trip in scan::tests and the C-encoder
// vector test in ecc::tests, ampaper now decodes a real PB 1.10 BMP.
//
// The file format read here is BMP because PaperBack 1.10's I/O
// is BMP-only — see the project memory `scan_input_formats.md` and
// `tests/golden/v1-paperbak/README.md` for why we don't pre-convert
// to PNG. ampaper's decoder is format-agnostic; the `image` crate
// handles the BMP/PNG distinction.

use ampaper::scan::scan_decode;
use std::path::Path;

/// Load an image (BMP or PNG) and produce a (pixels, width, height)
/// triple suitable for [`scan_decode`]. Pixels are 8-bit grayscale,
/// top-down (image crate convention). PB 1.10's BMP is bottom-up
/// per BMP spec; the image crate flips it during decode.
fn load_grayscale(path: &Path) -> (Vec<u8>, u32, u32) {
    let img = image::open(path)
        .unwrap_or_else(|e| panic!("failed to open {path:?}: {e}"))
        .to_luma8();
    let (w, h) = img.dimensions();
    (img.into_raw(), w, h)
}

#[test]
fn lorem_paperbak_v110_bmp_decodes_to_input_bytes() {
    let dir = Path::new("tests/golden/v1-paperbak");
    let bmp_path = dir.join("lorem.bmp");
    if !bmp_path.exists() {
        // Capture not yet committed. The .input file is in place but
        // the user hasn't run PB 1.10 against it yet. Skip rather
        // than fail — this test is a contract for when the BMP lands,
        // not a gate that blocks unrelated work.
        eprintln!(
            "skip: {bmp_path:?} not present; run PaperBack 1.10 on lorem.input \
             and save the BMP to capture this golden vector"
        );
        return;
    }

    let (pixels, w, h) = load_grayscale(&bmp_path);
    let input = std::fs::read(dir.join("lorem.input")).expect("lorem.input must exist");

    let recovered = scan_decode(&[(&pixels, w, h)], None)
        .expect("scan_decode must succeed on a valid PB 1.10 BMP");

    assert_eq!(
        recovered,
        input,
        "PB 1.10 BMP did not decode to lorem.input \
         (recovered len = {}, input len = {})",
        recovered.len(),
        input.len()
    );
}
