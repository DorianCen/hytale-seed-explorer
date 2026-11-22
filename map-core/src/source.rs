//! The headless source contract.
//!
//! Everything that produces world data for map-core implements
//! [`WorldSource`] (and optionally [`FeatureSource`]): the real worldgen
//! adapter (T08), the fake in-memory source used by the UI and tests
//! [`crate::test_support`], and future remote sources.
//!
//! Batch region methods exist so a tile never costs one channel round-trip
//! per pixel: a 256×256 tile is a single `*_region` call, not 65,536
//! `biome_at` calls. Single-point methods remain for hover, search rings and
//! spot checks.
//!
//! Trait objects (`Box<dyn WorldSource>`, `&dyn FeatureSource`) are the
//! intended storage for the adapter, so both traits are object-safe and
//! `Send + Sync`: tiles are generated on a rayon pool and read from the UI
//! thread.

use crate::catalog::{BiomeCatalog, BlockCatalog, FluidCatalog, ZoneCatalog};
use crate::error::SourceResult;
use crate::types::{
    BiomeGrid, BiomeId, Feature, FeatureFilter, GeneratorIdentity, HeightGrid, Rect, Seed,
    SliceGrid, SpawnPoint, SurfaceGrid, SurfaceSample, ZoneGrid, ZoneId,
};

/// A source of world data at the map-core boundary.
///
/// Implementors must be deterministic for a given seed and generator identity:
/// tiles are keyed by coordinates plus [`WorldSource::identity`], and the same
/// tile may be regenerated in a different worker.
pub mod chunks;
pub mod worldgen;

/// How much of the generator a chunk summary asks for (T29).
///
/// [`SummaryFidelity::Full`] runs the whole pipeline and is the only mode
/// that may serve an authoritative (refine or user-step) tile.
/// [`SummaryFidelity::SurfaceOnly`] skips the generator's cave and prefab
/// stages — the recorded 5.5× probe speedup — and is valid ONLY for
/// placeholder-level tiles: its summaries differ wherever a prefab or a
/// cave breaches the surface. The two modes are distinct cache keys, so a
/// surface-only summary can never satisfy a full request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SummaryFidelity {
    /// Tint → environment → blocks → caves → prefabs → water.
    #[default]
    Full,
    /// Tint → environment → blocks → water; caves and prefabs skipped.
    SurfaceOnly,
}

impl SummaryFidelity {
    /// Whether data rendered at `self` is at least as authoritative as
    /// `required` — the tile-cache provenance rule (T29).
    ///
    /// [`SummaryFidelity::Full`] serves every request;
    /// [`SummaryFidelity::SurfaceOnly`] serves only surface-only passes, so
    /// an authoritative pass must re-render over a weaker cached tile.
    #[must_use]
    pub fn satisfies(self, required: SummaryFidelity) -> bool {
        match self {
            SummaryFidelity::Full => true,
            SummaryFidelity::SurfaceOnly => required == SummaryFidelity::SurfaceOnly,
        }
    }
}

pub trait WorldSource: Send + Sync {
    /// The generator/assets identity used in tile keys and exports.
    fn identity(&self) -> &GeneratorIdentity;
    /// The parsed seed this source generates from.
    fn seed(&self) -> &Seed;

    /// The biome id at one block position.
    fn biome_at(&self, x: i32, z: i32) -> SourceResult<BiomeId>;
    /// A biome grid covering `rect`, one sample per `step` blocks.
    fn biome_region(&self, rect: Rect, step: u32) -> SourceResult<BiomeGrid>;

    /// The terrain surface height at one column.
    fn height_at(&self, x: i32, z: i32) -> SourceResult<i32>;
    /// A height grid covering `rect`, one sample per `step` blocks.
    fn height_region(&self, rect: Rect, step: u32) -> SourceResult<HeightGrid>;

    /// The full surface sample (biome, height, block, fluid, tint) at a column.
    ///
    /// Always [`SummaryFidelity::Full`]: a single hover/inspector sample is
    /// authoritative, never a placeholder.
    fn surface_at(&self, x: i32, z: i32) -> SourceResult<SurfaceSample>;
    /// A surface grid covering `rect`, one sample per `step` blocks, served
    /// from chunk summaries at `fidelity`.
    fn surface_region(
        &self,
        rect: Rect,
        step: u32,
        fidelity: SummaryFidelity,
    ) -> SourceResult<SurfaceGrid>;

    /// `surface_region` with caller-owned cancellation (the expensive
    /// chunk-summary path, T21): the source checks `cancel` between the
    /// chunk generations so a tile whose token fired stops early instead of
    /// paying for every chunk first. The default ignores the token — cheap
    /// sources never need it; `WorldgenSource` overrides with the real
    /// per-chunk check.
    fn surface_region_cancelled(
        &self,
        rect: Rect,
        step: u32,
        cancel: &crate::scheduler::CancelToken,
        fidelity: SummaryFidelity,
    ) -> SourceResult<SurfaceGrid> {
        let _ = cancel;
        self.surface_region(rect, step, fidelity)
    }

    /// A horizontal slice at altitude `y` covering `rect`.
    fn slice_region(&self, rect: Rect, step: u32, y: i32) -> SourceResult<SliceGrid>;

    /// Every spawn point this world offers.
    fn spawn_points(&self) -> SourceResult<Vec<SpawnPoint>>;
    /// The zone containing a column, if the generator exposes zones.
    fn zone_at(&self, x: i32, z: i32) -> SourceResult<Option<ZoneId>>;
    /// A zone-id grid covering `rect`, one sample per `step` blocks (T19's
    /// zone debug layer); [`None`] where the generator reports no zone.
    fn zone_region(&self, rect: Rect, step: u32) -> SourceResult<ZoneGrid>;

    /// Biome id → name/color lookups.
    fn biome_catalog(&self) -> &BiomeCatalog;
    /// Block id → name lookups (inspector).
    fn block_catalog(&self) -> &BlockCatalog;
    /// Fluid id → name lookups (inspector).
    fn fluid_catalog(&self) -> &FluidCatalog;
    /// Zone id → name lookups.
    fn zone_catalog(&self) -> &ZoneCatalog;
}

/// Batch feature queries.
///
/// Kept separate from [`WorldSource`] because feature placement is expensive
/// and not every source supports it (T22 decides the final query semantics).
pub trait FeatureSource: Send + Sync {
    /// Features whose position lies inside `rect` and that pass `filter`.
    fn features_in(&self, rect: Rect, filter: &FeatureFilter) -> SourceResult<Vec<Feature>>;
}

#[cfg(test)]
mod tests {
    use super::{FeatureSource, WorldSource};

    fn assert_object_safe_and_thread_safe<T: ?Sized + Send + Sync>() {}

    // Compile-time witnesses that coercion to the trait objects succeeds.
    const _: fn(&dyn WorldSource) = |_| {};
    const _: fn(&dyn FeatureSource) = |_| {};

    #[test]
    fn the_source_traits_are_object_safe_and_thread_safe() {
        // The trait bounds are `Send + Sync`, so this generic assertion proves
        // both the auto traits and that the traits can be used as `dyn`.
        assert_object_safe_and_thread_safe::<dyn WorldSource>();
        assert_object_safe_and_thread_safe::<dyn FeatureSource>();

        // Function pointers are a cheap compile-time witness that coercion to
        // the trait object succeeds.
        let _world: fn(&dyn WorldSource) = |_| {};
        let _features: fn(&dyn FeatureSource) = |_| {};
    }
}
