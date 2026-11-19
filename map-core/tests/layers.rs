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
use map_core::source::WorldSource;
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
    use map_core::render::fluid::fluid_color;
    use map_core::render::height::height_color;
    use map_core::render::tiles::LayeredTileRenderer;
    use map_core::scheduler::{TileRenderer, TileSubject};
    use map_core::source::WorldSource;
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
        )
    }

    /// A tile key for the fake's synthetic identity at `(tx, ty)`.
    fn key(layer: Layer, tx: i32, ty: i32) -> TileKey {
        TileSubject::new(1, 7, layer, 0, ZoomLevel::MIN).key_at(tx, ty)
    }

    /// Renders one tile through the dispatcher. The fake is unbounded and
    /// instant, so this cannot fail or cancel.
    fn render_tile(renderer: &LayeredTileRenderer, layer: Layer, tx: i32, ty: i32) -> Tile {
        TileRenderer::render(renderer, &key(layer, tx, ty), &cancel_token())
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
        let result = TileRenderer::render(&renderer, &key(Layer::Height, 0, 0), &cancel);
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
            .surface_region(tile_rect_at(0, 0), 1)
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
    fn layered_render_answers_renderer_less_layers_with_unsupported() {
        // The defensive contract: the app never schedules these (its
        // radios map them to placeholders), and the dispatcher says so
        // with the typed error rather than rendering wrong pixels.
        let renderer = fake_renderer();
        for layer in [
            Layer::Surface,
            Layer::CaveSlice,
            Layer::CaveDensity,
            Layer::Zone,
        ] {
            let result = TileRenderer::render(&renderer, &key(layer, 0, 0), &cancel_token());
            assert_eq!(
                result.err(),
                Some(SourceError::Unsupported(
                    "this layer has no renderer yet (surface: T20/T21, caves: T28)"
                )),
                "{layer} has no renderer"
            );
        }
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
        .surface_region(center_rect(LAYER_GOLDEN_CENTER), GOLDEN_STEP)
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
