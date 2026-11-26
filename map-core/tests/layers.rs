//! T17 acceptance: the height, relief and fluid layers end to end.
//!
//! Two kinds of coverage, mirroring the task's test list:
//!
//! - **Synthetic grids** (the fake source, `test-support`-gated): the
//!   height tile colors exactly what the source reports, relief is
//!   seam-free across a tile boundary (the border ring's whole point),
//!   the fluid layer tints only wet columns, cancellation stops a tile
//!   before its query, and the dispatcher answers renderer-less layers
//!   with the typed "unsupported".
//! - **Fixed-seed goldens** (generator-gated, the T11 fixture case:
//!   seed 0, 64×64 at step 2 around the origin): one SHA-256 per layer
//!   over the exact PNG bytes. Any change — one pixel of ramp, the
//!   light vector, the blend, the encoder, the generator — breaks the
//!   hash; regenerate only for an intentional output change, following
//!   the fidelity-fixture update policy.
//!
//! The height golden's field was additionally cross-checked against the
//! sibling repo's own `worldgen-cli render-map --mode height` over the
//! same rect (its grayscale inverted back to heights; 4,096/4,096
//! samples agreeing within the 8-bit quantization) — the task's
//! "orientation/shape" manual check, satisfied once and recorded here.

mod common;

use common::{GOLDEN_SIZE, GOLDEN_STEP, center_rect, distinct_colors, sha256, source, world_dir};
use map_core::render::fluid::{DEFAULT_FLUID_OPACITY, fluid_image};
use map_core::render::height::height_image;
use map_core::render::relief::relief_image;
use map_core::scheduler::cancel_token;
use map_core::source::{SummaryFidelity, WorldSource};
use map_core::types::Rect;

/// The T17 layer-golden case: the shore window at `(272, -192)` — a
/// 2,048-block sweep around the origin found the area where water,
/// shore, lowland and upland meet, so every layer's golden covers real
/// variation (the sweep's own render is archived with the T17 notes in
/// `docs/benchmarks.md`).
const LAYER_GOLDEN_CENTER: (i32, i32) = (272, -192);

/// The height/relief goldens sample at step 4 (a 256-block window):
/// dense enough for the generator's column cache to stay warm, wide
/// enough to cross the shore. The fluid golden uses the T11 fixture's
/// own step-2 geometry ([`common::GOLDEN_STEP`]), re-centered on the
/// shore — the surface path pays chunk summaries per covered chunk, so
/// its window stays minimal.
const LAYER_GOLDEN_STEP: u32 = 4;

/// SHA-256 of the height golden's PNG bytes (seed 0, 64×64 @ step 4,
/// center `(272, -192)`).
const HEIGHT_GOLDEN_SHA256: &str =
    "fec2768a2e437087572e8b97b39a7343d5f6b0c8199ab192c4e452e6fa024922";

/// SHA-256 of the relief golden's PNG bytes (same case; the query
/// carries the one-sample border ring, `(-1, -1)` origin).
const RELIEF_GOLDEN_SHA256: &str =
    "405c3a0e954459cef4a694972122c2641bc57cf47fc06d7211f89ef04f4a9db7";

/// SHA-256 of the fluid golden's PNG bytes (seed 0, 64×64 @ step 2, the
/// same shore center: real wet blends over the surface path).
const FLUID_GOLDEN_SHA256: &str =
    "3f7f81d0292cc4c7f8922ce8e8d6601da7e2b71f062c2a18d8bdb5e86e93762d";

/// The block rect behind a layer-golden case: `GOLDEN_SIZE` samples per
/// axis at `step` blocks, centered on `center`.
fn case_rect(center: (i32, i32), step: u32) -> Rect {
    let blocks = i64::from(GOLDEN_SIZE) * i64::from(step);
    let half = blocks / 2;
    let (cx, cz) = (i64::from(center.0), i64::from(center.1));
    Rect::new(
        i32::try_from(cx - half).expect("the layer golden cases fit i32"),
        i32::try_from(cz - half).expect("the layer golden cases fit i32"),
        i32::try_from(cx + half - 1).expect("the layer golden cases fit i32"),
        i32::try_from(cz + half - 1).expect("the layer golden cases fit i32"),
    )
}

