# T01 — Worldgen API audit and gap matrix

**Status:** proposed
**Milestone:** 0 — API audit
**Depends on:** —
**Blocks:** T05, T06, T22
**Plan refs:** [§2](../hytale-seed-map-plan.md#2-what-the-explorer-needs-from-your-worldgen), [§10.1](../hytale-seed-map-plan.md#10-risks-and-open-questions), [§10.3](../hytale-seed-map-plan.md#10-risks-and-open-questions), [§11.1](../hytale-seed-map-plan.md#11-next-actions)
**Agent fit:** medium

## Goal

Produce the authoritative capability matrix between the plan's requirements and
the existing world generator. Every later task must know whether it can use an
existing API, needs a small addition to `hytale-server-rust`, or must design a
workaround. This task does not implement the explorer; it removes guesswork.

## Context

The generator lives at `/home/chome/Projects/hytale-server-rust`:

- `crates/worldgen-hytale/src/world_gen.rs` — `WorldGen` trait,
  `WorldGenService`, priority queue, cancellation, `WorldGenStats`.
- `crates/worldgen-hytale/src/chunk/chunk_generator.rs` — the hot-path
  generator with `get_zone_biome_result_at`, `get_height`,
  `generate_height_between`, `get_cave`, `get_unique_prefabs`,
  `get_spawn_points`.
- `crates/worldgen-hytale/src/chunk/populator/prefab_populator.rs` — the only
  full prefab-placement pass. Candidate collection is private and currently
  coupled to `ChunkGeneratorExecution`.
- `crates/worldgen-hytale/src/chunk/populator/cave_populator.rs` — cave entry
  points and the authoritative entrance predicate.
- `crates/worldgen-hytale/src/biome/biome.rs` — `Biome` trait, including
  `get_map_color`, `get_name`, `get_id`.
- `crates/worldgen-hytale/src/container/unique_prefab_container.rs` —
  `UniquePrefabEntry` with `is_show_on_map`, `is_spawn_location`, bounds.
- `crates/worldgen-model/src/registry.rs` — block/fluid/environment registry
  names and metadata.
- `crates/worldgen-cli/src/main.rs` — existing headless `render-map`, `bench`,
  `dump-chunk`, `validate` commands.
- `done/world-generation/ARCHITECTURE.md`, `BENCHMARKS.md`, `REMAINING.md` —
  the archived porting docs.

The explorer is a separate workspace. Treat the generator as an external
dependency unless a task explicitly says to add a query API there (T22/T27).

## Scope

### In

- Audit every row of plan §2 against public APIs and document the exact call
  chain, not just the type name.
- Identify what can be computed without generating a `GeneratedChunk` and what
  cannot.
- Identify thread-safety constraints: `ChunkGenerator` is `Rc`-based; find the
  existing safe concurrency boundary (`WorldGenService`) and the cheapest way
  for the explorer to run many column queries from UI/tile workers.
- Identify the seed type and version-tag gaps.
- Record benchmark commands and a first-order cost estimate for a 256×256 tile
  at step 2 and step 1.
- Recommend the smallest worldgen additions, with proposed signatures and
  owning task IDs.

### Out

- Implementing the query API (T22/T27) or the explorer adapter (T08/T09).
- Changing generator behavior, golden data or the plan.
- Final palette/licensing decisions (T03).

## Deliverable

`docs/worldgen-api-audit.md` containing:

1. The capability matrix (below).
2. A "safe concurrency" section explaining which types cross threads and why.
3. A "gaps" list with priority (`must`, `should`, `stretch`), proposed API,
   and owning task.
4. A "verified facts" section with file:line references.
5. An "open questions" section for anything that still needs a live asset pack
   or a live Hytale server.

## Work

1. Read the archived worldgen architecture and benchmark docs, then the current
   public APIs. Use `rg` for discovery and read only the matched sections.
2. Fill in this matrix with exact signatures and file:line evidence:

   | Plan need | Existing capability | Cost / caveat | Gap |
   |---|---|---|---|
   | Biome at `(x,z)` without blocks | `ChunkGenerator::get_zone_biome_result_at(seed,x,z) -> ZoneBiomeResult`; `ZoneBiomeResult::get_biome()`; `Biome::get_name/get_id/get_map_color` | Per-column cache (50k/20s); no chunk generation. Batch calls are required for tiles. | `biome_region` convenience only. |
   | Surface height | `ChunkGenerator::get_height(seed,x,z)`, `generate_height_between` | Cheap; cached core data. | None. |
   | Top block / fluid depth | Only via a generated `GeneratedChunk` (`get_block`, `get_fluid_id`, `get_fluid_level`) | Requires full chunk generation and a large chunk buffer (~10 MB). Extract a compact column summary and drop the chunk. | `surface_at` / `surface_region` with summary cache. |
   | Feature placement without terrain | Unique prefabs: `get_unique_prefabs(seed)`, `get_spawn_points(seed)`. Biome prefabs: private `PrefabPopulator` candidate pass. | The prefab pass uses height/layer/cover data, not generated blocks, but is coupled to `ChunkGeneratorExecution`. | Public `features_in(rect, filter)` placement query. |
   | Cave entrances | `CaveType::get_entry_point_generator`, `is_entry_threshold`, `is_height_threshold`; authoritative filter in `CavePopulator::run`. | Cave graph itself is expensive; entrance filter needs only zone/biome + height. | Public `cave_entrances_in(rect)`. |
   | Cave solidity / column profile | `ChunkGenerator::get_cave(cave_type, seed,x,z)` returns a `Cave`; actual carve state needs a generated chunk. | Cave graph generation is cached; block state requires chunk. | Slice extraction from chunks; optional column profile. |
   | Deterministic, thread-safe sampling | `ChunkGenerator` is `Rc` and not `Send`/`Sync`; `WorldGenService` owns one generator per worker thread and is the safe boundary. | A `WorldGenService` request generates a whole chunk; no cheap column query service. | Explorer-side worker pool (T08) or a worldgen query pool. |
   | Version tag | No `GENERATOR_VERSION` constant. | Tile keys and exports need one. | Add a constant in `worldgen-hytale` (T02). |
   | Seed semantics | `WorldGenRequest.seed: i32`; world config seed is `i64` and is truncated with `as i32` before generation. | String seeds are not parsed by the generator. | Verify Hytale seed semantics in T02. |

3. Check the costly paths:
   - `ChunkGenerator::get_zone_biome_result_at` and `get_height` cache sizes and
     eviction; are they safe under many parallel readers?
   - `PrefabPopulator::collect_prefabs` / `collect_prefab` / `is_base_valid`:
     confirm no `GeneratedChunk` access is required.
   - `CavePopulator::run`: confirm the entrance predicate does not require the
     cave graph.
   - `get_spawn_points`: confirm it is cheap enough for a spawn marker.
4. Measure a first-order baseline (detail in T04):
   - 65,536 `get_zone_biome_result_at` samples (one 256×256 tile at step 2).
   - 65,536 `get_height` samples.
   - one 256×256 tile at step 1 via chunk generation (how many chunks, how
     long on N workers).
   - enumeration of unique prefabs for one seed.
   Write the numbers and the exact commands into the audit doc, marked as
   preliminary and linked to T04.
5. Recommend the API split:
   - Explorer-side `GeneratorPool` (T08) versus a new worldgen query service.
   - A compact `ChunkSummary`/`SurfaceGrid` extraction step (T09) versus
     retaining `GeneratedChunk` in the cache.
   - A placement query module (T22) versus duplicating populator logic.
   State the decision and the reasons; do not leave both options open.
6. Flag every assumption that needs a live asset pack or the real game, and
   assign it to T02/T03/T05.

## Acceptance

```sh
cd /home/chome/Projects/hytale-server-rust
cargo test -p worldgen-hytale --lib
cargo run -p worldgen-cli -- bench --world testdata/Default --seed 0 --chunks 144
```

- `docs/worldgen-api-audit.md` exists, has the full matrix, file:line
  evidence, the gap list and the recommended API split.
- Every plan §2 row is marked `existing`, `needs wrapper`, or `needs worldgen
  API`, with an owning task.
- The audit explicitly answers: "Can features be enumerated without generating
  terrain?" with evidence from the code.
- The audit explicitly answers: "What is the cheapest way to get a surface
  block?" with measured numbers.
- No production code is changed in this task.

## Tests

- Documentation-only task. The acceptance commands prove the audited APIs
  still build and the baseline runs.
- If a small probe is added under `tools/`, it must be a separate binary or
  script and must not become a dependency of the explorer.

## Risks / notes

- The prefab placement pass is the highest-risk gap. If the audit finds a
  hidden terrain dependency, T22 must be re-scoped before UI work starts.
- The generator's per-worker cache behavior means "cheap" queries are not
  necessarily cheap in parallel. Measure before choosing the worker count.
- Do not infer seed semantics from the `i64 -> i32` cast alone; T02 must verify
  the intended Hytale behavior.
