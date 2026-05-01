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

use app::AmpaperApp;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1024.0, 720.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("ampaper"),
        ..Default::default()
    };
    eframe::run_native(
        "ampaper",
        options,
        Box::new(|_cc| Ok(Box::new(AmpaperApp::default()))),
    )
}
