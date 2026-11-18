# Hytale Seed Explorer

A desktop map explorer for Hytale seeds, in the spirit of chunkbase.com: enter
a seed, pan/zoom a rendered world map, and search for structures, prefabs,
biomes and caves. It is built on the
[`hytale-server-rust`](https://github.com/DorianCen/hytale-server-rust) world
generator (a bit-exact Rust port of Hytale's Java `WorldGenerator`).

**Status:** pre-v1. The plan is [`hytale-seed-map-plan.md`](hytale-seed-map-plan.md)
(scope authority); per-task detail lives in [`tasks/`](tasks/README.md) and the
completed milestone-0 audits in [`docs/`](docs):

- [`docs/worldgen-api-audit.md`](docs/worldgen-api-audit.md) — what the
  generator can already answer, and the gaps with owners (T01).
- [`docs/seed-semantics.md`](docs/seed-semantics.md) — how a seed becomes the
  generator's integer, and the generator identity (T02).
- [`docs/assets-and-palette.md`](docs/assets-and-palette.md) — where every map
  color comes from, and the licensing rules (T03).

## Build and run

**Prerequisites**

- Rust (stable, edition 2024 capable).
- A checkout of the worldgen workspace
  [`hytale-server-rust`](https://github.com/DorianCen/hytale-server-rust)
  **next to this repository** (`../hytale-server-rust`) — the generator is an
  external path dependency and is never copied or vendored here. Its own
  prerequisites (the committed `testdata/Default` world is enough for the
  headless paths; the asset pack is optional) are documented in that repo.
- Linux GUI builds additionally need the X11/Wayland dev libraries (see
  `.github/workflows/ci.yml` for the exact apt list).

**Commands**

```sh
cargo run -p map-app              # the desktop shell (eframe)
cargo run -p map-cli -- --help    # the headless CLI
cargo run -p map-cli -- info       # prints the resolved generator identity
cargo test -p map-core -p map-cli  # headless tests
cargo fmt --all && cargo clippy -p map-core -p map-cli --all-targets -- -D warnings
```

The application logic is headless by design: `map-core` (sources, tiles,
renderers, search) and `map-cli` run without a GPU or display, so the app is
testable end to end outside the GUI; `map-app` is a thin shell. CI runs the
same battery plus a `cargo build -p map-app` link gate and a
`cargo tree` check that `map-core` never grows GUI dependencies.

## Hytale assets: you must supply your own

This tool does **not** distribute any Hytale game assets — no textures, no
models, no extracted data. Hytale's art and data are proprietary and governed
by the Hytale Shared Source Agreement; redistributing them (or files derived
from them, such as generated color palettes) is not permitted, so none are
committed to this repository.

To use the full-block map layers, point the app at **your own** copy of the
game asset pack (the one your Hytale access grants you). The app reads it
locally and generates its color palette on your machine, into your user cache
directory — that generated palette is for your use only and must not be
committed or shared. Without a pack the app still works: the biome layer uses
the generator's own biome map colors, and surface blocks fall back to a small
built-in palette of **approximate, hand-picked colors** (clearly marked as
such).

See [`docs/assets-and-palette.md`](docs/assets-and-palette.md) for the exact
color sources, formats and rules.

## License

This repository's own code and documentation are original work. The Hytale
assets, world definitions and Java source it interoperates with remain the
property of Hypixel Studios and are governed by their terms; see the assets
section above.
