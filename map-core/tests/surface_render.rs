//! T21 acceptance: the full-block surface layer end to end.
//!
//! Two kinds of coverage, mirroring the task's test list and the
//! `tests/layers.rs` idiom:
//!
//! - **Synthetic grids** (the fake source, `test-support`-gated): a
//!   surface tile colors exactly the T20 palette formula over the source's
//!   own samples; an unknown block stays visible grey+hue (never
//!   transparent); wet columns follow T17's fluid depth curve; the
//!   hillshade reproduces T17's Lambert factor (and the knob gates it);
//!   cancellation stops a tile before its query.
//! - **The real generator** (gated on the sibling checkout and the asset
//!   pack, like `tests/layers.rs`): the fidelity fixture's block (0, 0) —
//!   `Soil_Gravel_Sand_White` under `Water_Source` — renders the palette
//!   formula over the real surface path, every in-world pixel opaque; a
//!   pre-armed token returns [`SourceError::Cancelled`] before any chunk
//!   generation; and the ignored benchmark prints the cold/warm full-block
//!   tile timing for `docs/benchmarks.md`.
//!
//! The generated-palette path is *not* exercised here: a palette derived
//! from the user's pack may never be committed (`docs/assets-and-palette.md`
//! §2), so the real-data test uses the committed fallback palette.

mod common;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use common::world_dir;
use map_core::coords::ZoomLevel;
use map_core::error::SourceError;
use map_core::palette::BlockPalette;
use map_core::render::{Layer, SurfaceOptions, SurfaceTileRenderer, surface_color};
use map_core::scheduler::{CancelToken, TileRenderer, TileSubject, cancel_token};
use map_core::source::WorldSource;
use map_core::source::worldgen::{WorldgenSource, WorldgenSourceConfig};
use map_core::tiles::TileKey;
use map_core::types::Rect;

/// A tile key for a layer at `(tx, ty)` (step 1 — `ZoomLevel::MIN`).
fn key(layer: Layer, tx: i32, ty: i32) -> TileKey {
    TileSubject::new(1, 0, layer, 0, ZoomLevel::MIN).key_at(tx, ty)
}

/// The seed-0 source shared by the gated tests in this binary (one pool
/// load instead of one per test).
fn real_source() -> Arc<WorldgenSource> {
    static SOURCE: OnceLock<Arc<WorldgenSource>> = OnceLock::new();
    Arc::clone(SOURCE.get_or_init(|| {
        let world = world_dir().expect("the shared source is only built after world_dir()");
        let config = WorldgenSourceConfig::new(world, "0");
        Arc::new(WorldgenSource::new(&config).expect("the seed-0 worldgen source loads"))
    }))
}

// ---------------------------------------------------------------------------
// Synthetic grids (the fake source)
// ---------------------------------------------------------------------------

#[cfg(feature = "test-support")]
mod fake_surface {
    use super::*;

    use std::collections::BTreeMap;

    use map_core::catalog::BlockCatalog;
    use map_core::coords::{TILE_SIZE, TileCoord, tile_rect};
    use map_core::error::SourceError;
    use map_core::palette::{BlockColor, unknown_color};
    use map_core::render::fluid::{DEFAULT_FLUID_OPACITY, fluid_blend};
    use map_core::render::relief;
    use map_core::render::surface_image;
    use map_core::render::tiles::LayeredTileRenderer;
    use map_core::render::{Layer, SurfaceOptions, surface_color, tint_rgb};
    use map_core::scheduler::{CancelToken, TileRenderer, TileSubject};
    use map_core::source::WorldSource;
    use map_core::test_support::FakeWorldSource;
    use map_core::tiles::{Tile, TileKey};
    use map_core::types::{BiomeId, BlockId, Rect, SurfaceGrid, SurfaceSample};

    /// A tile key for the fake's synthetic identity at `(tx, ty)`.
    fn key(layer: Layer, tx: i32, ty: i32) -> TileKey {
        TileSubject::new(1, 7, layer, 0, ZoomLevel::MIN).key_at(tx, ty)
    }

    /// The fake's step-1 tile rect at `(tx, ty)`.
    fn tile_rect_at(tx: i32, ty: i32) -> Rect {
        tile_rect(TileCoord::new(i64::from(tx), i64::from(ty)), 1)
            .expect("the fake's step-1 tiles fit the block space")
    }

