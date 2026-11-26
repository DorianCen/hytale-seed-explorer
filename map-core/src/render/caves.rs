//! The two cave layers (T28): a horizontal Y-slice and a cave-volume
//! density heat map.
//!
//! The slice is the block state of one horizontal plane, straight from
//! T09's [`SliceGrid`](crate::types::SliceGrid): solid blocks are dark
//! grey, fluids keep T17's placeholder blue, and air/unknown cells are
//! transparent so the layers underneath (or the canvas) show through.
//! The Y travels in the tile key's `mode` field
//! ([`cave_slice_mode`]); changing the slider therefore requests a new
//! key and leaves every other layer's tiles untouched.
//!
//! The density layer is a coarse top-down cave-volume proxy from
//! [`CaveDensityGrid`](crate::types::CaveDensityGrid): air cells strictly
//! below the column's topmost non-air block divided by that block's Y.
//! Fluids do not count, and the numerator includes every air pocket below
//! the surface (caves, overhangs and prefab voids), so this is a
//! cave-*volume* view, not a cave-type classifier. The metric lives in
//! [`cave_fraction`](crate::source::chunks::cave_fraction); its version
//! is [`CAVE_DENSITY_MODE`], carried in the density tile key so a future
//! metric change cannot serve old pixels.
//!
//! Both renderers use the shared tile skeleton and the source's cancellable
//! slice/density paths. The one-shot path makes one batched region query per
//! tile; the streaming path publishes one chunk-row band at a time, with a
//! transparent canvas outside the world and cancellation between bands.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use crate::coords::{TILE_SIZE, TileCoord, tile_rect};
use crate::error::{SourceError, SourceResult};
use crate::render::fluid::FLUID_COLOR;
use crate::render::image::{Rgba, RgbaImage};
use crate::render::tiles::{blit_patch, render_layer_tile, tile_query};
use crate::scheduler::{CancelToken, TileRenderer};
use crate::source::{SummaryFidelity, WorldSource};
use crate::tiles::{Tile, TileKey};
use crate::types::{CaveDensityGrid, Rect, SliceCell, SliceGrid};

/// The lowest world Y the cave-slice slider offers.
pub const CAVE_SLICE_Y_MIN: i32 = 0;
/// The highest world Y the cave-slice slider offers (the chunk height is
/// 320 blocks, so the top cell is 319).
pub const CAVE_SLICE_Y_MAX: i32 = 319;

/// The density metric/version carried in a density tile's key mode. T28's
/// metric is v1: air cells below the top block / the top block's Y, fluids
/// excluded. Changing either half of that definition must bump this value.
pub const CAVE_DENSITY_MODE: u16 = 1;

/// The slice's solid-block color: a dark neutral grey, deliberately not
/// asset-derived (T03's palette rules).
pub const CAVE_SOLID_COLOR: [u8; 4] = [64, 68, 76, 255];
/// Air in a slice is transparent: the layers below are the context, and a
/// cave-only view is simply the slice over a blank canvas.
pub const CAVE_AIR_COLOR: [u8; 4] = [0, 0, 0, 0];
/// Fluid in a slice uses T17's shared placeholder blue.
pub const CAVE_FLUID_COLOR: [u8; 4] = [FLUID_COLOR[0], FLUID_COLOR[1], FLUID_COLOR[2], 255];
/// An unsampled/out-of-world cell is transparent, like air, but carries no
/// claim that the generator reported empty space.
pub const CAVE_UNKNOWN_COLOR: [u8; 4] = [0, 0, 0, 0];

/// The density ramp's low/mid/high stops (dark blue-grey → red → orange).
const DENSITY_STOPS: [(f32, [u8; 3]); 3] = [
    (0.0, [24, 30, 44]),
    (0.5, [150, 52, 44]),
    (1.0, [255, 152, 48]),
];

/// One streaming band is a chunk row of a tile: `32 / step` pixels tall.
/// Above 32 blocks per sample the row is sub-pixel, so the one-shot path is
/// exact and cheaper.
fn cave_band_rows(step: u32) -> Option<u32> {
    (step >= 1 && 32_u32.is_multiple_of(step)).then(|| 32 / step)
}

