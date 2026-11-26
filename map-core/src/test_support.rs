//! Deterministic in-memory sources for renderer/UI development without a
//! world; enabled by the `test-support` feature.
//!
//! **This module is compiled only with `--features test-support`** (see the
//! `#[cfg(feature = "test-support")]` in `lib.rs`, mirrored by the
//! `#![cfg(feature = "test-support")]` in `tests/fake_region.rs`). It is not
//! part of the shipped headless core, and its output is deliberately synthetic:
//! [`FakeWorldSource::identity`] reports generator version 0 and the
//! `"fake"` asset set so tiles produced from it can never be mistaken for
//! real generator output.

use crate::catalog::{BiomeCatalog, BiomeInfo, BlockCatalog, FluidCatalog, ZoneCatalog, ZoneInfo};
use crate::error::SourceResult;
use crate::source::chunks::CHUNK_HEIGHT;
use crate::source::{FeatureSource, SummaryFidelity, WorldSource};
use crate::types::{
    BiomeGrid, BiomeId, BlockId, CaveDensityGrid, Feature, FeatureFilter, FeatureKind,
    FeatureMetadata, FluidId, GeneratorIdentity, HeightGrid, Rect, Rect3, Seed, SliceCell,
    SliceGrid, SpawnPoint, SurfaceGrid, SurfaceSample, ZoneGrid, ZoneId,
};

/// A cheap, deterministic [`WorldSource`] for renderers, tiles and the UI.
///
/// The patterns are chosen to exercise every rendering path:
///
/// - **Height** rises in an 8-block sawtooth gradient:
///   `64 + ((x + z) / 8).rem_euclid(16)`, so neighbouring columns share a
///   height and every region shows all 16 levels.
/// - **Biomes** form an 8-block checkerboard of 64 ids (`0..=63`), repeating
///   every 64 blocks in both axes.
/// - **Lakes** appear wherever the fake height is below 66 (the two lowest
///   gradient steps), so the fluid layer and inspector have something to show.
/// - **Caves** are a deterministic sparse void band below Y=8, so the T28
///   slice/density layers have non-uniform data without the asset pack.
#[derive(Debug, Clone)]
pub struct FakeWorldSource {
    seed: Seed,
    identity: GeneratorIdentity,
    biomes: BiomeCatalog,
    blocks: BlockCatalog,
    fluids: FluidCatalog,
    zones: ZoneCatalog,
}

impl FakeWorldSource {
    /// Builds the fake world for `seed`.
    ///
    /// # Panics
    ///
    /// Panics if `seed` is not a valid seed input; the fake source is for
    /// development and tests, where an invalid seed is a programming mistake.
    #[must_use]
    pub fn new(seed: &str) -> Self {
        let seed = Seed::parse(seed).expect("fake world seed must be a valid seed input");

        let mut biomes = BiomeCatalog::default();
        for i in 0..64 {
            biomes
                .insert(BiomeInfo {
                    id: BiomeId(i),
                    name: format!("Fake_{i}"),
                    map_color: 0x22_44_66 + i as u32,
                })
                .expect("fake biome ids and names are unique");
        }

        let mut blocks = BlockCatalog::default();
        for i in 1..=64 {
            blocks
                .insert(BlockId(i), format!("Fake_Block_{i}"))
                .expect("fake block ids and names are unique");
        }

        let mut fluids = FluidCatalog::default();
        fluids
            .insert(FluidId(1), "Fake_Water")
            .expect("the fake fluid catalog is empty before insert");

        let mut zones = ZoneCatalog::default();
        zones
            .insert(ZoneInfo {
                id: ZoneId(0),
                name: "Fake_Zone".to_string(),
            })
            .expect("the fake zone catalog is empty before insert");

        Self {
            seed,
            identity: GeneratorIdentity {
                generator_version: 0,
                world_assets: "fake".to_string(),
                palette_version: 0,
            },
            biomes,
            blocks,
            fluids,
            zones,
        }
    }

    /// The fake height formula, used by both the point and region methods.
    fn height_for(x: i32, z: i32) -> i32 {
        64 + ((x + z) / 8).rem_euclid(16)
    }

    /// The fake biome formula: an 8-block checkerboard of ids `0..=63`,
    /// repeating every 64 blocks. The cell index is `x_cell + 8 * z_cell`, so
    /// every id in the 64-entry catalog is reachable.
    fn biome_for(x: i32, z: i32) -> BiomeId {
        let x_cell = x.rem_euclid(64) / 8;
        let z_cell = z.rem_euclid(64) / 8;
        BiomeId(x_cell + 8 * z_cell)
    }

