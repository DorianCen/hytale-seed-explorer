//! T08 acceptance tests for the production worldgen source and its worker pool.
//!
//! The real generator checkout lives next to this repository (see
//! [`world_dir`]); when it is absent every test prints a skip message and
//! returns. One [`WorldgenSource`] is shared by the query tests so the pool is
//! loaded once (~0.9 s per worker); the direct-generator parity test loads one
//! additional generator on the test thread.
//!
//! Every test that touches the shared source holds [`SHARED_LOCK`]: the source
//! has a single cancellation token, so the cancel test must not race another
//! test's in-flight region request.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use map_core::error::SourceError;
use map_core::source::WorldSource;
use map_core::source::worldgen::{WorldgenSource, WorldgenSourceConfig};
use map_core::types::{BiomeGrid, BiomeId, Rect, Seed};

use worldgen_hytale::chunk::chunk_generator::ChunkGenerator;
use worldgen_hytale::loader::chunk_generator_json_loader::ChunkGeneratorJsonLoader;
use worldgen_hytale::world_gen_config::WorldGenConfig;
use worldgen_hytale::world_gen_provider::{HytaleWorldGenProvider, Semver};

/// Seed used by the shared source and by the direct generator it is compared
/// with (T02 semantics: plain decimal text).
const TEST_SEED: &str = "12345";

/// Serializes tests that share the pool; see the module docs.
static SHARED_LOCK: Mutex<()> = Mutex::new(());

/// The world-gen folder this repository's sibling checkout ships for tests,
/// or `None` (with a skip message) on machines without it — including
/// machines without the user-supplied, git-ignored asset pack, which the
/// committed world's prefab references require (CI has no pack).
fn world_dir() -> Option<PathBuf> {
    let dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hytale-server-rust/testdata/Default");
    if !dir.is_dir() {
        eprintln!(
            "skipping: the worldgen checkout is not present at {}",
            dir.display()
        );
        return None;
    }
    let assets = dir.join("../../Assets/extracted");
    if !assets.is_dir() {
        eprintln!(
            "skipping: the worldgen checkout has no asset pack at {} (the committed world \
             references prefabs from the user-supplied pack)",
            assets.display()
        );
        return None;
    }
    Some(dir)
}

/// The shared, already-loaded production source (2 workers, seed `12345`).
fn source() -> &'static WorldgenSource {
    static SOURCE: OnceLock<WorldgenSource> = OnceLock::new();
    SOURCE.get_or_init(|| {
        let world = world_dir().expect("the shared source is only built after world_dir()");
        let config = WorldgenSourceConfig {
            workers: Some(2),
            ..WorldgenSourceConfig::new(world, TEST_SEED)
        };
        WorldgenSource::new(&config).expect("the shared worldgen source loads")
    })
}

/// Locks the shared source; hold the guard for the whole test.
fn exclusive_source() -> MutexGuard<'static, ()> {
    SHARED_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Loads a generator directly on the calling thread, exactly like the pool
/// workers do, for parity comparisons.
fn load_direct_generator(world: &Path) -> std::rc::Rc<ChunkGenerator> {
    let provider = HytaleWorldGenProvider {
        name: "Default".to_string(),
        version: Semver::parse("0.0.0").expect("0.0.0 is a valid semver"),
        path: None,
    };
    let resolved = provider
        .resolve_path(world)
        .expect("the testdata world folder resolves");
    let config = WorldGenConfig::new(resolved, "Default".to_string(), "0.0.0".to_string());
    let loader =
        ChunkGeneratorJsonLoader::new(procedural::json::SeedString::new("ChunkGenerator"), config);
    loader.load()
}

