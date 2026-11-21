# T22 — Feature placement query API in worldgen

**Status:** done (deliverable: worldgen repo commits `7f512f7`/`459d3af`/`6f5cc1c` —
`crates/worldgen-hytale/src/query/` (`features_in`, `FeaturePlacement`,
`PlacementFilter`, `PlacementKind` with the T27 cave-entrance hook), the
`PrefabPopulator` refactor into the shared
`prefab_placement_pass.rs` (golden chunks byte-identical),
`tests/feature_query.rs` + the parity unit test (query == actual paste list
on golden chunks), the `feature_query_bench` probe (linear in area from 512
blocks up; 10,000² ≈ 6,400 s extrapolated, ring-search budget recorded in
`docs/benchmarks.md`), and `QUERY.md`. Explorer side: no code — the
generator API is the deliverable; numbers and T24 sizing rules are in
`docs/benchmarks.md` §"Feature enumeration (T22)".)
**Milestone:** 6 — Feature overlay
**Depends on:** T01, T05, T08
**Blocks:** T23, T24, T27
**Plan refs:** [§2](../hytale-seed-map-plan.md#2-what-the-explorer-needs-from-your-worldgen), [§6.1](../hytale-seed-map-plan.md#6-search-design), [§6.4](../hytale-seed-map-plan.md#64-multi-criteria-search-seed-finder-mode), [§10.1](../hytale-seed-map-plan.md#10-risks-and-open-questions)
**Agent fit:** high

## Goal

Add a public placement query to `hytale-server-rust` that enumerates unique
prefabs and biome prefabs in a rectangle **without generating terrain**. This
is the plan's most important design rule and the prerequisite for fast feature
search.

## Context

Current state:

- Unique prefabs: `ChunkGenerator::get_unique_prefabs(seed)` returns
  `UniquePrefabEntry` values (name, category, position, bounds, rotation,
  `is_show_on_map`, `is_spawn_location`). This is already a per-seed catalogue
  and is cheap.
- Spawn points: `ChunkGenerator::get_spawn_points(seed)`.
- Biome prefabs: `PrefabPopulator` collects candidates per chunk in
  `collect_biomes`, `collect_prefabs`, `collect_prefab`, then invalidates
  overlapping candidates and pastes the survivors. The predicates use:
  - `ChunkGenerator::get_zone_biome_result_at`;
  - `ChunkGenerator::get_height` / `generate_height_between`;
  - `Biome` layer/cover/water containers;
  - `PrefabPatternGenerator` noise/height/parent/base checks;
  - unique-prefab exclusion radius.
  None of these require a generated `GeneratedChunk`; the coupling is to
  `ChunkGeneratorExecution` for chunk-relative coordinates and core data.

The task should refactor that collection into a reusable query pass rather than
duplicating the placement logic in the explorer. Duplication would drift from
the generator and produce false search results.

## Scope

### In

- A new module in `crates/worldgen-hytale/src/query/` (name is flexible):
  - `FeaturePlacement` and `PlacementFilter`;
  - `ChunkGenerator::features_in(seed, rect, filter)` or a
    `FeatureQuery::new(&ChunkGenerator, seed)` API;
  - unique prefabs, spawn points and biome prefabs;
  - optional cave-entrance hook (T27 fills the predicate).
- Refactor `PrefabPopulator` so candidate collection/invalidation can run
  against a lightweight query context:
  - extract `collect_biomes`, `collect_prefabs`, `collect_prefab`,
    `invalidate_prefabs` and the predicates into a shared helper;
  - make the paste step a separate final pass that only the real populator
    runs;
  - keep the RNG draw order identical so actual placement is unchanged.
- Tests that compare query results with actual placed prefabs in golden chunks.
- A benchmark for a 10,000×10,000 rect.
- Update worldgen docs (`ARCHITECTURE.md` / a new `QUERY.md`) and the golden
  workflow if needed.

### Out

- Cave entrance enumeration (T27) but leave the kind/filter extension point.
- Explorer-side feature catalog/icons (T23) and search rings (T24).
- Changing placement rules, weights or random draws.
- Exposing prefab entity data (still deferred in worldgen).

## Proposed API

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeaturePlacement {
    pub kind: PlacementKind,        // UniquePrefab | BiomePrefab | Spawn
    pub name: String,
    pub category: String,
    pub position: (i32, i32, i32),
    pub bounds: Option<BlockBounds>, // inclusive
    pub rotation: i32,               // or PrefabRotation
    pub show_on_map: bool,
    pub spawn_location: bool,
}

pub struct PlacementFilter {
    pub kinds: PlacementKindMask,
    pub categories: Option<Vec<String>>,
    pub name_contains: Option<String>,
    pub min_size: Option<i32>,
}

impl ChunkGenerator {
    pub fn features_in(
        &self,
        seed: i32,
        rect: BlockRect,
        filter: &PlacementFilter,
    ) -> Vec<FeaturePlacement>;
}
```

`BlockRect`/`BlockBounds` should be a small public type in the query module;
do not reuse a chunk-only type.

## Work

1. Read `prefab_populator.rs` end to end and mark every method that touches
   `ChunkGeneratorExecution`. Classify each as:
   - pure generator/biome query (reusable);
   - chunk-relative coordinate helper (replace with the query rect);
   - paste/output (skip in query mode).
2. Extract a `PlacementContext` trait or struct with:
   - `seed`, `rect`, `chunk_x/z` iteration;
   - `zone_biome_result_at`, `get_height`, `generate_height_between`;
   - core data for the collected biomes;
   - unique prefab list for exclusion checks.
   The real populator and the query pass both use it.
3. Implement the query:
   - iterate the chunks covering `rect` padded by the maximum prefab extent
     (the same padding the populator uses), or iterate a region grid;
   - collect biomes across the padded region;
   - collect candidates for each biome/entry with the same `FastRandom` seed
     and draw order;
   - run the same invalidation;
   - yield only candidates whose bounds intersect `rect`;
   - add unique prefabs/spawn points from the per-seed lists, filtered by
     bounds/kind.
   Do not paste, do not allocate a `GeneratedChunk`.
4. Preserve behavior:
   - keep `PrefabPopulator::populate` producing byte-identical chunks;
   - run the existing golden chunk tests before/after the refactor;
   - add a test-only placement recorder if needed to compare the query result
     with the actual paste list.
5. Handle filters:
   - category/name/min size/kind; apply after placement so the RNG stream is
     unchanged by filtering.
6. Document the query:
   - it is deterministic for a seed/rect/filter;
   - it may scan a padded area and is not O(1);
   - it does not generate blocks;
   - feature names/categories are the prefab names/categories, not translated
     display names.
7. Add tests:
   - unique prefabs for a fixed seed match `get_unique_prefabs`;
   - biome prefab placements for a golden chunk match the actual prefab
     voxels/placement list;
   - a small rect returns a subset of a larger rect (no duplicates, no
     missing items);
   - filter kinds/category/name/min size;
   - negative coordinates and rect edges;
   - determinism across repeated calls.
8. Benchmark:
   - 10,000×10,000 rect, one seed;
   - several smaller rects;
   - report time, placements and peak memory. Feed T04/T24/T33.

## Acceptance

```sh
cd /home/chome/Projects/hytale-server-rust
cargo test -p worldgen-hytale --test chunks_golden
cargo test -p worldgen-hytale query
cargo clippy -p worldgen-hytale --all-targets -- -D warnings
cargo fmt -p worldgen-hytale -- --check
```

- `features_in` is public and returns unique/biome prefab placements without
  generating a chunk.
- The existing golden chunk tests remain byte-identical.
- Query results match actual placements for the fixture chunks.
- A 10,000×10,000 query completes in a time that makes T24's ring search
  viable (record the number; target from T04).
- No explorer-side duplicate of the placement predicates is needed.

## Tests

- `crates/worldgen-hytale/tests/feature_query.rs`:
  - compare with `get_unique_prefabs`;
  - compare with the prefab placement list recorded during a golden chunk
    generation;
  - filters and rect subsets.
- A benchmark binary or ignored test for the 10k×10k query.

## Risks / notes

- This task edits a **different repository**. Coordinate with the main agent
  and the worldgen owner; do not copy the generator into the explorer.
- The placement pass is a long random stream. Any refactor that reorders draws
  changes every prefab. The golden chunk tests are the guard.
- If the refactor cannot preserve byte-exact placement, stop and report before
  changing the algorithm.
- The plan's seed-finder mode (§6.4) wants this API fast, but do not optimize
  prematurely; first make it correct and measure.
