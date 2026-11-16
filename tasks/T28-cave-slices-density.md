# T28 — Cave Y-slice layer and density heat map

**Status:** proposed
**Milestone:** 8 — Caves
**Depends on:** T09, T21, T27
**Blocks:** T33
**Plan refs:** [§5.4](../hytale-seed-map-plan.md#54-caves), [§6.3](../hytale-seed-map-plan.md#63-cave-search)
**Agent fit:** high

## Goal

Add the two top-down cave views the plan asks for: a horizontal Y-slice with a
slider, and a cave-density heat map for finding cave-rich areas. Both reuse the
chunk summary/slice pipeline from T09 and the cave entrance overlay from T27.

## Context

The plan's three cave views:

1. cave entrances overlay — T27;
2. Y-slice view: horizontal slice at a chosen Y, with a slider; solid/air/fluid
   shown in a tile layer keyed by `(…, y)`;
3. cave density heat map: a coarse top-down view showing how much of each
   column is cave.

`WorldSource::slice_region(rect, step, y)` from T09 already returns
`SliceCell::{Solid, Fluid, Air, Unknown}` per sample. A cave density value per
column can be derived from a generated chunk:
`cave_cells[column]` = count of air/fluid cells below the surface (or below a
fixed Y), normalized by height. T09's `ChunkSummary` has an optional field for
this; this task decides the exact metric.

## Scope

### In

- `map-core/src/render/caves.rs`:
  - `CaveSliceRenderer` — solid/air/fluid colors at a given Y; a Y slider
    (or numeric input) in the UI;
  - `CaveDensityRenderer` — per-column cave fraction heat map;
  - a `LayerId::CaveSlice { y }` / `LayerId::CaveDensity` mode in the tile key;
  - caching: slice tiles keyed by Y; density tiles keyed by the metric/version.
- Source extensions:
  - `slice_region` is already in T09; add a density query (or fill
    `ChunkSummary::cave_cells`) with a clear definition.
- UI:
  - cave layer selection, Y slider (0..319, default near sea level or the last
    used Y), density toggle;
  - status/legend showing the Y and the color scale;
  - only the cave layer is invalidated when Y changes; other tiles stay
    cached.
- Tests:
  - slice at several Y values matches direct chunk block/fluid ids;
  - density values match a directly computed count for fixture columns;
  - Y change invalidates only cave-slice keys;
  - negative Y/out-of-world cells are `Unknown`, not a panic.

### Out

- 3D cave rendering/meshing.
- Cave type search (T27 stretch).
- Editing or lighting.

## Work

1. Decide the slice color scheme:
   - solid: dark grey/black;
   - air/cave: transparent or light blue? The plan wants solid/air/fluid shown;
     use a dark solid, transparent air, blue fluid, with an optional
     "cave only" view that highlights air.
   - include a legend and a "show solid" toggle if needed.
2. Implement `CaveSliceRenderer`:
   - request `slice_region` for the tile rect/step/Y;
   - map cells to RGBA; apply a subtle depth/height tint if useful;
   - cancellation per row.
3. Define the density metric:
   - `cave_cells / column_height_below_surface` or
     `cave_cells / (surface - min_y)`; document what counts as cave (air below
     the surface, excluding water?) and whether prefab air counts.
   - Use the T09 chunk summary; if it is not available, compute from a
     generated chunk and cache the per-column count.
4. Implement `CaveDensityRenderer`:
   - color ramp (low = transparent/dark, high = bright orange/red);
   - optional contour/legend.
5. UI:
   - add a "Caves" layer group with a slice/density choice and a Y slider;
   - changing Y updates `LayerId::CaveSlice { y }` and requests only that
     layer;
   - keep the previous Y tile cached until the new one arrives (placeholder
     behavior from T16).
6. Tests:
   - a synthetic 3D chunk with known solid/air/fluid at Y values;
   - direct comparison with a generated fixture chunk at `y = 0`, `32`, `64`,
     `128`, `256`, `319`;
   - density count for a column with a known cave fraction;
   - key difference includes Y and layer;
   - changing Y does not invalidate biome/height tiles;
   - out-of-bounds cells.
7. Benchmark: slice tile and density tile latency at step 1/2, cold/warm
   summary cache. Report to T33.
8. Manual: find a known cave, compare entrance overlay with the slice at the
   entrance height.

## Acceptance

```sh
cargo test -p map-core cave_render
cargo test -p map-core cave_density
cargo run -p map-app
```

- The Y slider changes the slice; solid/air/fluid are distinguishable.
- Changing Y does not re-render other layers.
- The density heat map highlights cave-rich areas and matches fixture counts.
- The entrance overlay and the slice agree at a known entrance.
- No full 3D chunk is retained; memory stays bounded.

## Tests

- `map-core/tests/cave_render.rs`: fixture chunks, slice Y values, density.
- `map-app` state test: Y change updates the layer key and requests only the
  cave layer.

## Risks / notes

- Air below the surface includes caves, but also overhangs/voids. Define the
  metric precisely; a "cave density" that counts all air below the surface can
  be misleading.
- Y-slice at 1×1 is expensive (chunk generation); use the summary cache and a
  coarse placeholder first.
- The slice color scheme must make solid vs air obvious; a low-contrast palette
  makes the layer useless.
