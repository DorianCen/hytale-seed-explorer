# T30 — Waypoints and annotations

**Status:** proposed
**Milestone:** 9 — Tools and persistence
**Depends on:** T16, T29
**Blocks:** T31, T32
**Plan refs:** [§1](../hytale-seed-map-plan.md#1-scope), [§5.5](../hytale-seed-map-plan.md#55-overlays-vector-drawn-on-top-not-baked-into-tiles), [§4](../hytale-seed-map-plan.md#4-tech-choices)
**Agent fit:** medium

## Goal

Let the user save named, colored points with notes, draw them on the map, and
edit/delete them. Waypoints are the first persistent user data and the model
for the project/share/settings work in T31.

## Context

The plan lists "Waypoints and annotations" and "Persistence: settings,
waypoints, seed history" using `serde`, `serde_json`, `directories`.

Design decisions needed:

- Are waypoints per seed/world or global? The map is seed-centric, so key them
  by the generator seed/identity; a global waypoint is confusing when the seed
  changes. Store a `seed` field and only show matching waypoints.
- Are annotations separate from waypoints? A note field on a waypoint covers
  v1; a free-floating text annotation is a stretch.
- Coordinates: block X/Z; optional Y (surface/height) for teleport strings.

## Scope

### In

- `map-core/src/project.rs`:
  - `Waypoint { id: Uuid or u64, name, color, x, z, y: Option<i32>, note,
    created_at }`;
  - `WaypointStore` with add/update/delete/list, serde;
  - validation (non-empty name, coordinate range, color).
- `map-app/src/overlays/waypoints.rs`:
  - draw markers with labels/colors;
  - hover tooltip with note;
  - click selects/copies; double-click opens edit;
  - right-click map → "Add waypoint here";
  - a waypoint list panel with edit/delete/zoom-to;
  - color picker (a small preset palette is enough).
- Persistence:
  - save/load to a JSON file in the user config/data directory (T31 owns the
    directory policy; this task defines the model and a simple save/load API);
  - atomic write (temp file + rename);
  - handle missing/corrupt files gracefully.
- Tests for CRUD, serde round-trip, seed filtering and coordinate parsing.

### Out

- Sharing waypoints in a URL/share string (T31 decides; v1 share state is
  map-only).
- Free-floating text annotations, routes, folders, icons beyond a preset set.
- Syncing waypoints between machines.

## Work

1. Define the model:
   - stable `id` (UUID if the dependency is acceptable, otherwise a
     timestamp+counter; do not use the array index);
   - `seed: i32` + `identity_hash: u64` so a generator version change does not
     silently attach old waypoints to new terrain (or warn and keep them);
   - `name`, `color: [u8;3]`, `x`, `z`, `y: Option<i32>`, `note`,
     `created_at`.
2. Implement `WaypointStore`:
   - `add`, `update`, `remove`, `get`, `iter_for_seed`;
   - duplicate-name warning (not an error);
   - serde with a schema version;
   - no panics on malformed input.
3. Persistence API:
   - `load(path) -> Result<WaypointStore, StoreError>` (missing file = empty);
   - `save(path)` with atomic rename;
   - a migration hook for schema versions (v1 only for now);
   - tests with a temp dir, corrupt JSON, unwritable path.
4. UI:
   - map right-click context menu with "Add waypoint here" (prefill coords);
   - an edit dialog: name, color, note, coordinates, delete;
   - list panel grouped by seed, with zoom-to and edit;
   - draw labels only when the marker is visible and zoom is sufficient;
     otherwise show a dot with a tooltip.
5. Hit testing:
   - reuse T23's screen-space hit-test helper; waypoints should be on top of
     feature icons or at least not block them (define order).
6. Tests:
   - add/update/delete/round-trip;
   - seed filtering;
   - negative coordinates;
   - corrupt file → empty store + error message, no panic;
   - atomic save leaves the old file if the write fails (test with a read-only
     dir where possible).
7. Manual: add waypoints at several coords, restart, verify they load; switch
   seed and verify they are hidden/warned.

## Acceptance

```sh
cargo test -p map-core waypoints
cargo test -p map-app waypoints
cargo run -p map-app
```

- Waypoints can be added from the map, named/colored/noted, edited and deleted.
- They are drawn at the correct block coordinates and persist across restarts.
- Switching seed does not show waypoints from another seed.
- A corrupt store does not crash the app.
- The list panel can zoom to a waypoint.
- No waypoint data is lost on an interrupted save (atomic write test).

## Tests

- `map-core/tests/waypoints.rs`: CRUD, serde, seed filter, corrupt file.
- `map-app` state tests: add/edit dialog transitions, hit testing.
- Manual restart test with a temp config dir.

## Risks / notes

- Do not key waypoints only by name; duplicates are allowed and names change.
- If the generator version changes, old waypoints may point to different
  terrain. Keep them but show a version/seed warning; do not delete user data.
- The persistence directory policy belongs to T31; this task can accept a path
  and let T31 choose it. Do not hard-code a path in two places.
