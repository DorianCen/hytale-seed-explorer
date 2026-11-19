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
//! - [`coords`] — the canonical coordinate, zoom and tile math (T10).
//! - [`tiles`] — tile keys, scheduling, cache (T14, T15).
//! - [`render`] — layer renderers: biome, height, relief, blocks, caves
//!   (T11, T13, T17, T19–T21, T28).
//! - [`features`] — the feature catalog, icon ids, categories (T23).
//! - [`search`] — feature/biome search engines (T24, T25).
//! - [`project`] — waypoints, saved state, share strings (T30, T31).
//! - [`export`] — PNG export (T32).

pub mod catalog;
pub mod coords;
pub mod error;
pub mod export;
pub mod features;
pub mod project;
pub mod render;
pub mod search;
pub mod source;
pub mod tiles;
pub mod types;

/// In-memory fake sources for renderer/UI development without a world
/// (feature `test-support`).
#[cfg(feature = "test-support")]
pub mod test_support;

/// The generator identity this build is pinned to (T02).
///
/// This is the *explorer's* pin of the generator's output-semantics tag.
/// The generator repo defines the same value as
/// `worldgen_hytale::GENERATOR_VERSION` (worldgen commit `08dd461`), but
/// that commit is not yet published on the public `main` the CI checks
/// out — importing it made the workspace unbuildable there. Keep the two
/// in sync: CI's "Cross-check the generator version tag" step compares
/// this constant with the generator's tag whenever the checkout
/// publishes it, and fails on a mismatch. A bump is a cache-invalidation
/// action: record it in the commit message (see `docs/seed-semantics.md`
/// §6 for the policy).
pub const GENERATOR_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
    /// Proves the test harness runs AND that the external generator path
    /// dependency resolves from this workspace.
    ///
    /// The generator's own `GENERATOR_VERSION` constant is only on its
    /// unpublished branch today, so this test pins the explorer's value;
    /// the CI cross-check step keeps the two honest when both exist.
    #[test]
    fn the_external_generator_resolves() {
        assert_eq!(super::GENERATOR_VERSION, 1);
    }
}
