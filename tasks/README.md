# Tasks — Hytale Seed Explorer

Each task is a self-contained work packet for one branch/worktree. The task
files are the detailed rendition of [`../hytale-seed-map-plan.md`](../hytale-seed-map-plan.md);
the plan stays the scope authority, while these files carry the step-by-step
execution detail. Every task lists the plan sections it implements, the files
it owns, and the acceptance gate.

## Status board

| Task | Title | Milestone | Depends on | Status |
|---|---|---|---|---|
| [T01](T01-worldgen-api-audit.md) | Worldgen API audit and gap matrix | 0 | — | done |
| [T02](T02-seed-semantics.md) | Seed semantics and generator identity | 0 | — | done |
| [T03](T03-assets-and-palette.md) | Asset, palette and licensing decision | 0 | — | done |
| [T04](T04-performance-baseline.md) | Performance baseline harness | 0 | — | done |
| [T05](T05-fidelity-fixtures.md) | Fidelity fixture suite and test harness | 0–1 | T01 | done |
| [T06](T06-workspace-scaffold.md) | Workspace scaffold and CI | 1 | T01, T02 | done |
| [T07](T07-core-types-and-traits.md) | Core domain types and source traits | 1 | T06 | done |
| [T08](T08-generator-worker-pool.md) | Generator worker pool and cheap queries | 1 | T06, T07 | done |
| [T09](T09-chunk-surface-slice.md) | Chunk cache, surface and slice queries | 1 | T07, T08 | done |
| [T10](T10-coordinate-zoom-math.md) | Coordinate, zoom and tile math | 1 | T06, T07 | done |
| [T11](T11-headless-png-render.md) | Headless 512×512 PNG render | 1 | T05, T08, T09, T10 | done |
| [T12](T12-egui-app-shell.md) | egui shell, seed input and app state | 2 | T06, T07, T08, T10 | proposed |
| [T13](T13-biome-layer.md) | Biome layer and palette | 2 | T08, T10, T11 | proposed |
| [T14](T14-tile-cache.md) | Tile keys, cache and eviction | 3 | T07, T10 | proposed |
| [T15](T15-tile-scheduler.md) | Tile scheduler, workers and cancellation | 3 | T09, T10, T14 | proposed |
| [T16](T16-pan-zoom-viewport.md) | Pan, zoom and viewport rendering | 3 | T12, T13, T14, T15 | proposed |
| [T17](T17-height-relief-fluid-layers.md) | Height, relief and fluid layers | 4 | T09, T15 | proposed |
| [T18](T18-inspector-resolution.md) | Inspector readout and resolution selector | 4 | T09, T12, T16 | proposed |
| [T19](T19-layer-compositing-debug.md) | Layer compositing and debug layers | 4 | T13, T17, T18 | proposed |
| [T20](T20-surface-block-palette.md) | Surface block palette data | 5 | T03, T09 | proposed |
| [T21](T21-full-block-mode.md) | Full block mode and progressive refinement | 5 | T17, T19, T20 | proposed |
| [T22](T22-feature-placement-query.md) | Feature placement query API in worldgen | 6 | T01, T05, T08 | proposed |
| [T23](T23-feature-catalog-overlay.md) | Feature catalog, source adapter and overlay | 6 | T16, T22 | proposed |
| [T24](T24-feature-search.md) | Feature search engine and streaming | 7 | T22, T23 | proposed |
| [T25](T25-biome-search.md) | Biome search | 7 | T08, T24 | proposed |
| [T26](T26-search-ui.md) | Search UI, cancellation and filters | 7 | T18, T23, T24, T25 | proposed |
| [T27](T27-cave-entrances.md) | Cave entrance overlay | 8 | T22, T23 | proposed |
| [T28](T28-cave-slices-density.md) | Cave Y-slice and density heat map | 8 | T09, T21, T27 | proposed |
| [T29](T29-map-tools.md) | Grid, go-to, spawn/origin and measurement | 9 | T16, T23 | proposed |
| [T30](T30-waypoints.md) | Waypoints and annotations | 9 | T16, T29 | proposed |
| [T31](T31-share-settings-history.md) | Share strings, settings and seed history | 9 | T12, T16, T30 | proposed |
| [T32](T32-png-export.md) | PNG export | 9 | T21, T23, T29, T30 | proposed |
| [T33](T33-performance-tuning.md) | Performance, cache tuning and robustness | 10 | T04, T21, T24, T28, T32 | proposed |
| [T34](T34-packaging-docs-release.md) | Packaging, docs and release | 10 | T33 | proposed |

