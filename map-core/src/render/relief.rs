//! The shaded-relief layer (T17): hillshade from neighbour height
//! differences over the height ramp (relief = height color × shade).
//!
//! The renderer requests one extra ring of samples (border 1) so a
//! tile's edge shading matches its neighbour tile's — no seams when
//! panning. Missing neighbours at the world edge replicate the nearest
//! in-world height (the task's "nearest valid height, do not wrap"):
//! extrapolated shading beats a transparent hole in a light layer, and
//! the same clamp covers both the tile edge and the world edge.
//!
//! The light sits at the map's north-west, matching T13's documented
//! convention (light from the screen's top-left). The task's
//! `(-0.2, 0.8, 0.5)` has its Z sign flipped here because world Z grows
//! south / screen-down, so that is the same physical direction.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use crate::coords::TILE_SIZE;
use crate::error::{SourceError, SourceResult};
use crate::render::height::height_color;
use crate::render::tiles::render_layer_tile;
use crate::scheduler::{CancelToken, TileRenderer};
use crate::source::{SummaryFidelity, WorldSource};
use crate::tiles::{Tile, TileKey};
use crate::types::{HeightGrid, Rect};

use super::image::{Rgba, RgbaImage};

/// The direction TO the light, normalized: north-west and steeply above
/// the horizon (T13's convention — light from the screen's top-left).
///
/// The task's `(-0.2, 0.8, 0.5)` with the Z sign flipped because world Z
/// grows south / screen-down, i.e. `(-0.2, 0.8, -0.5) / sqrt(0.93)` with
/// `sqrt(0.2² + 0.8² + 0.5²) = 0.964365…`. `f32` has no const `sqrt`,
/// so the division is folded into the literals.
const LIGHT: [f32; 3] = [-0.207_390_34, 0.829_561_35, -0.518_475_83];

/// The vertical exaggeration of the task's surface normal `(dhdx, 3, dhdz)`.
const VERTICAL: f32 = 3.0;

/// The ambient/diffuse split of the task's `shade = 0.4 + 0.6 * lambert`.
const AMBIENT: f32 = 0.4;
const DIFFUSE: f32 = 0.6;

/// The T17 hillshade factor for one output sample from clamped
/// neighbour heights (blocks): the Lambert shade
/// `AMBIENT + DIFFUSE * max(0, dot(normal, LIGHT))` of the surface
/// normal `(dhdx, VERTICAL, dhdz)`, with the per-block slope
/// normalized by `step`. Relief and the full-block surface (T21)
/// share it — one hillshade convention, no drift.
///
/// The central-difference slope uses the four neighbours only; the
/// `center` height is accepted so the argument list reads like the
/// sample window the caller reads, and the full-block renderer already
/// has it in hand.
#[must_use]
pub fn hillshade(center: i32, east: i32, west: i32, south: i32, north: i32, step: u32) -> f32 {
    let _ = center;
    let east = east as f32;
    let west = west as f32;
    let south = south as f32;
    let north = north as f32;
    // A zoom step (at most 2^20 blocks) is exactly representable in f32.
    let step = step as f32;

    let dhdx = (east - west) / (2.0 * step);
    let dhdz = (south - north) / (2.0 * step);
    // `VERTICAL` is 3, so the normal never has zero length.
    let normal_len = (dhdx * dhdx + VERTICAL * VERTICAL + dhdz * dhdz).sqrt();
    let lambert =
        ((dhdx * LIGHT[0] + VERTICAL * LIGHT[1] + dhdz * LIGHT[2]) / normal_len).clamp(0.0, 1.0);
    AMBIENT + DIFFUSE * lambert
}

