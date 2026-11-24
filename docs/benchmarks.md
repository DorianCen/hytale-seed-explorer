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

## Layer tiles (T17)

One full layer tile through the whole pipeline — the 256×256-sample rect a
tile of the map asks for, at the plan's default 2×2 sampling (512×512
blocks) — via `map-cli render --release`, seed 0, 2 workers, cold caches:

```sh
cargo build --release -p map-cli
./target/release/map-cli render --world testdata/Default --seed 0 \
  --center 0,0 --size 256 --step 2 --layer height --out target/height-tile.png
./target/release/map-cli render --world testdata/Default --seed 0 \
  --center 0,0 --size 256 --step 2 --layer relief --out target/relief-tile.png
./target/release/map-cli render --world testdata/Default --seed 0 \
  --center 0,0 --size 256 --step 2 --layer fluid --out target/fluid-tile.png
```

| Metric | Median | p95 (worst of n) |
|---|---:|---:|
| Height tile (65,536 column queries, +ramp) | **20.38 s** (n=3) | 20.40 s |
| Relief tile (bordered 258×258 query, +hillshade) | **20.63 s** (n=3) | 20.86 s |
| Fluid tile (surface summaries, 256 chunks cold, 2 workers) | **86.23 s** (n=1, cold) | — |

What the numbers say:

- **The height path is the generator's column cache, not the columns.**
  A tile always asks for 65,536 samples and the per-instance column cache
  holds 50,000 (the T01 §4 thrash finding): every height/relief tile pays
  the ~310 µs/sample steady state — 3.5× better than the raw probe's
  single-threaded 70 s (T04), thanks to the pool's band-splitting, but
  still far from the **0.031 s** of a biome-warm 4,096-sample window
  (T08). Windows under the cache are µs/sample; the tile shape is what
  breaks it. Follow-ups for T33: derive tile heights from the chunk
  summaries (T09's cached-surface row: 0.21 s warm) instead of per-column
  queries, or cap a tile's sample count for the expensive layers.
- **The fluid path is chunk-bound**, as T09 predicted: 256 chunks per
  512-block tile at ~0.34 s each (2 workers) ≈ 86 s cold; warm summaries
  bring the same tile to ~0.2 s. The summary cache does the work — the
  first tile over any area is the expensive one, for every layer.
- **Relief's border is free** (20.63 vs 20.38 s: the 258×258 query rides
  the same churn), and the shading itself is nanoseconds per sample.
- The layer goldens (tests/layers.rs) sit on a 256-block shore window at
  step 4 (height/relief ≈ 0.4 s debug, under the cache) and a 128-block
  window at step 2 for fluids (16 chunks ≈ 30 s debug cold).

## Full-block tiles (T21)

The plan's most expensive mode end to end: one full-block surface tile —
256×256 samples @ step 1 (a 256-block tile, **64 chunks** cold) through
`SurfaceTileRenderer` with the fallback palette, seed 0, no hillshade.
Run with:

```sh
cargo test -p map-core --features test-support --release --test surface_render \
  -- --ignored surface_bench --nocapture
```

| Workers | Cold (s) | Warm repeat (s) | Chunks |
|---:|---:|---:|---:|
| 1 | **24.40** | 1.276 | 64 |
| 4 | **14.25** | 0.464 | 64 |
| 12 | **21.69** | 0.136 | 64 |

