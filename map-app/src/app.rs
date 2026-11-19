//! The egui skin over the headless [`crate::state`] (T12).
//!
//! Everything testable lives in `state.rs`; this module is layout and
//! wiring only: the plan's three regions (top bar, left panel, central
//! map), the status bar, keyboard shortcuts and the background source
//! load. No worldgen work ever happens on the UI thread — loads run on
//! a spawned thread and results arrive through a channel polled with
//! `try_recv` (plus a `request_repaint_after` nudge so the UI stays idle
//! otherwise).

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use eframe::egui;
use egui::{
    Align2, Color32, Context, Id, Key, KeyboardShortcut, Modifiers, RichText, TextStyle, Vec2,
};

use map_core::render::render_biome_rgba;
use map_core::source::WorldSource;
use map_core::source::worldgen::default_world_path;

use crate::labels;
use crate::state::{self, AppLayer, AppState, LoadRequest, LoadResult, RESOLUTION_STEPS};
use crate::texture::{MapTexture, texture_screen_rect};

/// How often to wake the UI while a background load is in flight.
const LOAD_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// The egui app: headless state plus the UI wiring.
pub struct SeedExplorerApp {
    state: AppState,
    /// Result channel of the single in-flight load, when there is one.
    load_rx: Option<Receiver<LoadResult>>,
    /// Result channel of the single in-flight texture grid request.
    grid_rx: Option<Receiver<state::GridResult>>,
    /// The central map area (a placeholder until T16).
    map: MapView,
}

impl SeedExplorerApp {
    /// The `eframe` constructor: the default world path and the plan's
    /// defaults. `cc` is unused — no custom fonts yet (monospace is a
    /// built-in egui style).
    pub fn new(_cc: &eframe::CreationContext) -> Self {
        SeedExplorerApp::with_state(AppState::new(default_world_path()))
    }

    /// Constructs the app around a prepared state — the headless test
    /// entry point (no window, no `CreationContext`).
    pub fn with_state(state: AppState) -> Self {
        SeedExplorerApp {
            state,
            load_rx: None,
            grid_rx: None,
            map: MapView::default(),
        }
    }

    /// One frame.
    fn update_inner(&mut self, ctx: &Context) {
        // Order matters: the status bar and top bar claim their strips
        // first, the left panel its column, and the central panel fills
        // the remainder.
        self.poll_load_channel(ctx);
        self.poll_grid_channel(ctx);
        self.keyboard_shortcuts(ctx);

        // Once a source is ready, request the biome grid for the view
        // (off-thread) and upload whatever has arrived.
        self.maybe_request_texture();
        self.upload_due_texture(ctx);

        // An invalidated source must not leave its map on screen.
        if *self.state.map_texture() == state::MapTexture::Empty {
            self.map.clear_texture();
        }

        self.status_bar(ctx);
        self.top_bar(ctx);
        self.error_banner(ctx);
        self.left_panel(ctx);

        // `flash_search` is consumed before the panel reads it — F
        // highlights the search section for one frame.
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(egui::Color32::from_rgb(16, 16, 20)))
            .show(ctx, |ui| {
                self.map.ui(ui, &self.state);
            });

        if self.state.show_go_to() {
            self.go_to_dialog(ctx);
        }

