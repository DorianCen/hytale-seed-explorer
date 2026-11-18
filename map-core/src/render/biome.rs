//! The biome layer (T11 flat path, T13 palette refinements).
//!
//! One palette, grounded in worldgen: [`BiomeInfo::map_color`] is the
//! source of truth (the generator's own `Biome::get_map_color()`, channel
//! order `0x00RRGGBB` fixed by the T08 catalog — see
//! [`BiomeInfo::map_color_rgb`]). No name-hash fallbacks: an id the
//! catalog cannot resolve renders magenta ([`UNKNOWN_BIOME_COLOR`]) and
//! is logged once, so a worldgen data bug is visible, never hidden.
//!
//! Two render paths:
//!
//! - [`render_biome_rgba`] — flat, the default layer and the golden
//!   (`tests/render_golden.rs`): one pixel per sample, row-major, north
//!   (−Z) at image row 0.
//! - [`render_biome_shaded`] — the optional subtle hillshade hook,
//!   disabled by default; the app stays flat until T17 wires height data
//!   in (the default layer must never depend on a full-block query).

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use crate::catalog::BiomeCatalog;
use crate::types::{BiomeGrid, BiomeId, HeightGrid};

use super::image::{Rgba, RgbaImage};

/// The color for data the catalog cannot resolve: magenta.
pub const UNKNOWN_BIOME_COLOR: [u8; 3] = [255, 0, 255];

/// Hillshade strength: multiplier change per block of west/north rise.
/// Subtle on purpose — this shades the biome layer, it does not replace
/// the height layer (T17 owns that).
const HILLSHADE_PER_BLOCK: f32 = 0.01;

/// The subtle hillshade range: never darker/brighter than these.
const HILLSHADE_MIN: f32 = 0.75;
const HILLSHADE_MAX: f32 = 1.25;

/// The ids already reported as unknown, so each is logged exactly once
/// per process (a full-viewport grid hits the same missing id tens of
/// thousands of times).
fn logged_unknown_ids() -> &'static Mutex<HashSet<i32>> {
    static IDS: OnceLock<Mutex<HashSet<i32>>> = OnceLock::new();
    IDS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Warns once per unknown id.
fn log_unknown_id_once(id: BiomeId) {
    let mut seen = logged_unknown_ids()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if seen.insert(id.0) {
        log::warn!(
            "biome id {} is missing from the catalog; rendering it magenta (logged once per id)",
            id.0,
        );
    }
}

/// Resolves a biome id to its RGBA pixel: the catalog's map color with
/// alpha 255, or magenta for an id the catalog cannot resolve (logged
/// once).
#[must_use]
pub fn biome_pixel(id: BiomeId, catalog: &BiomeCatalog) -> Rgba<u8> {
    match catalog.get(id) {
        Some(info) => {
            let [r, g, b] = info.map_color_rgb();
            Rgba([r, g, b, 255])
        }
        None => {
            log_unknown_id_once(id);
            Rgba([
                UNKNOWN_BIOME_COLOR[0],
                UNKNOWN_BIOME_COLOR[1],
                UNKNOWN_BIOME_COLOR[2],
                255,
            ])
        }
    }
}

/// Colors `grid` into a `grid.width × grid.height` image, one pixel per
/// sample, row-major (`z = 0` first, which is image row `y = 0` — north
/// at the top).
///
/// Flat and deterministic: this is the golden path shared by the CLI, the
/// golden test and the app texture. Pure and infallible — allocation
/// aside, this cannot fail.
#[must_use]
pub fn render_biome_rgba(grid: &BiomeGrid, catalog: &BiomeCatalog) -> RgbaImage {
    let mut image = RgbaImage::new(grid.width, grid.height);
    for y in 0..grid.height {
        for x in 0..grid.width {
            let id = grid.biomes[(y as usize) * (grid.width as usize) + (x as usize)];
            image.put_pixel(x, y, biome_pixel(id, catalog));
        }
    }
    image
}

