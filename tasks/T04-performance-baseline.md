# T04 — Performance baseline harness

**Status:** done (deliverable: [`docs/benchmarks.md`](../docs/benchmarks.md);
runner: [`tools/bench-worldgen.sh`](../tools/bench-worldgen.sh); probe
committed as `worldgen-cli`'s `query_bench` binary, worldgen repo commit
`9b03190`)
**Milestone:** 0 — API audit
**Depends on:** —
**Blocks:** T33
**Plan refs:** [§9](../hytale-seed-map-plan.md#9-performance-correctness-and-testing), [§11.2](../hytale-seed-map-plan.md#11-next-actions)
**Agent fit:** medium

## Goal

Record a reproducible release-mode baseline for the three numbers the plan
asks for: time per column, time per 256×256 region at step 2, and time to
enumerate features in a 10,000×10,000 area. The baseline drives the worker
count, cache budget and progressive-refinement design; later tasks must be
able to detect regressions against it.

## Context

The generator already has:

- `worldgen-cli bench` for chunk-grid throughput with per-stage timings;
- `cargo bench -p worldgen-hytale` for the moka cache and prefab-weight
  microbenchmarks;
- archived baseline numbers in
  `done/world-generation/BENCHMARKS.md` (release, Ryzen 7 9800X3D, seed 0,
  `testdata/Default` + `Assets/extracted`);
- `get_height` and `get_zone_biome_result_at` column queries that do not
  generate chunks;
- no public feature-enumeration API yet (T22 adds it).

The explorer workspace does not exist yet. This task may add a small
standalone probe in the `hytale-server-rust` `tools/` directory or a script
that drives the existing CLI; do not create a dependency from the explorer to
the probe.

## Scope

### In

- A repeatable command/script that measures:
  - one column: `get_zone_biome_result_at` and `get_height`;
  - a 512×512-block region at step 2 (65,536 samples);
  - a 256×256-block region at step 1, including chunk generation and surface
    extraction (the full-block cost);
  - unique-prefab enumeration for one seed;
  - feature enumeration over 10,000×10,000 blocks once T22 exists;
  - chunk-generation throughput at 1 and N workers (reuse `worldgen-cli bench`).
- Warmup, release build, hardware/toolchain recording, median over at least 3
  runs.
- A first-pass answer to: "How many chunks and how many seconds for a full
  block tile?"
- A regression table format that later tasks append to.

### Out

- Optimizing the generator.
- Adding the explorer criterion benches (T33) or the UI frame profiler.
- Benchmarking the real game. Use the same asset pack and seed for any
  comparison.

## Deliverable

`docs/benchmarks.md` in the explorer repo (created by this task) with:

- hardware, OS, rustc/cargo, build profile, asset pack and world path;
- exact commands;
- a results table per metric with median and p95 where practical;
- a "what this means for the explorer" section (worker count, cache budget,
  first-tile strategy);
- a "known gaps" section (feature enumeration pending T22, full block pending
  T09/T21).

Optionally `tools/bench-worldgen.sh` in the explorer repo that wraps the
commands; it must be portable enough for CI or clearly documented as manual.

## Work

1. Build release binaries:
   ```sh
   cd /home/chome/Projects/hytale-server-rust
   cargo build --release -p worldgen-cli -p worldgen-hytale
   ```
2. Measure chunk throughput with the existing CLI. Use at least 32 chunks per
   worker to avoid cold-cache distortion (see the archived benchmark notes):
   ```sh
   ./target/release/worldgen-cli bench --world testdata/Default --seed 0 \
     --start 0,0 --chunks 576
   ./target/release/worldgen-cli bench --world testdata/Default --seed 0 \
     --start 0,0 --chunks 576 --workers 1
   ```
3. Add a small probe (for example
   `crates/worldgen-cli/src/bin/query_bench.rs`, or a temporary example that is
   not committed if the task owner prefers) that:
   - loads one generator;
   - warms `get_height` and `get_zone_biome_result_at` on a 64×64 grid;
   - times 65,536 biome samples and 65,536 height samples;
   - times the 512×512 step-2 region;
   - times the 256×256 step-1 region using the chunk path and reports the
     number of chunks generated and the surface extraction cost;
   - times `get_unique_prefabs(seed)` and `get_spawn_points(seed)`.
   Keep the probe out of the default test suite.
4. Run each measurement at least 3 times. Record median and p95. Report the
   wall time dived into per-sample/per-chunk cost.
5. Add the feature-enumeration row as a placeholder now; T22 fills it in. If
   T22 has landed, measure a 10,000×10,000 rect and a few smaller rects.
6. Write the "what this means" section:
   - if column sampling is fast enough for a 256×256 step-2 tile within one
     worker, tile work can use a small pool;
   - if full block is dominated by chunk generation, keep the compact summary
     cache (T09) and progressive refinement (T21);
   - if first-tile latency is too high, mandate a coarse placeholder step
     (T15/T16).
7. Add the regression table header and the exact rerun commands to the doc.

## Acceptance

- `docs/benchmarks.md` exists with release-mode numbers, commands, hardware and
  the interpretation.
- All three plan §11.2 numbers are either measured or explicitly marked
  "blocked on T22" with the substitute measurement.
- The doc states the recommended worker count and the 512 MB cache implication
  in concrete terms.
- No default test or build depends on a multi-gigabyte asset pack.

## Tests

```sh
cargo build --release -p worldgen-cli
./target/release/worldgen-cli bench --world testdata/Default --seed 0 --chunks 144
cargo bench -p worldgen-hytale
```

The probe must be runnable by hand and must not be part of `cargo test`.

## Risks / notes

- Debug builds are roughly an order of magnitude slower on prefab-heavy
  chunks. Never publish debug numbers as a baseline.
- Small chunk counts are dominated by per-worker cache warm-up; document the
  warm-up rule.
- If the asset pack is missing, use `testdata/Default` but say so; do not
  compare numbers from different packs.
