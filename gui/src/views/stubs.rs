// Tabs that exist in the navigation rail but aren't backed by a
// landed milestone yet. Each renders a "coming in M<n>" placeholder
// so the layout settles now and the user gets a clear signal about
// what's not available.
//
// When M9 (printing) and M10 (scanning) land, these will be replaced
// by real view modules.

use eframe::egui;

pub fn show_print_stub(ui: &mut egui::Ui) {
    ui.heading("Print");
    ui.add_space(6.0);
    ui.label(
        "Send encoded bitmaps directly to a printer. This tab \
         lands with M9 (Windows GDI printing). Until then, save \
         bitmaps from the Encode tab and print them with whatever \
         tool you'd normally use.",
    );
    ui.add_space(12.0);
    if !cfg!(target_os = "windows") {
        ui.label(
            egui::RichText::new(
                "Native printer driver support is Windows-only at \
                 first. Linux (CUPS) and macOS (CGContext) follow \
                 once Windows is solid.",
            )
            .weak(),
        );
    } else {
        ui.label(
            egui::RichText::new("Coming with M9 — Windows GDI printing.")
                .italics()
                .weak(),
        );
    }
}

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
