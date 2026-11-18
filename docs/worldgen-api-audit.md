# Worldgen API audit and gap matrix (T01)

**Task:** [T01](../tasks/T01-worldgen-api-audit.md) — status: done.
**Audited workspace:** `/home/chome/Projects/hytale-server-rust` (commit `main`, working tree
clean; no production code was changed — see [§7](#7-preliminary-baseline-numbers) for the
one temporary, since-removed probe).
**Scope authority:** [plan §2](../hytale-seed-map-plan.md#2-what-the-explorer-needs-from-your-worldgen),
[§10](../hytale-seed-map-plan.md#10-risks-and-open-questions).

**Environment of every measured number below:** AMD Ryzen 7 9800X3D (8 cores / 16
threads), 63.4 GiB RAM, Linux 7.3.5 (CachyOS), rustc/cargo 1.99.1, `--release`, world
`testdata/Default`, seed `0`. Same hardware class as the archived worldgen baseline
(`done/world-generation/BENCHMARKS.md`), so the numbers are comparable. All numbers are
**preliminary, single-run**; the permanent warmed/multi-run harness is owned by
[T04](../tasks/T04-performance-baseline.md) (`docs/benchmarks.md`).

---

## 1. Headline answers

The two questions later tasks must not have to re-derive:

**"Can features be enumerated without generating terrain?"** — **Yes, with one
documented nuance.**

- Unique prefabs and spawn points: **yes, already public and cheap.**
  `ChunkGenerator::get_unique_prefabs(seed)` (chunk_generator.rs:319) and
  `get_spawn_points(seed)` (:751) never touch a chunk. Measured: 28 ms cold /
  ~6 µs warm for the whole per-seed list (2 entries for seed 0 in `testdata/Default`).
- Biome prefabs: **the predicates are terrain-free; the code path is not yet
  separable.** `PrefabPopulator`'s candidate collection
  (`collect_biomes`/`collect_prefabs`/`collect_prefab`,
  prefab_populator.rs:219/:353/:401) reads **no `GeneratedChunk`, no block arrays** — a
  workspace-wide search finds no `get_block`/`get_chunk` call in either populator. What
  it reads is: chunk-relative lattice bounds, `get_zone_biome_result_at`
  (:428-434), `get_height`/`generate_height_between` via `Self::get_height`
  (:868-909), and biome layer/cover/water *containers* (configuration, :1012-1095).
  The one latent block-stage dependency is `is_base_valid` (:769-831): for in-chunk
  probes it reads the execution's per-chunk height table (`execution.get_height`,
  :820-827), whose values are written by the **blocks stage**
  (`block_populator.rs:166-168` `put_height(seed, x, z, highest)`; `NO_HEIGHT = -1`
  before that). A chunk-free query would see `-1` there. Patterns with empty
  `base_checks` skip the check entirely (:779-781). **Re-scope note for T22:** it must
  either replicate the blocks-stage surface height for base-checked patterns or
  measure how many entries actually use base checks; this is the only place the
  "placement without terrain" rule has a caveat.
- Cave entrances: **yes.** The authoritative predicate in `CavePopulator::run`
  (cave_populator.rs:120-141) needs only `get_zone_biome_result_at` (zone identity via
  `Rc::ptr_eq`), `CaveType::is_entry_threshold` (pure map condition,
  cave_type.rs:231) and `get_height` + the height condition with the magic constant
  `seed.wrapping_add(0xF5ACDEB5)` (:239-251). `get_cave` (the expensive recursive
  graph) is called **only after** the predicate passes. The predicate is private
  (`fn run`, :112); exposing it is the T27 refactor.

**"What is the cheapest way to get a surface block?"** — **Generate the chunk, extract
a compact column summary, drop the chunk.** Measured alternatives:

| Path | Cost for one 256×256-block tile | What you get |
|---|---|---|
| Per-column `get_height` (65,536 samples) | ~65 s single worker (~1 ms/sample cold) | height only — **no block/fluid id**, and it is the pre-blocks-stage noise height, not the true surface |
| Per-column biome + height | ~25 s single worker | biome id + noise height, still no block id |
| Whole chunks (64 chunks = the same area) | **22.2 s cold on 12 workers** (avg 0.34 s/chunk; archived steady state 8.39 chunks/s → ~8 s warm) | the full column: top block, fluids, tint, environment, cave carve |

There is no cheaper block-level API: `GeneratedChunk` has **no top-block/height
accessor at all** (the port is explicitly missing, chunk.rs:67-74), so the top block is
a downward `get_block` scan (chunk.rs:234-240) using registry opacity
(`worldgen_model::registry::BlockTypeInfo`, registry.rs:179-193) — or the private
`legacy_heightmap` helper (hytale-world/src/chunk/column.rs:518-540), reachable only via
`ChunkColumn::from_generated_chunk_with_registries` → `heightmap()`. A retained
`GeneratedChunk` costs **~6.57 MiB** (5 × 327,680 × 4 B arrays + 327,680 B fluid
levels + column arrays), so a 256×256 tile's 64 chunks ≈ **420 MiB if retained** —
that alone busts the 512 MB cache budget and is the reason [T09](../tasks/T09-chunk-surface-slice.md)
extracts ~22 KB summaries instead. Per-column queries cannot substitute: they are both
slower (height ≈ 25× biome cost) and incomplete (no block id).

---

## 2. Capability matrix

Every plan §2 row, verdict, exact call chain, and owning task.

| Plan need | Verdict | Existing capability (file:line) | Cost / caveat | Owning task |
|---|---|---|---|---|
| Biome at `(x, z)` without blocks | **existing** | `ChunkGenerator::get_zone_biome_result_at(seed, x, z) -> ZoneBiomeResult` (chunk_generator.rs:328-334); `ZoneBiomeResult::get_biome() -> Option<&Rc<dyn Biome>>` (zone_biome_result.rs:59); `Biome::get_id/get_name/get_map_color` (biome.rs:25/:27/:49, color is `i32`) | Cached per instance (50k entries / 20 s idle, §4). ~40 µs cold, sub-µs genuine hit (probe "warm" average 12 µs includes churn). **No batch/region API exists** — the worker must loop per sample; that is T08's `BiomeRegion` command. | T08 (batch wrapper) |
| Surface height at `(x, z)` | **existing** | `ChunkGenerator::get_height(seed, x, z) -> i32` (chunk_generator.rs:337-341); `generate_height_between(seed, x, z, interpreter)` (:820-853) for prefab-style deep search | ~1 ms cold/sample (biome-count interpolation + up to ~320 threshold evaluations); atomic load when resident. This is the **noise height**, not the post-blocks-stage surface. | T08; height *layer* should prefer T09 summaries |
| Top block / fluid depth at `(x, z)` | **needs wrapper** | Only via a generated `GeneratedChunk`: `get_block`/`get_fluid_id`/`get_fluid_level` (chunk.rs:234-240/:339-345/:349-355, chunk-local x/z 0..31, world y 0..320). No `GeneratedChunk::get_height` exists (chunk.rs:67-74). | Requires whole-chunk generation; ~6.57 MiB per chunk while alive. Extract a compact summary and drop the chunk. | T09 (`ChunkSummary`, top-block scan via public `Registries`) |
| Feature placement without terrain (unique prefabs, spawn) | **existing** | `get_unique_prefabs(seed) -> Vec<UniquePrefabEntry>` (chunk_generator.rs:319-321); `get_spawn_points(seed) -> Vec<Transform>` (:751-789); entry accessors `get_name/get_position/is_show_on_map/is_spawn_location/get_low_bound_*` (unique_prefab_container.rs:276-368) | Per-seed catalogue (50-entry / 5 min cache). 28 ms cold / ~µs warm, seed 0. `is_show_on_map` = 0 of 2 entries in `testdata/Default` — the real asset pack may differ (T23). No aggregate `get_bounds()`; combine low/high accessors. | T08 (spawn marker), T23 (catalog) |
| Feature placement without terrain (biome prefabs) | **needs worldgen API** | Predicates exist and are terrain-free (§1) but private and coupled to `ChunkGeneratorExecution` (prefab_populator.rs:192-523); `Candidate` and result state are private (:110-129/:145-151) | Draw order is load-bearing (hash3 seed chains, :413-415); any reimplementation would drift. Base-check height-table nuance in §1. | **T22** (`features_in(seed, rect, filter)` query module) |
| Cave entrances | **needs worldgen API** | Predicate quoted in §1 (cave_populator.rs:120-141, :239-251); `CaveType::get_entry_point_generator/is_entry_threshold/get_maximum_size` (cave_type.rs:182/:231/:241) | Cheap (zone + map condition + height); `get_cave` must not be called in the query. `CavePopulator::run` is private (:112). | **T27** (extract predicate, `cave_entrances_in`) |
| Cave solidity / column profile | **needs wrapper** | `ChunkGenerator::get_cave(cave_type, seed, x, z) -> Option<Rc<Cave>>` (chunk_generator.rs:308-310, 5k/30 s cache); carve state itself only exists in a generated chunk | Cave graph is expensive; block state needs the chunk. Slice/density come from chunk summaries, not the graph. | T09 (slices), T28 (density) |
| Deterministic, thread-safe sampling | **existing boundary; needs explorer wrapper** | `WorldGenService` (world_gen.rs:421-436): one OS thread per worker, each owning its own `Rc<ChunkGenerator>` (worker_main :587-666), priority queue + `Arc<AtomicBool>` cancellation (:78, checked per stage), `WorldGenStats` (:329-351, 100-chunk warmup) | `ChunkGenerator` is `!Send`/`!Sync` (Rc fields, §3). The service only serves **whole-chunk** requests (`WorldGenRequest` :66-79); no cheap column-query service exists. | **T08** (`GeneratorPool`, same pattern, column commands) |
| Version tag | **needs worldgen API** | None in `worldgen-hytale`. The only `GENERATOR_VERSION` is `hytale-server`'s inert `"0.0.0"` (generation.rs:29); `WorldGenConfig.version` has no consumer (world_gen_config.rs:11-18) | Tile keys and exports need a real constant; caches are keyed by seed only, so generator changes cannot invalidate them today. | **T02** |
| Seed semantics | **existing (i32), string unverified** | `WorldGenRequest.seed: i32` (world_gen.rs:69); world config `seed: i64` (world_config.rs:401-403) truncated `save.config().seed as i32` (world_thread.rs:3145, :6209, :6223) | Silent truncation; distinct i64 seeds collide mod 2³². **No string-seed parser exists anywhere**; `procedural::json::SeedString` hashes loader *labels* (`"ChunkGenerator"`), not user seeds (seed_string.rs:31-60). | **T02** (verify against the game) |

Also verified for the UI's readouts and catalogs (all public, no worldgen change):

- Zone name/id: `ZoneGeneratorResult::get_zone() -> Option<&Rc<Zone>>`
  (zone_generator_result.rs:44-46); `Zone { pub id, pub name }` (zone.rs:22-33).
- Block/fluid names and metadata: `worldgen_model::registry::Registries`
  (registry.rs:611-619) via `ChunkGenerator::get_registries()` (chunk_generator.rs:250);
  `BlockRegistry::name/index_of` (:379/:393), `Opacity::is_transparent` (:166),
  `FluidRegistry::get_max_fluid_level` (:485).
- Generatable-area bounds: `WorldGenService::is_chunk_outside_generatable_area` /
  `generatable_chunk_bounds` (world_gen.rs:513-523);
  `ChunkGenerator::get_min/max_chunk_coord` (chunk_generator.rs:260/:265) — T09's
  out-of-bounds handling uses these.
- Headless rendering already works end to end: `worldgen-cli render-map`
  (mode height/zone/biome, main.rs:291-369) loops `get_height` /
  `get_zone_biome_result_at` per pixel and writes PNG — the archived golden render
  tests gate it.

---

## 3. Safe concurrency

What crosses threads and why:

- **`ChunkGenerator` is `!Send` and `!Sync` by construction** — not a bug, a design
  fact of the ported graph: every field is `Rc`-based
  (`zone_pattern_generator_cache: RwLock<HashMap<i32, Rc<ZonePatternGenerator>>>`,
  `generator_cache: RwLock<HashMap<CoordinateKey, CacheRecord>>` with
  `CacheRecord.value: Rc<CoreDataCacheEntry>`, `cave_generator_cache` holding
  `Rc<dyn Fn …>`, `unique_prefab_cache` holding `Rc<dyn Fn …>`,
  `registries: Option<Rc<Registries>>`), and the constructor returns `Rc<Self>`
  (chunk_generator.rs:122-150, :175-198). No `unsafe impl Send/Sync` exists anywhere
  in the workspace.
- **The safe boundary is thread ownership, not locking.** `WorldGenService` spawns
  exactly `options.workers` OS threads (world_gen.rs:447-458; default
  `max(2, ceil(0.75 × cores))`, :58-63), each building its **own** generator and
  registries inside the thread (worker_main :587-623) and communicating only through
  channels (request queue in, reply channel out). The module docs state this
  explicitly (world_gen.rs:12-32). The interior `RwLock`/atomics are uncontended
  because exactly one thread ever touches each generator.
- **Therefore the explorer's worker pool must copy this pattern, not fight it**
  (decision; alternatives rejected in §6): a `GeneratorPool` where each worker loads
  its own `ChunkGenerator` once (~0.89 s measured per load — do **not** eagerly load
  12 workers at app start; T08's "start 1-2, scale lazily" note is confirmed by the
  measurement) and answers **batch region commands** by looping the per-column APIs
  inside the worker. No `Rc` ever crosses a thread; the compile-time `!Send` bound
  enforces it for free.
- Consequences of N generators: N × cold start, N × per-instance caches (no sharing),
  N × zone-pattern cache (unbounded!). Round-robin single-column queries lose cache
  locality; batch region requests keep a whole tile on one worker. This matches
  T08's design (`BiomeRegion`/`HeightRegion` commands, cancellation checked every N
  rows).
- Determinism is per-instance and per-seed: `CoordinateKey { seed: i32, coord }`
  (coordinate_cache.rs:30-38) means two seeds coexist in one cache; parallel workers
  produce identical results to serial (verified by the workspace's golden tests; the
  archived ARCHITECTURE.md:439-470 records the bit-exactness contract, 21 chunks /
  21,504 columns bit-exact).

---

## 4. Cache behavior (the numbers that shape the tile design)

Per-`ChunkGenerator` caches (all private fields, chunk_generator.rs:127-136):

| Cache | Capacity | TTL / eviction | Cleanup |
|---|---|---|---|
| `generator_cache` (zone/biome/height core data) | 50,000 entries (chunk_generator.rs:88) | 20 s idle (:92); `lookup_live` refreshes on hit (:899-915); eviction: expired first, then **arbitrary** `HashMap` keys (:942-954) — no LRU | `cleanup_cache()` exists (:880-886) |
| `cave_generator_cache` | 5,000 (:96) | 30 s (:100), same policy | `cleanup()` never called from `ChunkGenerator` |
| `unique_prefab_cache` | 50 (:104) | 5 min (:108), same policy | same — evicts only on insert |
| `zone_pattern_generator_cache` | **unbounded** | **none** | none |

**Thrash finding (measured):** a 256×256-pixel tile at step 2 needs **65,536
core-data entries — more than the 50,000 cap.** The probe shows the consequence:
the "warm" second pass over 65,536 samples is no faster than the cold pass (biome:
1.68 s cold / 1.68 s warm; height 64.8 s / 74.6 s — the warm pass even *re-computes*
while cascading arbitrary evictions). Under the cap (16,384 samples), warm behavior
returns (biome 0.66 s cold → 0.20 s warm; height 17.5 s → 4.3 s, still churn-tainted
because the map was already at cap from the earlier large loops).

**Implications, stated as requirements:**

1. The explorer must **not** rely on the generator's column cache for tile-sized
   regions: batch queries must be sized under ~50k samples per burst (T08) and the
   returned grids kept explorer-side (map-core's own cache, T14).
2. `get_height` is ~25× the cost of a biome sample (≈1 ms vs ≈40 µs cold). The height
   *layer* and hillshade should prefer chunk-summary heights (T09/T17) over
   per-column `get_height` storms.
3. The 20 s idle TTL means cache locality is worthless across user-interaction
   timescales anyway — another reason results must be owned by map-core, not by the
   generator.
4. `ZonePatternGeneratorCache` is unbounded with no eviction (per seed); the
   explorer's worker count multiplies it (one zone pattern generator per seed per
   worker). Harmless for a single-seed session; worth remembering for the seed-finder
   stretch goal (§6.4 of the plan).

---

## 5. Prefab and cave placement — evidence appendix

Details backing §1, with the exact call chain:

- `PrefabPopulator::run` order (prefab_populator.rs:192-209): fetch
  `get_unique_prefabs` → `collect_biomes` → `collect_prefabs` → `invalidate_prefabs`
  → paste (`generate_prefabs`, `generate_unique_prefabs`). A placement query needs the
  first four, never the paste.
- `collect_biomes` (:219-316) reads the execution's **core-data table** (zone/biome
  entries, not blocks) and samples zone/biome at step `BIOME_SAMPLE_STEP_SIZE` (:101)
  over the padded region.
