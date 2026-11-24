//! The interactive map widget (T16): pan, zoom, tiles, placeholders.
//!
//! The headless half of the task lives elsewhere and is what the tests
//! drive: [`crate::view_state`] owns the pan/zoom math, `map_core::tiles`
//! the placeholder selection, `map_core::scheduler` the background
//! pipeline. This module is the egui skin — input, the per-frame
//! pipeline dance, and painting.
//!
//! # Frame loop
//!
//! 1. compute the frame's request (pure: [`frame_tiles`]) — every
//!    enabled layer's paint subject at the fine step, plus the
//!    scheduler passes (T21's coarse fill and fine refine for the
//!    surface layer when it needs them);
//! 2. when it *changed*, hand it to the scheduler (it queues the
//!    missing tiles nearest each pass's center first) and cancel
//!    everything that is not in the request — tiles that scrolled out
//!    of view *and* tiles of layers the user just disabled (T19);
//! 3. drain scheduler events — `Ready` tiles upload into the GPU bridge
//!    exactly once per key, the tile cache being the state of record;
//! 4. drop GPU textures for CPU-evicted tiles (T14's contract);
//! 5. paint each visible tile layer by layer, bottom → top at the fine
//!    step: every layer may crop its own cached coarser parent as a
//!    placeholder (T21's progressive fill), or — with nothing coarser
//!    cached — pool its cached finer siblings into the missing coarse
//!    tile (zoom-out continuity), while the quiet pending tint stays
//!    exclusive to the base (first) layer. Each texture is
//!    tinted with the layer's opacity (`Color32::from_white_alpha`) —
//!    egui's blending is exactly `map_core::layers::blend_over`'s math,
//!    so the screen and a future export (T32) agree by construction.
//!
//! Pipelines never die on the UI thread: a stale one (its source was
//! invalidated or reloaded) is parked in `retired` and the app drops
//!    it on a background thread — [`TileScheduler`]'s `Drop` joins its
//!    workers, the same reason [`crate::state`]'s sources retire that way.
//!
//! # Orientation
//!
//! Painting consumes the T10 viewport transform as-is: screen Y grows
//! downward, world Z grows south, so north (−Z) sits at the screen top
//! — the same orientation contract the renderers already bake into
//! their pixels (tile row 0 = north). Hover readouts go through the
//! same transform, which is what keeps the coordinates honest.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use eframe::egui;
use egui::{Color32, PointerButton, Response};

use map_core::coords::{TileCoord, TileRect, ZoomLevel, floor_div, tiles_in};
use map_core::features::{ClusterItem, FeatureOverlayState as CoreFeatureOverlayState};
use map_core::render::fluid::DEFAULT_FLUID_OPACITY;
use map_core::render::tiles::LayeredTileRenderer;
use map_core::render::{Layer, SurfaceOptions};
use map_core::scheduler::{TileEvent, TilePass, TileRenderer, TileScheduler, TileSubject};
use map_core::source::{SummaryFidelity, WorldSource};
use map_core::tiles::{
    PlaceholderTile, PooledTile, TileCache, TileKey, covering_placeholder, pooled_placeholder,
};
use map_core::types::Rect;

use crate::labels;
use crate::overlays::features as feature_overlay;
use crate::state::AppState;
use crate::texture::TextureCache;
use crate::view_state::{MapViewState, block_to_screen, expand_to_pixels, tile_screen_rect};

/// Tile-cache byte budget: 256 MiB ≈ a thousand 256×256 tiles. The
/// visible set is a few dozen tiles, so this holds a long pan's worth
/// of LRU-warm tiles; T33 tunes it against measured use.
const TILE_CACHE_BUDGET_BYTES: u64 = 256 * 1024 * 1024;

/// Wheel-to-zoom: egui reports a notched wheel as roughly this many
/// points of `smooth_scroll_delta` per notch, so one notch ≈ one zoom
/// level (the chunkbase feel). Trackpads report small point deltas
/// directly, which gives a fine-grained feel for free.
const SCROLL_POINTS_PER_LEVEL: f32 = 50.0;

/// While tiles are pending, keep frames coming so a finished tile pops
/// in on the very next one instead of waiting for other input.
const TILE_POLL_INTERVAL: Duration = Duration::from_millis(16);

/// The coarse rung of the surface layer's progressive fill (T21): fine
/// detail (1×1/2×2) rides a step-4 fill pass first — the plan's "step 4
/// or 8" rung, chosen over 8 because a 2,048-block placeholder tile
/// scales too coarsely to read as terrain.
const SURFACE_COARSE_STEP: u32 = 4;

/// The map canvas: the letterbox around the world, the color behind
/// transparent tile margins, and the loading blank.
const CANVAS: Color32 = Color32::from_rgb(16, 16, 20);

/// The loading tint for a pending tile with no placeholder. Subtle on
/// purpose: the plan wants the user to *see* refinement, not a
/// flashing checkerboard.
fn pending_tint() -> Color32 {
    Color32::from_rgba_unmultiplied(90, 90, 110, 26)
}

/// The border that goes with the tint.
fn pending_stroke() -> egui::Stroke {
    egui::Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(100, 120, 170, 70))
}

/// The full-texture UV of an exactly-cached tile.
fn full_uv() -> egui::Rect {
    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0))
}

/// An opacity in 0..=1 as the u8 alpha egui tints with (T19). The same
/// scaling `map_core::layers::blend_over` applies to the top layer's
/// alpha, so a slider value means the same thing on screen and in an
/// export.
fn alpha_byte(opacity: f32) -> u8 {
    (opacity.clamp(0.0, 1.0) * 255.0).round() as u8
}

// ---------------------------------------------------------------------------
// The frame request (pure, headless-testable)
// ---------------------------------------------------------------------------

/// One paint step of the frame: the layers sharing one sampling step,
/// with the opacity that never keys a tile (map-app's mirror of
/// `map_core::scheduler::TilePass`).
#[derive(Debug, Clone, PartialEq)]
pub struct FramePass {
    pub layers: Vec<(TileSubject, f32)>,
    pub visible: TileRect,
    pub center: TileCoord,
    pub refine: bool,
    /// The pass's summary fidelity (T29), derived in [`frame_tiles`] from
    /// the pass step relative to the frame's effective step.
    pub fidelity: SummaryFidelity,
}

/// A pass the app wants, before its visible range is computed (the
/// layers share one zoom by construction).
#[derive(Debug, Clone, PartialEq)]
pub struct PassLayers {
    pub layers: Vec<(TileSubject, f32)>,
    pub refine: bool,
}

/// What the canvas draws: every enabled layer at the FINE step,
/// bottom → top.
#[derive(Debug, Clone, PartialEq)]
pub struct PaintFrame {
    pub visible: TileRect,
    pub center: TileCoord,
    pub layers: Vec<(TileSubject, f32)>,
}

/// One frame's tile request: the scheduler passes plus the paint plan.
///
/// Pure on purpose — the same view, paint list, pass list and world
/// always produce the identical request, which is what keeps the
/// pipeline from re-queuing differently-rounded tiles every frame. The
/// whole struct is the dedup identity: a progressive frame's coarse
/// fill pass differs from the next frame's even when the paint plan is
/// unchanged.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameTiles {
    /// The scheduler's passes, fill passes before refinement passes.
    pub passes: Vec<FramePass>,
    /// The canvas's own plan: every enabled layer at the fine step,
    /// bottom → top.
    pub paint: PaintFrame,
}

/// Computes the frame's request from the pure view state: the paint
/// plan's fine range (clamped to the generatable world) plus each
/// pass's own range at its own sampling step, and the pass centers for
/// priorities.
///
/// [`None`] when the paint list is empty, the paint range leaves the
/// representable block space or lies entirely outside the world, or
/// every pass resolved to nothing — the widget paints the canvas and
/// requests nothing rather than a range that cannot exist. (Tiles
/// already queued for a superseded range are left to finish or be
/// cancelled by a later request; the degenerate case is a view flung
/// far off-world, and their pixels stay cached and unused.)
#[must_use]
pub fn frame_tiles(
    view: &MapViewState,
    paint: Vec<(TileSubject, f32)>,
    passes: Vec<PassLayers>,
    world: Option<Rect>,
) -> Option<FrameTiles> {
    // The paint list is the frame's fine step: every enabled layer
    // paints at the same zoom by construction.
    let paint_step = paint.first()?.0.zoom.step();
    let (paint_visible, paint_center) = visible_and_center(view, paint_step, world)?;

    let mut frame_passes = Vec::with_capacity(passes.len());
    for pass in passes {
        // An entry without layers has no step to key; the app never
        // builds one (its fill pass is conditional on being non-empty).
        let Some((first, _)) = pass.layers.first() else {
            continue;
        };
        let Some((visible, center)) = visible_and_center(view, first.zoom.step(), world) else {
            continue;
        };
        // T29's fidelity policy: a pass at the frame's own effective step
        // (`paint_step`, the user's step folded with the zoom) is
        // authoritative and must run the full generator pipeline. Only a
        // fill pass COARSER than it feeds placeholders, so only that one
        // may skip the cave/prefab stages. Step alone must never decide: a
        // user-chosen step-4 view makes step 4 the effective step, so its
        // fill pass stays `Full` even though 4 is the app's coarse rung.
        // Refinement is always authoritative.
        let fidelity = if pass.refine || first.zoom.step() <= paint_step {
            SummaryFidelity::Full
        } else {
            SummaryFidelity::SurfaceOnly
        };
        frame_passes.push(FramePass {
            layers: pass.layers,
            visible,
            center,
            refine: pass.refine,
            fidelity,
        });
    }
    if frame_passes.is_empty() {
        return None;
    }

    Some(FrameTiles {
        passes: frame_passes,
        paint: PaintFrame {
            visible: paint_visible,
            center: paint_center,
            layers: paint,
        },
    })
}