    /// The full fake surface sample at one column.
    fn surface_for(x: i32, z: i32) -> SurfaceSample {
        let biome = Self::biome_for(x, z);
        let height = Self::height_for(x, z);
        let wet = height < 66;
        SurfaceSample {
            x,
            z,
            biome,
            height,
            block: BlockId(biome.0 + 1),
            fluid: wet.then_some(FluidId(1)),
            fluid_depth: if wet { (66 - height) as u16 } else { 0 },
            tint: 0,
        }
    }

    /// The world coordinate of a sample at grid index `index`.
    fn sample_coord(origin: i32, index: u32, step: u32) -> i32 {
        let step = step.min(i32::MAX as u32) as i32;
        origin.saturating_add((index as i32).saturating_mul(step))
    }

    fn biome_grid(&self, rect: Rect, step: u32) -> BiomeGrid {
        let (width, height) = rect.sample_dims(step);
        let mut values = Vec::with_capacity(width as usize * height as usize);
        for z in 0..height {
            for x in 0..width {
                values.push(Self::biome_for(
                    Self::sample_coord(rect.min_x, x, step),
                    Self::sample_coord(rect.min_z, z, step),
                ));
            }
        }
        BiomeGrid::new(rect.min_x, rect.min_z, step, width, height, values)
    }

    fn height_grid(&self, rect: Rect, step: u32) -> HeightGrid {
        let (width, height) = rect.sample_dims(step);
        let mut values = Vec::with_capacity(width as usize * height as usize);
        for z in 0..height {
            for x in 0..width {
                values.push(Self::height_for(
                    Self::sample_coord(rect.min_x, x, step),
                    Self::sample_coord(rect.min_z, z, step),
                ));
            }
        }
        HeightGrid::new(rect.min_x, rect.min_z, step, width, height, values)
    }

    fn surface_grid(&self, rect: Rect, step: u32) -> SurfaceGrid {
        let (width, height) = rect.sample_dims(step);
        let mut values = Vec::with_capacity(width as usize * height as usize);
        for z in 0..height {
            for x in 0..width {
                values.push(Self::surface_for(
                    Self::sample_coord(rect.min_x, x, step),
                    Self::sample_coord(rect.min_z, z, step),
                ));
            }
        }
        SurfaceGrid::new(rect.min_x, rect.min_z, step, width, height, values)
    }

    /// A deterministic cave-ish void: one of every ~11 cells in the band
    /// between the bedrock floor and the surface. The exact pattern is not
    /// meant to resemble Hytale caves; it gives the cave layers something
    /// non-uniform to draw and test.
    fn cave_for(x: i32, z: i32, y: i32) -> bool {
        y >= 8 && (x * 31 + z * 17 + y * 13).rem_euclid(97) < 9
    }

    /// The fake slice cell at one world position.
    fn slice_for(x: i32, z: i32, y: i32) -> SliceCell {
        if !(0..CHUNK_HEIGHT).contains(&y) || y > Self::height_for(x, z) {
            return SliceCell::Air;
        }
        if y < 8 {
            return SliceCell::Solid(BlockId(1));
        }
        if Self::cave_for(x, z, y) {
            return SliceCell::Air;
        }
        SliceCell::Solid(BlockId(Self::biome_for(x, z).0 + 1))
    }

    /// The fake density fraction, consistent with [`Self::slice_for`]:
    /// counted air cells over the cells below the top block.
    fn cave_density_for(x: i32, z: i32) -> f32 {
        let height = Self::height_for(x, z);
        if height <= 0 {
            return 0.0;
        }
        let caves = (0..height).filter(|&y| Self::cave_for(x, z, y)).count() as f32;
        caves / height as f32
    }

    /// The fake zone formula: one zone covers the whole world, matching
    /// [`WorldSource::zone_at`].
    fn zone_for(_x: i32, _z: i32) -> Option<ZoneId> {
        Some(ZoneId(0))
    }

    fn zone_grid(&self, rect: Rect, step: u32) -> ZoneGrid {
        let (width, height) = rect.sample_dims(step);
        let mut values = Vec::with_capacity(width as usize * height as usize);
        for z in 0..height {
            for x in 0..width {
                values.push(Self::zone_for(
                    Self::sample_coord(rect.min_x, x, step),
                    Self::sample_coord(rect.min_z, z, step),
                ));
            }
        }
        ZoneGrid::new(rect.min_x, rect.min_z, step, width, height, values)
    }

