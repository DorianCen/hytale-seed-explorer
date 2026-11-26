//! Every user-visible string of the app, in one place.
//!
//! The task's rule (and T34's docs/screenshots need): labels are never
//! hard-coded at their call sites — panels refer to these constants, so a
//! reword or a translation pass touches exactly one file.
//!
//! Two kinds live here:
//!
//! - **Static labels** — plain `&'static str` constants;
//! - **Status vocabulary** — the words [`crate::state::format_status`]
//!   assembles into the status bar.
//!
//! Formatted, dynamic text (errors, timings) is built where the data is,
//! from these constants plus the value being shown.

// -- Window / top bar --------------------------------------------------------

/// The window title and main heading.
pub const APP_TITLE: &str = "Hytale Seed Explorer";

/// The seed field label.
pub const SEED: &str = "Seed";

/// The load button.
pub const GO: &str = "Go";

/// The version readout prefix (followed by `generator_version`).
pub const VERSION: &str = "Version";

/// The resolution selector label.
pub const RESOLUTION: &str = "Resolution";

/// The render-mode selector's label (top bar, next to Resolution).
pub const RENDER_MODE: &str = "Mode";

/// The default render mode: today's full layer stack.
pub const MODE_SURFACE: &str = "Surface (full blocks)";

/// The hard-filter render mode: only the biome layer paints, every
/// structure/prefab/spawn overlay is dark.
pub const MODE_BIOMES_ONLY: &str = "Biomes (only)";

/// The worker-cap control's label (T21): the generator pool size the
/// NEXT load uses.
pub const WORKERS: &str = "workers";

// -- Left panel --------------------------------------------------------------

pub const GROUP_LAYERS: &str = "Layers";
pub const GROUP_OVERLAYS: &str = "Overlays";
pub const GROUP_SEARCH: &str = "Search";

pub const LAYER_BIOMES: &str = "Biomes";
pub const LAYER_SURFACE: &str = "Surface";
pub const LAYER_HEIGHT: &str = "Height";
pub const LAYER_RELIEF: &str = "Relief";
pub const LAYER_FLUID: &str = "Fluid";
pub const LAYER_CAVES: &str = "Caves";
/// The Caves group's heading (T28).
pub const GROUP_CAVES: &str = "Caves";
/// The Caves group's off choice.
pub const CAVE_OFF: &str = "Off";
/// The Caves group's Y-slice choice.
pub const CAVE_SLICE: &str = "Y-slice";
/// The Caves group's density choice.
pub const CAVE_DENSITY: &str = "Density";
/// The slice's Y slider label.
pub const CAVE_Y: &str = "Y";
/// The slice legend's solid entry.
pub const CAVE_LEGEND_SOLID: &str = "solid";
/// The slice legend's air entry (transparent).
pub const CAVE_LEGEND_AIR: &str = "air";
/// The slice legend's fluid entry.
pub const CAVE_LEGEND_FLUID: &str = "fluid";
/// The density legend's low end.
pub const CAVE_DENSITY_LOW: &str = "low";
/// The density legend's high end.
pub const CAVE_DENSITY_HIGH: &str = "high";
/// The density metric's one-line explanation.
pub const CAVE_DENSITY_HINT: &str = "air below the surface / surface height";
/// The zone debug layer's checkbox (T19).
pub const LAYER_ZONE: &str = "Zones";
/// The word before a layer's opacity slider.
pub const OPACITY: &str = "opacity";
/// The hint under the layer list when the full-block layer samples
/// coarser than one block (T21's "auto-select step 1 only when visible"
/// note). `{n}×{n}` is the effective step.
pub const LAYER_HINT_SURFACE: &str = "full-block samples {n}×{n} — zoom in for every block";
/// Why the left panel's layer checkboxes and overlay rows don't change
/// the map: the top bar's render mode is a hard filter.
pub const MODE_HINT_BIOMES_ONLY: &str = "biomes-only mode: other layers and overlays are off";
/// What the status bar says when no renderable layer is enabled.
pub const LAYERS_NONE: &str = "none";

pub const OVERLAY_STRUCTURES: &str = "Structures";
pub const OVERLAY_PREFABS: &str = "Prefabs";
pub const OVERLAY_SPAWN: &str = "Spawn";
pub const OVERLAY_POI: &str = "POI";
pub const OVERLAY_CAVE_ENTRANCES: &str = "Cave ent.";
pub const OVERLAY_CHUNK_GRID: &str = "Chunk grid";

// -- Search panel (T26) -------------------------------------------------------

