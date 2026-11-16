The initial world generation in Rust is at /home/chome/Projects/hytale-server-rust



# Hytale Seed Explorer (Chunkbase-style) in Rust: Project Plan

Goal: a desktop application in Rust that, given a seed, renders an interactive Hytale map and lets you search for structures, caves, prefabs and biomes, like chunkbase.com. It uses the world generator you have already written.

> **Execution tasks:** the detailed work packets for this plan live in
> [`tasks/`](tasks/README.md). Each section and milestone below links to the
> tasks that implement it; [`tasks/README.md`](tasks/README.md) tracks
> dependencies and status. This document remains the scope authority; the task
> files carry the step-by-step detail.

Assumptions to verify early are marked **[verify]**; [T02](tasks/T02-seed-semantics.md) and [T03](tasks/T03-assets-and-palette.md) resolve the seed and asset assumptions.

---

## 1. Scope

**Tasks:** [T02](tasks/T02-seed-semantics.md), [T12](tasks/T12-egui-app-shell.md), [T13](tasks/T13-biome-layer.md), [T16](tasks/T16-pan-zoom-viewport.md), [T18](tasks/T18-inspector-resolution.md), [T20](tasks/T20-surface-block-palette.md), [T23](tasks/T23-feature-catalog-overlay.md), [T24](tasks/T24-feature-search.md), [T25](tasks/T25-biome-search.md), [T27](tasks/T27-cave-entrances.md), [T29](tasks/T29-map-tools.md), [T30](tasks/T30-waypoints.md), [T31](tasks/T31-share-settings-history.md), [T32](tasks/T32-png-export.md)

### Chunkbase-equivalent features
- Seed input (number or string, hashed the way Hytale does it) **[verify]**
- Pan (drag) and zoom (scroll wheel, centered on the cursor)
- Biome map as the default layer
- Cursor readout: block X/Z, biome, height, surface block
- Feature icons on the map (structures, prefabs, points of interest) with per-category toggles
- Click an icon or a point to copy its coordinates
- Grid overlays (chunks, regions)
- Go-to coordinates, origin/spawn marker
- Waypoints and annotations
- Shareable state: seed, position, zoom and enabled layers in one string

### Additions you asked for
- **Default resolution:** 2×2 blocks per sample, similar to Chunkbase. This is a configurable `step`, not a hard-coded value **[verify exactly what Chunkbase uses; it only matters for the default]**.
- **Full block mode:** a toggle to render at 1 sample per block, showing the actual surface block (grass, sand, stone, snow, water and so on) instead of a biome color.
- **Search** for structures, prefabs, caves and biomes, with results listed by distance from the current view or from a chosen point.

---

## 2. What the explorer needs from your worldgen

**Tasks:** [T01](tasks/T01-worldgen-api-audit.md), [T05](tasks/T05-fidelity-fixtures.md), [T08](tasks/T08-generator-worker-pool.md), [T09](tasks/T09-chunk-surface-slice.md), [T22](tasks/T22-feature-placement-query.md), [T27](tasks/T27-cave-entrances.md)

The GUI is only as good as the questions your generator can answer cheaply. Before writing UI code, check each item below against your existing crate. Anything missing becomes a small API addition to `worldgen`.

| Need | Why | Ideal API shape | Tasks |
|---|---|---|---|
| Biome at (x, z) without generating blocks | Fast biome layer and biome search | `biome_at(x, z) -> BiomeId` | T01, T08, T13 |
| Surface height and top block at (x, z) | Full block mode, shaded relief | `surface_at(x, z) -> { height, block, fluid_depth }` | T01, T09, T17, T21 |
| Feature placement as a pure function of seed and region | Structure/prefab search must not generate terrain | `features_in(rect, kind_filter) -> Vec<Feature>` | T01, T22, T24 |
| Cave information | Cave layer, slice view, cave search | `is_solid(x, y, z)` or a column cave profile, plus cave entrance positions | T01, T09, T27, T28 |
| Deterministic, thread-safe sampling | Parallel tile generation | `Send + Sync`, no global mutable state | T01, T08, T15 |
| Version tag | Cache invalidation, "generator version" shown in the UI | `GENERATOR_VERSION: u32` | T02, T14 |

The most important design rule: **structure and prefab placement must be computable without generating the terrain.** Chunkbase can search enormous areas quickly because feature positions come from the seed through a cheap deterministic function. If your worldgen only places prefabs as a side effect of generating chunks, expose a separate placement pass, since search depends on it.