/// Colors `grid` like [`render_biome_rgba`], then multiplies in a subtle
/// hillshade from `heights`.
///
/// The hook the plan calls "optional subtle hillshade, disabled by
/// default": the app renders flat until T17 provides heights for the same
/// sampled rect (asserted: same width/height, and the caller must pass a
/// height grid covering the same samples). Illumination comes from the
/// north-west, matching the map convention (light from the top-left).
#[must_use]
pub fn render_biome_shaded(
    grid: &BiomeGrid,
    heights: &HeightGrid,
    catalog: &BiomeCatalog,
) -> RgbaImage {
    assert_eq!(
        (heights.width, heights.height),
        (grid.width, grid.height),
        "the hillshade hook needs heights for the same sampled rect: \
         biome {}x{} vs heights {}x{}",
        grid.width,
        grid.height,
        heights.width,
        heights.height,
    );
    let mut image = RgbaImage::new(grid.width, grid.height);
    for y in 0..grid.height {
        for x in 0..grid.width {
            let id = grid.biomes[(y as usize) * (grid.width as usize) + (x as usize)];
            let Rgba([r, g, b, _]) = biome_pixel(id, catalog);
            let shade = hillshade_multiplier(heights, x, y);
            image.put_pixel(
                x,
                y,
                Rgba([
                    (r as f32 * shade).round().clamp(0.0, 255.0) as u8,
                    (g as f32 * shade).round().clamp(0.0, 255.0) as u8,
                    (b as f32 * shade).round().clamp(0.0, 255.0) as u8,
                    255,
                ]),
            );
        }
    }
    image
}

/// The hillshade multiplier for one sample: brighter when the terrain
/// rises toward the west/north neighbours, darker when it falls, flat
/// terrain exactly 1.0. Edges clamp to the sample itself (a flat border).
#[must_use]
pub fn hillshade_multiplier(heights: &HeightGrid, x: u32, y: u32) -> f32 {
    let width = heights.width as usize;
    let index = (y as usize) * width + (x as usize);
    let height = heights.heights[index] as f32;
    // "West" is the previous column (−X), "north" the previous row (−Z):
    // both are toward the image's top-left where the light comes from.
    let west = if x > 0 {
        heights.heights[index - 1] as f32
    } else {
        height
    };
    let north = if y > 0 {
        heights.heights[index - width] as f32
    } else {
        height
    };
    let slope = (height - west) + (height - north);
    (1.0 + slope * HILLSHADE_PER_BLOCK).clamp(HILLSHADE_MIN, HILLSHADE_MAX)
}

/// Renders a [`BiomeGrid`] with the catalog's map colors — the
/// [`super::LayerRenderer`] shape over [`render_biome_rgba`].
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

