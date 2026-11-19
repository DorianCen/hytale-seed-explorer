//! The height layer (T17): hypsometric tint from T09's cheap column
//! heights.
//!
//! One pixel per sample, north at image row 0, alpha 255 everywhere.
//! The ramp is anchored at [`SEA_LEVEL`] and its clamping lives in
//! [`height_color`], never in the source: world-edge heights may be `0`
//! or negative, and the map shows them as the ramp's lowest band rather
//! than as a hole.
//!
//! Shading is NOT here. The dedicated relief layer ([`super::relief`])
//! owns the hillshade, and compositing a shade onto other layers is
//! T19's job — the task's "optional contour/shade hook, disabled by
//! default" is thereby the relief layer's existence, not a toggle in
//! this one. Height stays the flat, golden-testable ramp.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use crate::error::{SourceError, SourceResult};
use crate::render::tiles::render_layer_tile;
use crate::scheduler::{CancelToken, TileRenderer};
use crate::source::WorldSource;
use crate::tiles::{Tile, TileKey};
use crate::types::{HeightGrid, Rect};

use super::image::{Rgba, RgbaImage};

/// The waterline the ramp is anchored to (the plan's reference surface;
/// the task's key elevations are relative to it).
pub const SEA_LEVEL: i32 = 64;

/// The hypsometric color table: one band per row, an inclusive upper
/// bound and its RGB. A height colors with the first row whose bound it
/// does not exceed; below the first and above the last row it clamps.
pub const HEIGHT_COLORS: [(i32, [u8; 3]); 7] = [
    (SEA_LEVEL - 24, [21, 44, 82]),   // deep bed
    (SEA_LEVEL - 8, [32, 88, 148]),   // shallow bed
    (SEA_LEVEL + 2, [196, 180, 132]), // shore / sand
    (96, [92, 128, 62]),              // lowland
    (160, [122, 104, 66]),            // upland
    (320, [139, 131, 125]),           // rock
    (i32::MAX, [240, 242, 246]),      // snow
];

/// The height ramp's color: the first table band whose bound covers `y`
/// (clamped at both ends), alpha 255.
///
/// Clamping lives HERE, never in the source — world-edge heights may be
/// `0` or negative, and the `i32::MAX` sentinel makes the search total
/// from below `i32::MIN` upwards.
#[must_use]
pub fn height_color(y: i32) -> Rgba<u8> {
    let (_, [r, g, b]) = HEIGHT_COLORS
        .iter()
        .copied()
        .find(|(upper, _)| y <= *upper)
        .expect("HEIGHT_COLORS ends at i32::MAX, so every height has a band");
    Rgba([r, g, b, 255])
}

/// Colors `grid` into a `grid.width × grid.height` image, one pixel per
/// sample, row-major, north at row 0.
///
/// `cancel` is checked once per 16-row batch (the worldgen pool's row
/// batched shape: an atomic load every 16 rows, not per pixel) and the
/// hit is returned as [`SourceError::Cancelled`].
pub fn height_image(grid: &HeightGrid, cancel: &CancelToken) -> SourceResult<RgbaImage> {
    let width = grid.width as usize;
    let mut image = RgbaImage::new(grid.width, grid.height);
    for y in 0..grid.height {
        if y.is_multiple_of(16) && cancel.load(Relaxed) {
            return Err(SourceError::Cancelled);
        }
        // The grid's row stride is `width`; `x` is a u32 sample index
        // (grids come from rects, far below usize::MAX).
        let row = (y as usize) * width;
        for x in 0..grid.width {
            image.put_pixel(x, y, height_color(grid.heights[row + x as usize]));
        }
    }
    Ok(image)
}

/// The height layer's tile renderer (border 0).
pub struct HeightTileRenderer {
    source: Arc<dyn WorldSource>,
    world: Option<Rect>,
}

impl HeightTileRenderer {
    /// `world` is the generatable block rect (inclusive); `None` = unbounded.
    #[must_use]
    pub fn new(source: Arc<dyn WorldSource>, world: Option<Rect>) -> Self {
        Self { source, world }
    }
}

impl TileRenderer for HeightTileRenderer {
    fn render(&self, key: &TileKey, cancel: &CancelToken) -> SourceResult<Tile> {
        // Border 0: the height ramp reads only the tile's own samples —
        // neighbour differences belong to the relief layer.
        render_layer_tile(
            self.source.as_ref(),
            key,
            cancel,
            self.world,
            0,
            |source, rect, step, _cancel| source.height_region(rect, step),
            |grid, query, cancel| height_image(grid, cancel).map(|image| (image, query.origin)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::AtomicBool;

    use crate::scheduler::cancel_token;

    #[test]
    fn height_render_color_table_at_key_elevations() {
        // The bands are inclusive on their upper bound, so each key
        // elevation sits on (or just inside) its documented band.
        for y in [-100, 0, 40] {
            assert_eq!(height_color(y), Rgba([21, 44, 82, 255]), "deep bed at {y}");
        }
        for y in [41, 56] {
            assert_eq!(
                height_color(y),
                Rgba([32, 88, 148, 255]),
                "shallow bed at {y}"
            );
        }
        for y in [57, 64, 66] {
            assert_eq!(height_color(y), Rgba([196, 180, 132, 255]), "shore at {y}");
        }
        for y in [67, 96] {
            assert_eq!(height_color(y), Rgba([92, 128, 62, 255]), "lowland at {y}");
        }
        for y in [128, 160] {
            assert_eq!(height_color(y), Rgba([122, 104, 66, 255]), "upland at {y}");
        }
        for y in [161, 255, 319, 320] {
            assert_eq!(height_color(y), Rgba([139, 131, 125, 255]), "rock at {y}");
        }
        for y in [321, 5_000, i32::MAX] {
            assert_eq!(height_color(y), Rgba([240, 242, 246, 255]), "snow at {y}");
        }
    }

    #[test]
    fn height_render_image_maps_every_sample_row_major() {
        // A step-2 grid with a negative origin; every value lands in a
        // different band, so a flipped or mis-strided image is visible.
        let heights = vec![40, 56, 64, 96, 160, 321];
        let grid = HeightGrid::new(-4, -6, 2, 3, 2, heights.clone());
        let image = height_image(&grid, &cancel_token()).expect("an uncancelled height image");

        assert_eq!((image.width(), image.height()), (3, 2));
        for y in 0..2u32 {
            for x in 0..3u32 {
                let index = (y * 3 + x) as usize;
                assert_eq!(
                    image.get_pixel(x, y),
                    &height_color(heights[index]),
                    "pixel ({x}, {y}) is the row-major sample {index}",
                );
            }
        }
    }

    #[test]
    fn height_render_image_honors_cancellation_between_row_batches() {
        // A pre-set token stops the render at the row-0 batch check,
        // before any row is colored into a returned image.
        let grid = HeightGrid::new(0, 0, 1, 2, 2, vec![64; 4]);
        let cancelled: CancelToken = Arc::new(AtomicBool::new(true));
        assert_eq!(
            height_image(&grid, &cancelled),
            Err(SourceError::Cancelled),
            "a cancelled image is a typed error, never a partial result",
        );
    }
}
