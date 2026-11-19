//! Coordinate, zoom and tile math — the explorer's single source of truth
//! (T10).
//!
//! One canonical mapping shared by the tile cache (T14), the scheduler
//! (T15), the renderers and the map widget (T16). Nobody downstream may
//! re-derive these conversions.
//!
//! # Vocabulary
//!
//! - `step` — blocks per sample, a power of two (1, 2, 4, …). Generation
//!   never produces sub-block samples; zooming closer than one block is done
//!   by scaling the tile *texture*, not by shrinking `step`.
//! - [`ZoomLevel`] — `log2(step)`, so `step = 1 << zoom_level`. Discrete;
//!   the UI interpolates smoothly between levels via the display `scale`.
//! - A tile covers [`TILE_SIZE`]·`step` blocks per side.
//!
//! # Formulas
//!
//! ```text
//! tile        = floor_div(block, TILE_SIZE * step)
//! tile_origin = tile * TILE_SIZE * step
//! screen_x    = (world_x - view.center.x) * view.scale + view.width / 2
//! world_x     = (screen_x - view.width / 2) / view.scale + view.center.x
//! visible tiles = tile range covering the four viewport corners, expanded
//!                 by one tile on every side
//! ```
//!
//! # Conventions
//!
//! - **Y axis:** screen Y increases downward (egui), world Z increases
//!   south. This module applies the same linear mapping to both axes and
//!   never flips Z — so north (−Z) lands at the screen top, exactly the
//!   orientation the renderers bake into their pixels (image row 0).
//!   The map widget (T16) consumes this transform as-is for both
//!   painting and cursor readouts; had a fixup ever been needed, its
//!   home would be that paint boundary, never here.
//! - **Intermediate arithmetic** runs in `i64`; values are narrowed to
//!   `i32` only at the public block boundary, with `None` on overflow.
//! - **Floor semantics:** a screen point maps to the block whose *floor*
//!   covers its world position; block → tile likewise floors, so `-1`
//!   belongs to tile `-1`, not tile `0`.

use serde::{Deserialize, Serialize};

use crate::types::Rect;

/// One side of a tile, in pixels. Frozen by the plan (§5.1): 256×256.
pub const TILE_SIZE: u32 = 256;

// ---------------------------------------------------------------------------
// Floor division
// ---------------------------------------------------------------------------

/// Floor division for signed integers: the quotient rounds toward negative
/// infinity.
///
/// Rust's `/` truncates toward zero, which is *not* floor for negative
/// operands: `(-1) / 256 == 0`, but `floor_div(-1, 256) == -1`. Every
/// block → tile conversion needs the floor behavior, so negative coordinates
/// fall into the tile to their left.
///
/// `divisor` must be non-zero; the helpers in this module only ever pass
/// positive powers of two.
#[must_use]
pub fn floor_div(value: i64, divisor: i64) -> i64 {
    debug_assert!(divisor != 0, "floor_div divisor must be non-zero");
    let quotient = value / divisor;
    let remainder = value % divisor;
    // Truncation already floors when the division is exact or both operands
    // share a sign; only a non-zero remainder across signs needs a fixup.
    if remainder != 0 && (value < 0) != (divisor < 0) {
        quotient - 1
    } else {
        quotient
    }
}

/// Floor modulo, consistent with [`floor_div`]:
/// `value == floor_div(value, divisor) * divisor + floor_mod(value, divisor)`
/// and the result has the divisor's sign (positive for tile sizes).
///
/// `divisor` must be non-zero.
#[must_use]
pub fn floor_mod(value: i64, divisor: i64) -> i64 {
    debug_assert!(divisor != 0, "floor_mod divisor must be non-zero");
    // Written via floor_div so the two can never drift apart, and so no
    // `abs()` overflow edge case exists at `i64::MIN` divisors.
    value - floor_div(value, divisor) * divisor
}

/// Ceiling division for a positive divisor: the smallest `quotient`
/// with `quotient * divisor >= value`. The sign-symmetric complement of
/// [`floor_div`] (`ceil_div(-5, 2) == -2`), used by the tile query
/// planner to find the first sample that reaches into a rect.
///
/// `divisor` must be positive; the helpers in this module only ever
/// pass positive powers of two.
#[must_use]
pub fn ceil_div(value: i64, divisor: i64) -> i64 {
    debug_assert!(divisor > 0, "ceil_div needs a positive divisor");
    (value + divisor - 1).div_euclid(divisor)
}

/// Asserts that `step` is a usable blocks-per-sample value.
fn assert_valid_step(step: u32) {
    assert!(
        step >= 1 && step.is_power_of_two(),
        "step must be a power of two (1, 2, 4, ...), got {step}",
    );
}

/// Blocks per side of one tile at `step`; the unit of all block ↔ tile math.
fn tile_blocks(step: u32) -> i64 {
    assert_valid_step(step);
    i64::from(TILE_SIZE) * i64::from(step)
}