        // Heavy sources never drop here: hand them to a detached thread.
        // Cancel first so any in-flight region bands bail at their next
        // row boundary instead of being waited out by the pool's join.
        let retired = self.state.take_retired_sources();
        if !retired.is_empty() {
            for source in &retired {
                source.cancel_pending();
            }
            std::thread::spawn(move || drop(retired));
        }
    }

    // -- Background loading ---------------------------------------------------

    /// Spawns the one background thread a load needs. The UI thread never
    /// constructs a `WorldgenSource` (its worker pool costs ~1 s per
    /// worker to load); this thread builds it and sends the result home.
    fn spawn_load(&mut self, request: LoadRequest) {
        let (sender, receiver) = std::sync::mpsc::channel::<LoadResult>();
        self.load_rx = Some(receiver);
        std::thread::spawn(move || {
            let result = map_core::source::worldgen::WorldgenSource::new(&request.config());
            // A send failure just means the UI is gone; drop the result
            // (and the source, off the UI thread, as it happens).
            let _ = sender.send((request.generation, result));
        });
    }

    /// Polls the load channel without blocking. Empty → repaint soon;
    /// disconnected while still loading → the worker died, fail the load
    /// so the banner offers a retry instead of an eternal spinner.
    fn poll_load_channel(&mut self, ctx: &Context) {
        let Some(receiver) = self.load_rx.as_ref() else {
            return;
        };
        match receiver.try_recv() {
            Ok((generation, result)) => {
                self.state.apply_load_result(generation, result);
            }
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(LOAD_POLL_INTERVAL);
            }
            Err(TryRecvError::Disconnected) => {
                self.load_rx = None;
                if self.state.is_loading() {
                    self.state.load_worker_died();
                }
            }
        }
    }

    // -- Map texture (T13) ----------------------------------------------

    /// Polls the texture-grid channel: same pattern as the load channel,
    /// one result per request, stale results dropped by the state.
    fn poll_grid_channel(&mut self, ctx: &Context) {
        let Some(receiver) = self.grid_rx.as_ref() else {
            return;
        };
        match receiver.try_recv() {
            Ok((rect, step, result)) => {
                self.state.apply_grid_result(rect, step, result);
                self.grid_rx = None;
            }
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(LOAD_POLL_INTERVAL);
            }
            Err(TryRecvError::Disconnected) => {
                self.grid_rx = None;
                if self.state.is_texture_pending() {
                    self.state.grid_worker_died();
                }
            }
        }
    }

    /// Requests the biome grid for the current view, once a source is
    /// ready and no request is in flight. The generation happens on a
    /// spawned thread — `update` never touches the generator.
    fn maybe_request_texture(&mut self) {
        if self.state.source().is_none() || self.grid_rx.is_some() {
            return;
        }
        let screen = self.map.screen();
        let step = self.state.resolution_step();
        let Some(rect) =
            state::view_texture_rect(self.state.center(), self.state.scale(), step, screen)
        else {
            return;
        };
        if !self.state.request_view_texture(rect, step) {
            return;
        }
        let Some(source) = self.state.source_arc() else {
            return;
        };
        let (sender, receiver) = std::sync::mpsc::channel::<state::GridResult>();
        self.grid_rx = Some(receiver);
        std::thread::spawn(move || {
            let result = source.biome_region(rect, step);
            let _ = sender.send((rect, step, result));
        });
    }

    /// Uploads an arrived grid as the map texture (the one egui-side
    /// copy; the upload itself is a GPU memcpy, safe on the UI thread).
    fn upload_due_texture(&mut self, ctx: &Context) {
        let Some((grid, rect, step)) = self.state.grid_to_upload() else {
            return;
        };
        let catalog = self
            .state
            .source()
            .expect("an arrived grid implies a loaded source")
            .biome_catalog();
        let image = render_biome_rgba(grid, catalog);
        self.map.texture = Some(MapTexture::upload(ctx, &image, rect, step));
        self.state.mark_uploaded();
    }

    // -- Keyboard -------------------------------------------------------------

    /// Global shortcuts, none of which may steal keys from a focused text
    /// field: `G` go-to, `F` search focus, `Ctrl+S` export placeholder.
    ///
    /// `consume_shortcut` both checks and consumes the press, so the same
    /// key never leaks into the frame's other handling; `COMMAND` is the
    /// cross-platform Ctrl/Cmd spelling.
    fn keyboard_shortcuts(&mut self, ctx: &Context) {
        if ctx.wants_keyboard_input() {
            return;
        }
        let go_to = KeyboardShortcut::new(Modifiers::NONE, Key::G);
        let search = KeyboardShortcut::new(Modifiers::NONE, Key::F);
        let export = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
        if ctx.input_mut(|input| input.consume_shortcut(&go_to)) {
            self.state.toggle_go_to();
        }
        if ctx.input_mut(|input| input.consume_shortcut(&search)) {
            self.state.focus_search();
        }
        if ctx.input_mut(|input| input.consume_shortcut(&export)) {
            self.state.export_placeholder();
        }
    }

    // -- Panels ----------------------------------------------------------------

    /// The plan's top bar: seed field, Go, version, resolution selector.
    fn top_bar(&mut self, ctx: &Context) {
        egui::TopBottomPanel::top(Id::new("top-bar")).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(labels::SEED);

                // The edit happens inside the closure; `set_seed_text`
                // re-parses and invalidates on the way out.
                let response = self.state.with_seed_text(|text| {
                    ui.add(
                        egui::TextEdit::singleline(text)
                            .font(TextStyle::Monospace)
                            .desired_width(140.0)
                            .hint_text("numeric"),
                    )
                });

                // Enter in the seed field triggers Go (singleline edits
                // lose focus on Enter).
                let enter_pressed =
                    response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                let can_go = !self.state.is_loading() && self.state.parsed_seed().is_some();
                let go = ui.add_enabled(can_go, egui::Button::new(labels::GO));
                if (go.clicked() || enter_pressed)
                    && can_go
                    && let Some(request) = self.state.begin_load()
                {
                    self.spawn_load(request);
                }

                // A parse failure is shown right where it was typed.
                if let Some(error) = self.state.seed_error() {
                    ui.label(RichText::new(error.to_string()).color(ui.visuals().error_fg_color));
                }

                ui.separator();
                ui.label(self.version_text());
                ui.separator();
                ui.label(labels::RESOLUTION);
                self.resolution_combo(ui);
            });
        });
    }

    /// `map-core`'s pinned generator version, upgraded to the loaded
    /// source's full identity once a world is ready.
    fn version_text(&self) -> String {
        match self.state.source() {
            Some(source) => format!(
                "{} {} ({})",
                labels::VERSION,
                source.identity().generator_version,
                source.identity().world_assets,
            ),
            None => format!("{} {}", labels::VERSION, map_core::GENERATOR_VERSION),
        }
    }

    /// The 1×1/2×2/4×4/8×8 resolution selector.
    fn resolution_combo(&mut self, ui: &mut egui::Ui) {
        let current = self.state.resolution_step();
        let mut selected = current;
        egui::ComboBox::from_id_salt("resolution")
            .selected_text(format!("{current}×{current}"))
            .show_ui(ui, |ui| {
                for step in RESOLUTION_STEPS {
                    ui.selectable_value(&mut selected, step, format!("{step}×{step}"));
                }
            });
        if selected != current {
            self.state.set_resolution_step(selected);
        }
    }

    /// The last load's failure, with a retry that re-issues the same load.
    fn error_banner(&mut self, ctx: &Context) {
        let Some(message) = self.state.source_error().map(String::from) else {
            return;
        };
        egui::TopBottomPanel::top(Id::new("error-banner"))
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(70, 20, 20))
                    .inner_margin(4.0),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(format!("{}: {message}", labels::ERROR_BANNER));
                    if ui.button(labels::RETRY).clicked()
                        && let Some(request) = self.state.begin_load()
                    {
                        self.spawn_load(request);
                    }
                });
            });
    }

    /// The plan's left panel: layer radios, overlay checkboxes, search
    /// placeholder. Widget order is stable across frames so egui keeps
    /// its state; every control writes into [`AppState`].
    fn left_panel(&mut self, ctx: &Context) {
        // Consumed here so the F shortcut highlights the section once.
        let flash_search = self.state.take_search_focus();

        egui::SidePanel::left(Id::new("left-panel"))
            .default_width(190.0)
            .resizable(false)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.label(RichText::new(labels::GROUP_LAYERS).strong());
                let mut layer = self.state.layer();
                for candidate in AppLayer::ALL {
                    if ui
                        .radio_value(&mut layer, candidate, candidate.label())
                        .changed()
                    {
                        self.state.set_layer(layer);
                    }
                }

                ui.add_space(8.0);
                ui.separator();
                ui.label(RichText::new(labels::GROUP_OVERLAYS).strong());
                let overlays = self.state.overlays_mut();
                ui.checkbox(&mut overlays.structures, labels::OVERLAY_STRUCTURES);
                ui.checkbox(&mut overlays.prefabs, labels::OVERLAY_PREFABS);
                ui.checkbox(&mut overlays.cave_entrances, labels::OVERLAY_CAVE_ENTRANCES);
                ui.checkbox(&mut overlays.chunk_grid, labels::OVERLAY_CHUNK_GRID);

                ui.add_space(8.0);
                ui.separator();
                let search_heading = if flash_search {
                    RichText::new(labels::GROUP_SEARCH)
                        .strong()
                        .color(Color32::from_rgb(120, 200, 255))
                } else {
                    RichText::new(labels::GROUP_SEARCH).strong()
                };
                ui.label(search_heading);
                ui.label(labels::SEARCH_PLACEHOLDER);

                // Palette diagnostics (T13): every biome the loaded world
                // defines, name and map color, so a palette bug is visible
                // without running a render.
                if let Some(source) = self.state.source() {
                    ui.add_space(8.0);
                    ui.separator();
                    egui::CollapsingHeader::new(labels::PALETTE)
                        .default_open(false)
                        .show(ui, |ui| {
                            // HashMap iteration order would jitter the
                            // list between runs; sort by name.
                            let mut entries: Vec<_> = source.biome_catalog().iter().collect();
                            entries.sort_by(|a, b| a.name.cmp(&b.name));
                            for info in entries {
                                let [r, g, b] = info.map_color_rgb();
                                ui.horizontal(|ui| {
                                    let (swatch, _) = ui.allocate_exact_size(
                                        Vec2::new(12.0, 12.0),
                                        egui::Sense::hover(),
                                    );
                                    ui.painter().rect_filled(
                                        swatch,
                                        2.0,
                                        Color32::from_rgb(r, g, b),
                                    );
                                    ui.label(&info.name);
                                });
                            }
                        });
                }
            });
    }

    /// The go-to dialog placeholder (`G`).
    fn go_to_dialog(&mut self, ctx: &Context) {
        egui::Window::new(labels::GO_TO_TITLE)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.label(labels::GO_TO_PLACEHOLDER);
                if ui.button(labels::CLOSE).clicked() {
                    self.state.toggle_go_to();
                }
            });
    }

    /// The plan's bottom bar; pure formatting lives in [`state::format_status`].
    fn status_bar(&mut self, ctx: &Context) {
        let mut line = state::format_status(
            self.state.cursor(),
            None,
            0,
            self.state.scale(),
            self.state.resolution_step(),
            self.state.source_status_text(),
        );
        if !self.state.status().is_empty() {
            line.push_str(" | ");
            line.push_str(self.state.status());
        }
        egui::TopBottomPanel::bottom(Id::new("status-bar")).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(line).monospace().weak());
            });
        });
    }
}

