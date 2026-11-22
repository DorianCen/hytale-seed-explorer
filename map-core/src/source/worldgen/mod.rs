//! The production `WorldSource` adapter over the external worldgen
//! workspace (T08).
//!
//! Cheap queries only: biome, height, zone and spawn-point data served from
//! a pool of generator worker threads (see [`pool`]). Chunk-dependent
//! queries — surface blocks, fluid depths, slices — return
//! [`SourceError::Unsupported`] until T09 adds chunk summaries.
//!
//! Feature placement ([`crate::source::FeatureSource`], T22/T23) also runs
//! here: a rect query is sent to a worker, the worker calls the generator's
//! public `ChunkGenerator::features_in` and translates every placement into
//! a plain map-core [`Feature`] before it crosses the thread boundary
//! (placements hold `Rc`s and cannot be sent). The worldgen query is the
//! single filter authority; the adapter only maps the kind mask.
//!
//! This module is the only place where worldgen types cross into map-core
//! (the T07 acceptance rule): the public API exposes plain map-core ids and
//! grids, translated here.

mod pool;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use worldgen_hytale::chunk::chunk_generator::ChunkGenerator;
use worldgen_hytale::chunk::chunk_generator_resource::ChunkGeneratorResource;
use worldgen_hytale::loader::chunk_generator_json_loader::ChunkGeneratorJsonLoader;
use worldgen_hytale::query::{
    BlockRect, FeaturePlacement, PlacementFilter, PlacementKind, PlacementKindMask,
};
use worldgen_hytale::world_gen::{WorldGenError, WorldGenRequest, generate_chunk};
use worldgen_hytale::world_gen_config::WorldGenConfig;
use worldgen_hytale::world_gen_provider::{HytaleWorldGenProvider, Semver};

use crate::catalog::{BiomeCatalog, BiomeInfo, BlockCatalog, FluidCatalog, ZoneCatalog, ZoneInfo};
use crate::error::{SourceError, SourceResult, panic_message};
use crate::source::chunks::{
    self, ChunkKey, ChunkSlice, ChunkSummary, DEFAULT_SLICE_CACHE_ENTRIES,
    DEFAULT_SUMMARY_BUDGET_BYTES, SliceCache, SliceKey, SummaryCache, SummaryColumn, chunk_of,
    column_index, local_of,
};
use crate::source::{FeatureSource, WorldSource};
use crate::types::{
    BiomeGrid, BiomeId, BlockId, Feature, FeatureFilter, FeatureKind, FeatureKindMask,
    FeatureMetadata, FluidId, GeneratorIdentity, HeightGrid, Rect, Rect3, Seed, SliceGrid,
    SpawnPoint, SurfaceGrid, SurfaceSample, ZoneGrid, ZoneId,
};

pub(crate) use pool::LoadSpec;
use pool::{CancelToken, GeneratorPool, GridOut, PoolReady, cancel_token};

/// Workers started by default. T04's interpretation section says start
/// small: each worker costs its own generator load (~0.9 s) and its own
/// cold caches; the pool can be configured larger for tile-heavy use.
const DEFAULT_WORKERS: usize = 2;

/// Workers consult their cancellation token at every row-batch boundary of
/// this many sample rows.
const CANCEL_CHECK_ROWS: u32 = 16;

/// Everything the app needs to construct a [`WorldgenSource`] by hand; the
/// UI will keep the interesting fields and default the rest.
#[derive(Debug, Clone)]
pub struct WorldgenSourceConfig {
    /// World-gen folder, resolved with the Java `HytaleWorldGenProvider`
    /// semantics (same meaning as `worldgen-cli --world`).
    pub world_path: PathBuf,
    /// The world-gen name (default `"Default"`).
    pub world_name: String,
    /// The world-gen version string (only carried into the identity).
    pub world_version: String,
    /// Extracted base-game asset root, when generation needs it.
    pub assets_root: Option<PathBuf>,
    /// A registry snapshot (`registries.json`) path override.
    pub registries_path: Option<PathBuf>,
    /// Worker pool size; `None` = [`DEFAULT_WORKERS`]. Must be >= 1.
    pub workers: Option<usize>,
    /// Byte budget for the chunk-summary cache; `None` = 256 MiB.
    pub summary_budget_bytes: Option<u64>,
    /// How many `(chunk, y)` slices to keep; `None` = 64.
    pub slice_cache_entries: Option<u64>,
    /// The seed exactly as the user typed it (T02 semantics).
    pub seed_input: String,
}

impl WorldgenSourceConfig {
    /// A config with Java-compatible defaults, mirroring `worldgen-cli`.
    pub fn new(world_path: impl Into<PathBuf>, seed_input: impl Into<String>) -> Self {
        Self {
            world_path: world_path.into(),
            world_name: "Default".to_string(),
            world_version: "0.0.0".to_string(),
            assets_root: None,
            registries_path: None,
            workers: None,
            summary_budget_bytes: None,
            slice_cache_entries: None,
            seed_input: seed_input.into(),
        }
    }
}

