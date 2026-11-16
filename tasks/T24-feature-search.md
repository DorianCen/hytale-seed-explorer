# T24 — Feature search engine and streaming

**Status:** proposed
**Milestone:** 7 — Search
**Depends on:** T22, T23
**Blocks:** T25, T26, T33
**Plan refs:** [§6.1](../hytale-seed-map-plan.md#6-search-design), [§6.4](../hytale-seed-map-plan.md#64-multi-criteria-search-seed-finder-mode), [§9](../hytale-seed-map-plan.md#9-performance-correctness-and-testing)
**Agent fit:** high

## Goal

Implement the search engine for structures, prefabs, spawns and cave entrances:
given a kind/tag, a center and a maximum radius, stream the nearest results
first, sorted by distance, with cancellation and optional filters.

## Context

T22 provides `features_in(rect, filter)` without terrain generation. The plan
requires expanding **rings** of regions so the nearest results come first,
streaming results to the UI, and cancellation. Each result has an icon, name,
coordinates and distance; clicking centers the map, right-click copies a
teleport string.

A "region" is a search tile, for example 512×512 blocks. Ring search means:

1. start with the region containing the center;
2. expand a ring of regions around it;
3. query each ring region, deduplicate features that straddle boundaries;
4. sort the features in the ring by distance;
5. emit them if they are within the radius and not already emitted;
6. stop when the ring's minimum possible distance exceeds the radius or the
   caller's result limit.

## Scope

### In

- `map-core/src/search/feature.rs`:
  - `FeatureSearch { kinds, center, max_radius, max_results, filters }`;
  - `SearchHandle` with `cancel()`;
  - `SearchEvent::{Started, Result(SearchResult), Progress(SearchProgress),
    Done(SearchStats), Error(SourceError)}`;
  - ring iterator with dedupe;
  - distance computation and sorting;
  - streaming callback/channel;
  - "within N blocks of another feature" post-filter.
- `SearchResult`:
  ```rust
  pub struct SearchResult {
      pub feature: Feature,
      pub distance: f64,
      pub teleport: String, // /tp x y z, shared formatter with T31
  }
  ```
- Tests with a fake `FeatureSource`, brute-force comparison, cancellation and
  filter tests.
- A benchmark against the T22 10k×10k query.

### Out

- The search UI (T26).
- Biome search (T25).
- Multi-criteria seed finder (T33/out of scope v1). Design the API so it can
  be reused, but do not implement batch seed search.
- Live feature placement changes; the source is immutable per seed.

## Algorithm details

- Region size: 512 blocks (configurable). Align rings to region boundaries
  around the center. For a center `(cx, cz)`, the center region is
  `floor(cx/512), floor(cz/512)`.
- Ring `r` is the set of regions with `max(|dx|, |dz|) == r`; query all of
  them, then merge.
- Deduplicate by `(kind, name, rounded position, bounds)`; the worldgen query
  may return a feature that intersects two adjacent query rects. Prefer the
  exact same key as T23.
- For each ring:
  - compute the minimum possible distance from the center to any point in the
    ring (`ring_min_distance`);
  - if `ring_min_distance > max_radius`, stop;
  - query all regions in the ring, collect features within `max_radius`;
  - sort by Euclidean distance (using the feature's anchor position);
  - skip results already emitted;
  - emit until `max_results` is reached.
- If `max_results` is reached, stop and report `Done { truncated: true }`.
- Filters:
  - kinds/categories/name/min size are passed to T22 where possible;
  - "within N blocks of another feature" needs a local index of all results in
    the current ring plus previously emitted results; define the semantics
    (within N of any other result of a given kind) and document.
- Cancellation:
  - check the token between regions and before emitting;
  - a cancelled search sends `Done { cancelled: true }` or no `Done`; pick one
    and document. The UI must not wait forever.

## Work

1. Implement the ring iterator and region alignment. Test with centers inside,
   on and outside region boundaries, including negative coordinates.
2. Implement the query/merge/dedupe/sort loop. Do not collect the entire world:
   stream ring by ring.
3. Implement `SearchHandle`:
   - spawn a named thread (or use a small worker) that owns the search;
   - `cancel()` sets an `AtomicBool`;
   - the event sender is bounded (for example 64 results) so a slow UI does not
     grow memory without bound; define backpressure (drop progress, block
     results).
4. Implement the result formatter:
   - `teleport` string `/tp x y z`;
   - display name: feature name, category and distance;
   - keep formatting pure and unit-testable (T26 reuses it).
5. Filters:
   - pass kind/category/name/min-size to T22;
   - implement the proximity filter locally; test that it does not drop a
     closer valid result due to ring order.
6. Tests:
   - a fake source with known features at known distances → exact order;
   - brute-force comparison on a small area: the search returns the same set
     and no closer feature is skipped;
   - max radius/result limits;
   - negative centers and features exactly on ring boundaries;
   - cancellation mid-ring;
   - an empty source → `Done` with zero results;
   - a source error → `Error` and no panic.
7. Benchmark:
   - 10,000×10,000 radius, a sparse and a dense seed;
   - time to first result, time to 100 results, total;
   - report to T04/T33. The plan target is first results within ~500 ms.

## Acceptance

```sh
cargo test -p map-core feature_search
cargo test -p map-core search_property
cargo clippy -p map-core --all-targets -- -D warnings
```

- Results are streamed nearest-first; the first result arrives before the
  full ring scan finishes.
- No result is duplicated or outside the radius.
- The brute-force test shows no closer feature was skipped for the same
  filters.
- Cancellation stops the search promptly (no thread leak).
- The API is reusable by T25/T26 and does not depend on egui.

## Tests

- `map-core/tests/feature_search.rs` with a fake source.
- `map-core/tests/search_property.rs` with proptest: random feature sets,
  compare against brute force.
- A manual benchmark using T22 on the fixture seed.

## Risks / notes

- The ring minimum-distance bound assumes features are points. If a feature's
  bounds extend closer than its anchor, use the bounds for the distance bound
  or document that search distance is anchor distance. T23 should expose the
  anchor policy.
- A single huge prefab can intersect many regions; dedupe before sorting.
- Do not use `rayon` for rings without preserving nearest-first streaming;
  parallel ring queries are fine if results are merged/sorted before emit.