    /// A palette covering every name the fake's block catalog can produce
    /// (`Fake_Block_1..=64`), so the palette-formula tests exercise real
    /// entries rather than the unknown-block fallback (which the
    /// unknown-block test covers on purpose). `Fake_Block_1` carries the
    /// task's example color; the rest get distinct, deterministic values so
    /// a wrong block/name lookup changes the pixel.
    fn palette_covering_the_fake_blocks() -> BlockPalette {
        let mut entries = BTreeMap::new();
        for i in 1_u8..=64 {
            let rgb = if i == 1 {
                [200, 40, 10]
            } else {
                [i.wrapping_mul(3), 200 - i, 40 + i]
            };
            entries.insert(
                format!("Fake_Block_{i}"),
                BlockColor {
                    rgb,
                    biome_tint: 0.0,
                    particle: None,
                },
            );
        }
        BlockPalette::from_entries(entries)
    }

    /// Builds the dispatcher around the fake source with the surface knobs
    /// under test; the fake is unbounded and instant.
    fn renderer(
        fake: &Arc<FakeWorldSource>,
        palette: &BlockPalette,
        hillshade: bool,
    ) -> LayeredTileRenderer {
        LayeredTileRenderer::new(
            Arc::clone(fake) as Arc<dyn WorldSource>,
            None,
            DEFAULT_FLUID_OPACITY,
            SurfaceOptions {
                palette: Arc::new(palette.clone()),
                hillshade,
            },
        )
    }

    fn render_surface_tile(renderer: &LayeredTileRenderer) -> Tile {
        TileRenderer::render(renderer, &key(Layer::Surface, 0, 0), &cancel_token())
            .expect("the fake renders any tile instantly")
    }

    #[test]
    fn surface_render_tile_colors_the_palette_formula() {
        // Hillshade off: the tile is the palette formula alone, sample for
        // sample, over the fake's own surface grid — row-major, no offset.
        let fake = Arc::new(FakeWorldSource::new("7"));
        let palette = palette_covering_the_fake_blocks();
        let tile = render_surface_tile(&renderer(&fake, &palette, false));
        assert_eq!((tile.width, tile.height), (TILE_SIZE, TILE_SIZE));

        let grid = fake
            .surface_region(tile_rect_at(0, 0), 1)
            .expect("the fake serves any rect");
        let catalog = fake.block_catalog();
        for y in 0..TILE_SIZE {
            for x in 0..TILE_SIZE {
                let index = (y * TILE_SIZE + x) as usize;
                assert_eq!(
                    tile.pixel(x, y).expect("the tile is TILE_SIZE square"),
                    surface_color(&grid.samples[index], &palette, catalog),
                    "tile pixel ({x}, {y}) must be the palette formula of its sample",
                );
            }
        }

        // And the formula is really the palette entry: (16, 16) is a dry,
        // tint-0 column whose block is 19, so its pixel is the entry's
        // plain rgb — no blend, no shade.
        let sample = &grid.samples[(16 * TILE_SIZE + 16) as usize];
        assert!(sample.fluid.is_none(), "(16, 16) is dry fake land");
        let name = catalog
            .get(sample.block)
            .expect("the fake block is catalogued");
        let entry = palette
            .get(name)
            .expect("the test palette covers every fake block");
        assert_eq!(
            tile.pixel(16, 16).expect("the sample is in the tile"),
            [entry.rgb[0], entry.rgb[1], entry.rgb[2], 255],
            "a dry tint-0 column resolves to its palette entry's rgb",
        );
    }

    #[test]
    fn surface_render_unknown_blocks_stay_visible() {
        // An empty palette: every catalogued block name is unknown. The
        // pixels must be the visible grey+hue policy — never transparent,
        // never the magenta "no metadata at all" color (the catalog can
        // name every fake block).
        let fake = Arc::new(FakeWorldSource::new("7"));
        let palette = BlockPalette::from_entries(BTreeMap::new());
        let tile = render_surface_tile(&renderer(&fake, &palette, false));

        let grid = fake
            .surface_region(tile_rect_at(0, 0), 1)
            .expect("the fake serves any rect");
        let catalog = fake.block_catalog();
        for y in 0..TILE_SIZE {
            for x in 0..TILE_SIZE {
                let index = (y * TILE_SIZE + x) as usize;
                let sample = &grid.samples[index];
                let pixel = tile.pixel(x, y).expect("the tile is TILE_SIZE square");
                assert_eq!(pixel[3], 255, "an unknown block is opaque at ({x}, {y})");
                if sample.fluid.is_none() {
                    let name = catalog
                        .get(sample.block)
                        .expect("the fake block is catalogued");
                    assert_eq!(
                        pixel,
                        unknown_color(name),
                        "dry unknown block at ({x}, {y}) is the grey+hue policy",
                    );
                }
            }
        }

        // The specific case the task names: (16, 16) is dry block 19.
        assert_eq!(
            tile.pixel(16, 16).expect("the sample is in the tile"),
            unknown_color("Fake_Block_19"),
            "an unknown block is visible grey+hue, not transparent",
        );
    }