/// The default world-gen folder for the explorer's frontends (CLI `render`,
/// the egui shell): the extracted pack when the user has one (cwd-relative,
/// like `worldgen-cli`'s probe), else the golden world copy of the sibling
/// worldgen checkout (compile-time manifest-relative, so it works from any
/// working directory). Frontends should still offer an override.
pub fn default_world_path() -> PathBuf {
    let extracted = PathBuf::from("Assets/extracted/Server/World/Default");
    if extracted.is_dir() {
        return extracted;
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hytale-server-rust/testdata/Default")
}

/// The catalogs one loaded generator yields; owned by the main thread after
/// startup (plain data, extracted on worker 0).
pub struct SourceCatalogs {
    pub biomes: BiomeCatalog,
    pub blocks: BlockCatalog,
    pub fluids: FluidCatalog,
    pub zones: ZoneCatalog,
}

/// The production [`WorldSource`]: cheap queries through the worker pool.
pub struct WorldgenSource {
    seed: Seed,
    identity: GeneratorIdentity,
    catalogs: SourceCatalogs,
    pool: GeneratorPool,
    min_chunk: i32,
    max_chunk: i32,
    /// Best-effort cancellation for in-flight region requests, shared by
    /// every query of this source. Cleared at the start of each region
    /// request; set by [`WorldgenSource::cancel_pending`]. One token for
    /// the whole source is a T08 compromise: T15's scheduler will carry
    /// per-request tokens through the same pool plumbing.
    cancel: CancelToken,
    /// Compact per-chunk surface data (T09), byte-weighed.
    summary_cache: SummaryCache,
    /// The last few `(chunk, y)` horizontal slices (T09).
    slice_cache: SliceCache,
}

impl WorldgenSource {
    /// Resolves the seed and world path (fail-fast on the main thread),
    /// then starts the worker pool and waits for every worker's generator
    /// to load. The first failure — bad seed, bad path, generator load
    /// panic — returns [`SourceError::Load`] with no leaked threads.
    pub fn new(config: &WorldgenSourceConfig) -> SourceResult<Self> {
        let seed = Seed::parse(&config.seed_input).map_err(|error| {
            SourceError::Load(format!("invalid seed {:?}: {error}", config.seed_input))
        })?;
        let provider = HytaleWorldGenProvider {
            name: config.world_name.clone(),
            version: Semver::parse(&config.world_version).map_err(|error| {
                SourceError::Load(format!(
                    "the world version {:?} is not valid semver: {}",
                    config.world_version, error.message
                ))
            })?,
            path: None,
        };
        let world_path = provider.resolve_path(&config.world_path).map_err(|error| {
            SourceError::Load(format!(
                "could not resolve the world folder {}: {}",
                config.world_path.display(),
                error.message
            ))
        })?;
        let workers = config.workers.unwrap_or(DEFAULT_WORKERS);
        if workers == 0 {
            return Err(SourceError::Load(
                "the generator pool needs at least one worker".to_string(),
            ));
        }
        let spec = LoadSpec {
            world_path,
            world_name: config.world_name.clone(),
            world_version: config.world_version.clone(),
            assets_root: config.assets_root.clone(),
            registries_path: config.registries_path.clone(),
            generator_seed: seed.generator(),
        };
        let (pool, ready) = GeneratorPool::start(&spec, workers)?;
        let PoolReady {
            catalogs,
            min_chunk,
            max_chunk,
        } = ready;
        let identity = GeneratorIdentity {
            generator_version: crate::GENERATOR_VERSION,
            world_assets: format!(
                "{}@{} ({})",
                config.world_name,
                config.world_version,
                config.world_path.display()
            ),
            // Bumped when the committed fallback palette changes (T03/T20).
            palette_version: crate::palette::PALETTE_VERSION,
        };
        Ok(Self {
            seed,
            identity,
            catalogs,
            pool,
            min_chunk,
            max_chunk,
            cancel: cancel_token(),
            summary_cache: chunks::summary_cache(
                config
                    .summary_budget_bytes
                    .unwrap_or(DEFAULT_SUMMARY_BUDGET_BYTES),
            ),
            slice_cache: chunks::slice_cache(
                config
                    .slice_cache_entries
                    .unwrap_or(DEFAULT_SLICE_CACHE_ENTRIES),
            ),
        })
    }

    /// The generator's chunk bounds (T09 uses them for out-of-bounds
    /// handling of chunk-dependent queries).
    pub fn chunk_bounds(&self) -> (i32, i32) {
        (self.min_chunk, self.max_chunk)
    }

    /// The pool size this source serves from.
    pub fn worker_count(&self) -> usize {
        self.pool.worker_count()
    }

    /// Requests that in-flight region queries stop at their next row-batch
    /// boundary and return [`SourceError::Cancelled`]. Best-effort: a
    /// region that already passed its last boundary completes normally.
    pub fn cancel_pending(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Arms a fresh region request: clears the shared token, then hands a
    /// clone to the pool.
    fn fresh_cancel(&self) -> CancelToken {
        self.cancel.store(false, Ordering::Relaxed);
        Arc::clone(&self.cancel)
    }

    /// The chunks a rect covers, bounds-checked, ordered nearest to the
    /// rect's center first (the visible middle of a tile fills in first).
    /// A rect reaching outside the generatable area is a typed error for
    /// the whole request — documented: the UI clamps its requests (T16).
    fn ordered_covering_chunks(&self, rect: Rect) -> SourceResult<Vec<(i32, i32)>> {
        let min_cx = chunk_of(rect.min_x);
        let max_cx = chunk_of(rect.max_x);
        let min_cz = chunk_of(rect.min_z);
        let max_cz = chunk_of(rect.max_z);
        let mut chunks =
            Vec::with_capacity(((max_cx - min_cx + 1) * (max_cz - min_cz + 1)) as usize);
        for cz in min_cz..=max_cz {
            for cx in min_cx..=max_cx {
                if cx < self.min_chunk
                    || cx > self.max_chunk
                    || cz < self.min_chunk
                    || cz > self.max_chunk
                {
                    return Err(SourceError::OutOfBounds {
                        x: rect.min_x,
                        z: rect.min_z,
                    });
                }
                chunks.push((cx, cz));
            }
        }
        let center_cx = chunk_of((rect.min_x + rect.max_x) / 2);
        let center_cz = chunk_of((rect.min_z + rect.max_z) / 2);
        chunks.sort_by_key(|(cx, cz)| (cx - center_cx).abs() + (cz - center_cz).abs());
        Ok(chunks)
    }

    /// The summary for one chunk: bounds-checked, cached, deduplicated
    /// across concurrent callers by the cache's in-flight coalescing
    /// (`try_get_with`), never caching failures.
    fn summary_for(&self, cx: i32, cz: i32) -> SourceResult<Arc<ChunkSummary>> {
        if cx < self.min_chunk || cx > self.max_chunk || cz < self.min_chunk || cz > self.max_chunk
        {
            // The chunk's origin is the representative coordinate.
            return Err(SourceError::OutOfBounds {
                x: cx << 5,
                z: cz << 5,
            });
        }
        let key = ChunkKey {
            identity: self.identity.stable_hash(),
            generator_seed: self.seed.generator(),
            cx,
            cz,
        };
        self.summary_cache
            .try_get_with(key, || self.pool.generate_summary(cx, cz))
            .map_err(|error: Arc<SourceError>| (*error).clone())
    }

    /// The `(chunk, y)` slice for one chunk: cached in the small last-few
    /// cache, deduplicated like summaries. A slice miss pays one chunk
    /// generation, and the same chunk also warms the summary cache (both
    /// products are extracted from the single generation).
    fn slice_for(&self, cx: i32, cz: i32, y: i32) -> SourceResult<ChunkSlice> {
        let key = SliceKey {
            chunk: ChunkKey {
                identity: self.identity.stable_hash(),
                generator_seed: self.seed.generator(),
                cx,
                cz,
            },
            y,
        };
        self.slice_cache
            .try_get_with(key, || {
                let (slice, summary) = self.pool.generate_slice(cx, cz, y)?;
                // The generation was paid; keep both products.
                self.summary_cache.insert(
                    ChunkKey {
                        identity: self.identity.stable_hash(),
                        generator_seed: self.seed.generator(),
                        cx,
                        cz,
                    },
                    summary,
                );
                Ok(slice)
            })
            .map_err(|error: Arc<SourceError>| (*error).clone())
    }

    /// Combines one summary column with its biome into the public sample.
    fn surface_sample(
        &self,
        x: i32,
        z: i32,
        biome: BiomeId,
        column: &SummaryColumn,
    ) -> SurfaceSample {
        SurfaceSample {
            x,
            z,
            biome,
            height: i32::from(column.height),
            block: column.block,
            fluid: (column.fluid.0 != 0).then_some(column.fluid),
            fluid_depth: column.fluid_depth,
            tint: column.tint,
        }
    }

    /// The shared body of [`WorldSource::surface_region`] and its
    /// cancellable variant: `external` is the caller's per-tile token
    /// (T21), `None` for the plain region query.
    fn surface_region_inner(
        &self,
        rect: Rect,
        step: u32,
        external: Option<&CancelToken>,
    ) -> SourceResult<SurfaceGrid> {
        let (width, height) = rect.sample_dims(step);
        let cancel = self.fresh_cancel();

        // 1) The chunks this rect needs; a rect reaching outside the
        //    generatable area is a typed error for the whole request
        //    (documented: the UI clamps its requests, T16).
        let ordered = self.ordered_covering_chunks(rect)?;

        // 2) Summaries, nearest to the rect's center first, one pool
        //    command per missing chunk, cancelled between chunks. This
        //    loop is the expensive path (a chunk generation per miss), so
        //    both tokens are consulted here: the source's own shared one
        //    (re-armed above) and the caller's per-tile one.
        let mut summaries: HashMap<(i32, i32), Arc<ChunkSummary>> = HashMap::new();
        for (cx, cz) in ordered {
            let cancelled = cancel.load(Ordering::Relaxed)
                || external.is_some_and(|token| token.load(Ordering::Relaxed));
            if cancelled {
                return Err(SourceError::Cancelled);
            }
            let summary = self.summary_for(cx, cz)?;
            summaries.insert((cx, cz), summary);
        }

        // 3) The biome of every sample in one cheap batch. It keeps the
        //    shared token only: the band walk has its own per-row checks
        //    and costs a fraction of a chunk, so a caller cancellation is
        //    honoured where it pays — the per-chunk gate above. Re-arming
        //    here would also lose a shared cancel that landed mid-loop.
        let biomes = self.pool.biome_region(rect, step, Arc::clone(&cancel))?;

        // 4) Assemble row-major.
        let mut samples = Vec::with_capacity((width * height) as usize);
        for z in 0..height {
            let world_z = rect.min_z + z as i32 * step as i32;
            for x in 0..width {
                let world_x = rect.min_x + x as i32 * step as i32;
                let index = (z * width + x) as usize;
                let biome = biomes.values[index];
                let summary = &summaries[&(chunk_of(world_x), chunk_of(world_z))];
                let column = summary
                    .column(local_of(world_x), local_of(world_z))
                    .expect("local coordinates are in range by construction");
                samples.push(self.surface_sample(world_x, world_z, biome, &column));
            }
        }
        Ok(SurfaceGrid::new(
            rect.min_x, rect.min_z, step, width, height, samples,
        ))
    }
}

impl WorldSource for WorldgenSource {
    fn identity(&self) -> &GeneratorIdentity {
        &self.identity
    }

    fn seed(&self) -> &Seed {
        &self.seed
    }

    fn biome_at(&self, x: i32, z: i32) -> SourceResult<BiomeId> {
        let grid = self.biome_region(Rect::new(x, z, x, z), 1)?;
        Ok(grid.biomes[0])
    }

    fn biome_region(&self, rect: Rect, step: u32) -> SourceResult<BiomeGrid> {
        let out = self.pool.biome_region(rect, step, self.fresh_cancel())?;
        Ok(BiomeGrid::new(
            out.origin_x,
            out.origin_z,
            out.step,
            out.width,
            out.height,
            out.values,
        ))
    }

    fn height_at(&self, x: i32, z: i32) -> SourceResult<i32> {
        let grid = self.height_region(Rect::new(x, z, x, z), 1)?;
        Ok(grid.heights[0])
    }

    fn height_region(&self, rect: Rect, step: u32) -> SourceResult<HeightGrid> {
        let out = self.pool.height_region(rect, step, self.fresh_cancel())?;
        Ok(HeightGrid::new(
            out.origin_x,
            out.origin_z,
            out.step,
            out.width,
            out.height,
            out.values,
        ))
    }

    fn surface_at(&self, x: i32, z: i32) -> SourceResult<SurfaceSample> {
        let (cx, cz) = (chunk_of(x), chunk_of(z));
        let summary = self.summary_for(cx, cz)?;
        let column = summary
            .column(local_of(x), local_of(z))
            .expect("local coordinates are in range by construction");
        let biome = self.biome_at(x, z)?;
        Ok(self.surface_sample(x, z, biome, &column))
    }

    fn surface_region(&self, rect: Rect, step: u32) -> SourceResult<SurfaceGrid> {
        self.surface_region_inner(rect, step, None)
    }

    fn surface_region_cancelled(
        &self,
        rect: Rect,
        step: u32,
        cancel: &CancelToken,
    ) -> SourceResult<SurfaceGrid> {
        self.surface_region_inner(rect, step, Some(cancel))
    }

    fn slice_region(&self, rect: Rect, step: u32, y: i32) -> SourceResult<SliceGrid> {
        let (width, height) = rect.sample_dims(step);
        let cancel = self.fresh_cancel();
        let ordered = self.ordered_covering_chunks(rect)?;

        // One (chunk, y) slice per needed chunk, nearest first; each miss
        // is a chunk generation, so cancellation is checked between chunks.
        let mut slices: HashMap<(i32, i32), ChunkSlice> = HashMap::new();
        for (cx, cz) in ordered {
            if cancel.load(Ordering::Relaxed) {
                return Err(SourceError::Cancelled);
            }
            let slice = self.slice_for(cx, cz, y)?;
            slices.insert((cx, cz), slice);
        }

        let mut cells = Vec::with_capacity((width * height) as usize);
        for z in 0..height {
            let world_z = rect.min_z + z as i32 * step as i32;
            for x in 0..width {
                let world_x = rect.min_x + x as i32 * step as i32;
                let slice = &slices[&(chunk_of(world_x), chunk_of(world_z))];
                // A sample between slice pixels takes the pixel at the
                // sample origin (nearest, never interpolated).
                let index = column_index(local_of(world_x), local_of(world_z));
                cells.push(slice[index]);
            }
        }
        Ok(SliceGrid::new(
            rect.min_x, rect.min_z, step, width, height, y, cells,
        ))
    }

    fn spawn_points(&self) -> SourceResult<Vec<SpawnPoint>> {
        self.pool.spawn_points()
    }

    fn zone_at(&self, x: i32, z: i32) -> SourceResult<Option<ZoneId>> {
        self.pool.zone_at(x, z)
    }

    fn zone_region(&self, rect: Rect, step: u32) -> SourceResult<ZoneGrid> {
        let out = self.pool.zone_region(rect, step, self.fresh_cancel())?;
        Ok(ZoneGrid::new(
            out.origin_x,
            out.origin_z,
            out.step,
            out.width,
            out.height,
            out.values,
        ))
    }

    fn biome_catalog(&self) -> &BiomeCatalog {
        &self.catalogs.biomes
    }

    fn block_catalog(&self) -> &BlockCatalog {
        &self.catalogs.blocks
    }

    fn fluid_catalog(&self) -> &FluidCatalog {
        &self.catalogs.fluids
    }

    fn zone_catalog(&self) -> &ZoneCatalog {
        &self.catalogs.zones
    }
}

impl FeatureSource for WorldgenSource {
    /// Sends one feature query to a generator worker and **blocks** until the
    /// reply arrives.
    ///
    /// The caller must be a background thread: the overlay fetches from a
    /// worker so panning and zooming stay responsive (the T23 acceptance
    /// bullet), and the UI thread must never call this. The worldgen query is
    /// expensive for large rects (see `docs/benchmarks.md`), so callers use a
    /// margin-expanded, cached rect rather than the raw viewport.
    ///
    /// A closed command or reply channel maps to [`SourceError::Internal`],
    /// exactly like the region path.
    fn features_in(&self, rect: Rect, filter: &FeatureFilter) -> SourceResult<Vec<Feature>> {
        // The filter is cloned onto the command: `FeatureFilter` is plain data
        // and the worker owns it for the duration of the query.
        self.pool.features_in(rect, filter.clone())
    }
}

// ---------------------------------------------------------------------------
// Worker-side functions: these run ON a worker thread with that thread's
// own `Rc<ChunkGenerator>`. Nothing here may be called from the main
// thread with a foreign generator.
// ---------------------------------------------------------------------------

/// Loads a worker's own generator, mirroring the proven
/// `WorldGenService::worker_main` loader chain (world_gen.rs:594-623).
///
/// **Style deviation, documented:** the repo guide bans `catch_unwind`, but
/// the generator's loader communicates failure *only by panicking*
/// (`ChunkGeneratorJsonLoader::load`, chunk_generator_json_loader.rs:161),
/// and the worldgen service uses exactly this panic-to-error boundary
/// (world_gen.rs:624-636). Without it a bad world would hang pool startup
/// instead of returning [`SourceError::Load`].
pub(crate) fn load_generator(spec: &LoadSpec) -> SourceResult<Rc<ChunkGenerator>> {
    catch_load_panics(|| {
        let config = WorldGenConfig::new(
            spec.world_path.clone(),
            spec.world_name.clone(),
            spec.world_version.clone(),
        );
        let mut loader = ChunkGeneratorJsonLoader::new(
            procedural::json::SeedString::new("ChunkGenerator"),
            config,
        );
        if let Some(assets_root) = &spec.assets_root {
            loader = loader.with_assets_root(assets_root);
        }
        if let Some(registries_path) = &spec.registries_path {
            loader = loader.with_registries_path(registries_path);
        }
        Ok(loader.load())
    })
}

/// Runs `work`, converting a panic into [`SourceError::Load`] — see the
/// deviation note on [`load_generator`].
fn catch_load_panics<T>(work: impl FnOnce() -> SourceResult<T>) -> SourceResult<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)) {
        Ok(result) => result,
        Err(payload) => Err(SourceError::Load(panic_message(payload))),
    }
}

