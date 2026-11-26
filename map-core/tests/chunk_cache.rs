//! T09 acceptance tests: the chunk-summary and slice caches must coalesce
//! concurrent loads, replay cached regions identically, stay correct under a
//! tiny budget, and never key a summary across a seed change.
//!
//! The real generator checkout lives next to this repository (see
//! [`world_dir`]); when it is absent every test prints a skip message and
//! returns. The shared [`source`] (2 workers, seed `12345`) serves the
//! comparison queries; tests that need an isolated cache build their own local
//! source (each worker costs ~1 s of startup, one chunk ~0.45 s).

use std::path::{Path, PathBuf};
use std::sync::{Barrier, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use map_core::source::worldgen::{WorldgenSource, WorldgenSourceConfig};
use map_core::source::{SummaryFidelity, WorldSource};
use map_core::types::Rect;

/// Seed used by the shared source (T02 semantics: plain decimal text).
const TEST_SEED: &str = "12345";
/// A different numeric seed for the cache-key isolation test.
const OTHER_SEED: &str = "12346";

/// The world-gen folder this repository's sibling checkout ships for tests, or
/// `None` (with a skip message) on machines without the checkout.
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

/// Builds a private source (its own pool and its own caches), so it cannot
/// share in-flight loads or evictions with other tests.
fn local_source(
    seed: &str,
    workers: usize,
    summary_budget_bytes: Option<u64>,
    slice_cache_entries: Option<u64>,
) -> WorldgenSource {
    let world = world_dir().expect("the worldgen checkout is present (checked by the caller)");
    let config = WorldgenSourceConfig {
        workers: Some(workers),
        summary_budget_bytes,
        slice_cache_entries,
        ..WorldgenSourceConfig::new(world, seed)
    };
    WorldgenSource::new(&config).expect("the local worldgen source loads")
}

/// Proves the T09 acceptance bullet: simultaneous requests for the same rect
/// share the in-flight chunk loads (moka coalescing) and all observe the same
/// samples.
#[test]
fn concurrent_requests_for_the_same_rect_coalesce_and_agree() {
    let Some(_world) = world_dir() else {
        return;
    };
    let source = source();

    // 2×1 chunks, so every thread needs the same two summary loads.
    let rect = Rect::from_dims(0, 0, 64, 32);
    let barrier = Barrier::new(4);
    let started = Instant::now();
    let results = thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    source
                        .surface_region(rect, 1, SummaryFidelity::Full)
                        .expect("a concurrent surface region")
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("a request thread does not panic"))
            .collect::<Vec<_>>()
    });
    eprintln!(
        "four coalesced 2x1-chunk surface requests took {:?}",
        started.elapsed()
    );

    assert_eq!(results.len(), 4, "every request thread returned a grid");
    let (width, height) = rect.sample_dims(1);
    for (index, grid) in results.iter().enumerate() {
        assert_eq!(
            (grid.width, grid.height, grid.step),
            (width, height, 1),
            "request {index}: the grid covers the requested rect"
        );
        assert_eq!(
            grid, &results[0],
            "request {index}: concurrent requests must observe identical samples"
        );
    }
    assert!(
        !results[0].samples.is_empty(),
        "the coalesced region is not empty"
    );
}

/// Proves the T09 acceptance bullet: a repeated region request hits the warm
/// summary cache and returns exactly the same samples.
#[test]
fn cached_regions_are_fast_and_identical() {
    let Some(_world) = world_dir() else {
        return;
    };
    // A private source: its idle pool keeps the cache-hit timing meaningful.
    let local = local_source(TEST_SEED, 1, None, None);

    let rect = Rect::from_dims(0, 0, 32, 32);
    let first = local
        .surface_region(rect, 1, SummaryFidelity::Full)
        .expect("the first surface region generates the chunk");
    let started = Instant::now();
    let second = local
        .surface_region(rect, 1, SummaryFidelity::Full)
        .expect("the second surface region is served from cache");
    let elapsed = started.elapsed();
    eprintln!("cached 1x1-chunk surface region took {elapsed:?}");

    assert_eq!(
        first, second,
        "the cached region must be identical to the generated one"
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "a cache hit must not pay for chunk generation again (took {elapsed:?})"
    );
}

/// Proves the T09 acceptance bullet: summaries are keyed by seed, so a second
/// source with another seed never reuses the first seed's terrain.
#[test]
fn a_seed_change_does_not_reuse_summaries() {
    let Some(_world) = world_dir() else {
        return;
    };
    let shared = source();
    let other = local_source(OTHER_SEED, 1, None, None);

    let rect = Rect::from_dims(0, 0, 32, 32);
    let first = shared
        .surface_region(rect, 1, SummaryFidelity::Full)
        .expect("seed 12345 serves the region");
    let second = other
        .surface_region(rect, 1, SummaryFidelity::Full)
        .expect("seed 12346 serves the region");

    let differing = first
        .samples
        .iter()
        .zip(&second.samples)
        .filter(|(left, right)| left != right)
        .count();
    assert!(
        differing > 0,
        "seeds 12345 and 12346 produced identical terrain over a 32x32 grid"
    );
    eprintln!(
        "{differing} of {} samples differ between the two seeds",
        first.samples.len()
    );

    // Both sources stay self-consistent on a repeated query.
    assert_eq!(
        first,
        shared
            .surface_region(rect, 1, SummaryFidelity::Full)
            .expect("seed 12345 repeats the region"),
        "the shared source must repeat its own region"
    );
    assert_eq!(
        second,
        other
            .surface_region(rect, 1, SummaryFidelity::Full)
            .expect("seed 12346 repeats the region"),
        "the second source must repeat its own region"
    );
}

