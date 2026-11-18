//! Layer renderers: grids in, RGBA pixels out (T11 and successors).
//!
//! T11 ships the first one — the flat biome layer — plus the minimal
//! [`LayerRenderer`] shape the later layers (height, relief, surface
//! blocks, caves: T13, T17, T19–T21, T28) plug into. Everything here is
//! headless: pure functions over map-core's own grid types, testable
//! without a GPU (the golden test `tests/render_golden.rs` runs the real
//! generator and hashes the PNG).
//!
//! Color rules for the biome layer (the plan's first golden, kept simple
//! on purpose — shading arrives with T13/T17):
//!
//! - [`BiomeInfo::map_color_rgb`] — the generator's own
//!   `Biome::get_map_color()`, channel order fixed by the T08 catalog;
//! - alpha 255 everywhere;
//! - an id missing from the catalog renders magenta
//!   ([`UNKNOWN_BIOME_COLOR`]) so missing data is visible, never silent.

pub mod image;

use std::fmt;
use std::str::FromStr;

use crate::catalog::BiomeCatalog;
use crate::error::{SourceError, SourceResult};
use crate::types::BiomeGrid;

pub use image::{RgbaImage, encode_png};

use self::image::Rgba;

/// The color for data the catalog cannot resolve: magenta, alpha 255.
pub const UNKNOWN_BIOME_COLOR: [u8; 3] = [255, 0, 255];

/// A renderable layer. Tile keys (T14) and the CLI `--layer` flag carry
/// this; only the biome layer exists yet — T17/T20/T28 extend the enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Layer {
    /// The flat biome map color layer.
    Biome,
}

impl fmt::Display for Layer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Layer::Biome => "biome",
        })
    }
}

impl FromStr for Layer {
    type Err = SourceError;

    /// Parses the layer names the CLI and share strings use. An unknown
    /// name is [`SourceError::Unsupported`], so the caller reports it the
    /// same way as any other unsupported query.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "biome" => Ok(Layer::Biome),
            _ => Err(SourceError::Unsupported(
                "unknown layer: only 'biome' exists (T17/T20/T28 add more)",
            )),
        }
    }
}

/// Renders one layer's grid data into a fresh [`RgbaImage`].
///
/// Minimal T11 shape: renderers own nothing (their inputs are borrowed),
/// dimensions come from the grid itself, and every renderer produces
/// exactly one pixel per sample. Later compositing tasks (T19) build on
/// this; if they need a richer contract the trait grows with them.
pub trait LayerRenderer {
    /// The grid type this layer consumes (e.g. [`BiomeGrid`]).
    type Input;

    /// Renders `input` into an RGBA image with one pixel per sample,
    /// row-major.
    fn render(&self, input: &Self::Input) -> SourceResult<RgbaImage>;
}

/// Renders a [`BiomeGrid`] with the catalog's map colors.
#[derive(Debug, Clone, Copy)]
pub struct BiomeRenderer<'a> {
    catalog: &'a BiomeCatalog,
}

impl<'a> BiomeRenderer<'a> {
    /// Creates a renderer resolving biome ids through `catalog`.
    pub fn new(catalog: &'a BiomeCatalog) -> Self {
        BiomeRenderer { catalog }
    }
}

impl LayerRenderer for BiomeRenderer<'_> {
    type Input = BiomeGrid;

    fn render(&self, input: &Self::Input) -> SourceResult<RgbaImage> {
        Ok(render_biome(input, self.catalog))
    }
}

