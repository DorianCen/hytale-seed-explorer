//! The water/fluid layer (T17): depth-shaded water over the surface's
//! own biome color, from T09's compact surface summaries.
//!
//! One pixel per sample, row-major, alpha 255. Dry columns pass through
//! untouched (the surface's biome map color); wet columns blend toward a
//! flat water blue with a depth attenuation that is zero at depth 0,
//! strictly increasing in depth, and asymptotic to the layer's opacity —
//! the cap that keeps deep water from going black. The surface color
//! always bleeds through, so shallow and deep water differ visibly
//! without ever hiding the terrain underneath.
//!
//! The blue is a plain, documented placeholder: T03 forbids shipping
//! asset-derived colors, and T20 may revisit the blend with fluid-catalog
//! data.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use crate::catalog::BiomeCatalog;
use crate::error::{SourceError, SourceResult};
use crate::render::biome::biome_pixel;
use crate::render::tiles::render_layer_tile;
use crate::scheduler::{CancelToken, TileRenderer};
use crate::source::{SummaryFidelity, WorldSource};
use crate::tiles::{Tile, TileKey};
use crate::types::{Rect, SurfaceGrid, SurfaceSample};

use super::image::{Rgba, RgbaImage};

/// The placeholder water color: a plain blue (T03's licensing rules —
/// no asset-derived colors).
pub const FLUID_COLOR: [u8; 3] = [50, 108, 200];

/// The default fluid blend opacity (T19's UI setting; the renderer
/// takes it as a constructor knob).
pub const DEFAULT_FLUID_OPACITY: f32 = 0.65;

/// T17's depth blend toward the fluid color over an arbitrary base:
/// `base` mixed with [`FLUID_COLOR`] by `opacity * depth / (depth + 2)`
/// per channel — zero at depth 0, strictly increasing, asymptotic to
/// `opacity`. The fluid layer and the full-block surface (T21) share it.
#[must_use]
pub fn fluid_blend(base: [u8; 3], depth: u16, opacity: f32) -> [u8; 3] {
    let depth = f32::from(depth);
    // The clamp keeps a stray out-of-range setting inside the blend's
    // meaningful domain instead of pushing a channel negative.
    let blend = opacity.clamp(0.0, 1.0) * depth / (depth + 2.0);
    let mix = |surface: u8, fluid: u8| -> u8 {
        // The clamp proves 0..=255 before the narrowing cast.
        (f32::from(surface) * (1.0 - blend) + f32::from(fluid) * blend)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    [
        mix(base[0], FLUID_COLOR[0]),
        mix(base[1], FLUID_COLOR[1]),
        mix(base[2], FLUID_COLOR[2]),
    ]
}

/// One surface sample's color: the column's biome map color, blended
/// toward [`FLUID_COLOR`] when the column is wet.
///
/// The blend is [`fluid_blend`]'s `opacity * depth / (depth + 2)`: zero at
/// depth 0 (a barely-wet column keeps its surface color), strictly
/// increasing in depth, and asymptotic to `opacity` — the cap that keeps
/// deep water from going black. Dry columns pass through untouched.
/// Alpha 255.
#[must_use]
pub fn fluid_color(sample: &SurfaceSample, catalog: &BiomeCatalog, opacity: f32) -> Rgba<u8> {
    let Rgba([r, g, b, _]) = biome_pixel(sample.biome, catalog);
    // Dry columns never tint; the same early return covers the
    // degenerate depth-0 wet column the formula resolves to zero.
    let Some(_fluid) = sample.fluid else {
        return Rgba([r, g, b, 255]);
    };

    let [r, g, b] = fluid_blend([r, g, b], sample.fluid_depth, opacity);
    Rgba([r, g, b, 255])
}

/// Colors `grid` (one pixel per sample, row-major), `cancel` checked
/// every 16 rows like the other layers.
pub fn fluid_image(
    grid: &SurfaceGrid,
    catalog: &BiomeCatalog,
    opacity: f32,
    cancel: &CancelToken,
) -> SourceResult<RgbaImage> {
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
            image.put_pixel(
                x,
                y,
                fluid_color(&grid.samples[row + x as usize], catalog, opacity),
            );
        }
    }
    Ok(image)
}

/// The fluid layer's tile renderer (border 0).
pub struct FluidTileRenderer {
    source: Arc<dyn WorldSource>,
    world: Option<Rect>,
    opacity: f32,
}

impl FluidTileRenderer {
    /// `opacity` is the blend setting ([`DEFAULT_FLUID_OPACITY`]).
    #[must_use]
    pub fn new(source: Arc<dyn WorldSource>, world: Option<Rect>, opacity: f32) -> Self {
        Self {
            source,
            world,
            opacity,
        }
    }
}

