# T25 — Biome search

**Status:** proposed
**Milestone:** 7 — Search
**Depends on:** T08, T24
**Blocks:** T26
**Plan refs:** [§6.2](../hytale-seed-map-plan.md#62-biome-search), [§9](../hytale-seed-map-plan.md#9-performance-correctness-and-testing)
**Agent fit:** high

## Goal

Find the nearest occurrence of a selected biome, optionally with a minimum
patch size, using a coarse pass over expanding rings and a fine refinement pass
at the boundary. Reuse T24's event/cancellation/streaming shape so the UI has
one search model.

## Context

`WorldSource::biome_at` is cheap and chunk-free. A naive full-resolution scan
over a large radius is too expensive; the plan proposes:

1. coarse pass: sample `biome_at` on a wide grid (16–32 blocks) in expanding
   rings to find candidates;
2. refinement pass: sample near the best candidates at the finest step to find
   the actual boundary and nearest block;
3. optional minimum patch area ("a Forest at least 200×200").

Biome boundaries are irregular; a coarse sample can miss a narrow biome or
report a false candidate. The refinement pass must verify at step 1 (or 2) and
return a real block coordinate, not a coarse cell center.

## Scope

### In

- `map-core/src/search/biome.rs`:
  - `BiomeSearch { biome: BiomeId, center, max_radius, min_patch_blocks:
    Option<u32>, max_results }`;
  - coarse ring scan at a configurable step (default 16, fallback 32);
  - candidate refinement to step 1/2;
  - optional contiguous patch-area measurement;
  - `SearchEvent` reuse (or a biome-specific event with the same shape);
  - cancellation and streaming.
- Tests with a synthetic `WorldSource` and a fixed-seed fixture.
- A benchmark for a 10,000×10,000 radius.

### Out

- The search UI (T26).
- Multi-biome / multi-criteria search (future).
- Exact polygon extraction; v1 returns points and optional area.

## Algorithm

Coarse pass:

- Expand rings of coarse cells (step 16 or 32) from the center outward.
- For each cell, sample the biome at the cell center (or all four corners if a
  biome is small and the step is large; document the trade-off). Use
  `WorldSource::biome_region` for a whole ring where possible; do not pay one
  worker-channel round-trip per cell.
- If it matches, record a candidate `(x, z, coarse_distance)`.
- Stop when the ring's minimum possible distance exceeds `max_radius` or the
  result limit is reached.

Refinement:

- For each candidate, search the local neighborhood at step 1/2 for a matching
  sample nearest to the center.
- Verify the match with a second sample or a small neighborhood to avoid a
  one-cell noise artifact.
- Compute the exact distance to the refined position; sort candidates by that
  distance before emitting. Because a coarse candidate may refine to a point
  closer than a previously emitted result, keep a small look-ahead window
  (for example refine all candidates in the current and next ring before
  emitting) or document that results are "approximately nearest first". The
  plan asks for nearest results, so prefer the look-ahead.
- Patch area:
  - if requested, flood-fill the matching biome on a coarse grid around the
    candidate;
  - convert cells to blocks (careful at boundaries);
  - return the area and only emit candidates meeting the minimum.
  This is optional/expensive; cap the flood-fill radius and report
  `area_capped = true`.

## Work

1. Implement the coarse ring iterator and candidate collection. Reuse T24's
   region/ring math where possible; biome search rings are coordinate rings,
   not feature-query regions.
2. Implement refinement:
   - local bounded search at step 1;
   - choose the nearest matching sample by Euclidean distance to the search
     center, not to the coarse cell;
   - if no match, discard the candidate.
3. Implement patch area:
   - BFS/flood fill on the coarse grid;
   - stop at the cap;
   - count matching cells and estimate blocks²; document that it is an
     estimate unless the boundary is exact.
4. Implement streaming with look-ahead:
   - refine candidates in the current ring and one ring ahead;
   - emit in distance order;
   - if a later ring produces a closer result than an emitted one, either
     re-emit with a correction event or document an "approximate" mode. Prefer
     a bounded look-ahead so this is rare; test it.
5. Add `SearchEvent::Result` with `BiomeMatch { position, distance, patch_area,
   area_capped }` and progress.
6. Tests:
   - synthetic biome grid with a known nearest cell;
   - narrow biome strip that a coarse sample could miss (document the
     limitation and test the chosen sampling strategy);
   - boundary refinement returns a block inside the biome;
   - negative coordinates;
   - min patch area filters small islands;
   - cancellation;
   - brute-force comparison on a small radius.
7. Benchmark: 10,000×10,000 radius, common and rare biomes; first result time
   and total time. Report to T04/T33; the plan target is first results within
   ~500 ms.

## Acceptance

```sh
cargo test -p map-core biome_search
cargo test -p map-core biome_search_property
cargo clippy -p map-core --all-targets -- -D warnings
```

- A search returns a real block position with the requested biome.
- Results are sorted by distance within the look-ahead window, and the
  documented guarantee is tested.
- Minimum patch area filters small regions without hanging.
- Cancellation is prompt.
- A brute-force small-area comparison finds the same nearest match.

## Tests

- `map-core/tests/biome_search.rs` with a synthetic source.
- `map-core/tests/biome_search_property.rs`: random biome fields, compare the
  nearest result with brute force within a small radius.
- A fixture check using T05's biome cases.

## Risks / notes

- Coarse sampling can miss 1-cell-wide biomes. If the plan/user cares about
  rare thin biomes, expose a "thorough" mode that lowers the step; document the
  cost.
- Patch area is expensive and approximate. Do not run it by default; make it
  an explicit criterion.
- Do not reuse the feature-search region grid blindly; biome continuity means a
  boundary can cross a region. The refinement pass is what guarantees the
  result.
