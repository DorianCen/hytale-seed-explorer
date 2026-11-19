//! `map-app` — the desktop shell of the Hytale seed explorer (T12).
//!
//! Launch: `cargo run -p map-app`. The window defaults to 1280×800 and
//! expects a world to load — either an extracted asset pack under
//! `Assets/extracted/Server/World/Default` (cwd-relative, as the game
//! tools resolve it) or the golden world copy of the sibling worldgen
//! checkout (`../hytale-server-rust/testdata/Default`). A seed typed into
//! the top bar loads on Go, in the background; everything heavy lives in
//! the headless `map-core`, so the UI thread never blocks on generation.
//!
//! All logic is testable without this binary: [`app`] is the egui skin,
//! [`state`] the headless state machine, [`view_state`] the pan/zoom
//! math, [`map_view`] the interactive map widget, [`labels`] the UI
//! strings.

mod app;
mod labels;
mod map_view;
mod state;
mod texture;
mod view_state;

use eframe::egui;

fn main() -> eframe::Result<()> {
    env_logger::init();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        labels::APP_TITLE,
        options,
        Box::new(|cc| Ok(Box::new(app::SeedExplorerApp::new(cc)))),
    )
}
