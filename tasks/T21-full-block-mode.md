# T21 — Full block mode and progressive refinement

**Status:** done (deliverable: `map-core/src/render/surface.rs` — the
full-block tile renderer: `surface_color` = T20's palette formula over
the sample's block id and the column's runtime `TintContainer` tint
(`tint_rgb`, map_image parity), wet columns blended with T17's exact
`fluid_blend` curve, the optional hillshade reusing T17's extracted
`hillshade` factor (border 1 only when on), every pixel alpha 255
(unknown blocks are the palette's visible fallbacks; true void is the
skeleton's margin) — plus `WorldSource::surface_region_cancelled`
(default: ignore; the worldgen override checks the caller's token
between chunk generations so a cancelled tile stops paying for chunks),
and the scheduler's `TilePass`/`schedule_passes` with `REFINE_PENALTY`
(refine jobs queue behind every fill job; `cancel_not_serving` takes
the pass union). The app: `Surface` joins the checkboxes, the frame
request splits into a step-4 coarse FILL + fine REFINE pass for the
surface when the effective step is finer than 4 (other layers ride one
fine pass; coarse tiles exist only as `covering_placeholder` parents,
never painted), paint allows placeholder crops for every layer, a
worker-cap DragValue feeds the next load's generator pool, and the
full-block hint names the current sampling. Acceptance green:
`cargo test -p map-core surface_render` (7 + the gated fixture test at
seed 0 — `Soil_Gravel_Sand_White`/`Water_Source`, pixel-exact — and
per-chunk cancellation, both RAN), `cargo test -p map-core refinement`
(fill-before-refine order, key separation + placeholder bridge,
refinement-only cancellation), map-core 213 / map-app 71 / map-cli 4
passed with `--features test-support`; the release benchmark is
recorded in `docs/benchmarks.md`'s T21 section: 64-chunk step-1 tile
cold 24.40/14.25/21.69 s at 1/4/12 workers, warm 1.276/0.464/0.136 s —
the coarse placeholder as the default experience, exactly as the risk
note demands.)

**Milestone:** 5 — Full block mode
**Depends on:** T17, T19, T20
**Blocks:** T28, T32, T33
**Plan refs:** [§5.2](../hytale-seed-map-plan.md#52-resolution-modes), [§5.3](../hytale-seed-map-plan.md#53-layers), [§10.2](../hytale-seed-map-plan.md#10-risks-and-open-questions)
**Agent fit:** high

## Goal

Implement the full-block layer: one sample per block, showing the actual
surface block and fluid instead of a biome color, with a progressive coarse
placeholder and a hard budget on chunk generation. This is the most expensive
mode and must not make the app feel broken.

## Context

T09 provides `surface_region` from compact `ChunkSummary` caches. A 256×256
full-block tile covers 8×8 chunks (64 chunks) at 1×1; at 2×2 it covers 16×16
chunks (256 chunks). Generating a chunk is expensive (~0.1 s release on the
reference machine), so a tile can take seconds unless the work is cached and
parallelized.

The plan's mitigations:

- only compute at zoom levels where the detail is visible;
- use the surface summary, not full chunks, and cache them;
- show a coarse placeholder instantly and refine progressively;
- show "generating N tiles" and let the user cap worker threads.

## Scope

### In

- `map-core/src/render/surface.rs`:
  - `SurfaceRenderer` using `SurfaceGrid` + `BlockPalette`;
  - optional hillshade from heights (reuse T17);
  - fluid blending (T17's function);
  - cancellation per chunk/row.
- Progressive refinement:
  - request a coarse tile (step 4 or 8) first, then step 2, then step 1 when
    the zoom allows;
  - mark the tile key/step so the cache can hold both and the view can swap;
  - scheduler support for "refine after coarse" priority (T15 extension).
- UI:
  - a "Full block" toggle / layer selection;
  - auto-select step 1 only when the display scale is at least 1 px/block; at
    coarser zoom, show step 2/4 and a hint;
  - a worker cap control (for example 1–N) that affects the generator pool;
  - "generating N tiles/chunks" indicator.
- Tests and a benchmark for full-block tile latency at 1×1 and 2×2.

### Out

- Cave layers (T28).
- PNG export (T32; it reuses the renderer).
- A `wgpu` pipeline or GPU chunk meshing.

## Work

1. Implement `SurfaceRenderer`:
   - `surface_region(rect, step)`; for each sample, color =
     `palette.color(block, biome_tint)`; if fluid/depth > 0, blend with the
     fluid color (T17);
   - apply the same hillshade formula as T17 if the relief option is enabled;
   - alpha 255 for known, transparent/void for unknown? Prefer magenta for
     unknown blocks so holes are visible; use transparency only for true void.
2. Progressive requests:
   - when the layer is enabled, request the coarsest step that covers the
     viewport (for example step 8), insert into the cache under its own key;
   - once ready (or after a short delay), request step 2, then step 1 if
     `display_scale >= 1.0` and the user's min step allows;
   - the view should paint the best available step for each tile, scaling
     coarser tiles as placeholders (T16).
   - Do not request step 1 for every tile at once; prioritize the center tile
     and the tiles under the viewport, then the border.
3. Scheduling:
   - add a `refine` priority or a second pass in `TileScheduler` so refinement
     jobs do not starve the initial fill;
   - cap the number of concurrent full-block chunk jobs to the generator worker
     count; do not queue 256 chunk requests per tile.
   - split a tile request into chunk batches and update the tile progressively
     if the renderer supports partial tiles. If that is too complex, start with
     whole-tile refinement and document it.
4. Cost controls:
   - show a confirmation/hint when the user selects 1×1 at a very coarse zoom;
   - cap the maximum chunks per frame or per second;
   - expose the worker cap (T33 can tune the default).
5. Tests:
   - a 64×64 step-1 tile matches the surface block/fluid from a directly
     generated chunk for a fixed seed;
   - a missing palette entry renders the fallback, not transparent;
   - fluid depth/color;
   - cancellation stops a tile before all chunks are generated;
   - refinement order: coarse tile appears before the step-1 tile in a fake
     renderer test;
   - cache keys separate steps and do not reuse a coarse tile as a fine one.
6. Benchmark:
   - 256×256 step 1, cold/warm summary cache, 1/4/12 workers;
   - 256×256 step 2;
   - report chunks generated and wall time; compare to T04.

## Acceptance

```sh
cargo test -p map-core surface_render
cargo test -p map-core refinement
cargo run -p map-app
```

- Full-block mode shows actual surface blocks and water, not biome colors.
- A coarse placeholder appears before the 1×1 tile finishes.
- Changing the seed/zoom does not show a full-block tile from the old state.
- The worker cap changes throughput and the UI remains responsive.
- A full-block tile at 1×1 for a fixed seed matches the fixture blocks.
- The benchmark numbers are recorded in `docs/benchmarks.md`.

## Tests

- `map-core/tests/surface_render.rs`: fixture blocks, fluids, fallback.
- `map-core/tests/refinement.rs`: fake progressive renderer, order and
  cancellation.
- Manual: enable full block at 1 px/block, pan quickly, verify placeholders and
  no unbounded queue.

## Risks / notes

- 1×1 at a zoomed-out view is wasteful. The plan explicitly says to compute
  only where the user can see the detail; enforce it in the UI and scheduler.
- If the summary cache is cold, a full-block tile can take many seconds. Make
  the coarse placeholder the default experience, not a fallback.
- Do not hold a lock across chunk generation. The summary cache must allow
  concurrent misses.
