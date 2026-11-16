# T07 — Core domain types and source traits

**Status:** proposed
**Milestone:** 1 — Scaffold
**Depends on:** T06
**Blocks:** T08, T09, T10, T12, T14
**Plan refs:** [§2](../hytale-seed-map-plan.md#2-what-the-explorer-needs-from-your-worldgen), [§3](../hytale-seed-map-plan.md#3-architecture), [§5.1](../hytale-seed-map-plan.md#51-tiles-and-zoom)
**Agent fit:** high

## Goal

Define the stable vocabulary that every other map-core task uses: seeds,
coordinates, grids, feature records, source traits and errors. This is the
contract between the worldgen adapter, renderers, search and the app.

## Context

The plan proposes:

```rust
pub trait WorldSource: Send + Sync {
    fn biome_at(&self, x: i32, z: i32) -> BiomeId;
    fn surface_region(&self, x0: i32, z0: i32, w: u32, h: u32, step: u32) -> SurfaceGrid;
    fn slice_region(&self, x0: i32, z0: i32, w: u32, h: u32, step: u32, y: i32) -> SliceGrid;
    fn seed(&self) -> Seed;
}

pub trait FeatureSource: Send + Sync {
    fn features_in(&self, rect: Rect, kinds: &FeatureFilter) -> Vec<Feature>;
}
```

Refinements needed before implementation:

- Errors: loading a world or generating a chunk can fail. Return
  `Result<_, SourceError>` rather than panicking in the UI.
- Batch queries: per-pixel `biome_at` would cross a worker channel 65,536 times
  per tile. Add region methods and keep `biome_at` for search/hover.
- IDs: `BiomeId` should be the generator's `i32` biome id; a catalog maps it to
  name and map color. Block/fluid ids are the registry ids; a catalog maps them
  to names for the inspector.
- The source must expose the generator version/identity for tile keys and
  exports.

## Scope

### In

- New modules in `map-core`: `types.rs`, `source.rs`, `error.rs`, `catalog.rs`.
- The types and traits below.
- A fake/in-memory source for tests and for UI development without a world.
- Serde derives for types that appear in share strings/settings.
- Unit tests for rect iteration, grid indexing and filter behavior.

### Out

- The worldgen adapter (T08/T09).
- Tile keys (T14), renderers (T13/T17), search algorithms (T24/T25).
- Feature placement semantics (T22).

## Proposed types

```rust
pub struct Seed { /* input, world i64, generator i32 */ }

pub struct BiomeId(pub i32);
pub struct BlockId(pub i32);
pub struct FluidId(pub i32);
pub struct ZoneId(pub i32);

pub struct BiomeInfo {
    pub id: BiomeId,
    pub name: String,
    pub map_color: u32, // RGBA or packed RGB, decide once
}

pub struct BiomeCatalog { /* id -> BiomeInfo, name -> id */ }
pub struct BlockCatalog { /* id -> name, name -> id */ }
pub struct FluidCatalog { /* id -> name */ }
pub struct ZoneCatalog { /* id -> name, color */ }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub min_x: i32,
    pub min_z: i32,
    pub max_x: i32, // inclusive
    pub max_z: i32,
}

pub struct Rect3 { pub min_x: i32, pub min_y: i32, pub min_z: i32,
                   pub max_x: i32, pub max_y: i32, pub max_z: i32 }

pub struct SurfaceSample {
    pub x: i32,
    pub z: i32,
    pub biome: BiomeId,
    pub height: i32,
    pub block: BlockId,
    pub fluid: Option<FluidId>,
    pub fluid_depth: u16,
    pub tint: i32,
}

pub struct SurfaceGrid {
    pub origin_x: i32,
    pub origin_z: i32,
    pub step: u32,
    pub width: u32,
    pub height: u32,
    pub samples: Vec<SurfaceSample>, // row-major, len = width * height
}

pub struct BiomeGrid {
    pub origin_x: i32,
    pub origin_z: i32,
    pub step: u32,
    pub width: u32,
    pub height: u32,
    pub biomes: Vec<BiomeId>,
}

pub struct HeightGrid {
    pub origin_x: i32,
    pub origin_z: i32,
    pub step: u32,
    pub width: u32,
    pub height: u32,
    pub heights: Vec<i32>,
}

pub struct SliceGrid {
    pub origin_x: i32,
    pub origin_z: i32,
    pub y: i32,
    pub step: u32,
    pub width: u32,
    pub height: u32,
    pub cells: Vec<SliceCell>, // solid/air/fluid + block id
}

pub struct SpawnPoint {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f32,
}

pub enum SliceCell {
    Solid(BlockId),
    Fluid(FluidId),
    Air,
    Unknown,
}

pub struct Feature {
    pub kind: FeatureKind,
    pub name: String,
    pub category: String,
    pub position: (i32, i32, i32),
    pub bounds: Option<Rect3>,
    pub metadata: FeatureMetadata,
}

pub enum FeatureKind {
    UniquePrefab,
    BiomePrefab,
    CaveEntrance,
    Spawn,
    Poi,
}

pub struct FeatureFilter {
    pub kinds: FeatureKindMask,
    pub categories: Option<Vec<String>>,
    pub name_contains: Option<String>,
    pub min_size: Option<i32>,
}

pub struct FeatureKindMask(pub u16);
pub struct FeatureMetadata {
    pub variant: Option<String>,
    pub size: Option<i32>,
    pub source: Option<String>,
}

pub struct GeneratorIdentity {
    pub generator_version: u32,
    pub world_assets: String,
    pub palette_version: u32,
}
```

Trait shape:

```rust
pub trait WorldSource: Send + Sync {
    fn identity(&self) -> &GeneratorIdentity;
    fn seed(&self) -> &Seed;

    fn biome_at(&self, x: i32, z: i32) -> Result<BiomeId, SourceError>;
    fn biome_region(&self, rect: Rect, step: u32) -> Result<BiomeGrid, SourceError>;

    fn height_at(&self, x: i32, z: i32) -> Result<i32, SourceError>;
    fn height_region(&self, rect: Rect, step: u32) -> Result<HeightGrid, SourceError>;

    fn surface_at(&self, x: i32, z: i32) -> Result<SurfaceSample, SourceError>;
    fn surface_region(&self, rect: Rect, step: u32) -> Result<SurfaceGrid, SourceError>;

    fn slice_region(&self, rect: Rect, step: u32, y: i32) -> Result<SliceGrid, SourceError>;

    fn spawn_points(&self) -> Result<Vec<SpawnPoint>, SourceError>;
    fn zone_at(&self, x: i32, z: i32) -> Result<Option<ZoneId>, SourceError>;

    fn biome_catalog(&self) -> &BiomeCatalog;
    fn block_catalog(&self) -> &BlockCatalog;
    fn fluid_catalog(&self) -> &FluidCatalog;
    fn zone_catalog(&self) -> &ZoneCatalog;
}

pub trait FeatureSource: Send + Sync {
    fn features_in(&self, rect: Rect, filter: &FeatureFilter)
        -> Result<Vec<Feature>, SourceError>;
}
```

`BiomeGrid` is a compact matrix of `BiomeId` plus the same origin/step/dims
metadata as `SurfaceGrid`; add it to `types.rs` rather than making biome tiles
allocate a full `SurfaceSample` per pixel.

## Work

1. Create the modules and types. Keep field names explicit; avoid tuples except
   small positions.
2. Implement `Rect`:
   - `new`, `width`, `height`, `is_empty`, `contains`, `intersects`,
     `expand`, `clamp`;
   - inclusive max bounds; document that the width is `max - min + 1`;
   - `iter_blocks(step)` for grids, with a test for negative origins.
3. Implement grid indexing:
   - row-major index = `z * width + x`;
   - `sample(x, z)` and `sample_mut` with bounds checks;
   - a debug assertion that `samples.len() == width * height`.
4. Implement `GeneratorIdentity` and `Seed` per T02. If T02 has not landed, put
   a placeholder with a `TODO(T02)` and do not invent hashing.
5. Implement the catalogs with stable lookup by id and name. Decide whether
   map colors are stored as `u32` packed RGBA or `[u8; 4]`; use one type
   everywhere. Map colors from `Biome::get_map_color()` need a documented
   channel order; the worldgen `map_image` packer is a useful reference.
6. Implement `SourceError` with variants for load, generation, out-of-bounds,
   cancelled and internal. Use `thiserror`.
7. Add `FakeWorldSource` and `FakeFeatureSource` under `#[cfg(test)]` or a
   `test-support` feature. The fake should generate deterministic gradients and
   a few features so renderers and UI can be developed without assets.
8. Add unit tests:
   - rect containment/intersection for negative and touching rects;
   - grid row-major indexing at the edges;
   - catalog round-trip;
   - filter matching (kinds, category, name, min size);
   - `Seed` parse error propagation (once T02 lands).

## Acceptance

```sh
cargo test -p map-core
cargo clippy -p map-core --all-targets -- -D warnings
cargo fmt --all -- --check
```

- All types compile and are documented.
- No dependency on worldgen types leaks into the public map-core API except
  where the adapter module explicitly re-exports/wraps them.
- The fake source implements both traits and can render a biome grid.
- Grid indexing is tested for negative origins and non-square regions.

## Tests

- Unit tests in `types.rs`, `source.rs`, `catalog.rs`.
- A compile-time test that `dyn WorldSource` and `dyn FeatureSource` are
  object-safe and `Send + Sync`.
- A small integration test that asks the fake source for a 7×5 region at step 2
  and checks the first/last sample coordinates.

## Risks / notes

- Do not over-generalize the traits for the seed-finder mode. Keep
  `FeatureSource` cheap and batch-oriented; T22 decides the final feature query
  signature.
- If `BiomeId` is not globally unique across zones/custom biomes, use a
  catalog-assigned id instead of the generator id. Verify in T01/T08 before
  freezing the public type.
- Avoid `Arc<dyn Trait>` inside the hot sample types; use plain ids and
  catalogs.