- `collect_prefab` (:401-523) filters, in order: noise density (:418-420),
  unique-prefab exclusion radius (:424-426), biome match via
  `get_zone_biome_result_at` (:428-434), weighted pick + rotation (:436-451),
  height via `Self::get_height` — water containers / `generate_height_between` /
  `get_height` depending on the pattern flags (:868-909) — height condition
  (:469-478), **parent-block check from biome layer/cover containers, not generated
  blocks** (:481-491 → :1012-1095), and the base check (:493-508 → :769-831, the one
  blocks-stage-height dependency, §1).
- RNG draw order is load-bearing and documented as such (:31-48, :413-415): candidate
  seeds derive from `hash3(seed, biome.id, entry_id)` and per-point
  `hash3(prefab_seed, x, z) * 0x5feb8caf`. Reordering or filtering *before* the
  draws changes every downstream pick — T22's filter-after-placement rule exists for
  this reason.
- Cave entrance predicate (cave_populator.rs:120-141, quoted verbatim in T27's
  context section): same-zone check via `Rc::ptr_eq` → `is_entry_threshold` →
  height condition `eval3(seed.wrapping_add(0xF5ACDEB5), x, height, z)` with
  `height = get_height(seed, x, z)` — then, only for survivors, `get_cave` + paste.
  `CavePopulator::run` is private (:112); only `populate` is public (:74-107).

