//! `pool_bench` — manual T08 probe: region queries through the worker pool.
//!
//! Fills the "parallel banded column queries" gap in the explorer's
//! `docs/benchmarks.md` (the T04 probe was single-worker by design). Not
//! part of `cargo test`; run by hand:
//!
//! ```sh
//! cargo run --release -p map-core --example pool_bench -- \
//!   ../hytale-server-rust/testdata/Default 12345 2
//! ```
//!
//! Args: `[world-dir] [seed] [workers]` (defaults: the golden world, seed
//! 12345, 2 workers). Times one 256×256-at-step-2 biome region (65,536
//! samples) and one 64×64-at-step-1 height region (4,096 samples) through
//! `WorldSource`, printing `key value` lines suitable for the regression
//! table.

use std::path::PathBuf;
use std::time::Instant;

use map_core::source::WorldSource;
use map_core::source::worldgen::{WorldgenSource, WorldgenSourceConfig};
use map_core::types::Rect;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let world = args
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("../hytale-server-rust/testdata/Default"));
    let seed = args.get(2).cloned().unwrap_or_else(|| "12345".to_string());
    let workers: usize = args
        .get(3)
        .and_then(|value| value.parse().ok())
        .unwrap_or(2);

    let mut config = WorldgenSourceConfig::new(&world, &seed);
    config.workers = Some(workers);

    let started = Instant::now();
    let source = WorldgenSource::new(&config).expect("the pool starts");
    println!(
        "pool_startup_seconds {:.6}",
        started.elapsed().as_secs_f64()
    );
    println!("workers {}", source.worker_count());

    let biome_rect = Rect::from_dims(-512, -512, 512, 512);
    let started = Instant::now();
    let grid = source
        .biome_region(biome_rect, 2)
        .expect("the biome region succeeds");
    println!(
        "biome_65536_pool_seconds {:.6} ({}x{} samples)",
        started.elapsed().as_secs_f64(),
        grid.width,
        grid.height
    );

    let height_rect = Rect::from_dims(0, 0, 64, 64);
    let started = Instant::now();
    let grid = source
        .height_region(height_rect, 1)
        .expect("the height region succeeds");
    println!(
        "height_4096_pool_seconds {:.6} ({}x{} samples)",
        started.elapsed().as_secs_f64(),
        grid.width,
        grid.height
    );

    // T09: the chunk path — surface region + slice through the summary caches.
    let surface_rect = Rect::from_dims(0, 0, 256, 256);
    let started = Instant::now();
    let grid = source
        .surface_region(surface_rect, 1)
        .expect("the surface region succeeds");
    println!(
        "surface_65536_pool_seconds {:.6} ({}x{} samples)",
        started.elapsed().as_secs_f64(),
        grid.width,
        grid.height
    );
    let started = Instant::now();
    let slice = source
        .slice_region(Rect::from_dims(0, 0, 64, 64), 1, 64)
        .expect("the slice succeeds");
    println!(
        "slice_4096_pool_seconds {:.6} ({}x{} cells at y={})",
        started.elapsed().as_secs_f64(),
        slice.width,
        slice.height,
        slice.y
    );
    let started = Instant::now();
    let _ = source.surface_region(surface_rect, 2).expect("cached");
    println!(
        "surface_16384_cached_step2_seconds {:.6}",
        started.elapsed().as_secs_f64()
    );
    // Cancellation round-trip, to prove the pool stays healthy after one.
    source.cancel_pending();
    match source.biome_region(biome_rect, 2) {
        Ok(grid) => println!("post_cancel_biome_samples {}", grid.biomes.len()),
        Err(error) => println!("post_cancel_biome_error {error}"),
    }
}
