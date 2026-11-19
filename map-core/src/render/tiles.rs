//! The per-tile plumbing every layer renderer shares (T16/T17).
//!
//! A tile of any layer is the same dance: check the cancellation token,
//! derive the tile's block rect, plan a source query that is aligned to
//! the tile's sample grid and clipped to the generatable world, fetch
//! the layer's grid, color it, and blit the pixels into a transparent
//! `TILE_SIZE` canvas — the out-of-world margins stay transparent so
//! the app's canvas shows through instead of fabricated data.
//!
//! [`tile_query`] generalizes T16's aligned query with a *border*
//! (relief (T17) needs one extra ring of neighbour samples around the
//! tile; every other layer passes `0`), and [`render_layer_tile`] is
//! the shared skeleton the layer renderers build on — each supplies
//! only its fetch and its colors.
//!
//! [`LayeredTileRenderer`] (also here) is the one renderer the app
//! hands the scheduler: it dispatches on the tile key's layer, so the
//! scheduler stays layer-agnostic and the cache keys stay per-layer.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use crate::coords::{TILE_SIZE, TileCoord, ceil_div, floor_div, tile_rect};
use crate::error::{SourceError, SourceResult};
use crate::render::Layer;
use crate::render::RgbaImage;
use crate::render::biome::BiomeTileRenderer;
use crate::render::fluid::FluidTileRenderer;
use crate::render::height::HeightTileRenderer;
use crate::render::relief::ReliefTileRenderer;
use crate::render::zone::ZoneTileRenderer;
use crate::scheduler::{CancelToken, TileRenderer};
use crate::source::WorldSource;
use crate::tiles::{Tile, TileKey};
use crate::types::Rect;

// ---------------------------------------------------------------------------
// The query plan
// ---------------------------------------------------------------------------

/// One tile's source-query plan: which rect to request, and where the
/// returned grid's `(0,0)` sample sits in tile-canvas samples.
///
/// `origin` is negative when the query starts before the tile (a
/// border ring), zero when it starts at the tile's corner, and positive
/// when the generatable world begins inside the tile (the samples
/// before it are out-of-world margins the layer never paints).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileQuery {
    /// The rect to request from the source: aligned to the tile's
    /// sample grid (`tile.min + k * step`) and fully inside the world.
    pub rect: Rect,
    /// The canvas-sample coordinate of the grid's `(0,0)` sample.
    pub origin: (i32, i32),
}