// ---------------------------------------------------------------------------
// Positions
// ---------------------------------------------------------------------------

/// A block column in world XZ space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlockPos {
    pub x: i32,
    pub z: i32,
}

impl BlockPos {
    pub fn new(x: i32, z: i32) -> BlockPos {
        BlockPos { x, z }
    }
}

/// A continuous point in world XZ space (block units, fractional allowed).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WorldPos {
    pub x: f64,
    pub z: f64,
}

impl WorldPos {
    pub fn new(x: f64, z: f64) -> WorldPos {
        WorldPos { x, z }
    }
}

/// A point on the screen, in egui points. Screen Y increases downward.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ScreenPos {
    pub x: f32,
    pub y: f32,
}

impl ScreenPos {
    pub fn new(x: f32, y: f32) -> ScreenPos {
        ScreenPos { x, y }
    }
}

/// A tile's coordinates: `floor_div(block, TILE_SIZE * step)` per axis, so
/// negative blocks get negative tile indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TileCoord {
    pub x: i64,
    pub z: i64,
}

impl TileCoord {
    pub fn new(x: i64, z: i64) -> TileCoord {
        TileCoord { x, z }
    }
}

/// An inclusive range of tiles; both `min` and `max` are covered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TileRect {
    pub min: TileCoord,
    pub max: TileCoord,
}

/// Task vocabulary alias: the tile range returned by [`tiles_in`] and
/// [`visible_tiles`] is a [`TileRect`].
pub type TileRange = TileRect;

impl TileRect {
    /// Creates a tile rect, asserting that the inclusive max coords are not
    /// smaller than the min coords on either axis.
    pub fn new(min: TileCoord, max: TileCoord) -> TileRect {
        assert!(
            max.x >= min.x && max.z >= min.z,
            "TileRect max coords must not be smaller than min coords \
             (max is inclusive): min=({}, {}), max=({}, {})",
            min.x,
            min.z,
            max.x,
            max.z,
        );
        TileRect { min, max }
    }

    /// True when `tile` lies inside the range, inclusive of both bounds.
    pub fn contains(&self, tile: TileCoord) -> bool {
        tile.x >= self.min.x && tile.x <= self.max.x && tile.z >= self.min.z && tile.z <= self.max.z
    }

    /// Grows the range by `by` tiles on all four bounds. A negative `by`
    /// shrinks it (and can invert it via direct field mutation only —
    /// this method asserts nothing, matching [`Rect::expand`]).
    #[must_use]
    pub fn expand(&self, by: i64) -> TileRect {
        TileRect {
            min: TileCoord {
                x: self.min.x - by,
                z: self.min.z - by,
            },
            max: TileCoord {
                x: self.max.x + by,
                z: self.max.z + by,
            },
        }
    }

    /// Iterates every tile in the range, row-major (`z` outer, `x` inner).
    pub fn iter(&self) -> impl Iterator<Item = TileCoord> {
        let min = self.min;
        let max = self.max;
        (min.z..=max.z).flat_map(move |z| (min.x..=max.x).map(move |x| TileCoord { x, z }))
    }
}

// ---------------------------------------------------------------------------
// Zoom
// ---------------------------------------------------------------------------

/// A discrete zoom level: `step = 1 << level` blocks per sample.
///
/// Level 0 is full block (1×1), level 1 the default 2×2, and so on up to
/// [`ZoomLevel::MAX`]. The level selects what the *generator* samples; how
/// large the result is drawn on screen is the separate display
/// [`Viewport::scale`], which may exceed one point per block by scaling the
/// tile texture.
///
/// At [`ZoomLevel::MAX`] (20) a tile spans `256 * 2^20 ≈ 2.7 * 10^8` blocks,
/// still far inside `i64` and comfortably inside the `i32` block space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ZoomLevel(pub u8);

impl ZoomLevel {
    /// Full block resolution: one sample per block.
    pub const MIN: ZoomLevel = ZoomLevel(0);
    /// The highest supported level (step `1 << 20`). Beyond this the world
    /// would be sampled more coarsely than one tile can meaningfully show;
    /// the UI clamps zoom here.
    pub const MAX: ZoomLevel = ZoomLevel(20);

    /// Creates a level, or [`None`] above [`ZoomLevel::MAX`].
    pub fn new(level: u8) -> Option<ZoomLevel> {
        (level <= Self::MAX.0).then_some(ZoomLevel(level))
    }

    /// Blocks per sample at this level: `1 << level`.
    #[must_use]
    pub fn step(self) -> u32 {
        1_u32 << self.0
    }

    /// Blocks per side of one tile at this level.
    #[must_use]
    pub fn tile_blocks(self) -> i64 {
        i64::from(TILE_SIZE) * i64::from(self.step())
    }

