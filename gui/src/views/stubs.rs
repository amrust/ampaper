// Tabs that exist in the navigation rail but aren't backed by a
// landed milestone yet. Each renders a "coming in M<n>" placeholder
// so the layout settles now and the user gets a clear signal about
// what's not available.
//
// When M10 (scanning from device) lands, this stub gets replaced by
// a real view module. M9 (printing) already landed — see views::print.

use eframe::egui;

pub fn show_scan_device_stub(ui: &mut egui::Ui) {
    ui.heading("Scan from device");
    ui.add_space(6.0);
    ui.label(
        "Drive a scanner directly. This tab lands with M10 (Windows \
         WIA + TWAIN bridge). For now, scan with whatever software \
         you have, save as PNG / JPG / BMP, and use the Decode tab.",
    );
    ui.add_space(12.0);
    if !cfg!(target_os = "windows") {
        ui.label(
            egui::RichText::new(
                "Native scanner driver support is Windows-only at \
                 first. Linux (SANE) and macOS (ICA) follow later.",
            )
            .weak(),
        );
    } else {
        ui.label(
            egui::RichText::new("Coming with M10 — WIA / TWAIN bridge.")
                .italics()
                .weak(),
        );
    }
}
