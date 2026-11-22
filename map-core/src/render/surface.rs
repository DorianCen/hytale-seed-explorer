//! The full-block surface layer (T21): one pixel per surface sample,
//! colored by the T20 palette formula over the sample's block and the
//! column's runtime tint.
//!
//! This is the game-map-faithful view (`docs/assets-and-palette.md` §5):
//! the base color is [`BlockPalette::color`] — `map_image.rs`'s
//! `block_color` over the block name the catalog resolves and the
//! generator's per-column `TintContainer` value — so the palette's own
//! unknown-block policy applies and every result is visible: grey+hue for
//! a catalogued name the palette lacks
//! ([`crate::palette::unknown_color`]), magenta only for an id the catalog
//! cannot name at all. Alpha is always 255, never transparent: a hole in a
//! full-block tile means the generator or the catalog failed, and the user
//! should see it.
//!
//! Wet columns then blend toward
//! [`crate::render::fluid::FLUID_COLOR`] with T17's depth curve
//! ([`fluid_blend`]) at [`DEFAULT_FLUID_OPACITY`], and the optional
//! hillshade multiplies the channels with T17's exact Lambert factor
//! ([`relief::hillshade`], with relief's channel rounding) over the
//! samples' own heights. Both are the shared functions, not copies: the
//! layers must not drift.
//!
//! The tile renderer asks for the *cancellable* surface query
//! ([`WorldSource::surface_region_cancelled`]): a full-block tile costs a
//! chunk generation per covered chunk, so a tile whose token fired stops
//! between chunk generations instead of paying for all of them first. The
//! border ring is only requested when the hillshade needs those
//! neighbours.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use crate::catalog::BlockCatalog;
use crate::coords::TILE_SIZE;
use crate::error::{SourceError, SourceResult};
use crate::palette::BlockPalette;
use crate::render::fluid::{DEFAULT_FLUID_OPACITY, fluid_blend};
use crate::render::relief;
use crate::render::tiles::render_layer_tile;
use crate::scheduler::{CancelToken, TileRenderer};
use crate::source::{SummaryFidelity, WorldSource};
use crate::tiles::{Tile, TileKey};
use crate::types::{Rect, SurfaceGrid, SurfaceSample};

use super::image::{Rgba, RgbaImage};

/// map_image.rs `rgb_from_packed`: the packed i32 tint's low 24 bits as
/// (r, g, b) — the generator's per-column TintContainer value the
/// palette formula expects (map_image.rs:327-331 parity).
///
/// The signed shifts are masked exactly like the reference; casting to
/// `u32` first makes that independent of arithmetic-shift behaviour.
#[must_use]
pub fn tint_rgb(packed: i32) -> [u8; 3] {
    let packed = packed as u32;
    [
        ((packed >> 16) & 0xFF) as u8,
        ((packed >> 8) & 0xFF) as u8,
        (packed & 0xFF) as u8,
    ]
}

/// One surface sample's full-block color: the T20 palette formula over
/// the block id and the column's runtime tint (the palette's own
/// unknown-block policy makes every result visible, never transparent
/// — magenta only when the catalog cannot even name the id).
///
/// A wet column blends the base toward
/// [`crate::render::fluid::FLUID_COLOR`] with [`fluid_blend`] at
/// [`DEFAULT_FLUID_OPACITY`], T17's exact curve. Alpha 255.
#[must_use]
pub fn surface_color(
    sample: &SurfaceSample,
    palette: &BlockPalette,
    catalog: &BlockCatalog,
) -> [u8; 4] {
    let [r, g, b, _] = palette.color(sample.block, tint_rgb(sample.tint), catalog);
    // Dry columns keep the palette color untouched; the same early return
    // covers the degenerate depth-0 wet column the curve resolves to zero.
    if sample.fluid.is_none() {
        return [r, g, b, 255];
    }

    let [r, g, b] = fluid_blend([r, g, b], sample.fluid_depth, DEFAULT_FLUID_OPACITY);
    [r, g, b, 255]
}