    /// Builds cubic `width`-block bounds used by the fake prefabs, so the
    /// largest bounds dimension (the `min_size` fallback) is exactly `width`.
    fn prefab_bounds(position: (i32, i32, i32), width: i32) -> Rect3 {
        let (x, y, z) = position;
        let half = width / 2;
        Rect3 {
            min_x: x - half,
            min_y: y - half,
            min_z: z - half,
            max_x: x + half - 1,
            max_y: y + half - 1,
            max_z: z + half - 1,
        }
    }
}

impl WorldSource for FakeWorldSource {
    fn identity(&self) -> &GeneratorIdentity {
        &self.identity
    }

    fn seed(&self) -> &Seed {
        &self.seed
    }

    fn biome_at(&self, x: i32, z: i32) -> SourceResult<BiomeId> {
        Ok(Self::biome_for(x, z))
    }

    fn biome_region(&self, rect: Rect, step: u32) -> SourceResult<BiomeGrid> {
        Ok(self.biome_grid(rect, step))
    }

    fn height_at(&self, x: i32, z: i32) -> SourceResult<i32> {
        Ok(Self::height_for(x, z))
    }

    fn height_region(&self, rect: Rect, step: u32) -> SourceResult<HeightGrid> {
        Ok(self.height_grid(rect, step))
    }

    fn surface_at(&self, x: i32, z: i32) -> SourceResult<SurfaceSample> {
        Ok(Self::surface_for(x, z))
    }

    fn surface_region(
        &self,
        rect: Rect,
        step: u32,
        _fidelity: SummaryFidelity,
    ) -> SourceResult<SurfaceGrid> {
        Ok(self.surface_grid(rect, step))
    }

    fn slice_region(&self, rect: Rect, step: u32, y: i32) -> SourceResult<SliceGrid> {
        let (width, height) = rect.sample_dims(step);
        let mut cells = Vec::with_capacity(width as usize * height as usize);
        for z in 0..height {
            for x in 0..width {
                cells.push(Self::slice_for(
                    Self::sample_coord(rect.min_x, x, step),
                    Self::sample_coord(rect.min_z, z, step),
                    y,
                ));
            }
        }
        Ok(SliceGrid::new(
            rect.min_x, rect.min_z, step, width, height, y, cells,
        ))
    }

    fn cave_density_region(&self, rect: Rect, step: u32) -> SourceResult<CaveDensityGrid> {
        let (width, height) = rect.sample_dims(step);
        let mut densities = Vec::with_capacity(width as usize * height as usize);
        for z in 0..height {
            for x in 0..width {
                densities.push(Self::cave_density_for(
                    Self::sample_coord(rect.min_x, x, step),
                    Self::sample_coord(rect.min_z, z, step),
                ));
            }
        }
        Ok(CaveDensityGrid::new(
            rect.min_x, rect.min_z, step, width, height, densities,
        ))
    }

    fn spawn_points(&self) -> SourceResult<Vec<SpawnPoint>> {
        Ok(vec![SpawnPoint {
            x: 0.5,
            y: 64.0,
            z: 0.5,
            yaw: 0.0,
        }])
    }

    fn zone_at(&self, _x: i32, _z: i32) -> SourceResult<Option<ZoneId>> {
        Ok(Some(ZoneId(0)))
    }

    fn zone_region(&self, rect: Rect, step: u32) -> SourceResult<ZoneGrid> {
        Ok(self.zone_grid(rect, step))
    }

    fn biome_catalog(&self) -> &BiomeCatalog {
        &self.biomes
    }

    fn block_catalog(&self) -> &BlockCatalog {
        &self.blocks
    }

    fn fluid_catalog(&self) -> &FluidCatalog {
        &self.fluids
    }

    fn zone_catalog(&self) -> &ZoneCatalog {
        &self.zones
    }
}

/// A fixed, plausible feature set for the feature overlay and search UI.
///
/// It contains three unique prefabs (one with negative coordinates, one with
/// `metadata.size`), a cave entrance and a spawn, so overlays, icons and
/// filters can be exercised before T22 lands.
#[derive(Debug, Clone, Default)]
pub struct FakeFeatureSource {
    features: Vec<Feature>,
}