---

## 6. Recommended API split (decisions)

The task requires deciding, not leaving options open:

1. **Explorer-side `GeneratorPool` (T08), not a new worldgen query service.**
   `WorldGenService` already solved the `!Send` problem with per-worker ownership;
   a worldgen-side column-query service would duplicate that pool, add a
   cross-repo API surface, and still need per-worker generators. The explorer
   mirrors the proven pattern (channels + per-worker `Rc<ChunkGenerator>` +
   batch region commands) and keeps `map-core` headless behind the `WorldSource`
   trait. No worldgen change needed for biome/height queries.
2. **Compact `ChunkSummary` extraction (T09), not retained `GeneratedChunk`.**
   Measured: a chunk is ~6.57 MiB; 64 chunks ≈ 420 MiB vs the plan's 512 MB total
   budget, while a summary is ~22 KB (T09's estimate, consistent with the measured
   array sizes). The top-block scan is implementable explorer-side from public APIs
   (`GeneratedChunk::get_block` + `Registries` opacity); no worldgen change
   required. The private `legacy_heightmap` (hytale-world column.rs:518-540) is a
   reference implementation to match, not an API to depend on.
3. **Worldgen placement query module (T22), not duplicated predicates.** The draw
   order and predicates (`Rc::ptr_eq` zone checks, magic `0xF5ACDEB5`,
   hash3 chains) are too subtle to reimplement without drift; T22 refactors
   `PrefabPopulator` so collection runs against a lightweight query context and
   only the real populator pastes, guarded by the existing golden chunk tests.
   T27 extracts the cave-entrance predicate the same way. These are the **only**
   worldgen code changes the explorer project requires for v1 (plus the T02
   constant).

