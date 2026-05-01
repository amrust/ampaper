// ampaper-gui — egui-based desktop front-end for the ampaper codec.
//
// This binary is a thin shell on top of the platform-agnostic
// `ampaper` library crate. The codec is fully cross-platform; this
// GUI also runs on Windows / Linux / macOS (per
// memory/cross_platform_goal.md). Per-platform tabs (print / scan
// from device) are greyed out where their backing milestones haven't
// shipped or the platform isn't supported yet.

mod app;
mod views;
mod worker;

use app::AmpaperApp;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1024.0, 720.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("ampaper")
            // Suppress eframe's bundled placeholder "egui" icon. Per
            // the eframe / egui docs (epi.rs:314, viewport.rs:420),
            // setting an empty IconData tells the framework to fall
            // back to the OS default — which on Windows means the
            // standard exe icon (and once we have an .ico embedded
            // via a build-resource step, that's what shows up here).
            .with_icon(eframe::egui::IconData::default()),
        ..Default::default()
    };
    eframe::run_native(
        "ampaper",
        options,
        Box::new(|_cc| Ok(Box::new(AmpaperApp::default()))),
    )
}
