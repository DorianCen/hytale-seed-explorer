//! T01 audit probe — first-order timings of the cheap column queries and the
//! per-seed feature lists the seed explorer depends on.
//!
//! This file is preserved for reproducibility of the numbers in
//! `docs/worldgen-api-audit.md` §7 and as the starting point for the T04
//! harness. It is a **standalone script, not an explorer dependency**: it must
//! run inside the `hytale-server-rust` workspace, which owns the crates it
//! imports.
//!
//! How to run: copy this file to
//! `hytale-server-rust/crates/worldgen-cli/examples/query_bench.rs`, then from
//! the workspace root:
//!
//! ```sh
//! cargo run --release -p worldgen-cli --example query_bench -- \
//!   testdata/Default 0
//! ```
//!
//! Remove the example afterwards so the worldgen working tree stays clean
//! (T04 decides whether its permanent harness is committed there or driven
//! from this repo).

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

use worldgen_hytale::loader::chunk_generator_json_loader::ChunkGeneratorJsonLoader;
use worldgen_hytale::world_gen_config::WorldGenConfig;
use worldgen_hytale::world_gen_provider::{HytaleWorldGenProvider, Semver};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let world = args
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("testdata/Default"));
    let seed: i32 = args.get(2).and_then(|value| value.parse().ok()).unwrap_or(0);

    let provider = HytaleWorldGenProvider {
        name: "Default".to_string(),
        version: Semver::parse("0.0.0").expect("0.0.0 is valid semver"),
        path: None,
    };
    let path = provider
        .resolve_path(&world)
        .expect("the world path resolves like the CLI default");
    let config = WorldGenConfig::new(path, "Default".to_string(), "0.0.0".to_string());
    let loader = ChunkGeneratorJsonLoader::new(
        procedural::json::SeedString::new("ChunkGenerator"),
        config,
    );

    let started = Instant::now();
    let generator = loader.load();
    println!("load_seconds {:.6}", started.elapsed().as_secs_f64());

    // Warm-up away from the measured region so (0,0)..(512,512) stays cold:
    // 64x64 samples at step 8 around (4096, 4096).
    for z in 0..64 {
        for x in 0..64 {
            let wx = 4096 + x * 8;
            let wz = 4096 + z * 8;
            generator.get_zone_biome_result_at(seed, wx, wz);
            generator.get_height(seed, wx, wz);
        }
    }

    // 65,536 biome samples: one 256x256 tile at step 2 over a 512x512 region.
    let mut names: HashSet<String> = HashSet::new();
    let mut heights: Vec<i32> = Vec::with_capacity(256);
    let cold = Instant::now();
    for z in 0..256 {
        for x in 0..256 {
            let result = generator.get_zone_biome_result_at(seed, x * 2, z * 2);
            if let Some(biome) = result.get_biome() {
                names.insert(biome.get_name().to_string());
            }
        }
    }
    println!("biome_step2_cold_seconds {:.6}", cold.elapsed().as_secs_f64());
    let warm = Instant::now();
    for z in 0..256 {
        for x in 0..256 {
            let result = generator.get_zone_biome_result_at(seed, x * 2, z * 2);
            if let Some(biome) = result.get_biome() {
                heights.push(biome.get_id());
            }
        }
    }
    println!("biome_step2_warm_seconds {:.6}", warm.elapsed().as_secs_f64());
    println!("biome_step2_distinct_biomes {}", names.len());

    // 65,536 height samples on the same region. The core-data entries are
    // already warm from the biome loop, so run 1 isolates the height-
    // generation cost and run 2 is the cached cost.
    let first = Instant::now();
    for z in 0..256 {
        for x in 0..256 {
            heights.push(generator.get_height(seed, x * 2, z * 2));
        }
    }
    println!("height_step2_first_seconds {:.6}", first.elapsed().as_secs_f64());
    let warm = Instant::now();
    for z in 0..256 {
        for x in 0..256 {
            heights.push(generator.get_height(seed, x * 2, z * 2));
        }
    }
    println!("height_step2_warm_seconds {:.6}", warm.elapsed().as_secs_f64());

    // 65,536 combined biome+height samples at step 1 over 256x256 blocks
    // (the per-column cost a full-block tile would pay before chunk caching).
    let cold = Instant::now();
    for z in 0..256 {
        for x in 0..256 {
            generator.get_zone_biome_result_at(seed, x, z);
            heights.push(generator.get_height(seed, x, z));
        }
    }
    println!(
        "biome_height_step1_cold_seconds {:.6}",
        cold.elapsed().as_secs_f64()
    );
    let warm = Instant::now();
    for z in 0..256 {
        for x in 0..256 {
            generator.get_zone_biome_result_at(seed, x, z);
            heights.push(generator.get_height(seed, x, z));
        }
    }
    println!(
        "biome_height_step1_warm_seconds {:.6}",
        warm.elapsed().as_secs_f64()
    );

    // Fit-regime measurements: a 128x128-tile region at step 2 is 16,384
    // samples, well under the 50,000-entry generator cache, so warm runs
    // actually hit the cache (unlike the 65,536-sample region above, which
    // exceeds the cap and thrashes it).
    let (ox, oz) = (2048, 2048);
    let cold = Instant::now();
    for z in 0..128 {
        for x in 0..128 {
            let result = generator.get_zone_biome_result_at(seed, ox + x * 2, oz + z * 2);
            if let Some(biome) = result.get_biome() {
                heights.push(biome.get_id());
            }
        }
    }
    println!(
        "fit_biome_step2_cold_seconds {:.6}",
        cold.elapsed().as_secs_f64()
    );
    let warm = Instant::now();
    for z in 0..128 {
        for x in 0..128 {
            generator.get_zone_biome_result_at(seed, ox + x * 2, oz + z * 2);
        }
    }
    println!(
        "fit_biome_step2_warm_seconds {:.6}",
        warm.elapsed().as_secs_f64()
    );
    let first = Instant::now();
    for z in 0..128 {
        for x in 0..128 {
            heights.push(generator.get_height(seed, ox + x * 2, oz + z * 2));
        }
    }
    println!(
        "fit_height_step2_first_seconds {:.6}",
        first.elapsed().as_secs_f64()
    );
    let warm = Instant::now();
    for z in 0..128 {
        for x in 0..128 {
            heights.push(generator.get_height(seed, ox + x * 2, oz + z * 2));
        }
    }
    println!(
        "fit_height_step2_warm_seconds {:.6}",
        warm.elapsed().as_secs_f64()
    );

    // Per-seed unique prefab list (candidate catalogue for feature search).
    let cold = Instant::now();
    let entries = generator.get_unique_prefabs(seed);
    println!(
        "unique_prefabs_cold_seconds {:.6} count {} show_on_map {} spawn_locations {}",
        cold.elapsed().as_secs_f64(),
        entries.len(),
        entries.iter().filter(|entry| entry.is_show_on_map()).count(),
        entries.iter().filter(|entry| entry.is_spawn_location()).count()
    );
    let warm = Instant::now();
    let again = generator.get_unique_prefabs(seed);
    println!(
        "unique_prefabs_warm_seconds {:.6} count {}",
        warm.elapsed().as_secs_f64(),
        again.len()
    );

    // Spawn points (the spawn marker).
    let cold = Instant::now();
    let spawn_points = generator.get_spawn_points(seed);
    println!(
        "spawn_points_first_seconds {:.6} count {}",
        cold.elapsed().as_secs_f64(),
        spawn_points.len()
    );
    let warm = Instant::now();
    let spawn_points_again = generator.get_spawn_points(seed);
    println!(
        "spawn_points_warm_seconds {:.6} count {}",
        warm.elapsed().as_secs_f64(),
        spawn_points_again.len()
    );

    // Keep the sums alive so the loops are not optimized away.
    let checksum: i64 = heights.iter().map(|value| i64::from(*value)).sum();
    println!("probe_checksum {checksum}");
}