/// Shades `width × height` output samples out of `grid`: output sample
/// `(x, z)` reads `grid` at `(x - origin.0)` / `(z - origin.1)` — the
/// query plan's mapping — and every neighbour read is CLAMPED to the
/// grid's bounds (the nearest valid height: no wrap, no fabricated
/// zeros, world edge and tile edge handled by the same rule).
///
/// Per output sample, with `step = grid.step` (heights are `step`
/// blocks apart):
///
/// - `dhdx = (east - west) / (2 * step)` and
///   `dhdz = (south - north) / (2 * step)` in blocks — the per-block
///   normalization keeps the same terrain shading identical across zoom
///   levels;
/// - normal = normalize(`(dhdx, VERTICAL, dhdz)`); lambert =
///   `dot(normal, LIGHT)` clamped to `0..=1`;
/// - shade = `AMBIENT + DIFFUSE * lambert` (the pure [`hillshade`]
///   function); the pixel is `height_color(center)` with each channel
///   multiplied by shade (rounded, clamped `0..=255`), alpha 255.
///
/// `cancel` is checked every 16 output rows, returning
/// [`SourceError::Cancelled`] on a hit.
pub fn relief_image(
    grid: &HeightGrid,
    origin: (i32, i32),
    width: u32,
    height: u32,
    cancel: &CancelToken,
) -> SourceResult<RgbaImage> {
    let grid_width = grid.width as usize;
    let grid_height = grid.height as usize;
    assert!(
        grid_width > 0 && grid_height > 0,
        "a relief source grid has at least one sample per axis",
    );
    let max_x = i64::from(grid.width) - 1;
    let max_z = i64::from(grid.height) - 1;
    // The clamp is what makes edge shading a "nearest valid height"
    // extrapolation rather than a read of zero.
    let sample = |x: i64, z: i64| -> i32 {
        let x = x.clamp(0, max_x) as usize;
        let z = z.clamp(0, max_z) as usize;
        grid.heights[z * grid_width + x]
    };

    let mut image = RgbaImage::new(width, height);
    for y in 0..height {
        if y.is_multiple_of(16) && cancel.load(Relaxed) {
            return Err(SourceError::Cancelled);
        }
        let gz = i64::from(y) - i64::from(origin.1);
        for x in 0..width {
            let gx = i64::from(x) - i64::from(origin.0);
            let center = sample(gx, gz);
            let shade = hillshade(
                center,
                sample(gx + 1, gz),
                sample(gx - 1, gz),
                sample(gx, gz + 1),
                sample(gx, gz - 1),
                grid.step,
            );

            let Rgba([r, g, b, _]) = height_color(center);
            image.put_pixel(
                x,
                y,
                Rgba([
                    shade_channel(r, shade),
                    shade_channel(g, shade),
                    shade_channel(b, shade),
                    255,
                ]),
            );
        }
    }
    Ok(image)
}

/// Applies `shade` to one 8-bit channel: multiply, round, clamp.
pub(crate) fn shade_channel(channel: u8, shade: f32) -> u8 {
    // The clamp proves the range before the narrowing cast.
    (f32::from(channel) * shade).round().clamp(0.0, 255.0) as u8
}

/// The relief layer's tile renderer (border 1: the neighbour ring).
pub struct ReliefTileRenderer {
    source: Arc<dyn WorldSource>,
    world: Option<Rect>,
}

impl ReliefTileRenderer {
    /// `world` is the generatable block rect (inclusive); `None` = unbounded.
    #[must_use]
    pub fn new(source: Arc<dyn WorldSource>, world: Option<Rect>) -> Self {
        Self { source, world }
    }
}

