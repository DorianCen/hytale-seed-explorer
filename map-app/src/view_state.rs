//! Headless pan/zoom view state (T16): the interactive half of the T10
//! viewport math, with no egui types anywhere.
//!
//! The tile zoom level stays **discrete**, exactly as the plan requires
//! ("zoom levels are discrete steps with smooth interpolation between them in
//! the UI"): the wheel drives a continuous `smooth_zoom` in `ZoomLevel` units,
//! [`MapViewState::zoom_level`] samples it with `ceil`, and painting scales
//! the tile texture by `2^-smooth_zoom` so motion between levels looks
//! continuous. Nothing is ever generated at a half level — the sampling cost
//! is bounded by the discrete level, the smoothness comes from the texture.
//!
//! # Why `ceil` samples the level
//!
//! The display scale is `2^-smooth_zoom` points per block, so one point covers
//! `2^smooth_zoom` blocks. Sampling finer than one point would be wasted work;
//! `ceil` is the level whose native scale is the finest one not finer than the
//! display, which is the plan's "only compute at zoom levels where the user can
//! actually see that detail". At exactly `1.0` point per block the full-block
//! level is exact.
//!
//! # Conversion authority
//!
//! Every screen ↔ world and tile ↔ block conversion delegates to
//! [`map_core::coords`]. This module holds state and policy only — a second
//! copy of the formulas would drift from the one the renderers and the tile
//! pipeline use (T16 shares the helper, never a copy).

use map_core::coords::{
    self, BlockPos, ScreenPos, TileCoord, TileRect, Viewport, WorldPos, ZoomLevel,
};

/// The continuous zoom range in level units: `MIN_SMOOTH_ZOOM` is one
/// point per block (the finest level), `MAX_SMOOTH_ZOOM` the coarsest
/// tile level T10 supports.
pub const MIN_SMOOTH_ZOOM: f32 = 0.0;
pub const MAX_SMOOTH_ZOOM: f32 = ZoomLevel::MAX.0 as f32;

/// The interactive view: where the map is centered, how far it is zoomed and
/// how much room the widget got this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MapViewState {
    center: WorldPos,
    smooth_zoom: f32,
    size: (f32, f32),
}

impl MapViewState {
    /// The plan's initial view: origin-centered at one point per block.
    #[must_use]
    pub fn new() -> MapViewState {
        MapViewState {
            center: WorldPos::new(0.0, 0.0),
            smooth_zoom: MIN_SMOOTH_ZOOM,
            size: (0.0, 0.0),
        }
    }

    /// The world position at the viewport center.
    #[must_use]
    pub fn center(&self) -> WorldPos {
        self.center
    }

    /// Centers the view on a world position, keeping the zoom (go-to,
    /// T18). Asserts finiteness — a center the UI cannot produce is a
    /// bug, not an input.
    pub fn set_center(&mut self, x: f64, z: f64) {
        assert!(
            x.is_finite() && z.is_finite(),
            "view center must be finite, got ({x}, {z})",
        );
        self.center = WorldPos::new(x, z);
    }

    /// The continuous zoom in level units (see the module docs).
    #[allow(dead_code)] // reserved: T31 share strings encode the view
    #[must_use]
    pub fn smooth_zoom(&self) -> f32 {
        self.smooth_zoom
    }

    /// The display scale in points per block: `2^-smooth_zoom`.
    #[must_use]
    pub fn scale(&self) -> f64 {
        2.0f64.powf(-f64::from(self.smooth_zoom))
    }

    /// Sets the viewport size. Panics (assert, with a message) on
    /// non-finite or negative sizes — the same invariant
    /// [`map_core::coords::Viewport::new`] asserts.
    pub fn set_size(&mut self, width: f32, height: f32) {
        assert!(
            width >= 0.0 && height >= 0.0 && width.is_finite() && height.is_finite(),
            "view size must be finite and non-negative, got {width} x {height}",
        );
        self.size = (width, height);
    }

