// End-to-end Decode-tab path for a v3 CMY PDF — simulates exactly
// what happens when the user drops a v3 CMY PDF onto the Decode
// tab in the GUI:
//
//   1. load_input renders the PDF at DEFAULT_PDF_RENDER_DPI.
//   2. run_decode dispatches via has_color sniff.
//   3. run_v3_cmy_decode does the actual decode.
//
// Pins that the runtime render DPI is high enough for v3's
// hardcoded geometry. If this test fails but the
// v3_cmyk_print_to_pdf_round_trip test passes (which uses 600
// DPI render explicitly), the gap is the runtime
// DEFAULT_PDF_RENDER_DPI value, NOT the codec.
//
// Skips when pdfium isn't installed.

#![allow(dead_code)]

use std::path::PathBuf;

use ampaper::v3::{PageGeometry, RgbPageBitmap, decode_pages_cmyk, encode_pages_cmyk};

#[path = "../src/print.rs"]
mod print;
#[path = "../src/worker.rs"]
mod worker;

use print::save_rgb_pages_as_pdf;
use worker::{DEFAULT_PDF_RENDER_DPI, render_pdf_pages};

fn gui_cmy_geometry() -> PageGeometry {
    PageGeometry { nx: 52, ny: 68, pixels_per_dot: 3 }
}

#[test]
fn cmy_pdf_decodes_at_runtime_default_render_dpi() {
    // Encode → save PDF → render at DEFAULT_PDF_RENDER_DPI →
    // decode via CMY path. This is the exact path triggered when
    // the user drops a v3 CMY PDF onto the Decode tab. If the
    // runtime DPI is too low for v3's hardcoded geometry, this
    // is the test that fails.
    let geom = gui_cmy_geometry();
    let mut payload = Vec::with_capacity(8192);
    let mut x: u32 = 0xCAFE_F00D;
    for _ in 0..8192 {
        x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
        payload.push((x >> 16) as u8);
    }

    let rgb_pages = encode_pages_cmyk(&payload, &geom, 25)
        .expect("CMY encode should succeed");

    let tmp = std::env::temp_dir().join("ampaper-gui-v3-cmy-decode-tab-e2e");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let pdf_path: PathBuf = tmp.join("v3-cmy.pdf");
    save_rgb_pages_as_pdf(&rgb_pages, 600, None, "v3-cmy", &pdf_path)
        .expect("save_rgb_pages_as_pdf should succeed");

    // Render at the RUNTIME default DPI — this is what the
    // Decode tab actually uses when the user drops the PDF.
    let rendered = match render_pdf_pages(&pdf_path, DEFAULT_PDF_RENDER_DPI) {
        Ok(p) => p,
        Err(e) if e.contains("PDFium library not found") => {
            eprintln!("skipping CMY Decode-tab e2e — pdfium not installed");
            return;
        }
        Err(e) => panic!("PDF render failed: {e}"),
    };

    let cmy_input: Vec<RgbPageBitmap> = rendered
        .into_iter()
        .map(|p| RgbPageBitmap {
            pixels: p.rgb,
            width: p.width,
            height: p.height,
        })
        .collect();
    let recovered = decode_pages_cmyk(&cmy_input, &geom)
        .expect("CMY decode at DEFAULT_PDF_RENDER_DPI should recover bytes");
    assert_eq!(recovered, payload);
}