impl super::LayerRenderer for BiomeRenderer<'_> {
    type Input = BiomeGrid;

    fn render(&self, input: &Self::Input) -> crate::error::SourceResult<RgbaImage> {
        Ok(render_biome_rgba(input, self.catalog))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::BiomeInfo;
    use crate::render::LayerRenderer;

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
    fn render_biome_rgba_maps_ids_to_map_colors_row_major() {
        // id 1 -> 0x112233 (r=17, g=34, b=51), id 2 -> 0x445566.
        let catalog = catalog_with(&[(1, 0x11_22_33), (2, 0x44_55_66)]);
        // Row-major: index 0 -> pixel (0,0), 1 -> (1,0), 2 -> (0,1), 3 -> (1,1).
        let grid = grid_2x2([1, 2, 2, 1]);
        let image = render_biome_rgba(&grid, &catalog);

        assert_eq!((image.width(), image.height()), (2, 2));
        assert_eq!(image.get_pixel(0, 0), &Rgba([0x11, 0x22, 0x33, 255]));
        assert_eq!(image.get_pixel(1, 0), &Rgba([0x44, 0x55, 0x66, 255]));
        assert_eq!(image.get_pixel(0, 1), &Rgba([0x44, 0x55, 0x66, 255]));
        assert_eq!(image.get_pixel(1, 1), &Rgba([0x11, 0x22, 0x33, 255]));
    }

    #[test]
    fn render_biome_rgba_north_is_image_row_zero() {
        // Orientation contract: z = 0 (north) is the TOP image row. Two
        // rows with different ids must keep that order — a flipped image
        // would swap them.
        let catalog = catalog_with(&[(1, 0xFF_00_00), (2, 0x00_00_FF)]);
        let grid = BiomeGrid::new(0, -10, 2, 1, 2, vec![BiomeId(1), BiomeId(2)]);
        let image = render_biome_rgba(&grid, &catalog);
        assert_eq!(
            image.get_pixel(0, 0),
            &Rgba([255, 0, 0, 255]),
            "z = -10 (north)"
        );
        assert_eq!(
            image.get_pixel(0, 1),
            &Rgba([0, 0, 255, 255]),
            "z = -8 (south)"
        );
    }

    #[test]
    fn render_biome_rgba_marks_unknown_ids_magenta() {
        let catalog = catalog_with(&[(1, 0x00_00_00)]);
        let grid = grid_2x2([1, 999, 1, 1]);
        let image = render_biome_rgba(&grid, &catalog);

        // Unknown id 999: magenta so the gap is visible, alpha still 255.
        assert_eq!(image.get_pixel(1, 0), &Rgba([255, 0, 255, 255]));
        // Known ids are untouched.
        assert_eq!(image.get_pixel(0, 0), &Rgba([0, 0, 0, 255]));
    }

    #[test]
    fn render_biome_rgba_dimensions_and_byte_length_follow_the_grid() {
        let catalog = catalog_with(&[(1, 0x10_20_30)]);
        let grid = BiomeGrid::new(-9, -7, 2, 3, 2, vec![BiomeId(1); 6]);
        let image = render_biome_rgba(&grid, &catalog);
        assert_eq!((image.width(), image.height()), (3, 2));
        // The exact buffer invariant the app's ColorImage conversion
        // relies on: width * height * 4 bytes, row-major.
        assert_eq!(image.as_raw().len(), 3 * 2 * 4);
        assert_eq!(image.as_raw(), &[0x10u8, 0x20, 0x30, 255].repeat(6));
    }

    #[test]
    fn unknown_ids_are_reported_exactly_once() {
        // The bookkeeping behind the once-per-id warning: only the first
        // report of an id claims it; the magenta render above already
        // claimed 999 through biome_pixel, so use a fresh id here.
        let mut seen = logged_unknown_ids()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(seen.insert(-777), "the first report claims the id");
        assert!(!seen.insert(-777), "the second report is silent");
    }

    #[test]
    fn render_biome_shaded_is_subtle_and_flat_where_flat() {
        let catalog = catalog_with(&[(1, 0xFF_FF_FF)]);
        let grid = BiomeGrid::new(0, 0, 1, 2, 2, vec![BiomeId(1); 4]);

        // Flat terrain: every multiplier exactly 1.0, pixels unchanged.
        let flat = HeightGrid::new(0, 0, 1, 2, 2, vec![64, 64, 64, 64]);
        assert_eq!(
            render_biome_shaded(&grid, &flat, &catalog),
            render_biome_rgba(&grid, &catalog)
        );

        // A 30-block rise toward the light (west+north both higher) brightens;
        // a drop darkens — both inside the subtle clamp range.
        let rising = HeightGrid::new(0, 0, 1, 2, 2, vec![30, 60, 60, 90]);
        let bright = hillshade_multiplier(&rising, 1, 1);
        assert!(bright > 1.0, "rising toward the light brightens: {bright}");
        assert!(bright <= HILLSHADE_MAX, "but only subtly: {bright}");

        let falling = HeightGrid::new(0, 0, 1, 2, 2, vec![90, 60, 60, 30]);
        let dark = hillshade_multiplier(&falling, 1, 1);
        assert!(dark < 1.0, "falling away from the light darkens: {dark}");
        assert!(dark >= HILLSHADE_MIN, "but only subtly: {dark}");

        // The shaded pixels move within the clamp and stay opaque.
        let shaded = render_biome_shaded(&grid, &rising, &catalog);
        let pixel = shaded.get_pixel(1, 1);
        assert_eq!(pixel[3], 255, "shading never touches alpha");
        assert!(
            pixel[0] >= render_biome_rgba(&grid, &catalog).get_pixel(1, 1)[0],
            "the brightest sample is never darker than flat"
        );
    }

    #[test]
    #[should_panic(expected = "the hillshade hook needs heights")]
    fn render_biome_shaded_rejects_mismatched_heights() {
        let catalog = catalog_with(&[(1, 0xFF_FF_FF)]);
        let grid = BiomeGrid::new(0, 0, 1, 2, 2, vec![BiomeId(1); 4]);
        let wrong = HeightGrid::new(0, 0, 1, 3, 1, vec![0; 3]);
        let _ = render_biome_shaded(&grid, &wrong, &catalog);
    }

    #[test]
    fn biome_renderer_trait_shape_matches_the_free_function() {
        let catalog = catalog_with(&[(1, 0x11_22_33)]);
        let grid = grid_2x2([1, 1, 1, 1]);
        let renderer = BiomeRenderer::new(&catalog);
        let via_trait = renderer
            .render(&grid)
            .expect("the biome layer is infallible");
        assert_eq!(via_trait, render_biome_rgba(&grid, &catalog));
    }
}