// ---------------------------------------------------------------------------
// Synthetic grids (the fake source)
// ---------------------------------------------------------------------------

#[cfg(feature = "test-support")]
mod fake_layers {
    use super::*;
    use std::sync::Arc;

    use map_core::coords::{TILE_SIZE, TileCoord, ZoomLevel, tile_rect};
    use map_core::error::SourceError;
    use map_core::render::Layer;
    use map_core::render::SurfaceOptions;
    use map_core::render::caves::{
        CAVE_DENSITY_MODE, cave_density_color, cave_slice_color, cave_slice_mode,
    };
    use map_core::render::fluid::fluid_color;
    use map_core::render::height::height_color;
    use map_core::render::tiles::LayeredTileRenderer;
    use map_core::render::zone_color;
    use map_core::scheduler::{TileRenderer, TileSubject};
    use map_core::source::{SummaryFidelity, WorldSource};
    use map_core::test_support::FakeWorldSource;
    use map_core::tiles::{Tile, TileKey};
    use map_core::types::SurfaceSample;

    /// The fake-source dispatcher every synthetic test renders through —
    /// the same object the app hands its scheduler.
    fn fake_renderer() -> LayeredTileRenderer {
        LayeredTileRenderer::new(
            Arc::new(FakeWorldSource::new("7")),
            None,
            DEFAULT_FLUID_OPACITY,
            SurfaceOptions::default(),
        )
    }

    /// A tile key for the fake's synthetic identity at `(tx, ty)`.
    fn key(layer: Layer, tx: i32, ty: i32) -> TileKey {
        key_with_mode(layer, 0, tx, ty)
    }

    /// A tile key with an explicit layer mode (T28's cave Y/metric).
    fn key_with_mode(layer: Layer, mode: u16, tx: i32, ty: i32) -> TileKey {
        TileSubject::new(1, 7, layer, mode, ZoomLevel::MIN).key_at(tx, ty)
    }

    /// Renders one tile through the dispatcher. The fake is unbounded and
    /// instant, so this cannot fail or cancel.
    fn render_tile(renderer: &LayeredTileRenderer, layer: Layer, tx: i32, ty: i32) -> Tile {
        TileRenderer::render(
            renderer,
            &key(layer, tx, ty),
            &cancel_token(),
            SummaryFidelity::Full,
        )
        .expect("the fake renders any tile instantly")
    }

    /// The fake's step-1 tile rect at `(tx, ty)`.
    fn tile_rect_at(tx: i32, ty: i32) -> Rect {
        tile_rect(TileCoord::new(i64::from(tx), i64::from(ty)), 1)
            .expect("the fake's step-1 tiles fit the block space")
    }

    #[test]
    fn height_render_tile_matches_the_queried_heights() {
        // The tile colors exactly what the source reports for the tile's
        // own samples — row-major, no offsets, no fabrication.
        let fake = FakeWorldSource::new("7");
        let renderer = fake_renderer();
        let tile = render_tile(&renderer, Layer::Height, 0, 0);
        assert_eq!((tile.width, tile.height), (TILE_SIZE, TILE_SIZE));

        let grid = fake
            .height_region(tile_rect_at(0, 0), 1)
            .expect("the fake serves any rect");
        for y in 0..TILE_SIZE {
            for x in 0..TILE_SIZE {
                let pixel = tile.pixel(x, y).expect("the tile is TILE_SIZE square");
                let height = grid.heights[(y * TILE_SIZE + x) as usize];
                assert_eq!(
                    pixel,
                    height_color(height).0,
                    "tile pixel ({x}, {y}) must be the ramp of the queried height"
                );
            }
        }
    }

