# T34 — Packaging, docs and release

**Status:** proposed
**Milestone:** 10 — Polish
**Depends on:** T33
**Blocks:** —
**Plan refs:** [§8](../hytale-seed-map-plan.md#8-milestones), [§10.8](../hytale-seed-map-plan.md#10-risks-and-open-questions), [§3](../hytale-seed-map-plan.md#3-architecture)
**Agent fit:** medium

## Goal

Make the explorer installable and understandable: a clean build from a fresh
checkout, a documented asset/world setup, a release artifact for desktop
platforms, and docs that explain architecture, fidelity, performance and
licensing. No new map features.

## Context

The plan's v1.0 includes caves/tools/export and packaging for
Windows/Linux/macOS. The app depends on an external generator checkout (path
dependencies) and a user-supplied Hytale asset pack, so the release docs must
be explicit about both.

The code license (MIT) and the game assets are separate; T03 records the asset
policy. Do not package proprietary assets.

## Scope

### In

- `README.md`:
  - what the app does and a short screenshot/diagram (no proprietary assets);
  - prerequisites: Rust toolchain, generator checkout path, Hytale asset pack
    location, default world;
  - build/run commands for app/CLI;
  - keyboard/mouse shortcuts;
  - troubleshooting (missing assets, generator load failure, platform libs).
- `docs/`:
  - `architecture.md` — map-core/map-app/map-cli, source pipeline, tile/cache,
    search, caves;
  - `assets.md` — the T03 decision, how to point at assets, what is/is not
    committed;
  - `fidelity.md` — T05 fixtures, generator version, how to update;
  - `benchmarks.md` — T04/T33 baseline and profiling;
  - `tasks.md` or a link to `tasks/README.md` for maintainers.
- Release packaging:
  - Linux: `cargo build --release -p map-app -p map-cli`; document required
    system libs and a `.desktop` file if useful;
  - Windows/macOS: document the build commands and known platform caveats;
  - a reproducible-ish archive script (`tools/release.sh`) with a SHA-256
    checksum and `BUILD-INFO.txt` (version, target, git revision, rustc);
  - do not bundle the asset pack or world saves.
- CI:
  - fmt/clippy/test matrix for Linux;
  - app build (not launch) on Linux; optional Windows/macOS build jobs if the
    repo has runners;
  - a headless CLI smoke test.
- Versioning:
  - workspace version, app title/about, CLI `--version`;
  - a short `CHANGELOG.md` with the v1.0 entry;
  - generator version compatibility table.
- License/notice:
  - code license (MIT unless the repo says otherwise);
  - third-party dependency license note (cargo-deny optional);
  - explicit "not affiliated with Hypixel/Hytale; assets are user-supplied".
- Final acceptance:
  - fresh clone + generator checkout + user assets → app runs and CLI renders;
  - task index status is accurate;
  - no secrets/assets/large fixtures in git.

### Out

- Auto-update, installers with code signing, app stores.
- WebAssembly build (plan §10.8).
- Bundling a generator binary or a world save.

## Work

1. Write the README and docs, starting from the actual commands that passed in
   T06/T33. Do not document an untested command.
2. Add a `tools/release.sh`:
   - `cargo build --release --locked -p map-app -p map-cli`;
   - stage the binaries, README, docs and license;
   - write `BUILD-INFO.txt`;
   - create a deterministic archive and SHA-256 (fixed mtimes if feasible);
   - refuse to include `Assets/`, `target/`, fixtures over a size limit or
     any `.zip`.
3. Add/refresh CI:
   - cache the cargo registry/target;
   - run `cargo fmt`, `clippy`, `test -p map-core -p map-cli`;
   - build `map-app`;
   - run `map-cli --help` and a tiny render if the fixture world is available;
   - optionally a release archive job.
4. Add version metadata:
   - root workspace version and `map-app`/`map-cli` `--version`;
   - a `GENERATOR_VERSION` compatibility table in `docs/fidelity.md`;
   - a `CHANGELOG.md` entry for v0.1/v1.0 (match the plan's slices).
5. Review the repository for accidental asset/data commits:
   - `git ls-files` for `Assets`, `*.zip`, large fixtures, user configs;
   - `.gitignore` coverage;
   - document the expected user asset path.
6. Run the final gate on a clean checkout/target:
   - `cargo clean` (or a fresh worktree), build, run the CLI render, launch
     the app, import a share string, export a PNG;
   - record the commands and results.
7. Update `tasks/README.md` statuses via the main agent and link the release
   docs from the plan (if the main agent wants the plan updated).

## Acceptance

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p map-core -p map-cli
cargo build --release -p map-app -p map-cli
./tools/release.sh
sha256sum -c dist/hytale-seed-explorer-*.sha256
```

- A fresh checkout with the documented generator path and user assets builds
  and runs.
- The README answers: where do assets go, how do I run the app/CLI, how do I
  report a map mismatch.
- The release archive contains no game assets, no saves and no secrets; the
  checksum validates.
- CI is green on the supported Linux runner; Windows/macOS instructions are
  clearly marked as untested or tested.
- The task board and changelog reflect the release state.

## Tests

- The final clean-build/render/export checklist, recorded in the release
  notes.
- `git ls-files` audit for large/proprietary files.
- A CLI `--version`/`--help` smoke test in CI.

## Risks / notes

- Path dependencies make a source checkout release awkward. Document the
  generator path or add an optional git dependency/vendoring step; do not
  silently break a fresh clone.
- A "portable" binary still needs the user's asset pack; say so up front.
- Do not claim byte-identical game parity in the release notes unless T05 and
  T33 prove it for the documented pack/version.