---

## 3. Architecture

**Tasks:** [T06](tasks/T06-workspace-scaffold.md), [T07](tasks/T07-core-types-and-traits.md), [T08](tasks/T08-generator-worker-pool.md), [T09](tasks/T09-chunk-surface-slice.md), [T14](tasks/T14-tile-cache.md), [T15](tasks/T15-tile-scheduler.md)

```
hytale-map/
├─ worldgen/          # existing crate (extended only where section 2 requires)
├─ map-core/          # no GUI dependencies; fully testable headlessly
│  ├─ source.rs       # WorldSource and FeatureSource traits plus the adapter to worldgen
│  ├─ tiles.rs        # tile keys, scheduling, cache
│  ├─ render/         # layer renderers (biome, height, relief, blocks, caves)
│  ├─ features.rs     # feature catalog, icon ids, categories
│  ├─ search.rs       # search engine (see section 6)
│  ├─ project.rs      # waypoints, saved state, share strings
│  └─ export.rs       # PNG export
├─ map-app/           # eframe/egui binary
└─ map-cli/           # optional: headless render/search for CI and scripting
```

### Core traits

```rust
pub trait WorldSource: Send + Sync {
    /// Cheap: biome only.
    fn biome_at(&self, x: i32, z: i32) -> BiomeId;

    /// Surface data for a grid of samples. `step` is blocks per sample (1, 2, 4, ...).
    fn surface_region(&self, x0: i32, z0: i32, w: u32, h: u32, step: u32) -> SurfaceGrid;

    /// Horizontal slice at a given Y (for cave view).
    fn slice_region(&self, x0: i32, z0: i32, w: u32, h: u32, step: u32, y: i32) -> SliceGrid;

    fn seed(&self) -> Seed;
}

pub trait FeatureSource: Send + Sync {
    /// Feature placements in a rect. Must not require terrain generation.
    fn features_in(&self, rect: Rect, kinds: &FeatureFilter) -> Vec<Feature>;
}
```

`Feature` carries: kind (structure, prefab, cave entrance, POI), a name/id, position, optional bounding box, and optional metadata such as variant or size.

---

## 4. Tech choices

**Tasks:** [T06](tasks/T06-workspace-scaffold.md), [T07](tasks/T07-core-types-and-traits.md), [T09](tasks/T09-chunk-surface-slice.md), [T12](tasks/T12-egui-app-shell.md), [T14](tasks/T14-tile-cache.md), [T15](tasks/T15-tile-scheduler.md), [T24](tasks/T24-feature-search.md), [T25](tasks/T25-biome-search.md), [T30](tasks/T30-waypoints.md), [T31](tasks/T31-share-settings-history.md), [T32](tasks/T32-png-export.md), [T34](tasks/T34-packaging-docs-release.md)

