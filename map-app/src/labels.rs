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
/// The zone debug layer's checkbox (T19).
pub const LAYER_ZONE: &str = "Zones";
/// The word before a layer's opacity slider.
pub const OPACITY: &str = "opacity";
/// The hint under the layer list when the full-block layer samples
/// coarser than one block (T21's "auto-select step 1 only when visible"
/// note). `{n}×{n}` is the effective step.
pub const LAYER_HINT_SURFACE: &str = "full-block samples {n}×{n} — zoom in for every block";
/// What the status bar says when no renderable layer is enabled.
pub const LAYERS_NONE: &str = "none";

pub const OVERLAY_STRUCTURES: &str = "Structures";
pub const OVERLAY_PREFABS: &str = "Prefabs";
pub const OVERLAY_CAVE_ENTRANCES: &str = "Cave ent.";
pub const OVERLAY_CHUNK_GRID: &str = "Chunk grid";

/// The search placeholder (real search arrives with T24/T26).
pub const SEARCH_PLACEHOLDER: &str = "Search arrives with T24";

// -- Central map placeholder -------------------------------------------------

/// Shown when no source has been loaded yet.
pub const MAP_PLACEHOLDER_NO_SOURCE: &str = "load a seed to explore";

/// Shown while a background load runs.
pub const MAP_PLACEHOLDER_LOADING: &str = "loading world…";

/// Shown when a Caves layer is selected (its renderer is T28's).
pub const MAP_PLACEHOLDER_CAVES: &str = "cave layers arrive with T28";

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
