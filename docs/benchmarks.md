# Performance baseline (T04)

**Task:** [T04](../tasks/T04-performance-baseline.md) — status: done.
**Purpose:** the reproducible release-mode baseline for every design number the
explorer plan makes: column cost, tile cost, chunk throughput, feature-list
cost. Later tasks (T08, T09, T15, T21, T33) append regressions to the table in
§7 and compare against this first row.

## Environment

| Item | Value |
|---|---|
| CPU | AMD Ryzen 7 9800X3D, 8 cores / 16 threads |
| RAM | 63.4 GiB |
| OS | Linux 7.3.5 (CachyOS) |
| Toolchain | rustc/cargo 1.99.1 |
| Profile | `--release` (debug is **never** a baseline; the same 144-chunk acceptance grid measured 193.1 s in debug vs 24.6 s in release — ≈8× slower) |
| World | `testdata/Default` (the committed golden world copy) |
| Asset pack | **none** — the golden world and default registry resolution only. Numbers from a different world folder or a full `Assets/extracted` pack are not comparable. |
| Seed | 0 |
| Harness | [`tools/bench-worldgen.sh`](../tools/bench-worldgen.sh) wraps every command below; the column/chunk probe is the committed worldgen binary `query_bench` (`crates/worldgen-cli/src/bin/query_bench.rs`, worldgen repo commit `9b03190`) |

Warm-up rules (from the archived `done/world-generation/BENCHMARKS.md` and
T01's audit):

- The probe warms column queries on a disjoint 64×64 grid; the measured region
  starts cold. Runs 2..N are steady-state (the 65,536-entry region exceeds the
  generator's 50,000-entry column cache, so every run pays eviction churn —
  that is the honest steady-state number; see the T01 audit §4).
- The chunk path creates a fresh `WorldGenService` per run so every run pays
  the same cold-cache cost.
- `worldgen-cli bench` grids use ≥32 chunks per worker where noted; smaller
  grids are dominated by per-worker cache warm-up.
- 5 runs per probe metric; median and p95 reported (with n=5, p95 = the worst
  run). 1 worker for the probe's chunk path (parallel scaling is measured
  separately by `bench`).

## Commands (exact, rerunnable)

```sh
cd /home/chome/Projects/hytale-server-rust
cargo build --release -p worldgen-cli -p worldgen-hytale

# column queries, per-seed lists, chunk path (median/p95 over 5 runs)
./target/release/query_bench testdata/Default 0 5 1

# chunk-grid throughput (default worker pool = 12 on this machine)
./target/release/worldgen-cli bench --world testdata/Default --seed 0 --start 0,0 --chunks 576
./target/release/worldgen-cli bench --world testdata/Default --seed 0 --start 0,0 --chunks 576 --workers 1

# acceptance grid (T04 tests section)
./target/release/worldgen-cli bench --world testdata/Default --seed 0 --chunks 144

# criterion microbenchmarks (optional, slow)
cargo bench -p worldgen-hytale
```

## Column-query and region costs (probe, 5 runs, median / p95)

| Metric | Median | p95 | Per sample (from median) |
|---|---:|---:|---:|
| Generator load (one instance) | 0.896 s | — | — |
| 65,536 biome samples (a 512×512-block tile at step 2) | **2.332 s** | 2.362 s | ~35.6 µs |
| 65,536 height samples (same region, core-data warm) | **70.634 s** | 70.888 s | ~1.08 ms |
| 65,536 biome+height samples at step 1 (256×256 blocks) | **25.987 s** | 26.175 s | ~397 µs |
| `get_unique_prefabs(0)` (2 entries) | 3 µs (cached) | 26.2 ms (cold) | — |
| `get_spawn_points(0)` (1 point) | 6 µs | 7 µs | — |

## Chunk-path region cost (probe, 1 worker, 5 runs)

The 256×256-block region at step 1 = 64 chunks, generated through the worker
pool with a compact surface extraction per chunk (top-block scan + top-fluid
scan + tint lookup — the extraction T09 will perform):

| Metric | Median | p95 |
|---|---:|---:|
| Generation (64 chunks, cold service per run) | **21.139 s** | 21.257 s |
| Surface extraction (64 chunks) | **0.055 s** | 0.056 s |
| Pipeline average per chunk (StageTimings total) | 0.306 s | 0.308 s |
| Region total (generation + extraction) | ~21.2 s | — |

