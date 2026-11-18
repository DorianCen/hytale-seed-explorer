//! The production `WorldSource` adapter over the external worldgen
//! workspace (T08).
//!
//! Cheap queries only: biome, height, zone and spawn-point data served from
//! a pool of generator worker threads (see [`pool`]). Chunk-dependent
//! queries — surface blocks, fluid depths, slices — return
//! [`SourceError::Unsupported`] until T09 adds chunk summaries. Feature
//! placement ([`crate::source::FeatureSource`]) is T22/T23's.
//!
//! This module is the only place where worldgen types cross into map-core
//! (the T07 acceptance rule): the public API exposes plain map-core ids and
//! grids, translated here.

mod pool;

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use worldgen_hytale::chunk::chunk_generator::ChunkGenerator;
use worldgen_hytale::loader::chunk_generator_json_loader::ChunkGeneratorJsonLoader;
use worldgen_hytale::world_gen_config::WorldGenConfig;
use worldgen_hytale::world_gen_provider::{HytaleWorldGenProvider, Semver};

use crate::catalog::{BiomeCatalog, BiomeInfo, BlockCatalog, FluidCatalog, ZoneCatalog, ZoneInfo};
use crate::error::{SourceError, SourceResult};
use crate::source::WorldSource;
use crate::types::{
    BiomeGrid, BiomeId, BlockId, FluidId, GeneratorIdentity, HeightGrid, Rect, Seed, SpawnPoint,
    ZoneId,
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
            seed_input: seed_input.into(),
        }
    }
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
            generator_version: worldgen_hytale::GENERATOR_VERSION,
            world_assets: format!(
                "{}@{} ({})",
                config.world_name,
                config.world_version,
                config.world_path.display()
            ),
            // Bumped when the committed fallback palette changes (T03/T20).
            palette_version: 0,
        };
        Ok(Self {
            seed,
            identity,
            catalogs,
            pool,
            min_chunk,
            max_chunk,
            cancel: cancel_token(),
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

    fn surface_at(&self, _x: i32, _z: i32) -> SourceResult<crate::types::SurfaceSample> {
        Err(SourceError::Unsupported(
            "surface queries arrive with T09 (chunk summaries)",
        ))
    }

    fn surface_region(&self, _rect: Rect, _step: u32) -> SourceResult<crate::types::SurfaceGrid> {
        Err(SourceError::Unsupported(
            "surface queries arrive with T09 (chunk summaries)",
        ))
    }

    fn slice_region(
        &self,
        _rect: Rect,
        _step: u32,
        _y: i32,
    ) -> SourceResult<crate::types::SliceGrid> {
        Err(SourceError::Unsupported(
            "slice queries arrive with T09 (chunk summaries)",
        ))
    }

    fn spawn_points(&self) -> SourceResult<Vec<SpawnPoint>> {
        self.pool.spawn_points()
    }

    fn zone_at(&self, x: i32, z: i32) -> SourceResult<Option<ZoneId>> {
        self.pool.zone_at(x, z)
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

/// Extracts a readable message from a panic payload (the same extraction
/// the worldgen repo's loader uses, loader/mod.rs:35-45).
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        return (*message).to_string();
    }
    "<non-string panic>".to_string()
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