    /// The discrete tile level the display needs: `ceil(smooth_zoom)`
    /// clamped into [`ZoomLevel`]'s range.
    ///
    /// WHY ceil: the display scale is `2^-smooth_zoom`, so one point covers
    /// `2^smooth_zoom` blocks; sampling finer than one point would be wasted
    /// work, and `ceil` is the level whose native scale is the finest one not
    /// finer than the display (the plan's "only compute at zoom levels where
    /// the user can actually see that detail"). At exactly `1.0` point per
    /// block the full-block level is exact.
    #[must_use]
    pub fn zoom_level(&self) -> ZoomLevel {
        let clamped = self
            .smooth_zoom
            .ceil()
            .clamp(f32::from(ZoomLevel::MIN.0), f32::from(ZoomLevel::MAX.0));
        // The clamp above keeps the value inside 0..=20, so the narrowing cast
        // cannot wrap; `ZoomLevel::new` then re-checks the range locally.
        ZoomLevel::new(clamped as u8)
            .expect("zoom_level does not exceed ZoomLevel::MAX after clamping")
    }

    /// The sampling step tiles need for this view: T10's
    /// [`coords::effective_step`] — the user's resolution selector is a
    /// minimum, the zoom may demand coarser.
    #[must_use]
    pub fn tile_step(&self, user_min_step: u32) -> u32 {
        coords::effective_step(user_min_step, self.zoom_level())
    }

    /// The T10 viewport for this state (the single conversion authority —
    /// nobody builds one by hand).
    #[must_use]
    pub fn viewport(&self) -> Viewport {
        Viewport::new(self.center, self.scale(), self.size.0, self.size.1)
    }

    /// Pans by a screen delta in points: the world moves with the pointer, so
    /// the center shifts OPPOSITE the delta, by `delta / scale` blocks.
    pub fn pan_by_screen(&mut self, delta: (f32, f32)) {
        let scale = self.scale();
        self.center = WorldPos::new(
            self.center.x - f64::from(delta.0) / scale,
            self.center.z - f64::from(delta.1) / scale,
        );
    }

    /// Zooms by `level_delta` continuous levels, keeping the world point under
    /// `cursor` (viewport-local points) fixed — T10's
    /// [`Viewport::zoom_at_cursor`] with the new scale.
    ///
    /// The zoom clamps into `[MIN_SMOOTH_ZOOM, MAX_SMOOTH_ZOOM]`; a delta that
    /// changes nothing (already clamped, zero, NaN or a non-finite cursor)
    /// leaves the state untouched.
    pub fn zoom_at_screen(&mut self, cursor: (f32, f32), level_delta: f32) {
        // NaN arrives with a real pointer near a corner or a wheel event with
        // no position; clamping a NaN yields NaN, so the finite check below is
        // what keeps the anchor math out of the garbage path.
        let new_smooth = (self.smooth_zoom + level_delta).clamp(MIN_SMOOTH_ZOOM, MAX_SMOOTH_ZOOM);
        if !new_smooth.is_finite() || !cursor.0.is_finite() || !cursor.1.is_finite() {
            return;
        }
        if new_smooth == self.smooth_zoom {
            return;
        }
        let new_scale = 2.0f64.powf(-f64::from(new_smooth));
        self.center = self
            .viewport()
            .zoom_at_cursor(ScreenPos::new(cursor.0, cursor.1), new_scale);
        self.smooth_zoom = new_smooth;
    }

    /// Zooms by `level_delta` keeping the viewport center fixed
    /// (keyboard `+`/`-`).
    pub fn zoom_at_center(&mut self, level_delta: f32) {
        self.zoom_at_screen((self.size.0 / 2.0, self.size.1 / 2.0), level_delta);
    }

    /// The block column under a viewport-local screen point.
    #[must_use]
    pub fn screen_to_block(&self, pos: (f32, f32)) -> Option<BlockPos> {
        self.viewport()
            .screen_to_block(ScreenPos::new(pos.0, pos.1))
    }

    /// The visible tile range at `step` (T10's [`coords::visible_tiles`],
    /// one-tile-expanded).
    #[must_use]
    pub fn visible_tiles(&self, step: u32) -> Option<TileRect> {
        coords::visible_tiles(&self.viewport(), step)
    }
}

