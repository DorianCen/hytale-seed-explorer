# T02 — Seed semantics and generator identity

**Status:** proposed
**Milestone:** 0 — API audit
**Depends on:** —
**Blocks:** T06
**Plan refs:** [§1](../hytale-seed-map-plan.md#1-scope), [§5.1](../hytale-seed-map-plan.md#51-tiles-and-zoom), [§10.4](../hytale-seed-map-plan.md#10-risks-and-open-questions), [§10.5](../hytale-seed-map-plan.md#10-risks-and-open-questions)
**Agent fit:** medium

## Goal

Pin down exactly how a seed typed by the user becomes the integer seed the
generator uses, and give every cache/export a generator identity that changes
when generation or palette data changes. Both are prerequisites for tile keys
and share strings.

## Context

- The world save stores `WorldConfig.seed: i64`
  (`crates/hytale-world/src/world_config.rs`).
- World generation receives `WorldGenRequest.seed: i32`
  (`crates/worldgen-hytale/src/world_gen.rs`), and the server truncates with
  `save.config().seed as i32` before generating
  (`crates/hytale-server/src/world_thread.rs`).
- `procedural::json::SeedString` hashes *seed labels* such as
  `"ChunkGenerator"` and noise suffixes. That is **not** the world seed input;
  do not confuse the two.
- `WorldGenOptions.seed_label` defaults to `"ChunkGenerator"` and is a constant
  loader label, not the user's seed.
- There is no `GENERATOR_VERSION` in `worldgen-hytale`.

The plan marks string-seed hashing as **[verify]**. Treat this as an
investigation first, implementation second.

## Scope

### In

- Verify Hytale's numeric seed path and string seed path from the Java source,
  the live server CLI, or a real save/config.
- Define the explorer's canonical seed representation and display rules.
- Define `GENERATOR_VERSION` and what it covers.
- Add tests that lock the observed behavior.

### Out

- Multi-criteria seed search (plan §6.4 / T22 design note).
- World save compatibility; the explorer is read-only.
- Re-deriving generator noise seeds.

## Deliverable

1. `docs/seed-semantics.md` with:
   - numeric input rules (range, sign, `i64` vs `i32` truncation);
   - string input rules, if supported, including the exact hash function and
     whether the original string is preserved for display;
   - the canonical `Seed` value and its generator-visible `i32`;
   - how the same seed is shown in the UI, share string and export metadata;
   - provenance (Java class/method, live-server command, or save file) for
     every claim.
2. A `GENERATOR_VERSION: u32` constant at the public root of
   `worldgen-hytale` (for example `crates/worldgen-hytale/src/lib.rs`) and a
   short policy comment: bump when generation logic, world assets, registry
   snapshot or block palette semantics change.
3. Seed parsing tests (in the task that owns the type; T07 if the type lands
   there).

## Work

1. Search the Java source and the live server for seed parsing:
   - `.hytale-source/**` for `seed`, `SeedString`, `WorldConfig`,
     `HytaleWorldGenProvider`, `--seed`, `String.hashCode`.
   - Confirm whether the client/server accepts a non-numeric seed and how it
     is hashed. Do not assume Java `String.hashCode()` without evidence.
   - Confirm where an `i64` is narrowed to the generator's `i32` and whether
     the high bits affect anything else (UUID, weather, spawn).
2. Test the live server if available:
   - create/read a world with a numeric seed and with a string seed;
   - inspect the saved `config.json` seed value;
   - generate the same chunk through `worldgen-cli` with the derived value and
     compare a known column/height/biome.
3. Define `Seed` semantics for the explorer. Proposed shape (T07 owns the
   final type):

   ```rust
   pub struct Seed {
       /// The exact text the user typed, kept for the UI/history/share string.
       input: String,
       /// The persisted Hytale world seed (i64), when one exists.
       world: i64,
       /// The generator-visible seed; currently `world as i32`.
       generator: i32,
   }
   ```

   If string seeds are not a real Hytale feature, the parser should reject them
   with a clear error instead of inventing a hash. If they are supported, the
   parser must reproduce the observed hash exactly and test known pairs.
4. Add `GENERATOR_VERSION` and decide whether it also covers the world asset
   pack version. Recommended: a single `GeneratorIdentity` in map-core later:
   `{ generator_version: u32, world_assets: String, palette_version: u32 }`;
   the tile key uses a stable hash of the identity. T14 owns the key.
5. Add tests for:
   - `0`, `1`, `-1`, `i32::MIN`, `i32::MAX`;
   - values outside `i32` (truncation documented, not silently wrapped);
   - empty/whitespace input;
   - string seeds, either accepted with a known pair or rejected;
   - version bump invalidates a tile key (test can live in T14).
6. Report the finding to the main agent so the plan's **[verify]** marker can
   be resolved.

## Acceptance

- `docs/seed-semantics.md` answers every bullet in *Deliverable* with evidence.
- The constant is public and documented; `cargo test -p worldgen-hytale`
  passes.
- A seed typed into the future UI has exactly one canonical generator value;
  the same text always maps to the same value.
- No unverified string hash is implemented as if it were Hytale behavior.

## Tests

```sh
cd /home/chome/Projects/hytale-server-rust
cargo test -p worldgen-hytale
cargo run -p worldgen-cli -- dump-chunk --seed <seed> --chunk 0,0
```

Add a small fixutre comparing the generator's output for the derived seed with
the live server's saved seed, if the live server is available.

## Risks / notes

- If only `i32` is meaningful, do not expose a fake `i64` range in the UI. Show
  the accepted range and reject the rest.
- If string seeds are a launcher/client feature rather than a world format
  feature, document that they are out of scope instead of guessing.
- Bumping `GENERATOR_VERSION` is a cache-invalidation action, not a cosmetic
  version bump. Record the reason in the commit message.