| Concern | Choice | Notes |
|---|---|---|
| GUI | `eframe` / `egui` | Side panels, search results table, custom-painted map. Supports a later WebAssembly build |
| Map rendering | One egui texture per tile | Move to a custom `wgpu` pipeline only if profiling shows a need |
| Parallelism | `rayon` plus `crossbeam-channel` | Tile jobs run on workers; results come back over a channel |
| Cache | `moka` (or `lru`) | Byte-bounded; also drop GPU textures on eviction |
| Search | `rayon` with cancellation tokens | Streamed results, cancellable |
| Image export | `image` | PNG |
| Persistence | `serde`, `serde_json`, `directories` | Settings, waypoints, seed history |
| Clipboard / share | `arboard` (or egui's built-in clipboard) | Copy coordinates and share strings |
| Benchmarks / tests | `criterion`, `insta` or plain golden PNGs | See section 9 |

---

## 5. Rendering design

**Tasks:** [T10](tasks/T10-coordinate-zoom-math.md), [T13](tasks/T13-biome-layer.md), [T14](tasks/T14-tile-cache.md), [T15](tasks/T15-tile-scheduler.md), [T16](tasks/T16-pan-zoom-viewport.md), [T17](tasks/T17-height-relief-fluid-layers.md), [T19](tasks/T19-layer-compositing-debug.md), [T20](tasks/T20-surface-block-palette.md), [T21](tasks/T21-full-block-mode.md), [T27](tasks/T27-cave-entrances.md), [T28](tasks/T28-cave-slices-density.md)

### 5.1 Tiles and zoom

**Tasks:** [T10](tasks/T10-coordinate-zoom-math.md), [T14](tasks/T14-tile-cache.md), [T15](tasks/T15-tile-scheduler.md), [T16](tasks/T16-pan-zoom-viewport.md)

- Tile = 256×256 pixels. Each zoom level maps pixels to blocks.
- Zoom levels are discrete steps with smooth interpolation between them in the UI.
- Tile key: `(generator_version, seed, layer, mode, zoom, tx, ty)`. Any change to the seed or generator version invalidates the tiles.
- Each frame: compute the visible tiles, queue missing ones **nearest the viewport center first**, and draw the nearest cached lower-detail tile as a placeholder until the real one arrives.
- Cancel queued tiles that scroll out of view (generation counter or priority queue).

### 5.2 Resolution modes

**Tasks:** [T18](tasks/T18-inspector-resolution.md), [T20](tasks/T20-surface-block-palette.md), [T21](tasks/T21-full-block-mode.md)

| Mode | Blocks per sample | When it is used |
|---|---|---|
| **Overview** | 4, 8, 16, ... | Zoomed far out; biomes only |
| **Default (Chunkbase-like)** | **2×2** | Normal exploring; biome color with light shading |
| **Full block** | **1×1** | Toggle; real surface block colors at high zoom |

How they interact:
- The user's resolution setting is a **minimum step** (finest allowed). The renderer picks `max(user_step, step_needed_for_zoom)`. At 1 pixel per block or finer, full block mode is exact.
- Full block mode is expensive (a 256×256 tile means 65,536 column samples). Mitigations:
  - Only compute at zoom levels where the user can actually see that detail.
  - Compute the surface block from `surface_at` rather than generating whole chunks, if worldgen allows. If it does not, cache whole generated chunks and reuse them for every layer.
  - Show a coarser placeholder instantly and refine progressively.
  - Show a "generating N tiles" indicator and let the user cap worker threads.

### 5.3 Layers

**Tasks:** [T13](tasks/T13-biome-layer.md), [T17](tasks/T17-height-relief-fluid-layers.md), [T19](tasks/T19-layer-compositing-debug.md), [T20](tasks/T20-surface-block-palette.md), [T21](tasks/T21-full-block-mode.md)

Each layer converts samples into pixels.

- **Biome** (default): biome palette, with optional subtle hillshade
- **Surface blocks:** per-block color table derived from block textures (average color), generated once by a tool, then stored as data
- **Height:** hypsometric tint
- **Shaded relief:** lighting from neighbor height differences
- **Water/fluids:** depth shading
- **Caves:** see section 5.4
- **Other generator data** (temperature, humidity, zones, and so on) as additional debug layers if your generator exposes them

Layers can be blended with an opacity slider.

### 5.4 Caves

**Tasks:** [T27](tasks/T27-cave-entrances.md), [T28](tasks/T28-cave-slices-density.md)

Caves are 3D, so a single top-down map cannot show them. Provide three complementary views:

1. **Cave entrances overlay:** icons where caves open onto the surface (the cheapest, and usually what players want).
2. **Y-slice view:** a horizontal slice at a chosen Y, with a slider. Solid/air/fluid shown in a tile layer keyed by `(…, y)`.
3. **Cave density heat map:** a coarse top-down view showing how much of each column is cave, for finding cave-rich areas.

Cave tiles are keyed by Y, so changing the slider invalidates only the cave layer.

### 5.5 Overlays (vector, drawn on top, not baked into tiles)

**Tasks:** [T23](tasks/T23-feature-catalog-overlay.md), [T24](tasks/T24-feature-search.md), [T27](tasks/T27-cave-entrances.md), [T29](tasks/T29-map-tools.md), [T30](tasks/T30-waypoints.md)

- Chunk and region grid (lines adapt to zoom)
- Feature icons with category toggles, clustered when zoomed out
- Spawn point and origin crosshair
- Waypoints, with labels and colors
- Measurement tool (distance, plus a direction readout)
- Search result markers, highlighted

---

## 6. Search design

**Tasks:** [T22](tasks/T22-feature-placement-query.md), [T24](tasks/T24-feature-search.md), [T25](tasks/T25-biome-search.md), [T26](tasks/T26-search-ui.md), [T27](tasks/T27-cave-entrances.md), [T28](tasks/T28-cave-slices-density.md)

### 6.1 Feature search (structures, prefabs, cave entrances)

**Tasks:** [T22](tasks/T22-feature-placement-query.md), [T23](tasks/T23-feature-catalog-overlay.md), [T24](tasks/T24-feature-search.md), [T26](tasks/T26-search-ui.md)

- Takes a feature kind (or tag), a center point and a maximum radius.
- Uses `FeatureSource::features_in` over expanding **rings** of regions, so the nearest results come first.
- Streams results to the UI as they are found, sorted by distance. Cancellable.
- Each result row: icon, name, coordinates, distance. Click centers the map and flashes a marker. Right-click copies a teleport-style coordinate string.
- Optional filters: variant, minimum size, "within N blocks of another feature".

### 6.2 Biome search

**Tasks:** [T25](tasks/T25-biome-search.md), [T26](tasks/T26-search-ui.md)

- Coarse pass: sample `biome_at` on a wide grid (for example every 16 to 32 blocks) in expanding rings, to find candidate locations.
- Refinement pass: sample near the best candidates at the finest step to locate the actual boundary and the nearest block.
- Optional criteria: biome patch of a minimum area ("a Forest at least 200×200").

### 6.3 Cave search

**Tasks:** [T27](tasks/T27-cave-entrances.md), [T28](tasks/T28-cave-slices-density.md)

- Cave entrances come from the placement pass when worldgen can provide it.
- Otherwise, sample the surface with a coarse grid, flag cells whose cave profile suggests an opening, then verify at full resolution.
- Cave type search (large cavern, ravine, ...) depends on what the generator can distinguish. Treat this as a stretch goal until the API is known.

### 6.4 Multi-criteria search ("seed finder" mode)

**Tasks:** [T22](tasks/T22-feature-placement-query.md), [T24](tasks/T24-feature-search.md), [T33](tasks/T33-performance-tuning.md) (design only; batch seed search is out of scope for v1)

Eventually: "find seeds where a given structure is within N blocks of spawn." This is a **seed-cracking-style batch search**, run offline over many seeds in parallel using `FeatureSource` only. Out of scope for v1, but design `FeatureSource` to be fast enough to make it possible.

---

## 7. UI layout

**Tasks:** [T12](tasks/T12-egui-app-shell.md), [T13](tasks/T13-biome-layer.md), [T16](tasks/T16-pan-zoom-viewport.md), [T18](tasks/T18-inspector-resolution.md), [T23](tasks/T23-feature-catalog-overlay.md), [T24](tasks/T24-feature-search.md), [T26](tasks/T26-search-ui.md), [T27](tasks/T27-cave-entrances.md), [T29](tasks/T29-map-tools.md), [T30](tasks/T30-waypoints.md), [T31](tasks/T31-share-settings-history.md), [T32](tasks/T32-png-export.md)

```
┌──────────────────────────────────────────────────────────────┐
│ Seed [__________] [Go]   Version [..]   Resolution [2x2 ▾]   │
├───────────────┬──────────────────────────────────────────────┤
│ Layers        │                                              │
│  ◉ Biomes     │                                              │
│  ○ Surface    │                 MAP VIEW                     │
│  ○ Height     │             (pan / zoom / click)             │
│  ○ Caves  Y▮  │                                              │
│ Overlays      │                                              │
│  ☑ Structures │                                              │
│  ☑ Prefabs    │                                              │
│  ☑ Cave ent.  │                                              │
│  ☐ Chunk grid │                                              │
│ Search        │                                              │
│  [kind ▾][Go] │                                              │
│  results ...  │                                              │
├───────────────┴──────────────────────────────────────────────┤
│ X:-1203 Z:455 | Biome: ... | Height: 71 | Block: ... | tiles  │
└──────────────────────────────────────────────────────────────┘
```

Keyboard and mouse: drag to pan, wheel to zoom at the cursor, click to copy coordinates, `G` to go to coordinates, `F` to open search, `Ctrl+S` to export PNG.

---

## 8. Milestones

**Tasks:** all tasks; the table maps each milestone to its work packets.

| # | Milestone | Deliverable | Tasks | Est. |
|---|---|---|---|---|
| 0 | **API audit** | Checklist from section 2 against `worldgen`; list of required additions; benchmark of column, region and feature-placement speed | [T01](tasks/T01-worldgen-api-audit.md), [T02](tasks/T02-seed-semantics.md), [T03](tasks/T03-assets-and-palette.md), [T04](tasks/T04-performance-baseline.md), [T05](tasks/T05-fidelity-fixtures.md) | 1 day |
| 1 | **Scaffold** | Workspace, traits, adapter, test rendering one 512×512 region at 2×2 to PNG | [T06](tasks/T06-workspace-scaffold.md), [T07](tasks/T07-core-types-and-traits.md), [T08](tasks/T08-generator-worker-pool.md), [T09](tasks/T09-chunk-surface-slice.md), [T10](tasks/T10-coordinate-zoom-math.md), [T11](tasks/T11-headless-png-render.md) | 1 day |
| 2 | **Static viewer** | egui window, seed box, one texture, biome palette | [T12](tasks/T12-egui-app-shell.md), [T13](tasks/T13-biome-layer.md) | 1–2 days |
| 3 | **Tiling and navigation** | Tile keys, workers, cache, pan/zoom, parent-tile placeholders, cancellation | [T14](tasks/T14-tile-cache.md), [T15](tasks/T15-tile-scheduler.md), [T16](tasks/T16-pan-zoom-viewport.md) | 3 days |
| 4 | **Layers and inspector** | Biome/height/relief, hover readout, resolution selector (default 2×2) | [T17](tasks/T17-height-relief-fluid-layers.md), [T18](tasks/T18-inspector-resolution.md), [T19](tasks/T19-layer-compositing-debug.md) | 2 days |
| 5 | **Full block mode** | Surface block palette tool, 1×1 rendering, progressive refinement | [T20](tasks/T20-surface-block-palette.md), [T21](tasks/T21-full-block-mode.md) | 2–3 days |
| 6 | **Feature overlay** | Placement pass integration, icons, toggles, clustering | [T22](tasks/T22-feature-placement-query.md), [T23](tasks/T23-feature-catalog-overlay.md) | 3 days |
| 7 | **Search** | Ring search for features, coarse/fine biome search, result list, cancellation | [T24](tasks/T24-feature-search.md), [T25](tasks/T25-biome-search.md), [T26](tasks/T26-search-ui.md) | 3–4 days |
| 8 | **Caves** | Entrance overlay, Y-slice layer, density heat map | [T27](tasks/T27-cave-entrances.md), [T28](tasks/T28-cave-slices-density.md) | 3–4 days |
| 9 | **Tools and persistence** | Grid, go-to, measure, waypoints, share string, settings, seed history, PNG export | [T29](tasks/T29-map-tools.md), [T30](tasks/T30-waypoints.md), [T31](tasks/T31-share-settings-history.md), [T32](tasks/T32-png-export.md) | 3 days |
| 10 | **Polish** | Profiling, cache tuning, error handling, packaging for Windows/Linux/macOS | [T33](tasks/T33-performance-tuning.md), [T34](tasks/T34-packaging-docs-release.md) | ongoing |

Rough total for a solid v1: 4 to 5 weeks of part-time work, dominated by how much of section 2 already exists.

**Suggested release slices**
- **v0.1:** milestones 0–4 (viewer with biomes and relief) → T01–T19
- **v0.5:** plus 5–7 (full blocks, features, search) → T20–T26
- **v1.0:** plus 8–9 (caves, tools, export) → T27–T32

---

## 9. Performance, correctness and testing

**Tasks:** [T04](tasks/T04-performance-baseline.md), [T05](tasks/T05-fidelity-fixtures.md), [T10](tasks/T10-coordinate-zoom-math.md), [T11](tasks/T11-headless-png-render.md), [T14](tasks/T14-tile-cache.md), [T15](tasks/T15-tile-scheduler.md), [T24](tasks/T24-feature-search.md), [T25](tasks/T25-biome-search.md), [T33](tasks/T33-performance-tuning.md)

### Performance targets (adjust after the milestone 0 benchmarks)
- First visible tile within ~100 ms of opening; any viewport fully filled with at least coarse data within ~1 s → [T15](tasks/T15-tile-scheduler.md), [T16](tasks/T16-pan-zoom-viewport.md), [T21](tasks/T21-full-block-mode.md)
- UI thread never blocks on generation (frame time stays under 16 ms) → [T12](tasks/T12-egui-app-shell.md), [T15](tasks/T15-tile-scheduler.md)
- Feature search returns the first results within ~500 ms → [T24](tasks/T24-feature-search.md), [T25](tasks/T25-biome-search.md)
- Memory capped (for example 512 MB of tile cache) with eviction → [T14](tasks/T14-tile-cache.md), [T33](tasks/T33-performance-tuning.md)

### Testing
- **Coordinate math:** unit tests for screen ↔ world ↔ tile conversions, including negative coordinates and tile-edge cases. → [T10](tasks/T10-coordinate-zoom-math.md)
- **Determinism:** the same seed produces byte-identical tiles; golden images for a few fixed seeds. → [T05](tasks/T05-fidelity-fixtures.md), [T11](tasks/T11-headless-png-render.md), [T14](tasks/T14-tile-cache.md)
- **Fidelity with the real game:** a regression suite of known `(seed, coordinate) → biome/feature` pairs taken from the actual game. This is the most valuable test: the map is only useful if it matches the game. → [T05](tasks/T05-fidelity-fixtures.md)
- **Search correctness:** property tests that every result is a real feature within the requested radius, and that no closer feature was skipped (compare to brute force on small areas). → [T24](tasks/T24-feature-search.md), [T25](tasks/T25-biome-search.md)
- **Headless rendering in CI:** `map-core` and `map-cli` need no GPU. → [T06](tasks/T06-workspace-scaffold.md), [T11](tasks/T11-headless-png-render.md)
- **Benchmarks:** `criterion` for tile render time per layer and per resolution mode, and for feature search. → [T04](tasks/T04-performance-baseline.md), [T33](tasks/T33-performance-tuning.md)

---

## 10. Risks and open questions

**Tasks:** [T01](tasks/T01-worldgen-api-audit.md), [T02](tasks/T02-seed-semantics.md), [T03](tasks/T03-assets-and-palette.md), [T04](tasks/T04-performance-baseline.md), [T08](tasks/T08-generator-worker-pool.md), [T09](tasks/T09-chunk-surface-slice.md), [T14](tasks/T14-tile-cache.md), [T20](tasks/T20-surface-block-palette.md), [T21](tasks/T21-full-block-mode.md), [T27](tasks/T27-cave-entrances.md), [T28](tasks/T28-cave-slices-density.md), [T34](tasks/T34-packaging-docs-release.md)

1. **Placement not separable from generation.** If prefabs and structures cannot be enumerated without generating terrain, search becomes slow. Fix this in `worldgen` first. → [T22](tasks/T22-feature-placement-query.md)
2. **Full block mode cost.** The per-column surface lookup may force whole-chunk generation. Measure in milestone 0 and decide between chunk caching and a dedicated surface function. → [T04](tasks/T04-performance-baseline.md), [T09](tasks/T09-chunk-surface-slice.md), [T21](tasks/T21-full-block-mode.md)
3. **Cave representation.** The cave views depend on whether worldgen exposes solidity per (x, y, z) or a per-column profile. → [T09](tasks/T09-chunk-surface-slice.md), [T27](tasks/T27-cave-entrances.md), [T28](tasks/T28-cave-slices-density.md)
4. **Seed semantics.** Confirm how Hytale converts seed input into the internal value, including string seeds **[verify]**. → [T02](tasks/T02-seed-semantics.md)
5. **Generator version drift.** Hytale updates may change generation. Tag cache keys and exports with a generator version and show it in the UI. → [T02](tasks/T02-seed-semantics.md), [T14](tasks/T14-tile-cache.md), [T31](tasks/T31-share-settings-history.md)
6. **Chunkbase parity details.** The 2×2 default and the exact feature set Chunkbase shows are assumptions. Confirm against the site and adjust the defaults. → [T03](tasks/T03-assets-and-palette.md), [T18](tasks/T18-inspector-resolution.md), [T23](tasks/T23-feature-catalog-overlay.md)
7. **Block color table.** Surface block colors need either texture-derived averages (requires the game assets) or hand-picked values. Decide the source and the licensing implications. → [T03](tasks/T03-assets-and-palette.md), [T20](tasks/T20-surface-block-palette.md)
8. **Platform scope.** Desktop first. A WebAssembly build is possible with egui, but threading (`rayon`) and memory limits make it a separate effort. → [T34](tasks/T34-packaging-docs-release.md)

---

## 11. Next actions

**Tasks:** [T01](tasks/T01-worldgen-api-audit.md), [T04](tasks/T04-performance-baseline.md), [T06](tasks/T06-workspace-scaffold.md), [T11](tasks/T11-headless-png-render.md)

1. Run the milestone 0 audit: list the public functions in `worldgen` and map them onto the section 2 table. → [T01](tasks/T01-worldgen-api-audit.md)
2. Benchmark three numbers: time per column, time per 256×256 region at step 2, and time to enumerate features in a 10,000×10,000 area. → [T04](tasks/T04-performance-baseline.md)
3. Create the workspace and render one PNG (milestone 1). → [T06](tasks/T06-workspace-scaffold.md), [T11](tasks/T11-headless-png-render.md)