/// Extracts the catalogs from a loaded generator (worker 0 only). The biome
/// walk is the documented T08 chain: provider → zones →
/// `biome_pattern_generator.get_biomes()` — which already contains the
/// custom biomes appended after the tile biomes, so nothing is counted
/// twice. Duplicate ids/names are [`SourceError::Internal`]: the catalog
/// contract (T07) makes the adapter, not the renderer, resolve collisions.
pub(crate) fn build_catalogs(generator: &Rc<ChunkGenerator>) -> SourceResult<SourceCatalogs> {
    // `Registries::load()` (the no-registries fallback) also panics on
    // failure; same boundary rule as the generator load.
    catch_load_panics(|| {
        let mut biomes = BiomeCatalog::default();
        let mut zones = ZoneCatalog::default();
        for zone in generator.zone_pattern_provider().get_zones() {
            zones
                .insert(ZoneInfo {
                    id: ZoneId(zone.id),
                    name: zone.name.clone(),
                })
                .map_err(|error| {
                    SourceError::Internal(format!(
                        "duplicate zone {} in the world: {error}",
                        zone.name
                    ))
                })?;
            for biome in zone.biome_pattern_generator.get_biomes() {
                biomes
                    .insert(BiomeInfo {
                        id: BiomeId(biome.get_id()),
                        name: biome.get_name().to_string(),
                        // Packed exactly as the generator parses it; the
                        // low 24 bits are RGB (see BiomeInfo::map_color_rgb).
                        map_color: biome.get_map_color() as u32,
                    })
                    .map_err(|error| {
                        SourceError::Internal(format!(
                            "duplicate biome {} in the world: {error}",
                            biome.get_name()
                        ))
                    })?;
            }
        }

        let registries = match generator.get_registries() {
            Some(registries) => Rc::clone(registries),
            None => Rc::new(worldgen_model::registry::Registries::load()),
        };
        let mut blocks = BlockCatalog::default();
        let mut fluids = FluidCatalog::default();
        for index in 0..registries.blocks.len() {
            if let Some(name) = registries.blocks.name(i64_as_index(index)) {
                blocks
                    .insert(BlockId(i64_as_index(index)), name)
                    .map_err(|error| {
                        SourceError::Internal(format!(
                            "duplicate block id {index} in the registry: {error}"
                        ))
                    })?;
            }
        }
        for index in 0..registries.fluids.len() {
            if let Some(name) = registries.fluids.name(i64_as_index(index)) {
                fluids
                    .insert(FluidId(i64_as_index(index)), name)
                    .map_err(|error| {
                        SourceError::Internal(format!(
                            "duplicate fluid id {index} in the registry: {error}"
                        ))
                    })?;
            }
        }
        Ok(SourceCatalogs {
            biomes,
            blocks,
            fluids,
            zones,
        })
    })
}

