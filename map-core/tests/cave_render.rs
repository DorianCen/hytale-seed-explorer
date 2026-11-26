//! T28 acceptance: the cave slice and cave-density queries against a
//! directly generated chunk.
//!
//! The real-generator tests skip (printing why) when the sibling worldgen
//! checkout or the user-supplied asset pack is absent. The unit tests in
//! `map_core::render::caves` cover the color/ramp mapping without a world.
//!
//! The task's "out-of-world cells are Unknown" test note is adapted to the
//! T09 contract: a Y outside `0..320` is `Air` everywhere (the generator
//! reports no blocks there), while a rect outside the generatable chunk area
//! is a typed `OutOfBounds`. Neither panics. The T28 tile renderer still
//! leaves out-of-world *tile margins* transparent through the shared
//! `render_layer_tile` skeleton.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use map_core::coords::ZoomLevel;
use map_core::render::Layer;
use map_core::render::caves::{
    CAVE_DENSITY_MODE, CaveDensityTileRenderer, CaveSliceTileRenderer, cave_density_color,
    cave_slice_color, cave_slice_mode,
};
use map_core::scheduler::{TileRenderer, TileSubject, cancel_token};
use map_core::source::worldgen::{WorldgenSource, WorldgenSourceConfig};
use map_core::source::{SummaryFidelity, WorldSource};
use map_core::types::{BlockId, FluidId, Rect, SliceCell};

use worldgen_hytale::chunk::chunk_generator::ChunkGenerator;
use worldgen_hytale::chunk::chunk_generator_resource::ChunkGeneratorResource;
use worldgen_hytale::loader::chunk_generator_json_loader::ChunkGeneratorJsonLoader;
use worldgen_hytale::world_gen::{WorldGenRequest, generate_chunk};
use worldgen_hytale::world_gen_config::WorldGenConfig;
use worldgen_model::chunk::GeneratedChunk;

/// Seed of the shared source and the direct generator (T02: plain decimal).
const TEST_SEED: &str = "12345";
const GENERATOR_SEED: i32 = 12345;
const CHUNK_SIZE: i32 = 32;
const CHUNK_HEIGHT: i32 = 320;

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

fn direct_generator() -> std::rc::Rc<ChunkGenerator> {
    let root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hytale-server-rust/testdata/Default");
    let config = WorldGenConfig::new(root, "Default".to_string(), "0.0.0".to_string());
    let loader =
        ChunkGeneratorJsonLoader::new(procedural::json::SeedString::new("ChunkGenerator"), config);
    loader.load()
}

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

/// The T28 density metric applied directly to a generated chunk: air cells
/// strictly below the topmost non-air block, divided by that block's Y.
fn expected_density(chunk: &GeneratedChunk, lx: i32, lz: i32) -> f32 {
    let mut height = -1;
    for y in (0..CHUNK_HEIGHT).rev() {
        if chunk.get_block(lx, y, lz) != 0 {
            height = y;
            break;
        }
    }
    if height <= 0 {
        return 0.0;
    }
    let caves = (0..height)
        .filter(|&y| chunk.get_block(lx, y, lz) == 0 && chunk.get_fluid_id(lx, y, lz) == 0)
        .count() as f32;
    caves / height as f32
}