/// Encodes a world Y in a [`SliceGrid`]-like tile key mode.
///
/// The app clamps its slider to [`CAVE_SLICE_Y_MIN`]..=[`CAVE_SLICE_Y_MAX`];
/// this helper clamps again so a persisted or programmatic value cannot
/// wrap into a different Y.
#[must_use]
pub fn cave_slice_mode(y: i32) -> u16 {
    let y = y.clamp(CAVE_SLICE_Y_MIN, CAVE_SLICE_Y_MAX);
    u16::try_from(y).expect("a Y clamped to 0..=319 fits u16")
}

/// The world Y a cave-slice tile key mode names.
#[must_use]
pub fn cave_slice_y(mode: u16) -> i32 {
    i32::from(mode)
}

/// The slice color for one cell.
#[must_use]
pub fn cave_slice_color(cell: SliceCell) -> Rgba<u8> {
    let [r, g, b, a] = match cell {
        SliceCell::Solid(_) => CAVE_SOLID_COLOR,
        SliceCell::Fluid(_) => CAVE_FLUID_COLOR,
        SliceCell::Air => CAVE_AIR_COLOR,
        SliceCell::Unknown => CAVE_UNKNOWN_COLOR,
    };
    Rgba([r, g, b, a])
}

/// Colors a slice grid one pixel per sample, row-major, north at row 0.
/// `cancel` is checked every 16 rows, the same batch shape as the other
/// layer images.
pub fn cave_slice_image(grid: &SliceGrid, cancel: &CancelToken) -> SourceResult<RgbaImage> {
    let width = grid.width as usize;
    assert!(
        width > 0 && grid.height > 0,
        "a slice source grid has at least one sample per axis",
    );
    let mut image = RgbaImage::new(grid.width, grid.height);
    for y in 0..grid.height {
        if y.is_multiple_of(16) && cancel.load(Relaxed) {
            return Err(SourceError::Cancelled);
        }
        let row = y as usize * width;
        for x in 0..grid.width {
            image.put_pixel(x, y, cave_slice_color(grid.cells[row + x as usize]));
        }
    }
    Ok(image)
}