/// Proves the T09 acceptance bullet: a cache budget barely large enough for a
/// single summary still serves correct surface and slice data.
#[test]
fn a_tiny_cache_budget_still_returns_correct_data() {
    let Some(_world) = world_dir() else {
        return;
    };
    let normal = source();
    // One summary is ~22 KB: 30 000 bytes holds one, never two; the slice cache
    // holds two (chunk, y) entries.
    let tiny = local_source(TEST_SEED, 1, Some(30_000), Some(2));

    for (x, z) in [(0, 0), (7, 13), (31, 31), (5, 20)] {
        let from_tiny = tiny
            .surface_at(x, z)
            .unwrap_or_else(|error| panic!("tiny-budget surface_at({x}, {z}) failed: {error}"));
        let from_normal = normal
            .surface_at(x, z)
            .unwrap_or_else(|error| panic!("normal surface_at({x}, {z}) failed: {error}"));
        assert_eq!(
            from_tiny, from_normal,
            "tiny-budget surface_at({x}, {z}) must match a normally budgeted source"
        );
    }

    let rect = Rect::from_dims(0, 0, 32, 32);
    for y in [64, 65] {
        let from_tiny = tiny
            .slice_region(rect, 1, y)
            .unwrap_or_else(|error| panic!("tiny-budget slice at y={y} failed: {error}"));
        let from_normal = normal
            .slice_region(rect, 1, y)
            .unwrap_or_else(|error| panic!("normal slice at y={y} failed: {error}"));
        assert_eq!(
            (from_tiny.y, from_tiny.cells),
            (from_normal.y, from_normal.cells),
            "tiny-budget slice at y={y} must match a normally budgeted source"
        );
    }
}

/// Proves the T09 acceptance bullet: a source with warm summary and slice
/// caches shuts its pool down cleanly instead of hanging on drop.
#[test]
fn dropping_a_source_with_warm_caches_does_not_hang() {
    let Some(_world) = world_dir() else {
        return;
    };
    {
        let local = local_source(TEST_SEED, 1, None, None);
        local
            .surface_at(0, 0)
            .expect("the local source warms one summary");
        local
            .slice_region(Rect::from_dims(0, 0, 32, 32), 1, 64)
            .expect("the local source warms one slice");
    }
    // Reaching this line is the assertion: the pool threads joined in `Drop`.
    eprintln!("a source with warm caches dropped without hanging");
}

/// Proves the T09 acceptance bullet: slices are cached per `(chunk, y)`, so a
/// repeated identical slice request does not regenerate the chunk. A slice
/// miss also inserts the full-fidelity summary extracted from the same
/// generation, so a later density/surface request over that chunk is warm.
#[test]
fn slice_requests_are_cached_by_chunk_and_y() {
    let Some(_world) = world_dir() else {
        return;
    };
    let local = local_source(TEST_SEED, 1, None, None);

    let rect = Rect::from_dims(0, 0, 32, 32);
    let y = 64;
    let first = local
        .slice_region(rect, 1, y)
        .expect("the first slice request generates the chunk");
    assert_eq!(first.y, y, "the grid reports the requested world height");

    let started = Instant::now();
    let second = local
        .slice_region(rect, 1, y)
        .expect("the second slice request is served from cache");
    let elapsed = started.elapsed();
    eprintln!("cached 1x1-chunk slice took {elapsed:?}");

    assert_eq!(
        first, second,
        "the cached slice must be identical to the generated one"
    );
    assert!(
        elapsed < Duration::from_millis(200),
        "a slice cache hit must not pay for chunk generation again (took {elapsed:?})"
    );
}

/// T29 fixture check: a surface-only summary really is generated by the
/// skipped-cave/prefab mode, and the fidelity namespacing keeps it from
/// satisfying a later full request on the same chunk.
///
/// The sibling generator's `tests/surface_only.rs` measured 177 differing
/// top-block columns on `testdata/Default`, seed 0, chunk (0, 0) (a biome
/// prefab touches the surface). Asking the explorer for the same chunk in
/// both fidelities must reproduce that difference — and because the
/// surface-only request is asked first, a zero difference would also mean
/// the full request hit the surface-only cache entry.
#[test]
fn fidelity_surface_only_summary_differs_from_full_on_a_prefab_chunk() {
    let Some(_world) = world_dir() else {
        return;
    };
    let local = local_source("0", 1, None, None);
    let rect = Rect::new(0, 0, 31, 31);

    let surface_only = local
        .surface_region(rect, 1, SummaryFidelity::SurfaceOnly)
        .expect("a surface-only chunk summary");
    let full = local
        .surface_region(rect, 1, SummaryFidelity::Full)
        .expect("a full chunk summary");

    assert_eq!(
        (surface_only.width, surface_only.height),
        (full.width, full.height),
        "both fidelities cover the same grid"
    );
    let mut block_differences = 0usize;
    let mut height_differences = 0usize;
    for (surface, full) in surface_only.samples.iter().zip(&full.samples) {
        block_differences += usize::from(surface.block != full.block);
        height_differences += usize::from(surface.height != full.height);
    }
    eprintln!(
        "fidelity on Default seed 0 chunk (0, 0): {block_differences}/1024 top blocks and \
         {height_differences}/1024 heights differ between surface-only and full"
    );
    assert!(
        block_differences > 0,
        "the fixture chunk carries a surface prefab, so surface-only must differ from full \
         (a zero count means the fidelity key leaked or the generator ignored the flag)"
    );
}
