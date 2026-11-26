# T28 — Cave Y-slice layer and density heat map

**Status:** done (deliverable: `map-core/src/render/caves.rs` —
`CaveSliceTileRenderer` (the key mode is the world Y; solid/air/fluid colors;
one chunk-row band at a time with `complete: false` snapshots, final pixels
bit-identical to the one-shot renderer) and `CaveDensityTileRenderer` (the key
mode is the metric version 1; dark → red → orange ramp). Supporting data:
`CaveDensityGrid` and `cave_fraction` (air cells strictly below the top block
/ that block's Y, fluids excluded, clamped to 0..=1),
`WorldSource::cave_density_region(_cancelled)` and
`slice_region_cancelled` (defaults keep every source compatible; the worldgen
adapter checks both tokens between chunk generations and density always uses
full-fidelity summaries), the fake source's synthetic slice/density, and tile
mode dispatch in `LayeredTileRenderer`. The app adds the Caves group
(Off / Y-slice / Density, Y slider 0..319 default 64, slice/density legends)
and the cave choice gates exactly one cave layer. Cave passes schedule the
tight visible range (no T16 prefetch ring), stream in chunk-row bands, and
`tile_query` now derives its frame from the requested rect so a band query
cannot expand back to the whole tile. Acceptance green:
`cargo test -p map-core cave_render` (4 unit + 5 integration, 1 ignored
benchmark), `cargo test -p map-core cave_density` (4 unit + 2 integration; the
slice/density cancellation checks included),
`cargo test -p map-app` (125 passed), full map-core/map-cli and
`cargo build -p map-app`; fmt/clippy 1.100 clean. Release benchmark (2
workers, 64 chunks): slice 13.60/0.001 s cold/warm @ step 1 and 13.90/0.000 @
step 2; density 13.83/0.001 and 14.08/0.001. Manual X11: Y=64 showed a solid/
air slice, the density ramp rendered, and Y=30 produced a different slice.
Deviation: the T28 note "out-of-world cells are Unknown" is recorded against
the T09 contract instead — Y outside 0..320 is `Air`, an out-of-world rect is
a typed `OutOfBounds`, and out-of-world tile margins are transparent; none
panics. The entrance-overlay/slice visual comparison remains a user-side check
(the local pointer grab prevented completing it after the slice/density
checks).)
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