    /// The *native* texture resolution: pixels per block before any display
    /// scaling. A 256-pixel tile covers `256 * step` blocks, so this is
    /// `1 / step`. The on-screen value is [`Viewport::scale`], which the
    /// widget may set larger by scaling the texture.
    #[must_use]
    pub fn pixels_per_block(self) -> f64 {
        1.0 / f64::from(self.step())
    }

    /// The level whose step is exactly `step`: [`None`] when `step` is not
    /// a power of two or lies outside the supported range.
    pub fn from_step(step: u32) -> Option<ZoomLevel> {
        if step >= 1 && step.is_power_of_two() && step <= Self::MAX.step() {
            Some(ZoomLevel(step.trailing_zeros() as u8))
        } else {
            None
        }
    }
}

/// The sampling step the renderer must use:
/// `max(round_up_to_power_of_two(user_min_step), step_needed_for_zoom)`.
///
/// `user_min_step` is the coarsest sampling the user accepts (0 counts as
/// 1 — a UI slider can produce it); the zoom level demands at least
/// `1 << zoom_level`. The renderer picks the larger so a zoomed-out view
/// never generates more samples than the user asked to skip.
#[must_use]
pub fn effective_step(user_min_step: u32, zoom_level: ZoomLevel) -> u32 {
    let user = user_min_step.max(1).next_power_of_two();
    user.max(zoom_level.step())
}

// ---------------------------------------------------------------------------
// Block ↔ tile conversions
// ---------------------------------------------------------------------------

/// The tile containing the block column, per axis:
/// `floor_div(block, TILE_SIZE * step)`.
#[must_use]
pub fn tile_of_block(block: BlockPos, step: u32) -> TileCoord {
    let blocks = tile_blocks(step);
    TileCoord {
        x: floor_div(i64::from(block.x), blocks),
        z: floor_div(i64::from(block.z), blocks),
    }
}

/// The inclusive block bounds covered by `tile` at `step`
/// (`tile * TILE_SIZE * step` … plus one tile side, minus one).
///
/// Returns [`None`] when the block bounds do not fit `i32` — the caller
/// treats such a tile as outside the representable world.
#[must_use]
pub fn tile_rect(tile: TileCoord, step: u32) -> Option<Rect> {
    let blocks = tile_blocks(step);
    let min_x = tile
        .x
        .checked_mul(blocks)
        .and_then(|v| i32::try_from(v).ok())?;
    let min_z = tile
        .z
        .checked_mul(blocks)
        .and_then(|v| i32::try_from(v).ok())?;
    // A tile side fits i32 whenever its origin does: 256 * step <= 2^28
    // blocks, and origins inside i32 leave at least 2^31 - 2^28 >= 0 headroom
    // for the inclusive max. Checked anyway so the invariant is local.
    let max_x = i64::from(min_x)
        .checked_add(blocks - 1)
        .and_then(|v| i32::try_from(v).ok())?;
    let max_z = i64::from(min_z)
        .checked_add(blocks - 1)
        .and_then(|v| i32::try_from(v).ok())?;
    Some(Rect::new(min_x, min_z, max_x, max_z))
}

/// The inclusive range of tiles that contain at least one block of `rect`.
#[must_use]
pub fn tiles_in(rect: &Rect, step: u32) -> TileRect {
    let blocks = tile_blocks(step);
    TileRect {
        min: TileCoord {
            x: floor_div(i64::from(rect.min_x), blocks),
            z: floor_div(i64::from(rect.min_z), blocks),
        },
        max: TileCoord {
            x: floor_div(i64::from(rect.max_x), blocks),
            z: floor_div(i64::from(rect.max_z), blocks),
        },
    }
}

/// The tiles the viewport needs at `step`: the range covering all four
/// corners, expanded by one tile on every side (so the scheduler (T15) has
/// a little slack while panning).
///
/// Returns [`None`] when a corner falls outside the `i32` block space — the
/// caller (T16) clamps the viewport or zoom instead of rendering a range
/// that cannot exist.
#[must_use]
pub fn visible_tiles(view: &Viewport, step: u32) -> Option<TileRect> {
    // scale > 0 makes both axes monotonic, so the top-left corner carries
    // the minima and the bottom-right corner the maxima.
    let min = view.screen_to_world(ScreenPos::new(0.0, 0.0));
    let max = view.screen_to_world(ScreenPos::new(view.width, view.height));
    let rect = Rect::new(
        floor_to_i32(min.x)?,
        floor_to_i32(min.z)?,
        floor_to_i32(max.x)?,
        floor_to_i32(max.z)?,
    );
    Some(tiles_in(&rect, step).expand(1))
}

// ---------------------------------------------------------------------------
// Viewport: screen ↔ world
// ---------------------------------------------------------------------------

