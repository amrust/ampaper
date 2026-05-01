// Decode tab.
//
// Two ways to feed it bitmap input:
//   1. Drag-and-drop one or more BMP / PNG / JPG files onto the
//      window. eframe surfaces these via `ctx.input(...).raw.dropped_files`.
//   2. Click "Open files..." and pick from a native dialog.
//
// On either entry, we kick off a worker thread that:
//   - decodes each image to grayscale,
//   - runs `scan_extract` per page to classify cells (CRC-good →
//     green / blue, CRC-damaged → red),
//   - runs `scan_decode` for the recovered bytes.
//
// The classification grid mirrors PaperBack 1.10's "Read" dialog: a
// per-cell colored grid that gives the user a clear "your scan is
// good / your scan is mostly red, redo it" signal without diving
// into per-dot detail (PaperBack 1.10 had that view too; we drop it
// here because there's nothing actionable a user can do with it).

use std::path::PathBuf;

use eframe::egui;

use crate::worker::{
    CellStatus, DecodeJob, DecodeMessage, DecodePage, DecodeRequest, PageReport,
};

#[derive(Default)]
pub struct DecodeView {
    /// Files queued for decode. Populated by drop / picker, drained
    /// by the worker spawn step.
    queued_paths: Vec<PathBuf>,
    /// Optional password — used for v1-AES-192 prints and v2-encrypted
    /// prints alike. Empty = no password.
    password: String,

    /// Active decode job, if any.
    job: Option<DecodeJob>,
    /// Latest status text (worker progress, error, or "done").
    last_status: String,
    /// Per-page classification reports, accumulated as the worker
    /// emits them. Cleared on each new request.
    reports: Vec<PageReport>,
    /// Recovered plaintext, cleared on each new request.
    last_plaintext: Option<Vec<u8>>,
}

impl DecodeView {
    pub fn show(&mut self, ui: &mut egui::Ui) {
        // Pull any drag-dropped files BEFORE drawing — that way the
        // queue / worker spawn happens this frame instead of next.
        self.poll_dropped_files(ui.ctx());
        self.drain_worker();

        ui.heading("Decode");
        ui.add_space(6.0);
        ui.label(
            "Drag one or more scanned bitmaps into this window, or click \
             \"Open files...\" Reads PaperBack 1.10 v1 prints (incl. legacy \
             AES-192) and ampaper v2 (AES-256-GCM) automatically.",
        );
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);

        let job_running = self.job.is_some();
        ui.add_enabled_ui(!job_running, |ui| {
            self.show_input_row(ui);
            self.show_password_row(ui);
            ui.add_space(6.0);
            self.show_decode_button(ui);
        });

        ui.add_space(12.0);
        if !self.last_status.is_empty() {
            ui.label(egui::RichText::new(&self.last_status).weak());
        }

        if !self.reports.is_empty() {
            ui.add_space(8.0);
            self.show_reports(ui);
        }

        if let Some(save_status) = self.show_save_row(ui) {
            self.last_status = save_status;
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
        if !dropped.is_empty() && self.job.is_none() {
            // New drop replaces any pending queue from a previous
            // drop — the user's intent is "decode THESE files."
            self.queued_paths = dropped;
            self.spawn_decode(ctx);
        }
    }

    fn show_input_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Input bitmap(s):");
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

