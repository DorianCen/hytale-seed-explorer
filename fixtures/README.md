# Fidelity fixtures

The regression data that answers **"does the map match the game?"** — the
plan's most valuable test (plan §9). A map that disagrees with the real game
is worse than no map, so every case carries an honest `source` and nothing is
guessed.

## Files

| File | Purpose |
|---|---|
| [`fidelity-v1.json`](fidelity-v1.json) | The cases: 10 world columns (seeds 0/1/42, negative coordinates, water and lava columns) + 9 feature entries (Spawn/Temple unique prefabs and spawn points per seed). |
| [`schemas/fidelity.schema.json`](schemas/fidelity.schema.json) | JSON Schema (draft-07) for the fixture shape. |

## Provenance — which values are authoritative

| `source` | Meaning | Coverage in v1 |
|---|---|---|
| `golden-chunks` | **Java-authoritative.** Extracted from `hytale-server-rust/testdata/golden/chunks.json`, produced by the real Java pipeline (`tools/java-dumper` `ChunkDumper.dumpChunks`: the vanilla asset pipeline boots the pack, the full `ChunkGeneratorJsonLoader.load()` runs the Default world, `ChunkGeneratorExecution.execute(seed)` runs the real stage order) for seeds 0/1/42 and chunks (0,0), (4,−5), (9,4), (7,3), (−12,−25), (16,−8), (30,15). | every case's `biome`, `surface_block`, `surface_fluid` |
| `golden-chunks` (derived) | `height` is derived from the same dump: the `yEnd` of the last non-air block run of the column. The derivation rule is part of the fixture's semantics (it is the game-visible surface, not the generator's noise height). | every case's `height` |
| `worldgen-test` | **Rust-port self-consistency** — proves the explorer's generator adapter sees the same values the port produces; NOT game fidelity by itself. Names and order of the unique prefabs are additionally pinned by `worldgen-hytale/tests/unique_prefabs.rs`. | all 9 feature entries |
| `live-server` | Recorded in the actual game. | **none yet — pending, see below** |
| `pending-live` | Placeholder, never trusted. | none |

Biome names are the generator's compound asset names (`River_Forest_Birch`,
`Plains_Tallgrass`, …), not display names — the in-game map/inspector will
show these until a translation layer exists.

## How to regenerate / verify

The fixture body was produced by the committed probe
`crates/worldgen-cli/src/bin/fidelity_check.rs` in the worldgen repo
(manual tool; not part of `cargo test`, not an explorer dependency):

```sh
cd /home/chome/Projects/hytale-server-rust

# verify the committed fixture against the golden dump + the generator
./target/release/fidelity_check check \
  testdata/Default testdata/golden/chunks.json \
  ../hytale-seed-explorer/fixtures/fidelity-v1.json
# → OK: 10 cases and 9 features match (exit 0); any mismatch prints a
#   readable per-case diff and exits 1.

# regenerate after an intended change (see the update policy below)
cargo run --release -p worldgen-cli --bin fidelity_check -- \
  extract testdata/Default testdata/golden/chunks.json
```

The case grid is the `CASES` constant in the probe; new cases (more seeds,
more chunks, a T22 biome-prefab/cave-entrance section once the placement
query exists) are added there and re-extracted.

## Update policy — `GENERATOR_VERSION` changes

1. Run `check` first. **Never silently accept a mismatch.**
2. If the mismatch is an unintended regression, fix the generator (worldgen
   repo), do not touch the fixture.
3. If the generator change is intended and shipped with a
   `GENERATOR_VERSION` bump (worldgen repo policy), re-run `extract`, review
   the diff, update `generator_version` in the JSON, and record the bump
   reason in the commit message.
4. Live-game fixtures are append-only: when the game itself changes
   generation between versions, keep the old cases under a new
   `world_assets` value or a new `fidelity-vN.json` instead of overwriting.

## Pending: live-game cases (manual collection procedure)

No live-server values are recorded yet — the fixture has no guessed entries.
To collect them:

1. Start a Hytale world with a known numeric seed (0, 1 or 42 first) and the
   same asset pack the worldgen repo uses.
2. Teleport to each fixture coordinate and record: the biome shown in-game,
   the surface block, any fluid, and the height the game reports (F3-style
   readout or map). Prefer coordinates near spawn plus one far-away column.
3. Record the exact game build, asset pack and world path in the case's
   `notes`, set `"source": "live-server"`.
4. If a live value disagrees with the golden-derived value, file it as a
   mismatch investigation (env/pack drift first, per the T05 risk note) —
   do not overwrite the golden case.

Once `map-core` exists (T07), the harness `map-core/tests/fidelity.rs` loads
this JSON and compares `WorldSource` results: golden-derived cases run in
CI without the live game; live-game cases are opt-in behind
`HYTALE_FIDELITY_LIVE=1 cargo test -p map-core --test fidelity -- --ignored`.

## Licensing

The fixture values (block/biome/fluid names at coordinates, heights) are
derived from the user-supplied Hytale asset pack's golden dumps, obtained
under the Hytale Shared Source Agreement — the same status as the worldgen
repo's committed golden data. The values here are small factual data points
for verification. **If this repository is ever made public, strip this
directory's game-derived values and regenerate them locally** (see
[`docs/assets-and-palette.md`](../docs/assets-and-palette.md) §2 for the
rules). No assets, no large dumps, no screenshots are committed here.