/// Proves the T28 acceptance bullet: a slice at each key elevation matches a
/// directly generated chunk's solid/fluid/air classification, including Y=0
/// and the top cell.
#[test]
fn cave_render_slice_matches_a_direct_chunk_at_six_y() {
    let Some(_world) = world_dir() else {
        return;
    };
    let source = source();
    let generator = direct_generator();
    let chunk = direct_chunk(&generator, 0, 0);
    let rect = Rect::from_dims(0, 0, CHUNK_SIZE as u32, CHUNK_SIZE as u32);

    for y in [0, 32, 64, 128, 256, 319] {
        let grid = source
            .slice_region(rect, 1, y)
            .unwrap_or_else(|error| panic!("slice_region at y={y} failed: {error}"));
        assert_eq!(grid.y, y, "the grid reports the requested Y");
        assert_eq!(
            (grid.width, grid.height, grid.step),
            (CHUNK_SIZE as u32, CHUNK_SIZE as u32, 1)
        );
        for lz in 0..CHUNK_SIZE {
            for lx in 0..CHUNK_SIZE {
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

/// Proves the T28 density definition: every column's fraction equals the
/// directly counted air-below-surface / top-block-Y value from one generated
/// chunk, and is inside `0.0..=1.0`.
#[test]
fn cave_density_matches_a_direct_chunk_count() {
    let Some(_world) = world_dir() else {
        return;
    };
    let source = source();
    let generator = direct_generator();
    let chunk = direct_chunk(&generator, 0, 0);
    let rect = Rect::from_dims(0, 0, CHUNK_SIZE as u32, CHUNK_SIZE as u32);

    let grid = source
        .cave_density_region(rect, 1)
        .expect("the source serves cave density");
    assert_eq!(
        (grid.width, grid.height, grid.step),
        (CHUNK_SIZE as u32, CHUNK_SIZE as u32, 1)
    );
    let mut nonzero = 0usize;
    for lz in 0..CHUNK_SIZE {
        for lx in 0..CHUNK_SIZE {
            let expected = expected_density(&chunk, lx, lz);
            let actual = *grid
                .sample(lx, lz)
                .unwrap_or_else(|| panic!("density grid misses ({lx}, {lz})"));
            assert!(
                (0.0..=1.0).contains(&actual),
                "({lx}, {lz}) density {actual} is a fraction"
            );
            assert!(
                (actual - expected).abs() < 1e-6,
                "({lx}, {lz}) density {actual} must match the direct count {expected}"
            );
            nonzero += usize::from(actual > 0.0);
        }
    }
    assert!(
        nonzero > 0,
        "the fixture chunk has cave volume; the density check is not vacuous"
    );
}

/// A pre-cancelled slice request returns the typed cancellation before
/// paying for a chunk generation.
#[test]
fn cave_render_slice_region_honors_cancellation() {
    let Some(_world) = world_dir() else {
        return;
    };
    let source = source();
    let cancel = Arc::new(AtomicBool::new(true));
    let result = source.slice_region_cancelled(Rect::from_dims(0, 0, 64, 64), 1, 64, &cancel);
    assert!(
        matches!(result, Err(map_core::error::SourceError::Cancelled)),
        "a cancelled slice is a typed cancellation, got {result:?}"
    );
}

/// The density path has the same caller-token contract.
#[test]
fn cave_density_region_honors_cancellation() {
    let Some(_world) = world_dir() else {
        return;
    };
    let source = source();
    let cancel = Arc::new(AtomicBool::new(true));
    let result = source.cave_density_region_cancelled(Rect::from_dims(0, 0, 64, 64), 1, &cancel);
    assert!(
        matches!(result, Err(map_core::error::SourceError::Cancelled)),
        "cancelled density is a typed cancellation, got {result:?}"
    );
}

/// The tile path, without a world: a user Y is encoded in the key mode and
/// selects the source plane; the density renderer uses the metric mode.
/// (The real source tests above prove the data; this proves the dispatch and
/// the key contract.)
#[test]
fn cave_render_tile_keys_carry_the_y_and_metric_mode() {
    let slice = TileSubject::new(
        7,
        12345,
        Layer::CaveSlice,
        cave_slice_mode(64),
        ZoomLevel::MIN,
    );
    let other_y = TileSubject::new(
        7,
        12345,
        Layer::CaveSlice,
        cave_slice_mode(65),
        ZoomLevel::MIN,
    );
    assert_eq!(slice.mode, 64, "the mode is the world Y");
    assert_ne!(
        slice.key_at(0, 0),
        other_y.key_at(0, 0),
        "a Y change is a different tile key"
    );

    let density = TileSubject::new(
        7,
        12345,
        Layer::CaveDensity,
        CAVE_DENSITY_MODE,
        ZoomLevel::MIN,
    );
    assert_eq!(density.mode, CAVE_DENSITY_MODE);
    assert_ne!(
        density.key_at(0, 0).mode,
        slice.key_at(0, 0).mode,
        "density and slice cannot collide"
    );
    assert_ne!(CAVE_DENSITY_MODE, 0, "mode 0 means no layer mode");
}

/// The slice renderer's streaming final tile is pixel-identical to the
/// one-shot render, and the intermediate publications are marked incomplete
/// (so the scheduler never treats a partial as an authoritative hit). A
/// 64-block world keeps this to 2×2 chunks.
#[test]
fn cave_render_streaming_final_matches_one_shot() {
    let Some(world) = world_dir() else {
        return;
    };
    let config = WorldgenSourceConfig {
        workers: Some(1),
        ..WorldgenSourceConfig::new(world, TEST_SEED)
    };
    let source: Arc<dyn WorldSource> =
        Arc::new(WorldgenSource::new(&config).expect("the streaming fixture source loads"));
    let world_rect = Some(Rect::from_dims(0, 0, 64, 64));
    let key = TileSubject::new(
        source.identity().stable_hash(),
        source.seed().generator(),
        Layer::CaveSlice,
        cave_slice_mode(64),
        ZoomLevel::MIN,
    )
    .key_at(0, 0);
    let renderer = CaveSliceTileRenderer::new(Arc::clone(&source), world_rect);

    let one_shot = TileRenderer::render(&renderer, &key, &cancel_token(), SummaryFidelity::Full)
        .expect("the one-shot slice renders");
    let mut published = Vec::new();
    let streamed = TileRenderer::render_streaming(
        &renderer,
        &key,
        &cancel_token(),
        SummaryFidelity::Full,
        &mut |tile| published.push(tile),
    )
    .expect("the streaming slice renders");

    assert!(!published.is_empty(), "streaming publishes each band");
    assert!(
        published.iter().all(|tile| !tile.complete),
        "every intermediate snapshot is incomplete"
    );
    assert!(streamed.complete, "the final streamed tile is complete");
    assert_eq!(streamed.rgba, one_shot.rgba, "final pixels are identical");

    // Density has the same band contract (and rides the summaries the slice
    // just warmed).
    let density_key = TileSubject::new(
        source.identity().stable_hash(),
        source.seed().generator(),
        Layer::CaveDensity,
        CAVE_DENSITY_MODE,
        ZoomLevel::MIN,
    )
    .key_at(0, 0);
    let density_renderer = CaveDensityTileRenderer::new(Arc::clone(&source), world_rect);
    let density_one_shot = TileRenderer::render(
        &density_renderer,
        &density_key,
        &cancel_token(),
        SummaryFidelity::Full,
    )
    .expect("the one-shot density renders");
    let mut density_published = Vec::new();
    let density_streamed = TileRenderer::render_streaming(
        &density_renderer,
        &density_key,
        &cancel_token(),
        SummaryFidelity::Full,
        &mut |tile| density_published.push(tile),
    )
    .expect("the streaming density renders");
    assert!(!density_published.is_empty());
    assert!(density_published.iter().all(|tile| !tile.complete));
    assert!(density_streamed.complete);
    assert_eq!(density_streamed.rgba, density_one_shot.rgba);
}

/// The colors the renderer uses for explicit cells/ramp anchors, so a
/// regression in the visual contract is visible in the `cave_render` gate.
#[test]
fn cave_render_colors_at_the_contract_anchors() {
    assert_eq!(
        cave_slice_color(SliceCell::Solid(BlockId(1)))[3],
        255,
        "solid is opaque"
    );
    assert_eq!(
        cave_slice_color(SliceCell::Fluid(FluidId(1)))[3],
        255,
        "fluid is opaque"
    );
    assert_eq!(cave_slice_color(SliceCell::Air)[3], 0, "air is transparent");
    assert_eq!(
        cave_density_color(0.0)[3],
        255,
        "the density ramp is opaque"
    );
    assert_ne!(
        cave_density_color(0.0),
        cave_density_color(1.0),
        "the ramp has distinct ends"
    );
}

/// The T28 benchmark, run manually in release:
///
/// ```sh
/// cargo test -p map-core --release --test cave_render \
///     -- --ignored cave_render_bench --nocapture
/// ```
///
/// Measures a 256-block region through the real tile renderers at steps 1
/// and 2. The step-2 tile covers the same 256 blocks with 128 samples per
/// axis; chunks (the dominant cost) are identical. Cold means a fresh source
/// with empty summary/slice caches; warm repeats the same request.
#[test]
#[ignore = "the T28 cave-tile benchmark — run manually in release (docs/benchmarks.md)"]
fn cave_render_bench_slice_and_density_tiles() {
    let Some(world) = world_dir() else {
        return;
    };
    let mut rows = Vec::new();
    for (mode, step) in [
        ("slice", 1_u32),
        ("slice", 2),
        ("density", 1),
        ("density", 2),
    ] {
        let config = WorldgenSourceConfig {
            workers: Some(2),
            ..WorldgenSourceConfig::new(world.clone(), TEST_SEED)
        };
        let source: Arc<dyn WorldSource> = match WorldgenSource::new(&config) {
            Ok(source) => Arc::new(source),
            Err(error) => {
                eprintln!("skipping the cave benchmark: the source did not load: {error}");
                return;
            }
        };
        let world_rect = Some(Rect::from_dims(0, 0, 256, 256));
        let zoom = ZoomLevel::from_step(step).expect("1 and 2 are in-range steps");
        let key = match mode {
            "slice" => TileSubject::new(
                source.identity().stable_hash(),
                source.seed().generator(),
                Layer::CaveSlice,
                cave_slice_mode(64),
                zoom,
            )
            .key_at(0, 0),
            "density" => TileSubject::new(
                source.identity().stable_hash(),
                source.seed().generator(),
                Layer::CaveDensity,
                CAVE_DENSITY_MODE,
                zoom,
            )
            .key_at(0, 0),
            _ => unreachable!("the benchmark's mode list is fixed"),
        };

        let cold_started = Instant::now();
        let cold_result = match mode {
            "slice" => TileRenderer::render(
                &CaveSliceTileRenderer::new(Arc::clone(&source), world_rect),
                &key,
                &cancel_token(),
                SummaryFidelity::Full,
            ),
            "density" => TileRenderer::render(
                &CaveDensityTileRenderer::new(Arc::clone(&source), world_rect),
                &key,
                &cancel_token(),
                SummaryFidelity::Full,
            ),
            _ => unreachable!("the benchmark's mode list is fixed"),
        };
        let cold = cold_started.elapsed().as_secs_f64();
        if let Err(error) = cold_result {
            eprintln!("skipping the cave benchmark: {mode} step {step} cold tile failed: {error}");
            return;
        }

        let warm_started = Instant::now();
        let warm_result = match mode {
            "slice" => TileRenderer::render(
                &CaveSliceTileRenderer::new(Arc::clone(&source), world_rect),
                &key,
                &cancel_token(),
                SummaryFidelity::Full,
            ),
            "density" => TileRenderer::render(
                &CaveDensityTileRenderer::new(Arc::clone(&source), world_rect),
                &key,
                &cancel_token(),
                SummaryFidelity::Full,
            ),
            _ => unreachable!("the benchmark's mode list is fixed"),
        };
        let warm = warm_started.elapsed().as_secs_f64();
        if let Err(error) = warm_result {
            eprintln!("skipping the cave benchmark: {mode} step {step} warm tile failed: {error}");
            return;
        }
        rows.push((mode, step, cold, warm));
    }

    eprintln!(
        "\nT28 cave tile benchmark — seed {TEST_SEED}, 256×256 blocks, 2 workers\n\n\
         | mode | step | samples | cold (s) | warm (s) |\n\
         |---|---|---:|---:|---:|"
    );
    for (mode, step, cold, warm) in rows {
        let samples = (256 / step) * (256 / step);
        eprintln!("| {mode} | {step} | {samples} | {cold:.2} | {warm:.3} |");
    }
}
