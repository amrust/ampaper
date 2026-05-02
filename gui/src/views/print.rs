// Print tab.
//
// Drop ANY file (or a pre-rendered ampaper bitmap, or an
// already-encoded PDF) and ampaper:
//   - if the input looks like a bitmap or PDF, passes it through;
//   - otherwise, encodes it on the fly via the Encode tab's settings
//     (paper size, redundancy, compression, v2 password).
// Then sends the resulting page bitmaps to a printer (Windows GDI)
// or saves them as a multi-page PDF (cross-platform).
//
// This mirrors PaperBack 1.10's "drag a file in, hit print" UX:
// the user doesn't have to encode separately on the Encode tab and
// then drag bitmaps over here unless they want explicit control
// over where the bitmap files land.

use std::path::PathBuf;

use eframe::egui;

use crate::print::{
    prepare_print_pages, print_pages, save_pages_as_pdf, PdfHeader, PrintError, PrintPage,
};
use crate::views::encode::EncodeSettings;

pub struct PrintView {
    queued_paths: Vec<PathBuf>,
    /// DPI used for direct printing AND for sizing PDF pages. Defaults
    /// to 600 (the EncodeView default; a typical consumer laser).
    pub print_dpi: u32,
    /// v2 encryption password. Empty disables v2 (raw inputs encode
    /// as v1; pre-encoded bitmaps just pass through). Like the Encode
    /// tab, we never persist this — it's session state only.
    v2_password: String,
    last_status: String,
}

impl Default for PrintView {
    fn default() -> Self {
        Self {
            queued_paths: Vec::new(),
            print_dpi: 600,
            v2_password: String::new(),
            last_status: String::new(),
        }
    }
}

impl PrintView {
    pub fn show(&mut self, ui: &mut egui::Ui, encode_settings: &EncodeSettings) {
        self.poll_dropped_files(ui.ctx());

        ui.heading("Print");
        ui.add_space(6.0);
        ui.label(
            "Drop ANY file into this window — text, image, archive, \
             whatever — and ampaper encodes it on the fly using your \
             Encode tab settings, then prints or saves a print-ready \
             PDF. Pre-encoded bitmaps and PDFs are passed through as-is.",
        );
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);

