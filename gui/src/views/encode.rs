// Encode tab.
//
// Flow:
//   1. User picks an input file.
//   2. User picks an output directory (or uses the input's parent).
//   3. User adjusts options (geometry, redundancy, compression, v2
//      password). Defaults match mrpods / PB 1.10 (200 dpi, 70% dot
//      fill, redundancy 5, max compression).
//   4. User clicks "Encode." We spawn a worker thread and watch
//      its mpsc channel for status / done / fail.
//   5. On Done, we list the files written. On Fail, we show the
//      error in the status area.
//
// Geometry note: the codec's PageGeometry takes pixel-domain inputs
// (ppix/ppiy = printer DPI, dpi = blocks per inch, dot_percent =
// fraction of pitch the inked dot covers, width/height = pixels per
// page). For now we expose dpi + dot_percent + a paper-size preset
// (US Letter / A4) and derive ppix/ppiy/width/height from that.

use std::path::PathBuf;

use ampaper::block::{NGROUP_DEFAULT, NGROUP_MAX, NGROUP_MIN};
use ampaper::encoder::EncodeOptions;
use ampaper::page::{BLACK_PAPER, PageGeometry};
use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::worker::{EncodeJob, EncodeMessage, EncodeRequest};

/// Paper-size preset for the geometry section.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaperSize {
    UsLetter,
    A4,
}

impl PaperSize {
    fn label(self) -> &'static str {
        match self {
            Self::UsLetter => "US Letter (8.5 × 11 in)",
            Self::A4 => "A4 (210 × 297 mm)",
        }
    }

    /// Page dimensions in (width_inches, height_inches). Used with
    /// `printer_dpi` to derive pixel width/height.
    fn inches(self) -> (f32, f32) {
        match self {
            Self::UsLetter => (8.5, 11.0),
            Self::A4 => (8.27, 11.69),
        }
    }
}

/// Encoder settings that persist across sessions. Excludes session-
/// only state (file paths, password, worker handle, results). Saved
/// to disk via eframe's persistence layer; hydrated at app launch.
/// Defaults match mrpods / PaperBack 1.10 (memory/mrpods_defaults.md).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EncodeSettings {
    pub paper_size: PaperSize,
    /// Printer DPI. PB 1.10 default is 600 (most consumer lasers).
    pub printer_dpi: u32,
    /// Blocks-per-inch. PB 1.10 default is 200.
    pub blocks_per_inch: u32,
    /// Dot fill percentage. PB 1.10 default is 70. Stored as u8 to
    /// match `PageGeometry::dot_percent`.
    pub dot_percent: u8,
    pub redundancy: u8,
    pub compress: bool,
    /// "Default to v2 encryption" — controls the checkbox state when
    /// the Encode tab opens. The password itself is NEVER persisted
    /// (security: an archival utility shouldn't tempt users into
    /// keeping passwords in plaintext config).
    pub v2_encrypt: bool,
}

impl Default for EncodeSettings {
    fn default() -> Self {
        Self {
            paper_size: PaperSize::UsLetter,
            printer_dpi: 600,
            blocks_per_inch: 200,
            dot_percent: 70,
            redundancy: NGROUP_DEFAULT,
            compress: true,
            v2_encrypt: false,
        }
    }
}

/// The state for the encode tab. Persisted defaults live in
/// `settings`; everything else is session-only.
pub struct EncodeView {
    pub settings: EncodeSettings,
    input_path: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    v2_password: String,
    /// Active worker, if any.
    job: Option<EncodeJob>,
    /// Latest status message from the worker (or last result/error).
    last_status: String,
    /// Files the most recent successful job produced.
    last_outputs: Vec<PathBuf>,
}

impl Default for EncodeView {
    fn default() -> Self {
        Self::with_settings(EncodeSettings::default())
    }
}

impl EncodeView {
    pub fn with_settings(settings: EncodeSettings) -> Self {
        Self {
            settings,
            input_path: None,
            output_dir: None,
            v2_password: String::new(),
            job: None,
            last_status: String::new(),
            last_outputs: Vec::new(),
        }
    }
}

