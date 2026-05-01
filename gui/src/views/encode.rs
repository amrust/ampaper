// Encode tab — pick a file, configure encoder options, render pages.
//
// Scaffolding only. Real wiring lands in subsequent commits:
//   - file picker (eframe::egui::FileDialog or rfd)
//   - encode-options panel (geometry, dpi, dot_percent, redundancy,
//     compress, optional v2 password)
//   - "Encode" button → spawn worker thread → progress bar
//   - save dialog for the resulting bitmap(s)
//
// Per memory/cross_platform_goal.md the codec call itself is fully
// platform-agnostic; only the file-picker layer touches OS APIs and
// rfd / eframe handle that for us cross-platform.

use eframe::egui;

#[derive(Default)]
pub struct EncodeView {
    // Placeholder — real fields (PathBuf input, options, worker
    // handle, result) land in the next commit.
}

impl EncodeView {
    pub fn show(&mut self, ui: &mut egui::Ui) {
        ui.heading("Encode");
        ui.add_space(6.0);
        ui.label(
            "Pick a file to encode into one or more printable bitmaps. \
             Output is byte-compatible with PaperBack 1.10 (v1) or uses \
             the AES-256-GCM v2 envelope when a password is provided.",
        );
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(12.0);

        ui.label(
            egui::RichText::new("Encoder UI coming next commit.")
                .italics()
                .weak(),
        );
    }
}
