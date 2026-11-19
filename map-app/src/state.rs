//! Headless app state (T12): everything the egui shell reads and writes,
//! in plain data.
//!
//! No egui types live here on purpose — the state machine (seed parsing,
//! source loading, stale-result rejection, resolution changes, the
//! layer stack, go-to submission, status formatting) is fully
//! unit-testable without a window, a GPU or even the generator.
//! `app.rs` is the thin egui skin over this module; the hover sampling
//! state machine itself lives in [`crate::inspector`].
//!
//! # Source loading model
//!
//! A load is *issued* by [`AppState::begin_load`] with a monotonically
//! increasing generation and executed on a background thread; the result
//! comes back through a channel and is applied by
//! [`AppState::apply_load_result`], which accepts it **only** while the
//! state is still `Loading` for that generation. Editing the seed text
//! (or invalidating) resets the state, so a result that lands afterwards
//! is stale and dropped — this is the guard the whole UI hangs on.
//!
//! Heavy sources never drop on the UI thread: any displaced
//! `WorldgenSource` is parked in a retirement list that the app drains
//! each frame and drops on a detached background thread, because dropping
//! joins the generator worker pool.

use std::path::PathBuf;
use std::sync::Arc;

use map_core::error::SourceResult;
use map_core::layers::LayerStack;
use map_core::render::Layer;
use map_core::source::worldgen::{WorldgenSource, WorldgenSourceConfig};
use map_core::types::{Rect, Seed, SeedError};

use crate::inspector::{GoToDialog, InspectorState, parse_go_to};
use crate::labels;
use crate::view_state::MapViewState;

/// The resolution steps the top-bar selector offers (blocks per sample).
/// The plan's default is 2×2.
pub const RESOLUTION_STEPS: [u32; 4] = [1, 2, 4, 8];

/// Whether the panel offers `layer` a checkbox (T19): true for every
/// layer with a renderer today. The later layers stay [`LayerStack`]
/// entries — their settings persist for their arrival — but no checkbox
/// schedules an unsupported query.
#[must_use]
pub fn offers_layer(layer: Layer) -> bool {
    matches!(
        layer,
        Layer::Biome | Layer::Height | Layer::Relief | Layer::Fluid | Layer::Surface | Layer::Zone
    )
}

/// The panel label of a layer (checkboxes, the status summary and the
/// future-layer notes share one mapping so the vocabulary never drifts).
#[must_use]
pub fn layer_label(layer: Layer) -> &'static str {
    match layer {
        Layer::Biome => labels::LAYER_BIOMES,
        Layer::Height => labels::LAYER_HEIGHT,
        Layer::Relief => labels::LAYER_RELIEF,
        Layer::Fluid => labels::LAYER_FLUID,
        Layer::Zone => labels::LAYER_ZONE,
        Layer::Surface => labels::LAYER_SURFACE,
        Layer::CaveSlice | Layer::CaveDensity => labels::LAYER_CAVES,
    }
}

/// The overlay checkboxes of the left panel. Placeholder booleans until
/// T23/T27/T29 render real overlays; the panel writes here either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlaySettings {
    pub structures: bool,
    pub prefabs: bool,
    pub cave_entrances: bool,
    pub chunk_grid: bool,
}

impl Default for OverlaySettings {
    fn default() -> Self {
        // The plan's layout ships with structures, prefabs and cave
        // entrances checked, the debug chunk grid off.
        OverlaySettings {
            structures: true,
            prefabs: true,
            cave_entrances: true,
            chunk_grid: false,
        }
    }
}

/// What a background load delivers back to the UI thread.
pub type LoadResult = (u64, SourceResult<WorldgenSource>);

/// Everything a background load needs: the app spawns one thread per
/// request and never constructs a `WorldgenSource` on the UI thread.
#[derive(Debug, Clone)]
pub struct LoadRequest {
    /// The generation this load was issued under.
    pub generation: u64,
    /// The already-parsed seed (T02 semantics — invalid text never
    /// reaches a worker).
    pub seed: Seed,
    /// Resolved world-gen folder (Java provider semantics downstream).
    pub world_path: PathBuf,
    /// Worker pool size; `None` = the source's default (2).
    pub workers: Option<usize>,
}

impl LoadRequest {
    /// The worker-side config: build it, call [`WorldgenSource::new`],
    /// send the result home.
    #[must_use]
    pub fn config(&self) -> WorldgenSourceConfig {
        let mut config = WorldgenSourceConfig::new(&self.world_path, self.seed.input());
        config.workers = self.workers;
        config
    }
}

/// The source lifecycle. `Ready` owns the heavy `WorldgenSource` (plus
/// its worker pool) behind an [`Arc`] so background texture requests can
/// hold it while the UI thread moves on; `Error` carries the
/// user-readable failure of the last load so the banner can offer a
/// retry.
///
/// `Debug` is manual: the source (and its pool of `JoinHandle`s) has no
/// cheap printable form, and state debugging cares about *which*
/// variant, not the source contents.
pub enum SourceState {
    /// No source, no load in flight, no error to show.
    Idle,
    /// A load for `generation` is running on a background thread.
    Loading { generation: u64 },
    /// A source loaded successfully and owns the current view's data.
    Ready(Arc<WorldgenSource>),
    /// The last load failed; the seed text is unchanged, so retry means
    /// re-issuing the same load.
    Error { message: String },
}