/// Proves the T08 acceptance bullet: region queries served by the worker pool
/// agree with one generator loaded directly, for every sample of a rect with a
/// negative origin.
#[test]
fn regions_match_a_directly_loaded_generator() {
    let Some(world) = world_dir() else {
        return;
    };
    let _serial = exclusive_source();
    let source = source();

    let rect = Rect::from_dims(-25, -17, 17, 11);
    let step = 3;

    let biomes = source
        .biome_region(rect, step)
        .expect("the source serves the biome region");
    let heights = source
        .height_region(rect, step)
        .expect("the source serves the height region");
    let (samples_x, samples_z) = rect.sample_dims(step);
    assert_eq!(
        (biomes.width, biomes.height),
        (samples_x, samples_z),
        "the biome grid covers exactly the requested samples"
    );
    assert_eq!(
        (heights.width, heights.height),
        (samples_x, samples_z),
        "the height grid covers exactly the requested samples"
    );

    let generator = load_direct_generator(&world);
    let seed = Seed::parse(TEST_SEED)
        .expect("the test seed parses")
        .generator();

    for (x, z) in rect.iter_blocks(step) {
        let expected_biome = generator
            .get_zone_biome_result_at(seed, x, z)
            .get_biome()
            .expect("the generator yields a biome for generated columns")
            .get_id();
        assert_eq!(
            biomes.sample(x, z),
            Some(&BiomeId(expected_biome)),
            "pool biome at ({x}, {z}) must match the direct generator"
        );

        let expected_height = generator.get_height(seed, x, z);
        assert_eq!(
            heights.sample(x, z),
            Some(&expected_height),
            "pool height at ({x}, {z}) must match the direct generator"
        );
    }
}

/// Proves the T08 acceptance bullet: the single-point queries agree with the
/// batch regions they are built on.
#[test]
fn single_point_queries_agree_with_regions() {
    let Some(_world) = world_dir() else {
        return;
    };
    let _serial = exclusive_source();
    let source = source();

    let biome = source
        .biome_at(-382, -774)
        .expect("biome_at serves the hover query");
    let biome_region = source
        .biome_region(Rect::from_dims(-382, -774, 1, 1), 1)
        .expect("the one-column biome region");
    assert_eq!(
        Some(&biome),
        biome_region.sample(-382, -774),
        "biome_at(-382, -774) must equal the region sample"
    );

    let height = source
        .height_at(0, 0)
        .expect("height_at serves the hover query");
    let height_region = source
        .height_region(Rect::from_dims(0, 0, 1, 1), 1)
        .expect("the one-column height region");
    assert_eq!(
        Some(&height),
        height_region.sample(0, 0),
        "height_at(0, 0) must equal the region sample"
    );
}

/// Proves the T08 acceptance bullet: biome regions are deterministic, so the
/// renderer may key tiles by coordinates and identity.
#[test]
fn regions_are_deterministic() {
    let Some(_world) = world_dir() else {
        return;
    };
    let _serial = exclusive_source();
    let source = source();

    let rect = Rect::from_dims(-25, -17, 17, 11);
    let step = 3;
    let first = source
        .biome_region(rect, step)
        .expect("the first biome region");
    let second = source
        .biome_region(rect, step)
        .expect("the second biome region");
    assert_eq!(first, second, "the same biome region must be identical");
}

/// Proves the T08 acceptance bullet: the pool answers concurrent requests
/// correctly, each result equal to the serial request for the same rect.
#[test]
fn parallel_requests_match_serial_results() {
    let Some(_world) = world_dir() else {
        return;
    };
    let _serial = exclusive_source();
    let source = source();

    let rects = [
        Rect::from_dims(64, 96, 13, 9),
        Rect::from_dims(-140, 40, 11, 13),
    ];
    let step = 2;
    let serial: Vec<BiomeGrid> = rects
        .iter()
        .map(|rect| {
            source
                .biome_region(*rect, step)
                .expect("the serial biome region")
        })
        .collect();

    thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(|| {
                    rects.map(|rect| {
                        source
                            .biome_region(rect, step)
                            .expect("the parallel biome region")
                    })
                })
            })
            .collect();

        for handle in handles {
            let results = handle.join().expect("the parallel request thread");
            for (index, result) in results.iter().enumerate() {
                assert_eq!(
                    result, &serial[index],
                    "parallel result for the rect at x = {} must equal the serial one",
                    rects[index].min_x
                );
            }
        }
    });
}

