# T33 — Performance, cache tuning and robustness

**Status:** proposed
**Milestone:** 10 — Polish
**Depends on:** T04, T21, T24, T28, T32
**Blocks:** T34
**Plan refs:** [§9](../hytale-seed-map-plan.md#9-performance-correctness-and-testing), [§10](../hytale-seed-map-plan.md#10-risks-and-open-questions), [§8](../hytale-seed-map-plan.md#8-milestones)
**Agent fit:** high

## Goal

Meet the plan's performance targets, bound memory, and make failures
recoverable. This is the "polish" milestone: profiling, cache tuning, error
handling and a soak test, not a new feature.

## Context

Plan targets:

- first visible tile within ~100 ms after opening (source ready);
- any viewport fully filled with at least coarse data within ~1 s;
- UI thread never blocks on generation (frame time under 16 ms);
- feature search first results within ~500 ms;
- memory capped (for example 512 MB of tile cache) with eviction.

Existing instrumentation:

- `SchedulerStats` (T15): queued/in-flight/completed/cancelled/render time;
- `TileCache::stats` (T14);
- `ChunkSummary` cache stats (T09);
- `WorldGenStats` from worldgen (T08);
- search stats (T24/T25).

## Scope

### In

- A debug/performance panel or status line showing:
  - FPS/frame time, queued/in-flight tiles, cache hits/misses/bytes, chunk
    summaries, search timing, worker counts.
- Profiling workflow:
  - `cargo flamegraph` or `puffin` (optional feature) for a repeatable scene;
  - document the exact scene (seed, center, zoom, layers, actions) and the
    top hotspots.
- Tuning:
  - tile cache budget vs summary cache budget;
  - scheduler worker count vs generator worker count;
  - batch size for biome/height/surface queries;
  - placeholder strategy/refinement thresholds.
- Robustness:
  - generator load failure, out-of-area, cancelled jobs, worker panic;
  - source reload on seed change while jobs are in flight;
  - app shutdown without hangs;
  - cache corruption/eviction races.
- A soak/stress test:
  - pan across dozens of tiles, zoom in/out, switch layers, run searches,
    add waypoints, export; monitor RSS/frame time;
  - a 10–15 minute run is enough for v1; 24h is out of scope.
- A regression table appended to `docs/benchmarks.md` with before/after.
- Fixes for the concrete bottlenecks found, not speculative rewrites.

### Out

- A `wgpu` renderer unless profiling proves egui textures are the bottleneck.
- Multi-threaded search beyond what T24/T25 already do.
- WebAssembly/threading (plan §10.8).
- New features.

## Work

1. Build a reproducible profiling scene and script:
   - start the app with a fixture seed;
   - load, wait for ready, pan a fixed path, zoom to 1×1 and back, run a
     feature search and a biome search, toggle layers.
   - Record the scene in `docs/benchmarks.md`.
2. Add metrics collection:
   - frame time histogram (egui's `ctx.input(|i| i.stable_dt)` or a rolling
     average);
   - scheduler counters and cache stats;
   - source query timings (biome/height/surface/slice/feature);
   - display in a collapsible "Performance" panel and log a summary on demand.
3. Profile:
   - CPU: `cargo flamegraph -p map-app` or `perf` if available;
   - identify UI-thread work (texture uploads, event draining, overlay
     drawing) vs worker work;
   - check for accidental blocking calls or per-frame allocations.
4. Tune:
   - measure cache hit rate after panning; adjust tile/summary budgets so the
     viewport plus margin fits;
   - measure full-block tile latency vs worker count; pick a default that keeps
     the UI responsive;
   - measure coarse/refined placeholder behavior; adjust thresholds.
   Record the chosen defaults and the evidence.
5. Robustness tests:
   - a bad world path → error UI, no retry storm;
   - a worker panic → `Failed` event, app continues;
   - source reload during in-flight tiles/search → no stale result applied;
   - closing the app during a full-block tile → clean shutdown (no hang);
   - out-of-area pan → Unknown/empty, no panic.
6. Soak test:
   - run the scene for 10–15 minutes with a memory sampler;
   - assert RSS does not grow monotonically beyond a threshold;
   - check texture count and cache entries return to a steady state.
7. Add a `map-cli stress` or an ignored test if it can run headless; otherwise
   document the manual run. Prefer headless for CI where possible.
8. Update `docs/benchmarks.md` with a T33 section and the final target table.
9. Fix regressions; if a target cannot be met, document why and the measured
   number instead of hiding it.

## Acceptance

```sh
cargo test -p map-core
cargo test -p map-cli
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo run -p map-app  # manual profiling scene
```

- The performance panel reports the metrics above.
- The plan's targets are measured and either met or documented with evidence.
- Memory stays under the configured budgets during the soak.
- A worker panic/load failure/source reload does not crash or hang the app.
- The final defaults (workers, caches, refinement) are recorded in the docs.

## Tests

- Headless scheduler/cache stress tests (T14/T15) with tighter budgets.
- An ignored `map-core` soak test that runs the query pipeline for N iterations
  and asserts bounded cache sizes.
- Manual app soak with the profiling scene; record RSS/frame time.

## Risks / notes

- Profiling a debug build is misleading; use `--release` with debug symbols
  (`[profile.release] debug = true` or `--profile bench`).
- Do not optimize the first tile by blocking the UI; a placeholder is better
  than a freeze.
- A cache budget that is too small causes thrash; measure hit rate before
  shrinking it.
