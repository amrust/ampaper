// Worker.rs is shared into this test binary via `#[path = ...]`.
#![allow(dead_code)]

// Golden test: decode a real scanner-produced PDF of a PaperBack
// 1.10 print of `lorem.input`. This is the user's actual workflow:
//
//     1. Encode lorem.input via PaperBack 1.10 (Dot density 100,
//        Dot size 70%, Compression Maximal, Redundancy 1:5).
//     2. Print on paper.
//     3. Scan the paper into a PDF.
//     4. Drop the PDF onto ampaper's Decode tab.
//
// This test pins step 4 against a real artifact captured from
// step 3: tests/golden/v1-paperbak/img20260501_21181215.pdf. If
// scan_decode regresses on real-scanner output (rotation tolerance,
// dot pitch detection, threshold calibration), this test catches it.
//
// Skipped gracefully when pdfium is not installed.

use std::path::PathBuf;

#[path = "../src/worker.rs"]
mod worker;

use worker::{render_pdf_pages, DEFAULT_PDF_RENDER_DPI};

#[test]
fn scanned_paperbak_pdf_decodes_to_lorem_input() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let pdf_path = PathBuf::from(manifest_dir)
        .join("../tests/golden/v1-paperbak/img20260501_21181215.pdf");
    let lorem_path = PathBuf::from(manifest_dir)
        .join("../tests/golden/v1-paperbak/lorem.input");

    if !pdf_path.exists() {
        eprintln!(
            "skipping — golden PDF not present at {}",
            pdf_path.display()
        );
        return;
    }
    let expected = std::fs::read(&lorem_path).expect("read lorem.input");

    // 1. Render via pdfium at the runtime default DPI. Skip if
    //    pdfium isn't installed on this machine.
    let rendered = match render_pdf_pages(&pdf_path, DEFAULT_PDF_RENDER_DPI) {
        Ok(p) => p,
        Err(e) if e.contains("PDFium library not found") => {
            eprintln!(
                "skipping — pdfium not installed; \
                 grab it from https://github.com/bblanchon/pdfium-binaries"
            );
            return;
        }
        Err(e) => panic!("PDFium render failed: {e}"),
    };
    assert!(!rendered.is_empty(), "PDF should have at least one page");

    // 2. Run scan_decode straight against the rendered bitmap. This
    //    is the same path drag-and-drop onto the Decode tab takes.
    let pages: Vec<(&[u8], u32, u32)> = rendered
        .iter()
        .map(|p| (p.luma.as_slice(), p.width, p.height))
        .collect();
    let recovered = ampaper::scan::scan_decode(&pages, None)
        .expect("scan_decode of scanned PB-1.10 PDF should succeed");

    // 3. Bytes must match the source `lorem.input` (446 bytes).
    assert_eq!(
        recovered, expected,
        "scanned-then-decoded bytes must match lorem.input ({} expected, got {})",
        expected.len(),
        recovered.len()
    );
}
