// v3 + PDF integration test (M12 / GUI v3 wiring).
//
// Mirrors the legacy print_to_pdf_round_trip.rs but exercises the
// v3 encode/decode codec all the way through the GUI's PDF pipe:
//
//   bytes → v3::encode_pages → save_pages_as_pdf → write file
//   → render_pdf_pages (pdfium) → v3::decode_pages → bytes match
//
// Pins the integration on top of the synthetic round-trip tests
// in tests/v3_page_round_trip.rs. Where those simulate the
// distortions a scanner introduces, this one runs through a real
// PDF rasteriser, so it catches plumbing-side regressions
// (geometry mismatches, channel rearrangements, DPI drift through
// the printpdf → pdfium path) that the synthetic tests can't see.
//
// Skips gracefully when pdfium isn't installed.

#![allow(dead_code)]

use std::path::PathBuf;

use ampaper::v3::{PageBitmap, PageGeometry, decode_pages, encode_pages};

#[path = "../src/print.rs"]
mod print;
#[path = "../src/worker.rs"]
mod worker;

use print::{PrintPage, save_pages_as_pdf};
use worker::render_pdf_pages;

/// The Print tab's hardcoded v3 geometry. Must match what
/// `views::print::PrintView::prepare_v3` and `worker::sniff_v3` /
/// `worker::run_v3_decode` use, otherwise the in-app round-trip
/// would silently mis-decode.
fn gui_v3_geometry() -> PageGeometry {
    PageGeometry { nx: 26, ny: 33, pixels_per_dot: 6 }
}

#[test]
fn v3_encode_save_pdf_render_decode_round_trips_bytes() {
    let geom = gui_v3_geometry();
    // Pseudo-random 4 KB payload — exercises RaptorQ's main path
    // without bloating test render time.
    let payload: Vec<u8> = (0u32..4096)
        .map(|i| (i.wrapping_mul(31).wrapping_add(7) & 0xFF) as u8)
        .collect();

    // 1. v3 encode.
    let v3_pages = encode_pages(&payload, &geom, 50)
        .expect("v3 encode_pages should succeed");
    let print_pages: Vec<PrintPage> = v3_pages
        .into_iter()
        .map(|p| PrintPage {
            bitmap: p.pixels,
            width: p.width,
            height: p.height,
        })
        .collect();
    assert!(
        !print_pages.is_empty(),
        "v3 encoder should produce at least one page"
    );

    // 2. Save as PDF using the GUI's pipeline.
    let tmp = std::env::temp_dir().join("ampaper-gui-v3-pdf-rt");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let pdf_path: PathBuf = tmp.join("v3.pdf");
    save_pages_as_pdf(&print_pages, 600, None, "v3-test", &pdf_path)
        .expect("save_pages_as_pdf should succeed for v3 pages");

    // 3. Render the PDF back via pdfium. Skip when pdfium isn't
    // installed (CI machines without the vendored dll).
    let rendered = match render_pdf_pages(&pdf_path, 600) {
        Ok(p) => p,
        Err(e) if e.contains("PDFium library not found") => {
            eprintln!("skipping v3 PDF round-trip — pdfium not installed");
            return;
        }
        Err(e) => panic!("PDF render failed: {e}"),
    };

    // 4. v3 decode — the rendered bitmaps include the Letter page
    // margin (centered bitmap with white space around it), which
    // Phase 2.5a's finder-based page detection is supposed to
    // handle. If THIS round-trip fails, the synthetic
    // page_in_larger_canvas test would have caught the codec-side
    // regression; the failure is plumbing.
    let v3_input: Vec<PageBitmap> = rendered
        .into_iter()
        .map(|p| PageBitmap {
            pixels: p.luma,
            width: p.width,
            height: p.height,
        })
        .collect();
    let recovered = decode_pages(&v3_input, &geom)
        .expect("v3 decode_pages should recover bytes from rendered PDF");
    assert_eq!(recovered, payload, "v3 PDF round-trip must be byte-exact");
}