What the numbers say (T21's design consequences):

- **The coarse placeholder must be the default experience**, exactly as
  the task's risk note demands: a cold 1×1 tile is 14–24 s regardless of
  workers, while the warm repeat over cached summaries is 0.03–1.3 s.
  The progressive schedule (fill @ step 4, refine @ step 1) front-loads
  chunk generation into the coarse pass; the fine tiles then ride the
  warm summaries at a fraction of the cold cost.
- **4 workers beats 12 cold** on a single 64-chunk tile: 64 chunks /
  12 workers ≈ 5 chunks each, inside the "<32 chunks per worker
  regresses" warm-up finding already recorded for the T04 grid runs.
  The worker cap's default (T33 tunes it) should sit near 4 for
  tile-shaped work; 12 only wins when many tiles are queued at once.
- **Against T04/T09:** the 64-chunk cold cost matches the chunk path's
  ~0.34 s/chunk (2-worker) throughput; the warm 0.136 s (12 workers)
  reproduces T09's "surface region @ step 2, summaries cached: 0.21 s"
  scale — the summary cache, not the renderer, owns the performance.

## Zoom-out pooling, tiered icons, surface-only fills (perf push)

**Origin:** user-reported slowness vs chunkbase-style explorers + the
request that unzooming reuse what was already generated ("pooling layer")
instead of blanking. Three levers landed; numbers below.

### Surface-only coarse fills (`SummaryFidelity`)

Fill passes at steps **coarser than the user's effective step** now request
`SummaryFidelity::SurfaceOnly` chunks (tint → environment → blocks → water;
cave/prefab populators skipped — an opt-in worldgen request mode, default
pipeline byte-identical, golden-guarded; worldgen commit `40dec05`).
Refine passes and the user's own step stay `Full`. Fidelity rides the
`TilePass`, is a separate summary-cache key (a SurfaceOnly summary can never
serve a Full request), and tile provenance re-renders weaker cache hits
(placeholders stay fidelity-blind stand-ins). Measured probe
(`crates/worldgen-cli/src/bin/surface_only_probe.rs`, release, 64 chunks,
single generator, seed 0):

| Mode | Region (s) | Per chunk (s) |
|---|---:|---:|
| Full pipeline | 20.6–21.2 | 0.32–0.33 |
| Surface-only | **3.82–3.84** | **0.060** |
| Speedup | **5.4× cold / 5.5× warm rerun** | |

**Consequence for the T21 row above:** the cold coarse fill of a 64-chunk
region drops from ~21 s (1 worker) to **~4 s**; the refine pass regenerates
its chunks Full (by design — placeholders are cheap, detail is exact). On
the fixture chunk (seed 0, chunk (0,0)), 177/1024 top-block columns differ
between the modes — every one is the chunk's biome prefab, resolved when
the refine pass lands.

### Zoom-out pooling (`pooled_placeholder`)

Unzooming no longer blanks: a missing coarse tile is derived instantly by
box-averaging the cached finer tiles covering it (factor = step ratio,
capped at 8; alpha-0 = no-data). The reverse direction (a missing fine tile
showing a cached coarser one) already existed — now both zoom directions
keep content on screen while real tiles generate. Pure map-core, 10
`pooling_` tests + 5 map-app paint/texture-bridge tests (one upload per
key, retired on real-tile arrival).

### Tiered feature icons

Overlay fetches are two tiers: cheap kinds (spawn/unique/POI — the T22 kind
mask skips the placement pass entirely) fire immediately at source-ready
over the visible rect (**milliseconds**, see the T22 probe: 26 ms cold for
the per-seed lists); the full biome-prefab fetch stays debounced
(150 ms settle, margin rect, 2048-block cap) and a one-shot 64×64 prewarm
query loads the prefab buffers (~8 s of asset loads) before it. The status
bar shows `features: generating…` while any tier is in flight. 15 overlay
tests.

## Feature enumeration (T22)

**Task:** [T22](../tasks/T22-feature-placement-query.md) — the measured
replacement for the substitute numbers recorded before the query existed.
Harness: the committed probe `feature_query_bench`
(`crates/worldgen-cli/src/bin/feature_query_bench.rs`, worldgen repo commit
`6f5cc1c`), release build, Default fixture, seed 0, one `features_in` call per
square rect centred on the origin (nested rects, so later runs re-cover
earlier ground):

| Rect | Seconds | Placements | Growth vs previous |
|---|---:|---:|---|
| 256×256 | 10.4 | 586 | — |
| 512×512 | 20.1 | 1,311 | 1.93× per 4× area |
| 1024×1024 | 73.3 | 6,015 | 3.64× |
| 2048×2048 | 326.3 | 35,378 (2 unique, 1 spawn) | 4.45× |
| 5000×5000 | 1,599.5 | 262,608 | 4.90× per 5.96× area |
| 10,000×10,000 | **≈6,400 (extrapolated)** | — | linear fit from 5000 |

Peak RSS of the probe process: **~5.0 GiB at the 5000 leg** (the killed
10,000 leg pushed VmHWM to ~7.5 GiB) — dominated by the generator's unbounded
prefab-store buffer cache and the query-lifetime zone-biome memo, not by
candidates (candidates hold a 24-byte footprint since the perf pass).

What changed vs the first measurement: the query originally grew ~cubic
(2048 was 871 s and 10,000 extrapolated to a day). Three fixes
(worldgen `6f5cc1c`) — footprint candidates, a per-query zone-biome memo,
and clone-free buckets with an x-sweep — made the cost **linear in area
from 512 blocks up**.

**T24 ring-search budget (the number the acceptance gate asked for):** ring
rects in the low hundreds of blocks cost **seconds each** (256: ~10 s cold
incl. asset warm-up, faster warm; 512: ~20 s). A search to a 10,000-block
radius as 40 concentric 256-wide rings is minutes of work, not hours — ring
search is viable. Two rules for T24/T26:

- Query the smallest rect that answers the step; a smaller rect's result is
  exactly a subset of a larger one's (the per-window invalidation makes
  this structural, not empirical), so filter client-side instead of
  re-querying nested rects.
