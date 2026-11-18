//! `map-app` — the desktop shell of the Hytale seed explorer.
//!
//! Placeholder build (T06): the real shell — seed input, panels, the custom
//! paint layer — arrives with T12. This stub exists so the eframe dependency
//! graph and the GUI link build are proven from day one; all logic lives in
//! the headless `map-core`.

use eframe::egui;

fn main() -> eframe::Result<()> {
    env_logger::init();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1024.0, 768.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Hytale Seed Explorer",
        options,
        Box::new(|_cc| Ok(Box::new(StubApp::default()))),
    )
}

/// A do-nothing app: one panel, one label.
#[derive(Default)]
struct StubApp;

impl eframe::App for StubApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Hytale Seed Explorer");
            ui.label(
                "Application shell pending (T12). This stub proves the eframe build; \
                 all logic lives in the headless map-core.",
            );
            ui.label(format!(
                "map-core reports generator_version {}",
                map_core::GENERATOR_VERSION
            ));
        });
    }
}