    fn show_password_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Password (if encrypted):");
            ui.add(
                egui::TextEdit::singleline(&mut self.password)
                    .password(true)
                    .desired_width(260.0),
            );
        });
    }

    fn show_decode_button(&mut self, ui: &mut egui::Ui) {
        let ready = !self.queued_paths.is_empty();
        ui.add_enabled_ui(ready, |ui| {
            if ui.button("Decode").clicked() {
                self.spawn_decode(ui.ctx());
            }
        });
        if !ready {
            ui.label(
                egui::RichText::new("Drop files into the window or pick them.")
                    .small()
                    .weak(),
            );
        }
    }

    fn spawn_decode(&mut self, ctx: &egui::Context) {
        if self.queued_paths.is_empty() {
            return;
        }
        // Reset display state for the new request.
        self.reports.clear();
        self.last_plaintext = None;
        self.last_status = "Loading...".into();

        let mut pages = Vec::with_capacity(self.queued_paths.len());
        for path in &self.queued_paths {
            match load_grayscale(path) {
                Ok((luma, w, h)) => pages.push(DecodePage {
                    source: path.clone(),
                    luma,
                    width: w,
                    height: h,
                }),
                Err(e) => {
                    self.last_status = format!("Failed to load {}: {e}", path.display());
                    return;
                }
            }
        }
        let password = if self.password.is_empty() {
            None
        } else {
            Some(self.password.clone())
        };
        let req = DecodeRequest { pages, password };
        let ctx2 = ctx.clone();
        self.job = Some(DecodeJob::spawn(req, move || ctx2.request_repaint()));
    }

    fn drain_worker(&mut self) {
        let Some(job) = &self.job else {
            return;
        };
        loop {
            match job.rx.try_recv() {
                Ok(DecodeMessage::Started) => self.last_status = "Decoding...".into(),
                Ok(DecodeMessage::Status(s)) => self.last_status = s,
                Ok(DecodeMessage::PageClassified(r)) => self.reports.push(r),
                Ok(DecodeMessage::Done { plaintext }) => {
                    self.last_status = format!("Done. Recovered {} bytes.", plaintext.len());
                    self.last_plaintext = Some(plaintext);
                    self.job = None;
                    return;
                }
                Ok(DecodeMessage::Failed(e)) => {
                    self.last_status = format!("Failed: {e}");
                    self.last_plaintext = None;
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

    /// Render the "recovered N bytes / Save as..." row. Returns
    /// `Some(status_string)` when the user clicked Save and we
    /// either wrote the file or failed; the caller writes that into
    /// `self.last_status` so we don't have to borrow self mutably
    /// inside the closure.
    fn show_save_row(&self, ui: &mut egui::Ui) -> Option<String> {
        let plaintext = self.last_plaintext.as_ref()?;
        ui.add_space(8.0);
        let mut new_status = None;
        ui.horizontal(|ui| {
            ui.label(format!(
                "Recovered {} byte{}.",
                plaintext.len(),
                if plaintext.len() == 1 { "" } else { "s" }
            ));
            if ui.button("Save as...").clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .set_file_name(self.suggested_save_name())
                    .save_file()
            {
                new_status = Some(match std::fs::write(&path, plaintext) {
                    Ok(()) => format!("Saved {}", path.display()),
                    Err(e) => format!("Save failed: {e}"),
                });
            }
        });
        new_status
    }

    fn show_reports(&self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Page status").strong());
        ui.add_space(4.0);
        // Legend.
        ui.horizontal(|ui| {
            legend_swatch(ui, status_color(CellStatus::DataOk), "Data");
            legend_swatch(ui, status_color(CellStatus::Super), "SuperBlock");
            legend_swatch(ui, status_color(CellStatus::Recovery), "Recovery");
            legend_swatch(ui, status_color(CellStatus::Damaged), "Damaged");
        });
        ui.add_space(6.0);

        for report in &self.reports {
            ui.label(
                egui::RichText::new(report.source.display().to_string())
                    .small()
                    .weak(),
            );
            paint_grid(ui, report);
            ui.add_space(8.0);
        }
    }

    fn suggested_save_name(&self) -> String {
        // Prefer the original filename the encoder embedded in the
        // SuperBlock — PB 1.10 stores it in name[0..32], v2 in
        // V2SuperBlockCell1.name[0..64]. Walk the per-page reports
        // for the first one that recovered a name.
        for r in &self.reports {
            if let Some(name) = &r.original_filename
                && !name.is_empty()
            {
                return name.clone();
            }
        }
        // Fall back to the input bitmap's stem with a `.recovered`
        // marker so the user doesn't accidentally overwrite the
        // bitmap they dropped in.
        if let Some(first) = self.queued_paths.first()
            && let Some(stem) = first.file_stem().and_then(|s| s.to_str())
        {
            return format!("{stem}.recovered.bin");
        }
        "ampaper-recovered.bin".into()
    }
}

fn status_color(s: CellStatus) -> egui::Color32 {
    // Colors chosen to be distinguishable in dark mode AND match
    // PB 1.10's posture: green = good, red = bad, blue = control.
    match s {
        CellStatus::DataOk => egui::Color32::from_rgb(72, 168, 96), // green
        CellStatus::Super => egui::Color32::from_rgb(80, 132, 200), // blue
        CellStatus::Recovery => egui::Color32::from_rgb(160, 110, 200), // purple-ish blue
        CellStatus::Damaged => egui::Color32::from_rgb(200, 70, 70), // red
    }
}

fn legend_swatch(ui: &mut egui::Ui, color: egui::Color32, label: &str) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 2.0, color);
    ui.label(label);
    ui.add_space(8.0);
}

fn paint_grid(ui: &mut egui::Ui, report: &PageReport) {
    // Maximum width we let the grid grow to. With nx columns we
    // pick a cell size that makes the whole grid fit inside the
    // available width, capped so we don't draw huge tiles when the
    // page only has a few cells.
    let available = ui.available_width().max(160.0);
    let max_cell = 14.0;
    let cell_size = (available / report.nx.max(1) as f32).min(max_cell);
    let gap = (cell_size * 0.08).clamp(0.5, 2.0);
    let pitch = cell_size + gap;
    let total_w = pitch * report.nx as f32;
    let total_h = pitch * report.ny as f32;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(total_w, total_h), egui::Sense::hover());
    let painter = ui.painter_at(rect);

    for y in 0..report.ny {
        for x in 0..report.nx {
            let idx = (y * report.nx + x) as usize;
            let status = *report.cells.get(idx).unwrap_or(&CellStatus::Damaged);
            let p = egui::pos2(
                rect.left() + x as f32 * pitch,
                rect.top() + y as f32 * pitch,
            );
            let cell_rect = egui::Rect::from_min_size(p, egui::vec2(cell_size, cell_size));
            painter.rect_filled(cell_rect, 1.5, status_color(status));
        }
    }
}

/// Decode an image file from disk into an 8-bit grayscale bitmap.
/// Accepts BMP, PNG, JPG (whichever the `image` crate's enabled
/// features cover). Returns (luma_pixels, width, height).
fn load_grayscale(path: &std::path::Path) -> Result<(Vec<u8>, u32, u32), String> {
    let img = image::open(path).map_err(|e| format!("{e}"))?;
    let luma = img.to_luma8();
    let (w, h) = luma.dimensions();
    Ok((luma.into_raw(), w, h))
}
