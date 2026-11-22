//! The biome layer (T11 flat path, T13 palette refinements, T16 tile
//! renderer).
//!
//! One palette, grounded in worldgen: [`BiomeInfo::map_color`] is the
//! source of truth (the generator's own `Biome::get_map_color()`, channel
//! order `0x00RRGGBB` fixed by the T08 catalog — see
//! [`BiomeInfo::map_color_rgb`]). No name-hash fallbacks: an id the
//! catalog cannot resolve renders magenta ([`UNKNOWN_BIOME_COLOR`]) and
//! is logged once, so a worldgen data bug is visible, never hidden.
//!
//! Three render paths:
//!
//! - [`render_biome_rgba`] — flat, the default layer and the golden
//!   (`tests/render_golden.rs`): one pixel per sample, row-major, north
//!   (−Z) at image row 0.
//! - [`render_biome_shaded`] — the optional subtle hillshade hook,
//!   disabled by default; the app stays flat — shading a *composite*
//!   is T19's compositing, and the shaded-relief layer itself is
//!   [`super::relief`] (T17). The default layer must never depend on a
//!   full-block query.
//! - [`BiomeTileRenderer`] — what the T15 scheduler calls per tile: one
//!   biome region query colored with [`render_biome_rgba`] on the shared
//!   [`super::tiles`] skeleton, transparent margins outside the
//!   generatable world.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use crate::catalog::BiomeCatalog;
use crate::error::SourceResult;
use crate::scheduler::{CancelToken, TileRenderer};
use crate::source::{SummaryFidelity, WorldSource};
use crate::tiles::{Tile, TileKey};
use crate::types::{BiomeGrid, BiomeId, HeightGrid, Rect};

use super::image::{Rgba, RgbaImage};
use super::tiles::render_layer_tile;

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

// ---------------------------------------------------------------------------
// Tile renderer (T16)
// ---------------------------------------------------------------------------

/// The biome layer's [`TileRenderer`] (T16): one tile = one biome region
/// query, colored with [`render_biome_rgba`].
///
/// The region query is a single work unit: the cancellation token is
/// checked before it starts (a cancelled tile never begins work), and a
/// render that completes despite its token is returned normally — the
/// scheduler caches it (the pixels are content-addressed) and reports
/// `Cancelled` itself.
///
/// **World limits.** A tile straddling the generatable edge would fail
/// the whole region query, so when `world` is given the renderer queries
/// only the in-world sub-rect — aligned to the tile's sample grid, so its
/// samples land on the same world columns the full tile would sample —
/// and leaves the out-of-world margins fully transparent (alpha 0). The
/// app's canvas shows through instead of the renderer fabricating data.
/// `world: None` means unbounded (fake sources, tests).
pub struct BiomeTileRenderer {
    source: Arc<dyn WorldSource>,
    world: Option<Rect>,
}

impl BiomeTileRenderer {
    /// Creates the renderer. `world` is the generatable block rect
    /// (inclusive bounds); `None` means the source serves any rect.
    #[must_use]
    pub fn new(source: Arc<dyn WorldSource>, world: Option<Rect>) -> Self {
        Self { source, world }
    }
}

