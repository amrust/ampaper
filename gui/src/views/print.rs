// Print tab.
//
// Pick / drag one or more BMP / PNG / JPG files and send them to a
// printer via the standard Windows print dialog. The actual GDI
// plumbing lives in `crate::print`; this view is thin UI on top:
// queue → load → call print → surface result in the status bar.
//
// Cross-platform: this view compiles everywhere; the print call
// returns `PlatformUnsupported` outside Windows and the button is
// greyed via `Tab::is_available()`. When M9 grows Linux (CUPS) /
// macOS (CGContext) backends, the `crate::print::print_pages` entry
// will dispatch internally and this view stays the same.

use std::path::PathBuf;

use eframe::egui;

use crate::print::{PrintError, pages_from_paths, print_pages};

#[derive(Default)]
pub struct PrintView {
    queued_paths: Vec<PathBuf>,
    /// Status text drawn at the bottom of the panel after a print
    /// attempt. Populated synchronously since PrintDlgExW is modal —
    /// the print dialog blocks the UI thread until the user chooses
    /// or cancels, by Windows convention.
    last_status: String,
}

impl PrintView {
    pub fn show(&mut self, ui: &mut egui::Ui) {
        self.poll_dropped_files(ui.ctx());

        ui.heading("Print");
        ui.add_space(6.0);
        ui.label(
            "Drag bitmaps into this window, or pick them with the button. \
             Click \"Print...\" to send them to a printer of your choice. \
             For an archival print: pick the same DPI in your printer \
             settings as you used in the Encode tab — that keeps every \
             dot at one device pixel.",
        );
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);

        self.show_input_row(ui);
        ui.add_space(6.0);
        self.show_print_button(ui);

        if !self.last_status.is_empty() {
            ui.add_space(12.0);
            ui.label(egui::RichText::new(&self.last_status).weak());
        }

        if !cfg!(target_os = "windows") {
            ui.add_space(12.0);
            ui.label(
                egui::RichText::new(
                    "This build can't drive a printer directly — printing \
                     is Windows-only at first. Save the bitmaps via the \
                     Encode tab and print them with whatever tool you \
                     normally use.",
                )
                .small()
                .weak(),
            );
        }
    }

    fn poll_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped.is_empty() {
            self.queued_paths = dropped;
            self.last_status.clear();
        }
    }

    fn show_input_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Bitmaps to print:");
            if self.queued_paths.is_empty() {
                ui.monospace("(drop files here, or use the button)");
            } else {
                ui.monospace(format!("{} file(s)", self.queued_paths.len()));
            }
        });
        ui.horizontal(|ui| {
            if ui.button("Open files...").clicked()
                && let Some(paths) = rfd::FileDialog::new()
                    .add_filter("Bitmap", &["bmp", "png", "jpg", "jpeg"])
                    .pick_files()
            {
                self.queued_paths = paths;
            }
            if !self.queued_paths.is_empty() && ui.button("Clear").clicked() {
                self.queued_paths.clear();
            }
        });
        if !self.queued_paths.is_empty() {
            for p in &self.queued_paths {
                ui.label(egui::RichText::new(p.display().to_string()).small().weak());
            }
        }
    }

    fn show_print_button(&mut self, ui: &mut egui::Ui) {
        let ready = !self.queued_paths.is_empty() && cfg!(target_os = "windows");
        ui.add_enabled_ui(ready, |ui| {
            if ui.button("Print...").clicked() {
                self.run_print();
            }
        });
        if !ready && !self.queued_paths.is_empty() && !cfg!(target_os = "windows") {
            ui.label(
                egui::RichText::new("Native printing is Windows-only in this build.")
                    .small()
                    .weak(),
            );
        }
    }

    fn run_print(&mut self) {
        // Load all images first; if any fail, surface the error
        // before opening the print dialog (UX nit: avoid making the
        // user click through a printer picker just to see a "failed
        // to load" error after).
        self.last_status = "Loading bitmaps...".into();
        let pages = match pages_from_paths(&self.queued_paths) {
            Ok(p) => p,
            Err(e) => {
                self.last_status = format!("{e}");
                return;
            }
        };

        let doc_name = self
            .queued_paths
            .first()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("ampaper")
            .to_string();

        // PrintDlgExW is modal — blocks the UI thread until the user
        // chooses a printer or cancels. egui's frame loop pauses
        // while it's open, which is fine; the Windows print picker
        // is the user's expected experience.
        match print_pages(&pages, &doc_name) {
            Ok(()) => {
                self.last_status = format!(
                    "Sent {} page{} to the printer.",
                    pages.len(),
                    if pages.len() == 1 { "" } else { "s" }
                );
            }
            Err(PrintError::UserCancelled) => {
                self.last_status = "Print cancelled.".into();
            }
            Err(e) => {
                self.last_status = format!("Print failed: {e}");
            }
        }
    }
}
