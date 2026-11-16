# T31 — Share strings, settings and seed history

**Status:** proposed
**Milestone:** 9 — Tools and persistence
**Depends on:** T12, T16, T30
**Blocks:** —
**Plan refs:** [§1](../hytale-seed-map-plan.md#1-scope), [§4](../hytale-seed-map-plan.md#4-tech-choices), [§7](../hytale-seed-map-plan.md#7-ui-layout)
**Agent fit:** medium

## Goal

Make a map state shareable and the app pleasant to reopen: a compact share
string containing seed, position, zoom and enabled layers; persisted settings;
and a recent-seed history.

## Context

Plan requirements:

- shareable state: seed, position, zoom and enabled layers in one string;
- persistence: settings, waypoints, seed history via `serde`, `serde_json`,
  `directories`;
- clipboard / share using `arboard` or egui's built-in clipboard;
- seed history in the top bar.

T12 has the seed input and state; T16 the view; T19 the layer settings; T30
waypoint persistence.

## Scope

### In

- `map-core/src/share.rs`:
  - `ShareState` with schema version, seed input, center, zoom, min step,
    enabled layers/opacities, overlay toggles;
  - encode/decode to a compact URL-safe string;
  - validation and human-readable errors;
  - optional CLI: `map-cli share <string>` to inspect.
- `map-app/src/settings.rs`:
  - `Settings { window, min_step, layers, overlays, theme, worker cap,
    last_seed, recent_seeds }`;
  - load/save from `directories::ProjectDirs` (config vs data);
  - atomic writes and corrupt-file recovery;
  - window size/position persistence (if eframe exposes it cleanly).
- Seed history:
  - a bounded list (for example 20) of recent valid seeds with timestamps;
  - top-bar dropdown/history; selecting one loads it.
- Share UI:
  - a "Share" button/menu: copy share string, copy coordinates, paste/import
    share string;
  - importing applies seed/view/layers and shows an error for invalid input;
  - clipboard via egui's built-in `ctx.copy_text` if sufficient; otherwise
    `arboard`.
- Tests for encode/decode, settings round-trip, history bounds and corrupt
  files.

### Out

- Server-side sharing or URL hosting.
- Sharing waypoints (decide; if included, extend the schema and test it).
- Cloud sync/accounts.

## Share string format

Recommended:

```text
hse1:<base64url(no-padding) of a compact JSON or binary payload>
```

Payload (versioned):

```json
{
  "v": 1,
  "seed": "0",
  "x": -1203.5,
  "z": 455.25,
  "zoom": 3,
  "step": 2,
  "layers": [{"id": "biome", "opacity": 1.0}, ...],
  "overlays": {"features": true, "grid": false, ...}
}
```

Rules:

- include a format version and reject unknown versions with a clear error;
- use the seed **input text** so a string seed round-trips once T02 defines
  it; include the generator-visible seed too if it differs;
- round coordinates to a sensible precision (for example 0.01 blocks) to keep
  the string short;
- do not include a generator version as an opaque number unless decoding can
  validate it; include it for display/warning;
- optional waypoints: if included, cap the count and document the privacy
  implication.

## Work

1. Implement `ShareState` and its serde model. Keep it independent of egui.
2. Implement encode/decode:
   - use `base64` URL-safe no-pad (add the dependency) or a compact custom
     string; prefer base64 JSON for debuggability;
   - tests for round-trip, version mismatch, truncated/corrupt data, unknown
     layer id, extreme coordinates.
3. Implement settings:
   - choose config dir (`ProjectDirs::config_dir`) for settings/history and
     data dir for waypoints;
   - schema version;
   - defaults if missing;
   - atomic save; on corrupt JSON, rename to `.bak` and start with defaults,
     show a warning.
4. Seed history:
   - only add after a successful source load, not on every keystroke;
   - dedupe by canonical seed;
   - cap; persist.
5. UI:
   - top-bar history dropdown; `Enter` on a history item loads it;
   - Share menu with copy/import;
   - import dialog text field + Apply; errors inline;
   - clipboard fallback test (manual).
6. Integration on import:
   - stop the current source/search;
   - set seed text and load;
   - once ready, apply center/zoom/step/layers/overlays;
   - if the generator identity differs, warn but apply the view.
7. Tests:
   - share round-trip with negative/fractional coordinates and every layer id;
   - settings round-trip and corrupt recovery;
   - history cap/dedupe;
   - import applies the state only after the source is ready;
   - invalid share string does not change the current state.
8. Manual: copy a share string, restart the app, import it, verify the exact
   view/layers; open the settings file and confirm it is valid JSON.

## Acceptance

```sh
cargo test -p map-core share
cargo test -p map-app settings
cargo run -p map-app
```

- A copied share string restores seed, position, zoom, step and layers in a
  fresh app instance.
- Invalid/truncated strings show a readable error and leave the current state
  intact.
- Settings and seed history persist across restarts; corrupt files do not
  crash.
- Seed history only contains successfully loaded seeds and is bounded.
- Clipboard copy works on the target platform (manual check).

## Tests

- `map-core/tests/share.rs`: round-trip, versions, corrupt input.
- `map-app` settings tests with a temp config dir.
- A manual restart/import checklist in the task report.

## Risks / notes

- Do not put the whole waypoint store in a share string by default; it can be
  large and private. If included, cap and make it explicit.
- `directories` path choices differ by OS; test with a temp dir, not the real
  user dir.
- A share string that references a different generator version should not
  silently pretend the terrain is identical; show a warning.
