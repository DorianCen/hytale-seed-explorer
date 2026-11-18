//! `map-core` — the headless heart of the Hytale seed explorer.
//!
//! **The headless contract:** this crate produces pixels and data, never
//! windows. It must not depend on `eframe`, `egui`, `arboard` or
//! `directories` — enforced by review, by CI, and by
//! `cargo tree -p map-core` in the T06 acceptance gate. Everything here is
//! testable without a GPU, a display or the game client.
//!
//! Planned module tree (owners arrive with their tasks; the stubs below
//! exist so the layout is frozen early):
//!
//! - [`source`] — the `WorldSource`/`FeatureSource` traits and the adapter
//!   to the external worldgen workspace (T07, T08, T09).
//! - [`tiles`] — tile keys, scheduling, cache (T10, T14, T15).
//! - [`render`] — layer renderers: biome, height, relief, blocks, caves
//!   (T13, T17, T19–T21, T28).
//! - [`features`] — the feature catalog, icon ids, categories (T23).
//! - [`search`] — feature/biome search engines (T24, T25).
//! - [`project`] — waypoints, saved state, share strings (T30, T31).
//! - [`export`] — PNG export (T32).

pub mod export;
pub mod features;
pub mod project;
pub mod render;
pub mod search;
pub mod source;
pub mod tiles;

/// The generator identity this build is pinned to (T02). The external
/// workspace must resolve for this constant to exist.
pub const GENERATOR_VERSION: u32 = worldgen_hytale::GENERATOR_VERSION;

#[cfg(test)]
mod tests {
    /// Proves the test harness runs AND that the external generator path
    /// dependency resolves from this workspace.
    #[test]
    fn the_external_generator_resolves() {
        assert_eq!(super::GENERATOR_VERSION, 1);
        assert_eq!(worldgen_hytale::GENERATOR_VERSION, 1);
    }
}
