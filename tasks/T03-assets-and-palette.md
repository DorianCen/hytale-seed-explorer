# T03 — Asset, palette and licensing decision

**Status:** done (deliverable: [`docs/assets-and-palette.md`](../docs/assets-and-palette.md);
probe: [`tools/block_color_probe.py`](../tools/block_color_probe.py))
**Milestone:** 0 — API audit
**Depends on:** —
**Blocks:** T20
**Plan refs:** [§5.3](../hytale-seed-map-plan.md#53-layers), [§10.7](../hytale-seed-map-plan.md#10-risks-and-open-questions)
**Agent fit:** medium

## Goal

Decide where biome colors, surface-block colors and water colors come from,
and whether the explorer may ship any data derived from the Hytale asset pack.
This decision must be made before the palette tool and full-block mode are
built, because it changes both the runtime dependencies and what may be
committed.

## Context

Useful existing sources:

- `crates/worldgen-hytale/src/biome/biome.rs` exposes `Biome::get_map_color()`;
  this is the generator's own biome map color, not a hand-picked palette.
- `crates/hytale-server/src/map_image.rs` renders the server's world-map image.
  It derives a block color from the item/block asset JSON:
  `TintUp` or `Tint`, `BiomeTintUp`, `ParticleColor`, and blends water using
  the environment `WaterTint` and fluid `ParticleColor`.
- `crates/hytale-assets/src/**` can load the asset pack and expose block, fluid
  and environment records, but it is a large dependency intended for the server.
- The base-game pack is at `Assets/Assets.zip` and an extracted tree is at
  `Assets/extracted/` in the `hytale-server-rust` checkout; the Default world is
  at `Assets/extracted/Server/World/Default` and `testdata/Default`.
- `testdata/golden/registries.json` contains block/fluid/environment ids,
  names, draw types and opacity. It does not contain tint colors.
- The plan's §5.3 wants "per-block color table derived from block textures
  (average color), generated once by a tool, then stored as data"; the plan's
  §10.7 flags licensing as unresolved.

## Scope

### In

- Determine the legal and practical options for:
  - biome colors (generator `get_map_color` — safe, no asset data needed);
  - surface block colors (asset JSON tint/particle vs texture averaging vs
    hand-picked fallback);
  - water/fluid colors (environment `WaterTint` + fluid metadata);
  - unknown/modded blocks.
- Decide whether the explorer loads the asset pack at runtime, generates a
  palette file at build time, ships a hand-picked fallback, or combines these.
- Record the exact fields and parsing rules.
- Record the license/attribution constraints in the README/release docs.
- Produce a sample palette for a few blocks to prove the chosen path works.

### Out

- Implementing the full palette tool (T20) or full-block renderer (T21).
- Bundling Hytale textures, models or other proprietary assets.
- Reverse-engineering the client shader; the server's `map_image.rs` is the
  reference, not a byte-exact target.

## Deliverable

`docs/assets-and-palette.md` with a decision table:

| Data | Source | Runtime or generated | Committed? | Fallback | Notes |
|---|---|---|---|---|---|
| Biome map color | `Biome::get_map_color()` | runtime from generator | no asset data | magenta | exact generator value |
| Surface block color | asset JSON tint/particle *or* generated palette | TBD | TBD | draw-type grey | decision required |
| Water color | environment `WaterTint` + fluid `ParticleColor` | runtime or generated | TBD | blue | depth shading |
| Unknown/modded block | deterministic hash color or magenta | runtime | n/a | magenta | debug visibility |

Plus:

- A recommended architecture: `BlockPalette` trait / data type in map-core,
  with a generated file or runtime provider behind it.
- A licensing section that a release user can understand: where to put the
  asset pack, never commit it, what derived data is safe.
- A tiny proof: generate colors for 3–5 known blocks (for example a grass,
  rock, sand and water block) and compare them with `map_image.rs` output or
  the asset JSON.

## Work

1. Inspect `map_image::block_color` and `blend_fluid` and write down the exact
   JSON fields and formulas. This is the cheapest, already-proven path and
   avoids texture decoding.
2. Inspect `hytale-assets` to see whether it can load only the block/fluid
   metadata without the full asset registry. If it is too heavy, decide to
   parse the small subset of asset JSON directly in a palette tool.
3. Evaluate texture averaging:
   - where the block textures live in the pack;
   - what image format and average method would be used;
   - whether the result is materially better than the tint/particle fields.
   Do not add an image-decoding dependency unless the visual gain is clear.
4. Decide the distribution model:
   - **Runtime assets:** the user points the explorer at their own pack; the
     explorer builds a palette in memory or in the user cache. No derived data
     is committed. This is the safest default.
   - **Generated palette:** the tool writes a numeric palette; commit it only
     if the licensing review allows it. Include a source pack hash and schema
     version.
   - **Fallback palette:** a small hand-picked table for common blocks, so the
     app is usable without the pack. Mark it as approximate.
   Recommend runtime assets + fallback; make the generated palette optional.
5. Decide the unknown-block behavior. Use the registry `drawType`/`opacity`
   for a neutral grey and a deterministic hue; reserve magenta for "no
   metadata at all".
6. Write the doc and a short README section: "You must supply your own Hytale
   assets; they are not distributed with this tool."
7. Hand the schema to T20; do not implement the tool here.

## Acceptance

- The decision table is complete, with no TBD left for the v1 path.
- The doc names exact files/fields and includes a small color proof.
- The licensing section is explicit about what is not committed.
- T20 can implement the palette without re-litigating the source.

## Tests

- If a probe is added, it must be a small `tools/` script or a doc example,
  not production code.
- Verify the documented JSON fields exist in the extracted pack or
  `testdata/Default` and that the sample colors are reproducible.

## Risks / notes

- Numeric colors derived from proprietary assets may still be legally
  sensitive. When unsure, default to runtime generation by the user and do not
  commit the output.
- Texture averaging may look better but introduces a larger tool and a
  redistributable-data question. Prefer the proven tint/particle path first.
- Biome `get_map_color` is not necessarily the same as the in-game map; it is
  the generator's map color. Use it for the biome layer and document the
  difference from full-block surface rendering.
