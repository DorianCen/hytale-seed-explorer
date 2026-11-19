# T17 — Height, relief and fluid layers

**Status:** done (deliverable: `map-core/src/render/tiles.rs` grew the
shared per-tile plumbing — `TileQuery`/`tile_query` (T16's aligned
query generalized with a border ring, `ceil_div` moved to `coords.rs`),
`blit_patch`, the `render_layer_tile` skeleton, and
`LayeredTileRenderer` dispatching on the tile key's layer — plus
`map-core/src/render/height.rs` (`SEA_LEVEL`, the documented
`HEIGHT_COLORS` hypsometric ramp, `height_color`/`height_image`,
`HeightTileRenderer`; the task's "optional contour/shade hook, disabled
by default" is the relief layer's own existence — compositing a shade
onto other layers is T19's), `relief.rs` (one-sample-border hillshade ×
the height ramp; NW light per the T13 convention — the task's light
vector with the Z sign flipped and the flip documented; per-block slope
normalization so zoom levels shade alike; nearest-valid-height edge
clamping — no wrap, no zeros, no seams; `ReliefTileRenderer`) and
`fluid.rs` (`FLUID_COLOR`, `DEFAULT_FLUID_OPACITY`,
`fluid_color`/`fluid_image` with the depth-capped blend that never
saturates, dry pass-through; `FluidTileRenderer`); the app's radios
gained Relief/Fluid with `AppLayer::tile_layer()` driving the tile
subject (switching layers switches keys — no stale tiles by
construction), the status bar shows the layer name, and the map
placeholder distinguishes "arrives with T20/T28" from "outside the
world"; `map-cli render --layer height|relief|fluid`; tests:
`height_render_*`/`relief_render_*`/`fluid_render_*` unit tests
(3+5+4: the ramp at key elevations, light-facing brightness, flat
uniformity, edge clamping, step normalization, monotone capped depth,
dry pass-through, cancellation between row batches),
`map-core/tests/layers.rs` (9: fake-driven tile end-to-end — ramp
exactness against the queried heights, seam-free relief across a tile
boundary, wet/dry fluid, pre-query cancellation, typed unsupported for
renderer-less layers — and three generator-gated goldens on the varied
shore window at `(272, -192)`: height four bands, relief a shaded
spectrum, fluid real wet blends; byte-identical repeats), plus one
map-app layer-switching pipeline test; benchmark rows in
`docs/benchmarks.md` §Layer tiles (T04 format): height tile 20.38 s
median / relief 20.63 s at 2 workers — the 65,536-column tile exceeds
the generator's 50,000-entry column cache and pays the honest churn
(the no-chunk design this task chose; summary-derived heights stay
T33's follow-up), fluid tile 86.23 s cold / ~0.2 s warm summaries; the
task's manual `worldgen-cli render-map --mode Height` shape check was
automated and passed: the reference grayscale inverted back to heights
agrees with the explorer's height field 4,096/4,096 within the 8-bit
quantization)
**Milestone:** 4 — Layers and inspector
**Depends on:** T09, T15
**Blocks:** T19, T21
**Plan refs:** [§5.3](../hytale-seed-map-plan.md#53-layers), [§8](../hytale-seed-map-plan.md#8-milestones), [§1](../hytale-seed-map-plan.md#1-scope)
**Agent fit:** high

## Goal

Add the height, shaded-relief and water/fluid layers to the tile pipeline,
using the compact height/surface summaries from T09. These layers are the
foundation for full-block mode and for the inspector's height readout.

## Context

T09 exposes:

- `height_region(rect, step)` — cheap per-column surface heights;
- `surface_region(rect, step)` — height, top block, fluid id/depth, tint;
- `ChunkSummary` — per-chunk surface data.

The plan's layers:

- **Height:** hypsometric tint.
- **Shaded relief:** lighting from neighbor height differences.
- **Water/fluids:** depth shading.
- **Other generator data:** debug layers (T19).

Each layer converts samples to a 256×256 RGBA tile. The default biome layer
stays unchanged.

## Scope

### In

- `map-core/src/render/height.rs`:
  - `HeightRenderer` with a hypsometric ramp (deep water → shore → lowland →
    highland → snow/rock), sea level constant, clamping and alpha 255.
- `map-core/src/render/relief.rs`:
  - hillshade from height differences (north/south/east/west), a fixed light
    direction, and a neutral base color (or multiply an existing layer).
  - Handle tile edges by sampling one extra border from the source or
    extrapolating; document the choice.
- `map-core/src/render/fluid.rs`:
  - water/fluid depth shading from `SurfaceGrid` (or a fluid-specific grid);
  - blend with the underlying surface/biome color at a configurable opacity;
  - cap depth shading so deep water does not become black.
- `LayerId` variants and `TileRenderer` implementations.
- App layer controls: radio buttons for Height/Relief/Fluid; the selected
  layer replaces or overlays the biome layer per T19.
- Golden tests for synthetic and fixed-seed grids.

### Out

- Surface block mode/palette (T20/T21).
- Layer opacity/blending UI (T19).
- Caves (T28).
- Temperature/humidity debug layers (T19, data permitting).

## Work

1. Implement the height ramp as a pure function `height_color(y) -> [u8;4]`
   with a documented color table and a unit test for key elevations (below sea
   level, sea level, 64, 128, 255, 319).
2. Implement `HeightRenderer`:
   - request `height_region` for the tile rect at the tile step;
   - map heights to colors;
   - optional contour/shade hook, disabled by default;
   - cancellation per row.
3. Implement relief:
   - request heights with a 1-sample border (rect expanded by `step`);
   - compute `dhdx = (east - west) / 2`, `dhdz = (south - north) / 2`;
   - normal `(dhdx, 3, dhdz)` (or the server's formula), light
     `(-0.2, 0.8, 0.5)`, Lambert clamp 0;
   - shade = `0.4 + 0.6 * lambert`; multiply the neutral/height color;
   - ensure edge samples use the nearest in-tile height, not zero.
4. Implement fluid:
   - use `SurfaceSample.fluid`/`fluid_depth`;
   - if fluid is present, blend from the fluid color (use a simple blue/asset
     color and a depth attenuation) over the surface color;
   - dry cells pass through;
   - expose `fluid_opacity` as a setting for T19.
5. Wire the layers into the scheduler and app:
   - add `LayerId::Height`, `Relief`, `Fluid`;
   - the left panel radio selects the base layer;
   - changing the layer invalidates/requests tiles for that layer only;
   - status bar shows the layer name.
6. Add tests:
   - height color function at known elevations;
   - synthetic ramp relief produces brighter slopes facing the light;
   - a flat height grid produces uniform relief;
   - fluid depth 0/1/10 shades monotonically;
   - tile edge does not produce a black seam;
   - cancellation stops a large height region.
7. Add a benchmark row for height/relief tile latency (T04 format).

## Acceptance

```sh
cargo test -p map-core height_render
cargo test -p map-core relief_render
cargo test -p map-core fluid_render
cargo run -p map-app
```

- Height and relief render for the same seed/rect without generating full
  chunks.
- Relief has no tile-edge seams and is stable when panning.
- Fluid depth is visible where water exists and does not tint dry columns.
- Switching layers does not show stale tiles from the previous layer.
- Golden hashes are stable for fixed seed/rect/step.

## Tests

- `map-core/tests/layers.rs`: synthetic grids + fixed-seed golden hashes.
- A manual visual check against `worldgen-cli render-map --mode Height` for
  orientation/shape (colors may differ; the shape must match).

## Risks / notes

- The height values are `i32` and may be `0`/negative at world edges; clamp in
  the color function, not in the source.
- Relief requires neighbor heights. If the tile is at the world edge, use the
  nearest valid height; do not wrap.
- Do not make the default biome layer depend on height queries; the plan's
  first tile should stay cheap.
