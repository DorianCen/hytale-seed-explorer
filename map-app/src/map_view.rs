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
//! 1. compute the frame's request (pure: [`frame_tiles`]);
//! 2. when it *changed*, hand it to the scheduler (it queues the
//!    missing tiles nearest the center first) and cancel everything
//!    that scrolled out of view;
//! 3. drain scheduler events — `Ready` tiles upload into the GPU bridge
//!    exactly once per key, the tile cache being the state of record;
//! 4. drop GPU textures for CPU-evicted tiles (T14's contract);
//! 5. paint each visible tile: its texture, else the nearest cached
//!    coarser tile cropped to this tile's world rect, else a subtle
//!    loading tint on the canvas.
//!
//! Pipelines never die on the UI thread: a stale one (its source was
//! invalidated or reloaded) is parked in `retired` and the app drops
//! it on a background thread — [`TileScheduler`]'s `Drop` joins its
//! workers, the same reason [`crate::state`]'s sources retire that way.
//!
//! # Orientation
//!
//! Painting consumes the T10 viewport transform as-is: screen Y grows
//! downward, world Z grows south, so north (−Z) sits at the screen top
//! — the same orientation contract the renderers already bake into
//! their pixels (tile row 0 = north). Hover readouts go through the
//! same transform, which is what keeps the coordinates honest.

use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Receiver;
use eframe::egui;
use egui::{Color32, PointerButton, Response};

use map_core::coords::{TileCoord, TileRect, ZoomLevel, floor_div, tiles_in};
use map_core::render::Layer;
use map_core::render::biome::BiomeTileRenderer;
use map_core::scheduler::{TileEvent, TileRenderer, TileScheduler, TileSubject};
use map_core::source::WorldSource;
use map_core::tiles::{TileCache, TileKey, covering_placeholder};
use map_core::types::Rect;

use crate::labels;
use crate::state::{AppLayer, AppState};
use crate::texture::TextureCache;
use crate::view_state::{MapViewState, expand_to_pixels, tile_screen_rect};

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

// ---------------------------------------------------------------------------
// The frame request (pure, headless-testable)
// ---------------------------------------------------------------------------

/// One frame's tile request: what the scheduler should be serving.
///
/// Pure on purpose — the same view, subject and world always produce the
/// identical request, which is what keeps the pipeline from re-queuing
/// differently-rounded tiles every frame.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameTiles {
    /// The tile identity every visible tile renders under (identity
    /// hash, seed, layer, mode, effective zoom).
    pub subject: TileSubject,
    /// The visible tile range, one-tile-expanded (T10) and clamped to
    /// the generatable world.
    pub visible: TileRect,
    /// The tile the queue's nearest-first priorities radiate from.
    pub center: TileCoord,
}

