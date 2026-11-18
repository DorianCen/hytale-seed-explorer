# Seed semantics and generator identity (T02)

**Task:** [T02](../tasks/T02-seed-semantics.md) — status: done.
**Question this document answers, once and for all:** how does a seed the user
types become the integer the generator uses, and what identity do caches and
exports need?

**Evidence sources** (every claim below carries provenance):

- **Java reference server** (the authority on Hytale behavior):
  `.hytale-source/hytale-shared-source/` in the worldgen workspace. Paths below
  abbreviated `$SRC` =
  `/home/chome/Projects/hytale-server-rust/.hytale-source/hytale-shared-source`.
- **Rust port** `/home/chome/Projects/hytale-server-rust` (the authority on what
  the explorer actually calls).
- Golden fixtures under `testdata/golden/`.

**Headline verdict — the plan's [verify] markers are resolved:**

1. **Numeric seeds only.** There is **no string-seed path** in the Hytale server:
   no command, no CLI flag, no config field accepts or hashes a user-typed seed
   string. A world seed enters the game exclusively as a JSON number in the
   world's `config.json` (`"Seed"`). The explorer must **reject** string seeds
   with a clear error, not invent a hash.
2. **The world seed is an `i64` on disk, but world generation sees only its low
   32 bits.** Every Java worldgen entry point truncates `(int) getSeed()`; the
   Rust port does the identical `seed as i32`. Seeds `s` and `s + 2³²` produce
   **identical terrain** but different weather/block-spawner output.
3. **The generator version tag exists now:** `worldgen_hytale::GENERATOR_VERSION`
   (`u32`, currently `1`) with the bump policy below.

---

## 1. Numeric input rules

| Rule | Behavior | Provenance |
|---|---|---|
| Stored type | `long` (`i64`), field `seed`, JSON key `"Seed"` | Java: `$SRC/HytaleServer/CoreServer/src/main/java/com/hypixel/hytale/server/core/universe/world/WorldConfig.java:400-402` (`private long seed = System.currentTimeMillis();`), codec block `:82-90` (`new KeyedCodec<>("Seed", Codec.LONG)`); schema `$SRC/HytaleAssets/Schema/InstanceConfig.json:128-135` (`"type": "integer"`). Rust: `crates/hytale-world/src/world_config.rs:401-403` (`#[serde(rename = "Seed", default)] pub seed: i64`) |
| Parse of the JSON value | `Long.parseLong` on the raw number | Java: `LongCodec.decodeJson` → `RawJsonReader.readLongValue` (`$SRC/HytaleServer/Codec/src/main/java/com/hypixel/hytale/codec/codecs/simple/LongCodec.java:35-37`, `codec/util/RawJsonReader.java:819-830`). A JSON *string* value fails with `NumberFormatException` |
| Range / sign validation | **None.** No `addValidator`, no zero/negative/`Long.MIN_VALUE` checks anywhere in the seed path | Java: the `KeyedCodec` block at `WorldConfig.java:82-90` carries only `.documentation` + `.metadata(NoDefaultValue)`; workspace-wide search found no `setSeed` callers (`:562` is the only match) and no seed comparisons. Rust: plain serde round-trip (`world_config.rs:645/:654`), no `validate` |
| Default when `"Seed"` is absent | `System.currentTimeMillis()` at `WorldConfig` construction ("randomly generated") | Java: `WorldConfig.java:402`, load path `:870-876`. Rust: `serde default` → `0` (a port deviation, see §5) |
| Existing config wins | When a world already has a `config.json`, a creation-time seed argument is ignored | Rust: `crates/hytale-world/src/world.rs:970-986` ("`seed` … only used when the config does not exist yet"). Java has no creation-time seed argument at all (§2) |
| Generator-visible value | Low 32 bits of the i64: Java `(int)` narrowing (JLS 5.1.3 — low bits, sign-preserved), Rust `as i32` (identical) | Java call-site census in §3; Rust: `crates/hytale-server/src/world_thread.rs:3145/:6209/:6223` |

**Truncation consequences for the explorer:**

- The *canonical generator value* is an **`i32`**: `generator_seed = world_seed as i32` (low 32 bits). Accepted input range: full signed `i64`; anything beyond `i32` must be **documented, not silently accepted as-is**: the UI shows the effective `i32` next to the input ("`4294967297` → generates as `1`").
- Two different i64 seeds with equal low 32 bits generate **identical terrain, biomes, caves, prefabs and spawn points** — the explorer cannot distinguish them and must not pretend to (see §4 for what *does* differ).
- Edge values `0`, `1`, `-1`, `i32::MIN`, `i32::MAX` are all legal; no seed is special-cased anywhere in Java or Rust.

