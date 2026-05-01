// Top-level app state and frame dispatch.
//
// The GUI is a single window with a left-side navigation rail. Each
// tab is its own module under `views/`; this file holds the enum
// dispatch and the cross-tab state (settings, last error, worker
// channel handle when one is running).

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::views;

/// Which tab is currently visible in the central panel.
///
/// Order matters: tabs are drawn in this order in the side rail.
/// Encode / Decode / Settings are always functional. Print and
/// "Scan from device" are platform-gated (Windows-only at M9 / M10)
/// and render a "not yet available" placeholder elsewhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
            // M9 ships Windows GDI printing; the view itself compiles
            // everywhere but the print call is Windows-only and the
            // dialog is Win32-modal. Show as available on Windows,
            // greyed elsewhere.
            Self::Print => cfg!(target_os = "windows"),
            // M10 (scanning from device) is still a pure stub; the
            // file-decode path is on the Decode tab already.
            Self::ScanDevice => false,
        }
    }
}

pub struct AmpaperApp {
    pub tab: Tab,
    pub encode: views::encode::EncodeView,
    pub decode: views::decode::DecodeView,
    pub print: views::print::PrintView,
    pub settings: views::settings::SettingsView,
}

/// Subset of [`AmpaperApp`] state that round-trips through eframe's
/// persistence layer (RON-serialized blob in `%APPDATA%\ampaper\` etc.).
/// Excludes everything session-only: file paths, passwords, worker
/// handles, status messages.
///
/// This struct is the single point of contact for "what's saved."
/// Adding a field requires two edits: declare it here and wire it in
/// `AmpaperApp::new` / `AmpaperApp::save`.
#[derive(Default, Serialize, Deserialize)]
struct PersistedState {
    last_tab: Option<Tab>,
    encode_settings: views::encode::EncodeSettings,
}

impl Default for AmpaperApp {
    fn default() -> Self {
        Self {
            tab: Tab::Encode,
            encode: Default::default(),
            decode: Default::default(),
            print: Default::default(),
            settings: Default::default(),
        }
    }
}

impl AmpaperApp {
    /// Construct from an [`eframe::CreationContext`], hydrating any
    /// persisted state. Falls back to defaults when no storage entry
    /// exists or deserialization fails (e.g., schema drift between
    /// ampaper versions — fail open, not loud).
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let persisted: PersistedState = cc
            .storage
            .and_then(|s| eframe::get_value(s, eframe::APP_KEY))
            .unwrap_or_default();
        Self {
            tab: persisted.last_tab.unwrap_or(Tab::Encode),
            encode: views::encode::EncodeView::with_settings(persisted.encode_settings),
            decode: Default::default(),
            print: Default::default(),
            settings: Default::default(),
        }
    }
}

impl eframe::App for AmpaperApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let state = PersistedState {
            last_tab: Some(self.tab),
            encode_settings: self.encode.settings.clone(),
        };
        eframe::set_value(storage, eframe::APP_KEY, &state);
    }

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
            Tab::Print => self.print.show(ui),
            Tab::ScanDevice => views::stubs::show_scan_device_stub(ui),
        });

        // Drain any per-tab signals raised this frame.
        if std::mem::take(&mut self.settings.reset_requested) {
            self.encode.settings = views::encode::EncodeSettings::default();
        }
    }
}

impl AmpaperApp {
    fn status_text(&self) -> String {
        // Placeholder until worker-thread + progress channel land.
        format!("Ready · {}", self.tab.label())
    }
}
