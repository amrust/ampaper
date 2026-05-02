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
