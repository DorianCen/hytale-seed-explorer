# T18 — Inspector readout and resolution selector

**Status:** proposed
**Milestone:** 4 — Layers and inspector
**Depends on:** T09, T12, T16
**Blocks:** T19, T26
**Plan refs:** [§1](../hytale-seed-map-plan.md#1-scope), [§5.2](../hytale-seed-map-plan.md#52-resolution-modes), [§7](../hytale-seed-map-plan.md#7-ui-layout)
**Agent fit:** medium

## Goal

Make the map informative: a hover readout with block X/Z, biome, height,
surface block and fluid; a resolution selector; and go-to coordinates. These
are the plan's default-resolution and inspector requirements and the data source
for later feature/search UI.

## Context

Available data:

- `WorldSource::biome_at(x,z)` — cheap;
- `WorldSource::surface_at(x,z)` — may generate a chunk summary (T09), so it
  is not free;
- `BiomeCatalog` / `BlockCatalog` / `FluidCatalog` — names for display;
- `SurfaceSample` — height, top block, fluid, depth, tint.

The plan's status bar:

```text
X:-1203 Z:455 | Biome: ... | Height: 71 | Block: ... | tiles
```

The resolution selector chooses the user's **minimum** step (finest allowed);
the renderer picks `max(user_step, step_needed_for_zoom)`. Default is 2×2.

## Scope

### In

- Hover sampling pipeline:
  - convert cursor to block coordinates (T16);
  - immediately show block X/Z and zoom/step;
  - debounce surface sampling (for example 50–100 ms) and run it off the UI
    thread;
  - cache the last N hover samples by block coordinate;
  - show "..." while a slow surface sample is pending, never freeze the UI.
- Status bar formatting with biome/height/block/fluid/tiles; include
  "out of world" for invalid coordinates.
- A collapsible inspector panel with the same data, including block/fluid
  names and ids.
- Resolution selector:
  - `1×1`, `2×2` (default), `4×4`, `8×8`;
  - updates `AppState.min_step` and tile keys;
  - shows the effective step/zoom in the status bar.
- Go-to coordinates:
  - `G` opens a modal/popup with `x`, `z` inputs;
  - parse integers, allow negative values and whitespace/comma;
  - center the view and close; invalid input shows an inline error;
  - optionally support `/tp x y z` paste by taking the first and last numbers.
- Keyboard: `G` go-to, `Esc` closes.
- Tests for parsing, debounce state and formatting.

### Out

- Feature/search result clicks (T23/T26).
- Copy-coordinate/share-string formatting (T31).
- A full block/entity inspector; only surface data.

## Work

1. Add an `InspectorState` in map-app:
   - `hovered: Option<BlockPos>`;
   - `sample: Option<SurfaceSample>`;
   - `pending: bool`;
   - `error: Option<String>`;
   - `last_requested: Option<BlockPos>` and a timer/debounce.
2. Implement sampling:
   - on hover change, set `pending`, spawn a worker (or use a small query
     thread) that calls `surface_at` and `biome_at`;
   - tag requests with a generation/coordinate; ignore stale replies;
   - limit in-flight hover requests to one; cancel/replace the previous;
   - keep a small LRU of recent samples to avoid re-querying.
3. Format the status bar:
   ```text
   X:-1203 Z:455 | Biome: Forest | Height: 71 | Block: Rock_Grass | tiles 3
   ```
   Unknown values: `Biome: ?`, `Height: ?`, `Block: ?`, `Fluid: -`.
   Use a pure `format_readout(...)` with unit tests.
4. Add the inspector panel with:
   - coordinates, biome name/id, height, block name/id, fluid name/id/depth,
     tint (hex), zoom/step;
   - a "copy coordinates" button (uses the clipboard helper, not a new dep).
5. Resolution selector:
   - store `min_step: u32` in `AppState`;
   - ensure it is a power of two (1,2,4,8,...);
   - invalidate/request tiles when it changes;
   - `effective_step(min_step, zoom)` from T10;
   - display "2×2 (effective 4×4)" when zoom forces a coarser step.
6. Go-to:
   - a small popup state (`open`, `x_text`, `z_text`, `error`);
   - parse with `i32::from_str` after trimming; accept `x,z` in one field as a
     convenience;
   - clamp? Prefer reject if outside the generator bounds and show the range;
   - center the view; keep the current zoom.
7. Add tests:
   - readout formatting with all fields, missing fields and out-of-world;
   - resolution changes map to the expected effective step;
   - go-to parsing: `-1203 455`, `-1203,455`, whitespace, invalid, overflow;
   - stale hover reply does not overwrite a newer coordinate.
8. Document the debounce/in-flight policy in the module docs.

## Acceptance

```sh
cargo test -p map-app inspector
cargo test -p map-app resolution
cargo run -p map-app
```

- Hovering shows correct block X/Z immediately and biome/height/block when the
  sample arrives.
- Moving the cursor quickly never freezes or shows a sample for the wrong
  coordinate.
- The resolution selector changes the rendered tile detail and the default is
  2×2.
- `G` centers the map on valid coordinates and shows an inline error for
  invalid input.
- The status bar matches the plan's format.

## Tests

- Pure state/format tests in `map-app` (no GPU).
- A fixture comparison: for a known `(seed,x,z)` from T05, the inspector shows
  the expected biome/height/block names.
- Manual: hover across a water edge and a prefab, verify fluid/block names.

## Risks / notes

- `surface_at` can be expensive on a cache miss. Do not call it on every
  pointer move; debounce and cache.
- The UI thread must never wait for a source reply. Use a channel and a
  generation tag.
- Block names come from the registry; a missing id should show `block#123`
  rather than panic.
