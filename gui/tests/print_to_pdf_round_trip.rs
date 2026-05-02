// print.rs / worker.rs are shared into this test binary via
// `#[path = ...]` — silence dead-code noise from unused parts of
// their public-API surface.
#![allow(dead_code)]

// Pin the lorem2.pdf bug: ampaper's Print-tab PDF output must
// round-trip through pdfium → scan_decode cleanly. Earlier the
// Print path encoded with `print_border: false`, which produced
// PDFs that decode_v1 could read but the auto-detect scan path
// could not — drop-and-decode the same PDF on the Decode tab and
// scan_decode failed with "no SuperBlock decoded successfully on
// any page" because the grid finder needs the sync-raster border
// to lock onto the dot pattern after a PDF render roundtrip.
//
// This test exercises the literal user flow:
//   1. Encode lorem.input via the GUI's encode_options_from_settings
//      with each typical block density (100 and 200 dot/inch — the
//      current and legacy defaults).
//   2. save_pages_as_pdf at 600 DPI page sizing.
//   3. render_pdf_pages at DEFAULT_PDF_RENDER_DPI.
//   4. scan_decode of the rendered bitmap recovers the input bytes.
//
// Skipped gracefully when pdfium isn't installed.

use std::path::PathBuf;

use ampaper::block::NGROUP_DEFAULT;
use ampaper::encoder::{encode, EncodeOptions, FileMeta};
use ampaper::page::{BLACK_PAPER, PageGeometry};

#[path = "../src/print.rs"]
mod print;
#[path = "../src/worker.rs"]
mod worker;

use print::{save_pages_as_pdf, PrintPage};
use worker::render_pdf_pages;

/// Test fixture encodes at 200 dot/inch (the legacy default before
/// the QualityPreset auto-density work). The runtime
/// `DEFAULT_PDF_RENDER_DPI` (300) gives 1.5 px/dot at 200 dot/in
/// which is below scan_decode's calibrated 3 px/dot floor; pass an
/// explicit 600 DPI render so the existing 200-dot/in fixture
/// continues to validate the round-trip.
const TEST_RENDER_DPI: u32 = 600;

fn lorem_input() -> Vec<u8> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    std::fs::read(
        PathBuf::from(manifest_dir).join("../tests/golden/v1-paperbak/lorem.input"),
    )
    .expect("lorem.input must exist for this test")
}

fn round_trip(blocks_per_inch: u32) {
    let lorem = lorem_input();
    let geometry = PageGeometry {
        ppix: 600,
        ppiy: 600,
        dpi: blocks_per_inch,
        dot_percent: 70,
        // Letter at 600 DPI — what the GUI uses by default.
        width: (8.5 * 600.0) as u32,
        height: (11.0 * 600.0) as u32,
        // The fix this test pins: PB-1.10-style sync raster around
        // the data area. Without it, scan_decode can't lock onto
        // the dot grid after a PDF roundtrip.
        print_border: true,
    };
    let opts = EncodeOptions {
        geometry,
        redundancy: NGROUP_DEFAULT,
        compress: true,
        black: BLACK_PAPER,
        pad_to_full_page: false,
    };
    let meta = FileMeta {
        name: "lorem.input",
        modified: 0,
        attributes: 0x80,
    };

    let pages = encode(&lorem, &opts, &meta).expect("encode should succeed");
    let print_pages: Vec<PrintPage> = pages
        .into_iter()
        .map(|p| PrintPage {
            bitmap: p.bitmap,
            width: p.width,
            height: p.height,
        })
        .collect();

    let tmp = std::env::temp_dir().join(format!("ampaper-print-pdf-rt-{blocks_per_inch}"));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let pdf_path = tmp.join("out.pdf");
    save_pages_as_pdf(&print_pages, 600, None, "lorem", &pdf_path)
        .expect("save_pages_as_pdf should succeed");

    let rendered = match render_pdf_pages(&pdf_path, TEST_RENDER_DPI) {
        Ok(p) => p,
        Err(e) if e.contains("PDFium library not found") => {
            eprintln!("skipping — pdfium not installed");
            return;
        }
        Err(e) => panic!("render_pdf_pages failed: {e}"),
    };

    let scan_pages: Vec<(&[u8], u32, u32)> = rendered
        .iter()
        .map(|p| (p.luma.as_slice(), p.width, p.height))
        .collect();
    let recovered = ampaper::scan::scan_decode(&scan_pages, None).unwrap_or_else(|e| {
        panic!(
            "scan_decode of ampaper-produced PDF (blocks_per_inch={blocks_per_inch}, \
             border=true) failed: {e}"
        );
    });
    assert_eq!(
        recovered, lorem,
        "PDF round-trip at {blocks_per_inch} dot/inch must recover lorem.input exactly"
    );
}

#[test]
fn pdf_round_trip_at_100_dot_per_inch() {
    // PaperBack 1.10's default dot density and ampaper's current
    // default. Most common case.
    round_trip(100);
}

#[test]
fn pdf_round_trip_at_200_dot_per_inch() {
    // Legacy density — some users have this persisted in eframe
    // storage from before the PB-1.10-defaults commit. This was
    // the lorem2.pdf bug's actual repro: 200 dot/inch encode +
    // print_border:false produced PDFs that wouldn't decode.
    round_trip(200);
}