/// Computes the frame's request from the pure view state: the visible
/// tile range at the subject's step, clamped to the generatable world,
/// plus the center tile for priorities.
///
/// `None` when the view leaves the representable block space or lies
/// entirely outside the world — the widget paints the canvas and
/// requests nothing rather than a range that cannot exist. (Tiles
/// already queued for a superseded range are left to finish or be
/// cancelled by a later request; the degenerate case is a view flung
/// far off-world, and their pixels stay cached and unused.)
#[must_use]
pub fn frame_tiles(
    view: &MapViewState,
    subject: TileSubject,
    world: Option<Rect>,
) -> Option<FrameTiles> {
    let step = subject.zoom.step();
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
    Some(FrameTiles {
        subject,
        visible,
        center,
    })
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
    /// Builds the pipeline for `source`: a fresh cache, the biome tile
    /// renderer with the world's limits, and the default worker pool.
    ///
    /// The identity + seed are pinned *before* the renderer takes the
    /// source — they key every tile this pipeline ever requests, and a
    /// source that moved underneath means a new pipeline.
    fn new(source: Arc<dyn WorldSource>, world: Option<Rect>) -> Self {
        let served = (source.identity().stable_hash(), source.seed().generator());
        let cache = Arc::new(TileCache::new(TILE_CACHE_BUDGET_BYTES));
        let renderer = Arc::new(BiomeTileRenderer::new(source, world));
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
            // A later task's layer renders no tiles, but the drain below
            // still keeps the pending count honest while its placeholder
            // is on screen.
            let request = (state.layer() == AppLayer::Biomes)
                .then(|| {
                    let view = state.view();
                    let step = view.tile_step(state.resolution_step());
                    // The effective step is a power of two inside the
                    // level range by construction (T10), so this expect is
                    // an invariant, not a failure path.
                    let zoom = ZoomLevel::from_step(step)
                        .expect("effective_step yields an in-range power of two");
                    let subject = TileSubject::new(
                        pipeline.served.0,
                        pipeline.served.1,
                        Layer::Biome,
                        0,
                        zoom,
                    );
                    frame_tiles(view, subject, pipeline.world)
                })
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
                // Canvas plus a message: a layer whose renderer is a
                // later task's (a source-less view takes the same path
                // below).
                self.placeholder(ui, &painter, state);
            }
            self.pipeline = Some(pipeline);
        } else {
            // No source (yet): canvas plus a message, no tile traffic.
            self.placeholder(ui, &painter, state);
        }
    }

    // -- Input --------------------------------------------------------------

    /// Pointer input into the headless view: hover readout, drag pan,
    /// wheel zoom at the cursor, click-to-copy coordinates.
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

        // A primary click without a drag copies the block coordinates
        // (egui separates clicks from drags for us). The exact string is
        // T18/T31's decision; "x z" is the working placeholder.
        if response.clicked_by(PointerButton::Primary)
            && let Some((x, z)) = state.cursor()
        {
            response.ctx.copy_text(format!("{x} {z}"));
            state.set_status(format!("{} X:{x} Z:{z}", labels::STATUS_COPIED));
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
    /// the scheduler, cancel what left the view, upload finished tiles
    /// once per key, drop evicted textures, and note the pending count.
    fn serve(
        &mut self,
        ctx: &egui::Context,
        pipeline: &mut TilePipeline,
        request: Option<&FrameTiles>,
    ) {
        if let Some(request) = request
            && pipeline.last.as_ref() != Some(request)
        {
            // New view or zoom: queue what is missing nearest-center
            // first, cancel what scrolled away, and remember the
            // request so an unchanged frame is a no-op.
            pipeline.scheduler.schedule(
                request.subject,
                request.visible,
                request.center,
                Arc::clone(&pipeline.renderer),
            );
            pipeline.scheduler.cancel_outside(&request.visible);
            pipeline.last = Some(request.clone());
        }

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
        // The GPU mirror follows every CPU eviction (T14's contract).
        self.textures.drop_evicted(pipeline.evictions.try_iter());

        let stats = pipeline.scheduler.stats();
        self.pending = stats.queued + stats.in_flight;
        if self.pending > 0 {
            // Results arrive on worker threads; keep frames coming so
            // a finished tile appears on the very next one.
            ctx.request_repaint_after(TILE_POLL_INTERVAL);
        }
    }

    // -- Painting ---------------------------------------------------------------

    /// Paints each visible tile: its exact texture, else the nearest
    /// cached coarser tile cropped to this tile's world rect, else a
    /// subtle pending tint. Adjacent tiles agree on their shared edge
    /// because floor/ceil to physical pixels is a pure function of the
    /// shared coordinate — no seams, no double-paint.
    fn paint(
        &mut self,
        ctx: &egui::Context,
        painter: &egui::Painter,
        request: &FrameTiles,
        pipeline: &TilePipeline,
        view: &MapViewState,
    ) {
        let step = request.subject.zoom.step();
        let ppp = ctx.pixels_per_point();
        for coord in request.visible.iter() {
            // Visible ranges are i64 tile space; keys are i32 — the
            // reachable world sits far inside (T10's bounds analysis).
            let (Ok(tx), Ok(ty)) = (i32::try_from(coord.x), i32::try_from(coord.z)) else {
                continue;
            };
            let key = request.subject.key_at(tx, ty);
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

            if let Some(id) = self.textures.texture_id(&key) {
                painter.image(id, screen, full_uv(), Color32::WHITE);
            } else if let Some(placeholder) = covering_placeholder(&pipeline.cache, &key) {
                // Instant low detail while the real tile renders. The
                // parent may be cached but not yet uploaded — upload it
                // on demand; the exact tile's later upload replaces it.
                let id = self
                    .textures
                    .texture_id(&placeholder.tile.key)
                    .unwrap_or_else(|| {
                        self.textures
                            .upload(ctx, placeholder.tile.key, &placeholder.tile)
                    });
                let uv = egui::Rect::from_min_max(
                    egui::pos2(placeholder.uv[0], placeholder.uv[1]),
                    egui::pos2(placeholder.uv[2], placeholder.uv[3]),
                );
                painter.image(id, screen, uv, Color32::WHITE);
            } else if pipeline.scheduler.is_pending(&key) {
                // Pending with no placeholder: a quiet "filling in".
                painter.rect_filled(screen, 0.0, pending_tint());
                painter.rect_stroke(screen, 0.0, pending_stroke(), egui::StrokeKind::Middle);
            }
        }
    }

    // -- The no-tile states -----------------------------------------------------

    /// The canvas-only states: no source, a failed load, or a layer
    /// whose renderer is a later task's.
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
            Some(_) => labels::MAP_PLACEHOLDER_LAYER.to_string(),
            None => match state.source_error() {
                Some(error) => {
                    format!("{} {error}", labels::MAP_PLACEHOLDER_ERROR_PREFIX)
                }
                None if state.is_loading() => labels::MAP_PLACEHOLDER_LOADING.to_string(),
                None => labels::MAP_PLACEHOLDER_NO_SOURCE.to_string(),
            },
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
    use map_core::source::worldgen::{WorldgenSource, WorldgenSourceConfig};
    use map_core::test_support::FakeWorldSource;
    use map_core::tiles::covering_placeholder;
    use map_core::types::Rect;

    use super::{FrameTiles, MapView, TilePipeline, TileSubject, frame_tiles, world_rect};
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

    /// The frame request for `pipeline`'s own subject at `zoom`.
    fn request_for(pipeline: &TilePipeline, view: &MapViewState, zoom: ZoomLevel) -> FrameTiles {
        let subject = TileSubject::new(pipeline.served.0, pipeline.served.1, Layer::Biome, 0, zoom);
        frame_tiles(view, subject, pipeline.world)
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
        let request = frame_tiles(&view, subject, None).expect("the view resolves");
        assert_eq!(request.visible.min, TileCoord::new(-6, -2));
        assert_eq!(request.visible.max, TileCoord::new(0, 3));
        assert_eq!(request.center, TileCoord::new(-3, 0));
    }

    #[test]
    fn frame_tiles_clamps_the_range_to_the_generatable_world() {
        // A 200x200-block world around the origin: at step 1 exactly
        // tiles (-1, -1)..(0, 0), no matter how wide the view is.
        let view = view(0.0, 1024.0, 768.0);
        let world = Rect::new(-100, -100, 99, 99);
        let subject = TileSubject::new(1, 7, Layer::Biome, 0, ZoomLevel::MIN);
        let request =
            frame_tiles(&view, subject, Some(world)).expect("the view overlaps the world");
        assert_eq!(request.visible.min, TileCoord::new(-1, -1));
        assert_eq!(request.visible.max, TileCoord::new(0, 0));
        assert!(
            request.visible.contains(request.center),
            "the priority center clamps into the tiny range"
        );

        // A world entirely outside the view serves nothing.
        let far = Rect::new(100_000, 100_000, 100_200, 100_200);
        assert!(frame_tiles(&view, subject, Some(far)).is_none());
    }

    #[test]
    fn frame_tiles_is_identical_across_unchanged_frames() {
        // The dedup guard: the same inputs produce the identical
        // request, so an unchanged frame re-queues nothing (the
        // generation witness lives in the pipeline test below).
        let view = view(0.0, 1024.0, 768.0);
        let subject = TileSubject::new(1, 7, Layer::Biome, 0, ZoomLevel::MIN);
        assert_eq!(
            frame_tiles(&view, subject, None),
            frame_tiles(&view, subject, None)
        );
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
        let request = request_for(&pipeline, &view, ZoomLevel::MIN);

        settle(&mut map, &ctx, &mut pipeline, &request);

        // Every visible tile is cached and uploaded exactly once.
        for coord in request.visible.iter() {
            let (Ok(tx), Ok(ty)) = (i32::try_from(coord.x), i32::try_from(coord.z)) else {
                continue;
            };
            let key = request.subject.key_at(tx, ty);
            assert!(pipeline.cache.contains(&key), "tile {coord:?} is cached");
            assert!(
                map.textures.texture_id(&key).is_some(),
                "tile {coord:?} is uploaded"
            );
        }

        // The fake's 8-block checkerboard paints many distinct colors
        // over one tile, none of them the canvas.
        let center_key = request.subject.key_at(0, 0);
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
        let request = request_for(&pipeline, &view, ZoomLevel::MIN);

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
        let first = request_for(&pipeline, &first_view, ZoomLevel::MIN);
        settle(&mut map, &ctx, &mut pipeline, &first);
        let first_key = first.subject.key_at(0, 0);

        let mut far_view = view(0.0, 600.0, 400.0);
        far_view.pan_by_screen((200_000.0, 0.0));
        let second = request_for(&pipeline, &far_view, ZoomLevel::MIN);
        assert!(
            !second.visible.contains(TileCoord::new(0, 0)),
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
        );
        settle(&mut map, &ctx, &mut pipeline, &coarse);

        let fine_view = view(0.0, 600.0, 400.0);
        let fine = request_for(&pipeline, &fine_view, ZoomLevel::MIN);
        let child = fine.subject.key_at(0, 0);
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
        let request = request_for(&pipeline, &view, ZoomLevel::new(1).expect("in range"));
        let center_key = request.subject.key_at(
            i32::try_from(request.center.x).expect("in i32"),
            i32::try_from(request.center.z).expect("in i32"),
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