/// Registry tables are indexed by `i32` position; `len()` is a `usize`.
fn i64_as_index(index: usize) -> i32 {
    i32::try_from(index).expect("registry ids fit i32 by construction")
}

/// Serves one biome band: per-sample `get_zone_biome_result_at`, checking
/// the cancellation token between row batches.
pub(crate) fn biome_region(
    generator: &Rc<ChunkGenerator>,
    seed: i32,
    rect: Rect,
    step: u32,
    cancel: &CancelToken,
) -> SourceResult<GridOut<BiomeId>> {
    let (width, height) = rect.sample_dims(step);
    let mut ids = Vec::with_capacity((width * height) as usize);
    for z in 0..height {
        if z % CANCEL_CHECK_ROWS == 0 && cancel.load(Ordering::Relaxed) {
            return Err(SourceError::Cancelled);
        }
        let world_z = rect.min_z + z as i32 * step as i32;
        for x in 0..width {
            let world_x = rect.min_x + x as i32 * step as i32;
            let result = generator.get_zone_biome_result_at(seed, world_x, world_z);
            // The generator always sets a biome on generated results
            // (chunk_generator.rs:503-506; `None` is an ungenerated record,
            // which the workers never see) — parity with the generator's own
            // Java-NPE expects.
            let biome = result
                .get_biome()
                .expect("the generated result has no biome (Java: NPE parity)");
            ids.push(BiomeId(biome.get_id()));
        }
    }
    GridOut::new(rect, step, width, height, ids)
}

