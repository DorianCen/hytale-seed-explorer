# Assets, palette and licensing decision (T03)

**Task:** [T03](../tasks/T03-assets-and-palette.md) — status: done.
**Unblocks:** [T20](../tasks/T20-surface-block-palette.md) (palette tool), and the
biome layer colors of T13/T17.

This document is the decision record: where every color the explorer draws comes
from, what may be committed, and the exact parsing rules. T20 implements the
palette tool from this spec without re-litigating the sources.

---

## 1. Decision table (v1 path — no TBD)

| Data | Source | Runtime or generated | Committed? | Fallback | Notes |
|---|---|---|---|---|---|
| Biome map color (biome layer) | `Biome::get_map_color()` read live from the world folder the user opens (`i32`, JSON key `MapColor`, loaded at biome_json_loader.rs:264-273) | runtime from generator | no asset data by this repo (the world folder is user-supplied, as worldgen already requires) | magenta `#FF00FF` | exact generator value; **not** what the in-game map shows — see §5 |
| Surface block color | `map_image.rs` formula over the user's own pack: `TintUp`/`Tint` → `BiomeTintUp` biome blend → `ParticleColor` multiply (§3, map_image.rs:220-249) | runtime; cached as a palette file in the **user cache dir** (never committed) | **no** — Shared Source Agreement (§2) | committed hand-picked table of ~30 common blocks, marked approximate, original values | palette keyed by block **name**, never by id (§4) |
| Water color | environment `WaterTint` × fluid `ParticleColor`, depth attenuation `1/(depth+1)` (map_image.rs:251-292) | runtime (same palette file) | no | hand-picked blue | 9 distinct `WaterTint` values across 43 environment files in the pack; water's fluid record has no color of its own |
| Other fluids (lava, tar, …) | fluid `ParticleColor` where present (8 fluids), else `FluidFX` fog color is *not* used for tiles | runtime (same palette file) | no | hand-picked (lava orange-red) | |
| Unknown / modded block | `drawType`/`opacity` neutral grey + deterministic hue from the block-name hash (T20 rule) | runtime | n/a | magenta reserved for "no metadata at all" | debug visibility |

**Distribution model (decided):** runtime assets + committed fallback; the
generated palette is optional and lives only in the user's cache directory.

## 2. Licensing (why "committed? = no" for anything pack-derived)

The Hytale asset pack and the shared Java source are **proprietary and
confidential**. They are governed by the *Hytale Shared Source Agreement v2.0*
(effective 2026-07-21, published at `hytale.com/shared-source-license`), which
the worldgen checkout's `.hytale-source/` and `Assets/` trees were obtained
under. The clauses that bind this project:

- **§3.2** — Licensed Assets may be used "solely as required to create, develop,
  and operate Your Content" within the Hytale ecosystem; you may **not**
  "reproduce, use, or distribute the Licensed Assets on a standalone or separate
  basis". A committed palette file extracted from the pack would be exactly
  that.
- **§6.1** — the Licensed Materials are confidential; "You will not post the
  Licensed Materials to any public repository or other publicly accessible
  location."
- **§7.4** — copying is limited to designated repositories / what is needed to
  operate permitted content.
- **§7.6** — content that is purely original and incorporates **no Hypixel IP**
  is unrestricted. The committed fallback palette is original hand-picked
  approximations, so it falls here — as long as it is *approximate and
  hand-picked*, not extracted values.

The pack itself ships **no license/EULA/readme file** (verified:
`find Assets/extracted -iname "*license*" -o -iname "*eula*" -o -iname
"*readme*" -o -iname "*legal*"` → zero results). The worldgen repo already
follows the right precedent: `.hytale-source/` and `Assets/` are gitignored
there ("not part of the source tree"). This repo does the same (see
`.gitignore`) and its README states the user-supply rule.

**Therefore, hard rules for this repository:**

1. Never commit the pack, the zip, textures, models, or any file extracted
   from them.
2. Never commit a generated palette, whole or partial. The palette tool writes
   to the user cache directory only; `.gitignore` lists the pattern.
