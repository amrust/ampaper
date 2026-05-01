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

use crate::print::{PrintError, pages_from_paths, print_pages, save_pages_as_pdf};

pub struct PrintView {
    queued_paths: Vec<PathBuf>,
    /// DPI used for direct printing AND for sizing PDF pages. Defaults
    /// to 600 (the EncodeView default; a typical consumer laser).
    /// User adjusts when their encode used a different DPI — without
    /// this, a 4800×6600 bitmap saved as PDF at the wrong DPI would
    /// come out the wrong physical size.
    pub print_dpi: u32,
    /// Status text drawn at the bottom of the panel after a print
    /// attempt. Populated synchronously since PrintDlgExW is modal —
    /// the print dialog blocks the UI thread until the user chooses
    /// or cancels, by Windows convention.
    last_status: String,
}

impl Default for PrintView {
    fn default() -> Self {
        Self {
            queued_paths: Vec::new(),
            print_dpi: 600,
            last_status: String::new(),
        }
    }
}

impl PrintView {
    pub fn show(&mut self, ui: &mut egui::Ui) {
        self.poll_dropped_files(ui.ctx());

        ui.heading("Print");
        ui.add_space(6.0);
        ui.label(
            "Drag bitmaps into this window, or pick them with the button. \
             Send them to a printer directly, or save a print-ready PDF \
             for later. Set the DPI to match what you used at encode time \
             so each dot lands on one device pixel on paper.",
        );
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);

        self.show_input_row(ui);
        ui.add_space(8.0);
        self.show_dpi_row(ui);
        ui.add_space(8.0);
        self.show_action_row(ui);

        if !self.last_status.is_empty() {
            ui.add_space(12.0);
            ui.label(egui::RichText::new(&self.last_status).weak());
        }

        if !cfg!(target_os = "windows") {
            ui.add_space(12.0);
            ui.label(
                egui::RichText::new(
                    "Direct printing is Windows-only in this build — but \
                     \"Save as PDF...\" works everywhere, so you can take \
                     the PDF to whichever printer driver you'd normally \
                     use.",
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

    fn show_dpi_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("DPI:");
            ui.add(egui::DragValue::new(&mut self.print_dpi).range(150..=2400));
            ui.label(
                egui::RichText::new(
                    "Use the same value you set in Encode → Printer DPI.",
                )
                .small()
                .weak(),
            );
        });
    }

    fn show_action_row(&mut self, ui: &mut egui::Ui) {
        let have_files = !self.queued_paths.is_empty();
        ui.horizontal(|ui| {
            // Direct print — Windows only at this milestone.
            let print_enabled = have_files && cfg!(target_os = "windows");
            ui.add_enabled_ui(print_enabled, |ui| {
                if ui.button("Print...").clicked() {
                    self.run_print();
                }
            });

            // Save as PDF — cross-platform via printpdf.
            ui.add_enabled_ui(have_files, |ui| {
                if ui.button("Save as PDF...").clicked() {
                    self.run_save_pdf();
                }
            });
        });
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

        let doc_name = self.doc_name();

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

    fn run_save_pdf(&mut self) {
        self.last_status = "Loading bitmaps...".into();
        let pages = match pages_from_paths(&self.queued_paths) {
            Ok(p) => p,
            Err(e) => {
                self.last_status = format!("{e}");
                return;
            }
        };

        let doc_name = self.doc_name();
        let suggested = format!("{doc_name}.pdf");
        let Some(path) = rfd::FileDialog::new()
            .add_filter("PDF document", &["pdf"])
            .set_file_name(&suggested)
            .save_file()
        else {
            self.last_status = "PDF save cancelled.".into();
            return;
        };

        match save_pages_as_pdf(&pages, self.print_dpi, &doc_name, &path) {
            Ok(()) => {
                self.last_status = format!(
                    "Saved {} page{} to {}",
                    pages.len(),
                    if pages.len() == 1 { "" } else { "s" },
                    path.display()
                );
            }
            Err(e) => {
                self.last_status = format!("PDF save failed: {e}");
            }
        }
    }

    fn doc_name(&self) -> String {
        self.queued_paths
            .first()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("ampaper")
            .to_string()
    }
}