/// Serves one zone band: per-sample zone lookup from the same result as
/// the biome walk, checking the cancellation token between row batches.
/// A column where the generator reports no zone stays [`None`].
pub(crate) fn zone_region(
    generator: &Rc<ChunkGenerator>,
    seed: i32,
    rect: Rect,
    step: u32,
    cancel: &CancelToken,
) -> SourceResult<GridOut<Option<ZoneId>>> {
    let (width, height) = rect.sample_dims(step);
    let mut zones = Vec::with_capacity((width * height) as usize);
    for z in 0..height {
        if z % CANCEL_CHECK_ROWS == 0 && cancel.load(Ordering::Relaxed) {
            return Err(SourceError::Cancelled);
        }
        let world_z = rect.min_z + z as i32 * step as i32;
        for x in 0..width {
            let world_x = rect.min_x + x as i32 * step as i32;
            let result = generator.get_zone_biome_result_at(seed, world_x, world_z);
            zones.push(
                result
                    .get_zone_result()
                    .and_then(|zone_result| zone_result.get_zone())
                    .map(|zone| ZoneId(zone.id)),
            );
        }
    }
    GridOut::new(rect, step, width, height, zones)
}

/// Serves one height band: per-sample `get_height`, checking the
/// cancellation token between row batches.
pub(crate) fn height_region(
    generator: &Rc<ChunkGenerator>,
    seed: i32,
    rect: Rect,
    step: u32,
    cancel: &CancelToken,
) -> SourceResult<GridOut<i32>> {
    let (width, height) = rect.sample_dims(step);
    let mut heights = Vec::with_capacity((width * height) as usize);
    for z in 0..height {
        if z % CANCEL_CHECK_ROWS == 0 && cancel.load(Ordering::Relaxed) {
            return Err(SourceError::Cancelled);
        }
        let world_z = rect.min_z + z as i32 * step as i32;
        for x in 0..width {
            let world_x = rect.min_x + x as i32 * step as i32;
            heights.push(generator.get_height(seed, world_x, world_z));
        }
    }
    GridOut::new(rect, step, width, height, heights)
}