---

## 7. Preliminary baseline numbers

> **Preliminary, single-run, release profile.** [T04](../tasks/T04-performance-baseline.md)
> owns the repeatable harness (`docs/benchmarks.md`, ≥3 runs, medians/p95, warmup
> rules). These numbers exist to size the design, not to gate regressions.

Commands (all from `/home/chome/Projects/hytale-server-rust`, world `testdata/Default`,
seed 0, default assets/registries resolution as in `worldgen-cli`):

```sh
# acceptance: the audited APIs still build and pass
cargo test -p worldgen-hytale --lib          # 691 passed, 0 failed
cargo run -p worldgen-cli -- bench --world testdata/Default --seed 0 --chunks 144   # debug; NOT a baseline

# release build + chunk-path measurement
cargo build --release -p worldgen-cli -p worldgen-hytale
./target/release/worldgen-cli bench --world testdata/Default --seed 0 --start 0,0 --chunks 64

# column-query probe (now the committed T04 harness)
cargo run --release -p worldgen-cli --bin query_bench -- testdata/Default 0
```

The original T01 probe ran as a temporary `worldgen-cli/examples/query_bench.rs`
and was removed after the measurements (leaving that working tree clean); T04
committed the upgraded form as `crates/worldgen-cli/src/bin/query_bench.rs`,
which reproduces and supersedes the numbers below.

