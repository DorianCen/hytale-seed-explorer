//! Map overlays drawn on top of the tile canvas (T23).
//!
//! Overlays are vector layers over the T16 map widget: they never touch the
//! tile cache, and their data comes from a background query. Modules here own
//! the fetch state machine and the egui styling; the pure catalog, category
//! and clustering logic lives in `map_core::features`.

pub mod features;