    #[test]
    fn height_render_tile_cancellation_stops_the_region_query() {
        // The task's cancellation row: a cancelled tile never begins
        // its (large) region query — the pre-query token check.
        let renderer = fake_renderer();
        let cancel: map_core::scheduler::CancelToken =
            Arc::new(std::sync::atomic::AtomicBool::new(true));
        let result = TileRenderer::render(
            &renderer,
            &key(Layer::Height, 0, 0),
            &cancel,
            SummaryFidelity::Full,
        );
        assert_eq!(result.err(), Some(SourceError::Cancelled));
    }

    #[test]
    fn relief_render_tiles_shade_seams_free_across_boundaries() {
        // The border ring's whole point: shading the east tile's western
        // edge uses the west tile's real samples as neighbours, so two
        // adjacent tiles reproduce exactly what one wide render of the
        // combined region produces — no seam, no discontinuity.
        let fake = FakeWorldSource::new("7");
        let renderer = fake_renderer();
        let west = render_tile(&renderer, Layer::Relief, 0, 0);
        let east = render_tile(&renderer, Layer::Relief, 1, 0);

        // One render of the combined 512-block region, queried with the
        // same one-sample border (origin (-1, -1), 512x256 output).
        let bordered = Rect::new(-1, -1, 512, 256);
        let grid = fake
            .height_region(bordered, 1)
            .expect("the fake serves any rect");
        let wide = relief_image(&grid, (-1, -1), 512, 256, &cancel_token())
            .expect("the wide relief render succeeds");
        assert_eq!((wide.width(), wide.height()), (512, 256));

        for y in 0..TILE_SIZE {
            // The west tile's columns, then the east tile's, line up with
            // the wide render's columns 0..512 — the boundary column 255
            // vs 256 included, which is where a seam would live.
            for x in 0..TILE_SIZE {
                assert_eq!(
                    wide.get_pixel(x, y).0,
                    west.pixel(x, y).expect("the west tile is square"),
                    "west tile column {x} (row {y}) matches the wide render"
                );
                assert_eq!(
                    wide.get_pixel(TILE_SIZE + x, y).0,
                    east.pixel(x, y).expect("the east tile is square"),
                    "east tile column {x} (row {y}) matches the wide render"
                );
            }
        }
    }

    #[test]
    fn relief_render_tile_keeps_alpha_opaque_over_the_fake_sawtooth() {
        // End-to-end sanity over varying terrain: the relief tile paints
        // every sample (no transparent holes — extrapolated shading at
        // world edges is relief's documented edge policy) and its
        // shading actually varies with the fake's sawtooth terrain.
        let renderer = fake_renderer();
        let tile = render_tile(&renderer, Layer::Relief, 0, 0);
        let mut colors = std::collections::HashSet::new();
        for y in 0..TILE_SIZE {
            for x in 0..TILE_SIZE {
                let pixel = tile.pixel(x, y).expect("the tile is TILE_SIZE square");
                assert_eq!(pixel[3], 255, "relief paints opaque samples");
                colors.insert([pixel[0], pixel[1], pixel[2]]);
            }
        }
        assert!(colors.len() > 1, "the sawtooth terrain must shade unevenly");
    }

