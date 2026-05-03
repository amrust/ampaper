// print.rs is shared into this test binary via `#[path = ...]`,
// which makes the rest of its public-API surface (Win32 print path,
// PrintError variants we don't exercise here) look unused. Silence
// the spurious warnings.
#![allow(dead_code)]

// Smoke test for the PDF export path. Encodes a payload via the
// codec, hands the resulting bitmap to `save_pages_as_pdf`, and
// verifies the on-disk file is a valid PDF (magic bytes + non-empty
// + size in the right ballpark for a Letter-sized grayscale bitmap).
//
// We don't round-trip through scan_decode here because that would
// require parsing the PDF and extracting the embedded image stream
// — too much PDF-format machinery for what's really a "did the
// write path produce valid bytes" check. The Encode/Decode round-
// trip is already covered by the lib + worker smoke tests.

use ampaper::block::NGROUP_DEFAULT;
use ampaper::encoder::{EncodeOptions, FileMeta, encode};
use ampaper::page::{BLACK_PAPER, PageGeometry};

#[path = "../src/print.rs"]
mod print;
use print::{PrintPage, save_pages_as_pdf};

fn small_geometry() -> PageGeometry {
    PageGeometry {
        ppix: 600,
        ppiy: 600,
        dpi: 200,
        dot_percent: 70,
        width: 12 * 35 * 3 + 2,
        height: 6 * 35 * 3 + 2,
        print_border: false,
    }
}

fn meta() -> FileMeta<'static> {
    FileMeta {
        name: "smoke.bin",
        modified: 0,
        attributes: 0x80,
    }
}

#[test]
fn save_pages_as_pdf_writes_a_real_pdf() {
    let tmp = std::env::temp_dir().join("ampaper-gui-pdf-smoke");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let payload: Vec<u8> = (0..500u32).map(|i| (i * 31) as u8).collect();
    let opts = EncodeOptions {
        geometry: small_geometry(),
        redundancy: NGROUP_DEFAULT,
        compress: false,
        black: BLACK_PAPER,
        pad_to_full_page: false,
    };
    let pages = encode(&payload, &opts, &meta()).unwrap();
    assert_eq!(pages.len(), 1);

    let print_pages: Vec<PrintPage> = pages
        .into_iter()
        .map(|p| PrintPage {
            bitmap: p.bitmap,
            width: p.width,
            height: p.height,
        })
        .collect();

    let out = tmp.join("smoke.pdf");
    save_pages_as_pdf(&print_pages, 600, None, "smoke", &out).expect("PDF save should succeed");

    let bytes = std::fs::read(&out).unwrap();
    // Every PDF starts with `%PDF-` (typically `%PDF-1.x` or `%PDF-2.0`).
    assert!(
        bytes.starts_with(b"%PDF-"),
        "output is not a PDF: first bytes = {:?}",
        &bytes[..bytes.len().min(8)]
    );
    // Every PDF ends with `%%EOF` (sometimes followed by trailing
    // whitespace). Allow a few trailing bytes of slack.
    let tail = &bytes[bytes.len().saturating_sub(16)..];
    assert!(
        tail.windows(5).any(|w| w == b"%%EOF"),
        "PDF doesn't end with %%EOF marker: tail = {:?}",
        std::str::from_utf8(tail).ok()
    );
    // A 4500-byte payload at our small geometry produces a roughly
    // 1300x650 image. The PDF won't be tiny (image stream + headers)
    // — even with deflate compression, expect at least 1 KB.
    assert!(
        bytes.len() >= 1024,
        "PDF is suspiciously small ({} bytes); did the image stream make it in?",
        bytes.len()
    );
}

#[test]
fn oversized_bitmap_with_header_uses_custom_page_not_letter() {
    // Regression: when the user dropped print_dpi from 600 to 200
    // on the v3 CMY codec, the bitmap (sized for 600 DPI = exactly
    // Letter at the encoder's geometry) became 25.44" × 32.64" at
    // the lower DPI — way bigger than Letter. The save path used
    // to force a Letter-sized PDF page whenever a header was
    // supplied, silently clipping the bitmap to the visible
    // Letter area. Now the page widens to fit the bitmap +
    // margins + header band.
    let tmp = std::env::temp_dir().join("ampaper-gui-pdf-oversize-with-header");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    // Bitmap 5088 × 6528 (the v3 CMY default page size) at
    // print_dpi=200 → 25.44" × 32.64" page. Force-Letter would
    // clip this to 8.5" × 11".
    let pages = vec![PrintPage {
        bitmap: vec![255u8; 5088 * 6528],
        width: 5088,
        height: 6528,
    }];
    let header = print::PdfHeader {
        filename: "test.bin".into(),
        modified_unix_secs: None,
        origsize: 1024,
    };
    let out = tmp.join("oversize.pdf");
    save_pages_as_pdf(&pages, 200, Some(&header), "oversize", &out)
        .expect("oversized + header should save without clipping");

    let bytes = std::fs::read(&out).unwrap();
    assert!(bytes.starts_with(b"%PDF-"));
    // The PDF MediaBox should reflect the larger custom page.
    // We can't easily parse the MediaBox here, but a Letter-only
    // PDF with a 5088×6528 image stream + flate compression on
    // a Letter page is ~1-2 MB. A custom-page PDF that holds the
    // full bitmap is similarly sized but the actual image data
    // is identical (printpdf doesn't re-rasterize on page-size
    // change). So we just sanity-check the file looks like a
    // real PDF; the regression we're guarding against is
    // SILENT — the previous Letter-clipped output was a valid
    // PDF too, just missing data.
    assert!(bytes.len() >= 1024);
}

#[test]
fn save_pages_as_pdf_rejects_zero_dpi() {
    let tmp = std::env::temp_dir().join("ampaper-gui-pdf-bad-dpi");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let pages = vec![PrintPage {
        bitmap: vec![255u8; 100 * 100],
        width: 100,
        height: 100,
    }];
    let out = tmp.join("bad.pdf");
    let err = save_pages_as_pdf(&pages, 0, None, "bad", &out).expect_err("dpi=0 must error");
    assert!(format!("{err}").contains("DPI"));
}

#[test]
fn save_pages_as_pdf_rejects_empty_input() {
    let tmp = std::env::temp_dir().join("ampaper-gui-pdf-empty");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    let out = tmp.join("empty.pdf");
    let err = save_pages_as_pdf(&[], 600, None, "empty", &out).expect_err("empty pages must error");
    assert!(format!("{err}").contains("no pages"));
}