/// Serves a spawn-point request: the generator's transforms mapped onto
/// plain map-core data. Yaw is the raw generator value (radians, as
/// `Rotation3f::yaw` documents).
pub(crate) fn spawn_points(
    generator: &Rc<ChunkGenerator>,
    seed: i32,
) -> SourceResult<Vec<SpawnPoint>> {
    Ok(generator
        .get_spawn_points(seed)
        .into_iter()
        .map(|transform| {
            let position = transform.get_position();
            SpawnPoint {
                x: position.x,
                y: position.y,
                z: position.z,
                yaw: transform.get_rotation().yaw(),
            }
        })
        .collect())
}

/// Serves a zone lookup for one column.
pub(crate) fn zone_at(
    generator: &Rc<ChunkGenerator>,
    seed: i32,
    x: i32,
    z: i32,
) -> SourceResult<Option<ZoneId>> {
    let result = generator.get_zone_biome_result_at(seed, x, z);
    Ok(result
        .get_zone_result()
        .and_then(|zone_result| zone_result.get_zone())
        .map(|zone| ZoneId(zone.id)))
}

// ---------------------------------------------------------------------------
// Feature placement (T22 query, T23 adapter). Runs on a worker thread with
// that thread's generator; `FeaturePlacement` holds `Rc`s, so translation
// happens here and only plain `Feature` values cross the thread boundary.
// ---------------------------------------------------------------------------