impl eframe::App for SeedExplorerApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.update_inner(ctx);
    }
}

impl Drop for SeedExplorerApp {
    fn drop(&mut self) {
        // Move the heavy source off the UI thread on the way out too:
        // dropping it joins the generator worker pool, which can block
        // while a worker finishes a generation.
        self.state.invalidate_source();
        let retired = self.state.take_retired_sources();
        if !retired.is_empty() {
            for source in &retired {
                source.cancel_pending();
            }
            // Detached on purpose: at app exit, joining would only delay
            // the close; the OS reclaims the threads with the process.
            std::thread::spawn(move || drop(retired));
        }
    }
}

/// The central map area. With a source loaded it paints the biome
/// texture (T13); otherwise the T12 placeholder (dark canvas, block
/// grid, view readout). T16 replaces the single texture with pan/zoom
/// tiles, keeping this struct as the widget.
pub struct MapView {
    /// Screen size of the last painted frame, for the request math.
    screen: Vec2,
    /// The uploaded biome texture and the world it covers.
    texture: Option<MapTexture>,
}

impl Default for MapView {
    fn default() -> Self {
        MapView {
            screen: Vec2::ZERO,
            texture: None,
        }
    }
}

impl MapView {
    /// The last painted screen size, as plain floats for
    /// [`state::view_texture_rect`].
    fn screen(&self) -> (f32, f32) {
        (self.screen.x, self.screen.y)
    }

