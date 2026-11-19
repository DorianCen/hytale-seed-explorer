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

// -- Left panel --------------------------------------------------------------

pub const GROUP_LAYERS: &str = "Layers";
pub const GROUP_OVERLAYS: &str = "Overlays";
pub const GROUP_SEARCH: &str = "Search";

pub const LAYER_BIOMES: &str = "Biomes";
pub const LAYER_SURFACE: &str = "Surface";
pub const LAYER_HEIGHT: &str = "Height";
pub const LAYER_CAVES: &str = "Caves";

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

/// Shown when a layer is selected whose renderer is a later task's.
pub const MAP_PLACEHOLDER_LAYER: &str = "this layer's renderer arrives with T17/T28";

/// Shown in the map area instead of data when a load failed.
pub const MAP_PLACEHOLDER_ERROR_PREFIX: &str = "load failed:";

/// Reserved for the go-to dialog (T29 gives it real behavior).
pub const GO_TO_TITLE: &str = "Go to coordinates (T29)";
/// The go-to dialog's body placeholder.
pub const GO_TO_PLACEHOLDER: &str = "Coordinate entry and travel arrive with T29.";
/// Generic close button.
pub const CLOSE: &str = "Close";

// -- Error banner ------------------------------------------------------------

/// Prefix of the load-error banner; the message follows.
pub const ERROR_BANNER: &str = "could not load the world";

/// The banner's retry button.
pub const RETRY: &str = "Retry";

// -- Status bar --------------------------------------------------------------

pub const STATUS_UNKNOWN: &str = "unknown";
pub const STATUS_BIOME: &str = "Biome";
pub const STATUS_HEIGHT: &str = "Height";
pub const STATUS_BLOCK: &str = "Block";
/// Followed by the pending-tile count.
pub const STATUS_TILES: &str = "tiles";
/// Followed by the display scale.
pub const STATUS_ZOOM: &str = "zoom";
/// The unit after the display scale (points per block).
pub const STATUS_PT_PER_BLOCK: &str = "pt/blk";
/// Prefix of the click-copy feedback (the coordinates follow).
pub const STATUS_COPIED: &str = "copied";

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
