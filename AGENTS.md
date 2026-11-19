# Hytale Seed Explorer — agent instructions

Repo-specific guidance for OpenCode sessions. The plan
(`hytale-seed-map-plan.md`) is the scope authority; per-task detail
lives in `tasks/`. When this file and the code disagree, the code wins —
and this file gets fixed.

## Build prerequisites (the workspace does not build without these)

- The generator is an **external path dependency**: a checkout of
  `DorianCen/hytale-server-rust` must sit at `../hytale-server-rust`
  (a sibling of this repo). It is never vendored or forked. Generation
  semantics change THERE, never in a duplicated explorer implementation,
  and only with a `GENERATOR_VERSION` bump — a bump is a
  cache-invalidation action recorded in the commit message.
- `map-core` pins `GENERATOR_VERSION` itself (`map-core/src/lib.rs`).
  The generator's own tag is not yet on its public `main`, so CI's
  cross-check step compares the two whenever the tag appears; keep them
  equal.
- Generator-backed tests **skip, printing why**, on machines without the
  sibling checkout or without a user-supplied `Assets/extracted` pack.
  Green-but-skipped is normal (CI has no pack); read the skip messages
  before believing a suite covered the real generator.
- GUI builds link X11/Wayland; the exact apt list is in
  `.github/workflows/ci.yml`. No window is ever opened in CI — the GUI
  gate is `cargo build -p map-app`.
- Never commit game assets or palettes derived from them: Hytale assets
  are proprietary (`docs/assets-and-palette.md` has the rules).

## Commands

```sh
cargo test -p map-core -p map-cli                  # headless suite
cargo test -p map-core --features test-support      # fake-source tests
cargo test -p map-core <filter>                     # e.g. tile_cache, scheduler
cargo test -p map-app                               # shell unit tests (CI only builds map-app)
cargo run -p map-app                                # desktop shell
cargo run -p map-cli -- info                        # CLI smoke / identity dump
cargo fmt --all && cargo clippy -p map-core -p map-cli -p map-app --all-targets -- -D warnings
```

- **Test-name filters are the acceptance gate.** Each task's
  `Acceptance` section names a filter (`tile_cache`, `scheduler`,
  `texture_cache`, …) and the tests are deliberately named with that
  prefix; a new test that misses the prefix is invisible to the gate.
- `map-core` must stay GUI-free. CI fails the build on any GUI dep:
  `! cargo tree -p map-core -e normal | grep -E "eframe|egui|arboard|winit|wgpu"`.

## How work is organized

- `tasks/README.md` is the status board; each `tasks/TNN-*.md` is a
  self-contained work packet (goal, scope, work, acceptance, risks).
  Read the task file and its plan refs before writing code; do not
  redesign scope from scratch.
- One task per branch/worktree. Do not edit files another task's file
  claims; shared files (workspace manifests, the plan, the board, CI)
  are main-agent-owned.
- Acceptance is the gate: run the task's listed commands plus
  `cargo fmt` and `cargo clippy` for touched crates; report deviations
  instead of silently widening scope; report files changed, commands
  run, results, deviations, follow-ups.
- Marking done: the board row flips to `done` and the task file's status
  becomes `**Status:** done (deliverable: …)` naming what landed (see
  T10/T13/T14/T15 for the idiom). Commit subjects follow `TNN: title`.
- Subagent packets (docs/status updates, mechanical edits) fit a child
  session; implementation that must stay coherent does not.

## Architecture boundaries that are easy to violate

- Three crates: `map-core` (headless: sources, tiles, scheduler,
  renderers, search — no GUI deps), `map-app` (thin eframe/egui shell),
  `map-cli` (headless CLI). Pixels and data are computed in `map-core`;
  windowing, textures and clipboard live in `map-app`.
- `map-core::source::worldgen` is the ONLY place worldgen types cross
  into map-core; the public API speaks plain ids and grids.
- All coordinate/zoom/tile math is canonical in
  `map-core/src/coords.rs`; nobody downstream re-derives conversions.
- Cache keys (`TileKey`, `ChunkKey`) hash `GeneratorIdentity::stable_hash`
  plus the generator-visible seed (an `i32`). Seeds are numeric only
  (T02: low-32-bit truncation of the input; string hashing does not
  exist — never add a hash fallback). Seed/version/asset/palette changes
  flow through the key, never a side channel.
- Heavy background objects (generator sources, worker pools) are dropped
  on background threads, never the UI thread — see the retirement
  pattern in `map-app/src/state.rs`.

## Style (condensed; precedence: existing code and lint config win)

- rustfmt formats everything; `clippy -D warnings` is the bar. Match
  surrounding code first; `#[rustfmt::skip]` only for deliberate
  alignment or generated code.
- Panics only for violated invariants, via
  `expect("why the invariant holds")`; asserts carry a message that
  explains the condition. Bare `unwrap()` does not appear in this repo.
- `catch_unwind` is banned EXCEPT at foreign-code boundaries — the
  worldgen loader/query boundaries and the scheduler's renderer boundary
  use it as a *documented deviation*: comment the rationale at the site
  and map the payload to a typed `SourceError` via `error::panic_message`.
- API staged for a later task gets `#[allow(dead_code)] // reserved: TNN`
  — neither deleted nor speculatively wired.
- Getters and pure functions carry `#[must_use]`; domain ids are
  newtypes (`BlockId`, `BiomeId`, …); read-only inputs are borrowed
  (`&str`, `&[T]`); `Default` on all-public options structs.
- `Option` for simple failures with an obvious cause, `Result` where the
  caller needs the why. Don't `clone()` to silence the borrow checker;
  don't generalize single-binary code prematurely.
- Comments say what the code cannot: WHY, not what. `//!` module headers
  also carry the module's task ownership.

## Subagent model selection

- Use subagents for small, independent packets. Before spawning, check
  the current catalog (`tools.opencode.models`) — availability changes;
  prefer a listed zero-cost model for the smallest tasks.
- Documented defaults: `opencode-go/deepseek-v4.1-flash` (DeepSeek V4.1
  Flash 1017) is the paid fallback when no free model fits.
  `opencode/glm-5.3-flash` is paid — do not count it as the free option
  unless the catalog actually lists a zero-cost variant.
- Do not force a cheap/free model onto large or high-risk work; keep
  that in the main session. Record the subagent model in the task
  report when it matters.
