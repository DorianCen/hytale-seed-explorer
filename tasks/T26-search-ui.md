# T26 — Search UI, cancellation and filters

**Status:** done (deliverable: `map-app/src/search_state.rs` — the headless
`SearchState`/`SearchDriver` seam: generation-guarded start, ≤64-event
per-frame draining, result dedupe/binary-ordered insertion, cancel-then-Done,
stale-event rejection, selected-hit identity and the one-second flash;
`map-app/src/search_panel.rs` — the left-panel form (feature kind/category/
name/min-size/proximity or biome/patch/coarse-step targets, view or custom
center, 1k/5k/10k/25k radius, result cap), the virtualized results table with
category icons and `1.2 km` distances, click-to-center/flash, double-click
zoom and right-click teleport copy; app wiring for `F`, `Esc`, source-change
cancellation, a 50 ms poll repaint and the map flash ring; 20 headless
`search` tests including real T24/T25 driver runs against the fake sources.
Manual acceptance was run against the X11 display with the real fixture
(`cargo run -p map-app`, seed 12345): a Unique-prefabs 10k search streamed
two rows, clicking a row centered the view (including negative coordinates)
and drew the flash ring, right-click copied `/tp -333 120 302`, and `Esc`
turned a running search into `cancelling…`. The final cold-load search (the
14-tile map still generating on the shared two-worker pool) measured
119.4 s; a settled/warm re-search on the same view reported `done: 2
results · 0.0 s`. The UI uses 128-block regions for biome-prefab/cave
searches (prompt cancellation, inside the T27 cave cap) and 2048-block
regions for the cheap kinds (few pool round trips), and the default kind is
Unique prefabs rather than including the minutes-long biome pass. The
biome path is covered by the real-driver headless test and the panel's
request test; the manual mode-combo click was not automatable in this
session. The 64-event frame cap bounds the drain work.)
**Milestone:** 7 — Search
**Depends on:** T18, T23, T24, T25
**Blocks:** —
**Plan refs:** [§6.1](../hytale-seed-map-plan.md#6-search-design), [§6.2](../hytale-seed-map-plan.md#62-biome-search), [§7](../hytale-seed-map-plan.md#7-ui-layout)
**Agent fit:** high

## Goal

Expose the feature and biome search engines in the left panel: choose a search
kind/target, center point and radius, run/cancel a search, stream results into
a sorted table, click a result to center the map and flash its marker, and
right-click to copy a teleport string.

## Context

T24 streams feature results; T25 streams biome matches. T23 has feature
markers and a coordinate/teleport formatter. T18 has the status bar and go-to.
The plan's search panel:

```text
Search
  [kind ▾][Go]
  results ...
```

Requirements:

- feature kind/tag, center, max radius;
- results sorted by distance from the current view or a chosen point;
- each row: icon, name, coordinates, distance;
- click centers the map and flashes a marker;
- right-click copies a teleport-style coordinate string;
- optional filters: variant, minimum size, "within N blocks of another
  feature";
- cancellable, streamed results;
- first results within ~500 ms.

## Scope

### In

- `map-app/src/search_panel.rs`:
  - search mode: Features (kind/category) or Biome;
  - center: current view center or a manually entered point;
  - radius selector (1k/5k/10k/25k blocks);
  - feature filters (category, name contains, min size, proximity);
  - biome patch-area criterion;
  - Search/Cancel buttons, progress and result count;
  - results table/list with a virtualized scroll area;
  - click → center/flash; right-click → copy teleport; double-click → center
    and zoom in;
  - error display.
- `map-app/src/search_state.rs` (headless):
  - `SearchState::{Idle, Running, Done, Failed}`;
  - request generation, event draining, cancellation;
  - result insertion/dedupe/sort;
  - selected result/flash state.
- Integration with T23 markers and T16 centering.
- Keyboard `F` focuses the search panel; `Esc` cancels/clears.

### Out

- New search algorithms (T24/T25).
- Multi-criteria seed finder.
- Saving search results to a project (optional; T31 can add).

## Work

1. Build the search form:
   - a `SearchTarget` enum: `Features(FeatureFilter)` or `Biome(BiomeId)`;
   - a target picker populated from the feature catalog/biome catalog;
   - center mode "view center" vs "custom point"; custom point uses the T18
     coordinate parser;
   - radius and max-results controls;
   - filter fields only enabled for the matching target.
2. Implement `SearchState`:
   - `start(request)` increments a generation, spawns the engine thread (T24/
     T25) or uses their `SearchHandle`, stores the event receiver;
   - `poll()` drains a bounded number of events per frame (for example 64) to
     keep the frame under 16 ms;
   - `cancel()` calls the handle and transitions to `Cancelling`/`Idle`;
   - stale events from an older generation are ignored;
   - results are deduplicated and kept sorted by distance; if the engine
     already streams sorted, insert at the end and only re-sort if a correction
     arrives.
3. Results table:
   - columns: icon, name, category/kind, coordinates, distance;
   - use `egui::ScrollArea::show_rows` for large result sets;
   - highlight the selected/flashing result;
   - format distance in blocks (integer) and a human-readable `1.2 km` for
     large values.
4. Interaction:
   - left-click: set `AppState.center` to the result position and set a
     `flash_until` timestamp; T23 draws the ring;
   - right-click/context menu: copy `/tp x y z` using the shared formatter;
   - double-click: center and zoom to the result's step (optional).
5. Progress/cancel:
   - show "searching ring R, N results";
   - cancel button is always available while running;
   - closing the panel or changing the seed cancels the search.
6. Empty/error states:
   - no results within radius;
   - source error with a retry;
   - cancelled.
7. Tests:
   - `SearchState` transitions and stale-event rejection;
   - result dedupe/sort/format;
   - click centers correctly for negative coordinates;
   - cancel stops polling and does not leak the thread;
   - filter serialization/round-trip (if stored).
8. Manual acceptance with the fixture seed: search a common feature/prefab,
   click results, search a biome, cancel a large search.

## Acceptance

```sh
cargo test -p map-app search
cargo test -p map-core feature_search biome_search
cargo run -p map-app
```

- The search panel can run a feature search and a biome search.
- Results stream in with icon/name/coords/distance and are sorted by distance.
- Clicking a result centers the map and flashes the marker; right-click copies
  a teleport string.
- Cancel stops the search; a new search does not mix old results.
- The UI remains responsive during a 10k-radius search.
- The first results appear within the plan's ~500 ms target on the reference
  scene, or the measured number is documented.

## Tests

- `map-app` headless state tests with a fake search engine.
- Format/dedupe tests.
- A manual timing note in the task report: first result, 100 results, cancel.

## Risks / notes

- Draining an unbounded result stream on the UI thread can cause frame spikes.
  Cap events per frame and let the engine's bounded channel provide
  backpressure.
- A click that centers the map should not also copy coordinates (conflicting
  with T16's map click). Use the result row, not the map, for search result
  clicks.
- Biome search results may be approximate under look-ahead; show the distance
  and do not claim "nearest" if the engine reports approximate mode.