/// The feature/biome target selector's label.
pub const SEARCH_MODE: &str = "Mode";
/// The feature target mode.
pub const SEARCH_MODE_FEATURES: &str = "Features";
/// The biome target mode.
pub const SEARCH_MODE_BIOME: &str = "Biome";
/// The feature-kind selector's label.
pub const SEARCH_KIND: &str = "Kind";
/// The all-kinds choice (cave entrances have their own choice; T27's dense
/// entrance scan would dominate a general search).
pub const SEARCH_KIND_ALL: &str = "All (no caves)";
/// The unique-prefab kind choice.
pub const SEARCH_KIND_UNIQUE: &str = "Unique prefabs";
/// The biome-prefab kind choice.
pub const SEARCH_KIND_BIOME_PREFAB: &str = "Biome prefabs";
/// The spawn kind choice.
pub const SEARCH_KIND_SPAWN: &str = "Spawn";
/// The POI kind choice.
pub const SEARCH_KIND_POI: &str = "POI";
/// The cave-entrance kind choice.
pub const SEARCH_KIND_CAVE: &str = "Cave entrances";
/// The category filter's label.
pub const SEARCH_CATEGORY: &str = "Category";
/// The category filter's hint (exact match, empty = any).
pub const SEARCH_CATEGORY_HINT: &str = "any";
/// The name-substring filter's label.
pub const SEARCH_NAME: &str = "Name contains";
/// The minimum-footprint filter's checkbox.
pub const SEARCH_MIN_SIZE: &str = "Min size";
/// The proximity filter's checkbox.
pub const SEARCH_PROXIMITY: &str = "Near another feature";
/// The proximity separation field's suffix.
pub const SEARCH_PROXIMITY_BLOCKS: &str = "blocks apart";
/// The biome picker's label.
pub const SEARCH_BIOME: &str = "Biome";
/// Shown when the world has no biome catalog to choose from.
pub const SEARCH_BIOME_UNKNOWN: &str = "no biomes in this world";
/// The results-count noun.
pub const SEARCH_RESULTS: &str = "results";
/// The progress line's scanned-work noun (regions or coarse samples).
pub const SEARCH_SCANNED: &str = "scanned";
/// The biome patch-area minimum checkbox.
pub const SEARCH_MIN_PATCH: &str = "Min patch area";
/// The coarse sampling step's label.
pub const SEARCH_COARSE_STEP: &str = "Coarse step";
/// The center-mode row's label.
pub const SEARCH_CENTER: &str = "Center";
/// The view-center center mode.
pub const SEARCH_CENTER_VIEW: &str = "View center";
/// The custom-point center mode.
pub const SEARCH_CENTER_CUSTOM: &str = "Custom point";
/// The radius selector's label.
pub const SEARCH_RADIUS: &str = "Radius";
/// The result-cap label.
pub const SEARCH_MAX_RESULTS: &str = "Max results";
/// The run button.
pub const SEARCH_GO: &str = "Search";
/// The cancel button.
pub const SEARCH_CANCEL: &str = "Cancel";
/// The clear-results button (idle).
pub const SEARCH_CLEAR: &str = "Clear";
/// The retry button after a failure.
pub const SEARCH_RETRY: &str = "Retry";
/// Shown before a world is loaded.
pub const SEARCH_NEED_SOURCE: &str = "load a seed to search";
/// Shown before the first search.
pub const SEARCH_IDLE: &str = "choose a target and press Search";
/// Shown while the first engine event is pending.
pub const SEARCH_STARTING: &str = "searching…";
/// Shown after Cancel until the engine acknowledges with Done.
pub const SEARCH_CANCELLING: &str = "cancelling…";
/// Shown when a completed search found nothing.
pub const SEARCH_NO_RESULTS: &str = "no results within the radius";
/// Shown when the search worker vanishes without a terminal event.
pub const SEARCH_WORKER_DIED: &str = "the search worker stopped without reporting";
/// Status-bar prefix after a result was selected.
pub const STATUS_SEARCH_SELECTED: &str = "selected";
/// Status-bar feedback after copying a result's teleport string.
pub const STATUS_SEARCH_COPIED: &str = "copied";
/// Status-bar feedback after clearing the search.
pub const STATUS_SEARCH_CLEARED: &str = "search cleared";
/// Status-bar feedback after a search starts.
pub const STATUS_SEARCH_STARTED: &str = "searching";
/// The completed-search summary prefix.
pub const SEARCH_DONE: &str = "done";
/// The cancelled-search summary suffix.
pub const SEARCH_TRUNCATED: &str = "result cap reached";
/// The cancelled-search summary.
pub const SEARCH_CANCELLED: &str = "cancelled";

// -- Central map placeholder -------------------------------------------------

/// Shown when no source has been loaded yet.
pub const MAP_PLACEHOLDER_NO_SOURCE: &str = "load a seed to explore";

/// Shown while a background load runs.
pub const MAP_PLACEHOLDER_LOADING: &str = "loading world…";

/// Shown when a rendered layer has nothing to serve: the view left the
/// generatable world.
pub const MAP_PLACEHOLDER_OFF_WORLD: &str = "this view is outside the world";

/// Shown when every renderable layer is unticked (T19 allows a blank
/// map rather than forcing a base layer).
pub const MAP_PLACEHOLDER_NO_LAYERS: &str = "no layer enabled — tick one in the Layers panel";

