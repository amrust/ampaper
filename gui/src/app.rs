// Top-level app state and frame dispatch.
//
// The GUI is a single window with a left-side navigation rail. Each
// tab is its own module under `views/`; this file holds the enum
// dispatch and the cross-tab state (settings, last error, worker
// channel handle when one is running).

use eframe::egui;

use crate::views;

/// Which tab is currently visible in the central panel.
///
/// Order matters: tabs are drawn in this order in the side rail.
/// Encode / Decode / Settings are always functional. Print and
/// "Scan from device" are platform-gated (Windows-only at M9 / M10)
/// and render a "not yet available" placeholder elsewhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Encode,
    Decode,
    Settings,
    Print,
    ScanDevice,
}

impl Tab {
    /// Display label for the side-rail button.
    pub fn label(self) -> &'static str {
        match self {
            Self::Encode => "Encode",
            Self::Decode => "Decode",
            Self::Settings => "Settings",
            Self::Print => "Print",
            Self::ScanDevice => "Scan from device",
        }
    }

    /// True when this tab is fully implemented on the current
    /// platform. Tabs that are not available render a stub message
    /// and the side-rail label is rendered weak/dimmed.
    pub fn is_available(self) -> bool {
        match self {
            Self::Encode | Self::Decode | Self::Settings => true,
            // M9 (printing) and M10 (scanning) ship Windows-first.
            // Even on Windows they are currently stubs until those
            // milestones land — we expose the tabs now so the layout
            // settles, but they render "coming in M9 / M10."
            Self::Print | Self::ScanDevice => false,
        }
    }
}

pub struct AmpaperApp {
    pub tab: Tab,
    pub encode: views::encode::EncodeView,
    pub decode: views::decode::DecodeView,
    pub settings: views::settings::SettingsView,
}

impl Default for AmpaperApp {
    fn default() -> Self {
        Self {
            tab: Tab::Encode,
            encode: Default::default(),
            decode: Default::default(),
            settings: Default::default(),
        }
    }
}

impl eframe::App for AmpaperApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Left navigation rail. Fixed width so the central content
        // doesn't reflow when tab labels change (or when a long
        // status message lands later).
        egui::Panel::left("ampaper-nav")
            .exact_size(180.0)
            .show_inside(ui, |ui| {
                ui.add_space(8.0);
                ui.heading("ampaper");
                ui.label(egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION"))).weak());
                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);

                for tab in [
                    Tab::Encode,
                    Tab::Decode,
                    Tab::Settings,
                    Tab::Print,
                    Tab::ScanDevice,
                ] {
                    let label = tab.label();
                    let text = if tab.is_available() {
                        egui::RichText::new(label)
                    } else {
                        egui::RichText::new(label).weak()
                    };
                    let selected = self.tab == tab;
                    if ui.selectable_label(selected, text).clicked() {
                        self.tab = tab;
                    }
                }
            });

        // Status / footer bar — anchor for "encoding page 3/12..."
        // updates once the worker thread + progress channel land.
        egui::Panel::bottom("ampaper-status")
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(self.status_text());
                });
            });

        // Central area: tab dispatch. The bare `ui` from App::ui has
        // no margin or background color (per eframe 0.34 docs), so
        // wrap it in CentralPanel to pick up the standard panel_fill
        // — otherwise the central region falls through to the much
        // darker window fill, which makes the content area look
        // pitch-black against the medium-grey side panels.
        egui::CentralPanel::default().show_inside(ui, |ui| match self.tab {
            Tab::Encode => self.encode.show(ui),
            Tab::Decode => self.decode.show(ui),
            Tab::Settings => self.settings.show(ui),
            Tab::Print => views::stubs::show_print_stub(ui),
            Tab::ScanDevice => views::stubs::show_scan_device_stub(ui),
        });
    }
}

impl AmpaperApp {
    fn status_text(&self) -> String {
        // Placeholder until worker-thread + progress channel land.
        format!("Ready · {}", self.tab.label())
    }
}
