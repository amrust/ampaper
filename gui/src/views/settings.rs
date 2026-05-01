// Settings tab — app-wide preferences and a "reset to defaults"
// escape hatch for the encode panel.
//
// Encode-specific settings (paper size, dpi, redundancy, etc.) live
// on the Encode tab itself and are persisted via eframe's `save`
// hook in app.rs. The Settings tab here is for app-wide stuff:
// theme, version info, and the reset-to-defaults button. Future
// candidates: default output directory, theme accent color, "always
// emit v2" toggle.

use eframe::egui;

#[derive(Default)]
pub struct SettingsView {
    /// Set when the user clicks "Reset to defaults" so the tab
    /// dispatcher in app.rs can pull encoder defaults back. The flag
    /// is consumed by the App on the same frame — drained, not held.
    pub reset_requested: bool,
}

impl SettingsView {
    pub fn show(&mut self, ui: &mut egui::Ui) {
        ui.heading("Settings");
        ui.add_space(6.0);
        ui.label("Application-wide preferences. Per-encode and per-decode options live in their respective tabs.");
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);

        self.show_theme_section(ui);
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);
        self.show_reset_section(ui);
        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);
        self.show_about_section(ui);
    }

    fn show_theme_section(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Theme").strong());
        ui.add_space(4.0);
        // egui ships its own theme switcher widget that handles
        // dark / light / "follow system" — and persists across
        // sessions automatically when eframe persistence is enabled.
        egui::widgets::global_theme_preference_buttons(ui);
    }

    fn show_reset_section(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Reset").strong());
        ui.add_space(4.0);
        ui.label(
            "Restore the Encode tab's geometry, redundancy, and \
             compression options to their PaperBack 1.10 defaults \
             (200 dpi, 70% dot fill, redundancy 5, compression on).",
        );
        ui.add_space(6.0);
        if ui.button("Reset Encode defaults").clicked() {
            self.reset_requested = true;
        }
    }

    fn show_about_section(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("About").strong());
        ui.add_space(4.0);
        ui.label(format!("ampaper v{}", env!("CARGO_PKG_VERSION")));
        ui.label(
            egui::RichText::new(
                "Rust port of Oleh Yuschuk's PaperBack 1.10. \
                 Reads existing PB 1.10 prints; writes ampaper v1 \
                 (binary-compatible) and v2 (AES-256-GCM).",
            )
            .small()
            .weak(),
        );
    }
}
