# T32 — PNG export

**Status:** proposed
**Milestone:** 9 — Tools and persistence
**Depends on:** T21, T23, T29, T30
**Blocks:** T33
**Plan refs:** [§1](../hytale-seed-map-plan.md#1-scope), [§4](../hytale-seed-map-plan.md#4-tech-choices), [§7](../hytale-seed-map-plan.md#7-ui-layout)
**Agent fit:** medium

## Goal

Export the current map view (or a specified rectangle) to a PNG, including the
enabled layers and optional overlays, from both the app (`Ctrl+S`) and the CLI.
Exports must match what the user sees and be deterministic for a fixed state.

## Context

- T21 renders surface tiles; T13/T17 render biome/height/relief/fluid; T19
  defines the blend function.
- T23/T29/T30 draw features, grids, markers and waypoints as vector overlays.
- The plan lists `image` for PNG export and `Ctrl+S` as the shortcut.
- The app paints textures on the GPU; export must run through the same
  renderers/logic, not a screenshot, so it works headless and at a chosen
  scale.

## Scope

### In

- `map-core/src/export.rs`:
  - `ExportRequest { identity, seed, rect, step, layer_stack, overlays,
    scale }`;
  - render base layers for the rect, composite with T19's blend function;
  - draw overlays into an RGBA buffer with a shared `OverlayPainter` trait;
  - encode PNG with `image` or `png`;
  - progress/cancellation.
- `map-cli export`:
  - flags: seed, center or rect, size, step, layers, overlays, output;
  - headless, no GPU.
- `map-app`:
  - `Ctrl+S` opens a save dialog (use `rfd` or a simple path dialog; if a file
    dialog dependency is undesirable, default to `export-<seed>-<x>_<z>.png` in
    the current directory and show the path);
  - export in a background thread with a progress indicator;
  - export the current viewport or a chosen rect/scale.
- Overlay painting:
  - grid lines, feature icons/clusters (or a simplified marker), spawn/origin,
    waypoints, measurement line, search markers (optional);
  - use the same screen transform as the app, but in export pixel space;
  - labels: use a tiny bitmap font or skip labels in v1; document.
- Tests/golden:
  - deterministic PNG bytes for a fixed small export;
  - dimensions/step correctness;
  - alpha/compositing;
  - features/waypoints appear at the right pixel coordinates.

### Out

- Screenshotting the egui window.
- Vector (SVG/PDF) export.
- Printing/annotations beyond the existing overlay data.
- Server-side map image export (the server already has its own map images).

## Work

1. Define `ExportRequest` and a headless render path:
   - resolve the rect from center/size/step;
   - request the base layer tiles/regions at the export resolution;
   - composite in the same order as the app;
   - if a tile/summary is missing, either render it synchronously (CLI) or
     wait with progress (app).
2. Implement `OverlayPainter`:
   - `draw_line`, `draw_rect`, `draw_circle`, `draw_marker` primitives on an
     RGBA buffer;
   - a simple bitmap/block font or a label-free v1;
   - feature icons use the T23 shapes/colors; waypoints use T30 colors;
   - grid visibility uses the same min-pixel rule as T29.
3. Implement PNG encoding:
   - `image::RgbaImage::save` or `png::Encoder`;
   - no timestamp/comment chunks; deterministic bytes;
   - preserve alpha (or composite over a background; choose and document).
4. CLI:
   - `map-cli export --seed 0 --center -1203,455 --size 1024 --step 2 --layer biome --features --waypoints --out out.png`;
   - print dimensions, block extent, step, feature/waypoint counts and elapsed
     time;
   - reject invalid combinations (e.g. step 1 with a huge size) with a clear
     error.
5. App:
   - background export with a progress event; disable duplicate exports;
   - default filename includes seed/center; show a toast with the path;
   - `Ctrl+S` only when the source is ready.
6. Tests:
   - a 64×64 export for a fixed seed/rect matches a golden hash;
   - a 2× scale doubles dimensions and preserves the block extent;
   - negative center coordinates;
   - overlays: one feature/waypoint at a known coordinate lands at the expected
     pixel;
   - deterministic bytes across runs;
   - cancellation stops a large export.
7. Document the export limitations: labels, anti-aliasing, and the fact that
   the map is a prediction.

## Acceptance

```sh
cargo test -p map-core export
cargo test -p map-cli
cargo run -p map-cli -- export --seed 0 --center 0,0 --size 512 --step 2 \
  --layer biome --out target/export.png
cargo run -p map-app
```

- The CLI writes a PNG with the requested dimensions/block extent/step.
- The app's `Ctrl+S` writes the current view without freezing the UI.
- The exported image matches the on-screen layers and overlays (within the
  documented label/anti-aliasing differences).
- The same request twice produces byte-identical output.
- A huge export is rejected or cancelled with a clear message.

## Tests

- `map-core/tests/export.rs`: golden hash, dimensions, overlays, determinism.
- A CLI smoke test with a temporary output path.
- Manual: export with biome + features + waypoints and compare to the app.

## Risks / notes

- Exporting at 1×1 over a large area can trigger thousands of chunk
  generations. Enforce a size/step limit or require a confirmation.
- A screenshot would be easier but non-deterministic and GPU-dependent; use the
  core renderers.
- Labels require a font; if v1 skips them, say so in the UI/export dialog so
  users are not surprised.