impl TileRenderer for ReliefTileRenderer {
    fn render(
        &self,
        key: &TileKey,
        cancel: &CancelToken,
        fidelity: SummaryFidelity,
    ) -> SourceResult<Tile> {
        // Border 1: the shading at the tile edge needs the neighbour
        // sample beyond it, and the plan clips that ring at the world
        // edge. The patch is canvas-true (TILE_SIZE square at (0, 0)):
        // the ring is read, never painted. Relief reads the direct height
        // query, not summaries, so the pass fidelity does not apply.
        render_layer_tile(
            self.source.as_ref(),
            key,
            cancel,
            fidelity,
            self.world,
            1,
            |source, rect, step, _cancel| source.height_region(rect, step),
            |grid, query, cancel| {
                relief_image(grid, query.origin, TILE_SIZE, TILE_SIZE, cancel)
                    .map(|image| (image, (0, 0)))
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::AtomicBool;

    use crate::scheduler::cancel_token;

    /// The flat-ground shade: the normal is straight up, so the Lambert
    /// term is the light vector's Y component.
    fn flat_shade() -> f32 {
        let light_y = 0.8 / (0.2f32 * 0.2 + 0.8 * 0.8 + 0.5 * 0.5).sqrt();
        AMBIENT + DIFFUSE * light_y
    }

    #[test]
    fn relief_render_flat_grid_is_uniform() {
        let grid = HeightGrid::new(0, 0, 1, 4, 4, vec![64; 16]);
        // Origin (-1, -1): the output's top-left sample sits one grid
        // sample inside the grid and the bottom-right clamps at its far
        // edge — constant height makes both reads the same.
        let image = relief_image(&grid, (-1, -1), 4, 4, &cancel_token())
            .expect("an uncancelled relief image");

        let Rgba([r, g, b, _]) = height_color(64);
        let shade = flat_shade();
        assert!(
            shade > AMBIENT && shade < 1.0,
            "flat shade is ambient+diffuse"
        );

        let first = *image.get_pixel(0, 0);
        for y in 0..4u32 {
            for x in 0..4u32 {
                assert_eq!(
                    image.get_pixel(x, y),
                    &first,
                    "flat ground shades uniformly at ({x}, {y})",
                );
            }
        }
        assert_eq!(first[3], 255, "relief is opaque");

        let base = [r, g, b];
        for (actual, base) in first.0.iter().take(3).zip(base) {
            let expected = (f32::from(base) * shade).round().clamp(0.0, 255.0);
            let diff = (f32::from(*actual) - expected).abs();
            // 1% tolerance absorbs the f32 rounding of the shade and the
            // unit-scale rounding of the channel; no magic constants.
            assert!(
                diff <= expected * 0.01 + 1.0,
                "channel {actual} is within 1% of {expected}",
            );
        }
    }

    #[test]
    fn relief_render_slopes_facing_the_light_are_brighter() {
        // Rising toward the north-west faces the light; rising toward
        // the south-east faces away. Mirror-image samples so each pair
        // differs only in slope direction.
        let mut nw_values = Vec::new();
        let mut se_values = Vec::new();
        for row in 0..3 {
            for col in 0..3 {
                nw_values.push(64 - col - row);
                se_values.push(64 + col + row);
            }
        }
        let nw = HeightGrid::new(0, 0, 1, 3, 3, nw_values);
        let se = HeightGrid::new(0, 0, 1, 3, 3, se_values);
        let nw_image =
            relief_image(&nw, (0, 0), 3, 3, &cancel_token()).expect("an uncancelled relief image");
        let se_image =
            relief_image(&se, (0, 0), 3, 3, &cancel_token()).expect("an uncancelled relief image");

        for z in 0..3u32 {
            for x in 0..3u32 {
                let bright = nw_image.get_pixel(x, z);
                let dark = se_image.get_pixel(2 - x, 2 - z);
                assert!(
                    bright[0] > dark[0],
                    "NW-rising sample ({x}, {z}) is brighter than its SE mirror: {} vs {}",
                    bright[0],
                    dark[0],
                );
            }
        }

        // And the SE-facing centre is darker than flat ground: the light
        // really is directional, not just a brightness offset.
        let se_center = f32::from(se_image.get_pixel(1, 1)[0]);
        let flat_center = f32::from(height_color(66)[0]) * flat_shade();
        assert!(
            se_center < flat_center,
            "SE-facing centre {se_center} is darker than flat {flat_center}",
        );
    }

    #[test]
    fn relief_render_step_normalization_keeps_shading_comparable() {
        // The same per-block slope written at step 1 and step 2: the
        // `/(2 * step)` normalization must give the same centre pixel.
        let mut step_one_values = Vec::new();
        let mut step_two_values = Vec::new();
        for row in 0..3 {
            for col in 0..3 {
                step_one_values.push(col + row);
                step_two_values.push((col + row) * 2);
            }
        }
        let step_one = HeightGrid::new(0, 0, 1, 3, 3, step_one_values);
        let step_two = HeightGrid::new(0, 0, 2, 3, 3, step_two_values);
        let one = relief_image(&step_one, (0, 0), 3, 3, &cancel_token())
            .expect("an uncancelled relief image");
        let two = relief_image(&step_two, (0, 0), 3, 3, &cancel_token())
            .expect("an uncancelled relief image");

        assert_eq!(
            one.get_pixel(1, 1),
            two.get_pixel(1, 1),
            "step-1 and step-2 renders of the same slope shade identically",
        );
    }

    #[test]
    fn relief_render_edges_use_the_nearest_valid_height_not_zero() {
        // A 3x3 ramp in a 5x5 output: the outer ring reads past the grid
        // (at the far side) and the top-left corner's west/north reads
        // are past it too — they must clamp to the nearest sample.
        let mut values = Vec::new();
        for row in 0..3 {
            for col in 0..3 {
                values.push(col + row);
            }
        }
        let grid = HeightGrid::new(0, 0, 1, 3, 3, values);
        let image = relief_image(&grid, (0, 0), 5, 5, &cancel_token())
            .expect("an uncancelled relief image");

        // No fabricated holes and no saturated artifact: a zero-neighbour
        // read would produce an extreme slope, visible as black or white.
        for y in 0..5u32 {
            for x in 0..5u32 {
                let pixel = image.get_pixel(x, y);
                assert!(
                    pixel.0.iter().take(3).all(|&channel| channel > 0),
                    "pixel ({x}, {y}) is not black: {pixel:?}",
                );
                assert!(
                    pixel.0.iter().take(3).all(|&channel| channel < 255),
                    "pixel ({x}, {y}) is not a saturated zero-slope artifact: {pixel:?}",
                );
            }
        }

        // The corner sample grid (0, 0) has east/south = 1 and its
        // west/north reads clamped to itself (0): the clamped half-slope
        // is (0.5, 0.5), not the zero-neighbour extremes.
        let Rgba([r, g, b, _]) = height_color(0);
        let dhdx = 0.5_f32;
        let dhdz = 0.5_f32;
        let normal_len = (dhdx * dhdx + VERTICAL * VERTICAL + dhdz * dhdz).sqrt();
        let lambert = ((dhdx * LIGHT[0] + VERTICAL * LIGHT[1] + dhdz * LIGHT[2]) / normal_len)
            .clamp(0.0, 1.0);
        let shade = AMBIENT + DIFFUSE * lambert;
        let expected = Rgba([
            shade_channel(r, shade),
            shade_channel(g, shade),
            shade_channel(b, shade),
            255,
        ]);
        assert_eq!(
            image.get_pixel(0, 0),
            &expected,
            "the corner clamps its out-of-grid neighbours to the nearest valid height",
        );

        // The clamp half-slope sits strictly between the flat shade and
        // the away-facing full slope — proof it extrapolated, not zeroed.
        let flat = f32::from(height_color(0)[0]) * flat_shade();
        let half = f32::from(expected[0]);
        let full = f32::from(
            relief_image(&grid, (0, 0), 3, 3, &cancel_token())
                .expect("an uncancelled relief image")
                .get_pixel(1, 1)[0],
        );
        assert!(
            flat > half && half > full,
            "clamped corner {half} lies between flat {flat} and full slope {full}",
        );
    }

    #[test]
    fn relief_render_image_honors_cancellation_between_row_batches() {
        let grid = HeightGrid::new(0, 0, 1, 2, 2, vec![64; 4]);
        let cancelled: CancelToken = Arc::new(AtomicBool::new(true));
        assert_eq!(
            relief_image(&grid, (0, 0), 4, 4, &cancelled),
            Err(SourceError::Cancelled),
            "a cancelled relief is a typed error, never a partial image",
        );
    }
}
