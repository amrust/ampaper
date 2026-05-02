// Quick smoke check: can we decode the user's scanned-from-paper
// PaperBack 1.10 PDF? Drives the same code path that drag-and-drop
// onto the Decode tab uses (PDFium render → scan_decode), so any
// failure here pinpoints exactly where things break.
//
// Run with:
//
//     cargo run -p ampaper-gui --example decode_scanned_pdf --release

#![allow(dead_code)]

#[path = "../src/worker.rs"]
mod worker;

use std::time::Instant;

use worker::render_pdf_pages;

fn main() -> Result<(), String> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    // First arg overrides the default scanned PDF — use it to point
    // at lorem2.pdf or any other test artifact.
    let pdf_path = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(manifest_dir)
                .join("../tests/golden/v1-paperbak/img20260501_21181215.pdf")
        });
    let lorem_path = std::path::PathBuf::from(manifest_dir)
        .join("../tests/golden/v1-paperbak/lorem.input");
    let expected =
        std::fs::read(&lorem_path).map_err(|e| format!("read lorem.input: {e}"))?;
    println!("PDF under test: {}", pdf_path.display());

    // PB 1.10 dot density 100 dpi → at a 300-dpi scan, each dot is
    // 3 device pixels (the ratio scan_decode's grid finder is
    // calibrated against). 600-dpi scan = 6 pixels per dot. 1200 =
    // 12 (over-sampled). Walk the candidate render DPIs to find the
    // one where scan_decode locks on cleanly. Once we know which
    // works for typical scanner PDFs we can pick a sensible default.
    for dpi in [300, 600, 1200] {
        println!("--- render DPI {dpi} ---");
        let t0 = Instant::now();
        let pages = match render_pdf_pages(&pdf_path, dpi) {
            Ok(p) => p,
            Err(e) => {
                println!("  render: {e}");
                continue;
            }
        };
        let elapsed = t0.elapsed().as_secs_f32();
        println!(
            "  rendered: {}x{} ({:.1}s)",
            pages[0].width, pages[0].height, elapsed
        );

        let scan_pages: Vec<(&[u8], u32, u32)> = pages
            .iter()
            .map(|p| (p.luma.as_slice(), p.width, p.height))
            .collect();
        let t1 = Instant::now();
        match ampaper::scan::scan_decode(&scan_pages, None) {
            Ok(recovered) => {
                let match_len = recovered == expected;
                println!(
                    "  scan_decode succeeded: {} bytes ({:.1}s) — {}",
                    recovered.len(),
                    t1.elapsed().as_secs_f32(),
                    if match_len { "bytes match lorem.input" } else { "BYTES DIFFER" }
                );
                // Walk all DPIs in this run rather than stopping
                // at first success — useful for confirming the
                // ladder behaves the same across the candidate set.
            }
            Err(e) => {
                println!("  scan_decode failed: {e}");
            }
        }
    }
    Ok(())
}