/// The density ramp: a clamped fraction in `0.0..=1.0` interpolated
/// through [`DENSITY_STOPS`]. A NaN (a corrupt source sample) is treated
/// as no caves rather than poisoning a channel.
#[must_use]
pub fn cave_density_color(fraction: f32) -> Rgba<u8> {
    let fraction = if fraction.is_nan() {
        0.0
    } else {
        fraction.clamp(0.0, 1.0)
    };
    let (low, high, t) = if fraction <= 0.5 {
        (DENSITY_STOPS[0], DENSITY_STOPS[1], fraction / 0.5)
    } else {
        (DENSITY_STOPS[1], DENSITY_STOPS[2], (fraction - 0.5) / 0.5)
    };
    let channel = |from: u8, to: u8| -> u8 {
        (f32::from(from) + (f32::from(to) - f32::from(from)) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Rgba([
        channel(low.1[0], high.1[0]),
        channel(low.1[1], high.1[1]),
        channel(low.1[2], high.1[2]),
        255,
    ])
}

/// Colors a density grid one pixel per sample, row-major, north at row 0,
/// `cancel` checked every 16 rows.
pub fn cave_density_image(grid: &CaveDensityGrid, cancel: &CancelToken) -> SourceResult<RgbaImage> {
    let width = grid.width as usize;
    assert!(
        width > 0 && grid.height > 0,
        "a cave-density source grid has at least one sample per axis",
    );
    let mut image = RgbaImage::new(grid.width, grid.height);
    for y in 0..grid.height {
        if y.is_multiple_of(16) && cancel.load(Relaxed) {
            return Err(SourceError::Cancelled);
        }
        let row = y as usize * width;
        for x in 0..grid.width {
            image.put_pixel(x, y, cave_density_color(grid.densities[row + x as usize]));
        }
    }
    Ok(image)
}

/// The cave Y-slice tile renderer: the Y comes from the key's mode, and
/// the fetch is the cancellable slice path (a slice miss generates the
/// covered chunks).
pub struct CaveSliceTileRenderer {
    source: Arc<dyn WorldSource>,
    world: Option<Rect>,
}

impl CaveSliceTileRenderer {
    /// `world` is the generatable block rect (inclusive); `None` = unbounded.
    #[must_use]
    pub fn new(source: Arc<dyn WorldSource>, world: Option<Rect>) -> Self {
        Self { source, world }
    }
}

impl TileRenderer for CaveSliceTileRenderer {
    fn render(
        &self,
        key: &TileKey,
        cancel: &CancelToken,
        fidelity: SummaryFidelity,
    ) -> SourceResult<Tile> {
        let y = cave_slice_y(key.mode);
        render_layer_tile(
            self.source.as_ref(),
            key,
            cancel,
            fidelity,
            self.world,
            0,
            |source, rect, step, cancel| source.slice_region_cancelled(rect, step, y, cancel),
            |grid, query, cancel| cave_slice_image(grid, cancel).map(|image| (image, query.origin)),
        )
    }

    /// Streams the slice one chunk row at a time: each band generates only
    /// that row's chunks and publishes the canvas so far, so a cold cave
    /// view starts showing rows after the first band instead of waiting for
    /// the whole 512-block tile. The final tile is pixel-identical to
    /// [`TileRenderer::render`].
    fn render_streaming(
        &self,
        key: &TileKey,
        cancel: &CancelToken,
        fidelity: SummaryFidelity,
        progress: &mut dyn FnMut(Tile),
    ) -> SourceResult<Tile> {
        let step = key.zoom.step();
        let Some(band_rows) = cave_band_rows(step) else {
            return TileRenderer::render(self, key, cancel, fidelity);
        };
        if cancel.load(Relaxed) {
            return Err(SourceError::Cancelled);
        }
        let coord = TileCoord::new(i64::from(key.tx), i64::from(key.ty));
        let Some(tile) = tile_rect(coord, step) else {
            return Err(SourceError::OutOfBounds {
                x: key.tx,
                z: key.ty,
            });
        };
        let y = cave_slice_y(key.mode);
        let mut canvas = RgbaImage::new(TILE_SIZE, TILE_SIZE);
        let mut band_top: u32 = 0;
        while band_top < TILE_SIZE {
            if cancel.load(Relaxed) {
                return Err(SourceError::Cancelled);
            }
            let band_min_z = i64::from(tile.min_z) + i64::from(band_top) * i64::from(step);
            let band_max_z = band_min_z + (i64::from(band_rows) - 1) * i64::from(step);
            let band = Rect::new(
                tile.min_x,
                i32::try_from(band_min_z).expect("a band lies inside the representable tile rect"),
                tile.max_x,
                i32::try_from(band_max_z).expect("a band lies inside the representable tile rect"),
            );
            let mut band_canvas = RgbaImage::new(TILE_SIZE, band_rows);
            if let Some(plan) = tile_query(band, self.world, step, 0) {
                let grid = self
                    .source
                    .slice_region_cancelled(plan.rect, step, y, cancel)?;
                let patch = cave_slice_image(&grid, cancel)?;
                blit_patch(&mut band_canvas, &patch, plan.origin);
            }
            blit_patch(
                &mut canvas,
                &band_canvas,
                (
                    0,
                    i32::try_from(band_top).expect("band_top is below TILE_SIZE"),
                ),
            );
            progress(
                Tile::new(*key, TILE_SIZE, TILE_SIZE, canvas.as_raw().clone())
                    .expect("the canvas is TILE_SIZE square with a matching buffer")
                    .with_fidelity(fidelity)
                    .with_complete(false),
            );
            band_top += band_rows;
        }
        Ok(
            Tile::new(*key, TILE_SIZE, TILE_SIZE, canvas.as_raw().clone())
                .expect("the canvas is TILE_SIZE square with a matching buffer")
                .with_fidelity(fidelity),
        )
    }
}

/// The cave-density tile renderer: one pixel per column's cave-volume
/// fraction, served from the full-fidelity summary cache.
pub struct CaveDensityTileRenderer {
    source: Arc<dyn WorldSource>,
    world: Option<Rect>,
}

impl CaveDensityTileRenderer {
    /// `world` is the generatable block rect (inclusive); `None` = unbounded.
    #[must_use]
    pub fn new(source: Arc<dyn WorldSource>, world: Option<Rect>) -> Self {
        Self { source, world }
    }
}

impl TileRenderer for CaveDensityTileRenderer {
    fn render(
        &self,
        key: &TileKey,
        cancel: &CancelToken,
        fidelity: SummaryFidelity,
    ) -> SourceResult<Tile> {
        render_layer_tile(
            self.source.as_ref(),
            key,
            cancel,
            fidelity,
            self.world,
            0,
            |source, rect, step, cancel| source.cave_density_region_cancelled(rect, step, cancel),
            |grid, query, cancel| {
                cave_density_image(grid, cancel).map(|image| (image, query.origin))
            },
        )
    }

    /// Density streams in the same chunk-row bands as the slice: the first
    /// band's summaries are enough to paint the top rows while the rest of
    /// the tile generates/caches. The final tile is pixel-identical to
    /// [`TileRenderer::render`].
    fn render_streaming(
        &self,
        key: &TileKey,
        cancel: &CancelToken,
        fidelity: SummaryFidelity,
        progress: &mut dyn FnMut(Tile),
    ) -> SourceResult<Tile> {
        let step = key.zoom.step();
        let Some(band_rows) = cave_band_rows(step) else {
            return TileRenderer::render(self, key, cancel, fidelity);
        };
        if cancel.load(Relaxed) {
            return Err(SourceError::Cancelled);
        }
        let coord = TileCoord::new(i64::from(key.tx), i64::from(key.ty));
        let Some(tile) = tile_rect(coord, step) else {
            return Err(SourceError::OutOfBounds {
                x: key.tx,
                z: key.ty,
            });
        };
        let mut canvas = RgbaImage::new(TILE_SIZE, TILE_SIZE);
        let mut band_top: u32 = 0;
        while band_top < TILE_SIZE {
            if cancel.load(Relaxed) {
                return Err(SourceError::Cancelled);
            }
            let band_min_z = i64::from(tile.min_z) + i64::from(band_top) * i64::from(step);
            let band_max_z = band_min_z + (i64::from(band_rows) - 1) * i64::from(step);
            let band = Rect::new(
                tile.min_x,
                i32::try_from(band_min_z).expect("a band lies inside the representable tile rect"),
                tile.max_x,
                i32::try_from(band_max_z).expect("a band lies inside the representable tile rect"),
            );
            let mut band_canvas = RgbaImage::new(TILE_SIZE, band_rows);
            if let Some(plan) = tile_query(band, self.world, step, 0) {
                let grid = self
                    .source
                    .cave_density_region_cancelled(plan.rect, step, cancel)?;
                let patch = cave_density_image(&grid, cancel)?;
                blit_patch(&mut band_canvas, &patch, plan.origin);
            }
            blit_patch(
                &mut canvas,
                &band_canvas,
                (
                    0,
                    i32::try_from(band_top).expect("band_top is below TILE_SIZE"),
                ),
            );
            progress(
                Tile::new(*key, TILE_SIZE, TILE_SIZE, canvas.as_raw().clone())
                    .expect("the canvas is TILE_SIZE square with a matching buffer")
                    .with_fidelity(fidelity)
                    .with_complete(false),
            );
            band_top += band_rows;
        }
        Ok(
            Tile::new(*key, TILE_SIZE, TILE_SIZE, canvas.as_raw().clone())
                .expect("the canvas is TILE_SIZE square with a matching buffer")
                .with_fidelity(fidelity),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::AtomicBool;

    use crate::scheduler::cancel_token;
    use crate::types::{BlockId, FluidId};

    #[test]
    fn cave_render_slice_colors_distinguish_solid_air_and_fluid() {
        assert_eq!(
            cave_slice_color(SliceCell::Solid(BlockId(7))),
            Rgba(CAVE_SOLID_COLOR)
        );
        assert_eq!(
            cave_slice_color(SliceCell::Fluid(FluidId(1))),
            Rgba(CAVE_FLUID_COLOR)
        );
        assert_eq!(cave_slice_color(SliceCell::Air), Rgba(CAVE_AIR_COLOR));
        assert_eq!(
            cave_slice_color(SliceCell::Unknown),
            Rgba(CAVE_UNKNOWN_COLOR)
        );
        assert_eq!(Rgba(CAVE_AIR_COLOR)[3], 0, "air is transparent");
        assert_eq!(Rgba(CAVE_SOLID_COLOR)[3], 255, "solid is opaque");
        assert_eq!(Rgba(CAVE_FLUID_COLOR)[3], 255, "fluid is opaque");
    }

    #[test]
    fn cave_render_slice_image_maps_cells_row_major() {
        // A step-2 grid with a negative origin: every cell type appears at a
        // different pixel, so a flipped or mis-strided image is visible.
        let cells = vec![
            SliceCell::Solid(BlockId(7)),
            SliceCell::Air,
            SliceCell::Fluid(FluidId(1)),
            SliceCell::Unknown,
        ];
        let grid = SliceGrid::new(-4, -6, 2, 2, 2, 64, cells.clone());
        let image = cave_slice_image(&grid, &cancel_token()).expect("an uncancelled slice image");

        assert_eq!((image.width(), image.height()), (2, 2));
        for y in 0..2u32 {
            for x in 0..2u32 {
                let index = (y * 2 + x) as usize;
                assert_eq!(
                    image.get_pixel(x, y),
                    &cave_slice_color(cells[index]),
                    "pixel ({x}, {y}) is the row-major sample {index}",
                );
            }
        }
    }

    #[test]
    fn cave_render_slice_image_honors_cancellation() {
        let grid = SliceGrid::new(0, 0, 1, 2, 2, 64, vec![SliceCell::Air; 4]);
        let cancelled: CancelToken = Arc::new(AtomicBool::new(true));
        assert_eq!(
            cave_slice_image(&grid, &cancelled),
            Err(SourceError::Cancelled),
            "a cancelled image is a typed error, never a partial result",
        );
    }

    #[test]
    fn cave_render_slice_mode_round_trips_and_clamps() {
        for y in [CAVE_SLICE_Y_MIN, 64, CAVE_SLICE_Y_MAX] {
            assert_eq!(cave_slice_y(cave_slice_mode(y)), y);
        }
        assert_eq!(cave_slice_mode(-1), cave_slice_mode(CAVE_SLICE_Y_MIN));
        assert_eq!(
            cave_slice_mode(CAVE_SLICE_Y_MAX + 1),
            cave_slice_mode(CAVE_SLICE_Y_MAX)
        );
    }

    #[test]
    fn cave_density_ramp_is_ordered_and_clamped() {
        assert_eq!(cave_density_color(0.0), Rgba([24, 30, 44, 255]));
        assert_eq!(cave_density_color(1.0), Rgba([255, 152, 48, 255]));
        assert_eq!(cave_density_color(-1.0), cave_density_color(0.0));
        assert_eq!(cave_density_color(2.0), cave_density_color(1.0));
        assert_eq!(cave_density_color(f32::NAN), cave_density_color(0.0));

        let low = cave_density_color(0.0);
        let mid = cave_density_color(0.5);
        let high = cave_density_color(1.0);
        assert!(low[0] < mid[0] && mid[0] < high[0], "red rises");
        assert!(mid[2] < high[2], "high density is the orange end");
        assert_eq!(low[3], 255, "the density map is opaque");
        assert_eq!(mid[3], 255);
        assert_eq!(high[3], 255);
    }

    #[test]
    fn cave_density_image_maps_values_row_major() {
        let densities = vec![0.0, 0.5, 0.25, 1.0];
        let grid = CaveDensityGrid::new(-4, -6, 2, 2, 2, densities.clone());
        let image =
            cave_density_image(&grid, &cancel_token()).expect("an uncancelled density image");

        assert_eq!((image.width(), image.height()), (2, 2));
        for y in 0..2u32 {
            for x in 0..2u32 {
                let index = (y * 2 + x) as usize;
                assert_eq!(
                    image.get_pixel(x, y),
                    &cave_density_color(densities[index]),
                    "pixel ({x}, {y}) is the row-major sample {index}",
                );
            }
        }
    }

    #[test]
    fn cave_density_image_honors_cancellation() {
        let grid = CaveDensityGrid::new(0, 0, 1, 2, 2, vec![0.0; 4]);
        let cancelled: CancelToken = Arc::new(AtomicBool::new(true));
        assert_eq!(
            cave_density_image(&grid, &cancelled),
            Err(SourceError::Cancelled),
            "a cancelled image is a typed error, never a partial result",
        );
    }

    #[test]
    fn cave_density_metric_version_is_nonzero() {
        assert_ne!(CAVE_DENSITY_MODE, 0, "mode 0 means 'no layer mode'");
    }
}