## 2. String input rules

**String seeds are not a Hytale server feature.** Evidence, exhaustively:

- The only typeable seed argument in the entire Java tree is the optional `seed`
  of `/worldgenbenchmark`, typed `int` (`ArgTypes.INTEGER` → `Integer.parseInt`;
  `$SRC/.../command/commands/world/worldgen/WorldGenBenchmarkCommand.java:51-52`,
  `.../command/system/arguments/types/ArgTypes.java:103-117`). `ArgTypes` has **no
  `LONG` type and no string-seed type**.
- No `createworld` command or `--seed` CLI flag exists in the Java server
  (world command directory enumerated; `WorldAddCommand.java:26-36` takes name/gen/storage
  only; no seed in `scripts/run.sh`, `Options.java` or `HytaleServer.java`).
- `WorldConfig.setSeed(long)` has **zero callers** in the tree — the seed is
  write-once via `config.json`.
- Rust agrees: every CLI seed arg is a clap-typed integer
  (`hytale-server/src/main.rs:83-85` i32, `:118-120/:186-188/:224-226/:275-277`
  i64; `worldgen-cli/src/main.rs:149-151/:167-169/:295-297/:418-420` i32); the only
  seed-related `FromStr` impls in the workspace parse UUIDs and log levels, not
  seeds.

**String hashing that exists but is NOT user input** — do not confuse these with
seed parsing (the T01 audit already flagged one of them):

| Mechanism | What it hashes | Provenance |
|---|---|---|
| `SeedOverride` (HytaleGenerator config) | A *server config string* that replaces the world seed via `seedOverride.hashCode()` (Java `String.hashCode`) | `$SRC/HytaleServer/builtin/HytaleGenerator/.../plugin/Handle.java:34-44`, config field `HytaleGenerator.java:227-232` |
| `SeedBox` (HytaleGenerator noise derivation) | `Integer.toString(intSeed) + childKey` → `String.hashCode` | `$SRC/.../hytalegenerator/rng/SeedBox.java:14-31`, used `HytaleGenerator.java:170/:288` |
| `SeedString` / `"ChunkGenerator"` label | Constant loader *labels* (`".MaskProvider"`, `".Zones"`, …) consumed at asset load time | `$SRC/HytaleServer/ProceduralLibrary/.../json/SeedString.java:12-30` (`hash = seed.hashCode() * 114512143`), `.../worldgen/HytaleWorldGenProvider.java:89-98`; Rust twin `crates/procedural/src/json/seed_string.rs:21/:31-60`, label `crates/worldgen-hytale/src/world_gen.rs:596` |
| `"Seed"` strings inside worldgen asset JSON | Noise-generator labels (e.g. `testdata/Default/Mask.json:5 "Seed": "RANDOMIZER"`) | `$SRC/.../modifier/content/common/NoiseMask.java:24`; schema `HytaleAssets/Schema/common.json:37019-37030` |