- Never issue a single bulk rect larger than ~2048 per call in the UI path
  (~5 min, multi-GiB); a 10,000² bulk query is a ~1.8 h batch job with
  multi-GiB memory — the follow-up lever (bounded prefab-store cache,
  LRU memo) belongs to T33.

## Feature search (T24)

**Task:** [T24](../tasks/T24-feature-search.md) — the ring engine measured
against the real generator. Harness: the ignored test
`feature_search_bench_10k_radius` (`map-core/tests/feature_search.rs`),
release build, Default fixture, 512-block ring regions, **cheap-tier kinds
only** (spawn / unique prefab / POI — the kind mask skips the generator's
biome placement pass, which is what makes the UI path viable at all):

| Seed | Radius | First result | 100th result | Results | Regions | Total |
|---|---:|---:|---|---:|---:|---:|
| 12345 | 10,000 | 48.8 ms | n/a | 3 | 1,681 | 73.1 ms |
| 0 | 10,000 | 50.2 ms | n/a | 3 | 1,681 | 72.9 ms |
| 0 | 50,000 | 49.2 ms | n/a | 3 | 38,809 | 614.6 ms |

Readings:

- **The plan's "first results within ~500 ms" target is met with a 10×
  margin**: ~49 ms to the first streamed result at the standard
  10,000-block radius. The latency is the generator's cold unique/spawn
  list build (T04: 26.2 ms cold) plus the first ring's pool round-trips;
  warm regions then cost ~44 µs each, so the *bound* (the first safe
  emission threshold), not query latency, sets first-result time.
- The full 10k scan is ~73 ms over 1,681 regions; a 50,000-radius sweep
  stays at 0.6 s over 38,809. Ring count for a radius is
  `ceil(r / 512) + 1` per side — the region size, not the radius, shapes
  the streaming granularity.
- "Time to 100 results" is **n/a on this fixture**: the Default world
  defines only three cheap-tier features within 50,000 blocks of the
  origin on both seeds, so no dense case exists at the UI path's kind
  tier. Density lives in biome prefabs, whose ring rects pay the T22
  seconds-each price — a biome-prefab-inclusive search is the T33 batch
  lever, not a UI-path benchmark.

## Biome search (T25)