3. The few raw hex values quoted in §6/§7 below (and in the committed golden
   data of the *private* worldgen repo, e.g. `BlockType.bin`'s palette fields)
   are verification data recorded under the same agreement the user already
   accepted. **If this repository is ever made public, strip §6's sample values
   and rely on the probe script to regenerate them locally.**
4. Texture averaging is **rejected** as a committed-data path anyway: the pack
   already ships the game's own texture averages (`TextureComputedColor`, hex,
   present in 2,986 files), and the in-game map's proven path is the
   tint/particle formula in §3 — no image-decoding dependency is needed for v1
   (plan §5.3's "average the textures" idea is thereby retired).

## 3. Exact formulas and parsing rules (the proven path)

Reference implementation: `crates/hytale-server/src/map_image.rs` (the port of
the Java in-game world map; the Java client shader is out of scope per T03).

**`block_color(assets, block_id, biome_tint)` — map_image.rs:220-249:**

1. `self = parse(TintUp) or parse(Tint) or (255,255,255)` — `TintUp` preferred,
   arrays allowed (first element used).
2. `m = BiomeTintUp / 100`, missing/non-numeric → `0.0`.
3. `c = self + (biome_tint − self) × m` per channel.
4. If `m < 1.0` and `ParticleColor` parses: `c = c × ParticleColor / 255` per
   channel.
5. Alpha 255. Unknown block id → transparent (skipped pixel).

Consequences measured on the real pack: the majority of blocks have no
`Tint`/`TintUp` and `BiomeTintUp = 0`, so their map color is **exactly their
`ParticleColor`** (2,869 block records carry it; ~274–309 technical blocks have
nothing → white). Exactly five blocks (`Soil_Grass`, `Soil_Grass_Wet`,
`Soil_Leaves`, `Soil_Leaves_Full`, `Soil_Seaweed_Block`) declare
`BiomeTintUp: 100` → they render as **the runtime biome tint, unconditionally**
(their `ParticleColor`/`TintUp` are irrelevant to the map). Non-empty `TintUp`
arrays exist in exactly one file (`Soil_Grass.json`, `["#67b62d"]`).

**Biome tint input:** *not* `get_map_color` — the in-game map uses the biome's
**`TintContainer`** value at the column, disk-averaged with radius 4
(chunk_generator_execution.rs:340-385, biome JSON key `Tint`, default
`0xFF0000`). This is a runtime, position-dependent value, so it **cannot be
pre-baked into a palette**: the palette stores `biome_tint_pct` per block and
the renderer blends with the generator's tint at draw time (§5).

**`blend_fluid(assets, base, fluid)` — map_image.rs:251-292:**

1. `tint = (255,255,255) × env.WaterTint / 255` (environment record of the
   fluid's environment), then `× fluid.ParticleColor / 255` if present.
2. If `WaterTint` exists: `c = tint + (base − tint) × 1/(depth+1)` — **shallow
   water shows the floor, deep water approaches the tint**.
3. Else: `c = tint × max(0.75, 1 − 0.02×depth)`.
4. Water's own fluid record (`Server/Item/Block/Fluids/Water_Source.json`) has
   no color field — its color is entirely the environment `WaterTint`
   (43 environment files declare it; 9 distinct values, dominated by
   `#1983d9` ×16 and `#2076b5` ×8).

**Hex parsing rules (map_image.rs:294-324):** string or array (first element);
optional `#`; 3-digit expands each digit; 6-digit direct; 8-digit takes the
last 6. Anything else → `None`. Legacy worldgen-config files also contain
bare `5b9e28` and `0xd5b656` forms — T20's palette tool must accept those two
prefix variants for robustness even though the block store doesn't use them.

**Height shading** (Lambert, factor 0.4–1.0, map_image.rs:131-142) and the map's
8-step quantization are **renderer** concerns, not palette data — the explorer's
full-block layer reuses the plan §5.3 hillshade instead.

## 4. Keying: by name, never by id

Block **ids are schedule-dependent**: the golden registry itself records the
caveat that "a real server decodes assets in parallel … its numeric ids are
schedule-dependent and NOT reproducible from this dump" (testdata/golden/
registries.json:11), and every consumer must use one consistent snapshot. The
explorer therefore:

- keys the palette and fallback table by **block name** (`Soil_Sand`, …),
- resolves names ↔ ids through *the generator's own registry snapshot*
  (`ChunkGenerator::get_registries()`, `BlockRegistry::index_of/name`,
  registry.rs:379/:393) at runtime,
- never persists ids in any palette file.

## 5. Biome layer vs full-block layer (the difference that must be documented in the UI too)

Two different, both-correct color notions:

- **Biome layer (T13):** `Biome::get_map_color()` — the *generator's* map color
  per biome (270 biome records in the golden loader, 57 distinct values). In
  this workspace it has exactly one consumer (the loader golden test,
  loader_golden_dump.rs:1087-1093); the in-game map does **not** use it, and
  `worldgen-cli render-map --mode biome` colors by name hash (a debug scheme,
  main.rs:379-387) — the explorer's biome layer will be the first real
  consumer. It is cheap, exact and needs **no asset pack at all**.
- **Full-block surface (T20/T21):** the `map_image.rs` formula above — what the
  in-game map actually shows, tinted per-position by the biome `Tint`. Needs
  the user's pack for the block/fluid/environment JSON, plus the generator's
  runtime tint column.

The explorer offers both; the doc (and eventually an in-app note) says the
biome layer is the generator's classification view, full-block is the
game-map-faithful view.

## 6. Proof: sample colors, reproducible

Probe: [`tools/block_color_probe.py`](../tools/block_color_probe.py) (stdlib
Python; reads only the user's pack; not part of any build). Command:

```sh
python3 tools/block_color_probe.py ../hytale-server-rust/Assets/extracted
```

Recorded output (2026-11-18, pack dated 2026-10-20):

```text
pack: ../hytale-server-rust/Assets/extracted
block colors (map_image::block_color rules, biome tint #639726):
  Soil_Grass           rgb(99, 151, 38)  fields=[TintUp/Tint, BiomeTintUp=1.00]
  Soil_Sand             rgb(249, 203, 87)  fields=[ParticleColor]
  Rock_Stone            rgb(115, 112, 85)  fields=[ParticleColor]
  Soil_Snow             rgb(221, 240, 244)  fields=[ParticleColor]
  Soil_Leaves           rgb(99, 151, 38)  fields=[TintUp/Tint, BiomeTintUp=1.00]
fluid tint (map_image::blend_fluid tint part):
  Water_Source + Env_Zone0 -> tint rgb(32, 118, 181)  WaterTint=(32, 118, 181) ParticleColor=None  env=Server/Environments/Zone0/Env_Zone0.json
    depth 0: rgb(255, 255, 255)
    depth 1: rgb(143, 186, 218)
    depth 3: rgb(87, 152, 199)
```

Cross-check against the pack JSON (all values verified present on disk):

- `Soil_Grass.json` → `"BiomeTintUp": 100`, `"TintUp": ["#67b62d"]` → with the
  sample Plains tint `#639726` the formula yields exactly `(99,151,38)` ✓
  (`BiomeTintUp: 100` makes `TintUp`/`ParticleColor` irrelevant, per §3).
- `Soil_Sand.json` → `"ParticleColor": "#f9cb57"` → `(249,203,87)` ✓.
- `Rock_Stone.json` → `"ParticleColor": "#737055"` → `(115,112,85)` ✓.
- `Soil_Snow.json` → `"ParticleColor": "#ddf0f4"` → `(221,240,244)` ✓.
- `Env_Zone0.json` → `"WaterTint": "#2076b5"` → water tint `(32,118,181)` ✓;
  depth attenuation matches `blend_fluid`'s code direction (§3.2).

*(Sample hex values above are quoted from the user-supplied pack for
verification under the Shared Source Agreement — see the §2 hard rules.)*

## 7. `hytale-assets` is the wrong dependency for the palette (decision)

The full registry is too heavy for the explorer's palette path:
`AssetRegistry::load_p0` decodes in **1,271 ms** and the resident registry is
**~503 MiB** (BENCHMARKS.md:116-119, :147-149) because it parses every item,
material, bounding-box, entity-stat, projectile and weather store. There is no
public subset loader (verified: `load_p0` is the smallest; tint fields are not
typed anywhere — they live in `record.value.json`).

**Decision:** the palette tool (T20) reads the pack **directly** through the
lightweight public raw API — `AssetPack::open` + `list`/`read_many`
(hytale-assets pack.rs:161-275) + `ParentResolver` (pack.rs:409-418) —
touching only:

- `Server/Item/Items/**` → each file's `"BlockType"` object (3,178 files),
- `Server/Item/Block/Fluids/*.json` (19 files),
- `Server/Environments/**/*.json` (134 files, 43 with `WaterTint`).

That is ~3,300 JSON files, no textures, no models, no decode of PNGs. T20 may
lift the `extract_item_block_types`/parent-resolution logic from
`hytale-assets/src/registry.rs:2882-2911` as a reference. The full
`hytale-assets` crate stays a non-dependency of map-core; `map-app` already
gets the pack path as configuration only.

## 8. Architecture handed to T20

```rust
/// map-core/src/render/palette.rs (T20 implements)
pub trait BlockPalette: Send + Sync {
    /// Static color for a block name; `None` → unknown-block policy.
    /// The palette supplies the *unblended* color and the block's
    /// `biome_tint_pct`; the renderer blends with the runtime biome tint.
    fn block(&self, name: &str) -> Option<BlockColor>;

    /// Fluid tint (WaterTint × ParticleColor resolved); depth attenuation
    /// is applied by the renderer, not the palette.
    fn fluid(&self, name: &str) -> Option<Rgb>;
}

pub struct BlockColor {
    pub rgb: Rgb,               // formula result with white base (m<1 path)
    pub biome_tint_pct: Option<u8>,  // BiomeTintUp; 100 → renderer uses pure tint
}
```

Implementations, in resolution order at runtime:

1. `AssetPackPalette` — built once from the user's pack (§7 path), persisted to
   the user cache dir (via the `directories` crate already in the plan) as:

   ```json
   {
     "schema_version": 1,
     "pack_hash": "sha256:<hash of Assets.zip or extracted tree manifest>",
     "blocks": {
       "Soil_Sand": { "rgb": [249, 203, 87], "biome_tint_pct": null },
       "Soil_Grass": { "rgb": [255, 255, 255], "biome_tint_pct": 100 }
     },
     "fluids": { "Water": { "tint": [32, 118, 181], "source_env": "Env_Zone0" } }
   }
   ```

   Cached by `pack_hash`; regenerated when the pack changes. For the five
   `BiomeTintUp: 100` blocks the stored `rgb` is a placeholder — the renderer
   must use the runtime tint column (§3). Water/fluid entries are resolved per
   *environment*, so the palette stores one tint per environment record and
   the renderer picks by the column's environment id.

2. `FallbackPalette` — committed, hand-picked, approximate (clearly labeled in
   code docs and the in-app note): grass green, sand, stone, snow, the common
   soils/rock variants, water blue, lava orange-red; ~30 entries, original
   values, no Hypixel IP.

3. Unknown-block policy: known name without palette entry → neutral grey from
   `drawType`/`opacity` (solid ≈ `#8a8a8a`, transparent ≈ lighter) + a
   deterministic hue from `java_string_hash_code(name) & 0x3F` so distinct
   modded blocks are visually distinct; no metadata at all → magenta
   `#FF00FF`.

`GeneratorIdentity::palette_version` (from T02's
`docs/seed-semantics.md` §6) increments when the fallback palette's committed
data changes, not when the user's pack changes (that is covered by `pack_hash`).

## 9. What the README must say (done, see `README.md`)

A short section: the explorer does not distribute Hytale assets; the user must
supply their own pack (the same one the shared-source program granted them),
point the app at it, and the palette is generated locally; nothing derived is
committed to this repository; the fallback palette is approximate original
color data.

## 10. Follow-ups for other tasks

- **T13:** biome layer uses `get_map_color` directly; no pack required.
- **T17:** fluid depth shading uses §3.2's `1/(depth+1)` rule with the palette's
  water tint.
- **T20:** implements `AssetPackPalette` + tool from §7/§8; adds the three
  legacy hex prefix forms from §3 to its parser tests.
- **T21/T23:** the "approximate colors" in-app note when the fallback palette
  is active.
- **T03's own acceptance:** decision table complete (§1), fields+proof (§3/§6),
  licensing explicit (§2), README section exists (`README.md`), probe committed
  as a tools script (`tools/block_color_probe.py`).

## 11. Regenerating the palette (T20, done)

The palette tool is `map-cli palette`. It reads the user's own pack —
extracted tree **or** `Assets.zip`, through `hytale-assets`' raw
`AssetPack` API (§7) — resolves `BlockType` parent chains, and writes a
schema-1, name-keyed JSON file:

```sh
cargo run -p map-cli -- palette \
  --input ../hytale-server-rust/Assets/extracted \
  --registries ../hytale-server-rust/testdata/golden/registries.json \
  --out target/palette.json
```

- **Where the file lives:** wherever `--out` says; the intended home is the
  *user cache directory* (never this repository). `.gitignore` covers
  `*.palette.json` and `/target`, and §2's rule 2 stands: a generated
  palette is never committed, whole or partial — it is derived Licensed
  Asset data.
- **Why it may not be committed:** §2 (Shared Source Agreement §3.2/§6.1).
  The committed `assets/palette/fallback.json` is allowed *because* it is
  hand-picked original approximations (§7.6) — if it ever gains
  pack-extracted values, delete it instead.
- **When tiles change color meaning:** committed fallback data changes bump
  `map_core::palette::PALETTE_VERSION` (carried in `GeneratorIdentity`, so
  every tile keyed under the old version invalidates). A pack change does
  *not* bump anything — the palette file's `pack_hash` records that
  provenance, and the file is regenerated by re-running the command.
- **Determinism:** same pack + registries → byte-identical output (sorted
  BTreeMap entries, stable key order), an acceptance-tested property.