/// Plans a tile's source query: the tile's rect — expanded by `border`
/// samples on every side — clipped to the generatable world and aligned
/// to the tile's sample grid.
///
/// The alignment is the point: the tile's samples sit at world blocks
/// `tile.min + i * step`, so the query is always a run of whole samples
/// of that grid, never a shifted resample. A border of `0` reproduces
/// T16's in-world query exactly; a border of `1` (relief) adds one ring
/// of neighbours so the shading at one tile's edge matches the
/// neighbour tile's.
///
/// [`None`] when no sample of the (expanded) frame lies inside the
/// world — the tile renders fully transparent — or when the expanded
/// rect leaves the representable block space (absurd coordinates; the
/// caller reports its own out-of-bounds).
#[must_use]
pub fn tile_query(tile: Rect, world: Option<Rect>, step: u32, border: u32) -> Option<TileQuery> {
    // The expanded frame: `border` samples beyond the tile on every
    // side. Checked i64 arithmetic — an i32 tile near the block-space
    // edge with a huge step could overflow, and that is a None, not a
    // panic (unreachable for any view the app schedules).
    let border_i = i64::from(border);
    let step_i = i64::from(step);
    let frame_len = i64::from(TILE_SIZE) + 2 * border_i;
    let e_min_x = i64::from(tile.min_x).checked_sub(border_i * step_i)?;
    let e_min_z = i64::from(tile.min_z).checked_sub(border_i * step_i)?;
    let e_max_x = i64::from(tile.max_x).checked_add(border_i * step_i)?;
    let e_max_z = i64::from(tile.max_z).checked_add(border_i * step_i)?;

    let Some(world) = world else {
        // Unbounded source: the whole frame is one query, offset so the
        // tile's own samples start at `border` inside the grid.
        let rect = Rect::new(
            i32::try_from(e_min_x).ok()?,
            i32::try_from(e_min_z).ok()?,
            i32::try_from(e_max_x).ok()?,
            i32::try_from(e_max_z).ok()?,
        );
        let origin = (-(border as i32), -(border as i32));
        return Some(TileQuery { rect, origin });
    };

    // Sample `i` of the frame (i in -border .. frame_len - border)
    // covers blocks `e_min + i * step` .. `+ step - 1`. The first sample
    // whose blocks reach into the world rounds its distance up; one
    // past the last sample that stays inside rounds the span down. The
    // clamps keep both inside the frame, which also restores T16's
    // `0 ..= TILE_SIZE` bounds at `border == 0`.
    let first_x = ceil_div(i64::from(world.min_x) - e_min_x, step_i).clamp(-border_i, frame_len);
    let first_z = ceil_div(i64::from(world.min_z) - e_min_z, step_i).clamp(-border_i, frame_len);
    let end_x = floor_div(i64::from(world.max_x) - e_min_x + 1, step_i).clamp(-border_i, frame_len);
    let end_z = floor_div(i64::from(world.max_z) - e_min_z + 1, step_i).clamp(-border_i, frame_len);
    let count_x = (end_x - first_x).max(0);
    let count_z = (end_z - first_z).max(0);
    if count_x == 0 || count_z == 0 {
        return None;
    }

    // Clamped frame indices times the step stay well inside i32 (the
    // frame spans at most `TILE_SIZE + 2` samples of at most `2^20`
    // blocks), so the narrowing below cannot wrap.
    let rect = Rect::from_dims(
        i32::try_from(e_min_x + first_x * step_i).ok()?,
        i32::try_from(e_min_z + first_z * step_i).ok()?,
        u32::try_from(count_x).ok()? * step,
        u32::try_from(count_z).ok()? * step,
    );
    // The grid starts at frame index `first`; the canvas counts from the
    // tile's corner, which is `border` samples into the frame — so the
    // grid's (0,0) sits at canvas `(first - border)`: negative when a
    // border ring fits in the world, zero when it does not, positive
    // when the world begins inside the tile.
    let origin = (
        i32::try_from(first_x - border_i).ok()?,
        i32::try_from(first_z - border_i).ok()?,
    );
    Some(TileQuery { rect, origin })
}

/// Blits `patch` into the transparent tile canvas: the patch's `(px,pz)`
/// lands at canvas `(origin + px, origin + pz)`; anything outside the
/// canvas is dropped — a border ring (relief's neighbours, read but
/// never painted) or an out-of-world margin.
pub fn blit_patch(canvas: &mut RgbaImage, patch: &RgbaImage, origin: (i32, i32)) {
    let (width, height) = (canvas.width() as i32, canvas.height() as i32);
    for pz in 0..patch.height() as i32 {
        for px in 0..patch.width() as i32 {
            let (cx, cy) = (origin.0 + px, origin.1 + pz);
            if cx < 0 || cy < 0 || cx >= width || cy >= height {
                continue;
            }
            canvas.put_pixel(cx as u32, cy as u32, *patch.get_pixel(px as u32, pz as u32));
        }
    }
}

// ---------------------------------------------------------------------------
// The shared tile skeleton
// ---------------------------------------------------------------------------