/// One step's visible range and priority center: the view's tiles at
/// `step`, clamped to the generatable world. [`None`] when the view
/// leaves the representable block space or lies entirely outside the
/// world.
fn visible_and_center(
    view: &MapViewState,
    step: u32,
    world: Option<Rect>,
) -> Option<(TileRect, TileCoord)> {
    let mut visible = view.visible_tiles(step)?;
    if let Some(world) = world {
        visible = intersect_tiles(visible, tiles_in(&world, step))?;
    }
    let blocks_per_tile = i64::from(map_core::coords::TILE_SIZE) * i64::from(step);
    let center = TileCoord::new(
        center_axis(
            view.center().x,
            blocks_per_tile,
            visible.min.x,
            visible.max.x,
        ),
        center_axis(
            view.center().z,
            blocks_per_tile,
            visible.min.z,
            visible.max.z,
        ),
    );
    Some((visible, center))
}

/// The intersection of two inclusive tile ranges; [`None`] when empty.
fn intersect_tiles(a: TileRect, b: TileRect) -> Option<TileRect> {
    let min = TileCoord::new(a.min.x.max(b.min.x), a.min.z.max(b.min.z));
    let max = TileCoord::new(a.max.x.min(b.max.x), a.max.z.min(b.max.z));
    (min.x <= max.x && min.z <= max.z).then(|| TileRect::new(min, max))
}

/// The tile of the view center on one axis, clamped into the visible
/// range: priorities radiate from where the user actually looks. A
/// center flung outside the representable space still points at the
/// nearest visible tile.
fn center_axis(center: f64, blocks_per_tile: i64, min: i64, max: i64) -> i64 {
    // f64 → i64 casts saturate (NaN maps to 0), so any center the UI
    // can produce stays inside i64 and the floor division is exact.
    let block = center.floor() as i64;
    floor_div(block, blocks_per_tile).clamp(min, max)
}

/// The generatable world in block rect (inclusive) from the generator's
/// chunk bounds: chunk `c` covers blocks `c * 32 ..= c * 32 + 31`.
///
/// [`None`] when the bounds leave the `i32` block space — an absurd
/// world config; the pipeline then runs unclamped and the generator
/// reports its own out-of-bounds per query instead.
fn world_rect((min_chunk, max_chunk): (i32, i32)) -> Option<Rect> {
    let min = |chunk: i32| i32::try_from(i64::from(chunk) << 5);
    let max = |chunk: i32| i32::try_from((i64::from(chunk) << 5) + 31);
    Some(Rect::new(
        min(min_chunk).ok()?,
        min(min_chunk).ok()?,
        max(max_chunk).ok()?,
        max(max_chunk).ok()?,
    ))
}

// ---------------------------------------------------------------------------
// The pending-tile stand-in (pure decision)
// ---------------------------------------------------------------------------

/// What the canvas paints for a tile whose exact texture is not uploaded:
/// the placeholder choice between the two zoom directions.
#[derive(Debug, Clone)]
pub enum PendingFill {
    /// The nearest cached *coarser* tile of the same subject, cropped to
    /// this tile's world rect ([`covering_placeholder`]) — zoom-in
    /// continuity from real generated pixels.
    Covering(PlaceholderTile),
    /// A coarse tile derived by box-averaging the cached *finer* tiles
    /// that cover it ([`pooled_placeholder`]) — zoom-out continuity.
    /// Alpha-0 pixels are the pool's no-data value and show the canvas.
    Pooled(PooledTile),
    /// Nothing cached covers the tile in either direction: paint the
    /// loading tint and wait.
    Blank,
}

/// Decides the stand-in for a tile that has no uploaded texture this
/// frame.
///
/// Coarser first: a cached parent is a real generated tile (exact pixels
/// at lower detail), while a pooled tile is a derived average whose
/// no-data holes show the canvas — so an existing cover always wins when
/// both directions have candidates, preserving the T21 progressive-fill
/// look. Pooling is the fallback the zoom-out path needs, where nothing
/// coarser was ever generated but the finer detail that *was* on screen a
/// moment ago is still cached.
///
/// Reads go through `TileCache::get`, so both scans refresh LRU recency
/// and count as hits.
#[must_use]
pub fn pending_fill(cache: &TileCache, key: &TileKey) -> PendingFill {
    if let Some(covering) = covering_placeholder(cache, key) {
        return PendingFill::Covering(covering);
    }
    match pooled_placeholder(cache, key) {
        Some(pooled) => PendingFill::Pooled(pooled),
        None => PendingFill::Blank,
    }
}

// ---------------------------------------------------------------------------
// The pipeline
// ---------------------------------------------------------------------------

/// The tile pipeline bound to exactly one loaded source: the scheduler
/// (T15), its shared cache (T14), the biome tile renderer, and the
/// bookkeeping the frame loop needs.
///
/// Public because the app's retirement flow moves whole pipelines
/// (`MapView::take_retired_pipelines`); every field stays private.
///
/// `served` pins the identity + seed the tiles were keyed under; a
/// source that moved underneath (seed edit, reload) means a *new*
/// pipeline, and this one retires.
pub struct TilePipeline {
    scheduler: TileScheduler,
    cache: Arc<TileCache>,
    renderer: Arc<dyn TileRenderer>,
    /// Scheduler events (one clone, made once; clones compete).
    events: Receiver<TileEvent>,
    /// CPU-cache eviction notices for the GPU bridge (one clone).
    evictions: Receiver<TileKey>,
    /// The (identity hash, generator seed) this pipeline serves.
    served: (u64, i32),
    /// The generatable world in blocks; requests clamp to it.
    world: Option<Rect>,
    /// The last request handed to the scheduler: an unchanged frame
    /// re-schedules nothing (the dedup guard against per-frame churn).
    last: Option<FrameTiles>,
}

impl TilePipeline {
    /// Builds the pipeline for `source`: a fresh cache, the layer
    /// dispatcher (one renderer for every layer the radios can select —
    /// the tile key decides), and the default worker pool.
    ///
    /// The identity + seed are pinned *before* the renderer takes the
    /// source — they key every tile this pipeline ever requests, and a
    /// source that moved underneath means a new pipeline.
    fn new(source: Arc<dyn WorldSource>, world: Option<Rect>) -> Self {
        let served = (source.identity().stable_hash(), source.seed().generator());
        let cache = Arc::new(TileCache::new(TILE_CACHE_BUDGET_BYTES));
        let renderer = Arc::new(LayeredTileRenderer::new(
            source,
            world,
            DEFAULT_FLUID_OPACITY,
            SurfaceOptions::default(),
        ));
        let scheduler = TileScheduler::new(TileScheduler::default_workers(), Arc::clone(&cache));
        let events = scheduler.events();
        TilePipeline {
            scheduler,
            events,
            evictions: cache.subscribe_evictions(),
            cache,
            renderer,
            served,
            world,
            last: None,
        }
    }
}

// ---------------------------------------------------------------------------
// The widget
// ---------------------------------------------------------------------------

/// The central map area: input, the tile pipeline, and painting.
pub struct MapView {
    /// The last painted rect in screen points — the pointer → viewport
    /// translation.
    rect: egui::Rect,
    /// The pipeline serving the current source, when there is one.
    pipeline: Option<TilePipeline>,
    /// GPU textures, one per cached tile; the CPU cache is the truth.
    textures: TextureCache,
    /// Stale pipelines parked for an off-thread drop.
    retired: Vec<TilePipeline>,
    /// Tiles queued or rendering in the last frame — the status bar's
    /// "tiles N" (one frame of staleness; the count is advisory).
    pending: u64,
}

impl Default for MapView {
    fn default() -> Self {
        MapView::new()
    }
}

impl MapView {
    /// An empty map: no pipeline, no textures, origin view.
    #[must_use]
    pub fn new() -> Self {
        MapView {
            rect: egui::Rect::NOTHING,
            pipeline: None,
            textures: TextureCache::new(),
            retired: Vec::new(),
            pending: 0,
        }
    }

    /// Stale pipelines, taken for the app's background-thread drop (the
    /// scheduler joins its workers; never on the UI thread).
    pub fn take_retired_pipelines(&mut self) -> Vec<TilePipeline> {
        std::mem::take(&mut self.retired)
    }

    /// Every live and retired pipeline, for the app's exit-time drop.
    pub fn take_all_pipelines(&mut self) -> Vec<TilePipeline> {
        let mut all = self.take_retired_pipelines();
        all.extend(self.pipeline.take());
        all
    }

    /// Tiles queued or rendering in the last frame.
    #[must_use]
    pub fn pending_tiles(&self) -> u64 {
        self.pending
    }

    /// The generatable world rect of the current source (blocks), for
    /// validators that need the generator's bounds (the go-to dialog
    /// rejects coordinates outside it). [`None`] before a source loads
    /// or when the bounds exceed the block space.
    #[must_use]
    pub fn world_bounds(&self) -> Option<Rect> {
        self.pipeline.as_ref().and_then(|pipeline| pipeline.world)
    }

    /// One frame of the map.
    pub fn ui(&mut self, ui: &mut egui::Ui, state: &mut AppState) {
        let rect = ui.max_rect();
        self.rect = rect;
        // The headless state learns the layout first, so its viewport
        // math matches whatever this frame paints.
        state.view_mut().set_size(rect.width(), rect.height());

        // Canvas first: background, world letterbox, loading blank.
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, CANVAS);

