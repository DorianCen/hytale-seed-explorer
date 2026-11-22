# T23 — Feature catalog, source adapter and overlay

**Status:** done (deliverable: `map-core/src/features.rs` — categories,
catalog with (kind, name, position) dedupe, `FeatureOverlayState`,
screen-space clustering, `FeatureCache`; `FeatureSource` adapter over the
T22 worker query (`Command::FeatureQuery` + `translate_placement`, the
one documented worldgen-typed parity seam); `map-app/src/overlays/features.rs`
— fetch planner (150 ms debounce, margin rect clamped under the
2048-block fetch cap), background fetch thread, clustered vector markers,
tooltip/click-copy (`X:{x} Z:{z}`), per-group panel toggles wired to the
`structures`/`prefabs`/`spawn`/`poi` checkboxes. Gates:
`cargo test -p map-core features` 15/15 (adapters verified against a
directly loaded generator on the fixture),
`cargo test -p map-app features` 9/9, full suites 119/80,
`cargo build -p map-app`, clippy/fmt clean. Remaining manual step:
the visual `cargo run -p map-app` checklist in the packet-B report
(first marker fetch at the default view is capped near 2048 blocks and
takes minutes per the recorded T22 numbers; pan/zoom stays responsive
while it runs).)
**Milestone:** 6 — Feature overlay
**Depends on:** T16, T22
**Blocks:** T24, T26, T27, T29, T32
**Plan refs:** [§5.5](../hytale-seed-map-plan.md#55-overlays-vector-drawn-on-top-not-baked-into-tiles), [§6.1](../hytale-seed-map-plan.md#6-search-design), [§7](../hytale-seed-map-plan.md#7-ui-layout)
**Agent fit:** high

## Goal

Connect the worldgen placement query to the map: a feature catalog with
categories and icons, a `FeatureSource` adapter, an overlay that fetches
placements for the visible rectangle, draws icons, clusters them when zoomed
out, and exposes per-category toggles.

## Context

T22 returns `FeaturePlacement` values for unique prefabs, spawn points and
biome prefabs. T16 renders tiles and knows the viewport transform. T12 has the
left-panel overlay checkboxes.

Plan requirements:

- feature icons with per-category toggles;
- clustered when zoomed out;
- click an icon or point to copy coordinates;
- search result markers highlighted (T24/T26);
- spawn point and origin crosshair (T29).

Feature categories:

- unique prefab category (`PrefabCategory.name`, e.g. `Unique`, `Spawn`);
- biome prefab category (from `PrefabPatternGenerator::get_category`);
- spawn (from `get_spawn_points` / `is_spawn_location`);
- POI (`UniquePrefabEntry::is_show_on_map`);
- cave entrance (T27).

## Scope

### In

- `map-core/src/features.rs`:
  - `FeatureCategory` with id/name/color/icon kind/default visible;
  - `FeatureCatalog` built from placements + a static category map;
  - `FeatureOverlayState` (enabled categories, cluster threshold);
  - cluster algorithm for screen-space proximity (grid cells);
  - a `FeatureSource` adapter over `WorldgenSource`/worldgen query;
  - caching placements by `(seed, rect, filter)` with a TTL/version.
- `map-app/src/overlays/features.rs`:
  - fetch placements for the visible rect on viewport change (debounced);
  - draw icons (simple vector shapes or text glyphs; avoid a binary icon
    dependency);
  - cluster at low zoom; a cluster icon shows a count and expands on click;
  - hover tooltip (name, category, coordinates, distance);
  - click copies coordinates and selects/flashes the feature;
  - per-category checkboxes in the left panel.
- Tests for catalog mapping, clustering and screen placement.

### Out

- Search (T24/T26) and cave entrances (T27).
- Waypoints/grid/measure (T29/T30).
- 3D feature models or prefab previews.

## Work

1. Define the catalog:
   - map worldgen categories/names to stable `FeatureCategory` ids;
   - assign a color and a simple icon shape per category;
   - default visibility: structures/prefabs/spawn on, cave entrances on when
     implemented, POI on;
   - include a `source` field so a prefab and a unique prefab with the same
     name are distinguishable.
2. Implement the adapter:
   - convert `FeaturePlacement` to map-core `Feature` (T07);
   - compute bounds if absent;
   - deduplicate by `(kind, name, position)` (query padding can overlap).
3. Fetching:
   - expand the visible rect by a margin (for example one viewport width) to
     avoid re-fetching on every small pan;
   - debounce during drag (for example 150 ms after the view settles);
   - cancel/replace an in-flight fetch when the viewport changes;
   - cache the last few rects; invalidate on seed/version change.
4. Clustering:
   - project features to screen positions;
   - bucket by a grid cell of ~24–32 px;
   - if a bucket has >1 feature, draw a cluster marker with a count;
   - clicking a cluster zooms in or splits it;
   - do not cluster when the user is zoomed in enough that icons are distinct.
   Unit-test the cluster function with synthetic points.
5. Drawing:
   - draw after tiles, before the cursor/readout;
   - use a small vector icon (circle/diamond/house) with a category color and
     a dark outline for contrast;
   - selected/flashing feature: a ring/pulse for ~1 s;
   - cull features outside the map rect.
6. Interaction:
   - hit-test icons/clusters with a radius;
   - click copies `x z` (or `/tp x y z`) and shows a toast; the exact string is
     shared with T31;
   - hover shows a tooltip.
7. Tests:
   - catalog mapping for unique/prefab/spawn/POI;
   - deduplication across overlapping query rects;
   - cluster count and expansion;
   - screen position for negative coordinates;
   - disabled category is not drawn or fetched (or fetched but filtered; choose
     and document).
8. Manual check with the fixture seed: spawn marker, a unique prefab, a biome
   prefab; toggle categories; zoom out to cluster.

## Acceptance

```sh
cargo test -p map-core features
cargo test -p map-app features
cargo run -p map-app
```

- Feature icons appear at the correct block positions.
- Per-category toggles hide/show icons without reloading the map.
- Zooming out clusters nearby icons and shows a count.
- Clicking copies the feature coordinates; hover shows a tooltip.
- No feature from a previous seed/version remains after a source change.
- The overlay fetch does not block panning/zooming.

## Tests

- `map-core/tests/features.rs`: catalog/dedupe/cluster.
- `map-app` state test: viewport change schedules a fetch and applies the
  result.
- A fixture comparison: feature screen positions for a known viewport match
  the coordinate transform.

## Risks / notes

- The worldgen query can be expensive for a large visible rect. Cache and
  margin-expand; do not query on every frame.
- Prefab names can be long and noisy; keep the tooltip readable and use the
  category for the icon.
- If a category has thousands of features, drawing every icon is slow. Cluster
  early and add a hard cap per frame; T33 can profile.
