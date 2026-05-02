// Print.rs is shared into this test binary via `#[path = ...]`.
#![allow(dead_code)]

// Drop-any-file Print-tab smoke test: simulate the user dropping a
// raw text file onto the Print tab, building the page list via
// `prepare_print_pages` (which detects the input is non-bitmap +
// runs the codec on the fly), saving as PDF via `save_pages_as_pdf`,
// rendering the PDF back via PDFium, and confirming the recovered
// bytes match the original input. Closes the
// "drop file → encode-on-the-fly → PDF → decode" loop end-to-end —
// the exact PB-1.10-style UX this tab now supports.
//
// pdfium availability: skips when pdfium isn't installed (CI
// machines without it shouldn't fail this test).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ampaper::block::NGROUP_DEFAULT;
use ampaper::encoder::EncodeOptions;
use ampaper::page::{BLACK_PAPER, PageGeometry};

#[path = "../src/print.rs"]
mod print;
#[path = "../src/worker.rs"]
mod worker;

use print::{prepare_print_pages, save_pages_as_pdf, QualityPreset};
use worker::{
    DecodeJob, DecodeMessage, DecodePage, DecodeRequest, render_pdf_pages,
};

/// Encode geometry below uses 200 dot/inch; render at 600 DPI to
/// hit scan_decode's preferred 3-device-pixels-per-dot. The runtime
/// default (`worker::TEST_RENDER_DPI`) is 300, calibrated
/// for the user-facing 100 dot/inch encode default.
const TEST_RENDER_DPI: u32 = 600;

fn scan_geometry() -> PageGeometry {
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

fn run_decode(req: DecodeRequest) -> Result<Vec<u8>, String> {
    let job = DecodeJob::spawn(req, || {});
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if Instant::now() > deadline {
            return Err("decode worker did not finish within 30s".into());
        }
        match job.rx.recv_timeout(Duration::from_millis(50)) {
            Ok(DecodeMessage::Done { plaintext }) => return Ok(plaintext),
            Ok(DecodeMessage::Failed(e)) => return Err(e),
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("decode channel disconnected".into());
            }
        }
    }
}

#[test]
fn print_tab_encodes_raw_file_on_the_fly_and_round_trips_via_pdf() {
    let tmp = std::env::temp_dir().join("ampaper-gui-print-anyfile");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    // 1. Write a raw input file the user might "drop onto the Print
    //    tab" — a plain UTF-8 text doc, very PB-1.10-archival-vibe.
    let raw_path: PathBuf = tmp.join("notes.txt");
    let payload =
        b"Lorem ipsum dolor sit amet, consectetur adipiscing elit, \
          sed do eiusmod tempor incididunt ut labore et dolore magna \
          aliqua. Ut enim ad minim veniam, quis nostrud exercitation."
            .to_vec();
    std::fs::write(&raw_path, &payload).unwrap();

    // 2. Build encode options (mirrors what the Print tab does
    //    internally based on the persisted EncodeSettings).
    let opts = EncodeOptions {
        geometry: scan_geometry(),
        redundancy: NGROUP_DEFAULT,
        compress: false,
        black: BLACK_PAPER,
        pad_to_full_page: false,
    };

    // 3. prepare_print_pages should sniff "notes.txt", see it's not
    //    a bitmap or PDF, and encode it on the fly.
    let pages = prepare_print_pages(
        std::slice::from_ref(&raw_path),
        &opts,
        QualityPreset::Normal,
        None,
    )
    .expect("prepare_print_pages should encode raw input");
    assert!(
        !pages.is_empty(),
        "raw file should produce at least one bitmap page"
    );

    // 4. Save as PDF.
    let pdf_path = tmp.join("anyfile.pdf");
    save_pages_as_pdf(&pages, 600, None, "anyfile", &pdf_path)
        .expect("PDF save should succeed");

    // 5. Render the PDF back via pdfium. Skip if pdfium not present.
    let rendered = match render_pdf_pages(&pdf_path, TEST_RENDER_DPI) {
        Ok(p) => p,
        Err(e) if e.contains("PDFium library not found") => {
            eprintln!(
                "skipping print_anyfile — pdfium not installed; \
                 grab it from https://github.com/bblanchon/pdfium-binaries"
            );
            return;
        }
        Err(e) => panic!("PDF render failed: {e}"),
    };

    // 6. Run the rendered bitmap(s) through the Decode worker just
    //    like dragging the resulting PDF onto the Decode tab would.
    let decode_pages: Vec<DecodePage> = rendered
        .into_iter()
        .map(|p| DecodePage {
            source: pdf_path.clone(),
            luma: p.luma,
            width: p.width,
            height: p.height,
        })
        .collect();
    let req = DecodeRequest {
        pages: decode_pages,
        password: None,
    };
    let recovered = run_decode(req).expect("decode worker should recover bytes");
    assert_eq!(recovered, payload);
}