/// The map view: what the widget shows, in plain numbers so `map-core`
/// stays headless. `map-app` converts to/from `egui::Rect` at the boundary.
///
/// Invariants (asserted by [`Viewport::new`]): `scale` is positive and
/// finite; `width`/`height` are finite and non-negative. Hand-built
/// structs that violate them get infinities/NaNs out of the conversions,
/// not panics.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Viewport {
    /// World coordinates at the viewport center (block units).
    pub center: WorldPos,
    /// Display scale: screen points per block. Independent of the discrete
    /// sampling `step`; may exceed 1 (zooming in scales the texture).
    pub scale: f64,
    /// Screen width in points.
    pub width: f32,
    /// Screen height in points.
    pub height: f32,
}

impl Viewport {
    /// Creates a viewport, asserting the documented invariants.
    pub fn new(center: WorldPos, scale: f64, width: f32, height: f32) -> Viewport {
        assert!(
            scale > 0.0 && scale.is_finite(),
            "viewport scale must be positive and finite, got {scale}",
        );
        assert!(
            width >= 0.0 && height >= 0.0 && width.is_finite() && height.is_finite(),
            "viewport size must be finite and non-negative, got {width} x {height}",
        );
        Viewport {
            center,
            scale,
            width,
            height,
        }
    }

    /// The world position under a screen point:
    /// `(screen - size / 2) / scale + center`.
    #[must_use]
    pub fn screen_to_world(&self, point: ScreenPos) -> WorldPos {
        WorldPos {
            x: (f64::from(point.x) - f64::from(self.width) / 2.0) / self.scale + self.center.x,
            z: (f64::from(point.y) - f64::from(self.height) / 2.0) / self.scale + self.center.z,
        }
    }

    /// The screen point over a world position:
    /// `(world - center) * scale + size / 2`.
    #[must_use]
    pub fn world_to_screen(&self, world: WorldPos) -> ScreenPos {
        ScreenPos {
            x: ((world.x - self.center.x) * self.scale + f64::from(self.width) / 2.0) as f32,
            y: ((world.z - self.center.z) * self.scale + f64::from(self.height) / 2.0) as f32,
        }
    }

    /// The block column under a screen point: the floor of its world
    /// position. [`None`] when the position is non-finite or outside the
    /// `i32` block space.
    #[must_use]
    pub fn screen_to_block(&self, point: ScreenPos) -> Option<BlockPos> {
        let world = self.screen_to_world(point);
        Some(BlockPos {
            x: floor_to_i32(world.x)?,
            z: floor_to_i32(world.z)?,
        })
    }

    /// The screen point over a block column's origin corner.
    #[must_use]
    pub fn block_to_screen(&self, block: BlockPos) -> ScreenPos {
        self.world_to_screen(WorldPos {
            x: f64::from(block.x),
            z: f64::from(block.z),
        })
    }

    /// The new center that keeps the world point under `cursor` fixed when
    /// the display scale changes to `new_scale` (the wheel-zoom anchor).
    ///
    /// `new_center = world_under_cursor - (cursor - screen_center) / new_scale`.
    /// This is the most error-prone piece of UI math, so it lives here and
    /// is tested headlessly rather than in the widget.
    #[must_use]
    pub fn zoom_at_cursor(&self, cursor: ScreenPos, new_scale: f64) -> WorldPos {
        assert!(
            new_scale > 0.0 && new_scale.is_finite(),
            "new scale must be positive and finite, got {new_scale}",
        );
        let world = self.screen_to_world(cursor);
        WorldPos {
            x: world.x - (f64::from(cursor.x) - f64::from(self.width) / 2.0) / new_scale,
            z: world.z - (f64::from(cursor.y) - f64::from(self.height) / 2.0) / new_scale,
        }
    }
}