/// Colors `grid` one pixel per sample, row-major, `cancel` checked
/// every 16 rows. Output sample `(x, z)` reads the grid at
/// `(x - origin.0, z - origin.1)` — the query plan's mapping — and all
/// neighbour reads clamp to the grid's bounds (relief's edge rule).
/// Wet columns blend toward the fluid color with
/// [`fluid_blend`] at [`DEFAULT_FLUID_OPACITY`]; `hillshade` multiplies
/// the channels by [`relief::hillshade`] over the sample's own heights.
///
/// The clamped reads are why a bounded world's out-of-world margins
/// replicate their nearest in-world sample, exactly like
/// [`crate::render::relief::relief_image`] — one edge convention for the
/// layers that need a neighbour ring.
//
// Eight parameters are the task's fixed T21 API (grid, mapping, palette,
// catalog, knob, token); bundling them into a struct would change the
// signature later tasks consume. Clippy's default budget is seven.
#[allow(clippy::too_many_arguments)]
pub fn surface_image(
    grid: &SurfaceGrid,
    origin: (i32, i32),
    width: u32,
    height: u32,
    palette: &BlockPalette,
    catalog: &BlockCatalog,
    hillshade: bool,
    cancel: &CancelToken,
) -> SourceResult<RgbaImage> {
    let grid_width = grid.width as usize;
    assert!(
        grid_width > 0 && grid.height > 0,
        "a surface source grid has at least one sample per axis",
    );
    let max_x = i64::from(grid.width) - 1;
    let max_z = i64::from(grid.height) - 1;
    // The clamp is what makes edge shading a "nearest valid height"
    // extrapolation rather than a read of zero.
    let index = |x: i64, z: i64| -> usize {
        let x = x.clamp(0, max_x) as usize;
        let z = z.clamp(0, max_z) as usize;
        z * grid_width + x
    };

    let mut image = RgbaImage::new(width, height);
    for y in 0..height {
        if y.is_multiple_of(16) && cancel.load(Relaxed) {
            return Err(SourceError::Cancelled);
        }
        let gz = i64::from(y) - i64::from(origin.1);
        for x in 0..width {
            let gx = i64::from(x) - i64::from(origin.0);
            let sample = &grid.samples[index(gx, gz)];
            let [r, g, b, _] = surface_color(sample, palette, catalog);

            let pixel = if hillshade {
                let shade = relief::hillshade(
                    sample.height,
                    grid.samples[index(gx + 1, gz)].height,
                    grid.samples[index(gx - 1, gz)].height,
                    grid.samples[index(gx, gz + 1)].height,
                    grid.samples[index(gx, gz - 1)].height,
                    grid.step,
                );
                Rgba([
                    relief::shade_channel(r, shade),
                    relief::shade_channel(g, shade),
                    relief::shade_channel(b, shade),
                    255,
                ])
            } else {
                Rgba([r, g, b, 255])
            };
            image.put_pixel(x, y, pixel);
        }
    }
    Ok(image)
}

/// The full-block renderer's knobs.
#[derive(Debug, Clone)]
pub struct SurfaceOptions {
    /// The name-keyed block palette (T20's fallback until the user's pack
    /// palette is loaded).
    pub palette: Arc<BlockPalette>,
    /// The T17 hillshade on the block colors (the plan's full-block
    /// look; off for flat debugging).
    pub hillshade: bool,
}

impl Default for SurfaceOptions {
    fn default() -> Self {
        Self {
            palette: Arc::new(BlockPalette::fallback()),
            hillshade: true,
        }
    }
}

/// The surface layer's tile renderer. Border 1 when hillshade (the
/// neighbour ring for the slope), border 0 otherwise; the fetch is the
/// CANCELLED surface query — a tile whose token fired stops between
/// chunk generations instead of generating the whole 8×8 chunks first.
pub struct SurfaceTileRenderer {
    source: Arc<dyn WorldSource>,
    world: Option<Rect>,
    options: SurfaceOptions,
}

impl SurfaceTileRenderer {
    /// `world` is the generatable block rect (inclusive); `None` = unbounded.
    #[must_use]
    pub fn new(source: Arc<dyn WorldSource>, world: Option<Rect>, options: SurfaceOptions) -> Self {
        Self {
            source,
            world,
            options,
        }
    }
}

