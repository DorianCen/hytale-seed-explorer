# T27 — Cave entrance overlay

**Status:** done (deliverable: the worldgen cave stage's shared predicate
extraction (`CavePopulator::entrance_height` plus `entrance_zone`/
`entrance_height_matches`, so the populator and the query run exactly one
filter), `ChunkGenerator::cave_entrances_in` (zone/height column memo, the
point generator's interval padding for anchor rects, deterministic
`(x, z, y, name)` order, `PlacementKind::CAVE_ENTRANCE`, no `get_cave`), the
explorer adapter (`FeatureKind::CaveEntrance` → cave/zone names, catalog
category/icon/toggle already reserved), the 256-block overlay cap and the
visible-rect fetch rule for zoomed-in cave views, the zone-aware tooltip, and
the fixture tests (worldgen predicate/geometry/kind-filter/negative-rect;
map-core adapter/catalog/dedupe/cap). The 512×512 release benchmark is
61.9 s / 52,941 entrances; the 10,000×10,000 case is documented as a
~6.6–8 h batch job and is capped in the adapter (the task's "cap the rect"
allowance) rather than returned to the overlay. Deviation: the query pads the
anchor rect by the point generator's interval instead of the populator's
`maximum_size` — the larger reach exists to find caves crossing into a chunk,
not entry points inside a query rect; fixture chunk counts are unchanged.
Manual overlay verification remains the user's display-side check; the
worldgen `query`/`chunks_golden` gates, `map-core cave_entrances`, the full
`map-app` suite and `cargo build -p map-app` are green.)
**Milestone:** 8 — Caves
**Depends on:** T22, T23
**Blocks:** T28
**Plan refs:** [§5.4](../hytale-seed-map-plan.md#54-caves), [§6.3](../hytale-seed-map-plan.md#63-cave-search), [§2](../hytale-seed-map-plan.md#2-what-the-explorer-needs-from-your-worldgen)
**Agent fit:** high

## Goal

Enumerate cave entrances without generating cave geometry and show them as an
overlay. This is the plan's cheapest and most useful cave view: players want to
know where caves open onto the surface.

## Context

The authoritative entrance logic already exists in
`crates/worldgen-hytale/src/chunk/populator/cave_populator.rs`:

- for every zone with a cave generator and every cave type:
  - collect entry-point candidates from
    `CaveType::get_entry_point_generator().collect(seed, min_x, min_z, max_x,
    max_z, callback)` over the chunk padded by `CaveType::get_maximum_size()`;
  - for each candidate `(x, z)`:
    - `let result = chunk_generator.get_zone_biome_result_at(seed, x, z)`;
    - require the result's zone to be the same `Rc<Zone>` as the iterated zone;
    - require `cave_type.is_entry_threshold(seed, x, z)`;
    - require the height threshold:
      `cave_type.get_height_condition().eval3(seed.wrapping_add(0xF5ACDEB5u32 as i32), x, height, z)`
      where `height = chunk_generator.get_height(seed, x, z)`;
    - only then does it call `get_cave` and paste. The query must stop before
      `get_cave`.
- This is exactly the "placement pass" the plan asks for; it needs only
  zone/biome and height queries.

## Scope

### In

- Extend the worldgen query module (T22) with:
  - `cave_entrances_in(seed, rect) -> Vec<FeaturePlacement>`;
  - kind `CaveEntrance`, name = cave type name, position = `(x, height, z)`,
    optional bounds = a small marker box;
  - filtering by cave type/zone if useful.
- Explorer adapter:
  - `FeatureSource` maps cave entrances to `FeatureKind::CaveEntrance`;
  - catalog category/icon/toggle.
- Overlay:
  - draw entrance icons at the surface position;
  - toggle on/off;
  - tooltip with cave type/zone/coordinates;
  - click copies coordinates.
- Tests comparing entrances with generated cave geometry for fixture chunks.
- A benchmark for a 10,000×10,000 rect.

### Out

- Cave Y-slices and density (T28).
- Cave type search ("large cavern", "ravine") — stretch until the generator
  exposes type metadata; v1 only lists cave type names.
- Rendering the 3D cave graph.

## Work

1. In the worldgen repo, expose a shared entrance predicate:
   - extract the `run` filter from `CavePopulator` into a function that takes
     `seed`, `(x,z)`, `zone`, `cave_type`, `chunk_generator` and returns
     `Option<height>`;
   - make `CavePopulator::run` call it, then paste;
   - keep the exact zone identity check (`Rc::ptr_eq`), threshold order and
     magic constant `0xF5ACDEB5`.
2. Implement `cave_entrances_in`:
   - iterate the same zones/cave types as the populator;
   - `collect` over the query rect padded by `maximum_size`;
   - filter to the rect and the entrance predicate;
   - deduplicate `(cave_type, x, z)`;
   - return positions sorted by coordinates for deterministic output.
   Do not call `get_cave`.
3. Add the explorer adapter:
   - map `FeaturePlacement` kind `CaveEntrance` to the T23 catalog;
   - default category "Cave entrances", icon color (for example cyan);
   - cache with the feature overlay's rect cache.
4. Add tests:
   - for a fixture seed/rect, compare entrances with the actual cave blocks
     found near the surface in the golden chunks (a cave entrance should have
     a cave carve/air column within a few blocks);
   - no entrance outside `is_entry_threshold`/height condition;
   - same zone check (a cave type in another zone must not appear);
   - negative coordinates and rect edges;
   - determinism;
   - query does not generate cave geometry (assert timing or instrument the
     cache).
5. Add a benchmark and report the 10k×10k time. Cave point generators can be
   dense; if the query is slow, add a coarse prefilter or cap the rect.
6. Document the entrance semantics: a candidate is an entry point that would
   generate a cave, not a guaranteed walkable opening; the overlay is a
   prediction from the generator.

## Acceptance

```sh
cd /home/chome/Projects/hytale-server-rust
cargo test -p worldgen-hytale query
cargo test -p worldgen-hytale --test chunks_golden

cd /home/chome/Projects/hytale-seed-explorer
cargo test -p map-core cave_entrances
cargo run -p map-app
```

- Cave entrances appear as overlay icons with cave type names.
- The entrance set for fixture chunks matches the generated cave placements.
- Toggling the category hides/shows the icons.
- The query does not call `get_cave` / generate cave graphs.
- A 10k×10k query completes in a usable time (recorded).

## Tests

- `crates/worldgen-hytale/tests/cave_entrances.rs`: fixture seed, golden chunk
  cave voxels, negative coordinates, determinism.
- `map-core/tests/cave_entrances.rs`: adapter/catalog/dedupe.
- A manual overlay check: zoom into a known cave area and compare with the
  slice view (T28).

## Risks / notes

- The entrance predicate is subtle; do not reimplement it in the explorer. Use
  the shared worldgen function so a future generator change stays in sync.
- An entrance candidate may be inside a cave that does not actually reach the
  surface due to later carving; document the prediction and verify with the
  slice view.
- Cave point generators can produce many candidates; cap the query rect or
  process rings to keep the UI responsive.
