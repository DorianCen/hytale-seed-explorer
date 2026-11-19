# T16 — Pan, zoom and viewport rendering

**Status:** done (deliverable: `map-app/src/map_view.rs` — the egui map
widget: drag pan (left/middle), wheel zoom anchored at the cursor
through T10's `zoom_at_cursor`, `+`/`=`/`-` keyboard zoom, click-to-copy
`x z`, the pure `frame_tiles` request (visible range ∩ generatable
world, center-tile priorities, unchanged-request dedup), scheduler
event drain into the T14 GPU bridge, eviction-following texture drops,
physical-pixel tile rects (the seam rule), parent-tile placeholders
cropped to exact UVs, a subtle pending tint, and off-thread pipeline
retirement; plus `map-app/src/view_state.rs` — the headless
`MapViewState` (continuous `smooth_zoom` with a ceil'd discrete
`zoom_level`, all conversions delegated to T10) with
`tile_screen_rect`/`expand_to_pixels` and 17 tests; the biome tile
renderer `BiomeTileRenderer` in `map-core/src/render/biome.rs` (one
region query per tile, token-checked before the work unit, world-edge
tiles query the sample-grid-aligned in-world sub-rect and leave
transparent margins) with 6 tests; `covering_placeholder`/
`PlaceholderTile` in `map-core/src/tiles.rs` (nearest cached coarser
level, exact power-of-two UV crop) with 6 tests; the T13 single-texture
flow removed (`MapTexture`, grid requests, `MAX_TEXTURE_SAMPLES`,
`view_texture_rect`) in favor of the T15 pipeline; `AppState` owns the
`MapViewState`; 9 map-app pipeline/frame tests (fake source + the real
generator; serve/upload/unchanged-frame/cancel/placeholder, plus the
T13-style colored-map acceptance through the tile path); supporting:
`crossbeam-channel` in map-app, a test-support dev-dependency for the
fake-source pipeline tests, the stale "flips Z" line in `coords.rs`
module docs corrected to the shipped orientation contract. Manual
checklist pending a desktop session: pan across seams, zoom at
corner/edge, fast drag, seed change while loading — each item's math
and pipeline counterpart is covered headlessly.)
**Milestone:** 3 — Tiling and navigation
**Depends on:** T12, T13, T14, T15
**Blocks:** T18, T23, T29, T30, T31
**Plan refs:** [§5.1](../hytale-seed-map-plan.md#51-tiles-and-zoom), [§7](../hytale-seed-map-plan.md#7-ui-layout), [§1](../hytale-seed-map-plan.md#1-scope)
**Agent fit:** high

## Goal

Replace the static biome texture with the real interactive map: drag to pan,
scroll to zoom at the cursor, draw the visible tile grid, show lower-detail
placeholders while tiles load, and convert clicks/hover to block coordinates.

## Context

T10 provides the pure math, T14 the cache, T15 the scheduler, T13 the first
layer. This task is the integration point and the most UI-heavy part of
milestone 3.

The plan requires:

- pan (drag) and zoom (scroll wheel, centered on the cursor);
- discrete zoom levels with smooth interpolation;
- visible tiles queued nearest-center first;
- nearest cached lower-detail tile as a placeholder;
- cancel tiles that scroll out of view;
- click to copy coordinates, later feature clicks.

## Scope

### In

- `map-app/src/map_view.rs`: a custom `egui` widget/`Response`:
  - viewport state (`center: WorldPos`, `zoom: ZoomLevel`,
    `smooth_scale: f32`);
  - drag-to-pan with pointer delta;
  - wheel zoom centered on the cursor using T10's `zoom_at_cursor`;
  - smooth interpolation between discrete zooms (animate `scale`/`center`
    over a few frames or use a continuous display scale while the tile level
    stays discrete);
  - request/release visible tiles from T15;
  - paint cached tiles at their block rects;
  - placeholder selection: for a missing `(tx,ty,step)`, search coarser steps
    (`step*2`, `step*4`, …) for a cached tile covering the rect; draw it scaled
    with the correct UVs; if none, draw a background checker/blank;
  - tile seams: use integer pixel rects or a 0.5 px inset; do not show gaps at
    adjacent tiles;
  - hover coordinate readout through a callback;
  - click → copy block coordinates (and later feature selection).
- `map-app/src/view_state.rs` (or extend T12's state) with headless-testable
  pan/zoom state.
- Keyboard/mouse:
  - drag with left/middle button;
  - wheel zoom;
  - `+`/`-` keyboard zoom optionally;
  - double-click to center? optional.
- Tests for view state and placeholder selection.

### Out

- Feature icons/search markers (T23/T24/T26).
- Grid overlays/waypoints/measure (T29/T30).
- Export (T32).
- Full-block mode (T21) beyond the layer-agnostic plumbing.

## Work

1. Define `MapViewState` in map-core or map-app (pure, no egui):
   - `center: WorldPos`, `zoom: ZoomLevel`, `smooth_zoom: f32`,
     `viewport_size: [f32; 2]`;
   - methods `pan_by_screen(delta)`, `zoom_at(screen_pos, delta)`,
     `screen_to_block`, `visible_tiles`.
   Unit-test it without a window.
2. Implement the egui widget:
   - allocate the available rect;
   - handle `Sense::click_and_drag()`;
   - on drag, pan by `pointer.delta()` (convert screen delta to world using the
     current scale);
   - on scroll, call `zoom_at(pointer_pos, scroll_y)`; keep the world point
     under the cursor fixed;
   - clamp zoom to the allowed range (T10) and optionally clamp center to the
     generator's bounds from T08.
3. Frame loop:
   - compute visible tiles for the current zoom/step;
   - call `scheduler.schedule(visible, center, renderer)` if the visible set or
     generation changed;
   - drain scheduler events, insert tiles into `TileCache`/texture cache;
   - remove textures for evicted keys.
   Do not rebuild the whole texture map every frame; use a generation marker.
4. Painting:
   - for each visible tile, compute its screen rect from the tile block rect
     and the viewport transform;
   - if cached, draw the texture with UV `(0,0)..(1,1)`;
   - if missing, find the coarsest? nearest cached parent. Prefer the closest
     step (step*2) that covers the tile; draw it with UVs cropped to the
     child's block rect. Draw a subtle "loading" tint or border so the user
     sees refinement.
   - clip to the map rect.
5. Smooth zoom:
   - keep the tile `ZoomLevel` discrete; animate a display scale factor
     between levels (for example interpolate `log2(scale)`), and scale the
     painted textures. When the animation crosses a level, request the new
     tile set.
   - Avoid a feedback loop where every frame requests differently rounded
     tiles.
6. Input/callback:
   - store `last_cursor_block: Option<BlockPos>` in app state for the status
     bar (T18);
   - on primary click (without drag), copy `x z` (or `/tp x y z`?) to the
     clipboard and show a toast; decide the exact string in T18/T31.
7. Tests:
   - `MapViewState` pan/zoom math: cursor world point fixed within tolerance;
   - visible tile range for a straddling viewport;
   - placeholder selection for a missing tile with a cached parent;
   - tile screen rect at negative world coordinates;
   - no texture request when the visible set is unchanged.
8. Manual acceptance: pan across tile seams, zoom in/out at the cursor, verify
   no gaps/flicker and that the coordinate under the cursor stays stable.

## Acceptance

```sh
cargo test -p map-app
cargo test -p map-core coords::
cargo build -p map-app
cargo run -p map-app
```

- Drag pans smoothly; wheel zoom keeps the point under the cursor fixed.
- Tiles load nearest-center first and appear without blocking the UI.
- Missing tiles show a lower-detail placeholder, not a blank hole, when a
  parent exists.
- Tiles that leave the view are cancelled and removed from the queue.
- Clicks report the correct block coordinates, including negative coordinates.
- No seams/flicker at 100% zoom when all tiles are loaded.

## Tests

- `map-app` pure view-state tests (no GPU).
- `map-core` placeholder selection helper tests.
- A manual checklist recorded in the task report: pan, zoom at corner, zoom at
  a tile edge, fast drag, seed change while loading.

## Risks / notes

- egui's texture scaling uses `TextureOptions`; `NEAREST` gives a crisp
  block-map look, `LINEAR` looks smoother when zoomed. Decide per layer and
  document.
- Drawing one texture per tile per frame is fine for 20–50 tiles; if it is
  not, merge into a single mesh in T33. Do not prematurely add a `wgpu`
  pipeline.
- Tile edge seams often come from fractional screen rects. Round the painted
  rect to physical pixels and add a 0.5 px overlap if needed.
- The coordinate readout must use the same transform as painting; share the
  helper, not a copy.