Extraction is ~0.9 ms per chunk (~0.9 µs per column): **negligible next to
generation**, confirming T09's compact-summary design (a summary costs ~22 KB
per chunk versus ~6.6 MiB for a retained `GeneratedChunk`).

## Chunk-grid throughput (worker pool, release)

| Grid | Workers | Wall | Chunks/s | Avg total (s/chunk) | Caves (s) | Prefabs (s) |
|---|---:|---:|---:|---:|---:|---:|
| 576 chunks | 12 (default) | **69.77 s** | **8.26** | 0.111 | 0.057 | 0.042 |
| 576 chunks | 1 | 284.91 s | 2.02 | 0.452 | 0.378 | 0.048 |
| 144 chunks (acceptance grid) | 12 (default) | 24.60 s | 5.85 | 0.165 | 0.033 | 0.123 |

Notes:

- The 576/12 row reproduces the archived steady-state baseline almost exactly
  (8.26 vs the archived 8.39 chunks/s, avg 0.111 vs 0.109 s/chunk) — same
  hardware class, so old and new numbers are comparable.
- Single-worker grids are **cave-dominated** (0.378 s of 0.452 s per chunk):
  one generator holds all cave-graph work with poor locality, while 12 workers
  split it across per-worker caches (caves drop to 0.057 s). Parallelism helps
  the cave stage more than the prefab stage.
- The 144-chunk acceptance grid is cold-cache-dominated (0.165 s/chunk vs the
  0.111 steady state): 144 chunks over 12 workers is only 12 chunks/worker,
  under the ≥32/worker rule — expected, documented, and exactly why the
  acceptance gate uses it only as a smoke test, never as a baseline.

## Criterion microbenchmarks (`cargo bench -p worldgen-hytale`)

| Benchmark | Median | 95% CI (ns) |
|---|---:|---|
| `cache/sized_timeout_get_hit` (moka-backed `SizedTimeoutCache`) | 91.48 ns | [91.65, 91.94] |
| `cache/hash_map_get_hit` | 7.13 ns | [7.129, 7.135] |
| `cache/hash_map_get_or_insert` | 6.14 ns | [6.137, 6.148] |
| `prefab_weights/random_lookup` | 1.28 ns | [1.288, 1.300] |
| `prefab_weights/weighted_lookup_03` | 10.80 ns | [10.763, 10.811] |
| `prefab_weights/weighted_lookup_05` | 11.62 ns | [11.682, 11.924] |
| `prefab_weights/weighted_lookup_10` | 13.81 ns | [13.767, 13.804] |

Java JMH references from the port's own bench headers: `weighted_lookup_03`
5.14 ns (Rust ~2× slower), `weighted_lookup_05` 17.19 ns (Rust ~1.5× faster),
`weighted_lookup_10` 21.73 ns (Rust ~1.6× faster). First baseline run — no
criterion change statistics yet.

## Feature enumeration (10,000×10,000)

**Blocked on T22** (no public `features_in` query exists yet — T01 audit §2).
Substitute measurement recorded now so T24 can size ring search:

- unique prefabs + spawn points for one seed: **26 ms cold, ~3-6 µs warm**
  (probe above); a whole-seed unique-prefab catalogue is effectively free.
- the placement pass is the dominant chunk stage (prefabs 0.042 of 0.111 s
  steady-state per chunk; 0.123 of 0.165 s cold), so T22's query cost will
  scale with candidate density over the rect — its 10k×10k benchmark is the
  number to record here.

## What this means for the explorer

1. **Worker count: start 2, cap at 8-12.** Measured parallel scaling on the
   576 grid is **4.1×** (2.02 → 8.26 chunks/s from 1 to 12 workers), and the
   gain is mostly the cave stage. Per-worker cost is real: ~0.9 s generator
   load, own cold caches, and grids under ~32 chunks/worker *regress* (the
   144-chunk grid at 12 workers barely beats a single warm worker). T08's
   pool should start 1-2 workers and scale lazily; 12 is the sensible cap on
   this machine (SMT beyond physical cores adds nothing here).