**Task:** [T25](../tasks/T25-biome-search.md) — the ring engine measured
against the real generator. Harness: the ignored test
`biome_search_bench_10k_radius` (`map-core/tests/biome_search_fixture.rs`),
release build, Default fixture, seed 12345, step-16 coarse lattice, origin
center, 10,000-block radius, unlimited results. The common/rare roles come
from a runtime ±8,192 probe histogram (65,536 samples at step 64 — the
fixture's biome distribution is world data, not a constant):

| Biome | Role | First result | Results | Coarse samples | Total |
|---|---|---:|---:|---:|---:|
| 32 (Shore) | common | 20.8 ms | 1,797 | 1,575,025 | 170.2 s |
| 39 (Plains_Spawn) | rare | 143.2 ms | 1 | 1,575,025 | 34.6 s |

Readings:

- **The plan's "first results within ~500 ms" target is met**: 20.8 ms for
  the most common biome, 143.2 ms for a rare one. The first result rides
  the first rings (the walk outruns the radius by the
  `(step−1)·√2` lattice reach), never the full scan.
- The full 10k scan is 627 rings / ~2,508 strip queries / 1.57M coarse
  samples: ~35 s for a biome with one match, linear in radius² at a fixed
  step (the walk covers the whole (2·627+1)²-point lattice).
- The common case costs 5× the rare one: 1,797 frontier results each pay a
  ±31-block step-1 refinement (~4k samples), so refinement — not the scan
  — dominates when results are dense. The default `max_results` cap stops
  the walk when it is reached, so the UI path never pays the full 1,797.
- Patch-area flood fills are opt-in (`min_patch_blocks`, not set here) and
  are bounded by the 4-strips × 32-rings fill cap per candidate.

## What this means for the explorer

1. **Worker count: start 2, cap at 8-12.** Measured parallel scaling on the
   576 grid is **4.1×** (2.02 → 8.26 chunks/s from 1 to 12 workers), and the
   gain is mostly the cave stage. Per-worker cost is real: ~0.9 s generator
   load, own cold caches, and grids under ~32 chunks/worker *regress* (the
   144-chunk grid at 12 workers barely beats a single warm worker). T08's
   pool should start 1-2 workers and scale lazily; 12 is the sensible cap on
   this machine (SMT beyond physical cores adds nothing here).
2. **Tile budgets (single-worker math from the medians):**
   - Biome tile 256×256 @ step 2: **0.12 s through the 2-worker pool
     (T08's measured number — 0.036 s at 8 workers)**, because each band
     fits under the generator's column cache. The plan's first-visible-tile ~100 ms /
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
   ring rects in the low hundreds of blocks cost seconds each through
   `features_in` (T22, measured above) — ring search is viable at the
   10,000-block-radius scale in minutes.

## Known gaps

- ~~**Feature enumeration over a rect: blocked on T22**~~ — landed: measured
  table above. Remaining lever for bulk (>2048) rects is memory, not time
  (T33: bounded prefab-store cache / LRU memo).
- **Full-block render cost end to end: blocked on T09/T21** (the probe
  measures generation + extraction; palette lookup and texture upload are not
  yet implemented — T21 appends).
- **Parallel banded column queries: measured (T08).** The `GeneratorPool`
  band-splits region requests across workers, and bands fit under the
  generator's per-instance 50,000-entry column cache (the T01 §4 thrash
  finding disappears). Through `WorldSource` (`map-core/examples/pool_bench`,
  release, seed 12345):

  | Metric | 2 workers | 8 workers | T04 single-worker reference |
  |---|---:|---:|---:|
  | Pool startup (N generator loads) | 1.02 s | 1.17 s | 0.90 s (one) |
  | Biome region 256×256 @ step 2 (65,536 samples) | **0.120 s** | **0.036 s** | 2.332 s |
  | Height region 64×64 @ step 1 (4,096 samples, biome-warm caches) | 0.031 s | 0.013 s | ~4 s est. at 1 ms/sample |
  | Cancellation health (next request after `cancel_pending`) | completes | completes | — |

  **Chunk-path queries through the pool (T09, same probe):** a 256×256
  surface region at step 1 (64 chunk generations, cold) costs **24.1 s** at
  2 workers; a 64×64 slice at y=64 (4 generations) **1.8 s**; and the
  decisive number — a **cached** surface region (summaries warm, biome
  batch + assembly only) at step 2 costs **0.21 s**. The summary cache is
  what makes the plan's full-block mode viable: each chunk is paid once
  (~0.45 s) and every later layer, step and slice over that area is
  sub-second.
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
| 2026-11-18 | biome region 65,536 via pool, 2 / 8 workers | 0.120 s / 0.036 s | `cargo run --release -p map-core --example pool_bench -- <world> 12345 <workers>` | T08 |
| 2026-11-18 | height region 4,096 via pool (biome-warm), 2 / 8 workers | 0.031 s / 0.013 s | `pool_bench` as above | T08 |
| 2026-11-18 | surface region 65,536 @ step 1 (64 chunks, cold), 2 workers | 24.11 s | `pool_bench` as above | T09 |
| 2026-11-18 | slice region 4,096 @ y=64 (4 chunks, cold), 2 workers | 1.80 s | `pool_bench` as above | T09 |
| 2026-11-18 | surface region @ step 2, summaries cached | 0.21 s | `pool_bench` as above | T09 |
| 2026-11-18 | biome 512×512 PNG end to end (map-cli render, 2 workers, cold caches) | 1.08 s load + 3.47 s sample+encode | `cargo run --release -p map-cli -- render --seed 0 --center 0,0 --size 512 --step 2 --layer biome --out target/region-512.png` | T11 |
| 2026-11-19 | height tile 65,536 cols @ step 2 (map-cli render, 2 workers) | 20.38 s median / 20.40 p95 | `cargo run --release -p map-cli -- render --seed 0 --center 0,0 --size 256 --step 2 --layer height ...` | T17 |
| 2026-11-19 | relief tile (bordered query + hillshade, same setup) | 20.63 s median / 20.86 p95 | `... --layer relief ...` | T17 |
| 2026-11-19 | fluid tile (256 chunk summaries cold, 2 workers) | 86.23 s (n=1) | `... --layer fluid ...` | T17 |
| 2026-11-19 | full-block surface tile 65,536 @ step 1 (64 chunks cold), 1 / 4 / 12 workers | 24.40 s / 14.25 s / 21.69 s | `cargo test -p map-core --features test-support --release --test surface_render -- --ignored surface_bench --nocapture` | T21 |
| 2026-11-19 | same tile, warm (summaries cached), 1 / 4 / 12 workers | 1.276 s / 0.464 s / 0.136 s | as above | T21 |
| 2026-11-24 | feature search first result / total, 10,000 radius, cheap tier (seeds 12345 / 0) | 48.8 ms / 73.1 ms and 50.2 ms / 72.9 ms (3 results, 1,681 regions) | `cargo test -p map-core --release --test feature_search -- --ignored feature_search_bench --nocapture` | T24 |
| 2026-11-24 | feature search 50,000 radius sweep, cheap tier (seed 0) | 49.2 ms first / 614.6 ms total (38,809 regions) | as above | T24 |
| 2026-11-24 | biome search first result, 10,000 radius (common id 32 / rare id 39) | 20.8 ms / 143.2 ms | `cargo test -p map-core --release --test biome_search_fixture -- --ignored biome_search_bench --nocapture` | T25 |
| 2026-11-24 | biome search full 10,000 scan, unlimited results (common 1,797 results / rare 1 result) | 170.2 s / 34.6 s (1,575,025 coarse samples) | as above | T25 |

T11 golden hashes (`map-core/tests/render_golden.rs`, seed 0, 64×64 @ step 2,
PNG bytes):

- center `(0, 0)`:
  `a71236edde2ac1db70fac1613e882359a75e607412612b7744136dd86d17a41e`
- center `(-1203, 455)`:
  `b0dda0aa704f2334a1e9fb75f189b9a08eca195de5558aba090b01c105df3e0b`

The 512×512 acceptance render (command above) hashes to
`ae0db8c3101f2a1aefbf6c33ab0d88b916f86fb424470e1372b1248377f365b` —
byte-identical across repeat runs, debug and release profiles alike. The
sample cost is a cold-cache number: the pool's first query pays worker
warm-up; the steady-state reference for the same 65,536-sample region is
T08's 0.120 s. Negative-center render
(`--center -1203,455`, same size/step):
`1232715b5ecf6bbdcd7403e8ab36139dd219bd41185955dcdbb2d050a8ce5be2`
(113 distinct biome colors; the origin render shows 68 — the acceptance
check "not uniformly one color" holds for both).
