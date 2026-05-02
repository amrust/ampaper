// print.rs is shared via #[path] — silence the dead-code warnings
// we get for unused parts of its public-API surface.
#![allow(dead_code)]

// Quick sizing check: how big is the PDF we produce for the
// lorem.input golden file with the new defaults? Run with:
//
//     cargo run -p ampaper-gui --example lorem_pdf_size --release
//
// Useful for sanity-checking the user's "PDF is huge" complaint as
// the encoder defaults change. Prints one line per mode with the
// resulting file size. Reference numbers as of the
// pad_to_full_page+Flate landing:
//
//     compact   pad_to_full_page=false  ->  ~125 KB
//     full-page pad_to_full_page=true   ->  ~147 KB
//
// (The pre-Flate baseline was ~33 MB for either mode — the
// uncompressed grayscale image stream filled the whole PDF.)

use ampaper::block::NGROUP_DEFAULT;
use ampaper::encoder::{encode, EncodeOptions, FileMeta};
use ampaper::page::{BLACK_PAPER, PageGeometry};

#[path = "../src/print.rs"]
mod print;
use print::{save_pages_as_pdf, PrintPage};

fn main() -> Result<(), String> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let input_path =
        std::path::PathBuf::from(manifest_dir).join("../tests/golden/v1-paperbak/lorem.input");
    let payload = std::fs::read(&input_path).map_err(|e| format!("{e}"))?;

    // Letter at 600 DPI — the EncodeView default.
    let geometry = PageGeometry {
        ppix: 600,
        ppiy: 600,
        dpi: 200,
        dot_percent: 70,
        width: (8.5 * 600.0) as u32,
        height: (11.0 * 600.0) as u32,
        print_border: false,
    };
    let meta = FileMeta {
        name: "lorem.input",
        modified: 0,
        attributes: 0x80,
    };

    for label_pad in [("compact", false), ("full-page", true)] {
        let (label, pad) = label_pad;
        let opts = EncodeOptions {
            geometry,
            redundancy: NGROUP_DEFAULT,
            compress: true,
            black: BLACK_PAPER,
            pad_to_full_page: pad,
        };
        let pages = encode(&payload, &opts, &meta).map_err(|e| format!("{e}"))?;
        let print_pages: Vec<PrintPage> = pages
            .into_iter()
            .map(|p| PrintPage {
                bitmap: p.bitmap,
                width: p.width,
                height: p.height,
            })
            .collect();
        let out = std::env::temp_dir().join(format!("lorem-{label}.pdf"));
        save_pages_as_pdf(&print_pages, 600, "lorem", &out).map_err(|e| format!("{e}"))?;
        let size = std::fs::metadata(&out).map_err(|e| format!("{e}"))?.len();
        println!(
            "{label:>10} pad_to_full_page={pad}  ->  {} bytes  ({})",
            size,
            out.display()
        );
    }
    Ok(())
}