impl FakeFeatureSource {
    /// Builds the fixed feature set.
    #[must_use]
    pub fn new() -> Self {
        let features = vec![
            Feature {
                kind: FeatureKind::UniquePrefab,
                name: "Alpha".to_string(),
                category: "Unique".to_string(),
                position: (-120, 70, -80),
                bounds: Some(FakeWorldSource::prefab_bounds((-120, 70, -80), 10)),
                metadata: FeatureMetadata {
                    variant: Some("Temple".to_string()),
                    size: None,
                    source: Some("fake".to_string()),
                },
            },
            Feature {
                kind: FeatureKind::UniquePrefab,
                name: "Beta".to_string(),
                category: "Unique".to_string(),
                position: (64, 66, 32),
                bounds: Some(FakeWorldSource::prefab_bounds((64, 66, 32), 10)),
                metadata: FeatureMetadata {
                    variant: Some("Vault".to_string()),
                    size: Some(5),
                    source: Some("fake".to_string()),
                },
            },
            Feature {
                kind: FeatureKind::UniquePrefab,
                name: "Gamma".to_string(),
                category: "Unique".to_string(),
                position: (-300, 80, 410),
                bounds: Some(FakeWorldSource::prefab_bounds((-300, 80, 410), 10)),
                metadata: FeatureMetadata {
                    variant: Some("Spire".to_string()),
                    size: None,
                    source: Some("fake".to_string()),
                },
            },
            Feature {
                kind: FeatureKind::CaveEntrance,
                name: "Delve".to_string(),
                category: "Caves".to_string(),
                position: (12, 60, -18),
                bounds: Some(FakeWorldSource::prefab_bounds((12, 60, -18), 6)),
                metadata: FeatureMetadata {
                    variant: None,
                    size: None,
                    source: Some("fake".to_string()),
                },
            },
            Feature {
                kind: FeatureKind::Spawn,
                name: "Spawn".to_string(),
                category: "Spawn".to_string(),
                position: (0, 64, 0),
                bounds: None,
                metadata: FeatureMetadata {
                    variant: None,
                    size: None,
                    source: Some("fake".to_string()),
                },
            },
        ];
        Self { features }
    }

    /// The unfiltered feature count (handy for UI tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.features.len()
    }

    /// Whether the fake has no features (never true after [`Self::new`]).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.features.is_empty()
    }
}