impl TileRenderer for SurfaceTileRenderer {
    fn render(
        &self,
        key: &TileKey,
        cancel: &CancelToken,
        fidelity: SummaryFidelity,
    ) -> SourceResult<Tile> {
        // The hillshade's slope needs the neighbour samples; the flat
        // render does not, and a full-block ring is a whole chunk wider.
        let border = u32::from(self.options.hillshade);
        render_layer_tile(
            self.source.as_ref(),
            key,
            cancel,
            fidelity,
            self.world,
            border,
            |source, rect, step, cancel| {
                source.surface_region_cancelled(rect, step, cancel, fidelity)
            },
            |grid, query, cancel| {
                surface_image(
                    grid,
                    query.origin,
                    TILE_SIZE,
                    TILE_SIZE,
                    &self.options.palette,
                    self.source.block_catalog(),
                    self.options.hillshade,
                    cancel,
                )
                .map(|image| (image, (0, 0)))
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use crate::catalog::{BiomeCatalog, FluidCatalog, ZoneCatalog};
    use crate::coords::ZoomLevel;
    use crate::render::Layer;
    use crate::scheduler::cancel_token;
    use crate::types::{
        BiomeGrid, BiomeId, BlockId, GeneratorIdentity, HeightGrid, Seed, SliceGrid, SpawnPoint,
        SurfaceGrid, SurfaceSample, ZoneGrid, ZoneId,
    };

    /// Records the fidelity each surface query asks for; every other layer
    /// is unused by the surface renderer.
    struct RecordingSurfaceSource {
        identity: GeneratorIdentity,
        seed: Seed,
        biomes: BiomeCatalog,
        blocks: BlockCatalog,
        fluids: FluidCatalog,
        zones: ZoneCatalog,
        fidelities: Arc<Mutex<Vec<SummaryFidelity>>>,
    }

    fn unused() -> SourceError {
        SourceError::Unsupported("not used by the surface renderer tests")
    }

    impl WorldSource for RecordingSurfaceSource {
        fn identity(&self) -> &GeneratorIdentity {
            &self.identity
        }

        fn seed(&self) -> &Seed {
            &self.seed
        }

        fn biome_at(&self, _x: i32, _z: i32) -> SourceResult<BiomeId> {
            Err(unused())
        }

        fn biome_region(&self, _rect: Rect, _step: u32) -> SourceResult<BiomeGrid> {
            Err(unused())
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
            rect: Rect,
            step: u32,
            fidelity: SummaryFidelity,
        ) -> SourceResult<SurfaceGrid> {
            self.record(rect, step, fidelity)
        }

        fn surface_region_cancelled(
            &self,
            rect: Rect,
            step: u32,
            _cancel: &CancelToken,
            fidelity: SummaryFidelity,
        ) -> SourceResult<SurfaceGrid> {
            self.record(rect, step, fidelity)
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
            &self.biomes
        }

        fn block_catalog(&self) -> &BlockCatalog {
            &self.blocks
        }

        fn fluid_catalog(&self) -> &FluidCatalog {
            &self.fluids
        }

        fn zone_catalog(&self) -> &ZoneCatalog {
            &self.zones
        }
    }

    impl RecordingSurfaceSource {
        fn record(
            &self,
            rect: Rect,
            step: u32,
            fidelity: SummaryFidelity,
        ) -> SourceResult<SurfaceGrid> {
            self.fidelities
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(fidelity);
            let (width, height) = rect.sample_dims(step);
            let sample = SurfaceSample {
                x: rect.min_x,
                z: rect.min_z,
                biome: BiomeId(0),
                height: 64,
                block: BlockId(0),
                fluid: None,
                fluid_depth: 0,
                tint: 0,
            };
            Ok(SurfaceGrid::new(
                rect.min_x,
                rect.min_z,
                step,
                width,
                height,
                vec![sample; (width * height) as usize],
            ))
        }
    }

    /// T29: the surface tile renderer is the summary-based path, so the
    /// pass fidelity must reach the source's surface query unchanged.
    #[test]
    fn fidelity_renderer_forwards_the_pass_fidelity_to_the_source() {
        let fidelities = Arc::new(Mutex::new(Vec::new()));
        let source = Arc::new(RecordingSurfaceSource {
            identity: GeneratorIdentity {
                generator_version: 0,
                world_assets: String::from("test"),
                palette_version: 0,
            },
            seed: Seed::parse("7").expect("a numeric test seed"),
            biomes: BiomeCatalog::default(),
            blocks: BlockCatalog::default(),
            fluids: FluidCatalog::default(),
            zones: ZoneCatalog::default(),
            fidelities: Arc::clone(&fidelities),
        });
        let renderer = SurfaceTileRenderer::new(
            source,
            None,
            SurfaceOptions {
                palette: Arc::new(BlockPalette::fallback()),
                hillshade: false,
            },
        );
        let key = TileKey {
            identity_hash: 1,
            generator_seed: 7,
            layer: Layer::Surface,
            mode: 0,
            zoom: ZoomLevel::MIN,
            tx: 0,
            ty: 0,
        };
        let cancel = cancel_token();
        for fidelity in [SummaryFidelity::Full, SummaryFidelity::SurfaceOnly] {
            TileRenderer::render(&renderer, &key, &cancel, fidelity)
                .expect("the recording source serves any rect");
        }
        assert_eq!(
            *fidelities
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![SummaryFidelity::Full, SummaryFidelity::SurfaceOnly],
            "the renderer must forward the pass fidelity, in order"
        );
    }
}
