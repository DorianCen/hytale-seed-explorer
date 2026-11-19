# T15 — Tile scheduler, workers and cancellation

**Status:** done (deliverable: `map-core/src/scheduler.rs` — `CancelToken`
(structurally the worldgen pool's, so renderers pass tokens through),
`TileSubject` (the request's non-position key fields), the
`TileRenderer` trait, `TileEvent::{Started, Ready, Failed, Cancelled}`
carrying request generations, and `TileScheduler`: a priority queue of
plain worker threads (no rayon, per the task's risk note) with panic
containment, exact dedup against the tile cache, aggressive
`cancel_outside`, worker-side cache insertion (no re-render window),
`is_pending`/`SchedulerStats` for T16 placeholders and T33 tuning —
plus `map-core/tests/scheduler.rs` (9: nearest-first order, dedup,
cached skip, view-exit cancellation, stale-generation marking, panic
containment, clean shutdown, the 100-tile / 20 ms cancel row, the
30-view panning stress) and 3 unit tests; supporting changes:
`panic_message` moved to `error.rs`, `Tile` now derives
`Debug`/`Clone`/`PartialEq`)
**Milestone:** 3 — Tiling and navigation
**Depends on:** T09, T10, T14
**Blocks:** T16, T17
**Plan refs:** [§5.1](../hytale-seed-map-plan.md#51-tiles-and-zoom), [§5.2](../hytale-seed-map-plan.md#52-resolution-modes), [§9](../hytale-seed-map-plan.md#9-performance-correctness-and-testing)
**Agent fit:** high

## Goal

Build the background tile pipeline: compute the visible tiles, queue missing
ones nearest the viewport center first, render them on workers, deduplicate
in-flight work, cancel tiles that leave the view, and stream results back to
the UI without blocking the frame.

## Context

The plan requires:

- each frame compute visible tiles; queue missing ones nearest the center
  first;
- draw the nearest cached lower-detail tile as a placeholder until the real
  one arrives;
- cancel queued tiles that scroll out of view (generation counter or priority
  queue);
- UI frame time under 16 ms, first visible tile within ~100 ms after the source
  is ready.

The scheduler is layer-agnostic. It calls a `TileRenderer` trait; the biome,
height, relief, surface and cave layers provide implementations.

## Scope

### In

- `map-core/src/scheduler.rs`:
  - `TileRequest { key, rect, priority, cancel }`;
  - `TileRenderer` trait:
    ```rust
    pub trait TileRenderer: Send + Sync {
        fn render(&self, key: &TileKey, cancel: &CancelToken) -> Result<Tile, SourceError>;
    }
    ```
  - `TileScheduler` with a worker pool, priority queue, in-flight set,
    generation counter and result channel;
  - `schedule(visible: TileRange, viewport_center, renderer)` and
    `cancel_outside(visible)`;
  - `TileEvent::{Ready(Tile), Failed(TileKey, SourceError), Cancelled(TileKey),
    Started(TileKey)}`.
- A `CancelToken` (wrap `Arc<AtomicBool>`; or reuse the worldgen token).
- Tests with a fake renderer that sleeps and records start order.
- Integration hooks for the app: request visible tiles each frame, drain
  events, update cache/textures.

### Out

- Painting/placeholders and pan/zoom input (T16).
- The actual layer render algorithm (T13/T17/T21/T28).
- Feature/search jobs (they use their own scheduler or T24).

## Design

Priority queue:

- Key: tile distance from the viewport center, measured in tile-space
  (Chebyshev or squared Euclidean); nearest first.
- Higher priority for the tile under the cursor if desired; v1 uses center.
- Deduplicate: a tile already in-flight or cached is not queued.
- A generation counter increments when the visible set changes; events from an
  older generation for a tile no longer visible are dropped/cancelled.

Worker pool:

- `N` threads (configurable; default `min(4, available_parallelism)` initially,
  tune in T33).
- Each worker pops the highest-priority job, checks its cancel token, calls the
  renderer, sends the result.
- The renderer calls `WorldSource` (which has its own generator workers).
  Avoid nested rayon pools; use plain threads + channels to keep ownership
  clear.
- Panic recovery: `catch_unwind` around the renderer; send a `Failed` event
  instead of killing the worker.

Cancellation:

- `CancelToken` checked between rows/samples in each renderer.
- When a tile leaves the view, set its token; if it is still queued, remove it
  lazily on pop.
- Do not cancel work that is already 90% complete if the replacement would
  restart; v1 can cancel aggressively, then T33 tunes. Document the policy.

## Work

1. Implement `CancelToken` and `TileRenderer`.
2. Implement the priority queue (a small `BinaryHeap` + condvar, or
   `crossbeam_channel` with a separate priority heap). Include a `len()` and a
   `clear()`.
3. Implement `TileScheduler::new(workers)` and `schedule`:
   - compute the set difference between requested and in-flight/cached;
   - insert/update jobs with a new generation;
   - wake workers.
4. Implement `cancel_outside(visible)`:
   - set tokens for in-flight keys not in the visible set;
   - remove queued jobs not visible (or mark them cancelled).
5. Implement `poll_events` / `try_recv` for the UI. Include `Started` and
   `Ready` events so the app can show "generating N tiles".
6. Add a `FakeRenderer` for tests:
   - configurable sleep;
   - records the key and start timestamp;
   - can block on a token to test cancellation.
7. Tests:
   - nearest-first order for a 5×5 tile grid around a center;
   - duplicate `schedule` calls do not create duplicate jobs;
   - a cached tile is not requested again;
   - leaving the view cancels queued/in-flight tiles;
   - stale generation results are not delivered as fresh (or include a
     generation and let the app drop them);
   - a panicking renderer does not kill the worker (subsequent tiles still
     render);
   - shutdown joins all workers and does not hang.
8. Add a simple instrumentation counter: queued, in-flight, completed,
   cancelled, render time. Expose via `SchedulerStats` for T33.

## Acceptance

```sh
cargo test -p map-core scheduler
cargo clippy -p map-core --all-targets -- -D warnings
```

- Visible tiles are requested nearest-center first.
- No tile is rendered twice concurrently.
- Cancellation stops queued work and marks in-flight work; no stale tile is
  applied to the wrong view.
- A renderer panic is contained and reported.
- Shutdown is clean.
- The scheduler never blocks the caller; `schedule`/`poll` are quick.

## Tests

- `map-core/tests/scheduler.rs` with the fake renderer.
- A timing test: schedule 100 fake 5 ms tiles, cancel at 20 ms, verify fewer
  than 100 complete and no panics.
- A stress test with panning generations (simulate 30 view changes) and assert
  the final visible set is eventually rendered.

## Risks / notes

- The worldgen source has its own worker pool; oversubscribing (scheduler N ×
  generator M) can thrash. Start with a small scheduler pool and tune in T33.
- Do not use a rayon parallel iterator for the outer tile loop if each item
  blocks on a channel; it can deadlock work-stealing under load. Plain threads
  with a priority queue are simpler and predictable.
- Placeholder selection (parent tile) belongs to T16, but the scheduler must
  expose which keys are cached/in-flight to make it possible.