/// Proves the T08 acceptance bullet: a bad world path fails cleanly with
/// [`SourceError::Load`] and does not hang.
#[test]
fn a_bad_world_path_fails_cleanly() {
    let Some(_world) = world_dir() else {
        return;
    };

    let missing =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/no-such-worldgen-folder-t08");
    let config = WorldgenSourceConfig {
        workers: Some(1),
        ..WorldgenSourceConfig::new(missing, TEST_SEED)
    };
    let error = WorldgenSource::new(&config)
        .err()
        .expect("a missing world folder must not load");
    assert!(
        matches!(error, SourceError::Load(_)),
        "a missing world folder must fail with SourceError::Load, got {error:?}"
    );
}

/// Proves the T08 acceptance bullet: an in-flight region request can be
/// cancelled and reports [`SourceError::Cancelled`] instead of running to
/// completion.
#[test]
fn a_large_height_region_can_be_cancelled() {
    let Some(_world) = world_dir() else {
        return;
    };
    let _serial = exclusive_source();
    let source = source();

    // 8 x 4096 height samples at step 1 is roughly 32k cold samples (~30 s),
    // so the 100 ms cancel always lands mid-flight.
    let rect = Rect::from_dims(0, 0, 8, 4096);
    let result = thread::scope(|scope| {
        let worker = scope.spawn(|| source.height_region(rect, 1));
        thread::sleep(Duration::from_millis(100));
        source.cancel_pending();
        worker.join().expect("the height region thread")
    });

    assert!(
        matches!(result, Err(SourceError::Cancelled)),
        "the huge height region must be cancelled, got {result:?}"
    );
}

/// Smoke-checks the chunk-dependent queries after T09 landed them (the deep
/// fidelity tests live in `tests/surface_slice.rs` and the cache behavior in
/// `tests/chunk_cache.rs`).
#[test]
fn surface_and_slice_queries_are_implemented_by_t09() {
    let Some(_world) = world_dir() else {
        return;
    };
    let _serial = exclusive_source();
    let source = source();
    let rect = Rect::from_dims(0, 0, 4, 4);

    let surface = source.surface_at(0, 0).expect("T09 serves surface_at");
    assert!(
        (-1..320).contains(&surface.height),
        "the surface height must be a world y, got {surface:?}"
    );
    let region = source
        .surface_region(rect, 1)
        .expect("T09 serves surface_region");
    assert_eq!((region.width, region.height), (4, 4));
    let slice = source
        .slice_region(rect, 1, 64)
        .expect("T09 serves slice_region");
    assert_eq!(slice.y, 64);
}

/// Proves the T08 acceptance bullet: spawn points and zone lookups are served
/// and the zone id resolves through the zone catalog.
#[test]
fn spawn_and_zone_queries_work() {
    let Some(_world) = world_dir() else {
        return;
    };
    let _serial = exclusive_source();
    let source = source();

    let spawns = source
        .spawn_points()
        .expect("the source serves spawn points");
    assert!(
        !spawns.is_empty(),
        "the default world must offer at least one spawn point"
    );
    for spawn in &spawns {
        assert!(
            spawn.x.is_finite() && spawn.y.is_finite() && spawn.z.is_finite(),
            "spawn position ({}, {}, {}) must be finite",
            spawn.x,
            spawn.y,
            spawn.z
        );
        assert!(
            spawn.yaw.is_finite(),
            "spawn yaw {} must be finite",
            spawn.yaw
        );
    }

    let zone = source
        .zone_at(0, 0)
        .expect("the source serves zone lookups")
        .expect("the default world has a zone at the origin");
    assert!(
        source.zone_catalog().get(zone).is_some(),
        "zone {} must resolve through the zone catalog",
        zone.0
    );
}

