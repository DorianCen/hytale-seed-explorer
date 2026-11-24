//! The egui skin over the headless [`crate::state`] (T12), with the
//! interactive map of T16.
//!
//! Everything testable lives in `state.rs` and `view_state.rs`; the map
//! widget (input, tiles, painting) lives in `map_view.rs`. This module
//! is layout and wiring only: the plan's three regions (top bar, left
//! panel, central map), the status bar, keyboard shortcuts and the
//! background source load. No worldgen work ever happens on the UI
//! thread — loads run on a spawned thread and results arrive through a
//! channel polled with `try_recv` (plus a `request_repaint_after` nudge
//! so the UI stays idle otherwise).
//!
//! Heavy objects never drop on the UI thread, ever: sources join
//! generator pools when dropped, and retired tile pipelines (T16) join
//! their scheduler workers. Both retire through lists the app drains
//! each frame and drops on detached background threads.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Color32, Context, Id, Key, KeyboardShortcut, Modifiers, RichText, TextStyle, Vec2};

use map_core::source::FeatureSource;
use map_core::source::WorldSource;
use map_core::source::worldgen::{WorldgenSource, default_world_path};

use crate::inspector::{self, HoverKnowledge, HoverReply, Readout};
use crate::labels;
use crate::map_view::MapView;
use crate::overlays::features::{FeatureFetch, FeatureReplyOutcome};
use crate::search_panel::SearchPanel;
use crate::state::{
    self, AppState, LoadRequest, LoadResult, RESOLUTION_STEPS, RenderMode, layer_label,
    offers_layer, render_mode_label,
};

/// How often to wake the UI while a background load is in flight.
const LOAD_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How often to wake the UI while a hover sample is in flight, so its
/// reply lands on the very next frame instead of at the next input.
const HOVER_POLL_INTERVAL: Duration = Duration::from_millis(16);

/// How often to wake the UI while a feature query is in flight (T23). The
/// query itself runs for seconds-to-minutes on its worker; this only makes
/// the reply land promptly instead of at the next input event.
const FEATURE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How often to wake the UI while a search is streaming (T26). The state
/// drains at most 64 events per frame; this keeps the stream flowing without
/// pinning the CPU.
const SEARCH_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Which overlay slot a spawned query feeds: the immediate cheap tier, the
/// invisible biome-prefab prewarm, or the debounced full tier.
#[derive(Debug, Clone, Copy)]
enum FeatureQuery {
    Quick,
    Prewarm,
    Full,
}