        self.show_input_row(ui);
        ui.add_space(8.0);
        self.show_dpi_row(ui);
        ui.add_space(8.0);
        self.show_v2_row(ui, encode_settings);
        ui.add_space(8.0);
        self.show_action_row(ui, encode_settings);

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
            ui.label("Input file(s):");
            if self.queued_paths.is_empty() {
                ui.monospace("(drop files here, or use the button)");
            } else {
                ui.monospace(format!("{} file(s)", self.queued_paths.len()));
            }
        });
        ui.horizontal(|ui| {
            if ui.button("Open files...").clicked()
                && let Some(paths) = rfd::FileDialog::new().pick_files()
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
            ui.label("PDF page DPI:");
            ui.add(egui::DragValue::new(&mut self.print_dpi).range(150..=2400));
            ui.label(
                egui::RichText::new(
                    "PDF page sizing only — does not affect printer DPI.",
                )
                .small()
                .weak(),
            );
        });
    }

    fn show_v2_row(&mut self, ui: &mut egui::Ui, encode_settings: &EncodeSettings) {
        // Only show the password field when the Encode tab settings
        // ask for v2 encryption. Otherwise raw inputs encode as v1
        // and the password isn't used.
        if encode_settings.v2_encrypt {
            ui.horizontal(|ui| {
                ui.label("v2 password:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.v2_password)
                        .password(true)
                        .desired_width(260.0),
                );
            });
            if self.v2_password.is_empty() {
                ui.label(
                    egui::RichText::new(
                        "Encode tab is set to v2 — raw files need a password. \
                         (Pre-encoded bitmaps and PDFs pass through unchanged.)",
                    )
                    .small()
                    .weak(),
                );
            }
        } else {
            ui.label(
                egui::RichText::new(
                    "Raw file inputs will be encoded as v1 (PaperBack-1.10-\
                     compatible). Toggle \"Encrypt with AES-256-GCM\" on the \
                     Encode tab if you want v2 here.",
                )
                .small()
                .weak(),
            );
        }
    }

    fn show_action_row(&mut self, ui: &mut egui::Ui, encode_settings: &EncodeSettings) {
        let have_files = !self.queued_paths.is_empty();
        ui.horizontal(|ui| {
            let print_enabled = have_files && cfg!(target_os = "windows");
            ui.add_enabled_ui(print_enabled, |ui| {
                if ui.button("Print...").clicked() {
                    self.run_print(encode_settings);
                }
            });
            ui.add_enabled_ui(have_files, |ui| {
                if ui.button("Save as PDF...").clicked() {
                    self.run_save_pdf(encode_settings);
                }
            });
        });
    }

    fn run_print(&mut self, encode_settings: &EncodeSettings) {
        let Some(pages) = self.prepare(encode_settings) else {
            return;
        };
        let doc_name = self.doc_name();
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

    fn run_save_pdf(&mut self, encode_settings: &EncodeSettings) {
        let Some(pages) = self.prepare(encode_settings) else {
            return;
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
        let header = self.build_pdf_header();
        match save_pages_as_pdf(&pages, self.print_dpi, header.as_ref(), &doc_name, &path) {
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

    /// Build the PaperBack-1.10-style header line metadata from the
    /// first queued input. PB 1.10 prints
    /// `<filename> [<date_time>, <bytes>] Page X of Y` at the top of
    /// every page; we mirror it. Returns None when we can't get
    /// usable metadata (e.g., no files queued or stat failed) — in
    /// that case save_pages_as_pdf falls back to a header-less PDF.
    fn build_pdf_header(&self) -> Option<PdfHeader> {
        let path = self.queued_paths.first()?;
        let meta = std::fs::metadata(path).ok()?;
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "input".to_string());
        let modified_unix_secs = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        Some(PdfHeader {
            filename,
            modified_unix_secs,
            origsize: meta.len(),
        })
    }

    /// Build the page list, encoding any raw inputs through the
    /// codec. Returns `None` when something failed; in that case we
    /// also wrote the error to `last_status` for the user.
    fn prepare(&mut self, encode_settings: &EncodeSettings) -> Option<Vec<PrintPage>> {
        self.last_status = "Preparing pages...".into();
        let opts = encode_options_from_settings(encode_settings);
        let v2 = if encode_settings.v2_encrypt && !self.v2_password.is_empty() {
            Some(self.v2_password.as_str())
        } else {
            None
        };
        if encode_settings.v2_encrypt && self.v2_password.is_empty() {
            // The Encode tab asks for v2 but we don't have a password —
            // fail loudly instead of silently emitting v1.
            self.last_status =
                "v2 encryption is enabled in Settings but no password was supplied.".into();
            return None;
        }
        match prepare_print_pages(
            &self.queued_paths,
            &opts,
            encode_settings.quality,
            v2,
        ) {
            Ok(pages) => Some(pages),
            Err(e) => {
                self.last_status = format!("{e}");
                None
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

/// Build a full [`ampaper::encoder::EncodeOptions`] from the
/// persisted [`EncodeSettings`] + paper-size lookup. Mirrors the
/// equivalent code on the Encode tab (`build_request`); kept in
/// sync because both consume the same settings object.
fn encode_options_from_settings(
    settings: &EncodeSettings,
) -> ampaper::encoder::EncodeOptions {
    use ampaper::page::{BLACK_PAPER, PageGeometry};
    let (in_w, in_h) = settings.paper_size.inches();
    let width = (in_w * settings.printer_dpi as f32) as u32;
    let height = (in_h * settings.printer_dpi as f32) as u32;
    // Density at this point is a placeholder — the quality preset's
    // *target* density. prepare_print_pages re-picks per-input by
    // running auto_blocks_per_inch on the actual payload size, so
    // the final encoded bitmap uses dots as big as the data allows.
    let (_, placeholder_density) = settings.quality.density_range();
    ampaper::encoder::EncodeOptions {
        geometry: PageGeometry {
            ppix: settings.printer_dpi,
            ppiy: settings.printer_dpi,
            dpi: placeholder_density,
            dot_percent: settings.dot_percent,
            width,
            height,
            // PB-1.10 always prints the sync raster around the data
            // area (Printer.cpp:858-864), and scan_decode's grid
            // finder relies on it to lock onto the dot pattern after
            // any roundtrip introduces sub-pixel drift (PDF page-
            // size rounding, scanner jitter, etc.). Without the
            // border, ampaper-produced PDFs at typical densities
            // (200 dot/in) fail "no SuperBlock decoded" on real
            // scans even when the dots themselves are clean.
            print_border: true,
        },
        redundancy: settings.redundancy,
        compress: settings.compress,
        black: BLACK_PAPER,
        // Compact layout — Print tab definitely doesn't want a full
        // page of SuperBlock-copy fillers when the input is small.
        pad_to_full_page: false,
    }
}