impl std::fmt::Debug for SourceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceState::Idle => f.write_str("Idle"),
            SourceState::Loading { generation } => {
                write!(f, "Loading {{ generation: {generation} }}")
            }
            SourceState::Ready(_) => f.write_str("Ready(<WorldgenSource>)"),
            SourceState::Error { message } => {
                write!(f, "Error {{ message: {message:?} }}")
            }
        }
    }
}

/// The state behind the egui shell.
///
/// Invariants:
///
/// - `source` is `Loading { generation }` **iff** a load is in flight,
///   and that generation is the newest issued;
/// - `loaded_seed_input` mirrors the seed text the current source (or
///   the in-flight load) belongs to — editing away from it invalidates;
/// - displaced sources go to `retired`, never dropped on the UI thread.
///
/// No `Debug`: the source in `Ready` has no printable form; use the
/// accessors.
pub struct AppState {
    seed_text: String,
    parsed_seed: Option<Seed>,
    seed_error: Option<SeedError>,

    source: SourceState,
    /// Seed text the current source/in-flight load was issued for.
    loaded_seed_input: Option<String>,
    /// The load-generation source of truth; bumped by every issue.
    next_generation: u64,

    /// Resolved world folder the next load will use.
    world_path: PathBuf,
    workers: Option<usize>,

    /// Blocks per sample (one of [`RESOLUTION_STEPS`]); the plan's
    /// default is 2.
    resolution_step: u32,

    /// The interactive view (T16's pan/zoom state): the world position
    /// at the map center, the continuous wheel zoom, the viewport size.
    /// The map widget drives it; everything else reads it.
    view: MapViewState,

    /// The layer stack (T19): every planned layer with enable, opacity
    /// and order. It is plain session state — a source reload never
    /// touches it, so settings survive; serialization arrives with T31.
    layers: LayerStack,

    /// The overlay checkboxes of the left panel (placeholders).
    overlays: OverlaySettings,

    /// Cursor block coordinates once the map widget exists (T16).
    cursor: Option<(i32, i32)>,

    /// The hover sampling pipeline (T18): debounced background surface
    /// samples, a small LRU and the current readout knowledge. Reset
    /// whenever the source changes — its samples belong to exactly one
    /// source.
    inspector: InspectorState,

    /// Transient one-line message (keyboard placeholders etc.).
    status: String,

    /// The go-to dialog (`G` opens it, T18): text fields plus the inline
    /// error of the last failed submit. [`None`] = closed.
    go_to: Option<GoToDialog>,

    search_focused: bool,

    /// Heavy sources waiting for a background drop. [`Arc`] refs: a
    /// background texture request may still hold one, in which case the
    /// pool joins on *that* thread when the last ref drops — never the
    /// UI thread.
    retired: Vec<Arc<WorldgenSource>>,
}

impl Default for AppState {
    fn default() -> Self {
        AppState {
            seed_text: String::new(),
            parsed_seed: None,
            seed_error: None,
            source: SourceState::Idle,
            loaded_seed_input: None,
            next_generation: 0,
            world_path: PathBuf::new(),
            workers: None,
            resolution_step: 2,
            view: MapViewState::new(),
            layers: LayerStack::new(),
            overlays: OverlaySettings::default(),
            cursor: None,
            inspector: InspectorState::new(),
            status: String::new(),
            go_to: None,
            search_focused: false,
            retired: Vec::new(),
        }
    }
}

impl AppState {
    /// Creates the state for a resolved world folder. The app passes
    /// [`map_core::source::worldgen::default_world_path`] unless the user
    /// overrides it.
    pub fn new(world_path: impl Into<PathBuf>) -> AppState {
        AppState {
            world_path: world_path.into(),
            // The plan's default resolution is 2×2.
            resolution_step: 2,
            ..AppState::default()
        }
    }

    // -- Seed ---------------------------------------------------------------

    /// Runs `edit` against the seed text, then re-parses and applies the
    /// change rules (see [`AppState::set_seed_text`]).
    ///
    /// The egui text edit needs a `&mut String`; this closure-based
    /// access is the one door, so no edit can bypass the state machine.
    pub fn with_seed_text<R>(&mut self, edit: impl FnOnce(&mut String) -> R) -> R {
        let mut text = std::mem::take(&mut self.seed_text);
        let result = edit(&mut text);
        self.set_seed_text(text);
        result
    }