    #[test]
    fn surface_render_fluid_columns_blend_t17_curve() {
        // The fake's lakes are where height < 66 with depth 66 - height;
        // (0, 0) is the deepest (height 64, depth 2). A wet pixel must be
        // T17's exact curve over the palette base.
        let fake = Arc::new(FakeWorldSource::new("7"));
        let palette = palette_covering_the_fake_blocks();
        let tile = render_surface_tile(&renderer(&fake, &palette, false));

        let grid = fake
            .surface_region(tile_rect_at(0, 0), 1)
            .expect("the fake serves any rect");
        let catalog = fake.block_catalog();
        let sample = &grid.samples[0];
        assert_eq!(sample.fluid_depth, 2, "(0, 0) is the fake's deepest lake");
        assert!(sample.fluid.is_some(), "(0, 0) is wet");

        let base = palette.color(sample.block, tint_rgb(sample.tint), catalog);
        let expected = fluid_blend(
            [base[0], base[1], base[2]],
            sample.fluid_depth,
            DEFAULT_FLUID_OPACITY,
        );
        let pixel = tile.pixel(0, 0).expect("the sample is in the tile");
        for channel in 0..3 {
            assert_eq!(
                pixel[channel], expected[channel],
                "wet channel {channel} follows T17's depth curve",
            );
        }
        assert_eq!(pixel[3], 255, "the fluid blend stays opaque");
        assert_ne!(
            [pixel[0], pixel[1], pixel[2]],
            [base[0], base[1], base[2]],
            "the depth-2 blend actually moved the base color",
        );

        // The tile really covers both wet and dry columns.
        let (wet, dry) = grid.samples.iter().fold((0_u32, 0_u32), |counts, sample| {
            if sample.fluid.is_some() {
                (counts.0 + 1, counts.1)
            } else {
                (counts.0, counts.1 + 1)
            }
        });
        assert!(wet > 0, "the fake's lakes reach into this tile");
        assert!(dry > 0, "and its dry land does too");
    }

    /// A block catalog plus palette with one entry, for the synthetic
    /// grids: block 1 → `Test_Block`, a mid-tone rgb so shading moves.
    fn test_block() -> (BlockCatalog, BlockPalette, [u8; 3]) {
        let rgb = [128, 64, 32];
        let mut catalog = BlockCatalog::default();
        catalog
            .insert(BlockId(1), "Test_Block")
            .expect("one test block id is unique");
        let mut entries = BTreeMap::new();
        entries.insert(
            "Test_Block".to_string(),
            BlockColor {
                rgb,
                biome_tint: 0.0,
                particle: None,
            },
        );
        (catalog, BlockPalette::from_entries(entries), rgb)
    }

    /// A dry, tint-0 synthetic surface grid over the given heights.
    fn synthetic_grid(heights: &[i32], width: u32, height: u32) -> SurfaceGrid {
        let mut samples = Vec::with_capacity(heights.len());
        for z in 0..height {
            for x in 0..width {
                samples.push(SurfaceSample {
                    x: x as i32,
                    z: z as i32,
                    biome: BiomeId(0),
                    height: heights[(z * width + x) as usize],
                    block: BlockId(1),
                    fluid: None,
                    fluid_depth: 0,
                    tint: 0,
                });
            }
        }
        SurfaceGrid::new(0, 0, 1, width, height, samples)
    }

