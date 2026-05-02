// Diagnose why ampaper-produced PDFs (e.g. lorem2.pdf) fail to
// decode after PDF round-trip. Steps:
//   1. Encode lorem.input directly → bitmap A. scan_decode A — does
//      it work? If not, the encoder output itself is broken.
//   2. Save bitmap A to PDF via save_pages_as_pdf, render the PDF
//      back at 600 DPI → bitmap B. scan_decode B — if A worked but
//      B doesn't, the PDF round-trip is corrupting things.
//   3. Compare A and B dimensions + histograms.

#![allow(dead_code)]

#[path = "../src/print.rs"]
mod print;
#[path = "../src/worker.rs"]
mod worker;

use ampaper::block::NGROUP_DEFAULT;
use ampaper::encoder::{encode, EncodeOptions, FileMeta};
use ampaper::page::{BLACK_PAPER, PageGeometry};
use print::{save_pages_as_pdf, PrintPage};
use worker::render_pdf_pages;

fn main() -> Result<(), String> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let lorem = std::fs::read(
        std::path::PathBuf::from(manifest_dir)
            .join("../tests/golden/v1-paperbak/lorem.input"),
    )
    .map_err(|e| format!("read lorem.input: {e}"))?;

    // Reproduce the user's lorem2.pdf scenario: 200 dot/in encode
    // (the OLD EncodeSettings default before the PB-1.10 commit;
    // anyone whose eframe storage was written before that commit
    // still has 200 persisted) with print_border now flipped on.
    let geometry = PageGeometry {
        ppix: 600,
        ppiy: 600,
        dpi: 200,
        dot_percent: 70,
        width: (8.5 * 600.0) as u32,
        height: (11.0 * 600.0) as u32,
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

    let pages = encode(&lorem, &opts, &meta).map_err(|e| format!("encode: {e}"))?;
    println!(
        "encoded: {} page(s), first = {}x{}",
        pages.len(),
        pages[0].width,
        pages[0].height
    );

    // Step 1: scan_decode the source bitmap directly.
    println!("--- step 1: scan_decode the source bitmap (no PDF) ---");
    let scan_pages_a: Vec<(&[u8], u32, u32)> = pages
        .iter()
        .map(|p| (p.bitmap.as_slice(), p.width, p.height))
        .collect();
    match ampaper::scan::scan_decode(&scan_pages_a, None) {
        Ok(out) => println!("  OK: {} bytes, matches lorem? {}", out.len(), out == lorem),
        Err(e) => println!("  FAILED: {e}"),
    }

    // Step 2: save to PDF and render back.
    let tmp = std::env::temp_dir().join("ampaper-diagnose-lorem2");
    std::fs::create_dir_all(&tmp).map_err(|e| format!("{e}"))?;
    let pdf_path = tmp.join("lorem2.pdf");
    let print_pages: Vec<PrintPage> = pages
        .iter()
        .map(|p| PrintPage {
            bitmap: p.bitmap.clone(),
            width: p.width,
            height: p.height,
        })
        .collect();
    let header = print::PdfHeader {
        filename: "lorem.input".into(),
        modified_unix_secs: Some(1_714_550_400), // 2024-05-01 08:00 UTC
        origsize: lorem.len() as u64,
    };
    save_pages_as_pdf(&print_pages, 600, Some(&header), "lorem", &pdf_path)
        .map_err(|e| format!("save pdf: {e}"))?;
    println!(
        "saved PDF: {} ({} bytes)",
        pdf_path.display(),
        std::fs::metadata(&pdf_path).map(|m| m.len()).unwrap_or(0)
    );

    println!("--- step 2: render PDF back at 600 DPI and scan_decode ---");
    let rendered = match render_pdf_pages(&pdf_path, 600) {
        Ok(p) => p,
        Err(e) => return Err(format!("render: {e}")),
    };
    println!(
        "  rendered: {}x{} pixels",
        rendered[0].width, rendered[0].height
    );

    // Histogram comparison.
    let hist_a = histogram(&pages[0].bitmap);
    let hist_b = histogram(&rendered[0].luma);
    println!("  source histogram: {hist_a:?}");
    println!("  rendered histogram: {hist_b:?}");

    let scan_pages_b: Vec<(&[u8], u32, u32)> = rendered
        .iter()
        .map(|p| (p.luma.as_slice(), p.width, p.height))
        .collect();
    match ampaper::scan::scan_decode(&scan_pages_b, None) {
        Ok(out) => println!("  OK: {} bytes, matches lorem? {}", out.len(), out == lorem),
        Err(e) => println!("  FAILED: {e}"),
    }

    // Dump both bitmaps for visual inspection.
    use image::ImageEncoder;
    let pre_path = tmp.join("source.bmp");
    image::codecs::bmp::BmpEncoder::new(&mut std::fs::File::create(&pre_path).unwrap())
        .write_image(
            &pages[0].bitmap,
            pages[0].width,
            pages[0].height,
            image::ExtendedColorType::L8,
        )
        .unwrap();
    let post_path = tmp.join("rendered.bmp");
    image::codecs::bmp::BmpEncoder::new(&mut std::fs::File::create(&post_path).unwrap())
        .write_image(
            &rendered[0].luma,
            rendered[0].width,
            rendered[0].height,
            image::ExtendedColorType::L8,
        )
        .unwrap();
    println!("  source bmp: {}", pre_path.display());
    println!("  rendered bmp: {}", post_path.display());
    Ok(())
}

fn histogram(pixels: &[u8]) -> Vec<(u8, usize)> {
    let mut buckets = [0usize; 256];
    for &p in pixels {
        buckets[p as usize] += 1;
    }
    buckets
        .iter()
        .enumerate()
        .filter(|(_, n)| **n > 0)
        .map(|(v, n)| (v as u8, *n))
        .collect()
}