        let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());
        self.input(&response, state);

        // Bind the pipeline to exactly the current source before use.
        self.sync_pipeline(state);

        if state.source().is_some() {
            // The pipeline exists (sync_pipeline just built or kept
            // it); take it out so the texture bridge can borrow `self`
            // while it works.
            let Some(mut pipeline) = self.pipeline.take() else {
                unreachable!("sync_pipeline builds a pipeline for a ready source");
            };
            // The frame's paint list (T19): enabled, offered layers in
            // stack order — bottom first — each with its paint opacity,
            // all at the fine step the view resolves.
            let zoom = {
                let view = state.view();
                let step = view.tile_step(state.resolution_step());
                // The effective step is a power of two inside the
                // level range by construction (T10), so this expect is
                // an invariant, not a failure path.
                ZoomLevel::from_step(step).expect("effective_step yields an in-range power of two")
            };
            let paint_layers: Vec<(TileSubject, f32)> = state
                .schedulable_layers()
                .into_iter()
                .map(|(layer, opacity)| {
                    // The subject's layer is the stack's: enabling a
                    // layer adds its keys, disabling removes them from
                    // the request — a disabled layer's tiles can never
                    // be served (or painted).
                    (
                        TileSubject::new(pipeline.served.0, pipeline.served.1, layer, 0, zoom),
                        opacity,
                    )
                })
                .collect();
            // T21: at a fine effective step the full-block surface is
            // requested twice — a coarse fill that covers the viewport
            // quickly and a fine refine behind it. The coarse tiles are
            // never painted directly; they exist so the fine tiles'
            // placeholder crops find a cached parent (T16's level scan
            // crosses zooms). Every other layer keeps one fine pass.
            // T29: `frame_tiles` marks exactly the coarser-than-effective
            // fill pass SurfaceOnly (speed) and everything at the
            // effective step Full (authoritative).
            let surface = paint_layers
                .iter()
                .find(|(subject, _)| subject.layer == Layer::Surface)
                .copied();
            let progressive = surface.is_some() && zoom.step() < SURFACE_COARSE_STEP;
            let mut passes = Vec::new();
            let fill_layers: Vec<(TileSubject, f32)> = if progressive {
                paint_layers
                    .iter()
                    .filter(|(subject, _)| subject.layer != Layer::Surface)
                    .copied()
                    .collect()
            } else {
                paint_layers.clone()
            };
            if !fill_layers.is_empty() {
                passes.push(PassLayers {
                    layers: fill_layers,
                    refine: false,
                });
            }
            if let Some((surface_subject, surface_opacity)) = surface
                && progressive
            {
                let coarse_zoom = ZoomLevel::from_step(SURFACE_COARSE_STEP)
                    .expect("SURFACE_COARSE_STEP is an in-range power of two");
                passes.push(PassLayers {
                    layers: vec![(
                        TileSubject::new(
                            surface_subject.identity_hash,
                            surface_subject.generator_seed,
                            Layer::Surface,
                            0,
                            coarse_zoom,
                        ),
                        surface_opacity,
                    )],
                    refine: false,
                });
                passes.push(PassLayers {
                    layers: vec![(surface_subject, surface_opacity)],
                    refine: true,
                });
            }
            let request = (!paint_layers.is_empty())
                .then(|| frame_tiles(state.view(), paint_layers.clone(), passes, pipeline.world))
                .flatten();
            self.serve(ui.ctx(), &mut pipeline, request.as_ref());
            if let Some(request) = &request {
                self.paint(ui.ctx(), &painter, request, &pipeline, state.view());
                if self.pending > 0 {
                    // The "generating N tiles" cue: a spinner near the
                    // top; the count itself lives in the status bar.
                    let spinner = egui::Rect::from_center_size(
                        rect.center_top() + egui::Vec2::new(0.0, 40.0),
                        egui::Vec2::splat(28.0),
                    );
                    ui.put(spinner, egui::Spinner::new());
                }
            } else {
                // Every layer unticked (a source-less view takes the
                // same path below): canvas plus a message.
                self.placeholder(ui, &painter, state);
            }
            // Derived pooled placeholders are per-frame conveniences, not
            // cache entries: whatever this frame's paint did not touch (the
            // real tile landed, the viewport moved) frees its GPU memory
            // now.
            self.textures.retire_unused_pooled();
            self.pipeline = Some(pipeline);
        } else {
            // No source (yet): canvas plus a message, no tile traffic.
            self.placeholder(ui, &painter, state);
        }

        // T23's vector layer: after the tiles, before anything else reads
        // the frame (the cursor readout is the status bar's next frame).
        // With no catalog this is a cheap no-op.
        self.paint_feature_overlay(ui, &painter, &response, state);
        // T26's search selection ring: a result row's click centers the map
        // and flashes this ring for one second.
        self.paint_search_flash(&painter, state);
    }

    /// Draws the flashing ring around the selected search result, when its
    /// one-second window is open.
    fn paint_search_flash(&self, painter: &egui::Painter, state: &AppState) {
        let Some((x, z)) = state.search().flash_position(Instant::now()) else {
            return;
        };
        let Some((screen_x, screen_y)) = block_to_screen(state.view(), x, z) else {
            return;
        };
        let center = self.rect.min + egui::vec2(screen_x, screen_y);
        painter.circle_stroke(
            center,
            12.0,
            egui::Stroke::new(2.0_f32, Color32::from_rgb(120, 200, 255)),
        );
        painter.circle_stroke(
            center,
            18.0,
            egui::Stroke::new(1.0_f32, Color32::from_rgb(60, 140, 220)),
        );
        painter.circle_filled(center, 2.5, Color32::from_rgb(220, 240, 255));
    }

    // -- Input --------------------------------------------------------------

    /// Pointer input into the headless view: hover readout, drag pan and
    /// wheel zoom. The click-to-copy decision runs after painting, where the
    /// feature overlay's markers are known (T23's click-to-select must win
    /// over the plain coordinate copy).
    fn input(&mut self, response: &Response, state: &mut AppState) {
        let rect = self.rect;

        // The block under the pointer feeds the status bar; T18 adds
        // the biome/height text on top of it.
        let hover = response
            .hover_pos()
            .map(|pos| (pos.x - rect.min.x, pos.y - rect.min.y));
        let block = hover.and_then(|local| state.view().screen_to_block(local));
        state.set_cursor(block.map(|block| (block.x, block.z)));

        // Drag pans with the pointer (left or middle button): the
        // world follows the hand, so the center moves opposite.
        if response.dragged_by(PointerButton::Primary) || response.dragged_by(PointerButton::Middle)
        {
            let delta = response.drag_delta();
            state.view_mut().pan_by_screen((delta.x, delta.y));
        }

        // Wheel zooms at the cursor: plain wheel by smoothed points,
        // pinch / ctrl-wheel through egui's zoom delta (the two input
        // streams are disjoint — ctrl-routed scroll never reaches
        // smooth_scroll_delta).
        if response.hovered() {
            let scroll = response.ctx.input(|input| input.smooth_scroll_delta.y);
            let pinch = response.ctx.input(|input| input.zoom_delta());
            if scroll != 0.0 || pinch != 1.0 {
                let cursor = hover.unwrap_or_else(|| (rect.width() / 2.0, rect.height() / 2.0));
                let pinch_levels = if pinch > 0.0 && pinch.is_finite() {
                    -pinch.log2()
                } else {
                    0.0
                };
                state
                    .view_mut()
                    .zoom_at_screen(cursor, -scroll / SCROLL_POINTS_PER_LEVEL + pinch_levels);
            }
        }
    }

    // -- Pipeline lifecycle ---------------------------------------------------

    /// Keeps the pipeline bound to exactly the current source: builds
    /// one when a source is ready, retires the previous when the
    /// identity or seed moved (a re-Go, a seed edit followed by a load).
    ///
    /// Retiring parks the old pipeline for the app's off-thread drop —
    /// never a drop here, where the scheduler would join its workers
    /// mid-frame.
    fn sync_pipeline(&mut self, state: &AppState) {
        let Some(source) = state.source() else {
            self.retire_pipeline();
            return;
        };
        let served = (source.identity().stable_hash(), source.seed().generator());
        if self
            .pipeline
            .as_ref()
            .is_some_and(|pipeline| pipeline.served == served)
        {
            return;
        }
        self.retire_pipeline();
        // Same `Ready` source as the borrow above — the pair cannot
        // change between the two calls.
        let Some(source) = state.source_arc() else {
            return;
        };
        let world = world_rect(source.chunk_bounds());
        self.pipeline = Some(TilePipeline::new(source, world));
    }

    /// Parks the current pipeline (if any) for retirement and clears
    /// the GPU bridge: a dead seed's tiles must not stay on screen.
    fn retire_pipeline(&mut self) {
        if let Some(pipeline) = self.pipeline.take() {
            self.retired.push(pipeline);
        }
        self.textures.clear();
    }

    // -- The frame dance -------------------------------------------------------

    /// One frame of pipeline bookkeeping: hand a *changed* request to
    /// the scheduler, cancel what the frame no longer serves — tiles
    /// that scrolled away *and* tiles of layers the stack just
    /// disabled — upload finished tiles once per key, drop evicted
    /// textures, and note the pending count.
    fn serve(
        &mut self,
        ctx: &egui::Context,
        pipeline: &mut TilePipeline,
        request: Option<&FrameTiles>,
    ) {
        if let Some(request) = request
            && pipeline.last.as_ref() != Some(request)
        {
            // New view, zoom or layer set: queue what is missing
            // nearest each pass's center first, cancel what stopped
            // being served, and remember the request so an unchanged
            // frame is a no-op. Both APIs take the same pass list, so
            // the scheduler's notion of "serving" matches exactly what
            // it was asked for (T21's refine pass included).
            let tile_passes: Vec<TilePass> = request
                .passes
                .iter()
                .map(|pass| TilePass {
                    subjects: pass.layers.iter().map(|(subject, _)| *subject).collect(),
                    visible: pass.visible,
                    center: pass.center,
                    refine: pass.refine,
                    fidelity: pass.fidelity,
                })
                .collect();
            pipeline
                .scheduler
                .schedule_passes(&tile_passes, Arc::clone(&pipeline.renderer));
            pipeline.scheduler.cancel_not_serving(&tile_passes);
            pipeline.last = Some(request.clone());
        }

        // The GPU mirror follows every CPU removal (T14's contract). Do
        // this BEFORE the event drain: an insert that replaces a key (T29's
        // weaker→stronger provenance upgrade) notifies its eviction first,
        // so the old handle must go before the Ready upload installs the
        // new pixels for that same key.
        self.textures.drop_evicted(pipeline.evictions.try_iter());

        // Events are hints (the cache is the state of record): each
        // key uploads exactly once; failures are logged, not fatal —
        // the tile paints as a placeholder and the next view change
        // re-queues it.
        for event in pipeline.events.try_iter() {
            match event {
                TileEvent::Ready { tile, .. } => {
                    self.textures.upload(ctx, tile.key, &tile);
                }
                TileEvent::Failed(key, error) => {
                    log::warn!("tile {key:?} failed to render: {error}");
                }
                TileEvent::Started(_) | TileEvent::Cancelled(_) => {}
            }
        }

        let stats = pipeline.scheduler.stats();
        self.pending = stats.queued + stats.in_flight;
        if self.pending > 0 {
            // Results arrive on worker threads; keep frames coming so
            // a finished tile appears on the very next one.
            ctx.request_repaint_after(TILE_POLL_INTERVAL);
        }
    }

    // -- Painting ---------------------------------------------------------------

    /// Paints each visible tile layer by layer, bottom → top (T19), all
    /// at the paint plan's fine step.
    ///
    /// Every layer may draw its exact texture, else the nearest cached
    /// coarser tile of *its own* layer cropped to this tile's world rect
    /// (T21: the progressive coarse pass exists precisely so the fine
    /// surface tile's `covering_placeholder` finds a parent). With no
    /// coarser cover, a pending coarse tile pools its cached finer
    /// siblings ([`pending_fill`]): zooming out shows the detail that was
    /// just on screen instead of a blank. Only the bottom-most layer
    /// (index 0) gets the quiet pending tint, and only over a pooled or
    /// blank stand-in: a tint under an overlay would double the base's
    /// placeholder rather than tell the user anything new, and a covering
    /// crop already reads as real terrain. Each texture is tinted with
    /// its layer's opacity (`from_white_alpha`): egui's vertex blending is
    /// straight-alpha-over, the same math as
    /// `map_core::layers::blend_over`, so the screen and a future export
    /// (T32) agree by construction.
    ///
    /// Adjacent tiles agree on their shared edge because floor/ceil to
    /// physical pixels is a pure function of the shared coordinate — no
    /// seams, no double-paint.
    fn paint(
        &mut self,
        ctx: &egui::Context,
        painter: &egui::Painter,
        frame: &FrameTiles,
        pipeline: &TilePipeline,
        view: &MapViewState,
    ) {
        let step = frame
            .paint
            .layers
            .first()
            .expect("the caller only paints with a non-empty paint list")
            .0
            .zoom
            .step();
        let ppp = ctx.pixels_per_point();
        for coord in frame.paint.visible.iter() {
            // Visible ranges are i64 tile space; keys are i32 — the
            // reachable world sits far inside (T10's bounds analysis).
            let (Ok(tx), Ok(ty)) = (i32::try_from(coord.x), i32::try_from(coord.z)) else {
                continue;
            };
            let Some((x0, y0, x1, y1)) = tile_screen_rect(view, coord, step) else {
                continue;
            };
            // Viewport-local → screen points, then out to whole pixels.
            let (min, max) = expand_to_pixels(
                (x0 + self.rect.min.x, y0 + self.rect.min.y),
                (x1 + self.rect.min.x, y1 + self.rect.min.y),
                ppp,
            );
            let screen =
                egui::Rect::from_min_max(egui::pos2(min.0, min.1), egui::pos2(max.0, max.1));

            for (index, (subject, opacity)) in frame.paint.layers.iter().enumerate() {
                let key = subject.key_at(tx, ty);
                // The layer's paint-time blend: white keeps the tile's
                // own colors, the alpha scales them by the slider.
                let tint = Color32::from_white_alpha(alpha_byte(*opacity));
                if let Some(id) = self.textures.texture_id(&key) {
                    painter.image(id, screen, full_uv(), tint);
                    continue;
                }
                // A pooled placeholder uploaded on an earlier frame paints
                // again without re-deriving: the derivation is keyed by
                // (tile key, source level), so the frame pays a lookup, not
                // a box filter. A derived texture is cheap to drop, so the
                // bridge sweeps whatever the viewport stops touching.
                //
                // `cue` tracks whether the base layer's pending tint belongs
                // on top: pooled and blank stand-ins still need it, a
                // covering crop and the exact texture do not.
                let mut cue = false;
                if let Some(id) = self.textures.pooled_texture_id(&key) {
                    painter.image(id, screen, full_uv(), tint);
                    cue = true;
                } else {
                    match pending_fill(&pipeline.cache, &key) {
                        PendingFill::Covering(placeholder) => {
                            // Instant low detail while the real tile
                            // renders. The parent may be cached but not yet
                            // uploaded — upload it on demand; the exact
                            // tile's later upload replaces it. Every layer
                            // crops its own parent: the surface's coarse
                            // pass (T21) is what makes a pending fine
                            // surface tile read as terrain instead of a
                            // blank.
                            let id = self
                                .textures
                                .texture_id(&placeholder.tile.key)
                                .unwrap_or_else(|| {
                                    self.textures.upload(
                                        ctx,
                                        placeholder.tile.key,
                                        &placeholder.tile,
                                    )
                                });
                            let uv = egui::Rect::from_min_max(
                                egui::pos2(placeholder.uv[0], placeholder.uv[1]),
                                egui::pos2(placeholder.uv[2], placeholder.uv[3]),
                            );
                            painter.image(id, screen, uv, tint);
                        }
                        PendingFill::Pooled(pooled) => {
                            // Zoom-out stand-in: the coarse tile's pixels are
                            // box averages of the finer tiles that were on
                            // screen a moment ago. Full UV — the pool covers
                            // exactly this tile's rect — and alpha 0 (the
                            // no-data value) lets the canvas show through
                            // while the coarse tile generates.
                            let id = self.textures.upload_pooled(ctx, &pooled);
                            painter.image(id, screen, full_uv(), tint);
                            cue = true;
                        }
                        PendingFill::Blank => cue = true,
                    }
                }
                // The base layer's in-progress cue stays *on top* of a
                // pooled/blank stand-in: a pooled tile has no-data holes and
                // fills in as its finer siblings land, so it must still read
                // as generating. A covering crop is a real coarser tile and
                // keeps its bare look, as T21 shipped it.
                if index == 0 && cue && pipeline.scheduler.is_pending(&key) {
                    painter.rect_filled(screen, 0.0, pending_tint());
                    painter.rect_stroke(screen, 0.0, pending_stroke(), egui::StrokeKind::Middle);
                }
            }
        }
    }

    // -- The feature overlay (T23) --------------------------------------------

    /// Draws the two catalogs' markers over the tiles, then applies the
    /// pointer's hover tooltip and click action.
    ///
    /// The draw list ([`feature_overlay::marker_list`]) is computed once per
    /// frame and reused for hit-testing, so what the user clicks is exactly
    /// what was drawn. A single marker's click copies its coordinates and
    /// selects it; a cluster's click zooms one level centred on the cluster
    /// (T23's "clicking a cluster zooms in or splits it"); anything else
    /// falls through to the plain block-coordinate copy.
    fn paint_feature_overlay(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        response: &Response,
        state: &mut AppState,
    ) {
        let view = *state.view();
        let mask = feature_overlay::toggle_mask(state.overlays());
        // Both tiers, deduped and filtered to the enabled groups: the
        // quick catalog keeps its cheap markers on screen until a covering
        // full catalog replaces it (the T22 subset property makes that
        // lossless), and the memoized union costs one build per change.
        let Some(catalog) = state.features_mut().draw_catalog(mask) else {
            return;
        };
        let viewport = (self.rect.width(), self.rect.height());
        let items = feature_overlay::marker_list(
            &catalog,
            mask,
            &view,
            viewport,
            CoreFeatureOverlayState::DEFAULT_CLUSTER_CELL_PX,
        );
        let now = Instant::now();
        let selected = state.features().selected_key().cloned();
        let flashing = state.features().is_flashing(now);
        feature_overlay::paint_items(
            painter,
            &items,
            &catalog,
            self.rect.min,
            selected.as_ref(),
            flashing,
        );

        let pointer = response
            .hover_pos()
            .map(|pos| (pos.x - self.rect.min.x, pos.y - self.rect.min.y));
        if let Some(hit) = pointer.and_then(|pointer| feature_overlay::hit_test(&items, pointer)) {
            let tooltip = match hit {
                feature_overlay::FeatureHit::Single { feature, .. } => catalog
                    .features()
                    .get(feature)
                    .map(|feature| feature_overlay::tooltip_text(feature, &view)),
                feature_overlay::FeatureHit::Cluster { item } => match items.get(item) {
                    Some(ClusterItem::Cluster { count, .. }) => {
                        Some(format!("{count} {}", labels::FEATURE_CLUSTER))
                    }
                    _ => None,
                },
            };
            if let Some(tooltip) = tooltip {
                response.clone().on_hover_ui_at_pointer(|ui| {
                    ui.label(tooltip);
                });
            }
        }

        if response.clicked_by(PointerButton::Primary) {
            if let Some(hit) =
                pointer.and_then(|pointer| feature_overlay::hit_test(&items, pointer))
            {
                match hit {
                    feature_overlay::FeatureHit::Single { feature, .. } => {
                        let Some(feature) = catalog.features().get(feature) else {
                            return;
                        };
                        let (x, _, z) = feature.position;
                        // Exactly the go-to dialog's copy string (T31
                        // formalizes sharing it).
                        ui.ctx().copy_text(format!("X:{x} Z:{z}"));
                        state.set_status(format!(
                            "{} {} X:{x} Z:{z}",
                            labels::STATUS_COPIED,
                            feature.name,
                        ));
                        state.features_mut().select(feature, now);
                    }
                    feature_overlay::FeatureHit::Cluster { item } => {
                        if let Some(ClusterItem::Cluster { center, .. }) = items.get(item)
                            && let Some(block) = view.screen_to_block(*center)
                        {
                            state
                                .view_mut()
                                .set_center(f64::from(block.x), f64::from(block.z));
                            state.view_mut().zoom_at_center(-1.0);
                        }
                    }
                }
                return;
            }
            // Not a feature: the map's plain click-copy (T16's working
            // placeholder, unchanged by T23).
            if let Some((x, z)) = state.cursor() {
                response.ctx.copy_text(format!("{x} {z}"));
                state.set_status(format!("{} X:{x} Z:{z}", labels::STATUS_COPIED));
            }
        }
    }

    // -- The no-tile states -----------------------------------------------------

    /// The canvas-only states: no source, no enabled layer, a failed
    /// load, or a view outside the generatable world.
    fn placeholder(&mut self, ui: &mut egui::Ui, painter: &egui::Painter, state: &AppState) {
        // A faint block grid, 64 points per cell, so the area reads as
        // a map canvas and not a label.
        let cell = 64.0;
        let stroke = egui::Stroke::new(1.0_f32, Color32::from_rgb(36, 36, 44));
        let first_vertical = (self.rect.left() / cell).ceil() * cell;
        let mut x = first_vertical;
        while x < self.rect.right() {
            painter.vline(x, self.rect.y_range(), stroke);
            x += cell;
        }
        let first_horizontal = (self.rect.top() / cell).ceil() * cell;
        let mut y = first_horizontal;
        while y < self.rect.bottom() {
            painter.hline(self.rect.x_range(), y, stroke);
            y += cell;
        }

        let message = match state.source() {
            None => match state.source_error() {
                Some(error) => {
                    format!("{} {error}", labels::MAP_PLACEHOLDER_ERROR_PREFIX)
                }
                None if state.is_loading() => labels::MAP_PLACEHOLDER_LOADING.to_string(),
                None => labels::MAP_PLACEHOLDER_NO_SOURCE.to_string(),
            },
            // A loaded world: either no renderable layer is ticked (the
            // stack allows a blank map), or the view simply left the
            // generatable world.
            Some(_) => {
                if state.schedulable_layers().is_empty() {
                    labels::MAP_PLACEHOLDER_NO_LAYERS.to_string()
                } else {
                    labels::MAP_PLACEHOLDER_OFF_WORLD.to_string()
                }
            }
        };
        let message_color = if state.source_error().is_some() {
            Color32::from_rgb(230, 120, 120)
        } else {
            Color32::from_rgb(200, 200, 210)
        };
        painter.text(
            self.rect.center(),
            egui::Align2::CENTER_CENTER,
            message,
            egui::TextStyle::Body.resolve(ui.style()),
            message_color,
        );

        // A spinner while the world itself is loading.
        if state.is_loading() {
            let spinner = egui::Rect::from_center_size(
                self.rect.center_top() + egui::Vec2::new(0.0, 40.0),
                egui::Vec2::splat(28.0),
            );
            ui.put(spinner, egui::Spinner::new());
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use map_core::coords::{TILE_SIZE, TileCoord, ZoomLevel};
    use map_core::render::Layer;
    use map_core::render::UNKNOWN_BIOME_COLOR;
    use map_core::source::SummaryFidelity;
    use map_core::source::worldgen::{WorldgenSource, WorldgenSourceConfig};
    use map_core::test_support::FakeWorldSource;
    use map_core::tiles::{Tile, TileCache, TileKey, covering_placeholder};
    use map_core::types::Rect;

    use super::{
        FrameTiles, MapView, PassLayers, PendingFill, TilePipeline, TileSubject, frame_tiles,
        pending_fill, world_rect,
    };
    use crate::view_state::MapViewState;

    /// A view of `width x height` points at the given continuous zoom,
    /// centered in the world.
    fn view(smooth_zoom: f32, width: f32, height: f32) -> MapViewState {
        let mut view = MapViewState::new();
        view.set_size(width, height);
        if smooth_zoom != 0.0 {
            // A center cursor keeps the center fixed while zooming.
            view.zoom_at_screen((width / 2.0, height / 2.0), smooth_zoom);
        }
        view
    }

    /// The frame request for `pipeline`'s own subject at `zoom` for one
    /// `layer` — the exact subject the widget builds for that checkbox.
    fn request_for(
        pipeline: &TilePipeline,
        view: &MapViewState,
        zoom: ZoomLevel,
        layer: Layer,
    ) -> FrameTiles {
        request_for_layers(pipeline, view, zoom, &[layer])
    }

    /// The frame request for a whole layer set (T19's stack order):
    /// the exact paint/subject list the widget builds for those
    /// checkboxes, served by one ordinary fill pass.
    fn request_for_layers(
        pipeline: &TilePipeline,
        view: &MapViewState,
        zoom: ZoomLevel,
        layers: &[Layer],
    ) -> FrameTiles {
        let paint: Vec<(TileSubject, f32)> = layers
            .iter()
            .map(|layer| {
                (
                    TileSubject::new(pipeline.served.0, pipeline.served.1, *layer, 0, zoom),
                    1.0,
                )
            })
            .collect();
        let passes = if paint.is_empty() {
            Vec::new()
        } else {
            vec![PassLayers {
                layers: paint.clone(),
                refine: false,
            }]
        };
        frame_tiles(view, paint, passes, pipeline.world)
            .expect("the test views sit inside the representable space")
    }

    /// A fake-source pipeline over an unbounded world.
    fn fake_pipeline() -> TilePipeline {
        TilePipeline::new(Arc::new(FakeWorldSource::new("7")), None)
    }

    /// Drives `serve` frames until no tile is pending or the deadline
    /// passes. The fake renders instantly, so this is one or two polls.
    fn settle(
        map: &mut MapView,
        ctx: &egui::Context,
        pipeline: &mut TilePipeline,
        request: &FrameTiles,
    ) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            map.serve(ctx, pipeline, Some(request));
            if map.pending_tiles() == 0 || Instant::now() > deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    // -- The frame request (pure) ---------------------------------------------

    /// One ordinary fill pass over `paint` — the shape every layer's
    /// checkbox builds when no progressive split is in play.
    fn fill_pass(paint: &[(TileSubject, f32)]) -> Vec<PassLayers> {
        vec![PassLayers {
            layers: paint.to_vec(),
            refine: false,
        }]
    }

    #[test]
    fn frame_tiles_matches_the_t10_table() {
        // T10's documented view end to end: center (-1203, 455),
        // scale 0.5 (smooth zoom 1), 1024x768, step 2 -> tiles
        // (-6, -2)..(0, 3) after the one-tile expansion, priorities
        // radiating from tile (-3, 0).
        let mut view = view(1.0, 1024.0, 768.0);
        // Pan to the center: 601.5 points at scale 0.5 = 1203 blocks.
        view.pan_by_screen((601.5, -227.5));
        let subject = TileSubject::new(1, 7, Layer::Biome, 0, ZoomLevel::new(1).expect("in range"));
        let paint = vec![(subject, 1.0)];
        let request =
            frame_tiles(&view, paint.clone(), fill_pass(&paint), None).expect("the view resolves");
        assert_eq!(request.paint.visible.min, TileCoord::new(-6, -2));
        assert_eq!(request.paint.visible.max, TileCoord::new(0, 3));
        assert_eq!(request.paint.center, TileCoord::new(-3, 0));
    }

    #[test]
    fn frame_tiles_clamps_the_range_to_the_generatable_world() {
        // A 200x200-block world around the origin: at step 1 exactly
        // tiles (-1, -1)..(0, 0), no matter how wide the view is.
        let view = view(0.0, 1024.0, 768.0);
        let world = Rect::new(-100, -100, 99, 99);
        let subject = TileSubject::new(1, 7, Layer::Biome, 0, ZoomLevel::MIN);
        let paint = vec![(subject, 1.0)];
        let request = frame_tiles(&view, paint.clone(), fill_pass(&paint), Some(world))
            .expect("the view overlaps the world");
        assert_eq!(request.paint.visible.min, TileCoord::new(-1, -1));
        assert_eq!(request.paint.visible.max, TileCoord::new(0, 0));
        assert!(
            request.paint.visible.contains(request.paint.center),
            "the priority center clamps into the tiny range"
        );
        assert!(
            request
                .passes
                .iter()
                .all(|pass| pass.visible.contains(pass.center)),
            "each pass's center clamps into its own range"
        );

        // A world entirely outside the view serves nothing.
        let far = Rect::new(100_000, 100_000, 100_200, 100_200);
        assert!(frame_tiles(&view, paint.clone(), fill_pass(&paint), Some(far)).is_none());
    }

    #[test]
    fn frame_tiles_is_identical_across_unchanged_frames() {
        // The dedup guard: the same inputs produce the identical
        // request, so an unchanged frame re-queues nothing (the
        // generation witness lives in the pipeline test below).
        let view = view(0.0, 1024.0, 768.0);
        let subject = TileSubject::new(1, 7, Layer::Biome, 0, ZoomLevel::MIN);
        let paint = vec![(subject, 1.0)];
        assert_eq!(
            frame_tiles(&view, paint.clone(), fill_pass(&paint), None),
            frame_tiles(&view, paint.clone(), fill_pass(&paint), None)
        );
    }

    #[test]
    fn surface_progressive_frame_splits_coarse_fill_from_fine_refine() {
        // T21's request shape, pure and headless: at a fine effective
        // step the surface layer rides two extra passes — a coarse
        // fill that lands first and a fine refine behind it — while
        // every other layer keeps one fine fill pass. Paint is still
        // every enabled layer at the fine step.
        let view = view(0.0, 600.0, 400.0);
        let fine = ZoomLevel::MIN;
        let coarse = ZoomLevel::from_step(4).expect("4 is an in-range step");
        let build = || {
            let biome = TileSubject::new(1, 7, Layer::Biome, 0, fine);
            let surface = TileSubject::new(1, 7, Layer::Surface, 0, fine);
            let coarse_surface = TileSubject::new(1, 7, Layer::Surface, 0, coarse);
            let paint = vec![(biome, 1.0), (surface, 1.0)];
            let passes = vec![
                PassLayers {
                    layers: vec![(biome, 1.0)],
                    refine: false,
                },
                PassLayers {
                    layers: vec![(coarse_surface, 1.0)],
                    refine: false,
                },
                PassLayers {
                    layers: vec![(surface, 1.0)],
                    refine: true,
                },
            ];
            frame_tiles(&view, paint, passes, None).expect("the view resolves")
        };

        let frame = build();
        assert_eq!(frame.passes.len(), 3, "fill, coarse fill, refine");
        assert_eq!(
            frame.paint.layers,
            vec![
                (TileSubject::new(1, 7, Layer::Biome, 0, fine), 1.0),
                (TileSubject::new(1, 7, Layer::Surface, 0, fine), 1.0),
            ],
            "the canvas still draws every enabled layer at the fine step"
        );

        let fill = &frame.passes[0];
        assert!(!fill.refine, "the first pass is the ordinary fill");
        assert_eq!(fill.layers.len(), 1, "the surface left the fine fill");
        assert_eq!(fill.layers[0].0.layer, Layer::Biome);

        let coarse_pass = &frame.passes[1];
        assert!(!coarse_pass.refine, "the coarse coverage is a fill pass");
        assert_eq!(coarse_pass.layers[0].0.layer, Layer::Surface);
        assert_eq!(
            coarse_pass.layers[0].0.zoom, coarse,
            "the coarse pass samples at the step-4 rung"
        );

        let refine = &frame.passes[2];
        assert!(refine.refine, "the detail rides the refine pass");
        assert_eq!(refine.layers[0].0.layer, Layer::Surface);
        assert_eq!(
            refine.layers[0].0.zoom, fine,
            "the refine pass carries the fine surface tile"
        );

        // The whole frame is the dedup identity: identical inputs never
        // change the scheduler's request.
        assert_eq!(frame, build());
    }

    /// T29's app policy: a fill pass below the frame's effective step is a
    /// placeholder source and may skip caves/prefabs; the pass at the
    /// user's own effective step — and every refine pass — is authoritative
    /// and keeps the full pipeline. The step number alone never decides:
    /// the same step-4 pass is SurfaceOnly at a finer view and Full when
    /// step 4 is what the user chose.
    #[test]
    fn fidelity_fill_pass_is_surface_only_only_below_the_user_step() {
        let view = view(0.0, 600.0, 400.0);
        let fine = ZoomLevel::MIN;
        let step4 = ZoomLevel::from_step(4).expect("4 is an in-range step");
        let biome = TileSubject::new(1, 7, Layer::Biome, 0, fine);
        let surface = TileSubject::new(1, 7, Layer::Surface, 0, fine);
        let coarse_surface = TileSubject::new(1, 7, Layer::Surface, 0, step4);

        // A fine effective step (1): the coarse coverage is a placeholder,
        // the fine fill and refine are authoritative.
        let paint = vec![(biome, 1.0), (surface, 1.0)];
        let frame = frame_tiles(
            &view,
            paint,
            vec![
                PassLayers {
                    layers: vec![(biome, 1.0)],
                    refine: false,
                },
                PassLayers {
                    layers: vec![(coarse_surface, 1.0)],
                    refine: false,
                },
                PassLayers {
                    layers: vec![(surface, 1.0)],
                    refine: true,
                },
            ],
            None,
        )
        .expect("the view resolves");
        assert_eq!(
            frame.passes[0].fidelity,
            SummaryFidelity::Full,
            "the fill at the effective step is authoritative"
        );
        assert_eq!(
            frame.passes[1].fidelity,
            SummaryFidelity::SurfaceOnly,
            "the coarser fill only feeds placeholders"
        );
        assert_eq!(
            frame.passes[2].fidelity,
            SummaryFidelity::Full,
            "refinement is always authoritative"
        );

        // The user's own effective step is 4: the same step-4 pass must
        // now run the full pipeline, because it is what the canvas paints.
        let step4_biome = TileSubject::new(1, 7, Layer::Biome, 0, step4);
        let step4_surface = TileSubject::new(1, 7, Layer::Surface, 0, step4);
        let paint4 = vec![(step4_biome, 1.0), (step4_surface, 1.0)];
        let frame4 = frame_tiles(&view, paint4.clone(), fill_pass(&paint4), None)
            .expect("the view resolves");
        assert_eq!(frame4.passes.len(), 1);
        assert_eq!(
            frame4.passes[0].fidelity,
            SummaryFidelity::Full,
            "a user-chosen step-4 view is authoritative, never a placeholder"
        );
    }

    #[test]
    fn surface_at_the_coarse_step_rides_the_ordinary_fill_pass() {
        // At step 4 the coarse rung *is* the fine step: no split, one
        // fill pass carrying the surface layer, and the paint plan
        // matches it.
        let view = view(0.0, 600.0, 400.0);
        let step4 = ZoomLevel::from_step(4).expect("4 is an in-range step");
        let biome = TileSubject::new(1, 7, Layer::Biome, 0, step4);
        let surface = TileSubject::new(1, 7, Layer::Surface, 0, step4);
        let paint = vec![(biome, 1.0), (surface, 1.0)];
        let frame =
            frame_tiles(&view, paint.clone(), fill_pass(&paint), None).expect("the view resolves");

        assert_eq!(frame.passes.len(), 1, "no progressive split at step 4");
        assert!(!frame.passes[0].refine);
        assert_eq!(
            frame.passes[0].layers, paint,
            "the surface rides the ordinary fill pass"
        );
        assert_eq!(frame.paint.layers, paint);
    }

    #[test]
    fn world_rect_converts_chunk_bounds_to_blocks() {
        // Chunk c spans blocks c*32 ..= c*32 + 31, square in x/z.
        assert_eq!(world_rect((-2, 3)), Some(Rect::new(-64, -64, 127, 127)));
        assert_eq!(world_rect((0, 0)), Some(Rect::new(0, 0, 31, 31)));
        // Absurd bounds refuse instead of overflowing the block space.
        assert_eq!(world_rect((i32::MIN, i32::MAX)), None);
    }

    // -- The pipeline drive (fake source, no window) ---------------------------

    #[test]
    fn tile_pipeline_serves_and_uploads_every_visible_tile() {
        let ctx = egui::Context::default();
        let mut map = MapView::new();
        let mut pipeline = fake_pipeline();
        let view = view(0.0, 600.0, 400.0);
        let request = request_for(&pipeline, &view, ZoomLevel::MIN, Layer::Biome);

        settle(&mut map, &ctx, &mut pipeline, &request);

        // Every visible tile is cached and uploaded exactly once.
        for coord in request.paint.visible.iter() {
            let (Ok(tx), Ok(ty)) = (i32::try_from(coord.x), i32::try_from(coord.z)) else {
                continue;
            };
            let key = request.paint.layers[0].0.key_at(tx, ty);
            assert!(pipeline.cache.contains(&key), "tile {coord:?} is cached");
            assert!(
                map.textures.texture_id(&key).is_some(),
                "tile {coord:?} is uploaded"
            );
        }

        // The fake's 8-block checkerboard paints many distinct colors
        // over one tile, none of them the canvas.
        let center_key = request.paint.layers[0].0.key_at(0, 0);
        let tile = pipeline
            .cache
            .get(&center_key)
            .expect("the center tile is cached");
        assert_eq!((tile.width, tile.height), (TILE_SIZE, TILE_SIZE));
        let mut colors = HashSet::new();
        for y in 0..tile.height {
            for x in 0..tile.width {
                let pixel = tile.pixel(x, y).expect("in bounds");
                assert_eq!(pixel[3], 255, "the fake world covers the whole tile");
                colors.insert([pixel[0], pixel[1], pixel[2]]);
            }
        }
        assert!(colors.len() > 1, "a colored map, not a flat one");
    }

    #[test]
    fn an_unchanged_frame_requeues_nothing() {
        // The "no request when the visible set is unchanged" acceptance
        // rule, witnessed at the pipeline level: serving the identical
        // frame twice leaves the scheduler on its first generation with
        // nothing queued.
        let ctx = egui::Context::default();
        let mut map = MapView::new();
        let mut pipeline = fake_pipeline();
        let view = view(0.0, 600.0, 400.0);
        let request = request_for(&pipeline, &view, ZoomLevel::MIN, Layer::Biome);

        settle(&mut map, &ctx, &mut pipeline, &request);
        assert_eq!(pipeline.scheduler.stats().generations, 1);

        map.serve(&ctx, &mut pipeline, Some(&request));
        assert_eq!(
            pipeline.scheduler.stats().generations,
            1,
            "an unchanged frame never re-requests"
        );
        assert_eq!(map.pending_tiles(), 0);
    }

    #[test]
    fn tiles_that_scroll_out_of_view_are_cancelled() {
        // A far-away second view: the first view's tiles are no longer
        // pending — cached (the fake renders instantly) or cancelled,
        // never still queued — and the request generation moved.
        let ctx = egui::Context::default();
        let mut map = MapView::new();
        let mut pipeline = fake_pipeline();
        let first_view = view(0.0, 600.0, 400.0);
        let first = request_for(&pipeline, &first_view, ZoomLevel::MIN, Layer::Biome);
        settle(&mut map, &ctx, &mut pipeline, &first);
        let first_key = first.paint.layers[0].0.key_at(0, 0);

        let mut far_view = view(0.0, 600.0, 400.0);
        far_view.pan_by_screen((200_000.0, 0.0));
        let second = request_for(&pipeline, &far_view, ZoomLevel::MIN, Layer::Biome);
        assert!(
            !second.paint.visible.contains(TileCoord::new(0, 0)),
            "the two views are disjoint"
        );

        map.serve(&ctx, &mut pipeline, Some(&second));
        assert!(!pipeline.scheduler.is_pending(&first_key));
        assert_eq!(
            pipeline.scheduler.stats().generations,
            2,
            "the request moved with the view"
        );
    }

    #[test]
    fn missing_tiles_find_their_cached_parent_as_a_placeholder() {
        // Zoom out: the coarser step-2 tiles render and upload. Zoom
        // back in: the step-1 children are missing, but each finds its
        // cached parent with the exact crop for its world rect.
        let ctx = egui::Context::default();
        let mut map = MapView::new();
        let mut pipeline = fake_pipeline();

        let coarse_view = view(1.0, 600.0, 400.0);
        let coarse = request_for(
            &pipeline,
            &coarse_view,
            ZoomLevel::new(1).expect("in range"),
            Layer::Biome,
        );
        settle(&mut map, &ctx, &mut pipeline, &coarse);

        let fine_view = view(0.0, 600.0, 400.0);
        let fine = request_for(&pipeline, &fine_view, ZoomLevel::MIN, Layer::Biome);
        let child = fine.paint.layers[0].0.key_at(0, 0);
        assert!(
            !pipeline.cache.contains(&child),
            "the fine tile has not been requested"
        );

        let placeholder = covering_placeholder(&pipeline.cache, &child)
            .expect("the coarse parent covers the child");
        assert_eq!(
            placeholder.tile.key.zoom,
            ZoomLevel::new(1).expect("in range")
        );
        // The parent was the visible level itself a moment ago, so its
        // texture is already uploaded — painting only crops it.
        assert!(map.textures.texture_id(&placeholder.tile.key).is_some());
        // Tile (0, 0) is the north-west quarter of its parent.
        assert_eq!(placeholder.uv, [0.0, 0.0, 0.5, 0.5]);
    }

    #[test]
    fn refinement_coarse_pass_placeholds_the_fine_surface_tiles() {
        // T21 end to end on the fake pipeline: the coarse fill pass is
        // served first and is never painted directly — it exists so a
        // pending fine surface tile finds a cached parent to crop.
        let ctx = egui::Context::default();
        let mut map = MapView::new();
        let mut pipeline = fake_pipeline();
        let view = view(0.0, 600.0, 400.0);
        let coarse = ZoomLevel::from_step(4).expect("4 is an in-range step");

        // The app's coarse fill pass, alone: only the surface layer at
        // the step-4 rung.
        let coarse_surface = TileSubject::new(
            pipeline.served.0,
            pipeline.served.1,
            Layer::Surface,
            0,
            coarse,
        );
        let coarse_request = frame_tiles(
            &view,
            vec![(coarse_surface, 1.0)],
            vec![PassLayers {
                layers: vec![(coarse_surface, 1.0)],
                refine: false,
            }],
            pipeline.world,
        )
        .expect("the view resolves");
        settle(&mut map, &ctx, &mut pipeline, &coarse_request);
        assert!(
            pipeline.cache.contains(&coarse_surface.key_at(0, 0)),
            "the coarse fill is cached"
        );

        // The full progressive frame the app builds next: biome fill,
        // coarse surface fill, fine surface refine.
        let fine_surface = TileSubject::new(
            pipeline.served.0,
            pipeline.served.1,
            Layer::Surface,
            0,
            ZoomLevel::MIN,
        );
        let biome = TileSubject::new(
            pipeline.served.0,
            pipeline.served.1,
            Layer::Biome,
            0,
            ZoomLevel::MIN,
        );
        let fine_key = fine_surface.key_at(0, 0);
        assert!(
            !pipeline.cache.contains(&fine_key),
            "the fine tile has not rendered yet"
        );
        let placeholder = covering_placeholder(&pipeline.cache, &fine_key)
            .expect("the coarse fill covers the fine tile");
        assert_eq!(
            placeholder.tile.key.zoom, coarse,
            "the step-4 rung placeholds the fine surface tile"
        );

        let request = frame_tiles(
            &view,
            vec![(biome, 1.0), (fine_surface, 1.0)],
            vec![
                PassLayers {
                    layers: vec![(biome, 1.0)],
                    refine: false,
                },
                PassLayers {
                    layers: vec![(coarse_surface, 1.0)],
                    refine: false,
                },
                PassLayers {
                    layers: vec![(fine_surface, 1.0)],
                    refine: true,
                },
            ],
            pipeline.world,
        )
        .expect("the view resolves");
        settle(&mut map, &ctx, &mut pipeline, &request);
        assert!(
            pipeline.cache.contains(&fine_key),
            "serving the full frame refines the fine tile"
        );
    }

    #[test]
    fn switching_layers_serves_only_the_new_layers_tiles() {
        // The T17 acceptance rule "switching layers does not show stale
        // tiles from the previous layer", at the pipeline level: the
        // subject's layer moves with the checkbox, keys differ per
        // layer, and the new layer's tile renders through the same
        // pipeline even though the old layer's tile at the same
        // position is already cached.
        let ctx = egui::Context::default();
        let mut map = MapView::new();
        let mut pipeline = fake_pipeline();
        let view = view(0.0, 600.0, 400.0);

        let biomes = request_for(&pipeline, &view, ZoomLevel::MIN, Layer::Biome);
        settle(&mut map, &ctx, &mut pipeline, &biomes);
        let biome_key = biomes.paint.layers[0].0.key_at(0, 0);
        assert!(pipeline.cache.contains(&biome_key), "biomes served first");

        // The same view, the Height checkbox: a different subject, keys
        // the cached biome tile can never satisfy.
        let heights = request_for(&pipeline, &view, ZoomLevel::MIN, Layer::Height);
        assert_eq!(heights.paint.layers[0].0.layer, Layer::Height);
        let height_key = heights.paint.layers[0].0.key_at(0, 0);
        assert_ne!(height_key, biome_key, "layers never share a key");
        assert!(
            !pipeline.cache.contains(&height_key),
            "the cached biome tile does not answer the height layer"
        );

        settle(&mut map, &ctx, &mut pipeline, &heights);
        assert!(
            pipeline.cache.contains(&height_key),
            "the height tile served"
        );
        assert!(map.textures.texture_id(&height_key).is_some());
        // And the biome tile is still cached for the switch back.
        assert!(pipeline.cache.contains(&biome_key));
    }

    #[test]
    fn layers_enabled_together_serve_independent_keys() {
        // T19's compositing row: a two-layer frame requests BOTH
        // layers' tiles through one schedule call — independent keys,
        // both cached and uploaded, the bottom layer's cache untouched
        // by the overlay's arrival.
        let ctx = egui::Context::default();
        let mut map = MapView::new();
        let mut pipeline = fake_pipeline();
        let view = view(0.0, 600.0, 400.0);

        let request = request_for_layers(
            &pipeline,
            &view,
            ZoomLevel::MIN,
            &[Layer::Biome, Layer::Fluid],
        );
        settle(&mut map, &ctx, &mut pipeline, &request);
        assert_eq!(request.paint.layers.len(), 2);

        let biome_key = request.paint.layers[0].0.key_at(0, 0);
        let fluid_key = request.paint.layers[1].0.key_at(0, 0);
        assert_ne!(biome_key, fluid_key, "layers never share a key");
        assert!(
            pipeline.cache.contains(&biome_key) && pipeline.cache.contains(&fluid_key),
            "both layers' tiles cached"
        );
        assert!(
            map.textures.texture_id(&biome_key).is_some()
                && map.textures.texture_id(&fluid_key).is_some(),
            "both layers' tiles uploaded"
        );

        // An unchanged two-layer frame is still a no-op (the dedup
        // guard covers the whole subject set at once).
        let generations = pipeline.scheduler.stats().generations;
        map.serve(&ctx, &mut pipeline, Some(&request));
        assert_eq!(
            pipeline.scheduler.stats().generations,
            generations,
            "the same subject set never re-requests"
        );
    }

    #[test]
    fn layers_disabled_layers_stop_rendering_and_revive_cleanly() {
        // The T19 toggle rule at the pipeline level: a frame that drops
        // a layer cancels that layer's pending tiles (the scheduler's
        // `cancel_not_serving`, witnessed by the cancelled stat), and a
        // later frame that re-adds the layer serves it again — the
        // completed base layer's cache was never disturbed.
        let ctx = egui::Context::default();
        let mut map = MapView::new();
        let mut pipeline = fake_pipeline();
        let view = view(0.0, 600.0, 400.0);

        // Base layer settles; the fluid overlay then joins the frame.
        let biomes = request_for(&pipeline, &view, ZoomLevel::MIN, Layer::Biome);
        settle(&mut map, &ctx, &mut pipeline, &biomes);
        let biome_key = biomes.paint.layers[0].0.key_at(0, 0);

        let both = request_for_layers(
            &pipeline,
            &view,
            ZoomLevel::MIN,
            &[Layer::Biome, Layer::Fluid],
        );
        map.serve(&ctx, &mut pipeline, Some(&both));
        let fluid_key = both.paint.layers[1].0.key_at(0, 0);

        // The fluid checkbox unticks possibly before its tiles finish:
        // the very next frame stops serving them — whichever way the
        // in-flight race resolves (completed into the cache or
        // cancelled), the pipeline settles with nothing pending for the
        // disabled layer and the base layer's cache untouched.
        map.serve(&ctx, &mut pipeline, Some(&biomes));
        let deadline = Instant::now() + Duration::from_secs(10);
        while pipeline.scheduler.is_pending(&fluid_key) && Instant::now() < deadline {
            map.serve(&ctx, &mut pipeline, Some(&biomes));
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !pipeline.scheduler.is_pending(&fluid_key),
            "the disabled layer left the pipeline"
        );
        assert!(
            pipeline.cache.contains(&biome_key),
            "the base layer's cache is untouched by the toggle"
        );

        // Re-enabling serves the overlay again, fresh keys intact.
        settle(&mut map, &ctx, &mut pipeline, &both);
        assert!(pipeline.cache.contains(&fluid_key));
    }

    #[test]
    fn resolution_change_rekeys_the_frame_request() {
        // The selector sets the user's minimum step; the frame request
        // samples at the effective one, so switching 2x2 -> 1x1 rekeys
        // every visible tile: the fine keys are uncached until they
        // render, while the coarse tiles stay cached underneath (the
        // parent-placeholder crop of T16).
        let ctx = egui::Context::default();
        let mut map = MapView::new();
        let mut pipeline = fake_pipeline();

        // The 2x2 default at one point per block: effective step 2.
        let coarse_view = view(0.0, 600.0, 400.0);
        let coarse = request_for(
            &pipeline,
            &coarse_view,
            ZoomLevel::new(1).expect("in range"),
            Layer::Biome,
        );
        settle(&mut map, &ctx, &mut pipeline, &coarse);
        let coarse_key = coarse.paint.layers[0].0.key_at(0, 0);
        assert!(pipeline.cache.contains(&coarse_key));

        // Selecting 1x1 in the top bar maps the frame to the full-block
        // level: different subject zoom, different keys.
        let fine = request_for(&pipeline, &coarse_view, ZoomLevel::MIN, Layer::Biome);
        assert_ne!(fine.paint.layers[0].0.zoom, coarse.paint.layers[0].0.zoom);
        let fine_key = fine.paint.layers[0].0.key_at(0, 0);
        assert_ne!(fine_key, coarse_key, "the resolution rekeys the tile");
        assert!(
            !pipeline.cache.contains(&fine_key),
            "the fine tile renders fresh"
        );
        assert!(
            pipeline.cache.contains(&coarse_key),
            "the coarse tile stays cached for the placeholder crop"
        );
    }

    // -- Pooled placeholders (zoom-out stand-ins) -------------------------------

    /// A solid small (2×2) tile under `key` — small tiles keep the
    /// placeholder tests' cache budgets tiny while still exercising the
    /// real `Tile` grid the pool samples.
    fn solid_tile(key: TileKey, color: [u8; 4]) -> Arc<Tile> {
        let rgba: Vec<u8> = color.iter().copied().cycle().take(16).collect();
        Arc::new(Tile::new(key, 2, 2, rgba).expect("the buffer matches 2x2 RGBA"))
    }

    /// A biome key at `zoom` for tile `(tx, ty)` under the fake identity.
    fn biome_key(zoom: ZoomLevel, tx: i32, ty: i32) -> TileKey {
        TileSubject::new(1, 7, Layer::Biome, 0, zoom).key_at(tx, ty)
    }

    /// A biome key at `zoom` for tile `(tx, ty)` under `pipeline`'s own
    /// subject — the keys its frame requests actually carry.
    fn pipeline_key(pipeline: &TilePipeline, zoom: ZoomLevel, tx: i32, ty: i32) -> TileKey {
        TileSubject::new(pipeline.served.0, pipeline.served.1, Layer::Biome, 0, zoom).key_at(tx, ty)
    }

    #[test]
    fn pooling_pending_fill_pools_cached_fine_children_on_zoom_out() {
        // The zoom-out path: the step-2 tile is pending and nothing
        // coarser exists, but its four step-1 children were just on
        // screen, so the fill is their box average.
        let cache = TileCache::new(1024 * 1024);
        let fine = ZoomLevel::MIN;
        let coarse = ZoomLevel::new(1).expect("step 2 is in range");
        for (tx, ty) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            cache.insert(solid_tile(biome_key(fine, tx, ty), [30, 60, 90, 255]));
        }

        let key = biome_key(coarse, 0, 0);
        match pending_fill(&cache, &key) {
            PendingFill::Pooled(pooled) => {
                assert_eq!(pooled.key, key, "the pool answers for the pending tile");
                assert_eq!(pooled.source_zoom, fine, "pooled from the level just left");
                assert_eq!((pooled.width, pooled.height), (2, 2));
                assert!(
                    pooled
                        .rgba
                        .chunks(4)
                        .all(|pixel| pixel == [30, 60, 90, 255]),
                    "four identical children average to their color"
                );
            }
            other => panic!("expected a pooled stand-in, got {other:?}"),
        }
    }

    #[test]
    fn pooling_pending_fill_prefers_a_cached_coarser_cover() {
        // Zoom-in: a cached parent exists, so the covering crop wins even
        // when finer children are also cached — the pool is the fallback,
        // not the first choice (a real tile beats a derived average).
        let cache = TileCache::new(1024 * 1024);
        let key_zoom = ZoomLevel::new(2).expect("step 4 is in range");
        let parent = biome_key(ZoomLevel::new(3).expect("step 8 is in range"), 0, 0);
        cache.insert(solid_tile(parent, [10, 20, 30, 255]));
        for (tx, ty) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            cache.insert(solid_tile(
                biome_key(ZoomLevel::new(1).expect("step 2 is in range"), tx, ty),
                [40, 50, 60, 255],
            ));
        }

        let key = biome_key(key_zoom, 0, 0);
        match pending_fill(&cache, &key) {
            PendingFill::Covering(covering) => {
                assert_eq!(covering.tile.key, parent, "the cached parent covers it");
            }
            other => panic!("expected a covering stand-in, got {other:?}"),
        }
    }

    #[test]
    fn pooling_pending_fill_blanks_without_any_cached_cover() {
        let cache = TileCache::new(1024 * 1024);
        let key = biome_key(ZoomLevel::new(1).expect("step 2 is in range"), 0, 0);
        assert!(
            matches!(pending_fill(&cache, &key), PendingFill::Blank),
            "an empty cache has nothing to stand in with"
        );
    }

    #[test]
    fn pooling_pending_fill_handles_negative_tile_indices() {
        // Negative indices floor toward the right parent: coarse (-1, -1)
        // owns the children (-2, -2)..(-1, -1), exactly as
        // `pooled_placeholder` documents.
        let cache = TileCache::new(1024 * 1024);
        let fine = ZoomLevel::MIN;
        let coarse = ZoomLevel::new(1).expect("step 2 is in range");
        for (tx, ty) in [(-2, -2), (-1, -2), (-2, -1), (-1, -1)] {
            cache.insert(solid_tile(biome_key(fine, tx, ty), [1, 2, 3, 255]));
        }
        let key = biome_key(coarse, -1, -1);
        match pending_fill(&cache, &key) {
            PendingFill::Pooled(pooled) => {
                assert_eq!(pooled.key, key);
                assert_eq!(pooled.source_zoom, fine);
            }
            other => panic!("expected a pooled stand-in, got {other:?}"),
        }

        // And a negative parent still covers a negative child.
        let child = biome_key(ZoomLevel::MIN, -1, -1);
        let parent = biome_key(ZoomLevel::new(1).expect("step 2 is in range"), -1, -1);
        cache.insert(solid_tile(parent, [4, 5, 6, 255]));
        assert!(
            matches!(pending_fill(&cache, &child), PendingFill::Covering(c) if c.tile.key == parent),
            "the coarser scan crosses negative indices too"
        );
    }

    #[test]
    fn pooling_zoom_out_paints_and_uploads_the_pooled_tile_once() {
        // The paint path, headless: a coarse tile with no exact texture and
        // no coarser cover paints its pooled children and uploads that
        // derivation once per (tile, source level), not once per frame.
        let ctx = egui::Context::default();
        let mut map = MapView::new();
        map.rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(600.0, 400.0));
        let pipeline = fake_pipeline();
        let (fine, coarse) = (
            ZoomLevel::MIN,
            ZoomLevel::new(1).expect("step 2 is in range"),
        );
        for (tx, ty) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            pipeline.cache.insert(solid_tile(
                pipeline_key(&pipeline, fine, tx, ty),
                [30, 60, 90, 255],
            ));
        }
        let coarse_view = view(1.0, 600.0, 400.0);
        let request = request_for(&pipeline, &coarse_view, coarse, Layer::Biome);
        assert!(
            request.paint.visible.contains(TileCoord::new(0, 0)),
            "the view paints the tile whose children are cached"
        );
        let key = pipeline_key(&pipeline, coarse, 0, 0);
        assert!(
            !pipeline.cache.contains(&key),
            "the coarse tile is still pending"
        );

        let painter = egui::Painter::new(ctx.clone(), egui::LayerId::background(), map.rect);
        map.paint(&ctx, &painter, &request, &pipeline, &coarse_view);
        assert!(
            map.textures.texture_id(&key).is_none(),
            "the derived pixels are never cached under the tile key"
        );
        let id = map
            .textures
            .pooled_texture_id(&key)
            .expect("the pending coarse tile pooled its cached children");

        // The next frame reuses the handle: one upload per (tile, level).
        map.paint(&ctx, &painter, &request, &pipeline, &coarse_view);
        assert_eq!(
            map.textures.pooled_texture_id(&key),
            Some(id),
            "a painted pooled tile is not re-derived and re-uploaded"
        );

        // The real tile's arrival retires the derivation under it.
        map.textures
            .upload(&ctx, key, &solid_tile(key, [9, 9, 9, 255]));
        assert_eq!(
            map.textures.pooled_texture_id(&key),
            None,
            "the exact texture replaces the pool"
        );
    }

    // -- The real generator (gated like every generator-backed test) ------------

    /// The worldgen checkout this repository's sibling ships for tests.
    fn world_dir() -> Option<PathBuf> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../hytale-server-rust/testdata/Default");
        // The committed world's prefab references need the user-supplied
        // asset pack; without it the generator cannot load (CI has none).
        let assets = dir.join("../../Assets/extracted");
        (dir.is_dir() && assets.is_dir()).then_some(dir)
    }

    #[test]
    fn tile_pipeline_renders_a_colored_real_map_tile() {
        // The T13 acceptance's successor, through the T16 tile pipeline:
        // the real generator serves one real tile, colored by its own
        // catalog. Nearest-first queueing makes the center tile the
        // first job, bounding the test to one region query.
        let Some(world) = world_dir() else {
            eprintln!("skipping: the worldgen checkout is not present");
            return;
        };
        let config = WorldgenSourceConfig::new(&world, "12345");
        let source = Arc::new(WorldgenSource::new(&config).expect("the golden world loads"));
        let world = world_rect(source.chunk_bounds());
        // The concrete Arc unsizes into the pipeline's dyn source at the
        // call, after the bounds were read.
        let mut pipeline = TilePipeline::new(source, world);

        // The plan's default resolution: 2x2 at one point per block ->
        // effective step 2 (level 1).
        let view = view(0.0, 600.0, 400.0);
        let request = request_for(
            &pipeline,
            &view,
            ZoomLevel::new(1).expect("in range"),
            Layer::Biome,
        );
        let center_key = request.paint.layers[0].0.key_at(
            i32::try_from(request.paint.center.x).expect("in i32"),
            i32::try_from(request.paint.center.z).expect("in i32"),
        );

        let ctx = egui::Context::default();
        let mut map = MapView::new();
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            map.serve(&ctx, &mut pipeline, Some(&request));
            if pipeline.cache.contains(&center_key) || Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let tile = pipeline
            .cache
            .get(&center_key)
            .expect("the center tile rendered");
        assert_eq!((tile.width, tile.height), (TILE_SIZE, TILE_SIZE));
        assert!(
            map.textures.texture_id(&center_key).is_some(),
            "the tile uploaded"
        );

        // A biome-colored map, not a placeholder: many colors among the
        // opaque pixels, no unknown-biome magenta, and any transparent
        // margin is the world edge, not missing data.
        let mut colors = HashSet::new();
        let mut opaque = 0;
        for y in 0..tile.height {
            for x in 0..tile.width {
                let pixel = tile.pixel(x, y).expect("in bounds");
                if pixel[3] == 0 {
                    continue;
                }
                opaque += 1;
                assert_ne!(
                    [pixel[0], pixel[1], pixel[2]],
                    UNKNOWN_BIOME_COLOR,
                    "the catalog covers the golden world"
                );
                colors.insert([pixel[0], pixel[1], pixel[2]]);
            }
        }
        assert!(opaque > 0, "the center tile is inside the world");
        assert!(
            colors.len() > 1,
            "a real biome map shows many colors, got {}",
            colors.len()
        );
    }
}