/// Serves one feature-placement query (worker thread).
///
/// The caller's [`FeatureFilter`] is mapped to the generator's
/// [`PlacementFilter`] and passed through unchanged otherwise. The worldgen
/// query is the single **placement** filter authority (its post-placement
/// filter has the same documented semantics as [`FeatureFilter::matches`]),
/// so the adapter never re-applies the category/name/size predicates.
///
/// The one map-core pass is the translated *kind*: [`FeatureKind::Poi`] and
/// [`FeatureKind::UniquePrefab`] are both slices of the worldgen
/// `UNIQUE_PREFAB` kind (`show_on_map` decides), so a query for only one of
/// them would otherwise leak the other. Dropping by the requested
/// [`FeatureKindMask`] is not a placement predicate and cannot drift from the
/// generator.
///
/// A [`FeatureKind::CaveEntrance`] request contributes no worldgen kind yet
/// (T27 reserves it); a filter that asks for nothing else returns an empty
/// list without running the placement pass.
pub(crate) fn feature_query(
    generator: &Rc<ChunkGenerator>,
    seed: i32,
    rect: Rect,
    filter: &FeatureFilter,
) -> SourceResult<Vec<Feature>> {
    catch_query_panics(|| {
        let Some(kinds) = placement_kinds(filter.kinds) else {
            return Ok(Vec::new());
        };
        let block_rect = BlockRect {
            min_x: rect.min_x,
            min_z: rect.min_z,
            max_x: rect.max_x,
            max_z: rect.max_z,
        };
        let placement_filter = PlacementFilter {
            kinds,
            categories: filter.categories.clone(),
            name_contains: filter.name_contains.clone(),
            min_size: filter.min_size,
        };
        Ok(generator
            .features_in(seed, block_rect, &placement_filter)
            .iter()
            .filter_map(translate_placement)
            .filter(|feature| filter.kinds.contains(feature.kind))
            .collect())
    })
}

/// Maps a map-core kind mask onto the generator's placement mask.
///
/// Returns [`None`] when no worldgen kind is requested (e.g. only cave
/// entrances, which the query does not emit until T27): the caller then
/// skips the query entirely instead of running it just to filter everything
/// out.
fn placement_kinds(mask: FeatureKindMask) -> Option<PlacementKindMask> {
    let mut kinds: Option<PlacementKindMask> = None;
    let mut add = |kind: PlacementKindMask| {
        kinds = Some(match kinds {
            Some(current) => current | kind,
            None => kind,
        });
    };
    // `Poi` is a map-core split of `UniquePrefab`, so it needs the same
    // worldgen kind collected; the translation below applies the split.
    if mask.contains(FeatureKind::UniquePrefab) || mask.contains(FeatureKind::Poi) {
        add(PlacementKindMask::UNIQUE_PREFAB);
    }
    if mask.contains(FeatureKind::BiomePrefab) {
        add(PlacementKindMask::BIOME_PREFAB);
    }
    if mask.contains(FeatureKind::Spawn) {
        add(PlacementKindMask::SPAWN);
    }
    // `CaveEntrance` has no worldgen placement kind yet (reserved for T27).
    kinds
}

/// Translates one worldgen placement into a plain map-core [`Feature`].
///
/// The mapping:
///
/// - `Spawn` → [`FeatureKind::Spawn`];
/// - `UniquePrefab` with `show_on_map` → [`FeatureKind::Poi`] (the
///   chunkbase-style map pin; unique prefabs are otherwise hidden landmarks);
/// - `UniquePrefab` without `show_on_map` → [`FeatureKind::UniquePrefab`];
/// - `BiomePrefab` → [`FeatureKind::BiomePrefab`];
/// - any unknown/future kind (T27's cave entrance) → [`None`], skipped.
///
/// `metadata.source` records which generator list produced the feature
/// (`"unique-prefab"`, `"biome-prefab"` or `"spawn"`), so same-named
/// features from different sources stay distinguishable. `metadata.size` is
/// the larger horizontal footprint when bounds exist. `bounds` maps straight
/// through: both sides use inclusive block bounds, and the y span a unique
/// prefab reports is its placement origin (the worldgen query's documented
/// contract).
///
/// Public so the adapter parity test can run the same translation over a
/// directly loaded generator's query result. It is the only worldgen-typed
/// function this module exposes; everything else speaks map-core types.
#[must_use]
pub fn translate_placement(placement: &FeaturePlacement) -> Option<Feature> {
    let (kind, source) = match placement.kind {
        PlacementKind::Spawn => (FeatureKind::Spawn, "spawn"),
        PlacementKind::UniquePrefab if placement.show_on_map => (FeatureKind::Poi, "unique-prefab"),
        PlacementKind::UniquePrefab => (FeatureKind::UniquePrefab, "unique-prefab"),
        PlacementKind::BiomePrefab => (FeatureKind::BiomePrefab, "biome-prefab"),
        // `PlacementKind` is `#[non_exhaustive]`: a future cave-entrance kind
        // maps to nothing until T27 defines its map-core identity.
        _ => return None,
    };

    let bounds = placement.bounds.map(|bounds| {
        Rect3::new(
            bounds.min_x,
            bounds.min_y,
            bounds.min_z,
            bounds.max_x,
            bounds.max_y,
            bounds.max_z,
        )
    });
    let size = placement.bounds.map(|bounds| {
        let size_x = i64::from(bounds.max_x) - i64::from(bounds.min_x) + 1;
        let size_z = i64::from(bounds.max_z) - i64::from(bounds.min_z) + 1;
        i32::try_from(size_x.max(size_z)).unwrap_or(i32::MAX)
    });

    Some(Feature {
        kind,
        name: placement.name.clone(),
        category: placement.category.clone(),
        position: placement.position,
        bounds,
        metadata: FeatureMetadata {
            // Reserved: the query reports no variant string yet.
            variant: None,
            size,
            source: Some(source.to_string()),
        },
    })
}

// ---------------------------------------------------------------------------
// Chunk generation (T09): summaries and slices. Runs on the worker thread
// with that thread's generator + resource; the GeneratedChunk is dropped
// when the extraction finishes.
// ---------------------------------------------------------------------------

/// The per-worker chunk-generation state: the generator's registry snapshot
/// (shared with the generator, never re-loaded) plus the reusable scratch
/// resource. One per worker thread — the resource is `Rc`-based inside.
pub(crate) struct GenState {
    registry: Rc<worldgen_model::registry::Registries>,
    resource: ChunkGeneratorResource,
}