    /// Sets the seed text and re-parses it (T02 semantics live in
    /// [`Seed`]).
    ///
    /// Moving away from the text the current source (or in-flight load)
    /// belongs to **invalidates** it: the state returns to `Idle` and a
    /// load that completes afterwards is dropped as stale. This is the
    /// acceptance rule "changing the seed while loading does not apply
    /// the old result".
    pub fn set_seed_text(&mut self, text: String) {
        if self.seed_text == text {
            return;
        }
        self.seed_text = text;
        if self.seed_text.is_empty() {
            self.parsed_seed = None;
            self.seed_error = None;
            self.invalidate_source();
            return;
        }
        match Seed::parse(&self.seed_text) {
            Ok(seed) => {
                self.parsed_seed = Some(seed);
                self.seed_error = None;
            }
            Err(error) => {
                self.parsed_seed = None;
                self.seed_error = Some(error);
            }
        }
        // A parsed seed equal to the loaded one keeps the source; any
        // other edit invalidates it.
        if self.parsed_seed.as_ref().map(Seed::input) != self.loaded_seed_input.as_deref() {
            self.invalidate_source();
        }
    }

    /// The seed text exactly as the user typed it.
    ///
    /// Reserved for the seed-history display (T31); the shell edits
    /// through [`AppState::with_seed_text`].
    #[allow(dead_code)]
    #[must_use]
    pub fn seed_text(&self) -> &str {
        &self.seed_text
    }

    /// The parsed seed, when the text is valid.
    #[must_use]
    pub fn parsed_seed(&self) -> Option<&Seed> {
        self.parsed_seed.as_ref()
    }

    /// Why the seed text does not parse, when it does not.
    #[must_use]
    pub fn seed_error(&self) -> Option<&SeedError> {
        self.seed_error.as_ref()
    }

    // -- Loading ------------------------------------------------------------

    /// Issues a background load, if one is possible right now.
    ///
    /// Returns [`None`] — and changes nothing — when a load is already
    /// running, the text is empty, or the seed does not parse (the Go
    /// button is disabled for exactly these cases; this is the guard
    /// behind it). Otherwise the state becomes `Loading` under a fresh
    /// generation and the caller spawns the worker.
    pub fn begin_load(&mut self) -> Option<LoadRequest> {
        if matches!(self.source, SourceState::Loading { .. })
            || self.seed_text.is_empty()
            || self.parsed_seed.is_none()
        {
            return None;
        }
        let generation = self.next_generation;
        self.next_generation += 1;

        // A re-load over a ready source retires the old one; dropping
        // happens off the UI thread (see the module docs).
        self.retire_ready_source();
        self.source = SourceState::Loading { generation };
        self.loaded_seed_input = Some(self.seed_text.clone());
        self.status.clear();

        Some(LoadRequest {
            generation,
            seed: self.parsed_seed.clone().expect("checked above"),
            world_path: self.world_path.clone(),
            workers: self.workers,
        })
    }

    /// Applies a background load's result; true when it was accepted.
    ///
    /// The result is accepted **only** while the state is still `Loading`
    /// for exactly this generation. Anything else — an edit reset the
    /// state, a newer load was issued — makes it stale: an `Ok` source is
    /// parked for background drop (never dropped here) and `false` is
    /// returned. The UI never sees a result for a seed it no longer shows.
    pub fn apply_load_result(
        &mut self,
        generation: u64,
        result: SourceResult<WorldgenSource>,
    ) -> bool {
        if !matches!(
            self.source,
            SourceState::Loading { generation: current } if current == generation
        ) {
            if let Ok(source) = result {
                self.retired.push(Arc::new(source));
            }
            return false;
        }

        match result {
            Ok(source) => {
                self.status = format!("{} {}", labels::STATUS_SOURCE_READY, self.seed_text);
                // A new source means new biome ids, new surfaces: the
                // inspector's cached samples and pending readout belong
                // to the source that just left.
                self.inspector.reset();
                self.source = SourceState::Ready(Arc::new(source));
            }
            Err(error) => {
                self.source = SourceState::Error {
                    message: error.to_string(),
                };
            }
        }
        true
    }

    /// The background worker died without reporting (it panicked, or the
    /// process is going away). Fails the in-flight load so the error
    /// banner offers a retry instead of an eternal spinner.
    pub fn load_worker_died(&mut self) {
        if let SourceState::Loading { .. } = self.source {
            self.source = SourceState::Error {
                message: labels::STATUS_WORKER_DIED.to_string(),
            };
        }
    }

    /// Drops the notion of a current source: `Ready` is parked for a
    /// background drop, an in-flight load is declared dead (its result
    /// will be rejected as stale), and an error banner is cleared.
    ///
    /// The transient status line is cleared too: a "ready <seed>" message
    /// must not outlive the source it announced. The map widget (T16)
    /// retires its tile pipeline and textures in the same situation —
    /// a dead seed's tiles must never stay on screen.
    pub fn invalidate_source(&mut self) {
        self.retire_ready_source();
        self.source = SourceState::Idle;
        self.loaded_seed_input = None;
        self.status.clear();
        // Samples and pending reads belong to the dead source; a hover
        // reply still in flight will find the state reset here and drop.
        self.inspector.reset();
    }

    /// True while a load is in flight (drives the Go button and spinner).
    #[must_use]
    pub fn is_loading(&self) -> bool {
        matches!(self.source, SourceState::Loading { .. })
    }

    /// The loaded source, when there is one.
    #[must_use]
    pub fn source(&self) -> Option<&WorldgenSource> {
        match &self.source {
            SourceState::Ready(source) => Some(source.as_ref()),
            _ => None,
        }
    }

