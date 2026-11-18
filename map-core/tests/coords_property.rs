//! T10 property tests: coordinate, zoom and tile math.
//!
//! The concrete tables live in the unit tests in `map_core::coords`; this suite
//! hunts for the edges those tables miss — negative coordinates, fractional
//! centers, very small and very large display scales. It checks the invariants
//! the tile cache, scheduler and map widget rely on: screen ↔ world converts
//! back to itself, tile indices are monotonic and floor-consistent, every block
//! of a `tile_rect` maps back to that tile, and `visible_tiles` covers all four
//! viewport corners.
//!
//! Tolerances are never exact: screen coordinates are `f32`, so the tests
//! compare floats within a small absolute margin.

use map_core::coords::{
    BlockPos, ScreenPos, TILE_SIZE, TileCoord, Viewport, WorldPos, ZoomLevel, effective_step,
    floor_div, floor_mod, tile_of_block, tile_rect, visible_tiles,
};
use proptest::prelude::*;

/// Smallest display scale exercised: 0.05 screen points per block (far out).
const MIN_SCALE: f64 = 0.05;
/// Largest display scale exercised: 64 screen points per block (zoomed in).
const MAX_SCALE: f64 = 64.0;
/// Number of steps used to sample the continuous scale range. proptest has no
/// ranged float strategy, so scales come from a fine integer grid instead.
const SCALE_STEPS: u32 = 10_000;

/// Largest viewport side exercised, in screen points.
const MAX_DIMENSION: u16 = 4096;

/// Screen round-trip tolerance in points. At the 4096-point edge an `f32` ulp
/// is about 0.00024, so a hundredth of a point leaves ample headroom.
const SCREEN_TOLERANCE: f32 = 0.01;

/// World round-trip tolerance per unit of `1 / scale`: a screen error of
/// `SCREEN_TOLERANCE` points becomes `SCREEN_TOLERANCE / scale` world blocks.
const WORLD_TOLERANCE: f64 = 0.01;

/// Denominators for the fractional parts of generated coordinates and screen
/// points, so a continuous range can be built from an integer strategy.
const FRACTION_DENOMINATOR: f64 = 10_000.0;

/// A random display scale from [`MIN_SCALE`] to [`MAX_SCALE`], inclusive.
fn scale_strategy() -> impl Strategy<Value = f64> {
    let span = f64::from(SCALE_STEPS);
    (0..=SCALE_STEPS)
        .prop_map(move |step| MIN_SCALE + (MAX_SCALE - MIN_SCALE) * f64::from(step) / span)
}

/// A finite, non-negative viewport side in screen points, up to
/// [`MAX_DIMENSION`]. Zero is legal for a `Viewport`.
fn dimension_strategy() -> impl Strategy<Value = f32> {
    (0u16..=MAX_DIMENSION).prop_map(f32::from)
}

/// A world coordinate around ±1e6 blocks with a fractional part, so negative
/// and non-integer centers are both exercised.
fn world_coord_strategy() -> impl Strategy<Value = f64> {
    (-1_000_000i32..=1_000_000, 0u16..=9_999)
        .prop_map(|(whole, fraction)| f64::from(whole) + f64::from(fraction) / FRACTION_DENOMINATOR)
}

/// A position across the viewport as a fraction of its width or height.
fn screen_fraction_strategy() -> impl Strategy<Value = f32> {
    (0u16..=1_000).prop_map(|step| f32::from(step) / 1_000.0)
}

/// Two block coordinates with the first not greater than the second, spanning
/// negative, zero and positive blocks.
fn ordered_block_pair_strategy() -> impl Strategy<Value = (i32, i32)> {
    (-5_000_000i32..=5_000_000, -5_000_000i32..=5_000_000)
        .prop_map(|(left, right)| (left.min(right), left.max(right)))
}

/// A sampling step as the renderer picks it: the larger of the user's minimum
/// step and the step the zoom level demands. Everything is a power of two.
fn step_strategy() -> impl Strategy<Value = u32> {
    (0u32..=16, 0u8..=8).prop_map(|(user_min_step, level)| {
        let zoom = ZoomLevel::new(level).expect("levels 0..=8 are within ZoomLevel::MIN..=MAX");
        effective_step(user_min_step, zoom)
    })
}