/// The skeleton every layer's `TileRenderer` builds on: cancel check,
/// query planning, the fetch, the coloring, the transparent canvas and
/// the final tile. The layer supplies only its two steps.
///
/// - `fetch` asks the source for the layer's grid over the planned
///   rect (one batched region call — never one query per pixel);
/// - `paint` colors that grid into a patch and says where the patch's
///   `(0,0)` sits in tile-canvas samples (the query's [`TileQuery`]
///   carries the offset; a border-aware layer may instead return a
///   canvas-true image at `(0,0)`).
///
/// The token is checked before the query — a cancelled tile never
/// starts its work unit — and `paint` may check it between row batches
/// (the expensive layers do). A render that completes despite its
/// token is returned normally: the scheduler caches it (the pixels are
/// content-addressed) and reports the cancellation itself.
pub(crate) fn render_layer_tile<G>(
    source: &dyn WorldSource,
    key: &TileKey,
    cancel: &CancelToken,
    world: Option<Rect>,
    border: u32,
    fetch: impl FnOnce(&dyn WorldSource, Rect, u32) -> SourceResult<G>,
    paint: impl FnOnce(&G, &TileQuery, &CancelToken) -> SourceResult<(RgbaImage, (i32, i32))>,
) -> SourceResult<Tile> {
    // A cancelled tile never begins its work unit (the region query);
    // the scheduler is the one that reports the cancellation.
    if cancel.load(Relaxed) {
        return Err(SourceError::Cancelled);
    }
    let step = key.zoom.step();
    let coord = TileCoord::new(i64::from(key.tx), i64::from(key.ty));
    let Some(rect) = tile_rect(coord, step) else {
        // The block bounds left the i32 space. Unreachable for any view
        // the app schedules, but the key cannot name such a tile.
        return Err(SourceError::OutOfBounds {
            x: key.tx,
            z: key.ty,
        });
    };

    // Zeroed means transparent: the out-of-world margins must show the
    // app's canvas rather than fabricated data.
    let mut canvas = RgbaImage::new(TILE_SIZE, TILE_SIZE);
    if let Some(plan) = tile_query(rect, world, step, border) {
        let grid = fetch(source, plan.rect, step)?;
        let (patch, origin) = paint(&grid, &plan, cancel)?;
        blit_patch(&mut canvas, &patch, origin);
    }
    Ok(
        Tile::new(*key, TILE_SIZE, TILE_SIZE, canvas.as_raw().clone())
            .expect("the canvas is TILE_SIZE square with a matching buffer"),
    )
}

// ---------------------------------------------------------------------------
// The layer dispatcher (T17)
// ---------------------------------------------------------------------------

/// One [`TileRenderer`] for every layer the pipeline serves: the app
/// hands this single Arc to the scheduler, and the tile key decides
/// which layer renders — the scheduler and the cache stay layer-agnostic
/// while the keys stay per-layer.
///
/// Layers without a renderer yet (surface blocks, caves) answer with
/// [`SourceError::Unsupported`]; the app never schedules them (its
/// radios map them to "arrives with T20/T28"), so this is the defensive
/// answer, not a flow.
pub struct LayeredTileRenderer {
    biome: BiomeTileRenderer,
    height: HeightTileRenderer,
    relief: ReliefTileRenderer,
    fluid: FluidTileRenderer,
    zone: ZoneTileRenderer,
}

impl LayeredTileRenderer {
    /// Creates the dispatcher; `fluid_opacity` is the fluid layer's
    /// blend setting (T19's UI writes it later).
    #[must_use]
    pub fn new(source: Arc<dyn WorldSource>, world: Option<Rect>, fluid_opacity: f32) -> Self {
        Self {
            biome: BiomeTileRenderer::new(Arc::clone(&source), world),
            height: HeightTileRenderer::new(Arc::clone(&source), world),
            relief: ReliefTileRenderer::new(Arc::clone(&source), world),
            fluid: FluidTileRenderer::new(Arc::clone(&source), world, fluid_opacity),
            zone: ZoneTileRenderer::new(source, world),
        }
    }
}

