// Save lorem.input as PDF using the exact same path the GUI Print
// tab takes: prepare_print_pages (which now shrinks geometry to the
// data extent) + save_pages_as_pdf (Letter page + header + centered
// bitmap).
//
// Run with `cargo run -p ampaper-gui --example save_lorem_pdf --release`.
// The PDF lands at $TMPDIR/ampaper-lorem.pdf — open it to compare
// against the user's reference (img20260501_21181215.pdf).

#![allow(dead_code)]

#[path = "../src/print.rs"]
mod print;

use ampaper::block::NGROUP_DEFAULT;
use ampaper::page::{BLACK_PAPER, PageGeometry};

use print::{prepare_print_pages, save_pages_as_pdf, PdfHeader};

fn main() -> Result<(), String> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let lorem_path = std::path::PathBuf::from(manifest_dir)
        .join("../tests/golden/v1-paperbak/lorem.input");

    // Letter geometry at 100 dot/inch — the new default matching PB 1.10.
    let geometry = PageGeometry {
        ppix: 600,
        ppiy: 600,
        dpi: 100,
        dot_percent: 70,
        width: (8.5 * 600.0) as u32,
        height: (11.0 * 600.0) as u32,
        print_border: true,
    };
    let opts = ampaper::encoder::EncodeOptions {
        geometry,
        redundancy: NGROUP_DEFAULT,
        compress: true,
        black: BLACK_PAPER,
        pad_to_full_page: false,
    };

    let pages = prepare_print_pages(&[&lorem_path], &opts, None)
        .map_err(|e| format!("prepare: {e}"))?;
    println!(
        "page count: {}, first page = {}x{} px",
        pages.len(),
        pages[0].width,
        pages[0].height
    );

    let lorem_meta = std::fs::metadata(&lorem_path).map_err(|e| format!("stat: {e}"))?;
    let modified_unix_secs = lorem_meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    let header = PdfHeader {
        filename: "lorem.input".into(),
        modified_unix_secs,
        origsize: lorem_meta.len(),
    };

    let out = std::env::temp_dir().join("ampaper-lorem.pdf");
    save_pages_as_pdf(&pages, 600, Some(&header), "lorem", &out)
        .map_err(|e| format!("save_pages_as_pdf: {e}"))?;

    let size = std::fs::metadata(&out).map_err(|e| format!("{e}"))?.len();
    println!("wrote {} ({} bytes)", out.display(), size);
    Ok(())
}
