// Settings tab — defaults that apply across encode / decode runs.
//
// Scaffolding only. Real wiring lands in subsequent commits. The
// fields here will mirror PaperBack 1.10's "Options" dialog and the
// mrpods INI defaults captured in memory/mrpods_defaults.md:
//   - dpi (default 200)
//   - dot_percent (default 70)
//   - redundancy (default 5)
//   - compression (default Max)
//   - default output directory
//   - v2 KDF iteration count is intentionally NOT user-configurable
//     — it's part of the v2 format, see docs/FORMAT-V2.md §3.2.
//
// Persistence: settings will round-trip through a small key=value
// file under the platform's standard config dir
// (`directories` crate or eframe's `Storage`). No tracking; no
// telemetry.

use eframe::egui;

#[derive(Default)]
pub struct SettingsView {
    // Placeholder.
}

impl SettingsView {
    pub fn show(&mut self, ui: &mut egui::Ui) {
        ui.heading("Settings");
        ui.add_space(6.0);
        ui.label("Defaults applied to new encode and decode runs.");
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(12.0);

        ui.label(
            egui::RichText::new("Settings UI coming next commit.")
                .italics()
                .weak(),
        );
    }
}