/// Colors `grid` into a `grid.width × grid.height` image, one pixel per
/// sample, row-major (`z = 0` first, which is image row `y = 0`).
///
/// Biomes resolve through `catalog`; an unknown id renders
/// [`UNKNOWN_BIOME_COLOR`]. Pure and infallible — allocation aside, this
/// cannot fail, so the [`LayerRenderer`] wrapper returns [`Ok`] directly.
#[must_use]
pub fn render_biome(grid: &BiomeGrid, catalog: &BiomeCatalog) -> RgbaImage {
    let mut image = RgbaImage::new(grid.width, grid.height);
    for y in 0..grid.height {
        for x in 0..grid.width {
            let id = grid.biomes[(y as usize) * (grid.width as usize) + (x as usize)];
            let [r, g, b] = catalog
                .get(id)
                .map(|info| info.map_color_rgb())
                .unwrap_or(UNKNOWN_BIOME_COLOR);
            image.put_pixel(x, y, Rgba([r, g, b, 255]));
        }
    }
    image
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::BiomeInfo;
    use crate::types::BiomeId;

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

    fn grid_2x2(ids: [i32; 4]) -> BiomeGrid {
        let values = ids.map(BiomeId);
        BiomeGrid::new(0, 0, 1, 2, 2, values.to_vec())
    }

    #[test]
    fn render_biome_maps_ids_to_map_colors_row_major() {
        // id 1 -> 0x112233 (r=17, g=34, b=51), id 2 -> 0x445566.
        let catalog = catalog_with(&[(1, 0x11_22_33), (2, 0x44_55_66)]);
        // Row-major: index 0 -> pixel (0,0), 1 -> (1,0), 2 -> (0,1), 3 -> (1,1).
        let grid = grid_2x2([1, 2, 2, 1]);
        let image = render_biome(&grid, &catalog);

        assert_eq!((image.width(), image.height()), (2, 2));
        assert_eq!(image.get_pixel(0, 0), &Rgba([0x11, 0x22, 0x33, 255]));
        assert_eq!(image.get_pixel(1, 0), &Rgba([0x44, 0x55, 0x66, 255]));
        assert_eq!(image.get_pixel(0, 1), &Rgba([0x44, 0x55, 0x66, 255]));
        assert_eq!(image.get_pixel(1, 1), &Rgba([0x11, 0x22, 0x33, 255]));
    }

    #[test]
    fn render_biome_marks_unknown_ids_magenta() {
        let catalog = catalog_with(&[(1, 0x00_00_00)]);
        let grid = grid_2x2([1, 999, 1, 1]);
        let image = render_biome(&grid, &catalog);

        // Unknown id 999: magenta so the gap is visible, alpha still 255.
        assert_eq!(
            image.get_pixel(1, 0),
            &Rgba([
                UNKNOWN_BIOME_COLOR[0],
                UNKNOWN_BIOME_COLOR[1],
                UNKNOWN_BIOME_COLOR[2],
                255
            ]),
        );
        // Known ids are untouched.
        assert_eq!(image.get_pixel(0, 0), &Rgba([0, 0, 0, 255]));
    }

    #[test]
    fn render_biome_dimensions_follow_the_grid() {
        let catalog = catalog_with(&[(1, 0x10_20_30)]);
        let grid = BiomeGrid::new(-9, -7, 2, 3, 2, vec![BiomeId(1); 6]);
        let image = render_biome(&grid, &catalog);
        assert_eq!((image.width(), image.height()), (3, 2));
        // Every pixel is the same known color; origin/step do not matter to
        // the renderer, only the sample grid.
        assert!(image.pixels().all(|p| *p == Rgba([0x10, 0x20, 0x30, 255])));
    }

    #[test]
    fn biome_renderer_trait_shape_matches_the_free_function() {
        let catalog = catalog_with(&[(1, 0x11_22_33)]);
        let grid = grid_2x2([1, 1, 1, 1]);
        let renderer = BiomeRenderer::new(&catalog);
        let via_trait = renderer
            .render(&grid)
            .expect("the biome layer is infallible");
        assert_eq!(via_trait, render_biome(&grid, &catalog));
    }

    #[test]
    fn layer_parses_and_displays_its_cli_names() {
        assert_eq!("biome".parse::<Layer>(), Ok(Layer::Biome));
        assert_eq!(Layer::Biome.to_string(), "biome");
        // Unknown names are typed errors, not panics.
        assert!(matches!(
            "height".parse::<Layer>(),
            Err(SourceError::Unsupported(_))
        ));
    }
}
