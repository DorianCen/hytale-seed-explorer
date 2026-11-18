//! T09 acceptance tests: extracted chunk summaries and horizontal slices must
//! agree with a chunk generated directly on the test thread.
//!
//! The real generator checkout lives next to this repository (see
//! [`world_dir`]); when it is absent every test prints a skip message and
//! returns. One [`WorldgenSource`] is shared by the fidelity tests so the pool
//! is loaded once (~1 s per worker); the direct-generator comparisons load one
//! additional generator on the test thread, which is `!Send` and therefore
//! never crosses a thread boundary.
//!
//! Every test holds [`SHARED_LOCK`]: the source has a single cancellation
//! token, so the cancel test must not race another test's in-flight request.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::Duration;

use map_core::error::SourceError;
use map_core::source::WorldSource;
use map_core::source::worldgen::{WorldgenSource, WorldgenSourceConfig};
use map_core::types::{BiomeId, BlockId, FluidId, Rect, SliceCell, SurfaceSample};

use worldgen_hytale::chunk::chunk_generator::ChunkGenerator;
use worldgen_hytale::chunk::chunk_generator_resource::ChunkGeneratorResource;
use worldgen_hytale::loader::chunk_generator_json_loader::ChunkGeneratorJsonLoader;
use worldgen_hytale::world_gen::{WorldGenRequest, generate_chunk};
use worldgen_hytale::world_gen_config::WorldGenConfig;
use worldgen_model::chunk::GeneratedChunk;

/// Seed used by the shared source and by the direct generators it is compared
/// with (T02 semantics: plain decimal text resolves to generator seed 12345).
const TEST_SEED: &str = "12345";
/// The generator-visible form of [`TEST_SEED`].
const GENERATOR_SEED: i32 = 12345;

/// Chunk edge in blocks (the generator's `ChunkUtil.SIZE`).
const CHUNK_SIZE: i32 = 32;
/// World height in blocks (the generator's `ChunkUtil.HEIGHT`).
const CHUNK_HEIGHT: i32 = 320;

/// Serializes every test that shares the pool; see the module docs.
static SHARED_LOCK: Mutex<()> = Mutex::new(());