    #[test]
    fn surface_render_hillshade_follows_the_relief_formula() {
        let (catalog, palette, rgb) = test_block();

        // Flat ground: the normal is straight up, so every pixel is the
        // base color times relief's flat factor — one shared formula.
        let flat = synthetic_grid(&[70; 16], 4, 4);
        let shaded = surface_image(
            &flat,
            (0, 0),
            4,
            4,
            &palette,
            &catalog,
            true,
            &cancel_token(),
        )
        .expect("an uncancelled flat surface image");
        let flat_factor = relief::hillshade(70, 70, 70, 70, 70, 1);
        let first = *shaded.get_pixel(0, 0);
        for y in 0..4u32 {
            for x in 0..4u32 {
                assert_eq!(
                    shaded.get_pixel(x, y),
                    &first,
                    "flat ground shades uniformly at ({x}, {y})",
                );
            }
        }
        for (channel, base) in first.0.iter().take(3).zip(rgb) {
            let expected = f32::from(base) * flat_factor;
            let diff = (f32::from(*channel) - expected).abs();
            // The same 1% tolerance relief's own flat test uses: it absorbs
            // the f32 shade rounding and the channel rounding.
            assert!(
                diff <= expected * 0.01 + 1.0,
                "channel {channel} is within 1% of {expected}",
            );
        }
        assert_eq!(first[3], 255, "hillshade never touches alpha");

        // A slope with the knob off is exactly the unshaded palette colors;
        // with the knob on it visibly shades — so the flag really gates the
        // math rather than always running it.
        let mut heights = Vec::new();
        for z in 0..3 {
            for x in 0..3 {
                heights.push(60 + x + z);
            }
        }
        let slope = synthetic_grid(&heights, 3, 3);
        let off = surface_image(
            &slope,
            (0, 0),
            3,
            3,
            &palette,
            &catalog,
            false,
            &cancel_token(),
        )
        .expect("an uncancelled flat surface image");
        let on = surface_image(
            &slope,
            (0, 0),
            3,
            3,
            &palette,
            &catalog,
            true,
            &cancel_token(),
        )
        .expect("an uncancelled shaded surface image");
        for y in 0..3u32 {
            for x in 0..3u32 {
                let index = (y * 3 + x) as usize;
                assert_eq!(
                    off.get_pixel(x, y).0,
                    surface_color(&slope.samples[index], &palette, &catalog),
                    "hillshade off is the unshaded palette color at ({x}, {y})",
                );
            }
        }
        assert!(
            (0..3u32).any(|y| (0..3u32).any(|x| on.get_pixel(x, y) != off.get_pixel(x, y))),
            "hillshade on shades the slope differently",
        );
    }

    #[test]
    fn surface_render_tile_cancellation_stops_before_the_query() {
        // The task's cancellation row: a cancelled tile never begins its
        // surface query — the skeleton's pre-query token check.
        let fake = Arc::new(FakeWorldSource::new("7"));
        let palette = palette_covering_the_fake_blocks();
        let renderer = renderer(&fake, &palette, false);
        let cancelled: CancelToken = Arc::new(AtomicBool::new(true));
        assert_eq!(
            TileRenderer::render(&renderer, &key(Layer::Surface, 0, 0), &cancelled),
            Err(SourceError::Cancelled),
            "a cancelled surface tile never begins its work unit",
        );
        assert!(
            TileRenderer::render(&renderer, &key(Layer::Surface, 0, 0), &cancel_token()).is_ok(),
            "an uncancelled surface tile renders normally",
        );
    }
}

// ---------------------------------------------------------------------------
// The real generator
// ---------------------------------------------------------------------------

#[test]
fn surface_render_real_tile_matches_the_fixture_blocks() {
    let Some(_world) = world_dir() else {
        return;
    };
    let source = real_source();
    let palette = BlockPalette::fallback();

    // The tile renderer always paints a TILE_SIZE canvas, so a 16×16 world
    // clip keeps the query — and the one chunk generation it costs — to
    // the fidelity fixture's own chunk while still exercising the whole
    // renderer path (fetch, palette formula, blit).
    let world = Rect::new(0, 0, 15, 15);
    let renderer = SurfaceTileRenderer::new(
        Arc::clone(&source) as Arc<dyn WorldSource>,
        Some(world),
        SurfaceOptions {
            palette: Arc::new(palette.clone()),
            hillshade: false,
        },
    );
    let tile = TileRenderer::render(&renderer, &key(Layer::Surface, 0, 0), &cancel_token())
        .expect("the fixture tile renders");

    let grid = source
        .surface_region(world, 1)
        .expect("the fixture surface region succeeds");
    let catalog = source.block_catalog();
    let sample = &grid.samples[0];
    assert_eq!(
        (sample.x, sample.z),
        (0, 0),
        "the fixture case is block (0, 0)"
    );
    assert_eq!(
        catalog.get(sample.block),
        Some("Soil_Gravel_Sand_White"),
        "the catalog resolves the fixture block's id to its name",
    );
    let fluid = sample.fluid.expect("the fixture column is wet");
    assert_eq!(
        source.fluid_catalog().get(fluid),
        Some("Water_Source"),
        "the fixture column's fluid is Water_Source",
    );
    assert!(
        palette.get("Soil_Gravel_Sand_White").is_some(),
        "the committed fallback palette covers the fixture block",
    );

    // Self-consistency over the real data path: the rendered tile is the
    // palette formula over the same grid `surface_region` reports —
    // including the generator's actual tint and fluid depth, which the
    // fallback entry (biome_tint 0.0) and the shared blend fold in.
    for y in 0..16u32 {
        for x in 0..16u32 {
            let index = (y * 16 + x) as usize;
            let pixel = tile.pixel(x, y).expect("the tile is TILE_SIZE square");
            assert_eq!(
                pixel,
                surface_color(&grid.samples[index], &palette, catalog),
                "fixture pixel ({x}, {y}) matches the palette formula over the real sample",
            );
            assert_eq!(
                pixel[3], 255,
                "every in-world fixture pixel is opaque, never transparent",
            );
        }
    }
}