2. **Tile budgets (single-worker math from the medians):**
   - Biome tile 256×256 @ step 2: ~2.3 s of column queries → ~0.6 s with
     banded 4-worker requests. The plan's first-visible-tile ~100 ms /
     coarse-fill ~1 s targets are reachable **for the biome layer only**.
   - Height/relief layers: never per-column at tile scale (~70 s/tile).
     Derive heights from chunk summaries (T09/T17): the same 64-chunk region
     gives every layer for ~21 s single-worker, or ~8 s across 12 warm
     workers (8.26 chunks/s ⇒ 64/8.26 ≈ 7.8 s).
   - Full-block tile (step 1, 256×256): ~0.31 s/chunk ⇒ 21 s cold on one
     worker, ~8 s warm across 12 workers, plus 55 ms extraction.
3. **512 MB cache implication (concrete):** a retained 64-chunk region is
   ~420 MiB of `GeneratedChunk` buffers — nearly the whole budget for one
   tile. Compact summaries (~22 KB/chunk) let a 256 MB summary cache hold
   ~11,000 chunks ≈ a 33×33-chunk area (~1,050×1,050 blocks), leaving the
   plan's 512 MB tile cache intact. The generator's own 50,000-entry column
   cache cannot hold a 65,536-entry tile (T01 §4 thrash finding), so tile
   results must be owned by map-core, never re-queried per sample.
4. **First-tile strategy (mandated by these numbers):** coarse-first is not
   optional for non-biome layers — T15/T16 should (a) show the biome layer
   immediately (it alone is cheap), (b) stream chunk summaries
   nearest-to-center first, (c) treat height/hillshade as derived from
   summaries, not as column queries.
5. **Feature search:** unique-prefab catalogues are free (26 ms per seed);
   T24's ring-search viability hangs on T22's query benchmark, to be recorded
   here when it lands.

## Known gaps

- **Feature enumeration over a rect: blocked on T22** (substitute row above).
- **Full-block render cost end to end: blocked on T09/T21** (the probe
  measures generation + extraction; palette lookup and texture upload are not
  yet implemented — T21 appends).
- **Parallel banded column queries: blocked on T08** (the probe is
  single-worker by design; T08 appends a banded 4-worker biome-tile number).
- The world is `testdata/Default`, not a live pack-derived world; absolute
  numbers may shift with real content, ratios should hold.

## Regression table

Format: one row per metric per re-measurement; append below, oldest first.
Only release runs on the environment above (or note the change).

| Date | Metric | Value | Command | Owner |
|---|---|---|---|---|
| 2026-11-18 | biome 65,536 samples | 2.332 s median / 2.362 p95 | `query_bench testdata/Default 0 5 1` | T04 |
| 2026-11-18 | height 65,536 samples | 70.634 s median / 70.888 p95 | `query_bench testdata/Default 0 5 1` | T04 |
| 2026-11-18 | biome+height step-1 65,536 | 25.987 s median / 26.175 p95 | `query_bench testdata/Default 0 5 1` | T04 |
| 2026-11-18 | 64-chunk region generation (1 worker) | 21.139 s median / 21.257 p95 | `query_bench testdata/Default 0 5 1` | T04 |
| 2026-11-18 | 64-chunk region surface extraction | 0.055 s median / 0.056 p95 | `query_bench testdata/Default 0 5 1` | T04 |
| 2026-11-18 | unique prefabs (seed 0) | 26.2 ms cold / 3 µs warm | `query_bench testdata/Default 0 5 1` | T04 |
| 2026-11-18 | 576-chunk grid, 12 workers | 69.77 s wall, 8.26 chunks/s | `bench --world testdata/Default --seed 0 --start 0,0 --chunks 576` | T04 |
| 2026-11-18 | 576-chunk grid, 1 worker | 284.91 s wall, 2.02 chunks/s | `bench ... --chunks 576 --workers 1` | T04 |
| 2026-11-18 | 144-chunk acceptance grid (12 workers) | 24.60 s wall, 5.85 chunks/s | `bench --world testdata/Default --seed 0 --chunks 144` | T04 |
| 2026-11-18 | criterion suite | medians in § criterion table | `cargo bench -p worldgen-hytale` | T04 |