/// The climate debug note: the plan wants temperature/humidity only if
/// the generator exposes them — it does not (T19's explicit refusal to
/// fake them from biome colors).
pub const CLIMATE_NOTE: &str = "temperature/humidity: not exposed by this generator";

/// The zone debug layer's legend heading (T19).
pub const ZONE_LEGEND: &str = "Zone colors";

/// The legend's empty-world case.
pub const ZONE_NONE: &str = "no zones in this world";

/// Shown in the map area instead of data when a load failed.
pub const MAP_PLACEHOLDER_ERROR_PREFIX: &str = "load failed:";

// -- Go-to dialog (T18) -------------------------------------------------------

/// The go-to dialog's title (`G` opens it).
pub const GO_TO_TITLE: &str = "Go to coordinates";
/// One-line hint above the x/z fields.
pub const GO_TO_HINT: &str = "enter x and z, or paste \"x z\", \"x,z\" or \"/tp x y z\"";
/// The x field label.
pub const GO_TO_X: &str = "X";
/// The z field label.
pub const GO_TO_Z: &str = "Z";
/// The go-to submit button.
pub const GO_TO_GO: &str = "Go";
/// Generic close button.
pub const CLOSE: &str = "Close";

// -- Inspector panel (T18) ----------------------------------------------------

/// The collapsible inspector heading in the left panel.
pub const INSPECTOR: &str = "Inspector";
/// The inspector's coordinates row label.
pub const INSPECTOR_COORDINATES: &str = "Coordinates";
/// The inspector's surface-tint row label (the value is hex).
pub const INSPECTOR_TINT: &str = "Tint";
/// The fluid depth row label.
pub const INSPECTOR_DEPTH: &str = "Depth";
/// The copy-coordinates button of the inspector panel.
pub const COPY_COORDINATES: &str = "Copy coordinates";

// -- Error banner ------------------------------------------------------------

/// Prefix of the load-error banner; the message follows.
pub const ERROR_BANNER: &str = "could not load the world";

/// The banner's retry button.
pub const RETRY: &str = "Retry";

// -- Status bar --------------------------------------------------------------

/// A value the readout cannot know yet (the plan's `?`).
pub const STATUS_MISSING: &str = "?";
/// A value that does not exist (no fluid, the plan's `-`).
pub const STATUS_NONE: &str = "-";
/// What a pending surface sample shows while it generates.
pub const STATUS_PENDING: &str = "...";
/// The readout when the cursor is outside the generatable world.
pub const STATUS_OUT_OF_WORLD: &str = "out of world";
pub const STATUS_BIOME: &str = "Biome";
pub const STATUS_HEIGHT: &str = "Height";
pub const STATUS_BLOCK: &str = "Block";
pub const STATUS_FLUID: &str = "Fluid";
/// Followed by the pending-tile count.
pub const STATUS_TILES: &str = "tiles";
/// Followed by the display scale.
pub const STATUS_ZOOM: &str = "zoom";
/// The unit after the display scale (points per block).
pub const STATUS_PT_PER_BLOCK: &str = "pt/blk";
/// Prefix of the click-copy feedback (the coordinates follow).
pub const STATUS_COPIED: &str = "copied";
/// Prefix of the feature-query failure toast (the error follows).
pub const STATUS_FEATURE_ERROR: &str = "feature query failed";
/// While an overlay query (either tier or the prewarm) is in flight. A
/// tier-2 fetch can run for minutes, so it must never look like a dead UI.
pub const STATUS_FEATURES_GENERATING: &str = "features: generating…";
/// The feature-query worker vanished without a result (T23).
pub const STATUS_FEATURE_WORKER_DIED: &str = "the feature worker stopped without reporting";
/// The unit in a feature tooltip's distance line.
pub const FEATURE_BLOCKS: &str = "blocks";
/// The word after a cluster marker's count in its tooltip.
pub const FEATURE_CLUSTER: &str = "features";
/// Precedes the zoom-forced step in the resolution readout.
pub const STATUS_EFFECTIVE: &str = "effective";
/// Prefix of the go-to feedback (the coordinates follow).
pub const STATUS_WENT_TO: &str = "went to";
/// Feedback after changing the worker cap: it applies to the next load,
/// not the running generator pool (T21).
pub const STATUS_WORKERS_NEXT_LOAD: &str = "worker cap applies on the next load";

pub const STATUS_SOURCE_IDLE: &str = "no source";
pub const STATUS_SOURCE_LOADING: &str = "loading";
pub const STATUS_SOURCE_READY: &str = "ready";
pub const STATUS_SOURCE_ERROR: &str = "error";

// -- Keyboard placeholders ---------------------------------------------------

/// The Ctrl+S placeholder message (real export is T32).
pub const STATUS_EXPORT_PLACEHOLDER: &str = "PNG export arrives with T32";

/// When a load worker vanishes without a result.
pub const STATUS_WORKER_DIED: &str = "the load worker stopped without reporting";

/// The palette legend header in the left panel.
pub const PALETTE: &str = "Palette";