## Execution order

The `Depends on` column in the status board is authoritative; the diagram below
is a reading aid for the main path.

```text
T01 ─┬─> T05 ────────────────┐
T02 ─┤                       │
T03 ─┤                       │
T04 ─┼─> T06 ─> T07 ─┬─> T08 ─┼─> T09 ─┬─> T11 ─> T13 ─┐
     │               │        │        │               │
     │               │        └─> T10 ─┴─> T14 ─> T15 ─┼─> T16
     │               └────────────────────> T12 ────────┘
     │                                              │
     │              T17 <───────────────────────────┤
     │              T18 <───────────────────────────┤
     │              T19 <───────────────────────────┤
     │              T20 <── T03                    │
     │              T21 <── T17, T19, T20          │
     │              T22 <── T01, T05, T08          │
     │              T23 <── T16, T22               │
     │              T24 <── T22, T23               │
     │              T25 <── T08, T24               │
     │              T26 <── T18, T23, T24, T25     │
     │              T27 <── T22, T23               │
     │              T28 <── T09, T21, T27          │
     │              T29 <── T16, T23               │
     │              T30 <── T16, T29               │
     │              T31 <── T12, T16, T30          │
     │              T32 <── T21, T23, T29, T30     │
     └────────────> T33 <── T04, T21, T24, T28, T32
                    T34 <── T33
```

T01 is the first audit; T02–T04 can run in parallel with it, and T05 starts
after T01. T06 is the gate that unblocks the map-core work. T22 can start as
soon as T01/T05 are done; it is the only task that edits the
`hytale-server-rust` workspace and should be coordinated with the main agent.

## Rules

1. **One task per branch/worktree.** Never edit files owned by another task.
2. **Shared files are main-agent-owned.** The root `Cargo.toml`, workspace
   manifests, `hytale-seed-map-plan.md`, this board and CI config are shared
   unless the task explicitly lists them.
3. **Read the plan references first.** Every task links to the plan section it
   implements; do not redesign scope from scratch.
4. **Keep `map-core` headless.** It must not depend on `eframe`, `egui`,
   `arboard` or windowing. All rendering that produces pixels belongs there;
   windowing, textures and clipboard belong in `map-app`.
5. **Do not commit game assets.** The Hytale asset pack is user-supplied and
   proprietary. Commit only code, schemas, small test fixtures and generated
   numeric palettes that the licensing decision (T03) explicitly allows.
6. **Worldgen changes are gated.** Do not change generation semantics without
   bumping `GENERATOR_VERSION` and updating the fidelity/golden fixtures.
   Generation changes belong in `hytale-server-rust`, not in a duplicated
   explorer implementation.
7. **Acceptance is the gate.** Run the commands listed in the task's
   *Acceptance* section, plus `cargo fmt` and `cargo clippy` for the touched
   crates. Report deviations instead of silently widening scope.
8. **Report:** files changed, commands run, results, deviations, follow-ups.
9. **Update this board only through the main agent.** The execution agent
   reports status; the main agent edits the shared status column.

## Task template

```md
# TNN — Title

**Status:** proposed
**Milestone:** N — name
**Depends on:** T…
**Blocks:** T…
**Plan refs:** [§…](../hytale-seed-map-plan.md#anchor)
**Agent fit:** low | medium | high

## Goal
## Context
## Scope
## Deliverables
## Work
## Acceptance
## Tests
## Risks / notes
```
