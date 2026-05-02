// Render the user's lorem2.pdf back to a bitmap and dump its
// histogram + dimensions. Compares against a freshly-encoded
// lorem.input to see what's structurally different.

#![allow(dead_code)]

#[path = "../src/worker.rs"]
mod worker;

use worker::render_pdf_pages;

fn main() -> Result<(), String> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let pdf = std::path::PathBuf::from(manifest_dir)
        .join("../tests/golden/v1-paperbak/lorem2.pdf");
    println!("inspecting: {}", pdf.display());

    for dpi in [300u32, 600, 1200] {
        let rendered = match render_pdf_pages(&pdf, dpi) {
            Ok(p) => p,
            Err(e) => {
                println!("  dpi {dpi}: render failed: {e}");
                continue;
            }
        };
        println!(
            "  dpi {dpi}: {}x{} pixels, {} pages",
            rendered[0].width,
            rendered[0].height,
            rendered.len()
        );
        let hist = histogram(&rendered[0].luma);
        println!("    histogram: {hist:?}");
        // Sniff what the histogram looks like along the first 200
        // rows — usually where the dot grid lives.
        let w = rendered[0].width as usize;
        let mut row_dark_runs = Vec::new();
        for y in 0..rendered[0].height.min(200) as usize {
            let row = &rendered[0].luma[y * w..(y + 1) * w];
            let dark = row.iter().filter(|&&p| p < 128).count();
            row_dark_runs.push(dark);
        }
        let max = *row_dark_runs.iter().max().unwrap_or(&0);
        let min = *row_dark_runs.iter().min().unwrap_or(&0);
        let avg: f32 = row_dark_runs.iter().sum::<usize>() as f32 / row_dark_runs.len() as f32;
        println!(
            "    first 200 rows: dark px / row min={min} avg={avg:.0} max={max}"
        );

        match ampaper::scan::scan_decode(
            &[(rendered[0].luma.as_slice(), rendered[0].width, rendered[0].height)],
            None,
        ) {
            Ok(b) => println!("    scan_decode: OK {} bytes", b.len()),
            Err(e) => println!("    scan_decode: {e}"),
        }
    }
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