#[test]
fn print_tab_passes_pre_rendered_bitmap_through() {
    // When the input is already a BMP, prepare_print_pages should
    // keep it byte-equivalent (no re-encode through the codec).
    let tmp = std::env::temp_dir().join("ampaper-gui-print-anyfile-bmp");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    // Build a tiny synthetic bitmap. 16x16 grayscale with a checker
    // pattern so we can verify pixels survive intact.
    let w = 16u32;
    let h = 16u32;
    let mut pixels = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            pixels.push(if (x ^ y) & 1 == 0 { 0u8 } else { 255u8 });
        }
    }
    let bmp_path = tmp.join("checker.bmp");
    use image::ImageEncoder;
    let mut f = std::fs::File::create(&bmp_path).unwrap();
    image::codecs::bmp::BmpEncoder::new(&mut f)
        .write_image(&pixels, w, h, image::ExtendedColorType::L8)
        .unwrap();

    let opts = EncodeOptions {
        geometry: scan_geometry(),
        redundancy: NGROUP_DEFAULT,
        compress: false,
        black: BLACK_PAPER,
        pad_to_full_page: false,
    };
    let pages = prepare_print_pages(&[bmp_path], &opts, QualityPreset::Normal, None)
        .expect("BMP input should pass through");
    assert_eq!(pages.len(), 1, "single BMP → single PrintPage");
    assert_eq!(pages[0].width, w);
    assert_eq!(pages[0].height, h);
    assert_eq!(pages[0].bitmap, pixels, "BMP pixel data must round-trip");
}

#[test]
fn print_tab_encodes_pdf_input_as_data_not_passthrough() {
    // Regression: dropping a regular (non-ampaper) PDF on the Print
    // tab used to pass it through as "already-rendered output",
    // re-rasterising the human-readable pages instead of encoding
    // its bytes. Now PDFs go through the same encode-as-data path
    // as any other binary file. Confirm by:
    //   - feeding a dummy `%PDF-...` byte string,
    //   - calling prepare_print_pages,
    //   - checking the bitmaps it returns aren't the input pixels
    //     (encoded ampaper bitmaps are 8.5"-wide-at-the-printer-DPI,
    //     vastly larger than a 612×792-point text PDF would render).
    let tmp = std::env::temp_dir().join("ampaper-gui-print-anyfile-pdf");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    // Simplest possible "valid-ish" PDF — magic bytes + minimum
    // structure. We don't need it to actually render; we just need
    // sniff_kind to see a PDF and prepare_print_pages to NOT route
    // it through pdfium-as-pass-through.
    let pdf_bytes = b"%PDF-1.4\n\
        1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n\
        2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n\
        3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >> endobj\n\
        xref\n0 4\ntrailer << /Size 4 /Root 1 0 R >>\n%%EOF\n"
        .to_vec();
    let pdf_path = tmp.join("dummy.pdf");
    std::fs::write(&pdf_path, &pdf_bytes).unwrap();

    let opts = EncodeOptions {
        geometry: scan_geometry(),
        redundancy: NGROUP_DEFAULT,
        compress: false,
        black: BLACK_PAPER,
        pad_to_full_page: false,
    };
    let pages = prepare_print_pages(
        std::slice::from_ref(&pdf_path),
        &opts,
        QualityPreset::Normal,
        None,
    )
    .expect("PDF input should encode through the codec, not fail");
    assert!(
        !pages.is_empty(),
        "PDF input should produce at least one ampaper-encoded bitmap"
    );

    // The old pass-through path rasterised PDFs at 1200 DPI, so a
    // 612×792-pt page came out 10200×13200 px. Encoded ampaper
    // bitmaps for a 215-byte payload at Normal density stay well
    // under 5000 px in either axis — they're sized by cell count,
    // not by source page area. A cap at 8000 px catches the
    // pass-through regression with comfortable headroom.
    for (i, page) in pages.iter().enumerate() {
        assert!(
            page.width < 8000 && page.height < 8000,
            "page {i}: bitmap {}x{} is too large for an encoded ampaper page \
             — likely a pdfium-rasterised pass-through (regression of the \
             tekscan.pdf bug)",
            page.width,
            page.height,
        );
    }
}