    #[test]
    fn fluid_render_tile_tints_wet_columns_and_passes_dry_through() {
        // The fake's lakes sit wherever its height is under the lake
        // line, so a tile around the origin always has both wet and dry
        // columns: the tile equals the fluid image of the fake's own
        // surface grid, sample for sample.
        let fake = FakeWorldSource::new("7");
        let renderer = fake_renderer();
        let tile = render_tile(&renderer, Layer::Fluid, 0, 0);
        assert_eq!((tile.width, tile.height), (TILE_SIZE, TILE_SIZE));

        let grid = fake
            .surface_region(tile_rect_at(0, 0), 1, SummaryFidelity::Full)
            .expect("the fake serves any rect");
        let catalog = fake.biome_catalog();
        let expected = fluid_image(&grid, catalog, DEFAULT_FLUID_OPACITY, &cancel_token())
            .expect("the fluid image renders");

        let (mut wet, mut dry) = (0u32, 0u32);
        for y in 0..TILE_SIZE {
            for x in 0..TILE_SIZE {
                let pixel = tile.pixel(x, y).expect("the tile is TILE_SIZE square");
                assert_eq!(
                    pixel,
                    expected.get_pixel(x, y).0,
                    "tile pixel ({x}, {y}) matches the fluid image of the same grid"
                );
                let sample: &SurfaceSample = &grid.samples[(y * TILE_SIZE + x) as usize];
                if sample.fluid.is_some() {
                    wet += 1;
                } else {
                    dry += 1;
                    // The dry columns keep the biome color untouched.
                    assert_eq!(
                        pixel,
                        fluid_color(sample, catalog, DEFAULT_FLUID_OPACITY).0,
                        "a dry column passes through untouched"
                    );
                }
            }
        }
        assert!(wet > 0, "the fake's lakes reach into this tile");
        assert!(dry > 0, "and its dry land does too");
    }

    #[test]
    fn zone_render_tile_matches_the_queried_zones() {
        // The fake reports zone 0 ("Fake_Zone") for every column, so the
        // whole tile is one flat hash color — but the pixel check still
        // proves the zone query path, the catalog lookup and the blit.
        let renderer = fake_renderer();
        let tile = render_tile(&renderer, Layer::Zone, 0, 0);
        assert_eq!((tile.width, tile.height), (TILE_SIZE, TILE_SIZE));

        let color = zone_color("Fake_Zone");
        for y in 0..TILE_SIZE {
            for x in 0..TILE_SIZE {
                assert_eq!(
                    tile.pixel(x, y).expect("the tile is TILE_SIZE square"),
                    [color[0], color[1], color[2], 255],
                    "tile pixel ({x}, {y}) must be the fake zone's color"
                );
            }
        }
    }

    #[test]
    fn cave_render_layered_slice_and_density_tiles() {
        // T28: the dispatcher now serves both cave layers. The slice's Y
        // comes from the key mode and its first pixel is the fake's cell at
        // (0, 0); density uses the metric-mode key and the matching ramp
        // color.
        let renderer = fake_renderer();
        let fake = FakeWorldSource::new("7");
        let slice_y = 64;
        let expected_cell = fake
            .slice_region(Rect::new(0, 0, 0, 0), 1, slice_y)
            .expect("the fake serves slices")
            .cells[0];
        let slice = TileRenderer::render(
            &renderer,
            &key_with_mode(Layer::CaveSlice, cave_slice_mode(slice_y), 0, 0),
            &cancel_token(),
            SummaryFidelity::Full,
        )
        .expect("the fake renders a cave slice instantly");
        assert_eq!(slice.pixel(0, 0), Some(cave_slice_color(expected_cell).0));

        // A different Y is a different key and selects a different plane:
        // find a sample the fake classifies differently at the two heights.
        let other_y = 32;
        let first_plane = fake
            .slice_region(Rect::from_dims(0, 0, 32, 32), 1, slice_y)
            .expect("the fake serves the first plane");
        let second_plane = fake
            .slice_region(Rect::from_dims(0, 0, 32, 32), 1, other_y)
            .expect("the fake serves the second plane");
        let (x, z) = (0..32)
            .flat_map(|z| (0..32).map(move |x| (x, z)))
            .find(|&(x, z)| first_plane.sample(x, z) != second_plane.sample(x, z))
            .expect("the fake's two planes differ somewhere");
        let other = TileRenderer::render(
            &renderer,
            &key_with_mode(Layer::CaveSlice, cave_slice_mode(other_y), 0, 0),
            &cancel_token(),
            SummaryFidelity::Full,
        )
        .expect("the fake renders the other slice");
        assert_ne!(other.key, slice.key);
        assert_ne!(
            other.pixel(x as u32, z as u32),
            slice.pixel(x as u32, z as u32),
            "the Y mode selects a different horizontal plane"
        );

        let expected_fraction = fake
            .cave_density_region(Rect::new(0, 0, 0, 0), 1)
            .expect("the fake serves density")
            .densities[0];
        let density = TileRenderer::render(
            &renderer,
            &key_with_mode(Layer::CaveDensity, CAVE_DENSITY_MODE, 0, 0),
            &cancel_token(),
            SummaryFidelity::Full,
        )
        .expect("the fake renders cave density instantly");
        assert_eq!(
            density.pixel(0, 0),
            Some(cave_density_color(expected_fraction).0)
        );
    }
}

