# T11 — Headless 512×512 PNG render

**Status:** done (deliverable: `map-core/src/render/` — `image.rs`
(`RgbaImage` + deterministic `png`-crate encoder) and `mod.rs`
(`Layer` enum, minimal `LayerRenderer` trait, `BiomeRenderer`/
`render_biome`, magenta unknown-biome fallback) — plus the
`map-cli render` command (seed via T02 `Seed`, negative centers,
`--world/--assets/--registries/--workers` overrides, stderr progress)
and the golden suite `tests/render_golden.rs` (3: seed-0 origin +
negative-center SHA-256 goldens, one-pixel sensitivity) and
`map-cli/tests/cli.rs` (4: help/seed/layer/center argument errors);
512×512 acceptance hash + cold/release times appended to
[`docs/benchmarks.md`](../docs/benchmarks.md))
**Milestone:** 1 — Scaffold
**Depends on:** T05, T08, T09, T10
**Blocks:** T13
**Plan refs:** [§3](../hytale-seed-map-plan.md#3-architecture), [§8](../hytale-seed-map-plan.md#8-milestones), [§9](../hytale-seed-map-plan.md#9-performance-correctness-and-testing)
**Agent fit:** high

## Goal

Prove the whole headless pipeline end to end: load the generator, sample a
512×512-block region at step 2, color it with the biome map color, and write a
PNG. This is milestone 1's deliverable and the first golden-image gate.

## Context

The plan's milestone 1 is "Workspace, traits, adapter, test rendering one
512×512 region at 2×2 to PNG." The existing `worldgen-cli render-map` can render
height/zone/biome, but it uses a name-hash color, not the generator's
`Biome::get_map_color`. The explorer needs its own renderer in `map-core` and a
`map-cli` entry point.

This task is also the first integration test for:

- `WorldgenSource` worker loading;
- `biome_region` batching;
- the coordinate math;
- deterministic output.

## Scope

### In

- `map-core/src/render/mod.rs` with a minimal `LayerRenderer` shape and a
  `render_biome` function.
- `map-core/src/image.rs` (or `render/image.rs`) with an RGBA buffer type and
  PNG encoding.
- `map-cli render` command:
  ```text
  map-cli render --seed 0 --center 0,0 --size 512 --step 2 --layer biome --out target/region.png
  ```
- A deterministic golden-image test.
- A progress/status line on stderr; no GUI.

### Out

- The egui viewer (T12/T13).
- Height/relief/surface layers (T17/T20/T21).
- Tiles/caching (T14/T15); this task renders one region directly.

## Render path

```text
CLI args -> WorldgenSourceConfig -> WorldgenSource
         -> biome_region(rect, step)
         -> BiomeRenderer::render(grid) -> RgbaImage
         -> PNG encoder
```

Biome color:

- use `Biome::get_map_color()` from the generator (T08 catalog);
- unpack the generator's packed color with the documented channel order;
- unknown biome: magenta (`255, 0, 255, 255`) so missing data is obvious;
- no shading in this task; keep the first golden simple.

## Work

1. Add an `RgbaImage`/`ImageBuffer` type in map-core. Prefer `image::RgbaImage`
   if the dependency is already in the workspace, or a small `Vec<u8>` wrapper
   with `width`, `height`, `pixels`. Keep PNG encoding behind the CLI or an
   `export` module so core remains image-library-light if desired.
2. Implement `render_biome(grid, catalog) -> RgbaImage`:
   - iterate row-major;
   - map `BiomeId` to `BiomeInfo`; color; alpha 255;
   - assert dimensions match.
3. Implement the CLI:
   - parse seed via T02's `Seed` (reject invalid with a clear error);
   - resolve the default world path and asset/registry paths the same way
     `worldgen-cli` does (`testdata/Default`, `Assets/extracted`, golden
     registries), with flags to override;
   - compute the rect from center and size: `x0 = center.x - size/2 * step`,
     `z0 = center.z - size/2 * step`, width/height = `size`;
   - call `biome_region`;
   - write the PNG, print the path, dimensions, step and elapsed time;
   - support negative centers.
4. Add a deterministic test:
   - fixed seed 0, center `(0,0)`, size 64×64, step 2 (small enough to commit);
   - compare a SHA-256 of the PNG bytes or an `insta` snapshot;
   - include a second case with a negative center.
   Golden files should be small (a few KB), not a 512×512 dump.
5. Add a manual acceptance run at 512×512 step 2 and record the hash/time in
   `docs/benchmarks.md` (T04).
6. Add a `--layer` enum with only `biome` for now; return
   `SourceError::Unsupported` or a CLI error for others. T17/T20 extend it.
7. Ensure no GUI dependency is pulled into `map-cli` or `map-core`.

## Acceptance

```sh
cargo run -p map-cli -- render --seed 0 --center 0,0 --size 512 --step 2 \
  --layer biome --out target/region-512.png
cargo test -p map-core --test render_golden
cargo test -p map-cli
cargo clippy -p map-core -p map-cli --all-targets -- -D warnings
```

- The command writes a 512×512 PNG with the correct block extent (1024×1024
  blocks at step 2).
- The same command twice produces identical bytes.
- Negative centers work.
- The image is not uniformly one color; a fixture check confirms at least two
  biome colors.
- The golden test fails on a one-pixel change.

## Tests

- `map-core/tests/render_golden.rs`: 64×64 step 2 golden for seed 0 and a
  negative-center case.
- A unit test for `RgbaImage` indexing and dimensions.
- A CLI test with `assert_cmd` or a plain `std::process::Command` that runs
  `--help` and an invalid seed (no asset-heavy render in CI unless the
  generator fixtures are present).

## Risks / notes

- The generator's `get_map_color` channel order must be confirmed against the
  `map_image` packer; a swapped red/blue channel is a silent visual bug.
- If loading the generator is slow, the CLI should print "loading world" before
  the bake so the user knows it is not hung.
- Do not make the golden depend on absolute paths or timestamps in the PNG.
  Use a PNG encoder configuration with no time metadata.