/// Floors a world coordinate into an `i32` block coordinate, or [`None`]
/// when it is non-finite or outside the `i32` range.
fn floor_to_i32(world: f64) -> Option<i32> {
    if !world.is_finite() {
        return None;
    }
    let floored = world.floor();
    if floored < f64::from(i32::MIN) || floored > f64::from(i32::MAX) {
        return None;
    }
    // Bounds-checked above; the cast of an in-range integral f64 is exact.
    Some(floored as i32)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(a: f64, b: f64) {
        assert!(
            (a - b).abs() < 1e-6,
            "expected {a} and {b} to agree within 1e-6",
        );
    }

    // -- floor_div / floor_mod ----------------------------------------------

    #[test]
    fn floor_div_floors_negative_values() {
        // The exact cases from the task, plus the positive mirror image.
        assert_eq!(floor_div(-1, 256), -1);
        assert_eq!(floor_div(-256, 256), -1);
        assert_eq!(floor_div(-257, 256), -2);
        assert_eq!(floor_div(0, 256), 0);
        assert_eq!(floor_div(1, 256), 0);
        assert_eq!(floor_div(255, 256), 0);
        assert_eq!(floor_div(256, 256), 1);
        assert_eq!(floor_div(-512, 256), -2);
    }

    #[test]
    fn floor_div_respects_divisor_sign() {
        // Same magnitudes with a negative divisor (never used by the tile
        // helpers, but the function is public arithmetic). floor(-1 / -256)
        // is floor(0.0039) = 0.
        assert_eq!(floor_div(-1, -256), 0);
        assert_eq!(floor_div(1, -256), -1);
        assert_eq!(floor_div(256, -256), -1);
        assert_eq!(floor_div(257, -256), -2);
    }

    #[test]
    fn floor_mod_is_positive_for_positive_divisors() {
        assert_eq!(floor_mod(-1, 256), 255);
        assert_eq!(floor_mod(-257, 256), 255);
        assert_eq!(floor_mod(-256, 256), 0);
        assert_eq!(floor_mod(257, 256), 1);
        assert_eq!(floor_mod(0, 256), 0);
        // The defining identity, including negative values.
        for value in [-1000, -256, -255, -1, 0, 1, 255, 256, 257, 1000] {
            let q = floor_div(value, 256);
            let r = floor_mod(value, 256);
            assert_eq!(value, q * 256 + r, "floor identity broken for {value}");
            assert!(r >= 0, "remainder must share the divisor's sign");
        }
    }

    // -- ZoomLevel ----------------------------------------------------------

    #[test]
    fn zoom_level_step_and_tile_blocks_are_powers_of_two() {
        assert_eq!(ZoomLevel::MIN.step(), 1);
        assert_eq!(ZoomLevel::new(1).expect("in range").step(), 2);
        assert_eq!(ZoomLevel::new(2).expect("in range").step(), 4);
        assert_eq!(ZoomLevel::MAX.step(), 1 << 20);
        assert_eq!(ZoomLevel::MIN.tile_blocks(), 256);
        assert_eq!(ZoomLevel::new(2).expect("in range").tile_blocks(), 1024);
        assert_eq!(
            ZoomLevel::MAX.tile_blocks(),
            i64::from(TILE_SIZE) * (1 << 20),
        );
    }

    #[test]
    fn zoom_level_new_rejects_out_of_range() {
        assert_eq!(ZoomLevel::new(0), Some(ZoomLevel::MIN));
        assert_eq!(ZoomLevel::new(20), Some(ZoomLevel::MAX));
        assert_eq!(ZoomLevel::new(21), None);
        assert_eq!(ZoomLevel::new(255), None);
    }

    #[test]
    fn zoom_level_from_step_round_trips_through_step() {
        for level in 0..=ZoomLevel::MAX.0 {
            let zoom = ZoomLevel::new(level).expect("in range");
            assert_eq!(ZoomLevel::from_step(zoom.step()), Some(zoom));
        }
        // Not a power of two, zero, or beyond the range.
        assert_eq!(ZoomLevel::from_step(3), None);
        assert_eq!(ZoomLevel::from_step(0), None);
        assert_eq!(ZoomLevel::from_step(1 << 21), None);
        assert_eq!(ZoomLevel::from_step(u32::MAX), None);
    }

    #[test]
    fn zoom_level_pixels_per_block_is_native_texture_scale() {
        assert_close(ZoomLevel::MIN.pixels_per_block(), 1.0);
        assert_close(ZoomLevel::new(1).expect("in range").pixels_per_block(), 0.5);
        assert_close(
            ZoomLevel::new(2).expect("in range").pixels_per_block(),
            0.25,
        );
    }

    #[test]
    fn effective_step_takes_the_max_of_user_and_zoom() {
        // The table from the task: user steps 1, 2 and 4 across zoom 0..=6.
        let expected_user_1 = [1, 2, 4, 8, 16, 32, 64];
        let expected_user_2 = [2, 2, 4, 8, 16, 32, 64];
        let expected_user_4 = [4, 4, 4, 8, 16, 32, 64];
        for level in 0..=6u8 {
            let zoom = ZoomLevel::new(level).expect("in range");
            let index = usize::from(level);
            assert_eq!(
                effective_step(1, zoom),
                expected_user_1[index],
                "user step 1 at zoom {level}",
            );
            assert_eq!(
                effective_step(2, zoom),
                expected_user_2[index],
                "user step 2 at zoom {level}",
            );
            assert_eq!(
                effective_step(4, zoom),
                expected_user_4[index],
                "user step 4 at zoom {level}",
            );
        }
        // Non-power-of-two user steps round up; zero counts as one.
        assert_eq!(
            effective_step(3, ZoomLevel::MIN),
            4,
            "3 rounds up to the next power of two",
        );
        assert_eq!(
            effective_step(5, ZoomLevel::MIN),
            8,
            "5 rounds up to the next power of two",
        );
        assert_eq!(effective_step(0, ZoomLevel::MIN), 1);
        // The user minimum wins over the zoom requirement and vice versa.
        assert_eq!(effective_step(16, ZoomLevel::new(1).expect("in range")), 16);
        assert_eq!(effective_step(1, ZoomLevel::new(4).expect("in range")), 16,);
    }

    // -- block ↔ tile -------------------------------------------------------

    #[test]
    fn tile_of_block_handles_tile_edges_and_negatives() {
        // Step 1: a tile is exactly 256 blocks.
        for (block, tile_x) in [
            (-257, -2),
            (-256, -1),
            (-1, -1),
            (0, 0),
            (255, 0),
            (256, 1),
            (511, 1),
            (512, 2),
        ] {
            assert_eq!(
                tile_of_block(BlockPos::new(block, 0), 1).x,
                tile_x,
                "block {block} at step 1",
            );
        }
        // Step 2: a tile covers 512 blocks, so the edges shift.
        for (block, tile_x) in [(-513, -2), (-512, -1), (-1, -1), (0, 0), (511, 0), (512, 1)] {
            assert_eq!(
                tile_of_block(BlockPos::new(block, 0), 2).x,
                tile_x,
                "block {block} at step 2",
            );
        }
        // Z mirrors X.
        assert_eq!(tile_of_block(BlockPos::new(0, -257), 1).z, -2);
        assert_eq!(tile_of_block(BlockPos::new(0, 255), 1).z, 0);
        assert_eq!(tile_of_block(BlockPos::new(0, 256), 1).z, 1);
    }

    #[test]
    #[should_panic(expected = "step must be a power of two")]
    fn tile_of_block_rejects_step_zero() {
        let _ = tile_of_block(BlockPos::new(0, 0), 0);
    }

    #[test]
    #[should_panic(expected = "step must be a power of two")]
    fn tile_of_block_rejects_non_power_of_two_steps() {
        let _ = tile_of_block(BlockPos::new(0, 0), 3);
    }

    #[test]
    fn tile_rect_covers_inclusive_negative_bounds() {
        // Tile 0 at step 1: blocks 0..=255.
        assert_eq!(
            tile_rect(TileCoord::new(0, 0), 1),
            Some(Rect::new(0, 0, 255, 255)),
        );
        // Tile -1 at step 1: blocks -256..=-1.
        assert_eq!(
            tile_rect(TileCoord::new(-1, 0), 1),
            Some(Rect::new(-256, 0, -1, 255)),
        );
        // Tile (-1, -1) at step 2: blocks -512..=-1 on both axes.
        assert_eq!(
            tile_rect(TileCoord::new(-1, -1), 2),
            Some(Rect::new(-512, -512, -1, -1)),
        );
        // Tile (1, 2) at step 2: X 512..=1023, Z 1024..=1535.
        assert_eq!(
            tile_rect(TileCoord::new(1, 2), 2),
            Some(Rect::new(512, 1024, 1023, 1535)),
        );
    }

    #[test]
    fn tile_rect_reaches_the_i32_boundary_exactly() {
        // The last tile whose bounds fit i32 at step 1: origin
        // 8388607 * 256 = 2147483392, max + 255 = i32::MAX.
        assert_eq!(
            tile_rect(TileCoord::new(8_388_607, 8_388_607), 1),
            Some(Rect::new(2_147_483_392, 2_147_483_392, i32::MAX, i32::MAX)),
        );
        // One tile further overflows; likewise a step-4 tile that far out.
        assert_eq!(tile_rect(TileCoord::new(8_388_608, 0), 1), None);
        assert_eq!(
            tile_rect(TileCoord::new(1 << 23, 0), 4),
            None,
            "2^23 * 1024 blocks overflows i32",
        );
    }

    #[test]
    fn tiles_in_straddles_the_zero_boundary() {
        // Blocks -10..=10 at step 1 span tiles -1..=0 on both axes.
        assert_eq!(
            tiles_in(&Rect::new(-10, -10, 10, 10), 1),
            TileRect::new(TileCoord::new(-1, -1), TileCoord::new(0, 0)),
        );
        // At step 2 (512-block tiles) the same rect spans tiles -1..=0.
        assert_eq!(
            tiles_in(&Rect::new(-10, -10, 10, 10), 2),
            TileRect::new(TileCoord::new(-1, -1), TileCoord::new(0, 0)),
        );
        // A rect inside one positive tile at step 2 (512-block tiles).
        assert_eq!(
            tiles_in(&Rect::new(600, 700, 700, 900), 2),
            TileRect::new(TileCoord::new(1, 1), TileCoord::new(1, 1)),
        );
        // The full i32 block space at step 1: tiles -8388608..=8388607.
        let full = tiles_in(&Rect::new(i32::MIN, i32::MIN, i32::MAX, i32::MAX), 1);
        assert_eq!(full.min, TileCoord::new(-8_388_608, -8_388_608));
        assert_eq!(full.max, TileCoord::new(8_388_607, 8_388_607));
    }

    #[test]
    fn tile_rect_iterates_row_major() {
        let range = TileRect::new(TileCoord::new(-1, -1), TileCoord::new(1, 0));
        let tiles: Vec<TileCoord> = range.iter().collect();
        assert_eq!(
            tiles,
            vec![
                TileCoord::new(-1, -1),
                TileCoord::new(0, -1),
                TileCoord::new(1, -1),
                TileCoord::new(-1, 0),
                TileCoord::new(0, 0),
                TileCoord::new(1, 0),
            ],
        );
        assert!(range.contains(TileCoord::new(0, 0)));
        assert!(!range.contains(TileCoord::new(2, 0)));
    }

    // -- screen ↔ world -----------------------------------------------------

    #[test]
    fn screen_world_round_trips_across_zooms_and_negative_centers() {
        // Several display scales (including sub-block and super-block) and
        // negative centers. Screen coordinates pass through f32, so the
        // tolerance accounts for single-precision rounding near the edges.
        for scale in [0.25, 0.5, 1.0, 2.0, 8.0] {
            for (center_x, center_z) in [(-1203.0, 455.0), (0.0, 0.0), (250_000.0, -750_000.0)] {
                let view = Viewport::new(WorldPos::new(center_x, center_z), scale, 1024.0, 768.0);
                for (sx, sy) in [(0.0, 0.0), (13.5, 761.25), (512.0, 384.0), (1023.5, 767.5)] {
                    let world = view.screen_to_world(ScreenPos::new(sx, sy));
                    let back = view.world_to_screen(world);
                    assert!(
                        (back.x - sx).abs() < 0.01 && (back.y - sy).abs() < 0.01,
                        "screen ({sx}, {sy}) round-tripped to ({}, {}), \
                         scale {scale}, center ({center_x}, {center_z})",
                        back.x,
                        back.y,
                    );
                }
            }
        }
    }

    #[test]
    fn screen_world_formulas_match_the_module_docs() {
        let view = Viewport::new(WorldPos::new(-1203.0, 455.0), 0.5, 1024.0, 768.0);
        // The center maps to the screen center.
        let center_screen = view.world_to_screen(view.center);
        assert_eq!(center_screen, ScreenPos::new(512.0, 384.0));
        // The top-left corner: (0 - 512) / 0.5 + (-1203) = -2227.
        let top_left = view.screen_to_world(ScreenPos::new(0.0, 0.0));
        assert_close(top_left.x, -2227.0);
        assert_close(top_left.z, -313.0);
        // The bottom-right corner: (1024 - 512) / 0.5 + (-1203) = -179.
        let bottom_right = view.screen_to_world(ScreenPos::new(1024.0, 768.0));
        assert_close(bottom_right.x, -179.0);
        assert_close(bottom_right.z, 1223.0);
    }

    #[test]
    fn screen_to_block_floors_and_reports_overflow() {
        let view = Viewport::new(WorldPos::new(-1203.0, 455.0), 1.0, 1024.0, 768.0);
        // Screen center is exactly block (-1203, 455): the floor of the
        // world point. One point left is inside the same block; a point
        // half a block below the block origin still floors to it.
        assert_eq!(
            view.screen_to_block(ScreenPos::new(512.0, 384.0)),
            Some(BlockPos::new(-1203, 455)),
        );
        assert_eq!(
            view.screen_to_block(ScreenPos::new(512.5, 384.4)),
            Some(BlockPos::new(-1203, 455)),
        );
        // -0.25 world floors to -1, exercising the negative floor edge.
        let fractional = Viewport::new(WorldPos::new(-0.25, -0.25), 1.0, 2.0, 2.0);
        assert_eq!(
            fractional.screen_to_block(ScreenPos::new(1.0, 1.0)),
            Some(BlockPos::new(-1, -1)),
        );
        // A tiny scale: only the screen center is still inside the i32
        // block space; every corner is far outside it.
        let far_out = Viewport::new(WorldPos::new(0.0, 0.0), 1e-7, 1000.0, 1000.0);
        assert_eq!(
            far_out.screen_to_block(ScreenPos::new(500.0, 500.0)),
            Some(BlockPos::new(0, 0)),
        );
        assert_eq!(far_out.screen_to_block(ScreenPos::new(0.0, 0.0)), None);
        // A non-finite center is reported, not panicked on.
        let nan_center = Viewport {
            center: WorldPos::new(f64::NAN, 0.0),
            scale: 1.0,
            width: 100.0,
            height: 100.0,
        };
        assert_eq!(nan_center.screen_to_block(ScreenPos::new(0.0, 0.0)), None);
    }

    #[test]
    fn block_to_screen_agrees_with_world_to_screen() {
        let view = Viewport::new(WorldPos::new(-1203.0, 455.0), 0.5, 1024.0, 768.0);
        let block = BlockPos::new(-1203, 455);
        assert_eq!(
            view.block_to_screen(block),
            view.world_to_screen(WorldPos::new(-1203.0, 455.0)),
        );
    }

    // -- zoom at cursor ------------------------------------------------------

    #[test]
    fn zoom_at_cursor_keeps_the_world_point_fixed() {
        let view = Viewport::new(WorldPos::new(100.0, -50.0), 2.0, 800.0, 600.0);
        let cursor = ScreenPos::new(123.0, 321.0);
        let world_under = view.screen_to_world(cursor);

        for new_scale in [0.5, 1.0, 4.0, 8.0] {
            let new_center = view.zoom_at_cursor(cursor, new_scale);
            let new_view = Viewport::new(new_center, new_scale, 800.0, 600.0);
            let still_under = new_view.screen_to_world(cursor);
            assert_close(still_under.x, world_under.x);
            assert_close(still_under.z, world_under.z);
        }

        // The exact hand-computed anchor from the module docs' formulas:
        // world (-38.5, -39.5), new center (-3.875, -42.125) at scale 8.
        let new_center = view.zoom_at_cursor(cursor, 8.0);
        assert_close(new_center.x, -3.875);
        assert_close(new_center.z, -42.125);

        // A cursor at the screen center keeps the world center unchanged.
        let center_cursor = ScreenPos::new(400.0, 300.0);
        let kept = view.zoom_at_cursor(center_cursor, 8.0);
        assert_close(kept.x, view.center.x);
        assert_close(kept.z, view.center.z);
    }

    #[test]
    fn zoom_at_cursor_at_negative_centers_and_corners() {
        let view = Viewport::new(WorldPos::new(-1203.0, 455.0), 0.5, 1024.0, 768.0);
        for cursor in [
            ScreenPos::new(0.0, 0.0),
            ScreenPos::new(1024.0, 0.0),
            ScreenPos::new(0.0, 768.0),
            ScreenPos::new(1024.0, 768.0),
        ] {
            let world_under = view.screen_to_world(cursor);
            for new_scale in [0.125, 0.25, 1.0, 2.0] {
                let new_view = Viewport::new(
                    view.zoom_at_cursor(cursor, new_scale),
                    new_scale,
                    1024.0,
                    768.0,
                );
                let still_under = new_view.screen_to_world(cursor);
                assert_close(still_under.x, world_under.x);
                assert_close(still_under.z, world_under.z);
            }
        }
    }

    // -- visible tiles -------------------------------------------------------

    #[test]
    fn visible_tiles_deterministic_table() {
        // The task's fixed table: center (-1203, 455), scale 0.5, step 2,
        // on a 1024 x 768 screen.
        let view = Viewport::new(WorldPos::new(-1203.0, 455.0), 0.5, 1024.0, 768.0);
        let visible = visible_tiles(&view, 2).expect("the view fits the i32 block space");

        // Corners map to blocks (-2227, -313) and (-179, 1223); at step 2 a
        // tile is 512 blocks, so the covering range is tiles (-5, -1)..(-1, 2)
        // before the one-tile expansion.
        assert_eq!(visible.min, TileCoord::new(-6, -2));
        assert_eq!(visible.max, TileCoord::new(0, 3));
        assert_eq!(visible.iter().count(), 7 * 6, "7 x 6 tiles after expansion");
        // Every corner block's tile is inside the (unexpanded) covering
        // range, hence inside the expanded result.
        for corner in [
            BlockPos::new(-2227, -313),
            BlockPos::new(-179, -313),
            BlockPos::new(-2227, 1223),
            BlockPos::new(-179, 1223),
        ] {
            assert!(visible.contains(tile_of_block(corner, 2)));
        }
    }

    #[test]
    fn visible_tiles_small_view_expands_by_one_tile() {
        // A view sitting entirely inside one step-1 tile still yields the
        // 3 x 3 neighborhood around it.
        let view = Viewport::new(WorldPos::new(128.0, 128.0), 1.0, 8.0, 8.0);
        let visible = visible_tiles(&view, 1).expect("small view");
        assert_eq!(visible.min, TileCoord::new(-1, -1));
        assert_eq!(visible.max, TileCoord::new(1, 1));
        assert_eq!(visible.iter().count(), 9);
    }

    #[test]
    fn visible_tiles_straddling_boundaries() {
        // A view centered exactly on a tile boundary covers two tiles per
        // axis (blocks -256..=256 at step 1), expanded to five per axis.
        let view = Viewport::new(WorldPos::new(0.0, 0.0), 0.5, 256.0, 256.0);
        let visible = visible_tiles(&view, 1).expect("the view fits the block space");
        assert_eq!(visible.min, TileCoord::new(-2, -2));
        assert_eq!(visible.max, TileCoord::new(2, 2));
        assert_eq!(visible.iter().count(), 25);
    }

    #[test]
    fn visible_tiles_returns_none_outside_the_block_space() {
        // An absurdly zoomed-out view exceeds i32 blocks at the corners.
        let far_out = Viewport::new(WorldPos::new(0.0, 0.0), 1e-7, 1000.0, 1000.0);
        assert_eq!(visible_tiles(&far_out, 1), None);
        // A view centered on the i32 edge also overflows at its far corner.
        let edge = Viewport::new(WorldPos::new(f64::from(i32::MAX), 0.0), 1.0, 100.0, 100.0);
        assert_eq!(visible_tiles(&edge, 1), None);
    }
}
