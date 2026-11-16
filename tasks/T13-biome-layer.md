# T13 — Biome layer and palette

**Status:** proposed
**Milestone:** 2 — Static viewer
**Depends on:** T08, T10, T11
**Blocks:** T16, T19
**Plan refs:** [§5.3](../hytale-seed-map-plan.md#53-layers), [§8](../hytale-seed-map-plan.md#8-milestones), [§1](../hytale-seed-map-plan.md#1-scope)
**Agent fit:** medium

## Goal

Render the biome layer into an egui texture and display it in the app. This is
the first real map image and the default layer. Use the generator's
`Biome::get_map_color` so the map is grounded in worldgen data, not arbitrary
name hashes.

## Context

- `Biome::get_map_color()` exists on both `TileBiome` and `CustomBiome`.
- `WorldgenSource::biome_region` (T08) returns a compact `BiomeGrid` plus a
  `BiomeCatalog` with name/map color.
- T11 already has a headless `render_biome` and PNG output. Reuse the exact
  color logic; do not create a second palette.
- egui textures are app-side (`egui::TextureHandle`); map-core returns RGBA
  bytes only.

The plan's default view is a biome map at 2×2 blocks per sample. Start with one
texture covering the viewport; pan/zoom tiling arrives in T14–T16.

## Scope

### In

- Extend `map-core/src/render/biome.rs` with:
  - `BiomeRenderer` / `render_biome_rgba(&BiomeGrid, &BiomeCatalog) -> RgbaImage`;
  - optional subtle hillshade hook, disabled by default.
- `map-app/src/texture.rs`: convert `RgbaImage` to `egui::ColorImage` and
  upload a `TextureHandle`; cache one texture for the current view.
- Map view integration for a static view:
  - on source ready, request a biome grid for the current viewport at step 2;
  - upload the texture;
  - draw it centered/scaled in the map area;
  - show a loading spinner while the grid is requested.
- Palette diagnostics:
  - a debug legend/list of biome name → color for the loaded world;
  - log unknown biome ids exactly once.
- Tests for color mapping and texture-size invariants.

### Out

- Tiling, placeholders, pan/zoom (T14–T16).
- Height/relief/surface layers (T17/T20/T21).
- Layer opacity/blending (T19).
- A user-editable palette (not in v1).

## Work

1. Refactor T11's biome renderer into a reusable function. Keep the PNG encoder
   in `map-cli`/`export`; core returns an `RgbaImage`.
2. Color mapping:
   - `BiomeInfo.map_color` is the source of truth;
   - unpack the packed generator color consistently (document channel order);
   - force alpha 255 for known biomes;
   - unknown ids: magenta, and log a warning once per id.
3. Add optional shading:
   - if `SurfaceGrid` heights are available for the same rect, apply a subtle
     hillshade multiplier;
   - if not, render flat. Do not make the default layer depend on a full-block
     query.
   Keep the flat path deterministic and use it for the golden.
4. App integration:
   - add `TextureState { image: RgbaImage, handle: TextureHandle, rect: Rect,
     step: u32 }`;
   - upload with `ctx.load_texture(..., TextureOptions::NEAREST)` or `LINEAR`
     for smooth zoom (decide and document; nearest is better for the 2×2
     pixel look);
   - paint with `egui::Image`/`Mesh` into the map rect;
   - when the viewport changes or the source is replaced, mark the texture
     dirty and request a new grid (a simple debounce is enough before T15).
5. Ensure the request runs off the UI thread. Use T12's worker/state pattern or
   a short-lived thread; do not block `update`.
6. Add a debug legend panel (collapsible) listing biome names and colors from
   the catalog. This makes palette bugs visible and helps T19.
7. Add tests:
   - a synthetic `BiomeGrid` with 3 known ids → expected RGBA bytes;
   - unknown id → magenta;
   - grid dimensions and row-major orientation (north/top vs south/bottom);
   - texture byte length = width × height × 4.
8. Compare the headless output for a fixed seed/size with T11's golden; they
   must match.

## Acceptance

```sh
cargo test -p map-core biome_render
cargo test -p map-app
cargo run -p map-app
```

- Loading a seed shows a biome-colored map, not a blank/placeholder image.
- The image orientation matches the coordinate readout (moving north changes
  the sampled Z in the expected direction).
- The same seed/rect produces the same pixels as the T11 headless render.
- Unknown biomes are visibly magenta and logged once.
- The UI thread remains responsive while the grid is generated (spinner
  visible, window draggable).

## Tests

- `map-core/tests/biome_render.rs`: synthetic grids, unknown ids, orientation.
- A golden hash comparison shared with T11 (use the same helper; do not
  duplicate expected bytes).
- `map-app` state test: source ready → texture request → uploading state.

## Risks / notes

- `Biome::get_map_color` may be packed ABGR/RGBA depending on the loader; use
  the same unpacking as the server's `map_image` or write a test with a known
  color. A channel swap is easy to miss.
- A full-viewport grid at 2×2 can be large; cap the request size until T14
  tiles it. For a 1280×800 viewport, 640×400 samples is acceptable for a first
  texture but not for repeated panning.
- Do not use name-hash colors as a fallback for known biomes; it hides
  worldgen data bugs.