// ---------------------------------------------------------------------------
// Stack settings and compositing (T19)
// ---------------------------------------------------------------------------

#[cfg(feature = "test-support")]
mod stack_blend {
    use map_core::layers::{LayerSettings, LayerStack, blend_over};
    use map_core::render::Layer;

    /// The synthetic 4x4 base's two opaque checkerboard colors.
    const GOLDEN_BASE_A: [u8; 4] = [200, 40, 10, 255];
    const GOLDEN_BASE_B: [u8; 4] = [20, 180, 90, 255];

    /// The synthetic 4x4 top's third color, drawn only on the diagonal.
    const GOLDEN_TOP_C: [u8; 4] = [10, 20, 230, 200];

    #[test]
    fn layers_stack_defaults_to_biome_alone() {
        let stack = LayerStack::new();

        // Exactly one enabled layer: the plan's default.
        let enabled: Vec<&LayerSettings> = stack.iter().filter(|s| s.enabled).collect();
        assert_eq!(enabled.len(), 1, "the default stack enables one layer");
        assert_eq!(enabled[0].id, Layer::Biome);
        assert_eq!(enabled[0].opacity, 1.0);
        assert_eq!(stack.enabled_layers(), vec![Layer::Biome]);

        // Sorted by order, Biome bottom and Zone top.
        let ids: Vec<Layer> = stack.iter().map(|s| s.id).collect();
        assert_eq!(ids.first(), Some(&Layer::Biome));
        assert_eq!(ids.last(), Some(&Layer::Zone));
        let orders: Vec<i16> = stack.iter().map(|s| s.order).collect();
        assert!(
            orders.windows(2).all(|pair| pair[0] < pair[1]),
            "the default stack is sorted bottom → top: {orders:?}"
        );

        assert_eq!(LayerStack::default(), stack, "Default delegates to new");
        assert!(!stack.is_dirty(), "a fresh stack is clean");
    }

    #[test]
    fn layers_stack_toggle_opacity_and_dirty_flags() {
        let mut stack = LayerStack::new();

        // A real change dirties; an identical write is a no-op that does
        // not re-dirty the stack.
        assert!(stack.set_enabled(Layer::Fluid, true));
        assert!(stack.is_dirty());
        assert!(stack.take_dirty());
        assert!(!stack.is_dirty());
        assert!(!stack.set_enabled(Layer::Fluid, true));
        assert!(!stack.is_dirty(), "a no-op toggle must not dirty");

        // Opacity clamps at both ends; each actual change dirties.
        assert!(stack.set_opacity(Layer::Fluid, 0.25));
        assert_eq!(stack.opacity(Layer::Fluid), 0.25);
        assert!(stack.take_dirty());
        assert!(!stack.set_opacity(Layer::Fluid, 0.25));
        assert!(!stack.is_dirty(), "a no-op opacity must not dirty");

        assert!(stack.set_opacity(Layer::Fluid, 2.0));
        assert_eq!(stack.opacity(Layer::Fluid), 1.0, "2.0 clamps to 1.0");
        assert!(stack.take_dirty());
        assert!(stack.set_opacity(Layer::Fluid, -1.0));
        assert_eq!(stack.opacity(Layer::Fluid), 0.0, "-1.0 clamps to 0.0");
        assert!(stack.take_dirty());
        assert!(stack.set_opacity(Layer::Fluid, 1.0));
        assert!(!stack.set_opacity(Layer::Fluid, 1.0));

        assert_eq!(stack.get(Layer::Fluid).map(|s| s.opacity), Some(1.0));
    }