impl TileRenderer for BiomeTileRenderer {
    fn render(
        &self,
        key: &TileKey,
        cancel: &CancelToken,
        fidelity: SummaryFidelity,
    ) -> SourceResult<Tile> {
        // The shared skeleton (see `render::tiles`): cancel check, the
        // aligned in-world query, the transparent canvas, the blit. The
        // biome layer supplies one batched region query and the catalog
        // colors; border 0 - shading is not this layer's job. Biome is a
        // direct column query, not a chunk summary, so fidelity is
        // ignored.
        render_layer_tile(
            self.source.as_ref(),
            key,
            cancel,
            fidelity,
            self.world,
            0,
            |source, rect, step, _cancel| source.biome_region(rect, step),
            |grid, query, _cancel| {
                let patch = render_biome_rgba(grid, self.source.biome_catalog());
                Ok((patch, query.origin))
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::BiomeInfo;
    use crate::render::LayerRenderer;

    use std::sync::atomic::AtomicBool;

    use crate::catalog::{BlockCatalog, FluidCatalog, ZoneCatalog};
    use crate::coords::{TILE_SIZE, TileCoord, ZoomLevel, tile_rect};
    use crate::error::SourceError;
    use crate::render::Layer;
    use crate::render::tiles::tile_query;
    use crate::scheduler::cancel_token;
    use crate::source::SummaryFidelity;
    use crate::types::{
        GeneratorIdentity, Seed, SliceGrid, SpawnPoint, SurfaceGrid, SurfaceSample, ZoneGrid,
        ZoneId,
    };

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

    // -- T16: the biome tile renderer --------------------------------------

    /// The biome id the fake source reports at world `(x, z)`:
    /// position-dependent over ids 0..=76, so a wrong query rect or blit
    /// offset necessarily changes pixels.
    fn fake_biome_id(x: i32, z: i32) -> i32 {
        x.rem_euclid(7) + z.rem_euclid(11) * 7
    }

    /// A catalog registering every id [`fake_biome_id`] can produce, each
    /// with a distinct map color (`r = g = b = id + 1` stays distinct
    /// inside 24 bits for 77 ids).
    fn fake_catalog() -> BiomeCatalog {
        let mut catalog = BiomeCatalog::default();
        for id in 0..=76 {
            catalog
                .insert(BiomeInfo {
                    id: BiomeId(id),
                    name: format!("fake biome {id}"),
                    map_color: (id as u32 + 1) * 0x0001_0101,
                })
                .expect("the fake catalog's ids are unique");
        }
        catalog
    }

    fn test_identity() -> GeneratorIdentity {
        GeneratorIdentity {
            generator_version: 0,
            world_assets: String::from("test"),
            palette_version: 0,
        }
    }

    fn zoom(level: u8) -> ZoomLevel {
        ZoomLevel::new(level).expect("the tests use in-range zoom levels")
    }

    fn tile_key(tx: i32, ty: i32, zoom: ZoomLevel) -> TileKey {
        TileKey {
            identity_hash: test_identity().stable_hash(),
            generator_seed: 1,
            layer: Layer::Biome,
            mode: 0,
            zoom,
            tx,
            ty,
        }
    }

    /// A source serving [`fake_biome_id`] over any rect and recording every
    /// biome region query. Ungated on purpose: biome.rs's tests must run
    /// without the `test-support` feature.
    struct FakeBiomeSource {
        identity: GeneratorIdentity,
        seed: Seed,
        biome_catalog: BiomeCatalog,
        block_catalog: BlockCatalog,
        fluid_catalog: FluidCatalog,
        zone_catalog: ZoneCatalog,
        queries: Arc<Mutex<Vec<(Rect, u32)>>>,
    }

    /// The failure every layer the biome tile renderer does not touch
    /// returns.
    fn unused() -> SourceError {
        SourceError::Unsupported("not used by the biome tile renderer tests")
    }

    impl WorldSource for FakeBiomeSource {
        fn identity(&self) -> &GeneratorIdentity {
            &self.identity
        }

        fn seed(&self) -> &Seed {
            &self.seed
        }

        fn biome_at(&self, _x: i32, _z: i32) -> SourceResult<BiomeId> {
            Err(unused())
        }

        fn biome_region(&self, rect: Rect, step: u32) -> SourceResult<BiomeGrid> {
            self.queries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((rect, step));
            let (width, height) = rect.sample_dims(step);
            let mut values = Vec::with_capacity((width * height) as usize);
            for row in 0..height {
                for column in 0..width {
                    let x = rect.min_x + (column * step) as i32;
                    let z = rect.min_z + (row * step) as i32;
                    values.push(BiomeId(fake_biome_id(x, z)));
                }
            }
            Ok(BiomeGrid::new(
                rect.min_x, rect.min_z, step, width, height, values,
            ))
        }

        fn height_at(&self, _x: i32, _z: i32) -> SourceResult<i32> {
            Err(unused())
        }

        fn height_region(&self, _rect: Rect, _step: u32) -> SourceResult<HeightGrid> {
            Err(unused())
        }

        fn surface_at(&self, _x: i32, _z: i32) -> SourceResult<SurfaceSample> {
            Err(unused())
        }

        fn surface_region(
            &self,
            _rect: Rect,
            _step: u32,
            _fidelity: SummaryFidelity,
        ) -> SourceResult<SurfaceGrid> {
            Err(unused())
        }

        fn slice_region(&self, _rect: Rect, _step: u32, _y: i32) -> SourceResult<SliceGrid> {
            Err(unused())
        }

        fn spawn_points(&self) -> SourceResult<Vec<SpawnPoint>> {
            Err(unused())
        }

        fn zone_at(&self, _x: i32, _z: i32) -> SourceResult<Option<ZoneId>> {
            Err(unused())
        }

        fn zone_region(&self, _rect: Rect, _step: u32) -> SourceResult<ZoneGrid> {
            Err(unused())
        }

        fn biome_catalog(&self) -> &BiomeCatalog {
            &self.biome_catalog
        }

        fn block_catalog(&self) -> &BlockCatalog {
            &self.block_catalog
        }

        fn fluid_catalog(&self) -> &FluidCatalog {
            &self.fluid_catalog
        }

        fn zone_catalog(&self) -> &ZoneCatalog {
            &self.zone_catalog
        }
    }

    struct Harness {
        renderer: BiomeTileRenderer,
        queries: Arc<Mutex<Vec<(Rect, u32)>>>,
        catalog: BiomeCatalog,
    }

    fn test_harness(world: Option<Rect>) -> Harness {
        let catalog = fake_catalog();
        let queries = Arc::new(Mutex::new(Vec::new()));
        let source = FakeBiomeSource {
            identity: test_identity(),
            seed: Seed::parse("1").expect("the test seed is numeric"),
            biome_catalog: catalog.clone(),
            block_catalog: BlockCatalog::default(),
            fluid_catalog: FluidCatalog::default(),
            zone_catalog: ZoneCatalog::default(),
            queries: Arc::clone(&queries),
        };
        Harness {
            renderer: BiomeTileRenderer::new(Arc::new(source), world),
            queries,
            catalog,
        }
    }

    fn recorded_queries(queries: &Mutex<Vec<(Rect, u32)>>) -> Vec<(Rect, u32)> {
        queries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Asserts tile pixel `(x, y)` shows the fake world sample at
    /// `(world_x, world_z)`.
    fn assert_pixel_shows_world(
        tile: &Tile,
        (x, y): (u32, u32),
        (world_x, world_z): (i32, i32),
        catalog: &BiomeCatalog,
    ) {
        let expected = biome_pixel(BiomeId(fake_biome_id(world_x, world_z)), catalog);
        assert_eq!(
            tile.pixel(x, y),
            Some(expected.0),
            "tile pixel ({x}, {y}) must show the biome at world ({world_x}, {world_z})",
        );
    }

    #[test]
    fn biome_tile_renderer_renders_the_full_tile_inside_the_world() {
        let harness = test_harness(None);
        let key = tile_key(0, 0, zoom(0));
        let tile = TileRenderer::render(
            &harness.renderer,
            &key,
            &cancel_token(),
            SummaryFidelity::Full,
        )
        .expect("an unbounded source renders every tile");

        assert_eq!((tile.width, tile.height), (TILE_SIZE, TILE_SIZE));
        // Tile (0, 0) at step 1 starts at block (0, 0): tile pixel (x, y)
        // is world block (x, y).
        for (x, y) in [
            (0, 0),
            (255, 0),
            (0, 255),
            (255, 255),
            (128, 128),
            (7, 11),
            (42, 200),
        ] {
            assert_pixel_shows_world(&tile, (x, y), (x as i32, y as i32), &harness.catalog);
        }
        assert!(
            tile.rgba
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| pixel[3] == 255),
            "every sample of an in-world tile is opaque",
        );
        assert_eq!(
            recorded_queries(&harness.queries).len(),
            1,
            "one tile is one region query",
        );

        // A world that covers the tile is no different from an unbounded
        // source: the full tile is one query and the pixels match.
        let bounded = test_harness(Some(Rect::new(-1_000, -1_000, 1_000, 1_000)));
        let covered = TileRenderer::render(
            &bounded.renderer,
            &key,
            &cancel_token(),
            SummaryFidelity::Full,
        )
        .expect("a world covering the tile renders it fully");
        let origin = tile_rect(TileCoord::new(0, 0), 1).expect("tile (0, 0) fits the i32 space");
        assert_eq!(
            recorded_queries(&bounded.queries),
            vec![(origin, 1)],
            "the whole tile lies inside the world",
        );
        assert_eq!(
            covered.rgba, tile.rgba,
            "the covered tile matches the unbounded render",
        );
    }

    #[test]
    fn biome_tile_renderer_leaves_transparent_margins_outside_the_world() {
        let world = Rect::from_dims(64, 64, 128, 128);
        let harness = test_harness(Some(world));
        let key = tile_key(0, 0, zoom(0));
        let tile = TileRenderer::render(
            &harness.renderer,
            &key,
            &cancel_token(),
            SummaryFidelity::Full,
        )
        .expect("a partially in-world tile renders");

        for (x, y) in [(0, 0), (63, 200), (200, 63), (192, 192), (255, 255)] {
            assert_eq!(
                tile.pixel(x, y).expect("the test pixels are in bounds"),
                [0, 0, 0, 0],
                "tile pixel ({x}, {y}) is outside the world and stays transparent",
            );
        }
        // The patch starts at sample (64, 64), which is world block (64, 64).
        assert_pixel_shows_world(&tile, (64, 64), (64, 64), &harness.catalog);
        assert_pixel_shows_world(&tile, (70, 90), (70, 90), &harness.catalog);
        assert_pixel_shows_world(&tile, (191, 191), (191, 191), &harness.catalog);

        assert_eq!(
            recorded_queries(&harness.queries),
            vec![(Rect::new(64, 64, 191, 191), 1)],
            "exactly the in-world sub-rect is queried, at the tile's step",
        );
    }

    #[test]
    fn biome_tile_renderer_aligns_queries_to_the_sample_grid() {
        let world = Rect::new(65, 65, 254, 254);
        let harness = test_harness(Some(world));
        let key = tile_key(0, 0, zoom(1));
        let tile = TileRenderer::render(
            &harness.renderer,
            &key,
            &cancel_token(),
            SummaryFidelity::Full,
        )
        .expect("a step-2 tile inside the world renders");

        // Sample 33 starts at block 0 + 33 * 2 = 66 (the first start >= 65);
        // the last fully inside ends at 253, so 94 samples (188 blocks).
        assert_eq!(
            recorded_queries(&harness.queries),
            vec![(Rect::new(66, 66, 253, 253), 2)],
            "the query starts on the sample grid, not at world.min",
        );
        assert_eq!(
            Rect::new(66, 66, 253, 253),
            Rect::from_dims(66, 66, 188, 188),
            "94 samples of 2 blocks",
        );

        assert_pixel_shows_world(&tile, (33, 33), (66, 66), &harness.catalog);
        assert_pixel_shows_world(&tile, (126, 126), (252, 252), &harness.catalog);
        // Sample 32 covers blocks 64..=65 and sample 127 covers 254..=255:
        // neither lies wholly inside the world, so both stay transparent.
        for (x, y) in [(32, 32), (127, 127)] {
            assert_eq!(
                tile.pixel(x, y).expect("the test pixels are in bounds")[3],
                0,
                "tile pixel ({x}, {y}) is a partially in-world sample and stays transparent",
            );
        }

        // The grid-alignment anchors on the untrimmed tile rect, through
        // the shared planner: an edge at origin + 5 starts at sample 3
        // (ceil(5 / 2)), one at origin + 9 ends at sample 5
        // (floor(10 / 2), exclusive); a world covering the tile yields
        // the full-tile query.
        let origin = tile_rect(TileCoord::new(0, 0), 2).expect("tile (0, 0) fits the i32 space");
        let plan = tile_query(origin, Some(Rect::new(5, 0, 9, 511)), 2, 0)
            .expect("the world overlaps the tile");
        assert_eq!(plan.rect, Rect::new(6, 0, 9, 511));
        assert_eq!(
            plan.origin,
            (3, 0),
            "ceil(5 / 2) = 3 keeps the query on the sample grid"
        );
        let plan = tile_query(origin, Some(origin), 2, 0).expect("the world covers the tile");
        assert_eq!(plan.rect, origin);
        assert_eq!(
            plan.origin,
            (0, 0),
            "a world covering the tile queries the whole tile"
        );
    }

    #[test]
    fn biome_tile_renderer_renders_a_transparent_tile_fully_outside_the_world() {
        let harness = test_harness(Some(Rect::from_dims(10_000, 10_000, 32, 32)));
        let key = tile_key(0, 0, zoom(0));
        let tile = TileRenderer::render(
            &harness.renderer,
            &key,
            &cancel_token(),
            SummaryFidelity::Full,
        )
        .expect("a fully out-of-world tile still renders");

        assert!(
            tile.rgba
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| pixel[3] == 0),
            "every pixel of a fully out-of-world tile is transparent",
        );
        assert!(
            recorded_queries(&harness.queries).is_empty(),
            "no region query is issued for a tile with no in-world samples",
        );
    }

    #[test]
    fn biome_tile_renderer_honors_the_cancel_token_before_the_query() {
        let harness = test_harness(None);
        let key = tile_key(0, 0, zoom(0));

        let cancelled: CancelToken = Arc::new(AtomicBool::new(true));
        assert_eq!(
            TileRenderer::render(&harness.renderer, &key, &cancelled, SummaryFidelity::Full),
            Err(SourceError::Cancelled),
            "a cancelled tile never begins its work unit",
        );
        assert!(
            recorded_queries(&harness.queries).is_empty(),
            "the cancel check comes before the region query",
        );

        assert!(
            TileRenderer::render(
                &harness.renderer,
                &key,
                &cancel_token(),
                SummaryFidelity::Full
            )
            .is_ok(),
            "an uncancelled tile renders normally",
        );
        assert_eq!(
            recorded_queries(&harness.queries).len(),
            1,
            "the uncancelled render issues its one query",
        );
    }

    #[test]
    fn biome_tile_renderer_keeps_tile_dimensions_at_coarser_steps() {
        let harness = test_harness(None);
        let key = tile_key(2, -3, zoom(2));
        let tile = TileRenderer::render(
            &harness.renderer,
            &key,
            &cancel_token(),
            SummaryFidelity::Full,
        )
        .expect("a coarser tile renders");

        assert_eq!((tile.width, tile.height), (TILE_SIZE, TILE_SIZE));
        let rect = tile_rect(TileCoord::new(2, -3), 4).expect("the tile fits the i32 block space");
        assert_eq!(rect.width(), 4 * TILE_SIZE, "step 4 spans 1024 blocks");
        assert_eq!(
            recorded_queries(&harness.queries),
            vec![(rect, 4)],
            "the query is the tile's full floor-aligned block rect",
        );

        // Sample (0, 0) is the tile's floor-aligned origin; sample (255,
        // 255) is the last column before its max bound.
        assert_pixel_shows_world(&tile, (0, 0), (rect.min_x, rect.min_z), &harness.catalog);
        assert_pixel_shows_world(
            &tile,
            (255, 255),
            (rect.min_x + 255 * 4, rect.min_z + 255 * 4),
            &harness.catalog,
        );
    }
}