impl Default for MapViewState {
    fn default() -> MapViewState {
        MapViewState::new()
    }
}

/// The viewport-local screen rect `(x0, y0, x1, y1)` of the tile at
/// `coord`/`step`, in points: the tile's min corner and its
/// (inclusive max + one block) corner mapped through the viewport.
/// `None` when the tile's block rect leaves the representable space.
#[must_use]
pub fn tile_screen_rect(
    view: &MapViewState,
    coord: TileCoord,
    step: u32,
) -> Option<(f32, f32, f32, f32)> {
    let blocks = coords::tile_rect(coord, step)?;
    let viewport = view.viewport();
    // The inclusive max block plus one is the exact edge shared with the next
    // tile, so neighbours meet on the same coordinate (the seam rule's input).
    let min = viewport.world_to_screen(WorldPos::new(
        f64::from(blocks.min_x),
        f64::from(blocks.min_z),
    ));
    let max = viewport.world_to_screen(WorldPos::new(
        f64::from(blocks.max_x + 1),
        f64::from(blocks.max_z + 1),
    ));
    Some((min.x, min.y, max.x, max.y))
}

/// Expands a screen rect outward to whole physical pixels: each min edge is
/// floored to a pixel boundary, each max edge ceiled.
///
/// WHY: two adjacent tiles share an edge coordinate, and floor/ceil are pure
/// functions of that coordinate — so neither tile leaves a gap because of
/// frame-to-frame rounding, and a pixel-aligned shared edge lands on the
/// identical boundary from both sides. A sub-pixel edge overlaps by at most
/// one physical pixel instead of showing a seam, which the widget paints in
/// the same color on both sides.
///
/// # Panics
/// Asserts `pixels_per_point` is positive and finite (egui always
/// supplies one).
#[must_use]
pub fn expand_to_pixels(
    min: (f32, f32),
    max: (f32, f32),
    pixels_per_point: f32,
) -> ((f32, f32), (f32, f32)) {
    assert!(
        pixels_per_point > 0.0 && pixels_per_point.is_finite(),
        "pixels_per_point must be positive and finite, got {pixels_per_point}",
    );
    let floor_px = |value: f32| (value * pixels_per_point).floor() / pixels_per_point;
    let ceil_px = |value: f32| (value * pixels_per_point).ceil() / pixels_per_point;
    (
        (floor_px(min.0), floor_px(min.1)),
        (ceil_px(max.0), ceil_px(max.1)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view_at(center: (f64, f64), width: f32, height: f32) -> MapViewState {
        let mut view = MapViewState::new();
        view.set_size(width, height);
        view.center = WorldPos::new(center.0, center.1);
        view
    }

    fn assert_close_f64(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-6,
            "expected {actual} and {expected} to agree within 1e-6",
        );
    }

    fn assert_close_f32(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 1e-6,
            "expected {actual} and {expected} to agree within 1e-6",
        );
    }

    // -- defaults -----------------------------------------------------------

    #[test]
    fn view_state_defaults_are_the_plan_initial_view() {
        let view = MapViewState::new();
        assert_eq!(view.center(), WorldPos::new(0.0, 0.0));
        assert_eq!(view.smooth_zoom(), 0.0);
        assert_eq!(view.scale(), 1.0);
        assert_eq!(view.zoom_level(), ZoomLevel::MIN);
        // The size asserts through the viewport it feeds (T10's shape).
        assert_eq!(view.viewport().width, 0.0);
        assert_eq!(view.viewport().height, 0.0);
        assert_eq!(
            MapViewState::default(),
            MapViewState::new(),
            "Default must delegate to new",
        );
        // A zero-sized viewport is still a valid T10 viewport.
        assert_eq!(view.viewport().width, 0.0);
    }

    // -- pan ----------------------------------------------------------------

    #[test]
    fn view_state_pan_moves_the_world_opposite_the_drag() {
        let mut view = view_at((0.0, 0.0), 1024.0, 768.0);

        view.pan_by_screen((100.0, -40.0));
        assert_eq!(view.center(), WorldPos::new(-100.0, 40.0));
        view.pan_by_screen((-25.0, 70.0));
        assert_eq!(view.center(), WorldPos::new(-75.0, -30.0));

        // At scale 0.5 a screen point covers two blocks, so the center moves
        // twice as far in world units.
        view.zoom_at_center(1.0);
        assert_eq!(view.scale(), 0.5);
        view.pan_by_screen((10.0, -20.0));
        assert_eq!(view.center(), WorldPos::new(-95.0, 10.0));
    }

    // -- set center (go-to) ---------------------------------------------------

    #[test]
    fn view_state_set_center_moves_the_view_without_touching_the_zoom() {
        let mut view = view_at((0.0, 0.0), 1024.0, 768.0);
        view.zoom_at_center(1.5);

        view.set_center(-1203.0, 455.0);
        assert_eq!(view.center(), WorldPos::new(-1203.0, 455.0));
        assert_eq!(view.smooth_zoom(), 1.5, "the zoom survives the jump");
    }

    #[test]
    #[should_panic(expected = "view center must be finite")]
    fn view_state_set_center_rejects_a_nan_coordinate() {
        let mut view = view_at((0.0, 0.0), 1024.0, 768.0);
        view.set_center(f64::NAN, 0.0);
    }

    // -- zoom at cursor -----------------------------------------------------

    #[test]
    fn view_state_zoom_at_screen_keeps_the_point_under_the_cursor() {
        for cursor in [(0.0, 0.0), (900.0, 100.0), (512.0, 384.0)] {
            for delta in [1.0, -0.7, 2.5] {
                // Start at level 2 so a negative delta stays inside the
                // clamp range and actually moves the zoom.
                let mut view = view_at((-1203.0, 455.0), 1024.0, 768.0);
                view.zoom_at_center(2.0);
                let world_under = view
                    .viewport()
                    .screen_to_world(ScreenPos::new(cursor.0, cursor.1));
                view.zoom_at_screen(cursor, delta);
                let still_under = view
                    .viewport()
                    .screen_to_world(ScreenPos::new(cursor.0, cursor.1));
                assert_close_f64(still_under.x, world_under.x);
                assert_close_f64(still_under.z, world_under.z);
                let expected_zoom = 2.0 + delta;
                assert_eq!(view.smooth_zoom(), expected_zoom);
                assert_eq!(view.scale(), 2.0f64.powf(-f64::from(expected_zoom)));
            }
        }
    }

    #[test]
    fn view_state_zoom_at_the_exact_center_leaves_the_center_alone() {
        let mut view = view_at((-1203.0, 455.0), 1024.0, 768.0);
        view.zoom_at_screen((512.0, 384.0), 3.0);
        assert_close_f64(view.center().x, -1203.0);
        assert_close_f64(view.center().z, 455.0);

        view.zoom_at_center(-1.5);
        assert_close_f64(view.center().x, -1203.0);
        assert_close_f64(view.center().z, 455.0);
        assert_eq!(view.smooth_zoom(), 1.5);
    }

    #[test]
    fn view_state_zoom_clamps_to_the_supported_range() {
        let mut view = view_at((-1203.0, 455.0), 1024.0, 768.0);

        view.zoom_at_screen((100.0, 100.0), -5.0);
        assert_eq!(view.smooth_zoom(), MIN_SMOOTH_ZOOM);
        assert_eq!(view.center(), WorldPos::new(-1203.0, 455.0));

        view.zoom_at_screen((100.0, 100.0), 100.0);
        assert_eq!(view.smooth_zoom(), MAX_SMOOTH_ZOOM);
        let at_max = view.center();

        // Already at the clamp: overshooting again changes nothing.
        view.zoom_at_screen((50.0, 50.0), 5.0);
        assert_eq!(view.smooth_zoom(), MAX_SMOOTH_ZOOM);
        assert_eq!(view.center(), at_max);
    }

    #[test]
    fn view_state_zoom_ignores_nan_deltas_and_non_finite_cursors() {
        let mut view = view_at((-1203.0, 455.0), 1024.0, 768.0);
        view.zoom_at_screen((10.0, 10.0), 2.0);
        let before = view;

        view.zoom_at_screen((10.0, 10.0), f32::NAN);
        assert_eq!(view, before);
        view.zoom_at_screen((f32::INFINITY, 10.0), 1.0);
        assert_eq!(view, before);
        view.zoom_at_screen((10.0, f32::NEG_INFINITY), 1.0);
        assert_eq!(view, before);
        view.zoom_at_screen((10.0, f32::NAN), 1.0);
        assert_eq!(view, before);
        view.zoom_at_screen((10.0, 10.0), 0.0);
        assert_eq!(view, before);

        // An infinite delta is not NaN — it clamps to the range end like any
        // other overshoot.
        view.zoom_at_screen((10.0, 10.0), f32::INFINITY);
        assert_eq!(view.smooth_zoom(), MAX_SMOOTH_ZOOM);
    }

    // -- zoom level ---------------------------------------------------------

    #[test]
    fn view_state_zoom_level_is_ceil_and_clamped() {
        for (smooth_zoom, level) in [
            (0.0_f32, 0_u8),
            (0.001, 1),
            (1.0, 1),
            (1.5, 2),
            (19.9, 20),
            (20.0, 20),
            (25.0, 20),
        ] {
            let view = MapViewState {
                center: WorldPos::new(0.0, 0.0),
                smooth_zoom,
                size: (1024.0, 768.0),
            };
            assert_eq!(
                view.zoom_level(),
                ZoomLevel::new(level).expect("test level is inside ZoomLevel's range"),
                "smooth_zoom {smooth_zoom}",
            );
        }
    }

    #[test]
    fn view_state_tile_step_folds_the_user_minimum_with_the_zoom() {
        let mut view = view_at((0.0, 0.0), 1024.0, 768.0);
        assert_eq!(view.tile_step(0), 1);
        assert_eq!(view.tile_step(2), 2);
        assert_eq!(view.tile_step(1), 1);
        assert_eq!(view.tile_step(8), 8);

        view.zoom_at_center(2.0);
        assert_eq!(view.tile_step(1), 4);
        assert_eq!(view.tile_step(2), 4);
        assert_eq!(view.tile_step(8), 8);

        view.zoom_at_center(-1.0);
        assert_eq!(view.zoom_level(), ZoomLevel::new(1).expect("in range"));
        assert_eq!(view.tile_step(8), 8);
        assert_eq!(view.tile_step(1), 2);
    }

    // -- screen -> block ----------------------------------------------------

    #[test]
    fn view_state_screen_to_block_floors_negative_coordinates() {
        let view = view_at((-1203.0, 455.0), 1024.0, 768.0);
        assert_eq!(
            view.screen_to_block((512.0, 384.0)),
            Some(BlockPos::new(-1203, 455)),
        );
        // Half a block inside the block origin still floors to the same block.
        assert_eq!(
            view.screen_to_block((512.5, 384.4)),
            Some(BlockPos::new(-1203, 455)),
        );

        // Zoomed far out with a center at the i32 edge, the far corner leaves
        // the representable block space and the conversion reports it.
        let mut edge = MapViewState {
            center: WorldPos::new(f64::from(i32::MAX), 0.0),
            smooth_zoom: 0.0,
            size: (1000.0, 1000.0),
        };
        assert!(edge.screen_to_block((500.0, 500.0)).is_some());
        edge.zoom_at_center(MAX_SMOOTH_ZOOM);
        assert_eq!(edge.screen_to_block((1000.0, 1000.0)), None);
    }

    #[test]
    fn view_state_visible_tiles_matches_the_t10_table() {
        let mut view = view_at((-1203.0, 455.0), 1024.0, 768.0);
        view.zoom_at_center(1.0);
        let visible = view
            .visible_tiles(2)
            .expect("the view fits the block space");
        assert_eq!(visible.min, TileCoord::new(-6, -2));
        assert_eq!(visible.max, TileCoord::new(0, 3));
        assert_eq!(visible.iter().count(), 7 * 6);
    }

    // -- tile screen rects --------------------------------------------------

    #[test]
    fn view_state_tile_screen_rect_at_negative_world_coordinates() {
        let view = view_at((-1203.0, 455.0), 800.0, 600.0);
        let (x0, y0, x1, y1) =
            tile_screen_rect(&view, TileCoord::new(0, 0), 1).expect("tile 0 is representable");
        // World x=0 sits 1203 blocks right of the center, plus 400 points of
        // half-width; world z=0 sits 455 blocks above it, plus 300.
        assert_eq!(x0, 1603.0);
        assert_eq!(y0, -155.0);
        assert_eq!(x1, 1859.0);
        assert_eq!(y1, 101.0);
        assert_eq!(x1 - x0, 256.0);
        assert_eq!(y1 - y0, 256.0);

        // The seam rule as arithmetic: tile (-1, -1)'s far corner is tile
        // (0, 0)'s near corner, bit for bit.
        let (nx0, ny0, nx1, ny1) =
            tile_screen_rect(&view, TileCoord::new(-1, -1), 1).expect("tile -1 is representable");
        assert_eq!(nx1, x0);
        assert_eq!(ny1, y0);
        assert!(nx0 < nx1 && ny0 < ny1);
    }

    #[test]
    fn view_state_tile_screen_rect_reports_tiles_outside_the_block_space() {
        let view = view_at((0.0, 0.0), 800.0, 600.0);
        assert_eq!(tile_screen_rect(&view, TileCoord::new(1 << 40, 0), 1), None);
    }

    // -- pixel expansion ----------------------------------------------------

    #[test]
    fn expand_to_pixels_floors_mins_and_ceils_maxes() {
        let (min, max) = expand_to_pixels((10.3, 20.6), (50.7, 60.2), 2.0);
        assert_eq!(min, (10.0, 20.5));
        assert_eq!(max, (51.0, 60.5));

        let (min, max) = expand_to_pixels((10.3, 20.6), (50.7, 60.2), 1.0);
        for value in [min.0, min.1, max.0, max.1] {
            assert_eq!(
                value,
                value.trunc(),
                "{value} should be integral at 1 px/pt"
            );
        }
    }

    #[test]
    fn expand_to_pixels_puts_adjacent_tiles_on_one_boundary() {
        // A pixel-aligned shared edge expands identically from both sides.
        let edge = 12.5;
        let left = expand_to_pixels((5.0, 5.0), (edge, 20.0), 2.0);
        let right = expand_to_pixels((edge, 5.0), (30.0, 20.0), 2.0);
        assert_eq!(left.1.0, right.0.0);

        // A sub-pixel edge overlaps by at most a pixel and never leaves a gap,
        // so the colour painted on both sides meets with nothing showing.
        let edge = 12.3;
        let left = expand_to_pixels((5.0, 5.0), (edge, 20.0), 2.0);
        let right = expand_to_pixels((edge, 5.0), (30.0, 20.0), 2.0);
        assert!(
            right.0.0 <= left.1.0,
            "neighbour min {} must not sit past the left max {}",
            right.0.0,
            left.1.0,
        );
    }

    #[test]
    #[should_panic(expected = "pixels_per_point must be positive and finite")]
    fn expand_to_pixels_rejects_zero_pixels_per_point() {
        let _ = expand_to_pixels((0.0, 0.0), (1.0, 1.0), 0.0);
    }

    #[test]
    #[should_panic(expected = "pixels_per_point must be positive and finite")]
    fn expand_to_pixels_rejects_a_nan_pixels_per_point() {
        let _ = expand_to_pixels((0.0, 0.0), (1.0, 1.0), f32::NAN);
    }

    // -- determinism --------------------------------------------------------

    #[test]
    fn view_state_derived_values_are_frame_stable() {
        let build = || {
            let mut view = view_at((-1203.0, 455.0), 1024.0, 768.0);
            view.zoom_at_screen((-17.0, 933.0), 2.3);
            view
        };
        let first = build();
        let second = build();
        assert_eq!(first, second);
        assert_eq!(first.visible_tiles(4), second.visible_tiles(4));
        assert_eq!(
            tile_screen_rect(&first, TileCoord::new(-3, 1), 4),
            tile_screen_rect(&second, TileCoord::new(-3, 1), 4),
        );
        assert_close_f32(first.smooth_zoom(), second.smooth_zoom());
    }
}