### Probe results (single worker, release)

| Measurement | Result | Per sample |
|---|---|---|
| Generator load (one instance) | 0.886-0.894 s | — |
| 65,536 biome samples (512×512 @ step 2) — cold | 1.678 s | ~26 µs |
| same, second pass | 1.682 s | cache-cap thrash, see §4 |
| 16,384 biome samples (fit regime) — cold | 0.656 s | ~40 µs |
| 16,384 biome — warm | 0.200 s | ≤ ~12 µs incl. churn |
| 65,536 height samples — first pass | 64.83 s | ~0.99 ms |
| 65,536 height — second pass | 74.63 s | thrash, §4 |
| 16,384 height — first pass | 17.55 s | ~1.07 ms |
| 16,384 height — warm | 4.30 s | churn-tainted |
| 65,536 biome+height @ step 1 (256×256 blocks) — cold | 25.40 s | ~387 µs |
| same, second pass | 25.35 s | thrash |
| `get_unique_prefabs(0)` | 0.028 s → 2 entries (0 show-on-map, 1 spawn-location) | cached thereafter (~6 µs) |
| `get_spawn_points(0)` | ~8 µs → 1 transform | — |

### Chunk-path results (release, worker pool)

| Measurement | Result |
|---|---|
| 64 chunks (a 256×256-block tile at step 1), 12 workers, cold caches | **22.22 s wall** (2.88 chunks/s), avg 0.340 s/chunk |
| per-stage avg, cold: prefabs **0.266 s**, caves 0.064 s, blocks 0.006 s, environment 0.0024 s, tint/water <0.001 s | |
| archived steady state (576 chunks, 12 workers): 0.109 s/chunk, 8.39 chunks/s → the same 64 chunks ≈ **~8 s warm** | BENCHMARKS.md:25-48 |
| debug (non-baseline, acceptance command): 144 chunks, 12 workers, 193.1 s, avg 1.316 s/chunk (prefabs 1.036 s) — debug ≈ 4× release here; never publish debug numbers as baselines | |