/// The world-gen folder this repository's sibling checkout ships for tests, or
/// `None` (with a skip message) on machines without the checkout.
fn world_dir() -> Option<PathBuf> {
    let dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hytale-server-rust/testdata/Default");
    if dir.is_dir() {
        Some(dir)
    } else {
        eprintln!(
            "skipping: the worldgen checkout is not present at {}",
            dir.display()
        );
        None
    }
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
fn direct_generator() -> std::rc::Rc<ChunkGenerator> {
    let root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hytale-server-rust/testdata/Default");
    let config = WorldGenConfig::new(root, "Default".to_string(), "0.0.0".to_string());
    let loader =
        ChunkGeneratorJsonLoader::new(procedural::json::SeedString::new("ChunkGenerator"), config);
    loader.load()
}

/// Generates one chunk directly on the calling thread (never moved across
/// threads: the generator is `!Send`).
fn direct_chunk(generator: &std::rc::Rc<ChunkGenerator>, cx: i32, cz: i32) -> GeneratedChunk {
    let registry = std::rc::Rc::clone(
        generator
            .get_registries()
            .expect("the loader attached registries"),
    );
    let mut resource = ChunkGeneratorResource::new();
    let request = WorldGenRequest::new(GENERATOR_SEED, cx, cz);
    generate_chunk(generator, registry, &mut resource, &request)
        .expect("direct chunk generation")
        .chunk
}

/// The chunk-local coordinate of a world block coordinate (`0..32`).
fn local_of(x: i32) -> i32 {
    x & 31
}

/// `Solid(block)` / `Fluid(fluid)` / `Air` for one cell of a direct chunk —
/// the documented [`SliceCell`] classification.
fn classify_cell(chunk: &GeneratedChunk, lx: i32, y: i32, lz: i32) -> SliceCell {
    let block_id = chunk.get_block(lx, y, lz);
    let fluid_id = chunk.get_fluid_id(lx, y, lz);
    if block_id != 0 {
        SliceCell::Solid(BlockId(block_id))
    } else if fluid_id != 0 {
        SliceCell::Fluid(FluidId(fluid_id))
    } else {
        SliceCell::Air
    }
}

/// The documented extraction contract applied to a directly generated chunk:
/// topmost non-air block, topmost fluid plus its contiguous run downward,
/// per-column tint, and the generator's cheap biome query.
fn expected_sample(
    generator: &std::rc::Rc<ChunkGenerator>,
    chunk: &GeneratedChunk,
    x: i32,
    z: i32,
) -> SurfaceSample {
    let (lx, lz) = (local_of(x), local_of(z));

    let mut height = -1;
    let mut block = BlockId(0);
    for y in (0..CHUNK_HEIGHT).rev() {
        let block_id = chunk.get_block(lx, y, lz);
        if block_id != 0 {
            height = y;
            block = BlockId(block_id);
            break;
        }
    }

    let mut fluid = None;
    let mut fluid_depth = 0_u16;
    for y in (0..CHUNK_HEIGHT).rev() {
        let fluid_id = chunk.get_fluid_id(lx, y, lz);
        if fluid_id == 0 {
            continue;
        }
        fluid = Some(FluidId(fluid_id));
        // The contiguous run from the topmost fluid cell downward.
        let mut depth_y = y;
        while depth_y >= 0 && chunk.get_fluid_id(lx, depth_y, lz) != 0 {
            fluid_depth += 1;
            depth_y -= 1;
        }
        break;
    }

    let biome = generator
        .get_zone_biome_result_at(GENERATOR_SEED, x, z)
        .get_biome()
        .expect("the generator yields a biome for generated columns")
        .get_id();

    SurfaceSample {
        x,
        z,
        biome: BiomeId(biome),
        height,
        block,
        fluid,
        fluid_depth,
        tint: chunk.get_tint(lx, lz),
    }
}

/// Asserts one [`SurfaceSample`] field by field with readable messages, so a
/// mismatch names the exact field and coordinate.
fn assert_sample_matches(actual: &SurfaceSample, expected: &SurfaceSample, cx: i32, cz: i32) {
    let (x, z) = (expected.x, expected.z);
    let context = format!("chunk ({cx}, {cz}) column ({x}, {z})");
    assert_eq!(actual.x, expected.x, "{context}: world x");
    assert_eq!(actual.z, expected.z, "{context}: world z");
    assert_eq!(actual.biome, expected.biome, "{context}: biome");
    assert_eq!(actual.height, expected.height, "{context}: surface height");
    assert_eq!(actual.block, expected.block, "{context}: surface block");
    assert_eq!(actual.fluid, expected.fluid, "{context}: topmost fluid");
    assert_eq!(
        actual.fluid_depth, expected.fluid_depth,
        "{context}: fluid depth"
    );
    assert_eq!(actual.tint, expected.tint, "{context}: tint");
}

/// Chunk-local probe columns: the four chunk corners first, then a spread over
/// the 32×32 grid (24 columns per chunk).
const PROBE_LOCALS: [(i32, i32); 24] = [
    (0, 0),
    (31, 0),
    (0, 31),
    (31, 31),
    (15, 15),
    (16, 16),
    (6, 22),
    (22, 6),
    (9, 4),
    (4, 9),
    (27, 18),
    (18, 27),
    (1, 7),
    (7, 1),
    (31, 16),
    (16, 31),
    (0, 16),
    (16, 0),
    (23, 29),
    (29, 23),
    (11, 11),
    (20, 3),
    (3, 20),
    (25, 25),
];

/// One directly generated chunk plus the chunk-local columns to compare in it.
struct ChunkProbes {
    cx: i32,
    cz: i32,
    locals: &'static [(i32, i32)],
}

/// Proves the T09 acceptance bullet: `surface_at` reproduces the surface of a
/// directly generated chunk — height, block, fluid and depth, tint and biome —
/// including the negative-coordinate chunk `(-1, -1)`.
#[test]
fn surface_samples_match_a_directly_generated_chunk() {
    let Some(_world) = world_dir() else {
        return;
    };
    let _serial = exclusive_source();
    let source = source();
    let generator = direct_generator();

    let probes = [
        ChunkProbes {
            cx: 0,
            cz: 0,
            locals: &PROBE_LOCALS,
        },
        ChunkProbes {
            cx: 4,
            cz: -5,
            locals: &PROBE_LOCALS,
        },
        // World (-1, -1) is chunk (-1, -1) local (31, 31).
        ChunkProbes {
            cx: -1,
            cz: -1,
            locals: &[(31, 31)],
        },
    ];

    for probe in probes {
        let (cx, cz) = (probe.cx, probe.cz);
        let chunk = direct_chunk(&generator, cx, cz);
        for (lx, lz) in probe.locals {
            let x = cx * CHUNK_SIZE + lx;
            let z = cz * CHUNK_SIZE + lz;
            let expected = expected_sample(&generator, &chunk, x, z);
            let actual = source
                .surface_at(x, z)
                .unwrap_or_else(|error| panic!("surface_at({x}, {z}) failed: {error}"));
            assert_sample_matches(&actual, &expected, cx, cz);
        }
    }
}

/// Proves the T09 acceptance bullet: `surface_region` is `surface_at` on a
/// strided grid, including across chunk borders and at steps 1, 2 and 4.
#[test]
fn surface_region_matches_surface_at_at_steps_1_2_4() {
    let Some(_world) = world_dir() else {
        return;
    };
    let _serial = exclusive_source();
    let source = source();

    let rect = Rect::from_dims(7, -40, 40, 29);
    for step in [1_u32, 2, 4] {
        let grid = source
            .surface_region(rect, step)
            .expect("the source serves the surface region");
        let (width, height) = rect.sample_dims(step);
        assert_eq!(
            (grid.width, grid.height),
            (width, height),
            "step {step}: the grid covers exactly the requested samples"
        );
        assert_eq!(
            (grid.width, grid.height),
            (40_u32.div_ceil(step), 29_u32.div_ceil(step)),
            "step {step}: width/height are ceil(blocks / step)"
        );
        assert_eq!(
            (grid.origin_x, grid.origin_z, grid.step),
            (7, -40, step),
            "step {step}: origin and step"
        );

        for z_index in 0..height {
            for x_index in 0..width {
                let x = rect.min_x + x_index as i32 * step as i32;
                let z = rect.min_z + z_index as i32 * step as i32;
                let sample = *grid
                    .sample(x, z)
                    .unwrap_or_else(|| panic!("step {step}: grid misses ({x}, {z})"));
                let point = source
                    .surface_at(x, z)
                    .unwrap_or_else(|error| panic!("surface_at({x}, {z}) failed: {error}"));
                assert_eq!(
                    sample, point,
                    "step {step}: grid sample ({x}, {z}) must equal surface_at"
                );
            }
        }
    }
}

/// Proves the T09 acceptance bullet: `slice_region` classifies every column of
/// a directly generated chunk exactly (solid block, fluid, or air) at several
/// world heights.
#[test]
fn slices_match_a_directly_generated_chunk_at_several_y() {
    let Some(_world) = world_dir() else {
        return;
    };
    let _serial = exclusive_source();
    let source = source();
    let generator = direct_generator();
    let chunk = direct_chunk(&generator, 0, 0);

    let rect = Rect::from_dims(0, 0, 32, 32);
    for y in [0, 64, 319] {
        let grid = source
            .slice_region(rect, 1, y)
            .expect("the source serves the slice region");
        assert_eq!(grid.y, y, "the grid reports the requested world height");
        assert_eq!(
            (grid.width, grid.height, grid.step),
            (32, 32, 1),
            "the slice covers the whole chunk at step 1"
        );
        assert_eq!(grid.origin_x, 0, "slice origin x");
        assert_eq!(grid.origin_z, 0, "slice origin z");
        assert_eq!(
            grid.cells.len(),
            32 * 32,
            "one cell per chunk column at step 1"
        );

        for lz in 0..32 {
            for lx in 0..32 {
                let expected = classify_cell(&chunk, lx, y, lz);
                let actual = *grid
                    .sample(lx, lz)
                    .unwrap_or_else(|| panic!("y={y}: grid misses ({lx}, {lz})"));
                assert_eq!(
                    actual, expected,
                    "y={y}: slice cell ({lx}, {lz}) must match the direct chunk"
                );
            }
        }
    }
}

/// Proves the T09 acceptance bullet: chunk-dependent queries outside the
/// generatable area are typed [`SourceError::OutOfBounds`] errors for the
/// single-point, surface-region and slice paths.
#[test]
fn out_of_bounds_queries_are_typed_errors() {
    let Some(_world) = world_dir() else {
        return;
    };
    let _serial = exclusive_source();
    let source = source();

    let (_, max_chunk) = source.chunk_bounds();
    let cx = max_chunk.checked_add(1).expect("the bound leaves room");
    let x = cx
        .checked_mul(CHUNK_SIZE)
        .expect("the out-of-bounds probe coordinate fits i32");
    let rect = Rect::from_dims(x, 0, 32, 32);

    // Check every path independently so one wrong result does not hide the
    // state of the others.
    let surface = source.surface_at(x, 0);
    let region = source.surface_region(rect, 1);
    let slice = source.slice_region(rect, 1, 64);

    let mut failures = Vec::new();
    if !matches!(surface, Err(SourceError::OutOfBounds { .. })) {
        failures.push(format!("surface_at({x}, 0) returned {surface:?}"));
    }
    if !matches!(region, Err(SourceError::OutOfBounds { .. })) {
        failures.push(format!(
            "surface_region over chunk ({cx}, 0) returned {region:?}"
        ));
    }
    if !matches!(slice, Err(SourceError::OutOfBounds { .. })) {
        failures.push(format!(
            "slice_region over chunk ({cx}, 0) returned {slice:?}"
        ));
    }
    assert!(
        failures.is_empty(),
        "chunk-dependent queries outside the generatable area must be OutOfBounds: {}",
        failures.join("; ")
    );
}

/// Proves the T09 acceptance bullet: a slice above or below the world height
/// is air everywhere (the generator reports 0 outside `0..320`).
#[test]
fn slice_of_uncovered_y_is_all_air() {
    let Some(_world) = world_dir() else {
        return;
    };
    let _serial = exclusive_source();
    let source = source();

    let rect = Rect::from_dims(0, 0, 32, 32);
    for y in [-5, 400] {
        let grid = source
            .slice_region(rect, 1, y)
            .expect("an out-of-height slice is a valid (air) grid");
        assert_eq!(grid.y, y, "the grid reports the requested world height");
        assert!(
            grid.cells.iter().all(|cell| *cell == SliceCell::Air),
            "y={y}: every cell outside the world height must be air"
        );
        assert_eq!(
            grid.sample(0, 0),
            Some(&SliceCell::Air),
            "y={y}: sampling the grid corner is air"
        );
    }
}

/// Proves the T09 acceptance bullet: a long surface request returns
/// [`SourceError::Cancelled`] promptly when the caller cancels it, and the
/// source re-arms its token for the next request.
#[test]
fn a_large_surface_region_can_be_cancelled() {
    let Some(_world) = world_dir() else {
        return;
    };
    let _serial = exclusive_source();
    let source = source();

    // 2×64 chunks: ~60 s uncancelled, but the token poll between chunks lands
    // within about one chunk generation.
    let rect = Rect::from_dims(0, 0, 64, 2048);
    let in_flight = thread::spawn(move || source.surface_region(rect, 1));
    thread::sleep(Duration::from_millis(150));
    source.cancel_pending();
    let result = in_flight
        .join()
        .expect("the cancelled request thread does not panic");
    assert!(
        matches!(result, Err(SourceError::Cancelled)),
        "the cancelled surface request must return Cancelled, got {result:?}"
    );

    // The token re-arms at the start of the next region request.
    let grid = source
        .surface_region(Rect::from_dims(0, 0, 1, 1), 1)
        .expect("the source still serves a tiny region after a cancel");
    assert_eq!(grid.width, 1, "the tiny region has one sample");
    assert_eq!(grid.height, 1, "the tiny region has one sample");
    source
        .surface_at(0, 0)
        .expect("the source still serves surface queries after a cancel");
}
