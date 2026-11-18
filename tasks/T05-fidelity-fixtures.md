# T05 — Fidelity fixture suite and test harness

**Status:** done (deliverable: [`fixtures/`](../fixtures/README.md) —
`fidelity-v1.json` (10 cases, 3 seeds, negative coords, fluid columns; 9
features) + schema + README; probe committed as `worldgen-cli`'s
`fidelity_check` binary, worldgen repo commit `8060235`)
**Milestone:** 0–1 — audit and scaffold
**Depends on:** T01
**Blocks:** T11, T22
**Plan refs:** [§9](../hytale-seed-map-plan.md#9-performance-correctness-and-testing), [§2](../hytale-seed-map-plan.md#2-what-the-explorer-needs-from-your-worldgen)
**Agent fit:** medium

## Goal

Build the regression suite that answers "does the map match the game?" with
small, versioned fixtures. The plan calls this the most valuable test: a map
that disagrees with the real game is worse than no map.

## Context

Two complementary sources exist:

1. **Java golden data** in `hytale-server-rust/testdata/golden/`:
   - `chunks.json` — 21 chunks / 21,504 columns with blocks, rotations,
     fillers, fluids, fluid levels, environments and priority bytes;
   - `loader.json` — zone/biome/cave loader values;
   - `registries.json` — block/fluid/environment ids and metadata.
2. **Live Hytale server** — the authoritative game behavior. Use a known seed,
   a few fixed coordinates, and record:
   - biome name/id;
   - height;
   - surface block and fluid at the surface;
   - unique/prefab feature positions with names and categories;
   - cave entrance positions if obtainable;
   - the world config seed and generator version.

The explorer should not commit large generated dumps. Keep the fixture file
small (tens of rows), deterministic, and traceable to a seed + asset pack
version.

## Scope

### In

- Define a compact JSON schema for fidelity fixtures.
- Curate an initial fixture set from the golden chunks and, where possible, the
  live game.
- Define the update procedure and provenance fields.
- Add a test harness skeleton that later tasks can call from map-core.
- Cover negative coordinates and at least two seeds.

### Out

- Full chunk byte comparison (the worldgen repo already has that).
- A live-game automation framework; a documented manual collection procedure
  is acceptable.
- Committing the asset pack or screenshots that reveal proprietary content.

## Deliverable

```text
fixtures/
├─ README.md                 # provenance, how to regenerate, licensing note
├─ fidelity-v1.json          # schema version + cases
└─ schemas/fidelity.schema.json  # optional JSON schema
```

Proposed `fidelity-v1.json` shape:

```json
{
  "schema": 1,
  "generator_version": 1,
  "world_assets": "Default@<pack-hash-or-version>",
  "cases": [
    {
      "seed": "0",
      "world_seed": 0,
      "generator_seed": 0,
      "x": -1203,
      "z": 455,
      "biome": "Forest",
      "height": 71,
      "surface_block": "Rock_Stone",
      "surface_fluid": null,
      "source": "live-server|golden-chunks|worldgen-test"
    }
  ],
  "features": [
    {
      "seed": "0",
      "kind": "unique-prefab",
      "name": "Spawn",
      "category": "Unique",
      "x": 0,
      "y": 64,
      "z": 0,
      "show_on_map": true,
      "source": "golden-chunks|live-server"
    }
  ]
}
```

A companion `fixtures/README.md` records:

- exact game version / asset pack / world path;
- how each case was collected;
- which cases are authoritative vs supplemental;
- how to regenerate after a generator version bump.

## Work

1. Define the schema and freeze the first version. Use strings for block/biome
   names and seed input, numbers for coordinates; make missing values explicit
   (`null`).
2. Extract a starter set from `testdata/golden/chunks.json`:
   - pick 1–2 seeds and 8–12 columns per seed;
   - include negative coordinates and a column with fluid/prefab content;
   - record the expected biome name, height, surface block and fluid.
   The worldgen repo's tests can dump the same values; use a small committed
   script under `tools/` if needed.
3. Collect or verify a smaller live-game set:
   - start a world with a known seed;
   - visit the fixed coordinates and record the values;
   - prefer coordinates near spawn and one far-away coordinate;
   - record the exact game/build and asset pack.
   If the live server is unavailable, mark the cases as pending and do not
   guess.
4. Define feature cases:
   - unique prefabs from `ChunkGenerator::get_unique_prefabs` /
     `get_spawn_points` for the fixture seeds;
   - after T22, biome prefab placements and cave entrances.
   Record name, category, position, bounds and `show_on_map`.
5. Add the harness skeleton:
   - `map-core/tests/fidelity.rs` (created after T07) that loads the JSON and
     compares `WorldSource` results;
   - a `map-cli fidelity --fixtures ...` command later if useful;
   - assert on exact names/heights/blocks where the source is authoritative,
     and on distance tolerance for features if the game rounds positions.
6. Add a CI-suitable subset: the golden-derived cases can run without the live
   game; the live-game cases are opt-in behind an environment variable or an
   `--ignored` test.
7. Document how to update the fixture when `GENERATOR_VERSION` changes. Never
   silently accept a mismatch.

## Acceptance

- The fixture schema and README exist; the first fixture file has at least 8
  cases and 2 seeds, including negative coordinates.
- Every case has provenance and a generator version.
- The harness skeleton is runnable once map-core exists; until then, the
  values are verified against the golden chunks by a small documented probe.
- A wrong biome/height/surface block fails the test with a readable diff.
- No game assets or large dumps are committed.

## Tests

```sh
# once map-core exists
cargo test -p map-core --test fidelity
# live-game cases, when explicitly enabled
HYTALE_FIDELITY_LIVE=1 cargo test -p map-core --test fidelity -- --ignored
```

## Risks / notes

- The live game may change generation between versions. Record the version and
  keep old fixtures under a new schema/version instead of overwriting them.
- A fixture taken from the Rust port only proves self-consistency, not game
  fidelity. Mark the source honestly.
- If a mismatch is caused by a missing asset pack or registry snapshot, fix the
  environment before changing the fixture.