#[test]
fn surface_region_cancelled_returns_before_generating() {
    let Some(_world) = world_dir() else {
        return;
    };
    let source = real_source();
    let cancel: CancelToken = Arc::new(AtomicBool::new(true));

    // A pre-armed token: the per-chunk gate fires on the first iteration,
    // so even a 256-chunk request returns without generating anything.
    let started = Instant::now();
    let result = source.surface_region_cancelled(Rect::from_dims(0, 0, 512, 512), 1, &cancel);
    let elapsed = started.elapsed();
    assert_eq!(
        result,
        Err(SourceError::Cancelled),
        "a pre-armed caller token returns Cancelled, not a generation error",
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "the cancel gate fires before chunk work (took {elapsed:?})",
    );

    // The plain query is unaffected by the external token — it re-arms its
    // own and serves normally.
    let grid = source
        .surface_region(Rect::from_dims(0, 0, 1, 1), 1)
        .expect("the source still serves a tiny region after a caller cancel");
    assert_eq!((grid.width, grid.height), (1, 1));
}

/// The T21 benchmark, run manually in release (the crate's debug profile
/// makes a cold 64-chunk tile take minutes):
///
/// ```sh
/// cargo test -p map-core --features test-support --release \
///     --test surface_render -- --ignored --nocapture
/// ```
///
/// It prints a Markdown table ready for `docs/benchmarks.md`.
#[test]
#[ignore = "the T21 full-block benchmark — run manually in release (docs/benchmarks.md §7)"]
fn surface_bench_step_one_tiles_cold_warm_and_worker_scaling() {
    let Some(world) = world_dir() else {
        return;
    };
    let palette = Arc::new(BlockPalette::fallback());
    let key = key(Layer::Surface, 0, 0);
    let chunks = 64;

    let mut rows = Vec::new();
    for workers in [1_usize, 4, 12] {
        let config = WorldgenSourceConfig {
            workers: Some(workers),
            ..WorldgenSourceConfig::new(world.clone(), "0")
        };
        let source: Arc<dyn WorldSource> = match WorldgenSource::new(&config) {
            Ok(source) => Arc::new(source),
            Err(error) => {
                eprintln!("skipping the full-block benchmark: the source did not load: {error}");
                return;
            }
        };
        let renderer = SurfaceTileRenderer::new(
            Arc::clone(&source),
            None,
            SurfaceOptions {
                palette: Arc::clone(&palette),
                hillshade: false,
            },
        );

        // Tile (0, 0) at step 1 covers blocks 0..=255: 8×8 chunks, 64 in
        // total (the border-0 query — hillshade off — adds no ring chunk).
        let cold_started = Instant::now();
        let cold_result = TileRenderer::render(&renderer, &key, &cancel_token());
        let cold = cold_started.elapsed().as_secs_f64();
        if let Err(error) = cold_result {
            eprintln!("skipping the full-block benchmark: cold tile failed: {error}");
            return;
        }

        // The summary cache is per source, so the immediate repeat is warm.
        let warm_started = Instant::now();
        let warm_result = TileRenderer::render(&renderer, &key, &cancel_token());
        let warm = warm_started.elapsed().as_secs_f64();
        if let Err(error) = warm_result {
            eprintln!("skipping the full-block benchmark: warm tile failed: {error}");
            return;
        }

        rows.push((workers, cold, warm));
    }

    eprintln!(
        "\nT21 full-block tile benchmark — seed 0, 256×256 @ step 1 \
         ({chunks} chunks), fallback palette, no hillshade\n\n\
         | workers | cold (s) | warm (s) | chunks |\n\
         |---|---|---|---|"
    );
    for (workers, cold, warm) in rows {
        eprintln!("| {workers} | {cold:.2} | {warm:.3} | {chunks} |");
    }
}