impl EncodeView {
    pub fn show(&mut self, ui: &mut egui::Ui) {
        // Drag-and-drop: take the FIRST dropped file as the input.
        // Encode is single-input by definition (one file → bitmaps),
        // so multi-drop is fine but only the first file is used. UX
        // mirrors the Decode tab.
        self.poll_dropped_files(ui.ctx());

        // Drain any pending worker messages first so the UI shown
        // this frame reflects the latest state.
        self.drain_worker();

        ui.heading("Encode");
        ui.add_space(6.0);
        ui.label(
            "Pick a file to encode into one or more printable bitmaps. \
             Output is byte-compatible with PaperBack 1.10 (v1) by default, \
             or uses the AES-256-GCM v2 envelope when a password is set.",
        );
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);

        let job_running = self.job.is_some();
        ui.add_enabled_ui(!job_running, |ui| {
            self.show_input_row(ui);
            self.show_output_row(ui);
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(6.0);
            self.show_options(ui);
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(6.0);
            self.show_v2_section(ui);
            ui.add_space(10.0);
            self.show_encode_button(ui);
        });

        ui.add_space(12.0);
        if !self.last_status.is_empty() {
            ui.label(egui::RichText::new(&self.last_status).weak());
        }

        if !self.last_outputs.is_empty() {
            ui.add_space(8.0);
            ui.label(format!(
                "Wrote {} file{}:",
                self.last_outputs.len(),
                if self.last_outputs.len() == 1 { "" } else { "s" }
            ));
            for path in &self.last_outputs {
                ui.monospace(path.display().to_string());
            }
        }
    }

    fn show_input_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Input file:");
            let display = self
                .input_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(drop a file here, or use the button)".to_string());
            ui.monospace(display);
        });
        ui.horizontal(|ui| {
            if ui.button("Choose file...").clicked() {
                if let Some(path) = rfd::FileDialog::new().pick_file() {
                    // Default the output dir to the input's parent
                    // when nothing else is set.
                    if self.output_dir.is_none() {
                        self.output_dir = path.parent().map(|p| p.to_path_buf());
                    }
                    self.input_path = Some(path);
                }
            }
            if self.input_path.is_some() && ui.button("Clear").clicked() {
                self.input_path = None;
            }
        });
    }

    fn show_output_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Output dir:");
            let display = self
                .output_dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(none)".to_string());
            ui.monospace(display);
        });
        ui.horizontal(|ui| {
            if ui.button("Choose folder...").clicked()
                && let Some(dir) = rfd::FileDialog::new().pick_folder()
            {
                self.output_dir = Some(dir);
            }
        });
    }

    fn show_options(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Geometry").strong());
        egui::ComboBox::from_label("Paper size")
            .selected_text(self.settings.paper_size.label())
            .show_ui(ui, |ui| {
                for size in [PaperSize::UsLetter, PaperSize::A4] {
                    ui.selectable_value(&mut self.settings.paper_size, size, size.label());
                }
            });
        ui.horizontal(|ui| {
            ui.label("Printer DPI:");
            ui.add(egui::DragValue::new(&mut self.settings.printer_dpi).range(150..=2400));
        });
        ui.horizontal(|ui| {
            ui.label("Blocks per inch:");
            ui.add(egui::DragValue::new(&mut self.settings.blocks_per_inch).range(50..=400));
        });
        ui.horizontal(|ui| {
            ui.label("Dot fill (%):");
            ui.add(egui::Slider::new(&mut self.settings.dot_percent, 30..=95));
        });
        ui.add_space(6.0);
        ui.label(egui::RichText::new("Redundancy").strong());
        ui.horizontal(|ui| {
            ui.label("Recovery group size:");
            ui.add(egui::Slider::new(
                &mut self.settings.redundancy,
                NGROUP_MIN..=NGROUP_MAX,
            ));
        });
        ui.checkbox(&mut self.settings.compress, "bzip2-compress before encoding");
    }

    fn show_v2_section(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Encryption (v2)").strong());
        ui.checkbox(
            &mut self.settings.v2_encrypt,
            "Encrypt with AES-256-GCM (emit v2 format)",
        );
        if self.settings.v2_encrypt {
            ui.horizontal(|ui| {
                ui.label("Password:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.v2_password)
                        .password(true)
                        .desired_width(260.0),
                );
            });
            ui.label(
                egui::RichText::new(
                    "v2 prints can ONLY be read by ampaper. PaperBack 1.10 \
                     cannot decode them.",
                )
                .small()
                .weak(),
            );
        }
    }

    fn show_encode_button(&mut self, ui: &mut egui::Ui) {
        let ready = self.input_path.is_some()
            && self.output_dir.is_some()
            && (!self.settings.v2_encrypt || !self.v2_password.is_empty());
        ui.add_enabled_ui(ready, |ui| {
            if ui.button("Encode").clicked()
                && let Some(req) = self.build_request()
            {
                let ctx = ui.ctx().clone();
                self.job = Some(EncodeJob::spawn(req, move || ctx.request_repaint()));
                self.last_status = "Starting...".into();
                self.last_outputs.clear();
            }
        });
        if !ready {
            let why = if self.input_path.is_none() {
                "Pick an input file."
            } else if self.output_dir.is_none() {
                "Pick an output directory."
            } else {
                "Set a v2 password (or disable encryption)."
            };
            ui.label(egui::RichText::new(why).small().weak());
        }
    }

    fn build_request(&self) -> Option<EncodeRequest> {
        let input_path = self.input_path.clone()?;
        let output_dir = self.output_dir.clone()?;
        let stem = input_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("ampaper")
            .to_string();
        let (in_w, in_h) = self.settings.paper_size.inches();
        let width = (in_w * self.settings.printer_dpi as f32) as u32;
        let height = (in_h * self.settings.printer_dpi as f32) as u32;
        let geometry = PageGeometry {
            ppix: self.settings.printer_dpi,
            ppiy: self.settings.printer_dpi,
            dpi: self.settings.blocks_per_inch,
            dot_percent: self.settings.dot_percent,
            width,
            height,
            print_border: false,
        };
        let options = EncodeOptions {
            geometry,
            redundancy: self.settings.redundancy,
            compress: self.settings.compress,
            black: BLACK_PAPER,
        };
        let v2_password = if self.settings.v2_encrypt {
            Some(self.v2_password.clone())
        } else {
            None
        };
        Some(EncodeRequest {
            input_path,
            output_dir,
            output_stem: stem,
            options,
            v2_password,
        })
    }

    fn poll_dropped_files(&mut self, ctx: &egui::Context) {
        // Only consume drops when we're not mid-encode AND have
        // nothing already queued the user might be looking at — drag
        // is a "set this up" gesture, not a "redo it" one.
        if self.job.is_some() {
            return;
        }
        let dropped: Option<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .find_map(|f| f.path.clone())
        });
        if let Some(path) = dropped {
            if self.output_dir.is_none() {
                self.output_dir = path.parent().map(|p| p.to_path_buf());
            }
            self.input_path = Some(path);
        }
    }

    fn drain_worker(&mut self) {
        let Some(job) = &self.job else {
            return;
        };
        loop {
            match job.rx.try_recv() {
                Ok(EncodeMessage::Started) => {
                    self.last_status = "Encoding...".into();
                }
                Ok(EncodeMessage::Status(s)) => {
                    self.last_status = s;
                }
                Ok(EncodeMessage::Done { files }) => {
                    self.last_status = format!("Done. Wrote {} file(s).", files.len());
                    self.last_outputs = files;
                    self.job = None;
                    return;
                }
                Ok(EncodeMessage::Failed(e)) => {
                    self.last_status = format!("Failed: {e}");
                    self.last_outputs.clear();
                    self.job = None;
                    return;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.job = None;
                    return;
                }
            }
        }
    }
}