/// Proves the T19 zone debug path: the batched `zone_region` grid equals
/// single-point `zone_at` for every sample, row-major.
#[test]
fn zone_region_matches_zone_at_sample_by_sample() {
    let Some(_world) = world_dir() else {
        return;
    };
    let _serial = exclusive_source();
    let source = source();

    // 8 x 8 blocks at step 2 → 4 x 4 samples, well inside the world.
    let rect = Rect::from_dims(8, 8, 8, 8);
    let step = 2;
    let grid = source
        .zone_region(rect, step)
        .expect("the source serves the zone region");
    let (width, height) = rect.sample_dims(step);
    assert_eq!(
        (grid.width, grid.height, grid.step),
        (width, height, step),
        "the zone grid covers exactly the requested samples"
    );

    let mut expected = Vec::with_capacity((width * height) as usize);
    for z in 0..height {
        for x in 0..width {
            let world_x = rect.min_x + (x * step) as i32;
            let world_z = rect.min_z + (z * step) as i32;
            expected.push(
                source
                    .zone_at(world_x, world_z)
                    .expect("the source serves zone lookups"),
            );
        }
    }
    assert_eq!(
        grid.zones, expected,
        "the batched zone grid must equal zone_at at every sample, row-major"
    );
}

/// Proves the T08 acceptance bullet: catalog data resolves the ids the query
/// path returns, plus the golden registries' block names.
#[test]
fn catalogs_resolve_the_queries() {
    let Some(_world) = world_dir() else {
        return;
    };
    let _serial = exclusive_source();
    let source = source();

    let region = source
        .biome_region(Rect::from_dims(-30, 60, 21, 15), 4)
        .expect("a small biome region for the catalog check");
    let biomes = source.biome_catalog();
    for biome in &region.biomes {
        let Some(info) = biomes.get(*biome) else {
            panic!("biome {} is missing from the catalog", biome.0);
        };
        assert!(
            !info.name.is_empty(),
            "biome {} must have a non-empty name",
            biome.0
        );
    }

    let blocks = source.block_catalog();
    assert!(!blocks.is_empty(), "the block catalog must not be empty");
    assert!(
        blocks.get_id("Rock_Stone").is_some(),
        "the golden registries contain Rock_Stone"
    );

    let fluids = source.fluid_catalog();
    assert!(!fluids.is_empty(), "the fluid catalog must not be empty");
}

/// Proves the T08 acceptance bullet: the source is `Send + Sync`, so tiles can
/// be generated on a rayon pool and read from the UI thread.
#[test]
fn the_source_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}

    let Some(_world) = world_dir() else {
        return;
    };
    assert_send_sync::<WorldgenSource>();
}

/// Proves the T08 acceptance bullet: dropping a source joins its workers and
/// returns instead of hanging.
#[test]
fn dropping_a_source_does_not_hang() {
    let Some(world) = world_dir() else {
        return;
    };

    // A private source: no shared state, so it can load concurrently with the
    // shared pool.
    let config = WorldgenSourceConfig {
        workers: Some(1),
        ..WorldgenSourceConfig::new(world, "777")
    };
    let source = WorldgenSource::new(&config).expect("the private worldgen source loads");
    let biome = source.biome_at(3, 7).expect("a tiny biome query");
    assert!(
        source.biome_catalog().get(biome).is_some(),
        "the tiny query returns a catalogued biome"
    );

    let started = Instant::now();
    drop(source);
    let elapsed = started.elapsed();
    // Reaching this assertion means the drop joined its worker; the bound just
    // documents that shutdown is prompt rather than a second generator load.
    assert!(
        elapsed < Duration::from_secs(30),
        "dropping the source took {elapsed:?}, expected a prompt worker join"
    );
}