    /// The loaded source as a shared handle, for background work: a
    /// texture request thread holds it while the UI thread may replace
    /// or drop the state's own reference. The pool joins wherever the
    /// last reference drops — with this pattern, never the UI thread.
    #[must_use]
    pub fn source_arc(&self) -> Option<Arc<WorldgenSource>> {
        match &self.source {
            SourceState::Ready(source) => Some(Arc::clone(source)),
            _ => None,
        }
    }

    /// The last load's failure, when there is one (the error banner).
    #[must_use]
    pub fn source_error(&self) -> Option<&str> {
        match &self.source {
            SourceState::Error { message } => Some(message),
            _ => None,
        }
    }

    /// A short status word for the status bar: idle / loading / ready /
    /// error.
    #[must_use]
    pub fn source_status_text(&self) -> &'static str {
        match &self.source {
            SourceState::Idle => labels::STATUS_SOURCE_IDLE,
            SourceState::Loading { .. } => labels::STATUS_SOURCE_LOADING,
            SourceState::Ready(_) => labels::STATUS_SOURCE_READY,
            SourceState::Error { .. } => labels::STATUS_SOURCE_ERROR,
        }
    }

    /// Displaced sources, taken for a background drop. The app calls this
    /// every frame; an empty vec is the common case.
    pub fn take_retired_sources(&mut self) -> Vec<Arc<WorldgenSource>> {
        std::mem::take(&mut self.retired)
    }

    /// Moves a `Ready` source into retirement (called by every path that
    /// leaves `Ready`).
    fn retire_ready_source(&mut self) {
        if let SourceState::Ready(source) = std::mem::replace(&mut self.source, SourceState::Idle) {
            self.retired.push(source);
        }
    }

    // -- View / panel settings ----------------------------------------------

    /// The interactive map view (T16): center, continuous zoom and
    /// viewport size. The map widget writes; the status bar, the
    /// inspector (T18) and go-to (T29) read.
    #[must_use]
    pub fn view(&self) -> &MapViewState {
        &self.view
    }

    /// Mutable view access, for the map widget's input handling.
    pub fn view_mut(&mut self) -> &mut MapViewState {
        &mut self.view
    }

    /// The blocks-per-sample resolution (one of [`RESOLUTION_STEPS`]).
    #[must_use]
    pub fn resolution_step(&self) -> u32 {
        self.resolution_step
    }

    /// Sets the resolution, refusing values the selector does not offer;
    /// the return value tells the caller whether it stuck.
    pub fn set_resolution_step(&mut self, step: u32) -> bool {
        if RESOLUTION_STEPS.contains(&step) {
            self.resolution_step = step;
            true
        } else {
            false
        }
    }

    /// The layer stack (read-only access for the panels).
    #[must_use]
    pub fn layers(&self) -> &LayerStack {
        &self.layers
    }

    /// The layer stack, for the panel's toggles and sliders.
    pub fn layers_mut(&mut self) -> &mut LayerStack {
        &mut self.layers
    }

    /// The frame's paint list: enabled layers the panel offers, in stack
    /// order (bottom → top), each with its paint-time opacity. Only
    /// these are scheduled and painted — a disabled or unsupported
    /// layer never reaches the scheduler.
    #[must_use]
    pub fn schedulable_layers(&self) -> Vec<(Layer, f32)> {
        self.layers
            .enabled_layers()
            .into_iter()
            .filter(|layer| offers_layer(*layer))
            .map(|layer| (layer, self.layers.opacity(layer)))
            .collect()
    }

    /// The status bar's layer segment: the enabled layers joined with
    /// `+` (e.g. `Biomes+Fluid`), or [`labels::LAYERS_NONE`] when the
    /// map is blank on purpose.
    #[must_use]
    pub fn layer_summary(&self) -> String {
        let enabled: Vec<&str> = self
            .schedulable_layers()
            .iter()
            .map(|(layer, _)| layer_label(*layer))
            .collect();
        if enabled.is_empty() {
            labels::LAYERS_NONE.to_string()
        } else {
            enabled.join("+")
        }
    }

    /// The overlay checkboxes (mutable so the panel writes them directly).
    pub fn overlays_mut(&mut self) -> &mut OverlaySettings {
        &mut self.overlays
    }

    /// Cursor block coordinates for the status bar, once the map widget
    /// reports them (T16).
    #[must_use]
    pub fn cursor(&self) -> Option<(i32, i32)> {
        self.cursor
    }

    /// Reports the cursor's block coordinates (or its absence).
    pub fn set_cursor(&mut self, cursor: Option<(i32, i32)>) {
        self.cursor = cursor;
    }

    /// The transient status line (keyboard placeholder feedback etc.).
    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Sets the transient status line.
    pub fn set_status(&mut self, status: impl Into<String>) {
        self.status = status.into();
    }

    // -- Inspector (T18) -----------------------------------------------------

    /// The hover sampling pipeline (read-only access for the panels).
    #[must_use]
    pub fn inspector(&self) -> &InspectorState {
        &self.inspector
    }

    /// The hover sampling pipeline, for the app's per-frame drive.
    pub fn inspector_mut(&mut self) -> &mut InspectorState {
        &mut self.inspector
    }

    // -- Go-to (T18) ---------------------------------------------------------

    /// Toggles the go-to dialog (`G`): opening starts from empty fields,
    /// toggling while open closes it (a submit closes it too).
    pub fn toggle_go_to(&mut self) {
        self.go_to = if self.go_to.is_some() {
            None
        } else {
            Some(GoToDialog::default())
        };
    }

    /// Closes the go-to dialog (`Esc` or the Close button).
    pub fn close_go_to(&mut self) {
        self.go_to = None;
    }

    /// Whether the go-to dialog is open.
    #[must_use]
    pub fn go_to(&self) -> Option<&GoToDialog> {
        self.go_to.as_ref()
    }

    /// The go-to dialog's editable fields, for the text edits.
    pub fn go_to_mut(&mut self) -> Option<&mut GoToDialog> {
        self.go_to.as_mut()
    }

    /// Validates and applies the go-to dialog: parse the two fields
    /// ([`parse_go_to`]), reject coordinates outside the generatable
    /// `world` (showing its range), then center the view — keeping the
    /// current zoom — and close. The return value tells whether the view
    /// moved.
    ///
    /// A failure leaves the dialog open with the inline error set.
    pub fn submit_go_to(&mut self, world: Option<Rect>) -> bool {
        let Some(dialog) = self.go_to.as_mut() else {
            return false;
        };
        let (x, z) = match parse_go_to(&dialog.x_text, &dialog.z_text) {
            Ok((x, z)) => (x, z),
            Err(message) => {
                dialog.error = Some(message);
                return false;
            }
        };
        if let Some(world) = world
            && !world.contains(x, z)
        {
            dialog.error = Some(format!(
                "outside the world: x {}..{}, z {}..{}",
                world.min_x, world.max_x, world.min_z, world.max_z,
            ));
            return false;
        }
        self.go_to = None;
        self.view.set_center(f64::from(x), f64::from(z));
        self.status = format!("{} X:{x} Z:{z}", labels::STATUS_WENT_TO);
        true
    }

    // -- Keyboard placeholder actions ---------------------------------------

    /// Marks the search section placeholder (`F`).
    pub fn focus_search(&mut self) {
        self.search_focused = true;
    }

    /// Whether the search placeholder was focused (the panel clears it).
    #[must_use]
    pub fn take_search_focus(&mut self) -> bool {
        std::mem::take(&mut self.search_focused)
    }

    /// The Ctrl+S export placeholder: no PNG yet (T32), just a status
    /// message proving the shortcut fired.
    pub fn export_placeholder(&mut self) {
        self.status = labels::STATUS_EXPORT_PLACEHOLDER.to_string();
    }

    // -- World path ----------------------------------------------------------

    /// The world folder the next load will use.
    #[allow(dead_code)] // reserved: T31's settings panel displays it
    #[must_use]
    pub fn world_path(&self) -> &PathBuf {
        &self.world_path
    }

    /// Overrides the world folder (settings arrive with T31).
    #[allow(dead_code)] // reserved: T31's settings panel
    pub fn set_world_path(&mut self, path: impl Into<PathBuf>) {
        self.world_path = path.into();
    }

    /// The worker-pool size override for the next load.
    #[must_use]
    pub fn workers(&self) -> Option<usize> {
        self.workers
    }

    /// Sets the worker-pool size override.
    pub fn set_workers(&mut self, workers: Option<usize>) {
        self.workers = workers;
    }
}

