# T08 — Generator worker pool and cheap queries

**Status:** proposed
**Milestone:** 1 — Scaffold
**Depends on:** T06, T07
**Blocks:** T09, T11, T12, T13, T22, T25
**Plan refs:** [§2](../hytale-seed-map-plan.md#2-what-the-explorer-needs-from-your-worldgen), [§3](../hytale-seed-map-plan.md#3-architecture), [§4](../hytale-seed-map-plan.md#4-tech-choices)
**Agent fit:** high

## Goal

Implement `WorldgenSource`, the production `WorldSource` adapter, with a pool
of dedicated generator threads. This is where the explorer gets cheap biome
and height queries without generating chunks, and where the thread-safety
constraint of the `Rc`-based generator is handled safely.

## Context

`ChunkGenerator` is `Rc`-based and is not `Send` or `Sync`. It cannot be stored
in a `Mutex` and shared. `WorldGenService` already solves this by giving every
worker thread its own generator and communicating through channels. The
explorer needs the same shape, but with column-level queries rather than only
whole-chunk requests.

`ChunkGeneratorJsonLoader::new(seed_label, WorldGenConfig)` loads a generator;
`WorldGenOptions` carries `assets_root` and `registries_path`. Loading is
I/O-heavy, so do it once per worker and keep the worker alive.

The loader's `seed_label` is the constant `"ChunkGenerator"`; the world seed is
passed per query (`get_height(seed, x, z)`,
`get_zone_biome_result_at(seed, x, z)`).

## Scope

### In

- `map-core/src/source/worldgen.rs` (or `source/worldgen/` with modules):
  - `GeneratorPool` with N worker threads;
  - command/reply protocol;
  - `WorldgenSource` implementing `WorldSource`;
  - batch `biome_region` and `height_region` queries;
  - catalog extraction (biome names/map colors, block/fluid names) from the
    loaded generator/registries;
  - graceful shutdown and error propagation.
- `map-core/src/source/fake.rs` remains the test source.
- A `WorldgenSourceConfig` with world path/name/version, assets root,
  registries path, worker count and optional generator seed policy.
- Tests for determinism, parallel queries and worker failure.

### Out

- Chunk generation, surface blocks, slices and feature placement (T09/T22).
- Tile scheduling (T15) and UI integration (T12).
- Changing the worldgen service.

## Design

```text
WorldgenSource
  ├─ seed: Seed
  ├─ catalogs: Arc<...>
  ├─ identity: GeneratorIdentity
  └─ pool: GeneratorPool
        ├─ worker 0: thread -> Rc<ChunkGenerator> + ChunkGeneratorResource
        ├─ worker 1: ...
        └─ command channel (crossbeam) + reply channel (std::sync::mpsc)
```

Commands (cheap-query phase):

```rust
enum GeneratorCommand {
    BiomeRegion { rect: Rect, step: u32, cancel: CancelToken,
                  reply: Sender<Result<BiomeGrid, SourceError>> },
    HeightRegion { rect: Rect, step: u32, cancel: CancelToken,
                   reply: Sender<Result<HeightGrid, SourceError>> },
    SpawnPoints { reply: Sender<Result<Vec<Transform>, SourceError>> },
    Shutdown,
}
```

`BiomeRegion` needs the biome id and map color per sample. `HeightRegion` is a
compact `Vec<i16>`/`Vec<i32>` grid. Both loop over the rect and call the
generator directly; no per-sample channel round-trip.

Dispatch policy:

- For a region request, split the rect into horizontal bands or row chunks and
  send one command per worker, then merge; or round-robin whole-region requests
  and let the caller parallelize tiles. Document the choice.
- For single `biome_at`/`height_at`, round-robin one request. These are for
  hover/search, not for tiles.
- Preserve the input rect/step in the result so the renderer can index it.

## Work

1. Implement `WorldgenSourceConfig` and a `load`/`new` function. Validate the
   world path and asset/registry paths, and return a readable `SourceError`.
2. Spawn workers. Each worker:
   - loads its own `ChunkGenerator` with `ChunkGeneratorJsonLoader`;
   - builds the catalogs once;
   - reports readiness or an error to the constructor;
   - loops over commands until shutdown.
   If one worker fails to load, shut down the pool and return the first error.
3. Implement `WorldSource`:
   - `identity()` from T02/T07;
   - `seed()` from the config;
   - `biome_at`/`height_at` via a one-command request;
   - `biome_region`/`height_region` via batch commands;
   - `surface_*`/`slice_*` return `SourceError::Unsupported` until T09.
4. Build catalogs from the loaded generator:
   - biome id/name/map color: walk `ZonePatternProvider::get_zones()` and each
     `BiomePatternGenerator`/biome list; deduplicate by id; custom biomes too;
   - block/fluid names: `worldgen_model::registry::Registries` attached to the
     generator (`ChunkGenerator::get_registries`), or the configured
     `registries_path`.
5. Implement cancellation: check the token every N rows (for example every 16
   rows) and return `SourceError::Cancelled`. Do not hold a worker hostage on a
   cancelled region.
6. Implement shutdown on `Drop`: send `Shutdown`, join workers, ignore a
   poisoned reply. Make it idempotent.
7. Add tests:
   - a fixed seed/rect returns the same biome ids and heights as a direct
     `ChunkGenerator` (test can live in map-core and load `testdata/Default`);
   - negative coordinates and a 1×1 region;
   - parallel requests from 4 threads produce identical results to serial;
   - a bad world path returns `SourceError::Load`;
   - cancellation of a large region returns `Cancelled`;
   - `Drop` does not hang.
8. Add a benchmark probe for 65,536 biome samples and 65,536 height samples;
   hand the numbers to T04/T33.

## Acceptance

```sh
cargo test -p map-core source::
cargo test -p map-core --test worldgen_source
cargo clippy -p map-core --all-targets -- -D warnings
```

- `WorldgenSource` implements `WorldSource` and is `Send + Sync`.
- No `Rc` or `ChunkGenerator` crosses a thread boundary; use a compile-time
  assertion or a comment explaining the worker-thread ownership.
- A 256×256 biome region completes without per-sample channel traffic.
- Loading a missing world fails cleanly; the app can display the error.
- The worker pool shuts down without leaking threads.

## Tests

- `map-core/tests/worldgen_source.rs` with a test helper that locates
  `../hytale-server-rust/testdata/Default` and `../hytale-server-rust/testdata/golden/registries.json`.
  Skip with a clear message if the generator checkout is absent.
- Direct-generator comparison for a small grid (for example 17×11 at step 3).
- A stress test that issues 8 concurrent region requests and compares the
  results.

## Risks / notes

- Loading N generators is expensive. Do not eagerly load 12 workers on app
  start; start with 1–2 and scale lazily, or show a loading state. The plan's
  first-tile target is measured after the source is ready.
- The generator's per-worker caches are not shared. Round-robin single queries
  can lose cache locality; batch region requests keep a whole region on one
  worker. Prefer batches for tiles.
- If biome ids are not globally unique, assign catalog ids at load time and
  map back inside the worker. Do not expose a non-unique id to the renderer.