impl TileRenderer for LayeredTileRenderer {
    fn render(&self, key: &TileKey, cancel: &CancelToken) -> SourceResult<Tile> {
        match key.layer {
            Layer::Biome => TileRenderer::render(&self.biome, key, cancel),
            Layer::Height => TileRenderer::render(&self.height, key, cancel),
            Layer::Relief => TileRenderer::render(&self.relief, key, cancel),
            Layer::Fluid => TileRenderer::render(&self.fluid, key, cancel),
            Layer::Zone => TileRenderer::render(&self.zone, key, cancel),
            Layer::Surface | Layer::CaveSlice | Layer::CaveDensity => {
                Err(SourceError::Unsupported(
                    "this layer has no renderer yet (surface: T20/T21, caves: T28)",
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(min_x: i32, min_z: i32) -> Rect {
        // A step-1 tile: 256 blocks per side.
        Rect::new(min_x, min_z, min_x + 255, min_z + 255)
    }

    #[test]
    fn tile_query_unbounded_is_the_tile_or_the_bordered_frame() {
        // Border 0: the tile itself, origin at its corner.
        let plan = tile_query(tile(0, 0), None, 1, 0).expect("unbounded plans");
        assert_eq!(plan.rect, tile(0, 0));
        assert_eq!(plan.origin, (0, 0));

        // Border 1 at step 2: one neighbour ring, 512 blocks beyond the
        // tile on every side, the grid starting one sample north-west.
        let plan = tile_query(Rect::new(0, 0, 511, 511), None, 2, 1).expect("unbounded plans");
        assert_eq!(plan.rect, Rect::new(-2, -2, 513, 513));
        assert_eq!(plan.origin, (-1, -1));
    }

    #[test]
    fn tile_query_matches_the_t16_alignment_anchors() {
        // The T16 anchors, verbatim: world inside the tile aligns the
        // query to the sample grid, offset by the first in-world sample.
        let world = Rect::from_dims(64, 64, 128, 128);
        let plan = tile_query(tile(0, 0), Some(world), 1, 0).expect("overlap");
        assert_eq!(plan.rect, Rect::new(64, 64, 191, 191));
        assert_eq!(plan.origin, (64, 64));

        // An unaligned world edge at step 2: first sample at 33 (block
        // 66), last in-world sample exclusive end 127 (block 253).
        let world = Rect::new(65, 65, 254, 254);
        let plan = tile_query(tile(0, 0), Some(world), 2, 0).expect("overlap");
        assert_eq!(plan.rect, Rect::new(66, 66, 253, 253));
        assert_eq!(plan.origin, (33, 33));

        // The world covering the whole tile plans the full tile.
        let world = Rect::from_dims(-10_000, -10_000, 20_000, 20_000);
        let plan = tile_query(tile(0, 0), Some(world), 1, 0).expect("overlap");
        assert_eq!(plan.rect, tile(0, 0));
        assert_eq!(plan.origin, (0, 0));
    }

    #[test]
    fn tile_query_outside_the_world_is_none() {
        // No frame sample inside the world: nothing to query.
        let world = Rect::from_dims(10_000, 10_000, 10, 10);
        assert!(tile_query(tile(0, 0), Some(world), 1, 0).is_none());
    }

    #[test]
    fn tile_query_border_clamps_at_the_world_edge() {
        // A world starting inside the tile clips the border ring on the
        // low sides: the grid starts at the tile's own samples there
        // (origin 0, not -1), while the high sides keep their ring —
        // the query reaches one sample beyond the tile's far edge.
        let world = Rect::from_dims(0, 0, 600, 600);
        let tile = Rect::new(0, 0, 255, 255);
        let plan = tile_query(tile, Some(world), 1, 1).expect("overlap");
        // Frame -1 ..= 256; world covers blocks 0 ..= 599, so first = 1
        // (frame 0 is the sample at block -1, outside) and end = 258
        // (the whole frame), i.e. blocks 0 ..= 256.
        assert_eq!(plan.rect, Rect::new(0, 0, 256, 256));
        assert_eq!(plan.origin, (0, 0));
    }

    #[test]
    fn tile_query_refuses_overflowing_frames() {
        // A tile at the block-space edge with a coarse step cannot
        // expand by a border — None, not a panic or a wrap.
        assert!(
            tile_query(
                Rect::new(i32::MAX - 255, 0, i32::MAX, 255),
                None,
                1 << 20,
                1
            )
            .is_none()
        );
    }
}
