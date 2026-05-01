// View modules — one per tab. Each owns its own state struct and
// exposes a `show(ui)` method called from app.rs's central-panel
// dispatch.
//
// Keeping tab state local to each module (rather than flattened into
// AmpaperApp) keeps the app shell agnostic to per-tab specifics —
// when M9 / M10 land we can grow the print + scan_device modules
// without touching the dispatcher.

pub mod decode;
pub mod encode;
pub mod settings;
pub mod stubs;
