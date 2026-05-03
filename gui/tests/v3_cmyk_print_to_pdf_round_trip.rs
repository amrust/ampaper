// v3 CMY + PDF integration test (M12 / Phase 6 GUI wiring).
//
// The load-bearing test for the GUI's CMY codec wiring: encode
// some bytes via v3::encode_pages_cmyk, save through
// save_rgb_pages_as_pdf (the Letter-page composer the Print tab
// uses for color), render the PDF back via pdfium at 600 DPI,
// decode via v3::decode_pages_cmyk. Asserts byte-exact recovery.
//
// Pins three plumbing-side properties:
//   1. The Print tab's hardcoded CMY geometry round-trips through
//      printpdf's RGB image embedding (R8G8B8 format) and
//      pdfium's RGB rendering output.
//   2. The save → render path preserves enough color separation
//      that decode_pages_cmyk's `< 128` per-channel threshold
//      classifies dots correctly.
//   3. The Decode tab's `has_color` sniff would correctly route
//      this PDF to the CMY decoder (the rendered RGB has color
//      content > 5% of sampled pixels).
//
// Skips gracefully when pdfium isn't installed.

#![allow(dead_code)]

use std::path::PathBuf;

use ampaper::v3::{PageGeometry, RgbPageBitmap, decode_pages_cmyk, encode_pages_cmyk};

#[path = "../src/print.rs"]
mod print;
#[path = "../src/worker.rs"]
mod worker;

use print::save_rgb_pages_as_pdf;
use worker::render_pdf_pages;

/// Print tab's hardcoded CMY geometry. Must match what
/// `views::print::v3_geometry()` and `worker::sniff_v3` /
/// `worker::run_v3_cmy_decode` use, otherwise the in-app
/// round-trip would mis-decode.
fn gui_cmy_geometry() -> PageGeometry {
    PageGeometry { nx: 52, ny: 68, pixels_per_dot: 3 }
}

#[test]
fn cmy_encode_save_pdf_render_decode_round_trips_bytes() {
    let geom = gui_cmy_geometry();
    // 8 KB pseudo-random — stays incompressible (so zstd skips
    // and the test exercises the raw-bytes-through-RaptorQ path),
    // but small enough that the PDF render at 600 DPI doesn't
    // exhaust test memory.
    let mut payload = Vec::with_capacity(8192);
    let mut x: u32 = 0xCAFE_F00D;
    for _ in 0..8192 {
        x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
        payload.push((x >> 16) as u8);
    }

    // 1. Encode via the CMY codec.
    let rgb_pages = encode_pages_cmyk(&payload, &geom, 25)
        .expect("CMY encode should succeed");
    assert!(!rgb_pages.is_empty(), "CMY encoder must produce ≥ 1 page");

    // 2. Save as RGB PDF using the GUI's pipeline.
    let tmp = std::env::temp_dir().join("ampaper-gui-v3-cmy-pdf-rt");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let pdf_path: PathBuf = tmp.join("v3-cmy.pdf");
    save_rgb_pages_as_pdf(&rgb_pages, 600, None, "v3-cmy-test", &pdf_path)
        .expect("save_rgb_pages_as_pdf should succeed");

    // 3. Render the PDF back via pdfium at 600 DPI. Skip when
    //    pdfium isn't installed (CI without the vendored dll).
    let rendered = match render_pdf_pages(&pdf_path, 600) {
        Ok(p) => p,
        Err(e) if e.contains("PDFium library not found") => {
            eprintln!("skipping CMY PDF round-trip — pdfium not installed");
            return;
        }
        Err(e) => panic!("PDF render failed: {e}"),
    };

    // 4. Verify the rendered RGB actually has color content —
    //    the Decode tab's `has_color` sniff would route this to
    //    the CMY decoder. If the PDF write/read accidentally
    //    converted to grayscale, the sniff would route to v3 B&W
    //    instead and fail with NoSolution. Pin this directly
    //    rather than infer it from a successful decode.
    let first = &rendered[0];
    let n = (first.width * first.height) as usize;
    assert_eq!(first.rgb.len(), n * 3, "rendered page must have RGB data");
    let stride = (n / 1000).max(1);
    let mut color_count = 0u32;
    let mut checked = 0u32;
    let mut i = 0usize;
    while i < n {
        let r = first.rgb[i * 3] as i32;
        let g = first.rgb[i * 3 + 1] as i32;
        let b = first.rgb[i * 3 + 2] as i32;
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        if max - min > 16 {
            color_count += 1;
        }
        checked += 1;
        i += stride;
    }
    assert!(
        color_count * 20 > checked,
        "rendered CMY PDF should have ≥ 5% colored pixels (got {}/{})",
        color_count,
        checked
    );

    // 5. Decode via the CMY path.
    let cmy_input: Vec<RgbPageBitmap> = rendered
        .into_iter()
        .map(|p| RgbPageBitmap {
            pixels: p.rgb,
            width: p.width,
            height: p.height,
        })
        .collect();
    let recovered = decode_pages_cmyk(&cmy_input, &geom)
        .expect("CMY decode should recover bytes from rendered PDF");
    assert_eq!(recovered, payload, "CMY PDF round-trip must be byte-exact");
}