**First-order tile cost estimates (12 workers):**

- Biome tile 256×256 px @ step 2: ~1.7 s of single-worker sampling → split bands
  across workers or accept ~1.7 s/tile/worker; fine for T15's coarse-first strategy.
- Height/relief layers: **do not** sample per column at tile scale (~65 s/tile);
  use chunk-summary heights (64 chunks ≈ 22 s cold / ~8 s warm per 256×256 blocks,
  and the summaries also serve full-block mode, caves and slices).
- Full-block tile 256×256 @ step 1: 64 chunks ≈ 22 s cold / ~8 s warm (12 workers),
  yielding every layer at once — this is the plan's "compute the surface block from
  chunks and cache summaries" path, now measured.
- The **prefab stage dominates** chunk cost (0.266 s of 0.340 s cold; 0.040 s of
  0.109 s warm). Since T22's query runs exactly that placement pass, feature search
  cost will scale with it — the T22 10k×10k benchmark is the number to watch for
  T24's ring-search viability.

---

## 8. Verified facts (file:line index)

All claims above reduce to these primary sources:

- Service/concurrency: `worldgen.rs:58-63` (worker count), `:66-79`
  (`WorldGenRequest`), `:113-139` (result/errors), `:155-158` (`WorldGen` trait),
  `:329-351` + `:367-413` (stats, 100-chunk warmup), `:421-436` (service struct),
  `:440-504` (spawn + readiness barrier), `:587-666` (per-worker generator),
  `chunk_generator_execution.rs:66-81/:174-218` (stage timings, per-stage cancel).
- Column queries: `chunk_generator.rs:281-295` (`get_core_data`), `:308-310`
  (`get_cave`), `:319-321` (`get_unique_prefabs`), `:328-334` (biome), `:337-348`
  (height/put), `:355-369` (biome counts), `:751-789` (spawn points),
  `:792-816`/`:820-853` (height compute), `:862` (area bounds), `:871-954`
  (cache sweep/evict), `:88/:92/:96/:100/:104/:108` (cache constants).
- `!Send` evidence: `chunk_generator.rs:122-150/:175-198`,
  `zone_pattern_generator_cache.rs:20-26`, `cave_generator_cache.rs:38/:55`,
  `unique_prefab_cache.rs:33/:49`, `cache/mod.rs:21-45`.
