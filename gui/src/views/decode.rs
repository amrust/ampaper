// Decode tab — pick a bitmap (or several), recover the original file.
//
// Scaffolding only. Real wiring lands in subsequent commits:
//   - file picker for one or more BMP / PNG / JPG / PDF inputs
//   - password prompt that appears when the SuperBlock asserts
//     PBM_ENCRYPTED (v1) or PBM_V2_ENCRYPTED (v2)
//   - progress bar driven from the scan_decode worker thread
//   - save dialog for the recovered file
//
// Decode is fully cross-platform: the codec is platform-agnostic and
// `image` decodes BMP / PNG / JPG everywhere. PDF input lands when
// pdf_extract or similar is wired in for M10.

use eframe::egui;

#[derive(Default)]
pub struct DecodeView {
    // Placeholder.
}

impl DecodeView {
    pub fn show(&mut self, ui: &mut egui::Ui) {
        ui.heading("Decode");
        ui.add_space(6.0);
        ui.label(
            "Open one or more scanned bitmaps to recover the original \
             file. Reads PaperBack 1.10 v1 prints (incl. legacy AES-192) \
             and ampaper v2 (AES-256-GCM) automatically.",
        );
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(12.0);

        ui.label(
            egui::RichText::new("Decoder UI coming next commit.")
                .italics()
                .weak(),
        );
    }
}
