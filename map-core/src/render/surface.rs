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
use crate::coords::{TILE_SIZE, TileCoord, tile_rect};
use crate::error::{SourceError, SourceResult};
use crate::palette::BlockPalette;
use crate::render::fluid::{DEFAULT_FLUID_OPACITY, fluid_blend};
use crate::render::relief;
use crate::render::tiles::{blit_patch, render_layer_tile, tile_query};
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

    /// The tile-pixel height of one streaming band at `step`.
    ///
    /// A band is one chunk row of the tile's chunk grid — the unit the
    /// summary cache and the generator work in — so it is `32 / step`
    /// pixels tall: a step-1 tile streams 8 bands of 32 rows, a step-2 tile
    /// 16 bands of 16, … a step-32 tile 256 single-row bands. Above
    /// [`CHUNK_BLOCKS`] a chunk row is smaller than a pixel: every band
    /// would publish the same pixels, so streaming returns [`None`] and the
    /// renderer takes the one-shot path (coarse views need no streaming —
    /// their tiles already group chunks).
    fn band_rows(step: u32) -> Option<u32> {
        (step >= 1 && CHUNK_BLOCKS.is_multiple_of(step)).then(|| CHUNK_BLOCKS / step)
    }
}

/// Blocks per chunk side: the height of one streaming band, in blocks.
const CHUNK_BLOCKS: u32 = 32;

/// The full-size tile snapshot a streaming band publishes: the canvas so
/// far, tagged with the pass's fidelity exactly like a finished tile. All
/// but the last snapshot are incomplete: their not-yet-drawn rows are
/// alpha 0, so they stand in as placeholders but never satisfy the
/// scheduler's authoritative cache hit.
fn snapshot(canvas: &RgbaImage, key: &TileKey, fidelity: SummaryFidelity, complete: bool) -> Tile {
    Tile::new(*key, TILE_SIZE, TILE_SIZE, canvas.as_raw().clone())
        .expect("the canvas is TILE_SIZE square with a matching buffer")
        .with_fidelity(fidelity)
        .with_complete(complete)
}

/// Fills canvas rows `0..end` with `patch`'s first row.
///
/// The one-shot render's coloring clamps every out-of-world read to the
/// nearest sample, so rows above the world repeat the first in-world row.
/// A band wholly above the world has no query, and only the first in-world
/// band knows that row — so that band back-fills the skipped rows here,
/// keeping the final tile pixel-identical to [`SurfaceTileRenderer::render`].
fn fill_rows_above(canvas: &mut RgbaImage, patch: &RgbaImage, end: u32) {
    if end == 0 {
        return;
    }
    let row: Vec<Rgba<u8>> = (0..canvas.width())
        .map(|x| *patch.get_pixel(x, 0))
        .collect();
    for y in 0..end {
        for (x, pixel) in row.iter().enumerate() {
            canvas.put_pixel(x as u32, y, *pixel);
        }
    }
}