/// The egui app: headless state plus the UI wiring.
pub struct SeedExplorerApp {
    state: AppState,
    /// Result channel of the single in-flight load, when there is one.
    load_rx: Option<Receiver<LoadResult>>,
    /// Reply channel of the single in-flight hover sample, when there is
    /// one (T18). Replacing it detaches the previous worker — its send
    /// fails and its reply dies with the old channel.
    hover_rx: Option<Receiver<HoverReply>>,
    /// The central map widget (T16): input, tile pipeline, painting.
    map: MapView,
    /// The left-panel search form (T26); its headless state lives in
    /// [`AppState`].
    search_panel: SearchPanel,
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
            hover_rx: None,
            map: MapView::new(),
            search_panel: SearchPanel::default(),
        }
    }

    /// One frame.
    fn update_inner(&mut self, ctx: &Context) {
        // Order matters: the status bar and top bar claim their strips
        // first, the left panel its column, and the central panel fills
        // the remainder. Hover replies drain before anything reads the
        // inspector's knowledge.
        self.poll_load_channel(ctx);
        self.poll_hover_replies();
        self.state.search_mut().poll();
        self.keyboard_shortcuts(ctx);

        self.status_bar(ctx);
        self.top_bar(ctx);
        self.error_banner(ctx);
        self.left_panel(ctx);

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(Color32::from_rgb(16, 16, 20)))
            .show(ctx, |ui| {
                self.map.ui(ui, &mut self.state);
            });

        // The map widget just refreshed the cursor: this is where the
        // frame's hover decision runs (T18) and where the feature overlay
        // notes the settled viewport and polls its query (T23).
        self.drive_hover_sampling(ctx);
        self.drive_feature_overlay(ctx);
        if self.state.search().is_running() {
            // Keep the search stream flowing while the user does nothing.
            ctx.request_repaint_after(SEARCH_POLL_INTERVAL);
        }
        if self.state.search().flash_position(Instant::now()).is_some() {
            // The search flash is a one-second animation; repaint until it
            // expires.
            ctx.request_repaint_after(Duration::from_millis(16));
        }

        if self.state.go_to().is_some() {
            self.go_to_dialog(ctx);
        }

        // Heavy objects never drop here: displaced sources and retired
        // tile pipelines go to detached threads. Cancel the sources
        // first so any in-flight region bails at its next row boundary
        // instead of being waited out by the pool's join.
        let retired = self.state.take_retired_sources();
        if !retired.is_empty() {
            for source in &retired {
                source.cancel_pending();
            }
            std::thread::spawn(move || drop(retired));
        }
        let pipelines = self.map.take_retired_pipelines();
        if !pipelines.is_empty() {
            std::thread::spawn(move || drop(pipelines));
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

    // -- Hover sampling (T18) -------------------------------------------------

    /// Drains hover replies without blocking. The worker owns the send
    /// half and drops it with its reply, so `Disconnected` means the
    /// request is finished — whether or not a reply came with it — and
    /// the inspector frees its in-flight slot either way.
    fn poll_hover_replies(&mut self) {
        let Some(receiver) = self.hover_rx.take() else {
            return;
        };
        let mut finished = false;
        loop {
            match receiver.try_recv() {
                Ok(reply) => self.state.inspector_mut().apply_reply(
                    reply.generation,
                    reply.x,
                    reply.z,
                    reply.result,
                ),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    finished = true;
                    break;
                }
            }
        }
        if finished {
            // Either the reply arrived (in-flight already cleared) or the
            // worker vanished; both mean: no request is pending anymore.
            self.state.inspector_mut().cancel_in_flight();
        } else {
            self.hover_rx = Some(receiver);
        }
    }

    /// One frame of the hover pipeline: after the map widget refreshed
    /// the cursor, decide whether the debounced background sample is due
    /// and spawn it. The UI thread never queries the source itself —
    /// `surface_at` can generate a chunk.
    fn drive_hover_sampling(&mut self, ctx: &Context) {
        // No source (yet): cancel any notion of an in-flight sample; the
        // inspector's knowledge was already reset by the state machine.
        let Some(source) = self.state.source_arc() else {
            self.hover_rx = None;
            self.state.inspector_mut().cancel_in_flight();
            return;
        };
        let now = Instant::now();
        let cursor = self.state.cursor();
        let Some((generation, x, z)) = self.state.inspector_mut().poll_due(now, cursor) else {
            // Nothing due; while a request is in flight keep frames
            // coming so the reply lands immediately.
            if self.state.inspector().in_flight().is_some() {
                ctx.request_repaint_after(HOVER_POLL_INTERVAL);
            }
            return;
        };
        // One worker per request, at most one alive: replacing the
        // receiver detaches any previous worker (its send fails, its
        // thread ends) — the generation guard inside the inspector is
        // the second line of defence.
        let (sender, receiver) = std::sync::mpsc::channel();
        self.hover_rx = Some(receiver);
        std::thread::spawn(move || {
            // `surface_at` already carries the biome in its sample, so a
            // separate `biome_at` would be a second query for data the
            // reply holds.
            let result = source.surface_at(x, z);
            // A send failure means a newer request replaced this one: the
            // reply dies with the old channel, exactly as intended.
            let _ = sender.send(HoverReply {
                generation,
                x,
                z,
                result,
            });
        });
        ctx.request_repaint_after(HOVER_POLL_INTERVAL);
    }

    // -- Feature overlay (T23) ------------------------------------------------

    /// Polls a finished feature query, then plans and spawns the tier queries
    /// that are due. `features_in` is synchronous and can block a worker for
    /// minutes on a large rect, so the UI thread only ever sees the channel.
    ///
    /// Three independent slots: tier 1 (cheap kinds, immediate, once per
    /// source), the invisible biome-prefab prewarm (once per source), and
    /// tier 2 (the debounced full fetch). They never wait on each other; the
    /// generator pool serializes the actual work.
    fn drive_feature_overlay(&mut self, ctx: &Context) {
        self.poll_feature_reply();
        let now = Instant::now();
        if let Some(source) = self.state.source_arc() {
            // Copy the view and settings out before the mutable borrows: the
            // planner is pure and the state machine does the rest.
            let view = *self.state.view();
            let settings = *self.state.overlays();
            self.state.features_mut().observe(now, &view);
            if let Some(request) = self.state.features_mut().plan_quick_fetch(&view, &settings) {
                self.spawn_feature_query(Arc::clone(&source), request, FeatureQuery::Quick);
            }
            if let Some(request) = self
                .state
                .features_mut()
                .plan_prewarm_fetch(&view, &settings)
            {
                self.spawn_feature_query(Arc::clone(&source), request, FeatureQuery::Prewarm);
            }
            if let Some(request) = self.state.features_mut().plan_fetch(now, &view, &settings) {
                self.spawn_feature_query(source, request, FeatureQuery::Full);
            }
        } else {
            // No world: a query cannot be wanted, and its reply (were one
            // still in flight) must never reach the next source.
            self.state.features_mut().cancel_fetch();
        }
        if self.state.features().has_fetch_in_flight() {
            ctx.request_repaint_after(FEATURE_POLL_INTERVAL);
        }
    }

    /// Spawns one feature query on its own thread and records its reply
    /// channel in the matching tier slot.
    ///
    /// The thread owns one `Arc<WorldgenSource>`; if the UI drops the
    /// receiver (source change), its send fails and the Arc drops HERE, off
    /// the UI thread — the retirement pattern. The prewarm's result only warms
    /// the generator's prefab buffers and lands in the query cache.
    fn spawn_feature_query(
        &mut self,
        source: Arc<WorldgenSource>,
        request: FeatureFetch,
        query: FeatureQuery,
    ) {
        let (sender, receiver) = std::sync::mpsc::channel();
        let filter = request.filter.clone();
        let rect = request.rect;
        match query {
            FeatureQuery::Quick => self
                .state
                .features_mut()
                .begin_quick_fetch(request, receiver),
            FeatureQuery::Prewarm => {
                self.state
                    .features_mut()
                    .begin_prewarm_fetch(request, receiver);
            }
            FeatureQuery::Full => self.state.features_mut().begin_fetch(request, receiver),
        }
        std::thread::spawn(move || {
            let result = source.features_in(rect, &filter);
            let _ = sender.send(result);
        });
    }

    /// Applies one reply from each in-flight feature query, keeping the last
    /// catalog on failure. A stale tier-2 reply is dropped silently (the next
    /// frame re-plans for the current view).
    fn poll_feature_reply(&mut self) {
        let now = Instant::now();
        let view = *self.state.view();
        let outcomes = [
            self.state.features_mut().poll(now, &view),
            self.state.features_mut().poll_quick(now, &view),
            self.state.features_mut().poll_prewarm(now, &view),
        ];
        for outcome in outcomes {
            match outcome {
                FeatureReplyOutcome::Failed(message) => {
                    self.state
                        .set_status(format!("{}: {message}", labels::STATUS_FEATURE_ERROR));
                }
                FeatureReplyOutcome::WorkerDied => {
                    self.state.set_status(labels::STATUS_FEATURE_WORKER_DIED);
                }
                FeatureReplyOutcome::Idle
                | FeatureReplyOutcome::Pending
                | FeatureReplyOutcome::Stale
                | FeatureReplyOutcome::Applied(_) => {}
            }
        }
    }

    /// The inspector's knowledge rendered for the status bar: sample ids
    /// resolved through the loaded source's catalogs, everything else
    /// passed through.
    fn current_readout(&self) -> Readout {
        match self.state.inspector().knowledge() {
            HoverKnowledge::Nothing => Readout::Nothing,
            HoverKnowledge::Pending => Readout::Pending,
            HoverKnowledge::OutOfWorld => Readout::OutOfWorld,
            HoverKnowledge::Sample(sample) => match self.state.source() {
                Some(source) => Readout::Sample(inspector::resolve_sample(
                    sample,
                    source.biome_catalog(),
                    source.block_catalog(),
                    source.fluid_catalog(),
                )),
                // A sample without its source cannot happen (the state
                // resets the inspector on invalidation); degrade to
                // unknown instead of ever panicking on the UI thread.
                None => Readout::Nothing,
            },
        }
    }

    // -- Keyboard -------------------------------------------------------------

    /// Global shortcuts, none of which may steal keys from a focused text
    /// field: `G` go-to, `F` search focus, `Ctrl+S` export placeholder,
    /// and T16's zoom keys — `+`/`=` zoom in, `-` zoom out, one level per
    /// press anchored on the viewport center (the wheel owns the cursor
    /// anchor).
    ///
    /// `Esc` closes the go-to dialog even while a field has focus: egui's
    /// text edits never consume Escape. `consume_shortcut` both checks
    /// and consumes the press, so the same key never leaks into the
    /// frame's other handling; `COMMAND` is the cross-platform Ctrl/Cmd
    /// spelling.
    fn keyboard_shortcuts(&mut self, ctx: &Context) {
        if self.state.go_to().is_some() && ctx.input_mut(|input| input.key_pressed(Key::Escape)) {
            self.state.close_go_to();
        } else if self.state.go_to().is_none()
            && ctx.input_mut(|input| input.key_pressed(Key::Escape))
        {
            // `Esc` cancels a running search or clears a finished one; it
            // works even while a text field has focus (egui never consumes
            // Escape in a singleline edit).
            self.state.cancel_or_clear_search();
        }
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
        if ctx.input_mut(|input| input.key_pressed(Key::Plus) || input.key_pressed(Key::Equals)) {
            self.state.view_mut().zoom_at_center(-1.0);
        }
        if ctx.input_mut(|input| input.key_pressed(Key::Minus)) {
            self.state.view_mut().zoom_at_center(1.0);
        }
    }

    // -- Panels ----------------------------------------------------------------

    /// The plan's top bar: seed field, Go, version, resolution selector,
    /// and T21's worker cap.
    fn top_bar(&mut self, ctx: &Context) {
        // Buffered like the panel toggles: the closure reads a local and
        // the write lands where the state is mutable again.
        let mut workers = self.state.workers().unwrap_or(2);
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
                ui.separator();
                // The chunkbase-style mode selector: Surface is today's
                // stack; Biomes (only) is the hard speed filter.
                ui.label(labels::RENDER_MODE);
                self.render_mode_combo(ui);
                // The cap feeds the GENERATOR pool of the NEXT load (T21's
                // cost control; T33 tunes the default). It cannot resize a
                // pool that is already generating, so the change is
                // announced rather than silently applied.
                ui.separator();
                ui.label(labels::WORKERS);
                if ui
                    .add(egui::DragValue::new(&mut workers).range(1..=16))
                    .changed()
                {
                    self.state.set_workers(Some(workers));
                    self.state.set_status(labels::STATUS_WORKERS_NEXT_LOAD);
                }
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

    /// The Surface / Biomes-only render-mode selector.
    fn render_mode_combo(&mut self, ui: &mut egui::Ui) {
        let current = self.state.render_mode();
        let mut selected = current;
        egui::ComboBox::from_id_salt("render-mode")
            .selected_text(render_mode_label(current))
            .show_ui(ui, |ui| {
                for mode in RenderMode::ALL {
                    ui.selectable_value(&mut selected, mode, render_mode_label(mode));
                }
            });
        if selected != current {
            self.state.set_render_mode(selected);
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

    /// The plan's left panel: the layer stack's checkboxes and opacity
    /// sliders (T19), overlay checkboxes, the search panel (T26), the
    /// inspector (T18), the zone legend and the palette diagnostics.
    /// Widget order is stable across frames so egui keeps its state;
    /// every control writes into [`AppState`] — through buffered locals
    /// where a closure would otherwise hold the borrow.
    fn left_panel(&mut self, ctx: &Context) {
        // Consumed here so the F shortcut highlights the section once.
        let flash_search = self.state.take_search_focus();

        // The inspector's rendering inputs, prepared up front so the
        // panel's closures read locals instead of borrowing the app.
        let readout = self.current_readout();
        let hovered = self.state.cursor();
        let inspector_error = self.state.inspector().error().map(String::from);
        let scale = self.state.view().scale();
        let step = self.state.resolution_step();
        let effective = self.state.view().tile_step(step);
        // T21's hint: the full-block layer is only worth its cost when a
        // sample is one block; at coarser steps it says so instead of
        // silently sampling blocks together.
        let surface_enabled = self
            .state
            .schedulable_layers()
            .iter()
            .any(|(layer, _)| *layer == map_core::render::Layer::Surface);
        // The mode is a hard filter: the checkboxes below keep their
        // settings but cannot change the map until Surface returns.
        let biomes_only = self.state.render_mode() == RenderMode::BiomesOnly;
        let mut copied = false;

        // The layer stack's edits, buffered while the panel closure
        // borrows the stack read-only and applied after it returns.
        let mut toggled: Option<(map_core::render::Layer, bool)> = None;
        let mut opacity_edited: Option<(map_core::render::Layer, f32)> = None;

        egui::SidePanel::left(Id::new("left-panel"))
            .default_width(190.0)
            .resizable(false)
            .show(ctx, |ui| {
                // T26 adds a search form and a result table to an already
                // tall column; the whole panel scrolls so the table and the
                // inspector below it stay reachable in a short window.
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add_space(6.0);
                        ui.label(RichText::new(labels::GROUP_LAYERS).strong());
                        if biomes_only {
                            ui.label(RichText::new(labels::MODE_HINT_BIOMES_ONLY).weak());
                        }
                        // Every offered layer in stack order: the checkbox, and
                        // — while enabled — its opacity slider. Order is paint
                        // order, bottom → top; the unoffered future layers get
                        // their grayed "arrives with" notes instead.
                        for settings in self.state.layers().iter().filter(|s| offers_layer(s.id)) {
                            let mut enabled = settings.enabled;
                            if ui
                                .checkbox(&mut enabled, layer_label(settings.id))
                                .changed()
                            {
                                toggled = Some((settings.id, enabled));
                            }
                            if enabled {
                                let mut opacity = settings.opacity;
                                ui.horizontal(|ui| {
                                    ui.label(labels::OPACITY);
                                    if ui.add(egui::Slider::new(&mut opacity, 0.0..=1.0)).changed()
                                    {
                                        opacity_edited = Some((settings.id, opacity));
                                    }
                                });
                            }
                        }
                        if surface_enabled && effective > 1 {
                            // The plan's "auto-select step 1 only when visible"
                            // note: full-block detail at 4×4 is a deliberate
                            // zoom-out, not a rendering bug.
                            let hint = labels::LAYER_HINT_SURFACE
                                .replace("{n}×{n}", &format!("{effective}×{effective}"));
                            ui.label(RichText::new(hint).weak());
                        }
                        ui.label(RichText::new(labels::MAP_PLACEHOLDER_CAVES).weak());
                        // The plan's climate debug layers: only if the
                        // generator exposes a per-column query — it does not,
                        // and the refusal is explicit rather than approximated.
                        ui.label(RichText::new(labels::CLIMATE_NOTE).weak());

                        ui.add_space(8.0);
                        ui.separator();
                        ui.label(RichText::new(labels::GROUP_OVERLAYS).strong());
                        let overlays = self.state.overlays_mut();
                        ui.checkbox(&mut overlays.structures, labels::OVERLAY_STRUCTURES);
                        ui.checkbox(&mut overlays.prefabs, labels::OVERLAY_PREFABS);
                        ui.checkbox(&mut overlays.spawn, labels::OVERLAY_SPAWN);
                        ui.checkbox(&mut overlays.poi, labels::OVERLAY_POI);
                        // The cave toggle drives the full tier's
                        // cave-entrance kind (T27); large views omit it per
                        // the adapter's cap.
                        ui.checkbox(&mut overlays.cave_entrances, labels::OVERLAY_CAVE_ENTRANCES);
                        ui.checkbox(&mut overlays.chunk_grid, labels::OVERLAY_CHUNK_GRID);

                        ui.add_space(8.0);
                        ui.separator();
                        // T26: the real search form, results table and actions.
                        // `flash_search` is the `F` shortcut's one-shot.
                        self.search_panel.ui(ui, &mut self.state, flash_search);

                        // The inspector (T18): the same data the status bar reads,
                        // with names *and* ids, the tint and the copy button.
                        ui.add_space(8.0);
                        ui.separator();
                        egui::CollapsingHeader::new(labels::INSPECTOR)
                            .default_open(true)
                            .show(ui, |ui| {
                                let coords = match hovered {
                                    Some((x, z)) => format!("X:{x} Z:{z}"),
                                    None => {
                                        format!(
                                            "X:{} Z:{}",
                                            labels::STATUS_MISSING,
                                            labels::STATUS_MISSING
                                        )
                                    }
                                };
                                ui.label(format!("{}: {coords}", labels::INSPECTOR_COORDINATES));
                                match &readout {
                                    Readout::Sample(sample) => {
                                        ui.label(format!(
                                            "{}: {} (#{})",
                                            labels::STATUS_BIOME,
                                            sample.biome,
                                            sample.biome_id.0,
                                        ));
                                        ui.label(format!(
                                            "{}: {}",
                                            labels::STATUS_HEIGHT,
                                            sample.height
                                        ));
                                        ui.label(format!(
                                            "{}: {} (#{})",
                                            labels::STATUS_BLOCK,
                                            sample.block,
                                            sample.block_id.0,
                                        ));
                                        match &sample.fluid {
                                            Some(fluid) => {
                                                ui.label(format!(
                                                    "{}: {fluid} (#{})",
                                                    labels::STATUS_FLUID,
                                                    sample.fluid_id.map_or_else(
                                                        || labels::STATUS_MISSING.to_string(),
                                                        |id| id.0.to_string()
                                                    ),
                                                ));
                                                ui.label(format!(
                                                    "{}: {}",
                                                    labels::INSPECTOR_DEPTH,
                                                    sample.fluid_depth
                                                ));
                                            }
                                            None => {
                                                ui.label(format!(
                                                    "{}: {}",
                                                    labels::STATUS_FLUID,
                                                    labels::STATUS_NONE
                                                ));
                                            }
                                        }
                                        ui.label(format!(
                                            "{}: #{:06x}",
                                            labels::INSPECTOR_TINT,
                                            sample.tint & 0xFF_FF_FF,
                                        ));
                                    }
                                    Readout::OutOfWorld => {
                                        ui.label(labels::STATUS_OUT_OF_WORLD);
                                    }
                                    Readout::Pending => {
                                        ui.label(labels::STATUS_PENDING);
                                    }
                                    Readout::Nothing => {
                                        ui.label(labels::STATUS_MISSING);
                                    }
                                }
                                if let Some(error) = &inspector_error {
                                    ui.label(
                                        RichText::new(error).color(ui.visuals().error_fg_color),
                                    );
                                }
                                ui.label(format!(
                                    "{}: {scale:.1} {}",
                                    labels::STATUS_ZOOM,
                                    labels::STATUS_PT_PER_BLOCK
                                ));
                                ui.label(state::format_resolution(step, effective));
                                if ui
                                    .add_enabled(
                                        hovered.is_some(),
                                        egui::Button::new(labels::COPY_COORDINATES),
                                    )
                                    .clicked()
                                {
                                    copied = true;
                                }
                            });

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
                                    let mut entries: Vec<_> =
                                        source.biome_catalog().iter().collect();
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

                            // The zone debug layer's legend (T19): the layer
                            // colors by a stable name hash, so the legend is how
                            // a color becomes a name again.
                            ui.add_space(4.0);
                            egui::CollapsingHeader::new(labels::ZONE_LEGEND)
                                .default_open(false)
                                .show(ui, |ui| {
                                    let mut zones: Vec<_> = source.zone_catalog().iter().collect();
                                    if zones.is_empty() {
                                        ui.label(labels::ZONE_NONE);
                                    }
                                    // Sorted like the palette: stable list, no
                                    // HashMap jitter.
                                    zones.sort_by(|a, b| a.name.cmp(&b.name));
                                    for info in zones {
                                        let [r, g, b] = map_core::render::zone_color(&info.name);
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
            });
        // The layer edits were buffered while the panel held the stack:
        // apply them here, where the state is mutable again. Each write
        // marks the stack dirty (T31's persistence hook) and the next
        // frame's request follows the new settings.
        if let Some((layer, enabled)) = toggled {
            self.state.layers_mut().set_enabled(layer, enabled);
        }
        if let Some((layer, opacity)) = opacity_edited {
            self.state.layers_mut().set_opacity(layer, opacity);
        }
        // The copy button only sets a flag inside the panel: the
        // clipboard write and the status feedback happen here, where
        // the state is mutable again.
        if copied && let Some((x, z)) = hovered {
            ctx.copy_text(format!("X:{x} Z:{z}"));
            self.state
                .set_status(format!("{} X:{x} Z:{z}", labels::STATUS_COPIED));
        }
    }

    /// The go-to dialog (`G`, T18): two monospace fields, an inline error
    /// under them, Enter or the Go button to submit, `Esc`/Close to
    /// dismiss. Works on a local copy of the dialog; the state gets the
    /// edits back and then validates through the state machine, so a
    /// failed submit keeps the dialog open with its error set.
    fn go_to_dialog(&mut self, ctx: &Context) {
        let mut dialog = std::mem::take(
            self.state
                .go_to_mut()
                .expect("the caller checked the dialog is open"),
        );
        let mut close = false;
        let mut submitted = false;
        egui::Window::new(labels::GO_TO_TITLE)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.label(labels::GO_TO_HINT);
                let mut enter_pressed = false;
                ui.horizontal(|ui| {
                    ui.label(labels::GO_TO_X);
                    let x = ui.add(
                        egui::TextEdit::singleline(&mut dialog.x_text)
                            .font(TextStyle::Monospace)
                            .desired_width(90.0),
                    );
                    ui.label(labels::GO_TO_Z);
                    let z = ui.add(
                        egui::TextEdit::singleline(&mut dialog.z_text)
                            .font(TextStyle::Monospace)
                            .desired_width(90.0),
                    );
                    // A singleline edit loses focus on Enter, so either
                    // field's loss-of-focus plus the Enter press submits.
                    enter_pressed = (x.lost_focus() || z.lost_focus())
                        && ui.input(|input| input.key_pressed(Key::Enter));
                });
                if let Some(error) = &dialog.error {
                    ui.label(RichText::new(error.as_str()).color(ui.visuals().error_fg_color));
                }
                if ui.button(labels::GO_TO_GO).clicked() || enter_pressed {
                    submitted = true;
                }
                if ui.button(labels::CLOSE).clicked() {
                    close = true;
                }
            });
        // Hand the edits back before deciding the dialog's fate: a
        // failed submit sets the inline error on exactly these fields,
        // a successful one closes and centers the view.
        *self
            .state
            .go_to_mut()
            .expect("the dialog stays open until a flag says otherwise") = dialog;
        if close {
            self.state.close_go_to();
        } else if submitted {
            let world = self.map.world_bounds();
            self.state.submit_go_to(world);
        }
    }

    /// The plan's bottom bar: the hover readout (T18's
    /// [`inspector::format_readout`]) followed by the tail of
    /// [`state::format_status`].
    fn status_bar(&mut self, ctx: &Context) {
        // The pending count is the last frame's: panels draw before the
        // map serves this frame's tiles, and egui keeps repainting while
        // tiles are pending anyway, so the count is at worst one frame
        // behind and never stuck.
        let pending = u32::try_from(self.map.pending_tiles()).unwrap_or(u32::MAX);
        let readout = self.current_readout();
        let mut line = inspector::format_readout(self.state.cursor(), &readout);
        line.push_str(" | ");
        line.push_str(&state::format_status(
            pending,
            self.state.view().scale(),
            self.state.resolution_step(),
            // The step the renderer actually samples at — the zoom may
            // force coarser than the selector's minimum.
            self.state.view().tile_step(self.state.resolution_step()),
            // The enabled layer set, e.g. "Biomes+Fluid", or the blank
            // map's word.
            &self.state.layer_summary(),
            self.state.source_status_text(),
        ));
        if !self.state.status().is_empty() {
            line.push_str(" | ");
            line.push_str(self.state.status());
        }
        // The overlay's own progress: a tier-2 fetch runs for minutes, so it
        // says so instead of leaving an apparently dead map. Appended last so
        // it never masks a toast.
        if let Some(hint) = self.state.features().status_hint() {
            line.push_str(" | ");
            line.push_str(hint);
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
        // Move everything heavy off the UI thread on the way out too:
        // sources join generator pools, tile pipelines join their
        // scheduler workers, and both can block while a worker finishes.
        self.state.invalidate_source();
        let retired = self.state.take_retired_sources();
        for source in &retired {
            source.cancel_pending();
        }
        let pipelines = self.map.take_all_pipelines();
        // Detached on purpose: at app exit, joining would only delay the
        // close; the OS reclaims the threads with the process.
        std::thread::spawn(move || {
            drop(retired);
            drop(pipelines);
        });
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::*;
    use map_core::source::worldgen::WorldgenSource;

    /// The smoke test the task asks for: the shell constructs its
    /// headless parts without a window or a `CreationContext`.
    #[test]
    fn the_app_constructs_headless_with_the_plans_defaults() {
        let app = SeedExplorerApp::with_state(AppState::new("/nonexistent/world"));

        assert!(!app.state.is_loading(), "starts idle");
        assert!(app.load_rx.is_none(), "no load was spawned");
        assert!(app.hover_rx.is_none(), "no hover request was spawned");
        assert_eq!(app.state.resolution_step(), 2, "the 2x2 default");
        // The plan's default layer set: Biome alone, at full opacity.
        assert_eq!(
            app.state.schedulable_layers(),
            vec![(map_core::render::Layer::Biome, 1.0)]
        );
        assert_eq!(app.map.pending_tiles(), 0, "no tiles pending");
        // The interactive view starts at the plan's initial view: the
        // origin at one point per block.
        assert_eq!(app.state.view().center().x, 0.0);
        assert_eq!(app.state.view().scale(), 1.0);
        // The version readout falls back to map-core's pinned version
        // before any source is ready.
        assert_eq!(
            app.version_text(),
            format!("{} {}", labels::VERSION, map_core::GENERATOR_VERSION),
        );
        // Status formatting is available before anything is loaded; the
        // tail carries the plan's default resolution and the default
        // layer set.
        assert!(
            state::format_status(
                0,
                app.state.view().scale(),
                app.state.resolution_step(),
                app.state.view().tile_step(app.state.resolution_step()),
                &app.state.layer_summary(),
                app.state.source_status_text(),
            )
            .contains("2×2")
        );
        // And so does the readout, all-unknown until the cursor exists.
        assert_eq!(
            inspector::format_readout(app.state.cursor(), &app.current_readout()),
            format!(
                "X:{Q} Z:{Q} | {BIO}: {Q} | {HEIGHT}: {Q} | {BLOCK}: {Q} | {FLUID}: {NONE}",
                Q = labels::STATUS_MISSING,
                NONE = labels::STATUS_NONE,
                BIO = labels::STATUS_BIOME,
                HEIGHT = labels::STATUS_HEIGHT,
                BLOCK = labels::STATUS_BLOCK,
                FLUID = labels::STATUS_FLUID,
            )
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
        let result = WorldgenSource::new(&config);
        assert!(app.state.apply_load_result(generation, result));
        assert!(app.state.source_error().is_some(), "the bad path failed");
    }

    // -- The hover pipeline over a real source (gated like every
    // generator-backed test) -----------------------------------------------

    /// The worldgen checkout this repository's sibling ships for tests.
    fn world_dir() -> Option<PathBuf> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../hytale-server-rust/testdata/Default");
        // The committed world's prefab references need the user-supplied
        // asset pack; without it the generator cannot load (CI has none).
        let assets = dir.join("../../Assets/extracted");
        (dir.is_dir() && assets.is_dir()).then_some(dir)
    }

    #[test]
    fn inspector_hover_pipeline_serves_a_real_sample_through_the_channel() {
        // End to end over the real generator: a parked cursor issues one
        // debounced background request, the reply lands through the
        // channel the UI polls, and the readout resolves through the
        // source's own catalogs. The values themselves are the fixture
        // comparison's business (see inspector's tests).
        let Some(world) = world_dir() else {
            eprintln!("skipping: the worldgen checkout is not present");
            return;
        };
        let mut app = SeedExplorerApp::with_state(AppState::new(&world));
        app.state.set_seed_text(String::from("0"));
        let request = app.state.begin_load().expect("the load issues");
        let result = WorldgenSource::new(&request.config());
        assert!(app.state.apply_load_result(request.generation, result));

        let ctx = egui::Context::default();
        app.state.set_cursor(Some((0, 0)));
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            app.drive_hover_sampling(&ctx);
            app.poll_hover_replies();
            if matches!(app.state.inspector().knowledge(), HoverKnowledge::Sample(_)) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the hover sample did not arrive in time"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        // The readout resolved names through the real catalogs; the
        // sample itself carries the golden chunk's column.
        let line = inspector::format_readout(app.state.cursor(), &app.current_readout());
        assert!(
            line.contains("X:0 Z:0"),
            "the readout names the column: {line}"
        );
        assert!(line.contains(labels::STATUS_BIOME), "{line}");
        let HoverKnowledge::Sample(sample) = app.state.inspector().knowledge() else {
            unreachable!("the loop only exits on a sample");
        };
        assert_eq!(sample.height, 107, "the fixture height at seed 0 (0, 0)");
    }
}