    #[test]
    fn layers_stack_reorder_keeps_settings() {
        let mut stack = LayerStack::new();
        assert!(stack.set_enabled(Layer::Height, true));
        assert!(stack.set_opacity(Layer::Height, 0.4));

        // Snapshot every planned entry so the reorder can prove nothing
        // travels or gets lost.
        let before: Vec<LayerSettings> = stack.iter().cloned().collect();

        stack.reorder(Layer::Height, -5);

        let first = stack.iter().next().expect("the stack is never empty");
        assert_eq!(first.id, Layer::Height, "Height is now painted first");
        assert!(first.enabled, "the move keeps the layer enabled");
        assert_eq!(first.opacity, 0.4, "the move keeps the opacity");

        for settings in before.iter().filter(|s| s.id != Layer::Height) {
            assert_eq!(
                stack.get(settings.id),
                Some(settings),
                "{:?} keeps its own settings across a reorder",
                settings.id
            );
        }

        // The enabled paint order follows the new sort: Height (-5) now
        // precedes the untouched Biome (0).
        assert_eq!(stack.enabled_layers(), vec![Layer::Height, Layer::Biome]);
    }

    #[test]
    fn layers_stack_serde_round_trips() {
        let mut stack = LayerStack::new();
        assert!(stack.set_enabled(Layer::Height, true));
        assert!(stack.set_opacity(Layer::Height, 0.35));
        assert!(stack.set_enabled(Layer::Fluid, true));
        assert!(stack.set_enabled(Layer::Biome, false));
        stack.reorder(Layer::Fluid, -1);

        // The dirty flag is runtime bookkeeping, not state: drain it so
        // the comparison is about the settings (and prove the skip).
        assert!(stack.take_dirty());

        let json = serde_json::to_string(&stack).expect("the stack serializes");
        assert!(
            !json.contains("\"dirty\""),
            "dirty is #[serde(skip)] in {json}"
        );

        let restored: LayerStack = serde_json::from_str(&json).expect("the stack deserializes");
        assert_eq!(restored, stack);

        let enabled: Vec<Layer> = restored.enabled_layers();
        assert_eq!(enabled, vec![Layer::Fluid, Layer::Height]);
        assert_eq!(restored.opacity(Layer::Height), 0.35);
    }

    #[test]
    fn blend_over_passes_through_edges() {
        let base = [10, 20, 30, 255];

        // A fully opaque top at full opacity wins outright.
        assert_eq!(
            blend_over(base, [200, 100, 50, 255], 1.0),
            [200, 100, 50, 255]
        );

        // A fully transparent top leaves every base pixel untouched.
        for base in [base, [1, 2, 3, 200], [250, 0, 120, 64]] {
            assert_eq!(blend_over(base, [7, 8, 9, 0], 1.0), base);
            assert_eq!(blend_over(base, [7, 8, 9, 0], 0.5), base);
        }

        // Opacity 0 is the same transparent edge, whatever the top says.
        for base in [base, [1, 2, 3, 200]] {
            assert_eq!(blend_over(base, [200, 100, 50, 255], 0.0), base);
        }
    }