- Biome surface: `biome.rs:23-50`, `zone_biome_result.rs:14-24/:59`,
  `core_data_cache_entry.rs:22-44`, `zone.rs:22-33`,
  `zone_generator_result.rs:44-46`, `tile_biome.rs:55/:168-170`.
- Chunk data: `chunk.rs:47-53` (327,680 blocks / 1,024 columns), `:67-74` (no
  heightmap port), `:76-145` (struct + eager alloc), `:234-250/:339-355` (block,
  fluid queries), `chunk_util.rs:13-40` (32×32×320);
  `hytale-world/src/chunk/column.rs:518-540` (private heightmap scan).
- Placement: `prefab_populator.rs:101/:104/:110-129/:137/:145-151/:182-209/:219-316/:353-523/:533/:769-831/:834-863/:868-909/:1012-1095`,
  `block_populator.rs:166-168` (`put_height`),
  `chunk_generator_execution.rs:312-323` (height table fallback),
  `height_threshold_interpolator.rs:5-9/:100-151` (42² table).
- Caves: `cave_populator.rs:74-141/:239-251`, `cave_type.rs:177-241`,
  `cave_generator_cache.rs:90-183`, `cave.rs:35-100`.
- Seeds: `world_gen.rs:69`, `world_config.rs:401-403`,
  `world_thread.rs:3145/:6209/:6223`, `seed_string.rs:19-25/:31-60`,
  `coordinate_cache.rs:30-38`; no `FromStr`/seed parser anywhere (workspace-wide
  search; only `WorldProviderKind` implements `FromStr`).
- Version tag: absent from all four worldgen crates; only
  `hytale-server/src/generation.rs:29` (`"0.0.0"`, unused by any cache/output path).
- CLI inventory: `worldgen-cli/src/main.rs:49-63/:104-139` (world args),
  `:291-371` (render-map), `:393-408` (validate), `:414-493` (bench: what it
  measures); no command benchmarks column queries or feature enumeration.

---

## 9. Open questions (live game / asset pack required)

| Question | Why it is open | Owner |
|---|---|---|
| Do string seeds exist in the real game, and with what hash? | No parser exists in any crate; `SeedString` hashes *loader labels*, not user input. Needs the Java source, launcher, or a live server. Do not guess `String.hashCode()`. | **T02** |
| Is `i64 → i32` truncation the game's real behavior, and do high bits affect UUID/weather/spawn? | The cast sites are in this repo's server, but whether they match Hytale needs the Java source or a live save. | **T02** |
| Does `testdata/Default` + default registries reproduce the live game's generation? | All numbers above used the repo's golden world copy; fidelity pairs from the actual game are the real guarantee. | **T05** (fixture suite) |
| Do real-world configs define `show_on_map` unique prefabs (seed 0 in `testdata/Default` has 0)? | The T23 catalog/overlay is only interesting with real content. | **T03/T23** |
| Biome palette source and licensing (texture averages vs hand-picked `MapColor`)? | `Biome::get_map_color() -> i32` exists, but the plan wants block-texture-derived colors for full-block mode; needs the asset pack. | **T03/T20** |
| Cave-entrance density on the real asset pack | The entrance predicate is cheap, but candidate density per `CaveType::get_entry_point_generator` is pack data; the T27 10k×10k benchmark must use the real pack. | **T27** |
| How many prefab patterns use non-empty `base_checks`? | Decides whether the T22 query must replicate blocks-stage surface heights (§1 nuance) — a one-off measurement against the real pack. | **T22** (acceptance criterion) |

---

## 10. Acceptance gate (run and passed)

```text
cargo test -p worldgen-hytale --lib
  → test result: ok. 691 passed; 0 failed; 0 ignored  (0.70s)

cargo run -p worldgen-cli -- bench --world testdata/Default --seed 0 --chunks 144
  → workers 12, wall 193.13 s, 0.746 chunks/s, avg 1.316 s/chunk (prefabs 1.037,
    caves 0.232, blocks 0.038)  [debug profile; release numbers in §7]
```

No production code was changed in `hytale-server-rust`; the only artifact added there
(the temporary `query_bench` example) was removed after the measurements, leaving
that working tree clean. The probe source is preserved in this repo at
`tools/query_bench.rs` for T04 to turn into the committed harness.