#[test]
fn print_tab_multi_page_payload_fills_pages_left_to_right_not_narrow_column() {
    // Regression: dropping a file too big to fit on one page used
    // to produce tall narrow column bitmaps (nx = redundancy+1 = 6
    // cells wide, hundreds of cells tall) that the PDF layer
    // clipped to Letter — most of the data ended up below the
    // visible page area, and the user saw a thin vertical strip
    // with mostly-empty trailing pages. Now multi-page payloads
    // skip the shrink and use the full Letter geometry, so each
    // page bitmap is page-shaped (cells wrap left-to-right).
    let tmp = std::env::temp_dir().join("ampaper-gui-print-multipage");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();

    // Build a payload large enough to need multiple pages at the
    // Normal preset's 100 dot/in target on Letter at 600 ppi.
    // ~50 KB of incompressible bytes well exceeds one page's
    // ~50 KB capacity at that density, forcing multi-page output.
    let mut payload = Vec::with_capacity(80_000);
    let mut x: u32 = 0x9E37_79B1;
    for _ in 0..80_000 {
        x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
        payload.push((x >> 16) as u8);
    }
    let raw_path = tmp.join("big.bin");
    std::fs::write(&raw_path, &payload).unwrap();

    // Letter geometry — same as the GUI's encode_options_from_settings.
    let opts = EncodeOptions {
        geometry: PageGeometry {
            ppix: 600,
            ppiy: 600,
            dpi: 100, // placeholder; auto_blocks_per_inch overrides
            dot_percent: 70,
            width: (8.5 * 600.0) as u32,
            height: (11.0 * 600.0) as u32,
            print_border: true,
        },
        redundancy: NGROUP_DEFAULT,
        compress: false,
        black: BLACK_PAPER,
        pad_to_full_page: false,
    };

    let pages = prepare_print_pages(
        std::slice::from_ref(&raw_path),
        &opts,
        QualityPreset::Normal,
        None,
    )
    .expect("multi-page encode should succeed");
    assert!(
        pages.len() >= 2,
        "80 KB at Normal/100-dpi should span ≥2 pages, got {}",
        pages.len()
    );

    // Each page bitmap should be page-shaped (wider than tall, or
    // close to it) — not the tall narrow column the old shrink
    // produced. Failure mode of the bug: width ~1370 px, height
    // ~50000+ px (aspect ratio 0.027). Healthy multi-page output:
    // width ~5100, height ~6600 (aspect ratio 0.77).
    for (i, page) in pages.iter().enumerate() {
        let aspect = page.width as f32 / page.height as f32;
        assert!(
            aspect > 0.4,
            "page {i}: bitmap {}x{} is a tall narrow column (aspect {:.3}); \
             multi-page output should fill Letter pages left-to-right \
             (regression of the tekscan.pdf bug)",
            page.width,
            page.height,
            aspect,
        );
        // Bitmap shouldn't extend past the Letter page when rendered
        // at the encode DPI — the PDF layer would clip it otherwise.
        assert!(
            page.height < (11 * 600 + 600), // ≤ 11" + 1" slack
            "page {i}: bitmap height {} px > Letter at 600 dpi; PDF would \
             clip the bottom",
            page.height,
        );
    }
}