/// The resolution readout: the selector's step, with the zoom-forced
/// effective step spelled out when it is coarser —
/// `2×2 (effective 4×4)`.
#[must_use]
pub fn format_resolution(step: u32, effective_step: u32) -> String {
    if effective_step > step {
        format!(
            "{step}×{step} ({EFFECTIVE} {effective_step}×{effective_step})",
            EFFECTIVE = labels::STATUS_EFFECTIVE,
        )
    } else {
        format!("{step}×{step}")
    }
}

/// Formats the status bar's tail: everything after the hover readout of
/// [`inspector::format_readout`] — pending tiles, zoom, the resolution
/// with its zoom-forced effective step, the layer and the source status.
///
/// Pure on purpose: the app tests it with synthetic values.
///
/// ```text
/// tiles 12 | zoom 0.5 pt/blk | 2×2 (effective 4×4) | Relief | ready
/// ```
#[must_use]
pub fn format_status(
    pending_tiles: u32,
    zoom: f64,
    step: u32,
    effective_step: u32,
    layer: &str,
    source_status: &str,
) -> String {
    format!(
        "{TILES} {pending_tiles} | {ZOOM} {zoom:.1} {PTBLK} | {resolution} | {layer} | \
         {source_status}",
        TILES = labels::STATUS_TILES,
        ZOOM = labels::STATUS_ZOOM,
        PTBLK = labels::STATUS_PT_PER_BLOCK,
        resolution = format_resolution(step, effective_step),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use map_core::error::SourceError;
    use map_core::source::WorldSource;

    fn state() -> AppState {
        AppState::new("/nonexistent/world")
    }

    #[test]
    fn seed_text_parses_and_reports_errors() {
        let mut state = state();
        state.set_seed_text(String::from("4294967297"));
        let seed = state.parsed_seed().expect("numeric seeds parse");
        assert_eq!(seed.world(), 4_294_967_297);
        assert_eq!(seed.generator(), 1, "documented low-32-bit truncation");
        assert!(state.seed_error().is_none());

        state.set_seed_text(String::from("abc"));
        assert!(state.parsed_seed().is_none(), "non-numeric is rejected");
        assert!(state.seed_error().is_some(), "the reason is reported");
        // Empty text clears both sides.
        state.set_seed_text(String::new());
        assert!(state.parsed_seed().is_none());
        assert!(state.seed_error().is_none());
    }

    #[test]
    fn begin_load_refuses_empty_invalid_and_inflight() {
        let mut state = state();

        assert!(state.begin_load().is_none(), "empty text never loads");
        state.set_seed_text(String::from("12x"));
        assert!(
            state.begin_load().is_none(),
            "an invalid seed never reaches a worker"
        );

        state.set_seed_text(String::from("42"));
        let first = state.begin_load().expect("a valid seed issues a load");
        assert_eq!(first.generation, 0);
        assert_eq!(first.seed.world(), 42);
        assert!(state.is_loading());
        assert!(
            state.begin_load().is_none(),
            "a second Go while loading changes nothing"
        );
        assert!(state.is_loading());
    }

    #[test]
    fn workers_setting_applies_to_the_next_load() {
        // T21's cost control: the top bar's cap feeds the GENERATOR pool
        // of the next load, never the current one.
        let mut state = state();
        state.set_workers(Some(4));
        assert_eq!(state.workers(), Some(4));
        state.set_seed_text(String::from("42"));
        let request = state.begin_load().expect("the load issues");
        assert_eq!(
            request.workers,
            Some(4),
            "the cap rides the next load request"
        );
        assert_eq!(
            request.config().workers,
            Some(4),
            "and reaches the generator config"
        );
    }

    #[test]
    fn load_error_lands_in_the_error_state() {
        let mut state = state();
        state.set_seed_text(String::from("1"));
        let request = state.begin_load().expect("the load issues");

        let accepted = state.apply_load_result(
            request.generation,
            Err(SourceError::Load(String::from("no such world"))),
        );
        assert!(accepted, "the result was for the current generation");
        assert_eq!(state.source_error(), Some("load failed: no such world"));
        assert_eq!(state.source_status_text(), labels::STATUS_SOURCE_ERROR);
        assert!(!state.is_loading());

        // Retry re-issues with a fresh generation.
        let retry = state.begin_load().expect("retry issues a new load");
        assert_eq!(retry.generation, request.generation + 1);
        assert!(state.is_loading());
    }

    #[test]
    fn stale_results_are_rejected() {
        let mut state = state();
        state.set_seed_text(String::from("7"));
        let request = state.begin_load().expect("the load issues");

        // An unrelated generation is stale even while loading.
        assert!(!state.apply_load_result(
            request.generation + 7,
            Err(SourceError::Load(String::from("late"))),
        ));
        assert!(state.is_loading(), "the in-flight load is untouched");

        // Editing the seed mid-load invalidates: the result then drops.
        state.set_seed_text(String::from("8"));
        assert!(!state.is_loading(), "the edit reset the state");
        assert_eq!(state.source_status_text(), labels::STATUS_SOURCE_IDLE);
        assert!(!state.apply_load_result(
            request.generation,
            Err(SourceError::Load(String::from("too late"))),
        ));
        assert_eq!(state.source_status_text(), labels::STATUS_SOURCE_IDLE);
    }

    #[test]
    fn editing_the_seed_text_invalidates_a_ready_source() {
        let mut state = state();
        state.set_seed_text(String::from("5"));
        let _ = state.begin_load().expect("the load issues");
        // Simulate success without a real generator: park a source via the
        // invalidation path instead — a full ready-transition test runs in
        // the worldgen-gated test below.
        state.set_seed_text(String::from("6"));
        assert!(state.source().is_none(), "no source survives an edit");
        assert_eq!(state.source_status_text(), labels::STATUS_SOURCE_IDLE);

        // Editing back to the same text does not resurrect anything.
        state.set_seed_text(String::from("6"));
        assert!(state.source().is_none());
    }

    #[test]
    fn resolution_selector_updates_and_rejects_unknown_steps() {
        let mut state = state();
        assert_eq!(state.resolution_step(), 2, "defaults to 2x2");
        for step in RESOLUTION_STEPS {
            assert!(state.set_resolution_step(step), "step {step} is offered");
            assert_eq!(state.resolution_step(), step);
        }
        for bad in [0, 3, 5, 16, 256] {
            assert!(!state.set_resolution_step(bad), "step {bad} is refused");
        }
    }

    #[test]
    fn layers_default_to_biomes_and_toggle_through_the_stack() {
        // T19: the radio became a stack of checkboxes. The default is
        // the plan's (Biome alone at full opacity); toggling others
        // grows the paint list in stack order, and the status summary
        // follows.
        let mut state = state();
        assert_eq!(
            state.schedulable_layers(),
            vec![(Layer::Biome, 1.0)],
            "the plan's default: one layer, full opacity"
        );
        assert_eq!(state.layer_summary(), labels::LAYER_BIOMES);

        // Enabling Height and Fluid paints bottom → top in stack order.
        assert!(state.layers_mut().set_enabled(Layer::Height, true));
        assert!(state.layers_mut().set_enabled(Layer::Fluid, true));
        assert_eq!(
            state.schedulable_layers(),
            vec![
                (Layer::Biome, 1.0),
                (Layer::Height, 1.0),
                (Layer::Fluid, 1.0)
            ],
            "the paint list is the stack's order, not the click order"
        );
        assert_eq!(
            state.layer_summary(),
            format!(
                "{}+{}+{}",
                labels::LAYER_BIOMES,
                labels::LAYER_HEIGHT,
                labels::LAYER_FLUID
            )
        );

        // Opacity clamps into 0..=1 and feeds the paint list. A clamp
        // that lands on the current value is a no-op write (false); a
        // real change reports true and reaches the paint list.
        assert!(!state.layers_mut().set_opacity(Layer::Height, 2.0));
        assert_eq!(state.layers_mut().opacity(Layer::Height), 1.0, "clamped");
        assert!(state.layers_mut().set_opacity(Layer::Height, 0.25));
        assert_eq!(
            state.schedulable_layers()[1],
            (Layer::Height, 0.25),
            "the slider's value reaches the paint list"
        );

        // Disabling everything is allowed: a blank map with a summary.
        for layer in [Layer::Biome, Layer::Height, Layer::Fluid] {
            state.layers_mut().set_enabled(layer, false);
        }
        assert_eq!(state.schedulable_layers(), Vec::new());
        assert_eq!(state.layer_summary(), labels::LAYERS_NONE);
    }

    #[test]
    fn layers_unoffered_and_future_layers_never_reach_the_scheduler() {
        // The stack carries every planned layer so settings persist, but
        // only layers with renderers get a checkbox and a tile request:
        // Caves (T28) never schedule however their entries read, while
        // Surface joined once T21 made it render.
        let mut state = state();
        assert!(state.layers_mut().set_enabled(Layer::Surface, true));
        assert!(state.layers_mut().set_enabled(Layer::CaveSlice, true));
        assert_eq!(
            state.schedulable_layers(),
            vec![(Layer::Biome, 1.0), (Layer::Surface, 1.0)],
            "surface is schedulable now; caves still are not"
        );
        assert_eq!(
            state.layer_summary(),
            format!("{}+{}", labels::LAYER_BIOMES, labels::LAYER_SURFACE)
        );
    }

    #[test]
    fn layers_survive_a_source_reload() {
        // Session state, not source state: a reload (or a failed one)
        // never touches the stack — T31 then serializes it across
        // restarts.
        let mut state = state();
        assert!(state.layers_mut().set_enabled(Layer::Zone, true));
        assert!(state.layers_mut().set_opacity(Layer::Zone, 0.4));
        assert!(state.layers_mut().set_enabled(Layer::Biome, false));

        state.set_seed_text(String::from("7"));
        let request = state.begin_load().expect("the load issues");
        assert!(state.apply_load_result(
            request.generation,
            Err(SourceError::Load(String::from("no such world"))),
        ));
        assert!(!state.layers().enabled_layers().contains(&Layer::Biome));
        assert!(state.layers().enabled_layers().contains(&Layer::Zone));
        assert_eq!(state.layers().opacity(Layer::Zone), 0.4);
    }

    #[test]
    fn layers_labels_and_offers_agree_on_the_vocabulary() {
        // Every planned layer has a label; the offered set is exactly
        // the layers with renderers today (Surface joined with T21).
        for layer in [
            Layer::Biome,
            Layer::Height,
            Layer::Relief,
            Layer::Fluid,
            Layer::Surface,
            Layer::CaveSlice,
            Layer::CaveDensity,
            Layer::Zone,
        ] {
            assert!(!layer_label(layer).is_empty(), "{layer:?} is labelled");
            assert_eq!(
                offers_layer(layer),
                !matches!(layer, Layer::CaveSlice | Layer::CaveDensity),
                "{layer:?}"
            );
        }
    }

    #[test]
    fn overlay_placeholders_write_through() {
        let mut state = state();
        let overlays = state.overlays_mut();
        overlays.chunk_grid = true;
        overlays.prefabs = false;
        assert!(state.overlays_mut().chunk_grid);
        assert!(!state.overlays_mut().prefabs);
        assert_eq!(
            state.overlays_mut().structures,
            OverlaySettings::default().structures,
            "untouched checkboxes keep their defaults"
        );
    }

    #[test]
    fn keyboard_placeholders_set_status_and_toggles() {
        let mut state = state();
        state.toggle_go_to();
        let dialog = state.go_to().expect("the go-to dialog opened");
        assert!(dialog.x_text.is_empty(), "the dialog starts empty");
        assert!(dialog.error.is_none());
        state.toggle_go_to();
        assert!(state.go_to().is_none(), "toggling again closes it");

        state.toggle_go_to();
        state.close_go_to();
        assert!(state.go_to().is_none(), "close is idempotent");

        state.focus_search();
        assert!(state.take_search_focus(), "the panel consumes the focus");
        assert!(!state.take_search_focus(), "and only once");

        state.export_placeholder();
        assert_eq!(state.status(), labels::STATUS_EXPORT_PLACEHOLDER);
    }

    #[test]
    fn the_view_state_lives_in_the_state_machine() {
        // T16's pan/zoom state is app state, not widget state: the
        // widget drives it, everything else reads it. A pan and a zoom
        // through the app-level accessors behave exactly like the
        // headless view_state tests promise.
        let mut state = state();
        assert_eq!(state.view().center().x, 0.0);
        assert_eq!(state.view().scale(), 1.0, "the plan's initial view");

        state.view_mut().pan_by_screen((100.0, 50.0));
        assert_eq!(state.view().center().x, -100.0);
        assert_eq!(state.view().center().z, -50.0);

        state.view_mut().zoom_at_center(1.0);
        assert_eq!(state.view().smooth_zoom(), 1.0);
        assert_eq!(state.view().scale(), 0.5);

        // The cursor/status plumbing the map widget feeds.
        state.set_cursor(Some((-1203, 455)));
        assert_eq!(state.cursor(), Some((-1203, 455)));
        state.set_status("copied X:-1203 Z:455");
        assert_eq!(state.status(), "copied X:-1203 Z:455");
    }

    #[test]
    fn resolution_changes_map_to_the_expected_effective_step() {
        // The selector sets the user's *minimum* step (T10's
        // effective_step folds it with the zoom level the view needs):
        // 2x2 at one point per block is exactly step 2, a zoomed-out
        // view forces coarser sampling no matter how fine the selector.
        let mut state = state();
        assert_eq!(state.resolution_step(), 2, "the plan's 2x2 default");
        assert_eq!(state.view().tile_step(state.resolution_step()), 2);

        state.set_resolution_step(1);
        assert_eq!(state.view().tile_step(state.resolution_step()), 1);

        // Two levels out, one point covers four blocks: 1x1 and 2x2 both
        // render at step 4, while an 8x8 selection stays 8x8.
        state.view_mut().zoom_at_center(2.0);
        assert_eq!(state.view().tile_step(1), 4);
        assert_eq!(state.view().tile_step(2), 4);
        assert_eq!(state.view().tile_step(8), 8);

        // And the finest selection survives zooming back in.
        state.set_resolution_step(8);
        state.view_mut().zoom_at_center(-2.0);
        assert_eq!(state.view().tile_step(state.resolution_step()), 8);
    }

    #[test]
    fn status_bar_formats_synthetic_values() {
        // The status bar's tail, after the readout the inspector owns:
        // a coarser effective step is spelled out, an exact one is not.
        let line = format_status(12, 0.5, 2, 4, "Relief", "ready");
        assert_eq!(
            line,
            "tiles 12 | zoom 0.5 pt/blk | 2×2 (effective 4×4) | Relief | ready"
        );

        let plain = format_status(0, 1.0, 2, 2, "Biomes", "no source");
        assert_eq!(
            plain,
            "tiles 0 | zoom 1.0 pt/blk | 2×2 | Biomes | no source"
        );
    }

    // -- The ready transition, gated on the real generator ------------------

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
    fn load_result_transitions_to_ready_with_a_real_source() {
        // The full idle → loading → ready machine needs one real
        // generator load, so it runs only where the sibling checkout
        // exists (every other test covers the paths without it).
        let Some(world) = world_dir() else {
            eprintln!("skipping: the worldgen checkout is not present");
            return;
        };
        let mut state = AppState::new(&world);
        state.set_seed_text(String::from("12345"));
        let request = state.begin_load().expect("the load issues");

        // The "worker": construct the source exactly like the app thread.
        let result = WorldgenSource::new(&request.config());
        assert!(
            state.apply_load_result(request.generation, result),
            "the current generation applies"
        );
        assert!(state.source().is_some(), "ready owns the source");
        assert_eq!(state.source().unwrap().seed().world(), 12_345);
        assert_eq!(state.source_status_text(), labels::STATUS_SOURCE_READY);

        // Re-Go retires the old source for a background drop.
        let second = state.begin_load().expect("re-Go issues a new load");
        assert_eq!(second.generation, request.generation + 1);
        let retired = state.take_retired_sources();
        assert_eq!(retired.len(), 1, "the old source was parked, not dropped");
        assert!(state.source().is_none(), "loading again");

        // A stale success for the retired generation parks, not replaces.
        let stale = WorldgenSource::new(&second.config());
        assert!(!state.apply_load_result(request.generation, stale));
        assert!(state.source().is_none());
        assert_eq!(
            state.take_retired_sources().len(),
            1,
            "the stale source was parked too"
        );
    }
}
