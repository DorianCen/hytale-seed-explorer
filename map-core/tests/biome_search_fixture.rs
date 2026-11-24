//! T25 fixture check and manual benchmark: the biome search against the
//! real generator.
//!
//! Both reuse the fixture-skip pattern from `tests/worldgen_source.rs`:
//! without the sibling generator checkout and its extracted asset pack they
//! print a skip message and pass (CI has no pack). The check proves the
//! refinement returns a true reading on the production source; the
//! benchmark measures the plan's first-result target at the standard
//! 10,000-block radius for a common and a rare biome (roles picked at
//! runtime from a probe histogram — the fixture's biome distribution is
//! world data, not a constant worth hardcoding).
//!
//! One shared [`WorldgenSource`] backs both; the biome search only issues
//! `biome_at`/`biome_region` queries, which the pool serves concurrently,
//! so no cancellation lock is needed (the T08 suite locks only because its
//! tests fire the source's own cancel token).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use map_core::scheduler::cancel_token;
use map_core::search::biome::{
    BiomeSearch, BiomeSearchEvent, DEFAULT_COARSE_STEP, run_biome_search, run_biome_search_blocking,
};
use map_core::source::WorldSource;
use map_core::source::worldgen::{WorldgenSource, WorldgenSourceConfig};
use map_core::types::{BiomeId, Rect};

/// The fixture seed, as the CLI would type it (matches `tests/features.rs`).
const FIXTURE_SEED: &str = "12345";

/// The world-gen folder this repository's sibling checkout ships for tests,
/// or `None` (with a skip message) on machines without it — including
/// machines without the user-supplied, git-ignored asset pack the committed
/// world's prefab references require (CI has no pack).
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

/// The shared, already-loaded production source behind an [`Arc`]: the
/// blocking check borrows it, the benchmark's spawned workers take clones
/// (and drop them on their own threads). Only call this after [`world_dir`]
/// returned [`Some`].
fn source() -> Arc<WorldgenSource> {
    static SOURCE: OnceLock<Arc<WorldgenSource>> = OnceLock::new();
    Arc::clone(SOURCE.get_or_init(|| {
        let world = world_dir().expect("callers check world_dir() first");
        let config = WorldgenSourceConfig::new(world, FIXTURE_SEED);
        Arc::new(WorldgenSource::new(&config).expect("the shared worldgen source loads"))
    }))
}

/// The acceptance check on the production source: searching for the biome
/// the generator itself reports at a known block must refine to a block
/// that really carries that biome, within a step of the center.
#[test]
fn biome_search_fixture_refines_a_true_reading() {
    if world_dir().is_none() {
        return; // skip message already printed by the helper
    }
    let source = source();

    let center = (3_i32, 7_i32);
    let target = source
        .biome_at(center.0, center.1)
        .expect("the fixture serves biome_at");
    let search = BiomeSearch {
        biome: target,
        center: (f64::from(center.0) + 0.5, f64::from(center.1) + 0.5),
        max_radius: 128.0,
        max_results: 1,
        min_patch_blocks: None,
        coarse_step: DEFAULT_COARSE_STEP,
    };

    let mut results = Vec::new();
    let mut done = None;
    run_biome_search_blocking(&*source, &search, &cancel_token(), |event| {
        match event {
            BiomeSearchEvent::Result(result) => results.push(result),
            BiomeSearchEvent::Done(stats) => done = Some(stats),
            _ => {}
        }
        true
    });
    let done = done.expect("a completed search sends Done");

    assert_eq!(done.results, results.len());
    assert!(
        done.results >= 1,
        "the biome at the center must be found around the center",
    );
    let result = results[0];
    assert_eq!(
        source.biome_at(result.position.0, result.position.1),
        Ok(target),
        "the refined position is a true production reading, not an artifact",
    );
    assert!(
        result.distance < f64::from(DEFAULT_COARSE_STEP),
        "the center's own biome is a few blocks away, got {}",
        result.distance,
    );
}

/// The T25 benchmark, run manually in release (the generator's query cost
/// makes a debug run useless — see `docs/benchmarks.md`):
///
/// ```sh
/// cargo test -p map-core --release --test biome_search_fixture -- \
///     --ignored biome_search_bench --nocapture
/// ```
///
/// It measures the plan's search target — first results within ~500 ms —
/// for a common and a rare biome at the standard 10,000-block radius, and
/// prints a Markdown table ready for `docs/benchmarks.md`. The roles come
/// from a ±8,192-block probe histogram at step 64 (65,536 samples): the
/// fixture's biome distribution is world data, so the bench adapts to it
/// instead of hardcoding ids.
#[test]
#[ignore = "the T25 biome-search benchmark — run manually in release (docs/benchmarks.md)"]
fn biome_search_bench_10k_radius() {
    if world_dir().is_none() {
        return; // skip message already printed by the helper
    }
    let source: Arc<dyn WorldSource> = source();

    // Probe: which biomes are common and which are rare around the origin?
    let probe = source
        .biome_region(Rect::from_dims(-8192, -8192, 16384, 16384), 64)
        .expect("the probe grid serves");
    let mut histogram: HashMap<BiomeId, u64> = HashMap::new();
    for biome in &probe.biomes {
        *histogram.entry(*biome).or_default() += 1;
    }
    let mut ranked: Vec<(BiomeId, u64)> = histogram.into_iter().collect();
    ranked.sort_by_key(|&(biome, count)| (count, biome.0));
    eprintln!(
        "probe over ±8192 at step 64 ({} samples), {} distinct biomes",
        probe.biomes.len(),
        ranked.len()
    );
    for (biome, count) in &ranked {
        let name = biome_name(&*source, *biome);
        eprintln!("  id {:>4} {:<24} {:>6} samples", biome.0, name, count);
    }
    // Rare = least frequent (ties break on id); common = most frequent.
    let rare = ranked[0].0;
    let common = ranked[ranked.len() - 1].0;

    eprintln!("| Biome | Role | First result | Results | Coarse samples | Total |");
    eprintln!("|---|---|---|---|---|---|");
    for (biome, role) in [(common, "common"), (rare, "rare")] {
        let name = biome_name(&*source, biome);
        let search = BiomeSearch {
            biome,
            center: (0.0, 0.0),
            max_radius: 10_000.0,
            max_results: 0,
            min_patch_blocks: None,
            coarse_step: DEFAULT_COARSE_STEP,
        };
        let handle = run_biome_search(Arc::clone(&source), search);
        let started = Instant::now();
        let mut first: Option<Duration> = None;
        let stats = loop {
            let event = handle
                .events()
                .recv_timeout(Duration::from_secs(600))
                .expect("the bench must keep producing events");
            match event {
                BiomeSearchEvent::Result(_) => {
                    if first.is_none() {
                        first = Some(started.elapsed());
                    }
                }
                BiomeSearchEvent::Done(stats) => break stats,
                _ => {}
            }
        };
        eprintln!(
            "| {} ({}) | {role} | {} | {} | {} | {:.1?} |",
            biome.0,
            name,
            first.map_or("n/a".to_string(), |duration| format!("{duration:.1?}")),
            stats.results,
            stats.samples,
            stats.elapsed,
        );
    }
}

/// The catalog name of a biome id, for the bench table.
fn biome_name(source: &dyn WorldSource, biome: BiomeId) -> String {
    source
        .biome_catalog()
        .get(biome)
        .map(|info| info.name.clone())
        .unwrap_or_else(|| "<unknown>".to_string())
}