    #[test]
    fn blend_over_half_opacity_over_opaque_base_is_exact() {
        // Opaque red at 50% over opaque blue: a_top = 1.0 * 0.5 = 0.5,
        // a_base = 1.0, a_out = 0.5 + 1.0 * 0.5 = 1.0.
        // r = round(255 * 0.5) = round(127.5) = 128;
        // b = round(255 * 1.0 * 0.5) = round(127.5) = 128.
        assert_eq!(
            blend_over([0, 0, 255, 255], [255, 0, 0, 255], 0.5),
            [128, 0, 128, 255]
        );

        // Top alpha 128 at opacity 1.0: a_top = 128/255 ≈ 0.501961,
        // a_base = 1.0, a_out = 1.0.
        // r = round(255 * 0.501961) = 128;
        // b = round(255 * (1 - 0.501961)) = round(127.0) = 127.
        assert_eq!(
            blend_over([0, 0, 255, 255], [255, 0, 0, 128], 1.0),
            [128, 0, 127, 255]
        );
    }

    #[test]
    fn layers_golden_composite_of_two_synthetic_layers() {
        // Two 4x4 synthetic layers, row-major. The base is an opaque
        // two-color checkerboard; the top is the third color on the main
        // diagonal at alpha 200, fully transparent elsewhere.
        let base: Vec<[u8; 4]> = (0..16)
            .map(|i| {
                let (x, y) = (i % 4, i / 4);
                if (x + y) % 2 == 0 {
                    GOLDEN_BASE_A
                } else {
                    GOLDEN_BASE_B
                }
            })
            .collect();
        let top: Vec<[u8; 4]> = (0..16)
            .map(|i| {
                let (x, y) = (i % 4, i / 4);
                if x == y { GOLDEN_TOP_C } else { [0, 0, 0, 0] }
            })
            .collect();

        let out: Vec<[u8; 4]> = base
            .iter()
            .zip(&top)
            .map(|(&base_pixel, &top_pixel)| blend_over(base_pixel, top_pixel, 0.5))
            .collect();

        // Uncovered pixels pass their checkerboard square through
        // exactly, for both colors: (2, 0) is even (A), (1, 0) is odd (B).
        assert_eq!(out[2], GOLDEN_BASE_A, "(2, 0) is base-only");
        assert_eq!(out[1], GOLDEN_BASE_B, "(1, 0) is base-only");

        // A covered diagonal pixel: a_top = (200/255) * 0.5 = 0.392157,
        // a_base = 1.0, a_out = 1.0. Over base A ([200, 40, 10]):
        // r = round(10 * 0.392157 + 200 * 0.607843) = round(125.4902) = 125;
        // g = round(20 * 0.392157 + 40 * 0.607843) = round(32.1569) = 32;
        // b = round(230 * 0.392157 + 10 * 0.607843) = round(96.2745) = 96.
        let covered = [125, 32, 96, 255];
        assert_eq!(out[5], covered, "(1, 1) blends the diagonal over A");
        assert_eq!(out[0], covered, "(0, 0), the corner, blends over A");
        assert_eq!(out[10], covered, "(2, 2) blends over A");
        assert_eq!(out[15], covered, "(3, 3) blends over A");

        // The whole composite stays opaque and actually changed where
        // the diagonal ran.
        assert!(out.iter().all(|pixel| pixel[3] == 255));
        assert_ne!(out[5], GOLDEN_BASE_A, "the diagonal really composited");
    }
}

// ---------------------------------------------------------------------------
// Fixed-seed goldens (the real generator)
// ---------------------------------------------------------------------------

/// Renders the height golden and returns the PNG bytes.
fn height_golden_png() -> Vec<u8> {
    let source = source();
    let grid = source
        .height_region(
            case_rect(LAYER_GOLDEN_CENTER, LAYER_GOLDEN_STEP),
            LAYER_GOLDEN_STEP,
        )
        .expect("the golden height region succeeds");
    let image = height_image(&grid, &cancel_token()).expect("the height image renders");
    map_core::render::encode_png(&image).expect("encoding the golden image")
}

