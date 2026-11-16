# T20 — Surface block palette data

**Status:** proposed
**Milestone:** 5 — Full block mode
**Depends on:** T03, T09
**Blocks:** T21
**Plan refs:** [§5.3](../hytale-seed-map-plan.md#53-layers), [§10.7](../hytale-seed-map-plan.md#10-risks-and-open-questions)
**Agent fit:** high

## Goal

Provide the block-id → color data that full block mode needs. The palette must
cover vanilla blocks, use the decision from T03, degrade visibly for unknown
blocks, and be reproducible from the user's asset pack without committing
proprietary assets.

## Context

T03 decides the source. The recommended path is:

- **Runtime assets:** load the user's block/fluid metadata and build the
  palette in memory or in a user cache.
- **Fallback:** a small hand-picked table for common blocks so the app works
  without the pack.
- **Optional generated file:** only if licensing allows; include a source pack
  hash and schema version.

Reference implementation: `crates/hytale-server/src/map_image.rs` derives a
block color from `TintUp`/`Tint`, `BiomeTintUp`, `ParticleColor`, and blends
water using the environment `WaterTint` and fluid `ParticleColor`. Reuse the
formula; do not invent a second one.

The generator's `Registries` gives block/fluid/environment ids and names but
not tint colors. The asset pack or a generated palette must supply colors.

## Scope

### In

- `map-core/src/palette.rs`:
  - `BlockColor { rgb: [u8;3], biome_tint: f32, particle: Option<[u8;3]> }`;
  - `BlockPalette` with `color(block_id, biome_tint_rgb) -> [u8;4]`;
  - `PaletteSource` (runtime file/cache/fallback) and loading errors;
  - `PaletteVersion`/source pack hash in the identity.
- `map-cli palette` or `tools/build-palette`:
  - input: extracted asset tree or `Assets/Assets.zip` + registries;
  - output: a JSON palette with schema version, pack hash and entries;
  - flags: `--input`, `--out`, `--registries`, `--fallback`, `--allow-commit`
    (documentation only; do not enforce legal policy in code).
- `assets/palette/fallback.json` (small, hand-picked, committed).
- Tests:
  - known block colors match the server `map_image` formula;
  - unknown block fallback;
  - biome tint/particle combination;
  - deterministic output hash for the same input.

### Out

- The full-block renderer/progressive refinement (T21).
- Texture decoding/averaging unless T03 explicitly chooses it.
- A GUI palette editor.

## Palette schema

```json
{
  "schema": 1,
  "source": "hytale-assets",
  "pack_hash": "sha256:...",
  "generated_by": "map-cli palette 0.1.0",
  "entries": {
    "Rock_Stone": {
      "id": 42,
      "rgb": [128, 128, 128],
      "biome_tint": 0.0,
      "particle": [120, 120, 120]
    }
  }
}
```

Prefer keying by **name** in the file and resolving to ids through the
registries at load time. Ids can swap between packs/versions; names are stable.
If the file must be id-keyed for speed, include the registries hash and verify
it on load.

## Work

1. Implement the color formula from `map_image.rs`:
   - base tint: `TintUp` or `Tint`, default white;
   - biome multiplier: `BiomeTintUp / 100`;
   - particle color multiplication when the biome multiplier is < 1;
   - alpha 255 for known, opaque blocks.
   Document the exact order.
2. Implement the fallback palette:
   - a few common blocks (grass/soil/stone/sand/snow/water-adjacent) with
     approximate colors;
   - unknown blocks: use `DrawType`/`Opacity` from `Registries` to pick a
     neutral grey or a deterministic hue; magenta only when no metadata exists.
   - keep the file small and human-editable.
3. Implement the palette tool:
   - read `registries.json` for names/ids;
   - read block/fluid/environment asset JSON from the extracted tree or zip;
   - emit entries sorted by name, stable serde ordering;
   - compute a SHA-256 of the input manifest(s) for provenance;
   - do not include texture bytes.
4. Implement `BlockPalette::load`:
   - load the generated file if present;
   - otherwise load the fallback;
   - otherwise use the registry draw type;
   - verify schema and (if present) pack/registries hash; on mismatch, warn and
     fall back rather than silently using wrong ids.
5. Add the palette to `GeneratorIdentity` so changing it invalidates tiles.
6. Tests:
   - synthetic block JSON matching `map_image` output for 3–5 blocks;
   - biome tint 0/0.5/1;
   - particle multiplication;
   - name→id resolution with a swapped id order;
   - unknown block fallback;
   - deterministic JSON bytes for the same input.
7. Document how to regenerate the file and why it may or may not be committed
   (T03).

## Acceptance

```sh
cargo test -p map-core palette
cargo run -p map-cli -- palette --input ../hytale-server-rust/Assets/extracted \
  --registries ../hytale-server-rust/testdata/golden/registries.json \
  --out target/palette.json
cargo clippy -p map-core -p map-cli --all-targets -- -D warnings
```

- The palette loads and returns a color for every vanilla block id in the
  registries.
- Unknown blocks are visible, not transparent.
- The generated file has a schema version and source hash.
- The fallback palette works without the asset pack.
- The same input produces byte-identical output.

## Tests

- `map-core/tests/palette.rs`: formula, fallback, name/id resolution.
- A CLI test for `palette --help` and a tiny synthetic input.
- A fixture comparing a handful of colors with `map_image` values.

## Risks / notes

- Block ids are pack-dependent. Key by name in the palette and resolve ids at
  load; if you must ship ids, verify the registries hash.
- Texture averaging is a bigger tool and a licensing question. Do not add it
  here unless T03 chose it.
- Water is a fluid, not a block; the fluid color/blend belongs to the surface
  renderer (T21) or the fluid layer (T17). Keep the palette focused on blocks.
