# T10 — Coordinate, zoom and tile math

**Status:** done (deliverable: `map-core/src/coords.rs` — floor division,
`ZoomLevel`/`effective_step`, block ↔ tile conversions, `Viewport`
screen ↔ world math with `zoom_at_cursor` — plus 25 unit tests including
the fixed table at center `(-1203, 455)` and the proptest suite
`tests/coords_property.rs` (5); `proptest` added as a dev-dependency)
**Milestone:** 1 — Scaffold (used by milestone 3)
**Depends on:** T06, T07
**Blocks:** T11, T12, T13, T14, T15
**Plan refs:** [§5.1](../hytale-seed-map-plan.md#51-tiles-and-zoom), [§7](../hytale-seed-map-plan.md#7-ui-layout), [§9](../hytale-seed-map-plan.md#9-performance-correctness-and-testing)
**Agent fit:** high

## Goal

Implement and test the coordinate system once, in headless code, so the tile
cache, scheduler, renderers and egui widget all share the same block ↔ pixel ↔
tile conversions. Negative coordinates and tile-edge cases are the main source
of map bugs; this task is the guardrail.

## Context

The plan's constraints:

- Tile = 256×256 pixels.
- Zoom levels are discrete; the UI interpolates smoothly between them.
- The default resolution is 2×2 blocks per sample; full block is 1×1.
- The renderer picks `max(user_step, step_needed_for_zoom)`.
- Tile key includes zoom/step, so a cache entry never mixes resolutions.

Define one canonical mapping:

- `step` = blocks per sample, a power of two (1, 2, 4, 8, ...).
- `zoom_level` = log2(step), so step = `1 << zoom_level`.
- A tile covers `256 * step` blocks per side.
- Screen scale can zoom in beyond 1 pixel/block by scaling the texture; do not
  generate sub-block samples.

## Scope

### In

- `map-core/src/coords.rs` (and `tiles.rs` for tile coordinates if not in T14):
  - `BlockPos`, `BlockRect` (use T07's `Rect`), `WorldPos` (f64),
    `ScreenPos`, `TileCoord`, `TileRect`, `Viewport`, `ZoomLevel`.
  - conversions: block ↔ tile, tile ↔ block rect, screen ↔ world, world ↔
    screen, visible tile range.
  - floor division for negative coordinates.
  - `ZoomLevel::step()`, `ZoomLevel::pixels_per_block()` (display scale),
    `ZoomLevel::from_step()`.
  - `effective_step(user_min_step, zoom_level)`.
- Unit tests and property tests.
- A small `map-cli` debug command or a `#[cfg(test)]` dump of conversions is
  optional; not required for acceptance.

### Out

- Texture allocation and egui painting (T14/T16).
- Smooth interpolation state (T16 uses these primitives).
- Tile keys/cache (T14).

## Definitions

```rust
pub const TILE_SIZE: u32 = 256;

pub struct BlockPos { pub x: i32, pub z: i32 } // block coordinates
pub struct WorldPos { pub x: f64, pub z: f64 } // continuous block coords
pub struct ScreenPos { pub x: f32, pub y: f32 } // egui points

pub struct ZoomLevel(pub u8); // 0 => step 1, 1 => step 2, 2 => step 4...
impl ZoomLevel {
    pub fn step(self) -> u32 { 1u32 << self.0 }
    pub fn tile_blocks(self) -> i64 { i64::from(TILE_SIZE) * i64::from(self.step()) }
}

pub struct Viewport {
    pub center: WorldPos,       // block coordinates at the viewport center
    pub scale: f64,             // screen points per block (display scale)
    pub screen_rect: egui::Rect, // app-only; core uses a plain size instead
}
```

Because `map-core` must stay headless, define `Viewport` with plain
`width`/`height` f32 fields, and let `map-app` convert to/from `egui::Rect`.

Conversion rules:

```text
tile = floor_div(block, TILE_SIZE * step)
tile_origin = tile * TILE_SIZE * step
screen_x = (world_x - view.center.x) * view.scale + view.width / 2
world_x  = (screen_x - view.width / 2) / view.scale + view.center.x
visible tiles = tile range covering the four viewport corners, expanded by 1
```

All intermediate tile math uses `i64`; clamp to `i32` only at the public block
boundary and return an error/`None` on overflow.

`effective_step`:

```text
step_needed = 1 << zoom_level
step = max(user_min_step, step_needed)
```

Round `user_min_step` up to the next power of two. The UI's discrete
`zoom_level` selects the step; smooth display scaling is separate.

## Work

1. Implement `floor_div` and `floor_mod` for signed integers; add tests for
   `-1 / 256 == -1`, `-256 / 256 == -1`, `-257 / 256 == -2`.
2. Implement `ZoomLevel` and `effective_step`. Decide the allowed range (for
   example 0..=20) and document the maximum world extent / overflow behavior.
3. Implement tile conversions:
   - `tile_of_block(block, step) -> TileCoord`;
   - `tile_rect(tile, step) -> Rect` (inclusive block bounds);
   - `tiles_in(rect, step) -> TileRange` (inclusive min/max tile coords);
   - `visible_tiles(viewport, step) -> TileRange`.
4. Implement screen/world conversions with f64 in core; no egui types.
5. Implement `Viewport::screen_to_block`, `block_to_screen`,
   `screen_to_world`, `world_to_screen`, and `zoom_at_cursor`:
   - given cursor screen position and a new zoom level, keep the world point
     under the cursor fixed by adjusting `center`.
   This is the most error-prone UI math; test it in core.
6. Add tests:
   - round-trip screen ↔ world for several zoom levels and negative centers;
   - tile-edge blocks (`-1`, `0`, `255`, `256`, `-256`, `-257`);
   - a viewport straddling tile boundaries returns the correct tile range;
   - `effective_step` for user steps 1/2/4 and zoom levels 0..6;
   - zoom-at-cursor preserves the world point within 1e-6;
   - overflow at extreme coordinates returns `None`/error, not a panic.
7. Add property tests with `proptest` (dev-dependency):
   - `world_to_screen(screen_to_world(p)) == p` within tolerance;
   - `tile_of_block` is monotonic and floor-consistent for random blocks;
   - every block in a tile rect maps back to that tile.
8. Document the formulas in the module docs so T16 does not re-derive them.

## Acceptance

```sh
cargo test -p map-core coords::
cargo test -p map-core --test coords_property
cargo clippy -p map-core --all-targets -- -D warnings
```

- All conversion functions are in `map-core`; `map-app` contains no duplicated
  floor-division math.
- Negative coordinates and tile edges are covered by tests.
- Zoom-at-cursor keeps the world point fixed.
- No `as` truncation from f64 to i32 without a checked conversion.

## Tests

- Unit tests in `coords.rs` for the exact edge cases above.
- `map-core/tests/coords_property.rs` with proptest.
- A deterministic table test with a viewport at center `(-1203, 455)`,
  scale 0.5 and step 2.

## Risks / notes

- `i32` block coordinates times a tile size can overflow; use `i64` internally.
- The generator's coordinate limits are much smaller than `i32::MIN/MAX`; use
  the generatable-area bounds from T08 to reject impossible views early.
- Do not let egui's Y-down screen convention leak into world Z. Pick one
  convention and document it: screen Y increases downward, world Z increases
  south; the map widget flips Z when painting.