/// Renders the relief golden (the query carries the border ring, like
/// the tile renderer and the CLI do) and returns the PNG bytes.
fn relief_golden_png() -> Vec<u8> {
    let source = source();
    let rect = case_rect(LAYER_GOLDEN_CENTER, LAYER_GOLDEN_STEP);
    let step = i64::from(LAYER_GOLDEN_STEP);
    let bordered = Rect::new(
        i32::try_from(i64::from(rect.min_x) - step).expect("the golden rect fits i32"),
        i32::try_from(i64::from(rect.min_z) - step).expect("the golden rect fits i32"),
        i32::try_from(i64::from(rect.max_x) + step).expect("the golden rect fits i32"),
        i32::try_from(i64::from(rect.max_z) + step).expect("the golden rect fits i32"),
    );
    let grid = source
        .height_region(bordered, LAYER_GOLDEN_STEP)
        .expect("the golden height region succeeds");
    let image = relief_image(&grid, (-1, -1), GOLDEN_SIZE, GOLDEN_SIZE, &cancel_token())
        .expect("the relief image renders");
    map_core::render::encode_png(&image).expect("encoding the golden image")
}

/// Renders the fluid golden and returns the PNG bytes. The step-2
/// fixture geometry is [`common::center_rect`]'s — the T11 golden case,
/// re-centered on the shore.
fn fluid_golden_png() -> Vec<u8> {
    let source = source();
    let grid = source
        .surface_region(
            center_rect(LAYER_GOLDEN_CENTER),
            GOLDEN_STEP,
            SummaryFidelity::Full,
        )
        .expect("the golden surface region succeeds");
    let image = fluid_image(
        &grid,
        source.biome_catalog(),
        DEFAULT_FLUID_OPACITY,
        &cancel_token(),
    )
    .expect("the fluid image renders");
    map_core::render::encode_png(&image).expect("encoding the golden image")
}

#[test]
fn height_render_golden_seed_0_shore() {
    if world_dir().is_none() {
        return;
    }
    let bytes = height_golden_png();
    assert_eq!(
        sha256(&bytes),
        HEIGHT_GOLDEN_SHA256,
        "the seed-0 height golden changed; see the module docs before \
         touching this hash"
    );
    // The shore window crosses several bands — a flat image would prove
    // the pipeline stopped varying with the world.
    assert!(
        distinct_colors(&bytes) >= 3,
        "the golden height map must show several bands"
    );
    assert_eq!(
        height_golden_png(),
        bytes,
        "a repeat render must be byte-identical"
    );
}

#[test]
fn relief_render_golden_seed_0_shore() {
    if world_dir().is_none() {
        return;
    }
    let bytes = relief_golden_png();
    assert_eq!(
        sha256(&bytes),
        RELIEF_GOLDEN_SHA256,
        "the seed-0 relief golden changed; see the module docs before \
         touching this hash"
    );
    // Shading multiplies the ramp continuously, so a healthy relief
    // render shows far more colors than the ramp's bands.
    assert!(
        distinct_colors(&bytes) >= 10,
        "the golden relief map must shade a spectrum"
    );
    assert_eq!(
        relief_golden_png(),
        bytes,
        "a repeat render must be byte-identical"
    );
}

#[test]
fn fluid_render_golden_seed_0_shore() {
    if world_dir().is_none() {
        return;
    }
    let bytes = fluid_golden_png();
    assert_eq!(
        sha256(&bytes),
        FLUID_GOLDEN_SHA256,
        "the seed-0 fluid golden changed; see the module docs before \
         touching this hash"
    );
    // The window sits on the shore: some pixel must carry the fluid
    // blend's signature (a color pulled well toward the documented
    // blue), proving the golden covers real wet columns.
    let decoded = image::load_from_memory(&bytes)
        .expect("the golden PNG decodes")
        .to_rgba8();
    let wet = decoded
        .pixels()
        .any(|p| p[2] > 150 && p[0] < 60 && p[3] == 255);
    assert!(wet, "the shore window must show wet blends");
    assert_eq!(
        fluid_golden_png(),
        bytes,
        "a repeat render must be byte-identical"
    );
}
