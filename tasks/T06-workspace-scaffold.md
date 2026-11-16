# T06 — Workspace scaffold and CI

**Status:** proposed
**Milestone:** 1 — Scaffold
**Depends on:** T01, T02
**Blocks:** T07, T08, T10, T12
**Plan refs:** [§3](../hytale-seed-map-plan.md#3-architecture), [§4](../hytale-seed-map-plan.md#4-tech-choices), [§8](../hytale-seed-map-plan.md#8-milestones)
**Agent fit:** medium

## Goal

Turn the single-package repository into the workspace the rest of the plan
assumes: a headless `map-core`, an `eframe`/`egui` `map-app`, and a headless
`map-cli`. The workspace must depend on the existing generator crates by path
and must not copy generator code into the explorer.

## Context

Current state:

- `Cargo.toml` is a single `[package] hytale-seed-explorer` with edition 2024
  and no dependencies.
- `src/main.rs` is a hello-world binary.
- The generator workspace is at
  `/home/chome/Projects/hytale-server-rust` with crates:
  `hytale-math`, `procedural`, `worldgen-model`, `worldgen-hytale`,
  `worldgen-cli`, `worldgen-server`.

The plan's layout names a `worldgen/` directory; in this repository the
generator stays external. Use path dependencies and document the assumption.
If the generator later moves, only the manifests change.

## Scope

### In

- Convert the root manifest to a virtual workspace with resolver 2.
- Create `map-core`, `map-app`, `map-cli` packages.
- Move the current binary stub to `map-app` (or delete it; the app task creates
  the real shell).
- Add the shared dependency table and the external generator path dependencies.
- Add a minimal CI workflow for fmt, clippy, tests and a headless app build.
- Add a root README with build/run commands.
- Keep `map-core` free of GUI dependencies (enforced by review and CI check).

### Out

- Implementing the traits, sources, renderers or UI (T07+).
- Publishing the crates or vendoring the generator.
- Adding asset files.

## Deliverable

```text
/hytale-seed-explorer
├─ Cargo.toml                # [workspace] members + workspace.dependencies
├─ README.md
├─ .gitignore
├─ map-core/
│  ├─ Cargo.toml
│  └─ src/lib.rs             # empty module tree, doc comment
├─ map-app/
│  ├─ Cargo.toml
│  └─ src/main.rs            # minimal eframe app or a stub that prints
├─ map-cli/
│  ├─ Cargo.toml
│  └─ src/main.rs            # clap skeleton
└─ .github/workflows/ci.yml
```

Root `Cargo.toml` shape:

```toml
[workspace]
resolver = "2"
members = ["map-core", "map-app", "map-cli"]

[workspace.package]
version = "0.1.0"
edition = "2024"
license = "MIT"

[workspace.dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
rayon = "1"
crossbeam-channel = "0.5"
moka = { version = "0.12", features = ["sync"] }
image = "0.25"
png = "0.17"
log = "0.4"
env_logger = "0.11"
clap = { version = "4", features = ["derive"] }
eframe = "0.31"
egui = "0.31"
directories = "6"
bytemuck = "1"

# External generator crates. The path is relative to this repository.
worldgen-hytale = { path = "../hytale-server-rust/crates/worldgen-hytale" }
worldgen-model = { path = "../hytale-server-rust/crates/worldgen-model" }
hytale-math = { path = "../hytale-server-rust/crates/hytale-math" }
procedural = { path = "../hytale-server-rust/crates/procedural" }
```

`map-core/Cargo.toml` depends on the generator crates, `serde`,
`serde_json`, `thiserror`, `rayon`, `crossbeam-channel`, `moka`, `image`,
`png`, `log`. It must not depend on `eframe`, `egui`, `arboard` or
`directories`.

`map-app/Cargo.toml` depends on `map-core`, `eframe`, `egui`, `serde`,
`serde_json`, `directories`, `log`, `env_logger`, `arboard` (only if egui's
clipboard is insufficient).

`map-cli/Cargo.toml` depends on `map-core`, `clap`, `serde_json`, `image`,
`png`, `log`, `env_logger`.

## Work

1. Replace the root package manifest with the virtual workspace. Keep the
   package name `hytale-seed-explorer` only as the repository name; binaries
   live in the member packages.
2. Create the three packages. Give `map-core` a `lib.rs` with module
   declarations for the planned modules but no implementation; use `//!`
   documentation to state the headless contract.
3. Move the current `src/main.rs` into `map-app` as a placeholder or delete it
   and let T12 create the app. Do not leave an unbuildable binary.
4. Add the path dependencies and verify that the generator's workspace
   inheritance resolves from outside its own workspace. If it does not, create
   a thin `map-core` adapter that re-exports the generator types and document
   the workaround; do not fork the generator.
5. Add `.gitignore` entries for `/target`, user asset packs, generated
   palettes that T03 decides are private, and local fixture dumps.
6. Add CI:
   - `cargo fmt --all -- --check`
   - `cargo clippy -p map-core -p map-cli --all-targets -- -D warnings`
   - `cargo test -p map-core -p map-cli`
   - `cargo build -p map-app`
   - a Linux dependency install step for eframe (`libxkbcommon-dev`,
     `libwayland-dev`, `libx11-dev`, etc.) or use a headless build flag.
7. Add a root README with:
   - prerequisites (Rust, generator checkout path, asset pack);
   - `cargo run -p map-app`;
   - `cargo run -p map-cli -- --help`;
   - test commands;
   - the note that the app is headless-testable through `map-core`/`map-cli`.
8. Run the workspace gate and record the exact commands in the report.

## Acceptance

```sh
cargo metadata --no-deps --format-version 1 > /dev/null
cargo check --workspace
cargo test -p map-core -p map-cli
cargo build -p map-app
cargo fmt --all -- --check
cargo clippy -p map-core -p map-cli --all-targets -- -D warnings
```

- The workspace builds with the external generator path.
- `map-core` has no GUI dependency (verify with `cargo tree -p map-core`).
- `map-app` runs or builds to a stub; `map-cli --help` works.
- CI config is syntactically valid and covers the same commands.
- No generator source is copied into this repository.

## Tests

- A trivial unit test in `map-core` proves the test harness runs.
- A CLI smoke test (`map-cli --help`) in CI.
- The app build is the only GUI gate; do not launch a window in CI.

## Risks / notes

- `eframe`/`egui` versions move quickly. Pin them in the workspace and bump
  deliberately.
- Path dependencies make the repo non-portable until the generator is
  published or vendored. Document the path clearly; T34 can add a vendoring or
  git-dependency option for release.
- The generator uses edition 2021 while this workspace uses 2024. Both are
  supported; do not force a rewrite of the generator for edition alignment.