impl FeatureSource for FakeFeatureSource {
    fn features_in(&self, rect: Rect, filter: &FeatureFilter) -> SourceResult<Vec<Feature>> {
        Ok(self
            .features
            .iter()
            .filter(|feature| {
                rect.contains(feature.position.0, feature.position.2) && filter.matches(feature)
            })
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::{FakeFeatureSource, FakeWorldSource};
    use crate::source::{FeatureSource, WorldSource};
    use crate::types::{
        BiomeId, FeatureFilter, FeatureKind, FeatureKindMask, FluidId, Rect, ZoneId,
    };

    fn all_features() -> FeatureFilter {
        FeatureFilter {
            kinds: FeatureKindMask::ALL,
            categories: None,
            name_contains: None,
            min_size: None,
        }
    }

    fn world_rect() -> Rect {
        Rect::new(-1000, -1000, 1000, 1000)
    }

    #[test]
    fn biome_checkerboard_repeats_every_64_blocks() {
        let source = FakeWorldSource::new("1");
        let mut ids = std::collections::HashSet::new();
        for z in 0..64 {
            for x in 0..64 {
                let id = source.biome_at(x, z).expect("fake biome never fails");
                ids.insert(id);
                assert_eq!(
                    id,
                    source
                        .biome_at(x + 64, z - 64)
                        .expect("fake biome never fails"),
                    "checkerboard must repeat every 64 blocks at ({x}, {z})"
                );
            }
        }
        assert_eq!(ids.len(), 64, "the fake exposes 64 distinct biomes");
        assert_eq!(
            source.biome_at(0, 0).expect("fake biome never fails"),
            BiomeId(0)
        );
        assert_eq!(
            source.biome_at(8, 0).expect("fake biome never fails"),
            BiomeId(1)
        );
        assert_eq!(
            source.biome_at(0, 8).expect("fake biome never fails"),
            BiomeId(8)
        );
    }

    #[test]
    fn height_gradient_and_lakes_are_consistent() {
        let source = FakeWorldSource::new("2");
        // (0, 0) sums to 0, the lowest gradient step: 64, i.e. under the lake line.
        assert_eq!(source.height_at(0, 0).expect("fake height"), 64);
        let wet = source.surface_at(0, 0).expect("fake surface");
        assert_eq!(wet.fluid, Some(FluidId(1)));
        assert_eq!(wet.fluid_depth, 2);
        assert_eq!(wet.block.0, wet.biome.0 + 1);

        // (-8, -8) sums to -16: height 64 + (-2).rem_euclid(16) = 78.
        assert_eq!(source.height_at(-8, -8).expect("fake height"), 78);
        let dry = source.surface_at(-8, -8).expect("fake surface");
        assert_eq!(dry.fluid, None);
        assert_eq!(dry.fluid_depth, 0);
    }

    #[test]
    fn zones_and_spawns_are_fixed() {
        let source = FakeWorldSource::new("3");
        assert_eq!(
            source.zone_at(123, -456).expect("fake zone"),
            Some(ZoneId(0))
        );
        let spawns = source.spawn_points().expect("fake spawns");
        assert_eq!(spawns.len(), 1);
        assert_eq!(spawns[0].x, 0.5);
        assert_eq!(spawns[0].y, 64.0);
        assert_eq!(spawns[0].z, 0.5);
    }

    #[test]
    fn region_uses_negative_origin_and_two_block_steps() {
        let source = FakeWorldSource::new("-40");
        // 14×10 blocks at step 2 becomes 7×5 samples.
        let rect = Rect::from_dims(-10, -7, 14, 10);
        let grid = source.biome_region(rect, 2).expect("fake biome region");
        assert_eq!((grid.width, grid.height, grid.step), (7, 5, 2));
        assert_eq!((grid.origin_x, grid.origin_z), (-10, -7));

        // First sample: (-10, -7); last: (2, 1).
        let first = grid.sample(-10, -7).expect("first sample exists");
        assert_eq!(*first, BiomeId(6 + 8 * 7));
        let last = grid.sample(2, 1).expect("last sample exists");
        assert_eq!(*last, BiomeId(0));
        assert!(grid.sample(4, 1).is_none());
    }

    #[test]
    fn feature_min_size_prefers_metadata_then_falls_back_to_bounds() {
        let features = FakeFeatureSource::new();

        // Beta reports metadata.size 5 even though its bounds are 10 wide; the
        // metadata path wins, so it is dropped at min_size 7. Alpha and Gamma
        // have no reported size and pass through their 10-block bounds.
        let filtered = features
            .features_in(
                world_rect(),
                &FeatureFilter {
                    min_size: Some(7),
                    ..all_features()
                },
            )
            .expect("fake features");
        let mut names: Vec<&str> = filtered.iter().map(|f| f.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["Alpha", "Gamma"]);

        // At the bounds size itself, unbounded-size features still pass...
        let at_bounds_size = features
            .features_in(
                world_rect(),
                &FeatureFilter {
                    min_size: Some(10),
                    ..all_features()
                },
            )
            .expect("fake features");
        assert_eq!(at_bounds_size.len(), 2);
        // ...but nothing has 11 blocks, so the bounds path drops them.
        let too_large = features
            .features_in(
                world_rect(),
                &FeatureFilter {
                    min_size: Some(11),
                    ..all_features()
                },
            )
            .expect("fake features");
        assert!(too_large.is_empty());
    }

    #[test]
    fn fake_feature_source_reports_its_size() {
        let features = FakeFeatureSource::new();
        assert_eq!(features.len(), 5);
        assert!(!features.is_empty());
        assert_eq!(FakeFeatureSource::default().len(), 0);
    }

    #[test]
    fn feature_filters_cover_kind_category_name_and_rect() {
        let features = FakeFeatureSource::new();
        assert_eq!(features.len(), 5);

        let all = features
            .features_in(world_rect(), &all_features())
            .expect("fake features");
        assert_eq!(all.len(), 5);

        let by_kind = features
            .features_in(
                world_rect(),
                &FeatureFilter {
                    kinds: FeatureKindMask::from_kind(FeatureKind::CaveEntrance),
                    ..all_features()
                },
            )
            .expect("fake features");
        assert_eq!(by_kind.len(), 1);
        assert_eq!(by_kind[0].name, "Delve");

        let by_category = features
            .features_in(
                world_rect(),
                &FeatureFilter {
                    categories: Some(vec!["Caves".to_string()]),
                    ..all_features()
                },
            )
            .expect("fake features");
        assert_eq!(by_category.len(), 1);
        assert_eq!(by_category[0].name, "Delve");

        let by_name = features
            .features_in(
                world_rect(),
                &FeatureFilter {
                    name_contains: Some("aLp".to_string()),
                    ..all_features()
                },
            )
            .expect("fake features");
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].name, "Alpha");

        // A rect around Alpha excludes Beta (64, 32) and Gamma (-300, 410).
        let small_rect = Rect::new(-200, -200, -1, -1);
        let in_rect = features
            .features_in(small_rect, &all_features())
            .expect("fake features");
        assert_eq!(in_rect.len(), 1);
        assert_eq!(in_rect[0].name, "Alpha");
    }
}