**Client-side:** this checkout contains **no client world-creation UI** (only
C# interop bindings under `$SRC/HytaleClient/`, no seed matches), so whether the
Hytale *client* ever accepted a string seed at world creation **cannot be verified
from the available source**. Until a live game or client source proves otherwise,
the explorer's parser rejects non-numeric input with a clear error. **No
unverified `String.hashCode` path is implemented as if it were Hytale behavior.**

## 3. The i64 → i32 narrowing (complete call-site census)

Every Java consumer of `WorldConfig.getSeed()`:

**Truncated to `int` (terrain and everything the explorer shows):**
`ChunkGrid.java:850/:1279/:1595` (all three chunk-generation entry points — the
`:1278` comment is literally *"TODO: Should the WorldConfig be changed to store an
int seed instead of long?"*), `WorldConfig.java:578` (default spawn provider),
`WorldGenBenchmarkCommand.java:82`, `BiomeDataSystem.java:48` (biome HUD),
`GeneratorChunkWorldMap.java:38` (world-map POIs), `BiomeLocationV1.java:70`,
`AbstractLocateSubcommand.java:152`, `LocatePrefabCommand.java:51` (`/locate`),
`NPCMemory.java:361`. The generator interface itself is int-typed:
`IWorldGen.generate(int seed, …)` (`$SRC/.../universe/world/worldgen/IWorldGen.java:16-28`),
legacy `ChunkGenerator.generate(int seed, …)` (`$SRC/HytaleServer/WorldGenerator/.../worldgen/chunk/ChunkGenerator.java:349-350`).

**Full 64 bits used:**
`WeatherSystem.java:127-141` (weather selection: `HashUtil.hash(worldSeed, …)` with a
genuine 64-bit mix, `$SRC/HytaleServer/Math/.../HashUtil.java:25-30`) and
`BlockSpawnerPlugin.java:177-191` (block-spawner randomization via
`HashUtil.random/rehash(x, y, z, seed + 0x9ab8cd9f)`).

**Explicitly NOT seed-derived (Java):** world UUID (`UUID.randomUUID()`,
`WorldConfig.java:400` — instances copy a world by re-randomizing the UUID and
keeping the seed, `InstanceEditCopyCommand.java:64-71`); entity client seeds
(`World.java:193/:1366`); NPC spawning (no `getSeed` under `NPC/`).

The Rust port mirrors the truncation exactly (`world_thread.rs:3145/:6209/:6223`)
and adds Rust-CoreServer-specific full-i64 consumers the explorer does not need
(drops `:3146`, spawn-flock RNG `:3160`, weather `:3524`, block simulation
`:5858`, boot report `:5994`). One mixed case worth knowing: command-driven prefab
paste generates with the truncated i32 then widens it back (`i64::from(seed)`,
`world_thread.rs:6399` → `hytale-world/src/prefab.rs:41-50`) — so even the paste
RNG effectively sees the 32-bit seed.

**Spawn-point parity note:** both implementations shuffle spawn points with the
same widened-multiply: Java `random.setSeed(seed * 0x5912_2134L)`
(`ChunkGenerator.java:225-228`), Rust `FastRandom::new((seed as
i64).wrapping_mul(0x5912_2134))` (`worldgen-hytale/src/chunk/chunk_generator.rs:784-787`).

## 4. Canonical seed representation for the explorer

Proposed shape (T07 owns the final type; T02 fixes the semantics):

```rust
pub struct Seed {
    /// The exact text the user typed, kept verbatim for the UI/history/share
    /// string. Must parse as a (signed) decimal i64; anything else is an error.
    input: String,
    /// The persisted Hytale world seed (i64), when a real-world seed is known.
    world: i64,
    /// The generator-visible value: `world as i32` (low 32 bits). This is the
    /// ONLY value terrain, biomes, features, caves and spawn depend on.
    generator: i32,
}
```

Rules (these are the acceptance criteria for T07's parser):

1. `input` must be a decimal integer in `i64` range (`Java Long.parseLong`
   parity). Whitespace-only, empty, non-numeric → **`SeedError::NotANumber`**.
   The error message names the supported format; it never hashes the input.
2. `generator = world as i32`. When `world` is outside `i32`, the UI *displays*
   both values ("generates as N") — documented truncation, never silent.
3. Same text ⇒ same triple, always (pure function of `input`).
4. Display / share / export:
   - **UI:** the number the user typed (or the world's stored i64 when opened
     from a save); the effective i32 in a tooltip/detail when it differs.
   - **Share string:** carries `input` verbatim (round-trips to the same
     `Seed`); the generator i32 is derived, never embedded twice.
   - **PNG/export metadata:** `{ seed_input: input, seed_world: i64,
     generator_version: u32 }` (below) so an export is reproducible and
     attributable.
5. The explorer never displays a "UUID" derived from the seed: Java world UUIDs
   are random (`WorldConfig.java:400`). The Rust repo's `WorldUuid::derive`
   (`world_thread.rs:5739-5745`) is a Rust CoreServer convenience, **not** Hytale
   parity — do not expose it as game behavior.

## 5. Verified deviations of the Rust repo (documented, not to be "fixed" here)

| Deviation | Java | Rust | Impact on explorer |
|---|---|---|---|
| Default seed when `"Seed"` absent | `System.currentTimeMillis()` (`WorldConfig.java:402`) | `0` (`serde default`, `world_config.rs:401-403`) | Opening a save without `"Seed"` shows 0 in the Rust stack; a real game world would have a time-based seed. Cosmetic only |
| World UUID derivation | random | derived from seed + name (`world_config.rs:578-584`, `world_thread.rs:5739`) | none — explorer must not use UUID for parity claims (§4.5) |
| Creation-time seed args | none (config.json only) | CLI `--seed` i64, ignored when config exists (`world.rs:970-986`) | none — explorer is read-only |
| Golden coverage of extreme seeds | — | worldgen goldens use `0, 1, 42, 1234567` (all comfortably i32); only `hytale-math` goldens exercise i64 extremes (`testdata/golden/mathutil.json`: `-9223372036854775808`, `9223372036854775807`) | the explorer's own tests must cover i64-outside-i32 inputs (T07) since worldgen goldens don't |

## 6. `GENERATOR_VERSION` (done in this task)

Added at the public root of `worldgen-hytale`
(`crates/worldgen-hytale/src/lib.rs`, commit `08dd461` in the worldgen repo):

```rust
pub const GENERATOR_VERSION: u32 = 1;
```

- **What it covers:** the generator's *output semantics* — generation logic,
  world asset/config changes that alter output, registry snapshot semantics
  (block/fluid ids, opacity) when they affect output, and downstream palette
  semantics. See the full policy comment in the source.
- **When not to bump:** refactors/perf work that keep output byte-identical
  (the golden chunk tests are the guard). A bump is a cache-invalidation
  action recorded in the commit message.
- **Naming note:** `hytale-server::generation::GENERATOR_VERSION`
  (`generation.rs:28-30`) is a *different, inert* `&str "0.0.0"` passed to
  `WorldGenConfig` as provider metadata; it never reaches a cache or export.
  The new `u32` at the `worldgen_hytale` root is the one tile keys consume.
- **Locked by test:** `worldgen_hytale::tests::generator_version_is_tagged`
  fails on accidental bumps (`cargo test -p worldgen-hytale --lib`).

**Identity composition for tile keys** (recommendation; T14 owns the key, T31 the
share string):

```rust
pub struct GeneratorIdentity {
    pub generator_version: u32,   // worldgen_hytale::GENERATOR_VERSION
    pub world_assets: String,     // world folder name + provider version (e.g. "Default/0.0.0")
    pub palette_version: u32,     // T20's palette data version, 0 until T03/T20 decide
}
```

Tile keys hash the identity + seed i32; a bump invalidates every tile, summary
and export keyed to the previous identity. The seed **i32** (not the input text)
participates in tile keys because terrain cannot distinguish more.

## 7. Test plan

| Test | Status | Owner |
|---|---|---|
| `GENERATOR_VERSION` present, typed, locked at 1 | **done** — `worldgen_hytale::tests::generator_version_is_tagged` | T02 (this task) |
| `cargo test -p worldgen-hytale` full suite green after the constant | **done** — 691 lib + integration suites pass; `cargo fmt --check`, `clippy -D warnings` clean | T02 |
| `dump-chunk --seed 42 --chunk 0,0` runs (seed flows through the CLI as i32) | **done** — JSON echoes `"seed": 42` | T02 |
| Parser: `0`, `1`, `-1`, `i32::MIN`, `i32::MAX` map to themselves | pending type | T07 |
| Parser: i64 outside i32 → documented truncation (e.g. `4294967297` → 1) | pending type | T07 |
| Parser: empty/whitespace/non-numeric → typed error, never a hash | pending type | T07 |
| Version bump invalidates a tile key | pending key | T14 |

## 8. Resolved plan markers (report to the main agent — done by this task)

- Plan §1 *"Seed input (number or string, hashed the way Hytale does it)
  **[verify]**"* → **numeric only; no string hashing exists.** The plan text now
  says so (see `hytale-seed-map-plan.md` §1).
- Plan §10.4 *"Confirm how Hytale converts seed input into the internal value,
  including string seeds **[verify]**"* → resolved: `config.json` `"Seed"` i64 →
  `(int)` truncation at every worldgen boundary; string seeds not a server
  feature; client-side unverifiable from the available source (§2).

## 9. Open items for later tasks

- **T03:** asset/palette versioning feeds `GeneratorIdentity::palette_version`.
- **T05:** a fidelity fixture should include one seed pair `(s, s + 2³²)` to
  lock the truncation semantics against the Java tool (the worldgen goldens
  never exercise a seed outside i32 — §5).
- **T07:** implement `Seed` + parser per §4, with the §7 test rows.
- **T14/T31:** tile-key identity and share-string format per §6/§4.