/// The coarse steps the tile-rect property is specified for.
fn tile_rect_step_strategy() -> impl Strategy<Value = u32> {
    prop::sample::select(vec![1u32, 2, 4, 8])
}

/// The number of offset samples taken across one tile side. The tile side is
/// a power of two (256 * step), so this divides it for steps 1, 2, 4 and 8.
const TILE_OFFSET_SAMPLES: u16 = 2048;

proptest! {
    /// A screen point inside the view survives `screen_to_world` followed by
    /// `world_to_screen`.
    #[test]
    fn screen_to_world_then_world_to_screen_is_identity(
        center_x in world_coord_strategy(),
        center_z in world_coord_strategy(),
        scale in scale_strategy(),
        width in dimension_strategy(),
        height in dimension_strategy(),
        fraction_x in screen_fraction_strategy(),
        fraction_y in screen_fraction_strategy(),
    ) {
        let view = Viewport::new(WorldPos::new(center_x, center_z), scale, width, height);
        let point = ScreenPos::new(width * fraction_x, height * fraction_y);
        let back = view.world_to_screen(view.screen_to_world(point));

        prop_assert!(
            (back.x - point.x).abs() <= SCREEN_TOLERANCE,
            "screen x {} came back as {} (scale {scale}, width {width}, center x {center_x})",
            point.x,
            back.x,
        );
        prop_assert!(
            (back.y - point.y).abs() <= SCREEN_TOLERANCE,
            "screen y {} came back as {} (scale {scale}, height {height}, center z {center_z})",
            point.y,
            back.y,
        );
    }

    /// A world point inside the view survives `world_to_screen` followed by
    /// `screen_to_world`. The tolerance is divided by the scale because that is
    /// how a screen-space `f32` rounding error converts into world blocks.
    #[test]
    fn world_to_screen_then_screen_to_world_is_identity(
        center_x in world_coord_strategy(),
        center_z in world_coord_strategy(),
        scale in scale_strategy(),
        width in dimension_strategy(),
        height in dimension_strategy(),
        fraction_x in screen_fraction_strategy(),
        fraction_y in screen_fraction_strategy(),
    ) {
        let view = Viewport::new(WorldPos::new(center_x, center_z), scale, width, height);
        // Place the world point where the given screen fraction lands, which
        // puts it strictly inside the visible world rectangle.
        let world = WorldPos::new(
            center_x + (f64::from(fraction_x) - 0.5) * f64::from(width) / scale,
            center_z + (f64::from(fraction_y) - 0.5) * f64::from(height) / scale,
        );
        let back = view.screen_to_world(view.world_to_screen(world));
        let tolerance = WORLD_TOLERANCE / scale;

        prop_assert!(
            (back.x - world.x).abs() <= tolerance,
            "world x {} came back as {} with tolerance {tolerance} \
             (scale {scale}, center x {center_x})",
            world.x,
            back.x,
        );
        prop_assert!(
            (back.z - world.z).abs() <= tolerance,
            "world z {} came back as {} with tolerance {tolerance} \
             (scale {scale}, center z {center_z})",
            world.z,
            back.z,
        );
    }

    /// Tile indices never decrease as blocks increase, and every block lies in
    /// the half-open interval `[floor_div * tile_blocks, (floor_div + 1) * tile_blocks)`.
    #[test]
    fn tile_of_block_is_monotonic_and_floor_consistent(
        (a_x, b_x) in ordered_block_pair_strategy(),
        (a_z, b_z) in ordered_block_pair_strategy(),
        step in step_strategy(),
    ) {
        let blocks_per_tile = i64::from(TILE_SIZE) * i64::from(step);
        let a = BlockPos::new(a_x, a_z);
        let b = BlockPos::new(b_x, b_z);
        let a_tile = tile_of_block(a, step);
        let b_tile = tile_of_block(b, step);

        prop_assert!(
            a_tile.x <= b_tile.x,
            "tile x went backwards from block {a_x} to {b_x} at step {step}: \
             {} then {}",
            a_tile.x,
            b_tile.x,
        );
        prop_assert!(
            a_tile.z <= b_tile.z,
            "tile z went backwards from block {a_z} to {b_z} at step {step}: \
             {} then {}",
            a_tile.z,
            b_tile.z,
        );

        for coordinate in [a.x, a.z, b.x, b.z] {
            let value = i64::from(coordinate);
            let tile = floor_div(value, blocks_per_tile);
            let lower = tile * blocks_per_tile;
            let upper = (tile + 1) * blocks_per_tile;

            prop_assert!(
                lower <= value && value < upper,
                "floor_div({value}, {blocks_per_tile}) = {tile} must satisfy \
                 {lower} <= {value} < {upper}",
            );

            let remainder = floor_mod(value, blocks_per_tile);
            prop_assert_eq!(
                value,
                lower + remainder,
                "floor_mod must fill the gap to the next tile for {}",
                value,
            );
            prop_assert!(
                (0..blocks_per_tile).contains(&remainder),
                "floor_mod({value}, {blocks_per_tile}) = {remainder} must be \
                 non-negative and below the tile size",
            );
        }
    }

    /// Any block sampled inside a tile's rect maps back to that tile on both
    /// axes. `Rect` max bounds are inclusive, and the offsets reach that max.
    #[test]
    fn every_block_of_a_tile_rect_maps_back_to_its_tile(
        tile_x in -1_000i64..=1_000,
        tile_z in -1_000i64..=1_000,
        step in tile_rect_step_strategy(),
        offset_x in 0u16..=(TILE_OFFSET_SAMPLES - 1),
        offset_z in 0u16..=(TILE_OFFSET_SAMPLES - 1),
    ) {
        let tile = TileCoord::new(tile_x, tile_z);
        let rect = tile_rect(tile, step)
            .expect("tile coords within +/-1000 fit the i32 block space at these steps");
        let blocks_per_tile = i64::from(TILE_SIZE) * i64::from(step);
        let offset_samples = i64::from(TILE_OFFSET_SAMPLES);
        // Fractions span the whole tile side, so the inclusive max block is
        // reachable rather than only the tile origin.
        let block_x =
            i64::from(rect.min_x) + i64::from(offset_x) * blocks_per_tile / offset_samples;
        let block_z =
            i64::from(rect.min_z) + i64::from(offset_z) * blocks_per_tile / offset_samples;
        let block = BlockPos::new(
            i32::try_from(block_x).expect("tile origin plus a within-tile offset stays inside i32"),
            i32::try_from(block_z).expect("tile origin plus a within-tile offset stays inside i32"),
        );

        prop_assert!(
            rect.contains(block.x, block.z),
            "{block:?} must lie inside {rect:?} for tile {tile:?} at step {step}",
        );
        prop_assert_eq!(
            tile_of_block(block, step),
            tile,
            "{:?} at step {} must map back to {:?}",
            block,
            step,
            tile,
        );
    }

    /// Every corner block's tile lies inside the range `visible_tiles` returns.
    #[test]
    fn visible_tiles_covers_all_four_corner_blocks(
        center_x in world_coord_strategy(),
        center_z in world_coord_strategy(),
        scale in scale_strategy(),
        width in dimension_strategy(),
        height in dimension_strategy(),
        step in step_strategy(),
    ) {
        let view = Viewport::new(WorldPos::new(center_x, center_z), scale, width, height);
        let visible = visible_tiles(&view, step).expect(
            "centers within +/-1e6 and scales of at least 0.05 keep every corner inside i32",
        );
        let corners = [
            ScreenPos::new(0.0, 0.0),
            ScreenPos::new(width, 0.0),
            ScreenPos::new(0.0, height),
            ScreenPos::new(width, height),
        ];

        for corner in corners {
            let block = view
                .screen_to_block(corner)
                .expect("a corner of an in-range view lies inside the i32 block space");
            let corner_tile = tile_of_block(block, step);
            prop_assert!(
                visible.contains(corner_tile),
                "corner ({}, {}) is block {block:?}, tile {corner_tile:?}, outside \
                 visible {visible:?} at step {step}",
                corner.x,
                corner.y,
            );
        }
    }
}
