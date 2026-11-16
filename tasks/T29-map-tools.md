# T29 — Grid, go-to, spawn/origin and measurement

**Status:** proposed
**Milestone:** 9 — Tools and persistence
**Depends on:** T16, T23
**Blocks:** T30, T32
**Plan refs:** [§5.5](../hytale-seed-map-plan.md#55-overlays-vector-drawn-on-top-not-baked-into-tiles), [§7](../hytale-seed-map-plan.md#7-ui-layout), [§1](../hytale-seed-map-plan.md#1-scope)
**Agent fit:** medium

## Goal

Add the core map utilities: chunk/region grid overlays that adapt to zoom, a
robust go-to dialog, the spawn/origin markers, and a measurement tool with
distance and direction. These are small features that make the map usable for
actual navigation.

## Context

T16 provides the viewport transform; T18 has a basic go-to parser; T23 has the
overlay layer. The plan's overlay list:

- chunk and region grid (lines adapt to zoom);
- spawn point and origin crosshair;
- measurement tool (distance, plus a direction readout);
- search result markers (T26).

Hytale constants:

- chunk size = 32 blocks (`hytale_math::chunk_util::SIZE`);
- a "region" size must be verified. Do not guess 512; check the world gen /
  server region size and document it. The grid spacing is configurable.

## Scope

### In

- `map-app/src/overlays/grid.rs`:
  - chunk grid every 32 blocks, region grid every N blocks (verified);
  - line width/opacity adapt to zoom; hide a grid when lines would be denser
    than a few pixels;
  - labels at the top/left (optional; chunk coordinates).
- `map-app/src/overlays/markers.rs`:
  - origin crosshair at `(0,0)`;
  - spawn marker from `ChunkGenerator::get_spawn_points(seed)` (via the source
    adapter); a spawn list with multiple points is possible; show the first or
    all;
  - labels/tooltips.
- Go-to:
  - a modal with `x`, `z` (and optional `y` ignored for the 2D map);
  - accepts `/tp x y z` paste, `x,z`, whitespace;
  - centers the view and places a temporary marker;
  - recent go-to history (optional; T31 persists).
- Measurement:
  - click once to start, move to preview, click again to finish;
  - show a line, distance in blocks, delta X/Z and a compass/direction label;
  - `Esc` cancels; a second measurement replaces the first;
  - copy the distance/endpoints.
- Tests for grid spacing/visibility, marker placement, measurement math and
  go-to parsing (extend T18).

### Out

- Chunk/region boundary data from the server (the grid is geometric).
- Waypoints/annotations (T30).
- Terrain-aware path measurement; this is straight-line only.

## Work

1. Verify the region size:
   - search `hytale-server-rust` for region/chunk constants and docs;
   - if the game has no fixed region grid, use a configurable "region" size
     (default 512 or 1024) and label it "region (configured)".
   - Document the constant and its source.
2. Implement pure grid geometry in map-core or map-app:
   - `grid_lines(rect, spacing, min_px) -> Vec<[BlockPos;2]>`;
   - skip lines when the screen spacing is below `min_px`;
   - test negative coordinates and line alignment at 0.
3. Draw:
   - chunk grid: thin, low-contrast lines; region grid: brighter/dashed;
   - labels only at high zoom; avoid clutter;
   - toggles in the left panel (`Chunk grid`, `Region grid`).
4. Markers:
   - origin: a crosshair with `0,0`;
   - spawn: fetch `spawn_points` from the source (cache per seed); draw a
     distinct icon and label; if the source fails, show a warning, not a
     panic;
   - clicking copies coordinates.
5. Go-to:
   - reuse T18's parser; add `/tp` support and a temporary marker;
   - clamp or reject out-of-world coordinates with the generator bounds;
   - center the view without changing zoom (or offer "zoom to fit"? keep
     simple).
6. Measure:
   - `MeasureState { start: Option<BlockPos>, current: Option<BlockPos> }`;
   - pointer move updates `current`; click sets start/finish; `Esc` clears;
   - `distance = hypot(dx, dz)`, direction = compass label from `atan2`
     (screen/world convention documented);
   - draw the line and a label near the midpoint;
   - copy `x1 z1 -> x2 z2 (distance)`.
7. Tests:
   - grid spacing at zoom levels 0/2/4 and negative rects;
   - spawn marker at a fixture seed;
   - measurement distance/direction for axis-aligned and diagonal lines;
   - go-to `/tp -1203 455` parsing;
   - `Esc` clears.
8. Manual: toggle grids at high/low zoom, go to a negative coordinate, measure
   a known chunk diagonal (32√2 blocks), check the spawn marker.

## Acceptance

```sh
cargo test -p map-core grid
cargo test -p map-app measure
cargo run -p map-app
```

- Chunk/region grids align with block 0 and hide when too dense.
- The spawn marker is correct for the fixture seed; origin is at `(0,0)`.
- Go-to accepts plain coordinates and `/tp` strings, including negatives.
- Measurement shows the correct distance/direction and can be cancelled.
- All overlays can be toggled without affecting tile caches.

## Tests

- Pure grid/measure tests in map-core or map-app.
- A fixture test for spawn coordinates (T05).
- Manual checklist for zoom-dependent grid visibility.

## Risks / notes

- Do not hard-code a region size without evidence. The plan says "regions" but
  Hytale's chunk/region terminology may differ; document the chosen constant.
- Drawing hundreds of grid lines every frame is fine, but cull off-screen lines
  first.
- Measurement is straight-line, not pathfinding; label it clearly.