    /// Drops the texture (an invalidated source's map must not linger).
    fn clear_texture(&mut self) {
        self.texture = None;
    }

    /// Draws one frame of the map.
    fn ui(&mut self, ui: &mut egui::Ui, app_state: &AppState) {
        let rect = ui.max_rect();
        self.screen = rect.size();

        let painter = ui.painter().clone();
        // Dark map background (also the letterbox around the texture).
        painter.rect_filled(rect, 0.0, Color32::from_rgb(16, 16, 20));

        if let Some(texture) = &self.texture {
            // The real map: the texture covers its world rect, mapped
            // through the view (center, scale) into this area. UV is the
            // full texture; the orientation contract (north/row 0 at
            // image top) makes the mapping a pure translation+scale.
            let screen_rect =
                texture_screen_rect(texture, app_state.center(), app_state.scale(), rect);
            painter.image(
                texture.id(),
                screen_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        } else {
            self.placeholder(ui, &painter, rect, app_state);
        }

        // A spinner while a grid for the current view is generating.
        if app_state.is_texture_pending() {
            let spinner_rect = egui::Rect::from_center_size(
                rect.center_top() + Vec2::new(0.0, 40.0),
                Vec2::splat(28.0),
            );
            ui.put(spinner_rect, egui::Spinner::new());
        }
    }

    /// The no-texture state: grid canvas, status text, view readout.
    fn placeholder(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        rect: egui::Rect,
        app_state: &AppState,
    ) {
        // A faint block grid, 64 points per cell, so the area reads as a
        // map canvas and not a label.
        let cell = 64.0;
        let stroke = egui::Stroke::new(1.0_f32, Color32::from_rgb(36, 36, 44));
        let first_vertical = (rect.left() / cell).ceil() * cell;
        let mut x = first_vertical;
        while x < rect.right() {
            painter.vline(x, rect.y_range(), stroke);
            x += cell;
        }
        let first_horizontal = (rect.top() / cell).ceil() * cell;
        let mut y = first_horizontal;
        while y < rect.bottom() {
            painter.hline(rect.x_range(), y, stroke);
            y += cell;
        }

        // The state readout in the middle of the map.
        let message = match app_state.source_error() {
            Some(error) => format!("{} {error}", labels::MAP_PLACEHOLDER_ERROR_PREFIX),
            None if app_state.is_texture_pending() => labels::MAP_PLACEHOLDER_RENDERING.to_string(),
            None if app_state.is_loading() => labels::MAP_PLACEHOLDER_LOADING.to_string(),
            None if app_state.source().is_some() => labels::STATUS_SOURCE_READY.to_string(),
            None => labels::MAP_PLACEHOLDER_NO_SOURCE.to_string(),
        };
        let message_color = if app_state.source_error().is_some() {
            Color32::from_rgb(230, 120, 120)
        } else {
            Color32::from_rgb(200, 200, 210)
        };
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            message,
            TextStyle::Body.resolve(ui.style()),
            message_color,
        );

        // The view readout: current center and zoom, from T10's state.
        let (center_x, center_z) = app_state.center();
        let view_text = format!(
            "{}: ({center_x:.0}, {center_z:.0}) @ {:.1} pt/blk",
            labels::MAP_PLACEHOLDER_VIEW,
            app_state.scale(),
        );
        painter.text(
            rect.center_bottom() - Vec2::new(0.0, 16.0),
            Align2::CENTER_BOTTOM,
            view_text,
            TextStyle::Small.resolve(ui.style()),
            Color32::from_rgb(120, 120, 130),
        );

        // A spinner while the world itself is loading.
        if app_state.is_loading() {
            let spinner_rect = egui::Rect::from_center_size(
                rect.center_top() + Vec2::new(0.0, 40.0),
                Vec2::splat(28.0),
            );
            ui.put(spinner_rect, egui::Spinner::new());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::PathBuf;

    use super::*;
    use map_core::render::render_biome_rgba;
    use map_core::types::Rect;

    /// The worldgen checkout this repository's sibling ships for tests.
    fn world_dir() -> Option<PathBuf> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../hytale-server-rust/testdata/Default");
        // The committed world's prefab references need the user-supplied
        // asset pack; without it the generator cannot load (CI has none).
        let assets = dir.join("../../Assets/extracted");
        (dir.is_dir() && assets.is_dir()).then_some(dir)
    }

    /// The smoke test the task asks for: the shell constructs its
    /// headless parts without a window or a `CreationContext`.
    #[test]
    fn the_app_constructs_headless_with_the_plans_defaults() {
        let app = SeedExplorerApp::with_state(AppState::new("/nonexistent/world"));

        assert!(!app.state.is_loading(), "starts idle");
        assert!(app.load_rx.is_none(), "no load was spawned");
        assert_eq!(app.state.resolution_step(), 2, "the 2x2 default");
        assert_eq!(app.state.layer(), AppLayer::Biomes);
        assert_eq!(app.map.screen, Vec2::ZERO);
        // The version readout falls back to map-core's pinned version
        // before any source is ready.
        assert_eq!(
            app.version_text(),
            format!("{} {}", labels::VERSION, map_core::GENERATOR_VERSION),
        );
        // Status formatting is available before anything is loaded.
        assert!(
            state::format_status(
                app.state.cursor(),
                None,
                0,
                app.state.scale(),
                app.state.resolution_step(),
                app.state.source_status_text(),
            )
            .contains("2×2")
        );
    }

    #[test]
    fn go_and_retry_issue_loads_and_stale_edits_reset() {
        let mut app = SeedExplorerApp::with_state(AppState::new("/nonexistent/world"));
        app.state.set_seed_text(String::from("42"));
        let request = app.state.begin_load().expect("the load issues");
        let config = request.config();
        let generation = request.generation;
        app.spawn_load(request);

        assert!(app.state.is_loading());
        assert!(app.load_rx.is_some(), "the channel exists");

        // The worker's result (here: a synthetic failure, since the world
        // path is nonsense) is applied through the same channel the UI
        // polls, and lands in the error state with a retry available.
        let result = map_core::source::worldgen::WorldgenSource::new(&config);
        assert!(app.state.apply_load_result(generation, result));
        assert!(app.state.source_error().is_some(), "the bad path failed");
    }

    /// The full T13 acceptance flow, headless against the real
    /// generator: load a source, request the view's biome grid, render
    /// and convert it — the exact sequence `update` drives (only the GPU
    /// blit itself needs a window). A biome-colored map must come out:
    /// many colors, none of them the placeholder background.
    #[test]
    fn the_full_biome_texture_pipeline_produces_a_colored_map() {
        let Some(world) = world_dir() else {
            eprintln!("skipping: the worldgen checkout is not present");
            return;
        };
        let mut app = SeedExplorerApp::with_state(AppState::new(&world));

        // Go with a valid seed: the load succeeds.
        app.state.set_seed_text(String::from("12345"));
        let request = app.state.begin_load().expect("the load issues");
        let source = map_core::source::worldgen::WorldgenSource::new(&request.config())
            .expect("the golden world loads");
        assert!(app.state.apply_load_result(request.generation, Ok(source)));
        assert!(app.state.source().is_some(), "ready");

        // The plan's static view: a 1280x800 map area at scale 1, the
        // default 2x2 resolution — exactly 640x400 capped samples over
        // 1280x800 blocks.
        let rect =
            state::view_texture_rect(app.state.center(), app.state.scale(), 2, (1280.0, 800.0))
                .expect("the view resolves");
        assert_eq!(rect, Rect::new(-640, -400, 639, 399));
        assert!(
            app.state.request_view_texture(rect, 2),
            "the first view request issues"
        );

        // The worker: the real region query, like maybe_request_texture's
        // spawned thread performs it.
        let grid = app
            .state
            .source()
            .expect("ready")
            .biome_region(rect, 2)
            .expect("the view region serves");
        assert_eq!((grid.width, grid.height), (640, 400));
        assert!(
            app.state.apply_grid_result(rect, 2, Ok(grid)),
            "the current view's result applies"
        );

        // The upload path: render + convert (upload_due_texture's body).
        let (grid, uploaded_rect, uploaded_step) = app
            .state
            .grid_to_upload()
            .expect("the arrived grid waits for upload");
        let catalog = app.state.source().expect("ready").biome_catalog();
        let image = render_biome_rgba(grid, catalog);
        app.state.mark_uploaded();
        assert!(matches!(
            app.state.map_texture(),
            state::MapTexture::Current { .. }
        ));
        assert_eq!((uploaded_rect, uploaded_step), (rect, 2));

        // The acceptance: a biome-colored map, not a placeholder. Many
        // distinct colors, none of them the map background, and no
        // unknown-biome magenta (the catalog covers the world).
        let color_image = crate::texture::to_color_image(&image);
        assert_eq!(color_image.size, [640, 400]);
        let colors: HashSet<[u8; 3]> = color_image
            .pixels
            .iter()
            .map(|c| [c.r(), c.g(), c.b()])
            .collect();
        assert!(
            colors.len() > 1,
            "the map shows multiple biome colors, got {}",
            colors.len()
        );
        assert!(
            !colors.contains(&[16, 16, 20]),
            "no placeholder background leaks into the map"
        );
        assert!(
            !colors.contains(&[255, 0, 255]),
            "no unknown-biome magenta in a catalog-covered world"
        );

        // Invalidate on edit: the map dies with its seed.
        app.state.set_seed_text(String::from("77"));
        assert_eq!(app.state.map_texture(), &state::MapTexture::Empty);
    }
}
