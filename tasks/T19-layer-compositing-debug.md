# T19 — Layer compositing and debug layers

**Status:** proposed
**Milestone:** 4 — Layers and inspector
**Depends on:** T13, T17, T18
**Blocks:** T21
**Plan refs:** [§5.3](../hytale-seed-map-plan.md#53-layers), [§7](../hytale-seed-map-plan.md#7-ui-layout), [§1](../hytale-seed-map-plan.md#1-scope)
**Agent fit:** medium

## Goal

Turn the single-layer selector into the plan's layer stack: enable/disable
layers, blend them with an opacity slider, and add useful debug layers (zones,
and temperature/humidity only if the generator exposes them). This makes the
height/relief/biome data composable and prepares the full-block layer.

## Context

The plan says:

> Layers can be blended with an opacity slider.
> Other generator data (temperature, humidity, zones, and so on) as additional
> debug layers if your generator exposes them.

Current layer IDs: Biome, Height, Relief, Fluid (T13/T17), Surface (T20/T21),
CaveSlice/CaveDensity (T28). The app currently selects one base layer.

Generator data available now:

- zone name via `ZoneBiomeResult::get_zone_result().get_zone().name`;
- biome name/id/map color;
- height, tint, environment id;
- climate/climate type exists in worldgen but is not exposed as a simple
  per-column query in the public API. Treat temperature/humidity as optional
  and do not fake them.

## Scope

### In

- `map-core/src/layers.rs`:
  - `LayerSettings { id, enabled, opacity: f32, order: i16 }`;
  - `LayerStack` with defaults, serde, enable/disable, opacity clamp, reorder.
- App rendering:
  - paint the base layer first, then each enabled overlay layer with alpha;
  - only request tiles for enabled layers;
  - a layer panel with checkboxes/radio semantics and opacity sliders;
  - remember settings across layer changes and (optionally) restarts (T31).
- Debug layers:
  - `LayerId::Zone`: color by zone name/id; implement with a `zone_at` /
    `zone_region` query (add to `WorldSource` if not already exposed; the
    adapter can call `get_zone_biome_result_at(...).get_zone_result()`).
  - `LayerId::Environment`: color by environment id (from the surface/tint
    data) if cheap.
  - Temperature/humidity: only if T01 finds a public per-column query. If not,
    show "not exposed by this generator" and leave a TODO; do not parse assets
    to approximate.
- Blending:
  - use straight alpha in RGBA tiles;
  - app paints `Color32::from_white_alpha(opacity)` when drawing a texture, or
    composites in core for export (T32 must match);
  - define the order explicitly (bottom → top).
- Tests for settings, opacity, layer ordering and zone colors.

### Out

- Full-block Surface layer (T20/T21).
- Cave layers (T28).
- A custom theme/palette editor.
- A vector overlay compositor for features/grid/waypoints (T23/T29/T30).

## Work

1. Implement `LayerSettings`/`LayerStack`:
   - `Default` = Biome enabled at 1.0, others disabled;
   - `enabled_layers()` returns the requested tile layers in order;
   - `set_opacity` clamps 0..=1 and marks the stack dirty;
   - serde for T31.
2. Add `WorldSource::zone_at`/`zone_region` (or a `zone_name_at`) if T01
   confirms the call is cheap. Reuse the generator's column cache. Add a
   `ZoneCatalog` mapping id/name → color (stable hash or a hand-picked table).
3. Implement `ZoneRenderer`:
   - sample zone names;
   - color by a stable hash of the name/id, with a debug legend;
   - no shading.
4. Add environment debug layer only if the surface summary already carries the
   environment id; otherwise defer. Use a stable color ramp and a legend.
5. App:
   - replace the layer radio with a list of checkboxes; enforce "at least one
     base layer" or allow a blank map with a message;
   - opacity sliders;
   - draw each enabled layer as a separate texture (the scheduler cache keys
     already include the layer);
   - display the layer order in the panel (Biome bottom, then Height, Relief,
     Fluid, Surface, cave layers; overlays on top).
6. Compositing math:
   - define `blend_over(base: [u8;4], top: [u8;4], opacity: f32) -> [u8;4]`
     using non-premultiplied alpha;
   - use the same function in the app and the PNG exporter (T32);
   - test a few known blends.
7. Add a debug legend for zone/environment colors, collapsible.
8. Tests:
   - `LayerStack` default, toggle, opacity clamp, serde round-trip;
   - zone color stability and unknown zone;
   - blend math (opaque, transparent, half);
   - disabled layer is not requested from the scheduler;
   - reordering does not lose settings.
9. Manual check: enable Biome + Height at 50%, verify the result looks like a
   blend and not a recolored single layer.

## Acceptance

```sh
cargo test -p map-core layers
cargo test -p map-app layers
cargo run -p map-app
```

- Multiple layers can be enabled and blended with opacity.
- Toggles take effect without stale tiles from a disabled layer.
- Zone debug layer works for the loaded world; temperature/humidity either
  works or is explicitly marked unavailable.
- Layer settings survive a source reload (same session) and serialize.
- The blend function is shared with export.

## Tests

- `map-core/tests/layers.rs`: stack/serde/blend.
- `map-app` state test: toggling a layer changes the requested layer set.
- A small golden composite of two synthetic 4×4 layers.

## Risks / notes

- Compositing multiple 256×256 textures in egui is cheap, but each layer has
  its own cache entries. Keep the default to one or two layers to protect the
  memory budget.
- Do not approximate temperature/humidity from biome colors; a debug layer
  must show real generator data or say it is unavailable.
- Alpha handling must match between the screen and PNG export, or exports will
  not look like the app.