impl TileRenderer for FluidTileRenderer {
    fn render(
        &self,
        key: &TileKey,
        cancel: &CancelToken,
        fidelity: SummaryFidelity,
    ) -> SourceResult<Tile> {
        // Border 0: the depth comes from the column's own surface
        // sample; no neighbour ring is needed. Fluid is summary-based, so
        // the pass fidelity rides through to the source.
        render_layer_tile(
            self.source.as_ref(),
            key,
            cancel,
            fidelity,
            self.world,
            0,
            |source, rect, step, _cancel| source.surface_region(rect, step, fidelity),
            |grid, query, cancel| {
                fluid_image(grid, self.source.biome_catalog(), self.opacity, cancel)
                    .map(|image| (image, query.origin))
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::AtomicBool;

    use crate::catalog::BiomeInfo;
    use crate::scheduler::cancel_token;
    use crate::types::{BiomeId, BlockId, FluidId};

    fn catalog_with(entries: &[(i32, u32)]) -> BiomeCatalog {
        let mut catalog = BiomeCatalog::default();
        for &(id, color) in entries {
            catalog
                .insert(BiomeInfo {
                    id: BiomeId(id),
                    name: format!("biome {id}"),
                    map_color: color,
                })
                .expect("test entries are unique");
        }
        catalog
    }

    fn sample(biome: i32, fluid: Option<FluidId>, depth: u16) -> SurfaceSample {
        SurfaceSample {
            x: 0,
            z: 0,
            biome: BiomeId(biome),
            height: 64,
            block: BlockId(0),
            fluid,
            fluid_depth: depth,
            tint: 0,
        }
    }

    fn distance_to_fluid(pixel: &Rgba<u8>) -> u32 {
        (0..3)
            .map(|channel| {
                let diff = i32::from(pixel[channel]) - i32::from(FLUID_COLOR[channel]);
                (diff * diff) as u32
            })
            .sum()
    }

    #[test]
    fn fluid_render_dry_columns_pass_through() {
        let catalog = catalog_with(&[(1, 0x11_22_33)]);
        let dry = sample(1, None, 0);
        for opacity in [0.0, 0.5, DEFAULT_FLUID_OPACITY, 1.0] {
            assert_eq!(
                fluid_color(&dry, &catalog, opacity),
                Rgba([17, 34, 51, 255]),
                "a dry column keeps its map color at opacity {opacity}",
            );
        }
    }

    #[test]
    fn fluid_render_depth_shades_monotonically_and_never_saturates() {
        let catalog = catalog_with(&[(1, 0x20_20_20)]);
        let dry = fluid_color(&sample(1, None, 0), &catalog, DEFAULT_FLUID_OPACITY);
        assert_eq!(dry, Rgba([32, 32, 32, 255]));

        // Opacity stays at the default: at depth `u16::MAX` the blend is
        // ~0.65, so the water tint approaches but never reaches the
        // placeholder blue (the rounding at opacity 1.0 could).
        let mut previous = dry;
        for depth in [0_u16, 1, 2, 10, 100, u16::MAX] {
            let pixel = fluid_color(
                &sample(1, Some(FluidId(1)), depth),
                &catalog,
                DEFAULT_FLUID_OPACITY,
            );
            if depth == 0 {
                assert_eq!(pixel, dry, "a depth-0 wet column matches the dry pixel");
            } else {
                assert!(
                    distance_to_fluid(&pixel) < distance_to_fluid(&previous),
                    "depth {depth} is closer to the fluid color than the previous depth",
                );
            }
            assert_ne!(
                pixel,
                Rgba([FLUID_COLOR[0], FLUID_COLOR[1], FLUID_COLOR[2], 255,]),
                "depth {depth} never reaches the pure fluid color",
            );
            assert!(
                pixel.0.iter().take(3).all(|&channel| channel > 0),
                "depth {depth} never goes black",
            );
            previous = pixel;
        }
    }

    #[test]
    fn fluid_render_opacity_zero_leaves_the_surface_untouched() {
        let catalog = catalog_with(&[(1, 0x20_20_20)]);
        let wet = sample(1, Some(FluidId(1)), 10);

        assert_eq!(
            fluid_color(&wet, &catalog, 0.0),
            Rgba([32, 32, 32, 255]),
            "opacity 0 is a passthrough",
        );

        // The default opacity lands strictly between the surface color
        // and the placeholder blue on every channel that differs.
        let blended = fluid_color(&wet, &catalog, DEFAULT_FLUID_OPACITY);
        assert_eq!(blended[2], 123, "the default blend is deterministic");
        assert!(blended[0] > 32 && blended[0] < FLUID_COLOR[0]);
        assert!(blended[1] > 32 && blended[1] < FLUID_COLOR[1]);
        assert!(blended[2] > 32 && blended[2] < FLUID_COLOR[2]);
    }

    #[test]
    fn fluid_render_image_covers_the_grid_row_major() {
        let catalog = catalog_with(&[(1, 0x11_22_33), (2, 0x44_55_66)]);
        // Row-major: dry biome 1, wet depth 3, wet depth 0, dry biome 2.
        let samples = vec![
            sample(1, None, 0),
            sample(1, Some(FluidId(1)), 3),
            sample(1, Some(FluidId(1)), 0),
            sample(2, None, 0),
        ];
        let grid = SurfaceGrid::new(10, 20, 1, 2, 2, samples);
        let image = fluid_image(&grid, &catalog, DEFAULT_FLUID_OPACITY, &cancel_token())
            .expect("an uncancelled fluid image");

        assert_eq!((image.width(), image.height()), (2, 2));
        for y in 0..2u32 {
            for x in 0..2u32 {
                let index = (y * 2 + x) as usize;
                assert_eq!(
                    image.get_pixel(x, y),
                    &fluid_color(&grid.samples[index], &catalog, DEFAULT_FLUID_OPACITY),
                    "pixel ({x}, {y}) colors sample {index}",
                );
            }
        }

        let cancelled: CancelToken = Arc::new(AtomicBool::new(true));
        assert_eq!(
            fluid_image(&grid, &catalog, DEFAULT_FLUID_OPACITY, &cancelled),
            Err(SourceError::Cancelled),
            "a cancelled fluid image is a typed error, never a partial image",
        );
    }
}
