// Worker.rs is shared into this test binary via `#[path = ...]`.
#![allow(dead_code)]

// PDF round-trip smoke test: encode a payload via the codec, write
// it as a PDF via the Print tab's save path, then render the PDF
// back to a bitmap via pdfium and run scan_decode to recover the
// bytes. The full "encode → PDF → decode-from-PDF" cycle is what
// the user sees when they save a PDF for digital archival and
// later drop the same PDF onto the Decode tab to verify their
// encode is recoverable without physical print + scan.
//
// pdfium availability: the test calls `render_pdf_pages` which
// loads pdfium dynamically. When pdfium isn't installed, the test
// SKIPs (prints a notice and returns Ok) rather than failing —
// CI machines without pdfium shouldn't be blocked on this check,
// and a `pdfium.dll` placed in target/debug/ makes the local dev
// loop work out of the box.

use ampaper::block::NGROUP_DEFAULT;
use ampaper::encoder::{EncodeOptions, FileMeta, encode};
use ampaper::page::{BLACK_PAPER, PageGeometry};

#[path = "../src/print.rs"]
mod print;
#[path = "../src/worker.rs"]
mod worker;

use print::{PrintPage, save_pages_as_pdf};
use worker::{DEFAULT_PDF_RENDER_DPI, render_pdf_pages};

fn scan_geometry() -> PageGeometry {
    // Same wider geometry the lib's scan tests use — detect_geometry
    // locks on more reliably.
    PageGeometry {
        ppix: 600,
        ppiy: 600,
        dpi: 200,
        dot_percent: 70,
        width: 16 * 35 * 3 + 2 * 35 * 3,
        height: 21 * 35 * 3 + 2 * 35 * 3,
        print_border: true,
    }
}

fn meta() -> FileMeta<'static> {
    FileMeta {
        name: "pdf-round-trip.bin",
        modified: 0,
        attributes: 0x80,
    }
}

#[test]
fn encode_save_pdf_render_decode_round_trips_bytes() {
    let tmp = std::env::temp_dir().join("ampaper-gui-pdf-round-trip");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    // 1. Encode a payload via the codec.
    let payload: Vec<u8> = (0..500u32).map(|i| (i * 31) as u8).collect();
    let opts = EncodeOptions {
        geometry: scan_geometry(),
        redundancy: NGROUP_DEFAULT,
        compress: false,
        black: BLACK_PAPER,
    };
    let pages = encode(&payload, &opts, &meta()).unwrap();

    // 2. Save as PDF at the same DPI we encoded at.
    let print_pages: Vec<PrintPage> = pages
        .into_iter()
        .map(|p| PrintPage {
            bitmap: p.bitmap,
            width: p.width,
            height: p.height,
        })
        .collect();
    let pdf_path = tmp.join("round-trip.pdf");
    save_pages_as_pdf(&print_pages, 600, "round-trip", &pdf_path)
        .expect("PDF save should succeed");

    // 3. Render back via pdfium. Skip the test if pdfium isn't
    // available on this machine (no pdfium.dll alongside the test
    // binary AND no system install).
    let rendered = match render_pdf_pages(&pdf_path, DEFAULT_PDF_RENDER_DPI) {
        Ok(p) => p,
        Err(e) if e.contains("PDFium library not found") => {
            eprintln!(
                "skipping pdf_round_trip — pdfium not installed on this machine.\n\
                 grab it from https://github.com/bblanchon/pdfium-binaries"
            );
            return;
        }
        Err(e) => panic!("PDF render failed: {e}"),
    };
    assert_eq!(rendered.len(), 1, "expected one rendered page");

    // 4. Run scan_decode on the rendered bitmap.
    let scan_pages: Vec<(&[u8], u32, u32)> = rendered
        .iter()
        .map(|p| (p.luma.as_slice(), p.width, p.height))
        .collect();
    let recovered = ampaper::scan::scan_decode(&scan_pages, None)
        .expect("scan_decode of rendered PDF page should succeed");
    assert_eq!(recovered, payload);
}
