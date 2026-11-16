# T09 — Chunk cache, surface and slice queries

**Status:** proposed
**Milestone:** 1 — Scaffold
**Depends on:** T07, T08
**Blocks:** T11, T15, T17, T18, T20, T28
**Plan refs:** [§2](../hytale-seed-map-plan.md#2-what-the-explorer-needs-from-your-worldgen), [§5.2](../hytale-seed-map-plan.md#52-resolution-modes), [§5.4](../hytale-seed-map-plan.md#54-caves)
**Agent fit:** high

## Goal

Fill the expensive half of `WorldSource`: surface blocks, fluid depth and
horizontal cave slices. Do it without retaining full `GeneratedChunk` buffers
(~10 MB each), by extracting compact per-chunk summaries and caching those.

## Context

A `GeneratedChunk` contains several 320×32×32 arrays (blocks, rotation, filler,
tint, environment column, environment, fluid, fluid level). Retaining a 256×256
full-block tile's 8×8 chunks would cost hundreds of megabytes and blow the
plan's 512 MB cache budget. The explorer only needs:

- per column: surface height, top block, fluid id/depth, tint, biome id;
- per horizontal slice: solid/fluid/air at a given Y;
- optionally: a count of cave/air cells below the surface for the density map.

Generate the chunk once on a worker, extract these compact views, drop the
chunk, and cache the summaries. The full chunk is an implementation detail.

## Scope

### In

- `map-core/src/source/chunks.rs`:
  - `ChunkKey { identity, cx, cz }`;
  - `ChunkSummary` with per-column surface data and optional slice/occupancy
    data;
  - `ChunkSummaryCache` (moka or a small LRU) with a byte weigher;
  - worker command `GenerateSummary` and deduplicated in-flight requests;
  - `WorldSource::surface_at`, `surface_region`, `slice_region`;
  - out-of-generatable-area handling.
- Integration with T08's worker pool.
- Tests against direct `GeneratedChunk` values.

### Out

- The full-block palette/renderer (T20/T21).
- Cave density map UI (T28) beyond the raw summary fields it needs.
- Feature placement (T22).

## Chunk summary

Proposed fields per 32×32 chunk:

```rust
pub struct ChunkSummary {
    pub cx: i32,
    pub cz: i32,
    pub height: [i16; 1024],       // or Vec<i16>, surface Y
    pub block: [i32; 1024],        // top block id
    pub fluid: [i32; 1024],        // top fluid id (0 = none)
    pub fluid_depth: [u16; 1024],
    pub tint: [i32; 1024],
    pub environment: [i32; 1024],
    pub cave_cells: [u16; 1024],   // optional: air/fluid below surface
    pub slice_bits: Option<SliceBits>, // optional, generated on demand
}
```

Size estimate: 1024 × (2 + 4 + 4 + 2 + 4 + 4 + 2) ≈ 22 KB per chunk, plus an
optional slice. A 512 MB budget holds tens of thousands of summaries; still
bound it explicitly.

`SurfaceSample` is built on demand from the summary plus the biome query. The
biome can come from the generator's column cache, not the chunk.

`SliceGrid` for a rect/step/Y:
- map each sample to its chunk and local x/z;
- request the chunk summary; for a slice, either use a per-chunk `slice_bits`
  for that Y or retain a compact occupancy representation;
- classify `Solid(block_id)`, `Fluid(fluid_id)`, `Air`, `Unknown`.

If a slice needs exact block ids at Y, extract a `32×32` block-id slice when
generating the chunk and cache it keyed by `(chunk, y)`. Do not cache every Y
by default; cache the last few requested slices.

## Work

1. Implement `ChunkKey` including the generator identity and seed. Do not key
   only by chunk coordinates; a seed change must not hit the old summary.
2. Add a worker command:
   ```rust
   GenerateSummary { cx, cz, reply: Sender<Result<Arc<ChunkSummary>, SourceError>> }
   ```
   The worker first checks the shared summary cache. On a miss it generates the
   chunk with `generate_chunk` (or the worker's existing generator path),
   extracts the summary, drops the chunk, and inserts it.
3. Deduplicate concurrent requests for the same key:
   - use `moka`'s `get_with` if the generation can run inside the cache loader,
     or an in-flight map of `Arc<OnceLock<Result<...>>>`;
   - avoid two workers generating the same chunk twice.
4. Implement `surface_at`:
   - rect → chunk/local;
   - get summary;
   - get biome via `biome_at` (or a batched biome query);
   - combine into `SurfaceSample`.
5. Implement `surface_region`:
   - split the rect into chunks;
   - request all missing summaries in parallel across workers, nearest to the
     region center first;
   - sample the requested step from the summaries;
   - check cancellation between chunks.
6. Implement `slice_region`:
   - request summaries for the rect;
   - read the exact block/fluid at `y`; if the summary lacks a slice, extract
     it from a freshly generated chunk and cache by `(chunk, y)`;
   - return `SliceCell` per sample.
7. Handle out-of-generatable-area:
   - `WorldGenService::is_chunk_outside_generatable_area` or the generator's
     min/max chunk bounds;
   - return `SourceError::OutOfBounds` or an `Unknown` cell; document the
     behavior. The UI should not crash at world edges.
8. Add memory accounting: `ChunkSummary::byte_size()` and a weighted cache.
   Default to a configurable budget (for example 256 MB for summaries, separate
   from the tile cache budget).
9. Add tests:
   - for a fixed seed/chunk, compare `surface_at` height/block/fluid/depth with
     a directly generated `GeneratedChunk`;
   - negative coordinates, chunk edges and a sample at `x = -1, z = -1`;
   - `surface_region` at steps 1, 2 and 4;
   - a slice at several Y values matches the chunk block/fluid ids;
   - cancellation stops a large region;
   - changing the seed does not reuse a summary;
   - cache eviction under a tiny budget still returns correct data.
10. Add a benchmark for a 256×256 full-block tile: number of chunks, time,
    cache hit rate. Report to T04/T33.

## Acceptance

```sh
cargo test -p map-core --test surface_slice
cargo test -p map-core --test chunk_cache
cargo clippy -p map-core --all-targets -- -D warnings
```

- Surface samples match a directly generated chunk for every fixture column.
- No full `GeneratedChunk` is retained after summary extraction.
- A 256×256 full-block tile can be produced without exceeding the configured
  summary cache budget.
- Slice queries return exact block/fluid ids at the requested Y.
- Out-of-bounds queries return a typed error/Unknown, not a panic.
- Seed/version changes invalidate summaries.

## Tests

- `map-core/tests/chunk_cache.rs`: cache key, eviction, deduplication.
- `map-core/tests/surface_slice.rs`: direct chunk comparison, edges, fluids,
  slice at `y = 0`, `y = 64`, `y = 319`.
- Bench probe under `map-core/benches/` or a `map-cli` subcommand, not in the
  default test suite.

## Risks / notes

- Surface extraction must use the **top solid block**, not blindly
  `height + 1`; prefabs, covers and water can change the top.
- If a chunk is generated at step 2 but the summary is keyed by chunk, the
  same summary serves all steps. Do not key the cache by step.
- Concurrency: generation is expensive; duplicate generation is a real risk.
  Deduplicate before optimizing.
- The summary cache and tile cache have separate budgets; document both.