/// Repeats canvas row `from` over the next `count` rows.
///
/// The below-the-world counterpart of [`fill_rows_above`]: the one-shot
/// render clamps every row past the world's end to the last in-world
/// sample, which is the row just above the first wholly-outside band.
fn repeat_row_below(canvas: &mut RgbaImage, from: u32, count: u32) {
    let row: Vec<Rgba<u8>> = (0..canvas.width())
        .map(|x| *canvas.get_pixel(x, from))
        .collect();
    for y in from + 1..from + 1 + count {
        for (x, pixel) in row.iter().enumerate() {
            canvas.put_pixel(x as u32, y, *pixel);
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

    /// Streams the tile one chunk row at a time: each band queries its own
    /// sub-rect (the summary cache dedups chunks across bands and across
    /// the fill/refine passes), colors that strip, publishes the full-size
    /// canvas so far, and then checks the token. The final tile covers
    /// exactly [`Self::render`]'s chunk set and is pixel-identical to it.
    fn render_streaming(
        &self,
        key: &TileKey,
        cancel: &CancelToken,
        fidelity: SummaryFidelity,
        progress: &mut dyn FnMut(Tile),
    ) -> SourceResult<Tile> {
        let step = key.zoom.step();
        // Coarse views: chunk rows are sub-pixel, so banding publishes no
        // new pixels. The one-shot path is exact and cheaper.
        let Some(band_rows) = Self::band_rows(step) else {
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
        let border = u32::from(self.options.hillshade);
        let step_i = i64::from(step);
        // The tile's frame (the bands plus the hillshade ring) is what
        // `tile_query` must see per band: with the full tile's 256-sample
        // window a band query would reach past the band and pick up a world
        // that does not touch it, or fetch the whole tile per band. Clamping
        // the world to the band frame is pixel-neutral — the frame already
        // contains every sample the band reads, border ring included — and
        // keeps each query one chunk row wide. Saturating only matters for a
        // tile at the very edge of the i32 block space, where the source is
        // out of bounds anyway.
        let ring = i32::try_from(border * step)
            .expect("the hillshade ring is at most one chunk row of blocks");
        let frame = Rect {
            min_x: tile.min_x.saturating_sub(ring),
            min_z: tile.min_z.saturating_sub(ring),
            max_x: tile.max_x.saturating_add(ring),
            max_z: tile.max_z.saturating_add(ring),
        };
        let mut canvas = RgbaImage::new(TILE_SIZE, TILE_SIZE);
        // Set once an in-world band has been drawn: bands below the world
        // then clamp to the row above; before that, the first in-world
        // band back-fills the rows above.
        let mut in_world = false;
        let mut band_top: u32 = 0;
        while band_top < TILE_SIZE {
            // A cancelled stream stops publishing; bands already handed to
            // `progress` stay in the cache as valid data (the scheduler
            // reports the cancellation).
            if cancel.load(Relaxed) {
                return Err(SourceError::Cancelled);
            }
            let band_min_z = i64::from(tile.min_z) + i64::from(band_top) * step_i;
            let band_max_z = band_min_z + (i64::from(band_rows) - 1) * step_i;
            let band = Rect::new(
                tile.min_x,
                i32::try_from(band_min_z).expect("a band lies inside the representable tile rect"),
                tile.max_x,
                i32::try_from(band_max_z).expect("a band lies inside the representable tile rect"),
            );
            let plan = match self.world {
                None => tile_query(band, None, step, border),
                Some(world) => frame
                    .intersects(&world)
                    .then(|| frame.clamp(&world))
                    .and_then(|visible| tile_query(band, Some(visible), step, border)),
            };
            match plan {
                Some(plan) => {
                    let grid = self
                        .source
                        .surface_region_cancelled(plan.rect, step, cancel, fidelity)?;
                    let patch = surface_image(
                        &grid,
                        plan.origin,
                        TILE_SIZE,
                        band_rows,
                        &self.options.palette,
                        self.source.block_catalog(),
                        self.options.hillshade,
                        cancel,
                    )?;
                    if !in_world {
                        fill_rows_above(&mut canvas, &patch, band_top);
                        in_world = true;
                    }
                    blit_patch(
                        &mut canvas,
                        &patch,
                        (
                            0,
                            i32::try_from(band_top).expect("band_top is below TILE_SIZE"),
                        ),
                    );
                }
                None => {
                    // No sample of this band's frame is in the world. Below
                    // an in-world band, the one-shot render clamps to the
                    // last in-world row; above it, the first in-world band
                    // back-fills.
                    if in_world {
                        repeat_row_below(&mut canvas, band_top - 1, band_rows);
                    }
                }
            }
            progress(snapshot(&canvas, key, fidelity, false));
            if cancel.load(Relaxed) {
                return Err(SourceError::Cancelled);
            }
            band_top += band_rows;
        }
        Ok(snapshot(&canvas, key, fidelity, true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering::Relaxed;

    use crate::catalog::{BiomeCatalog, FluidCatalog, ZoneCatalog};
    use crate::coords::ZoomLevel;
    use crate::render::Layer;
    use crate::scheduler::cancel_token;
    use crate::tiles::TileCache;
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
        /// When true, each sample varies with its world column (height,
        /// block, fluid depth and tint), so a band seam or a shifted row
        /// changes pixels. When false every sample is one constant — enough
        /// for the fidelity-forwarding test.
        varying: bool,
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
            let mut samples = Vec::with_capacity((width * height) as usize);
            let step_i = step as i32;
            for z in 0..height {
                for x in 0..width {
                    let world_x = rect.min_x + x as i32 * step_i;
                    let world_z = rect.min_z + z as i32 * step_i;
                    samples.push(self.sample_at(world_x, world_z));
                }
            }
            Ok(SurfaceGrid::new(
                rect.min_x, rect.min_z, step, width, height, samples,
            ))
        }

        /// The sample at one world column: constant unless `varying`, in
        /// which case height, block, fluid and tint all move with the
        /// column so band boundaries and row shifts are visible.
        fn sample_at(&self, x: i32, z: i32) -> SurfaceSample {
            if !self.varying {
                return SurfaceSample {
                    x,
                    z,
                    biome: BiomeId(0),
                    height: 64,
                    block: BlockId(0),
                    fluid: None,
                    fluid_depth: 0,
                    tint: 0,
                };
            }
            let cell = i32::from(x.rem_euclid(8) as u8) + 8 * i32::from(z.rem_euclid(8) as u8);
            let wet = (x + z).rem_euclid(7) == 0;
            SurfaceSample {
                x,
                z,
                biome: BiomeId(0),
                height: 64 + x.rem_euclid(17) + 2 * z.rem_euclid(13),
                block: BlockId(cell),
                fluid: wet.then_some(crate::types::FluidId(1)),
                fluid_depth: if wet { (x + z).rem_euclid(5) as u16 } else { 0 },
                tint: (x.wrapping_mul(31) ^ z.wrapping_mul(17)) & 0x00FF_FFFF,
            }
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
            varying: false,
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

    // -----------------------------------------------------------------------
    // Streaming (per-band publication)
    // -----------------------------------------------------------------------

    /// A source whose samples vary with the world column: height, block,
    /// fluid depth and tint all move, and the palette knows none of the
    /// block names (so each name's unknown-color hue differs). Enough that
    /// a band seam or a shifted row changes pixels.
    fn streaming_source() -> Arc<dyn WorldSource> {
        let mut blocks = BlockCatalog::default();
        for id in 0..64 {
            blocks
                .insert(BlockId(id), format!("Streaming_{id}"))
                .expect("the streaming test block ids are unique");
        }
        Arc::new(RecordingSurfaceSource {
            identity: GeneratorIdentity {
                generator_version: 0,
                world_assets: String::from("test"),
                palette_version: 0,
            },
            seed: Seed::parse("7").expect("a numeric test seed"),
            biomes: BiomeCatalog::default(),
            blocks,
            fluids: FluidCatalog::default(),
            zones: ZoneCatalog::default(),
            fidelities: Arc::new(Mutex::new(Vec::new())),
            varying: true,
        })
    }

    /// A surface renderer over [`streaming_source`]; the palette has no
    /// entries, so every block name takes its own unknown-color hue.
    fn streaming_renderer(world: Option<Rect>, hillshade: bool) -> SurfaceTileRenderer {
        SurfaceTileRenderer::new(
            streaming_source(),
            world,
            SurfaceOptions {
                palette: Arc::new(BlockPalette::from_entries(BTreeMap::new())),
                hillshade,
            },
        )
    }

    fn streaming_key(zoom: u8, tx: i32, ty: i32) -> TileKey {
        TileKey {
            identity_hash: 1,
            generator_seed: 7,
            layer: Layer::Surface,
            mode: 0,
            zoom: ZoomLevel::new(zoom).expect("the streaming tests use in-range levels"),
            tx,
            ty,
        }
    }

    /// Every pixel of row `y` is opaque (painted by a band).
    fn row_is_opaque(tile: &Tile, y: u32) -> bool {
        (0..TILE_SIZE).all(|x| tile.pixel(x, y).is_some_and(|pixel| pixel[3] == 255))
    }

    /// Every pixel of row `y` is the no-data value (alpha 0, all channels 0).
    fn row_is_no_data(tile: &Tile, y: u32) -> bool {
        (0..TILE_SIZE).all(|x| tile.pixel(x, y) == Some([0, 0, 0, 0]))
    }

    /// Streams `key` and returns every published band plus the final tile.
    fn stream(
        renderer: &SurfaceTileRenderer,
        key: &TileKey,
        fidelity: SummaryFidelity,
    ) -> (Vec<Tile>, Tile) {
        let cancel = cancel_token();
        let mut partials = Vec::new();
        let final_tile =
            TileRenderer::render_streaming(renderer, key, &cancel, fidelity, &mut |tile| {
                partials.push(tile);
            })
            .expect("the recording source serves every band");
        (partials, final_tile)
    }

    /// The core acceptance for band assembly: the streamed tile is the
    /// plain render's tile, pixel for pixel, and every band is a full-size
    /// snapshot with the completed rows painted and the rest no-data.
    #[test]
    fn streaming_render_matches_render_across_bands() {
        for hillshade in [false, true] {
            let renderer = streaming_renderer(None, hillshade);
            for (zoom, tx, ty) in [(0, 0, 0), (0, -1, 2), (1, 0, 0), (3, 1, -1)] {
                let key = streaming_key(zoom, tx, ty);
                let step = key.zoom.step();
                let band_rows = CHUNK_BLOCKS / step;
                let plain =
                    TileRenderer::render(&renderer, &key, &cancel_token(), SummaryFidelity::Full)
                        .expect("the plain path renders the tile");
                let (partials, streamed) = stream(&renderer, &key, SummaryFidelity::Full);

                assert_eq!(
                    plain.rgba, streamed.rgba,
                    "streamed and plain renders must be pixel-identical (zoom {zoom}, hillshade {hillshade})"
                );
                assert_eq!(
                    partials.len(),
                    (TILE_SIZE / band_rows) as usize,
                    "one publication per chunk row band"
                );
                for (index, partial) in partials.iter().enumerate() {
                    assert_eq!((partial.width, partial.height), (TILE_SIZE, TILE_SIZE));
                    assert_eq!(partial.key, key);
                    assert!(
                        !partial.complete,
                        "every published band is an incomplete tile"
                    );
                    let painted = (index as u32 + 1) * band_rows;
                    assert!(row_is_opaque(partial, painted - 1));
                    if painted < TILE_SIZE {
                        assert!(
                            row_is_no_data(partial, painted),
                            "rows past the published bands stay no-data"
                        );
                    }
                }
                assert!(
                    streamed.complete,
                    "the returned tile is complete even though the last band's snapshot is not"
                );
                assert_eq!(
                    partials.last().expect("at least one band").rgba,
                    streamed.rgba,
                    "the last publication covers the complete tile's pixels"
                );
            }
        }
    }

    /// World clipping goes through the same edge-clamp convention as the
    /// plain render: the bands above/below the world repeat the nearest
    /// in-world row instead of leaving holes, so partial coverage still
    /// matches pixel for pixel.
    #[test]
    fn streaming_render_matches_render_at_world_edges() {
        let worlds = [
            // Covers the whole tile.
            Some(Rect::new(-10_000, -10_000, 10_000, 10_000)),
            // Starts inside the tile, in the middle of a band.
            Some(Rect::new(-10_000, 100, 10_000, 10_000)),
            // Ends inside the tile, in the middle of a band.
            Some(Rect::new(-10_000, -10_000, 10_000, 200)),
            // Clipped on x only, full z.
            Some(Rect::new(64, -10_000, 180, 10_000)),
            // A small rect inside one band.
            Some(Rect::new(64, 100, 180, 120)),
            // Entirely outside the tile.
            Some(Rect::new(-10_000, 300, 10_000, 400)),
        ];
        for world in worlds {
            for hillshade in [false, true] {
                let renderer = streaming_renderer(world, hillshade);
                let key = streaming_key(0, 0, 0);
                let plain =
                    TileRenderer::render(&renderer, &key, &cancel_token(), SummaryFidelity::Full)
                        .expect("the plain path renders the tile");
                let (partials, streamed) = stream(&renderer, &key, SummaryFidelity::Full);
                assert_eq!(
                    plain.rgba, streamed.rgba,
                    "world {world:?} hillshade {hillshade}: streamed must equal plain",
                );
                assert_eq!(streamed.rgba.len(), (TILE_SIZE * TILE_SIZE * 4) as usize);
                // Every band publishes a full-size snapshot.
                assert_eq!(partials.len(), 8);
                assert!(
                    partials
                        .iter()
                        .all(|tile| tile.rgba.len() == (TILE_SIZE * TILE_SIZE * 4) as usize),
                );
            }
        }
    }

    /// Partial publications carry the pass's fidelity exactly like a
    /// finished tile, and are marked incomplete so a cancelled stream's
    /// leftover never satisfies an authoritative cache hit.
    #[test]
    fn streaming_partial_tiles_carry_provenance() {
        let renderer = streaming_renderer(None, true);
        let key = streaming_key(0, 0, 0);
        let (partials, final_tile) = stream(&renderer, &key, SummaryFidelity::SurfaceOnly);
        assert!(!partials.is_empty());
        assert!(
            partials
                .iter()
                .all(|tile| tile.fidelity == SummaryFidelity::SurfaceOnly),
            "every band snapshot carries the pass fidelity"
        );
        assert_eq!(final_tile.fidelity, SummaryFidelity::SurfaceOnly);
        assert!(
            partials.iter().all(|tile| !tile.complete),
            "band snapshots are incomplete"
        );
        assert!(final_tile.complete, "the returned tile is complete");

        let cache = TileCache::new(16 * 1024 * 1024);
        cache.insert(Arc::new(partials[0].clone()));
        assert!(
            !cache.contains_serving(&key, SummaryFidelity::SurfaceOnly),
            "an incomplete band never satisfies the scheduler's short-circuit"
        );
        assert!(
            !cache.contains_serving(&key, SummaryFidelity::Full),
            "a partial surface-only tile never serves an authoritative pass"
        );
        // The completed tile, same fidelity, does satisfy the pass.
        cache.insert(Arc::new(final_tile));
        assert!(
            cache.contains_serving(&key, SummaryFidelity::SurfaceOnly),
            "the completed surface-only tile serves a surface-only pass"
        );

        // The first band is the first chunk row only: 32 painted rows.
        assert!(row_is_opaque(&partials[0], 31));
        assert!(row_is_no_data(&partials[0], 32));
    }

    /// A token that fires mid-stream stops the next band: exactly the bands
    /// already published stay published, and the call reports `Cancelled`.
    #[test]
    fn streaming_cancellation_stops_publication() {
        let renderer = streaming_renderer(None, true);
        let key = streaming_key(0, 0, 0);
        let cancel = cancel_token();
        let arm = Arc::clone(&cancel);
        let mut partials = Vec::new();
        let result = TileRenderer::render_streaming(
            &renderer,
            &key,
            &cancel,
            SummaryFidelity::Full,
            &mut |tile| {
                partials.push(tile);
                // The view moves on while the next band is being fetched.
                arm.store(true, Relaxed);
            },
        );
        assert!(
            matches!(result, Err(SourceError::Cancelled)),
            "a cancelled stream returns Cancelled, not a tile"
        );
        assert_eq!(partials.len(), 1, "only the completed band was published");
        assert!(
            !partials[0].complete,
            "the leftover band is marked incomplete"
        );
        assert!(row_is_opaque(&partials[0], 31));
        assert!(row_is_no_data(&partials[0], 32));

        // The published band is valid data: exactly the plain render's rows.
        let plain = TileRenderer::render(&renderer, &key, &cancel_token(), SummaryFidelity::Full)
            .expect("the plain path renders the tile");
        let published = 32 * TILE_SIZE as usize * 4;
        assert_eq!(
            &partials[0].rgba[..published],
            &plain.rgba[..published],
            "published rows are the plain render's rows"
        );
    }

    /// Coarser than one pixel per chunk row: banding would publish the same
    /// pixels repeatedly, so the renderer takes the one-shot path and never
    /// calls `progress`.
    #[test]
    fn streaming_above_chunk_resolution_falls_back_to_one_shot() {
        let renderer = streaming_renderer(None, true);
        let key = streaming_key(6, 0, 0); // step 64 > 32 blocks per chunk
        let mut calls = 0;
        let streamed = TileRenderer::render_streaming(
            &renderer,
            &key,
            &cancel_token(),
            SummaryFidelity::Full,
            &mut |_| calls += 1,
        )
        .expect("the one-shot fallback still renders");
        assert_eq!(calls, 0, "no bands are published above chunk resolution");
        assert!(streamed.complete, "the one-shot tile is complete");
        let plain = TileRenderer::render(&renderer, &key, &cancel_token(), SummaryFidelity::Full)
            .expect("the plain path renders the tile");
        assert_eq!(streamed.rgba, plain.rgba);
    }
}