/// Builds the chunk-generation state after the generator loaded. The
/// registry snapshot comes from the generator itself (the loader always
/// attaches one); the `Registries::load()` fallback is a safety net that
/// never triggers for loader-built generators.
pub(crate) fn init_gen_state(generator: &Rc<ChunkGenerator>) -> SourceResult<GenState> {
    catch_load_panics(|| {
        let registry = match generator.get_registries() {
            Some(registry) => Rc::clone(registry),
            None => Rc::new(worldgen_model::registry::Registries::load()),
        };
        Ok(GenState {
            registry,
            resource: ChunkGeneratorResource::new(),
        })
    })
}

/// Maps the worldgen service's error onto the source vocabulary.
fn map_world_gen_error(error: WorldGenError) -> SourceError {
    match error {
        WorldGenError::Cancelled => SourceError::Cancelled,
        WorldGenError::Panic(message) => SourceError::Generation(message),
        WorldGenError::Load(message) => SourceError::Load(message),
        WorldGenError::Shutdown => {
            SourceError::Internal("the generator pool is shutting down".to_string())
        }
    }
}

/// Generates one chunk and extracts its compact summary (worker thread).
/// The `GeneratedChunk` is dropped when this returns; only the ~22 KB
/// summary crosses the thread boundary. Panics inside the pipeline are
/// caught and mapped to [`SourceError::Generation`] — the same per-job
/// boundary the worldgen service uses (world_gen.rs:650-658) — so a
/// hostile coordinate degrades to an error instead of killing the worker.
pub(crate) fn generate_summary(
    generator: &Rc<ChunkGenerator>,
    state: &mut GenState,
    seed: i32,
    cx: i32,
    cz: i32,
) -> SourceResult<Arc<ChunkSummary>> {
    catch_query_panics(|| {
        let request = WorldGenRequest::new(seed, cx, cz);
        let result = generate_chunk(
            generator,
            Rc::clone(&state.registry),
            &mut state.resource,
            &request,
        )
        .map_err(map_world_gen_error)?;
        Ok(Arc::new(chunks::extract_summary(&result.chunk, cx, cz)))
    })
}

/// Generates one chunk and extracts both the horizontal slice at `y` and
/// the compact summary — two products from one generation (worker thread).
pub(crate) fn generate_slice(
    generator: &Rc<ChunkGenerator>,
    state: &mut GenState,
    seed: i32,
    cx: i32,
    cz: i32,
    y: i32,
) -> SourceResult<(ChunkSlice, Arc<ChunkSummary>)> {
    catch_query_panics(|| {
        let request = WorldGenRequest::new(seed, cx, cz);
        let result = generate_chunk(
            generator,
            Rc::clone(&state.registry),
            &mut state.resource,
            &request,
        )
        .map_err(map_world_gen_error)?;
        let slice = chunks::extract_slice(&result.chunk, y);
        let summary = Arc::new(chunks::extract_summary(&result.chunk, cx, cz));
        Ok((slice, summary))
    })
}

/// Runs one worker-side query, converting a pipeline panic into
/// [`SourceError::Generation`] so the worker thread survives. See the
/// deviation note on [`load_generator`]; this mirrors the worldgen
/// service's per-job `catch_unwind` (world_gen.rs:650-658).
fn catch_query_panics<T>(work: impl FnOnce() -> SourceResult<T>) -> SourceResult<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)) {
        Ok(result) => result,
        Err(payload) => Err(SourceError::Generation(panic_message(payload))),
    }
}

#[cfg(test)]
mod tests {
    use super::{PlacementKind, PlacementKindMask, placement_kinds};
    use crate::types::{FeatureKind, FeatureKindMask};

    #[test]
    fn features_placement_kinds_map_poi_to_unique_and_skip_cave() {
        // Poi is the show_on_map slice of the worldgen unique kind.
        let poi = FeatureKindMask::from_kind(FeatureKind::Poi);
        let kinds = placement_kinds(poi).expect("Poi asks the generator for unique prefabs");
        assert!(kinds.contains(PlacementKind::UniquePrefab));
        assert!(!kinds.contains(PlacementKind::BiomePrefab));
        assert!(!kinds.contains(PlacementKind::Spawn));

        // Unique and Poi together still request one worldgen kind.
        let both = poi.union(FeatureKindMask::from_kind(FeatureKind::UniquePrefab));
        assert_eq!(
            placement_kinds(both),
            placement_kinds(poi),
            "the split kinds collapse to the same worldgen kind",
        );

        let spawn = placement_kinds(FeatureKindMask::from_kind(FeatureKind::Spawn))
            .expect("Spawn asks for spawn points");
        assert!(spawn.contains(PlacementKind::Spawn));
        assert!(!spawn.contains(PlacementKind::UniquePrefab));

        // Cave entrances (T27) and an empty mask request no worldgen kind,
        // so the query is skipped instead of running and dropping all.
        assert!(placement_kinds(FeatureKindMask::from_kind(FeatureKind::CaveEntrance)).is_none());
        assert!(placement_kinds(FeatureKindMask(0)).is_none());

        // Every worldgen kind can be requested by the matching map-core kind.
        let all = placement_kinds(FeatureKindMask::ALL).expect("at least one kind maps");
        assert_eq!(all, PlacementKindMask::all());
    }
}
