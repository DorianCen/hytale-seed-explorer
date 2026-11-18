# T12 — egui shell, seed input and app state

**Status:** done (deliverable: `map-app/src/state.rs` — the headless state
machine (seed parsing, `SourceState`, generation counter, stale-result
rejection, retired-source list, view/layer/overlay placeholders,
`format_status`) with 10 unit tests — plus `app.rs` (top bar, left panel,
`MapView` placeholder reserved for T16, status bar, error banner +
retry, background load thread, `G`/`F`/`Ctrl+S` shortcuts),
`labels.rs` (every UI string in one place) and the new 1280×800
`main.rs`; `default_world_path` shared into `map-core` so CLI and app
resolve the same world; README documents the launch command and
world-path assumptions)
**Milestone:** 2 — Static viewer
**Depends on:** T06, T07, T08, T10
**Blocks:** T16, T18, T31
**Plan refs:** [§7](../hytale-seed-map-plan.md#7-ui-layout), [§8](../hytale-seed-map-plan.md#8-milestones), [§1](../hytale-seed-map-plan.md#1-scope)
**Agent fit:** high

## Goal

Create the `map-app` shell: an `eframe` window with the plan's top bar, left
panel and status bar, a seed input that loads a `WorldgenSource` in the
background, and a central map placeholder. No tile rendering yet; the point is
to make source loading, seed changes, errors and view state explicit and
testable before adding the map.

## Context

The plan's layout:

```text
┌──────────────────────────────────────────────────────────────┐
│ Seed [__________] [Go]   Version [..]   Resolution [2x2 ▾]   │
├───────────────┬──────────────────────────────────────────────┤
│ Layers        │                                              │
│  ◉ Biomes     │                 MAP VIEW                     │
│  ○ Surface    │             (pan / zoom / click)             │
│  ○ Height     │                                              │
│  ○ Caves  Y▮  │                                              │
│ Overlays      │                                              │
│  ☑ Structures │                                              │
│ ...           │                                              │
│ Search        │                                              │
├───────────────┴──────────────────────────────────────────────┤
│ X:-1203 Z:455 | Biome: ... | Height: 71 | Block: ... | tiles  │
└──────────────────────────────────────────────────────────────┘
```

Milestone 2 is a static viewer: window, seed box, one texture, biome palette.
T13 adds the texture; this task adds the shell and the state machine.

## Scope

### In

- `map-app/src/main.rs` — `eframe::run_native`.
- `map-app/src/app.rs` — `SeedExplorerApp` implementing `eframe::App`.
- `map-app/src/state.rs` — headless-testable app state:
  - seed text, parsed `Seed`, parse error;
  - `SourceState::{Idle, Loading, Ready, Error}`;
  - source generation counter (stale load results are dropped);
  - view state (center, zoom) from T10;
  - layer/overlay settings placeholders;
  - status message.
- Top bar: seed text field, Go button, Enter handling, generator version
  display, resolution selector (1×1/2×2/4×4/8×8).
- Left panel: layer radio placeholders, overlay checkboxes placeholders,
  search section placeholder.
- Central map area: a placeholder drawing (grid + "load a seed" / "loading" /
  error text). T16 replaces the placeholder with the real widget.
- Bottom status bar: cursor coordinates (if available), biome, height, block,
  pending tiles, zoom/step, source status.
- Keyboard shortcuts: `G` opens a go-to dialog placeholder, `F` focuses the
  search section placeholder, `Ctrl+S` calls an export placeholder.
- Background source load:
  - spawn a thread that creates `WorldgenSourceConfig` + `WorldgenSource`;
  - send the result over a channel;
  - poll in `update`; never block the UI thread.
- Error banner with a retry action.

### Out

- Tile rendering, textures, pan/zoom (T13/T16).
- Real search (T24/T26), waypoints (T30), export (T32).
- Persistence (T31) beyond an optional in-memory recent-seed list.

## Work

1. Build the app skeleton with `eframe` and `egui`. Set a default viewport
   (1280×800), a title, and an `env_logger` initializer.
2. Implement `AppState` without egui types where possible:
   - `fn set_seed_text(&mut self, text: String)`;
   - `fn begin_load(&mut self) -> Option<LoadRequest>`;
   - `fn apply_load_result(&mut self, generation: u64, result: ...)`;
   - `fn invalidate_source(&mut self)`.
   Unit-test the state transitions (idle → loading → ready/error, seed change
   during loading drops the stale result).
3. Implement the top bar:
   - text edit with a monospace font;
   - Go button disabled when the text is empty or already loading;
   - Enter triggers Go;
   - show the generator version/identity once loaded;
   - resolution `ComboBox` with the default `2×2`.
4. Implement the left panel with the plan's groups. Use stable widget IDs and
   avoid rebuilding the tree from scratch; placeholder controls should still
   write to `AppState` so T13/T16 can wire them later.
5. Implement the central placeholder:
   - draw a dark background and a block grid;
   - show the current center/zoom;
   - show loading/error text with a spinner;
   - reserve a `MapView` struct/API for T16.
6. Implement status bar formatting as a pure function:
   `format_status(cursor, sample, pending, zoom/step, source_status) -> String`.
   Test it with synthetic values (including "unknown").
7. Add a source-load worker:
   - use a `std::sync::mpsc` channel;
   - generation counter;
   - on drop, signal shutdown and join if safe;
   - never send non-`Send` generator types to the UI thread; send only
     `WorldgenSource`/errors (the source is `Send + Sync` by design).
8. Add keyboard handling via `egui::InputState`; do not steal text-field keys.
9. Add a headless `AppState` test module and a `map-app` smoke test that
   constructs the app state without a window (if eframe's app struct allows
   it; otherwise test only the state module).
10. Document the app's launch command and the asset/world path assumptions in
    the README.

## Acceptance

```sh
cargo build -p map-app
cargo test -p map-app
cargo clippy -p map-app --all-targets -- -D warnings
cargo run -p map-app
```

- The window opens with the plan's three regions (top bar, left panel, map).
- Entering a valid seed and pressing Go shows a loading state, then ready.
- An invalid seed shows a readable error without panicking.
- Changing the seed while loading does not apply the old result.
- The resolution selector defaults to 2×2 and updates the state.
- The status bar formats coordinates/zoom even before a source is ready.
- No source load or generator work happens on the UI thread (verify with a
  deliberately slow load test or a log timestamp).

## Tests

- `map-app/src/state.rs` unit tests: transitions, stale-result rejection,
  invalid seed, resolution changes, status formatting.
- A `cargo test -p map-app` run that does not open a window.
- A manual smoke run with `testdata/Default` and a missing asset pack to verify
  the error path.

## Risks / notes

- `eframe`'s `update` is called continuously; do not spawn a new source on
  every frame. Use explicit user actions and the state machine.
- The source is heavy; dropping it on the UI thread may block while workers
  join. Move shutdown to a background thread or make `Drop` quick (send
  shutdown, join in a detached task).
- Keep all UI labels in one place for T34's docs/screenshots. Avoid
  hard-coding strings in multiple panels.
