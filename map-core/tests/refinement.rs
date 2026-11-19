//! T21 acceptance: progressive refinement scheduling end to end.
//!
//! The full-block layer asks for two passes in one frame — a coarse fill
//! that covers the viewport and a finer refine that carries the detail.
//! This file pins the scheduler's half of that contract:
//!
//! - every fill job starts before every refine job (the priority
//!   penalty, not the queue order, decides);
//! - coarse and fine sampling steps are separate cache keys — a coarse
//!   tile can never answer a fine request, but it *is* the fine tile's
//!   placeholder crop;
//! - dropping the refine pass mid-render cancels exactly its tiles and
//!   leaves the fill untouched, and re-requesting revives them;
//! - an unchanged pass set keeps its generation and queues nothing new.
//!
//! Helpers are duplicated from `tests/scheduler.rs` (each integration
//! test file is its own crate; `tests/layers.rs` follows the same
//! pattern for its shared helpers).

use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use map_core::coords::{TileCoord, TileRect, ZoomLevel};
use map_core::error::SourceError;
use map_core::render::Layer;
use map_core::scheduler::{CancelToken, TilePass, TileRenderer, TileScheduler, TileSubject};
use map_core::tiles::{Tile, TileCache, TileKey, covering_placeholder};

// ---------------------------------------------------------------------------
// The fake renderer and timing helpers (mirrors tests/scheduler.rs)
// ---------------------------------------------------------------------------

/// One recorded fake render: its key and start instant. Refinement only
/// needs the start order, so the end time is not kept.
#[derive(Debug, Clone, Copy)]
struct RenderRecord {
    key: TileKey,
    start: Instant,
}

/// The fake renderer: configurable cost, records every render's start
/// and honors cancellation between sleep slices.
#[derive(Clone)]
struct FakeRenderer {
    /// Simulated render cost per tile.
    render_ms: u64,
    /// Every render, in completion order.
    records: Arc<Mutex<Vec<RenderRecord>>>,
}

impl FakeRenderer {
    fn new(render_ms: u64) -> Self {
        Self {
            render_ms,
            records: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The records sorted by start time (the true start order).
    fn start_order(&self) -> Vec<RenderRecord> {
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        records.sort_by_key(|record| record.start);
        records
    }
}

impl TileRenderer for FakeRenderer {
    fn render(&self, key: &TileKey, cancel: &CancelToken) -> Result<Tile, SourceError> {
        let start = Instant::now();
        let deadline = start + Duration::from_millis(self.render_ms);
        while Instant::now() < deadline {
            if cancel.load(Relaxed) {
                return Err(SourceError::Cancelled);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(RenderRecord { key: *key, start });
        let byte = key.tx.rem_euclid(i32::from(u8::MAX)) as u8;
        Ok(Tile::new(*key, 2, 2, vec![byte; 16]).expect("the buffer matches 2x2 RGBA"))
    }
}

/// The coarse pass's subject (step 2) and the fine one's (step 1): the
/// same identity/seed/layer at two sampling steps, so their keys differ
/// only by zoom — the refinement contract's disjoint-key premise. The
/// factor-2 gap also makes the coarse tile the fine one's immediate
/// placeholder parent.
fn coarse() -> TileSubject {
    TileSubject::new(1, 7, Layer::Surface, 0, ZoomLevel(1))
}

fn fine() -> TileSubject {
    TileSubject::new(1, 7, Layer::Surface, 0, ZoomLevel::MIN)
}

/// An `n × n` tile view centered on `(cx, cz)` (inclusive bounds).
fn view(cx: i64, cz: i64, n: i64) -> TileRect {
    let half = n / 2;
    TileRect::new(
        TileCoord::new(cx - half, cz - half),
        TileCoord::new(cx + half, cz + half),
    )
}

/// Sleeps in small slices until `condition` holds, or the deadline
/// passes; returns the condition's final value.
fn wait_until(duration_ms: u64, condition: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_millis(duration_ms);
    loop {
        if condition() || Instant::now() >= deadline {
            return condition();
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// True once the scheduler has nothing queued or in flight.
fn idle(scheduler: &TileScheduler) -> bool {
    let stats = scheduler.stats();
    stats.queued == 0 && stats.in_flight == 0
}

/// The 3×3 fill + refine pair most tests request: both cover the same
/// center tile range at their own steps, with disjoint subject sets.
fn fill_and_refine() -> (TilePass, TilePass) {
    (
        TilePass::fill(vec![coarse()], view(0, 0, 3), TileCoord::new(0, 0)),
        TilePass::refine(vec![fine()], view(0, 0, 3), TileCoord::new(0, 0)),
    )
}

/// True when every tile of `subject`'s 3×3 view around the origin is
/// cached.
fn view_cached(cache: &TileCache, subject: &TileSubject) -> bool {
    (-1..=1).all(|tx| (-1..=1).all(|ty| cache.contains(&subject.key_at(tx, ty))))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn refinement_fill_jobs_pop_before_refine_jobs() {
    // One worker: the recorded start order is the heap's pop order, so a
    // refine tile starting before any fill tile means the penalty failed.
    let cache = Arc::new(TileCache::new(1024 * 1024));
    let scheduler = TileScheduler::new(1, Arc::clone(&cache));
    let renderer = Arc::new(FakeRenderer::new(10));
    let (fill, refine) = fill_and_refine();

    let generation = scheduler.schedule_passes(
        &[fill, refine],
        Arc::clone(&renderer) as Arc<dyn TileRenderer>,
    );
    assert_eq!(generation, 1, "the first request is generation 1");

    assert!(
        wait_until(30_000, || {
            let stats = scheduler.stats();
            stats.completed == 18 && idle(&scheduler)
        }),
        "both passes settle: {:?}",
        scheduler.stats()
    );

    // Every fill job started before every refine job. The subject sets
    // are disjoint by zoom, so a key's zoom names its pass.
    let order = renderer.start_order();
    assert_eq!(order.len(), 18, "each tile rendered exactly once");
    let last_fill = order
        .iter()
        .rposition(|record| record.key.zoom == coarse().zoom)
        .expect("nine fill tiles rendered");
    let first_refine = order
        .iter()
        .position(|record| record.key.zoom == fine().zoom)
        .expect("nine refine tiles rendered");
    assert!(
        last_fill < first_refine,
        "every fill job pops before every refine job (last fill at {last_fill}, \
         first refine at {first_refine})"
    );

    // All 18 tiles completed and landed in the cache under their own key.
    assert!(view_cached(&cache, &coarse()), "the coarse fill is cached");
    assert!(view_cached(&cache, &fine()), "the fine detail is cached");

    let stats = scheduler.stats();
    assert_eq!(stats.completed, 18);
    assert_eq!(stats.cancelled, 0);
    assert_eq!(stats.failed, 0);
}

#[test]
fn refinement_coarse_and_fine_keys_never_share() {
    let cache = Arc::new(TileCache::new(1024 * 1024));
    let scheduler = TileScheduler::new(1, Arc::clone(&cache));
    let renderer = Arc::new(FakeRenderer::new(2));
    let coarse_key = coarse().key_at(0, 0);
    let fine_key = fine().key_at(0, 0);
    assert_ne!(
        coarse_key, fine_key,
        "the zoom is part of the key at the same tile position"
    );

    // The coarse pass alone: its tile is cached, the fine request is not
    // answered by it (a coarse tile can never serve a fine key).
    scheduler.schedule_passes(
        &[TilePass::fill(
            vec![coarse()],
            view(0, 0, 3),
            TileCoord::new(0, 0),
        )],
        Arc::clone(&renderer) as Arc<dyn TileRenderer>,
    );
    assert!(
        wait_until(30_000, || {
            let stats = scheduler.stats();
            stats.completed == 9 && idle(&scheduler)
        }),
        "the coarse pass settles: {:?}",
        scheduler.stats()
    );
    assert!(cache.contains(&coarse_key), "the coarse center is cached");
    assert!(
        !cache.contains(&fine_key),
        "the coarse tile left the fine key uncached"
    );

    // Now the fine pass alone: it renders its own tiles, and the coarse
    // tile serves as the fine one's placeholder crop (the progressive
    // paint's data source).
    scheduler.schedule_passes(
        &[TilePass::refine(
            vec![fine()],
            view(0, 0, 3),
            TileCoord::new(0, 0),
        )],
        Arc::clone(&renderer) as Arc<dyn TileRenderer>,
    );
    assert!(
        wait_until(30_000, || {
            let stats = scheduler.stats();
            stats.completed == 18 && idle(&scheduler)
        }),
        "the fine pass settles on top of the coarse one: {:?}",
        scheduler.stats()
    );
    assert!(cache.contains(&coarse_key), "both keys are cached");
    assert!(cache.contains(&fine_key), "both keys are cached");

    let placeholder = covering_placeholder(&cache, &fine_key)
        .expect("the coarse tile covers the fine key's world rect");
    assert_eq!(
        placeholder.tile.key, coarse_key,
        "the placeholder is the coarse tile of the same subject"
    );
    // The fine tile is the north-west quarter of its step-2 parent.
    assert_eq!(
        placeholder.uv,
        [0.0, 0.0, 0.5, 0.5],
        "the crop is exactly the fine tile's share of the parent"
    );
}

#[test]
fn refinement_cancel_not_serving_drops_only_the_unserved_pass() {
    // Long renders so the cancel lands mid-flight; two workers so the
    // pipeline is genuinely busy when the refine pass is dropped.
    let cache = Arc::new(TileCache::new(1024 * 1024));
    let scheduler = TileScheduler::new(2, Arc::clone(&cache));
    let renderer = Arc::new(FakeRenderer::new(60));
    let (fill, refine) = fill_and_refine();

    scheduler.schedule_passes(
        &[fill.clone(), refine.clone()],
        Arc::clone(&renderer) as Arc<dyn TileRenderer>,
    );
    std::thread::sleep(Duration::from_millis(30));

    // Only the fill pass still serves the frame: every queued refine job
    // is cancelled, no fill job is.
    scheduler.cancel_not_serving(std::slice::from_ref(&fill));

    assert!(
        wait_until(30_000, || idle(&scheduler)),
        "the pipeline settles after dropping the refine pass: {:?}",
        scheduler.stats()
    );
    let stats = scheduler.stats();
    assert_eq!(stats.completed, 9, "all nine fill tiles completed");
    assert_eq!(
        stats.cancelled, 9,
        "exactly the refine pass's nine tiles stopped"
    );
    assert_eq!(stats.failed, 0, "no failures, only cancellations");
    assert!(view_cached(&cache, &coarse()), "the coarse fill is cached");
    assert!(
        !view_cached(&cache, &fine()),
        "no refine tile was rendered after the cancel"
    );

    // Re-requesting both passes revives the refine tiles (they were
    // queued-but-cancelled, not re-created) and both complete.
    scheduler.schedule_passes(
        &[fill, refine],
        Arc::clone(&renderer) as Arc<dyn TileRenderer>,
    );
    assert!(
        wait_until(30_000, || {
            let stats = scheduler.stats();
            stats.completed == 18 && idle(&scheduler)
        }),
        "the refine tiles complete after the revive: {:?}",
        scheduler.stats()
    );
    assert!(view_cached(&cache, &coarse()), "the fill stays cached");
    assert!(
        view_cached(&cache, &fine()),
        "the refined detail is cached after the revive"
    );
    assert_eq!(
        renderer.start_order().len(),
        18,
        "no tile rendered twice across the refine cycle"
    );
}

#[test]
fn refinement_unchanged_passes_dedup_and_changes_bump() {
    let cache = Arc::new(TileCache::new(1024 * 1024));
    let scheduler = TileScheduler::new(2, Arc::clone(&cache));
    let renderer = Arc::new(FakeRenderer::new(2));
    let (fill, refine) = fill_and_refine();

    let first =
        scheduler.schedule_passes(&[fill.clone(), refine.clone()], Arc::clone(&renderer) as _);
    assert_eq!(first, 1, "the first request is generation 1");

    assert!(
        wait_until(30_000, || {
            let stats = scheduler.stats();
            stats.completed == 18 && idle(&scheduler)
        }),
        "the pass set settles: {:?}",
        scheduler.stats()
    );

    // An identical pass set is the same request: same generation, and
    // the cache dedup means nothing new is queued or rendered.
    let repeated =
        scheduler.schedule_passes(&[fill.clone(), refine.clone()], Arc::clone(&renderer) as _);
    assert_eq!(
        repeated, first,
        "an unchanged pass set keeps its generation"
    );
    assert!(
        wait_until(1_000, || idle(&scheduler)),
        "nothing new was queued"
    );
    assert_eq!(
        renderer.start_order().len(),
        18,
        "no tile rendered twice for an unchanged pass set"
    );

    // Dropping the refine pass changes the request: the generation bumps
    // (and the fill tiles are still cached, so nothing re-renders).
    let changed = scheduler.schedule_passes(&[fill], Arc::clone(&renderer) as _);
    assert_eq!(
        changed,
        first + 1,
        "dropping the refine pass bumps the generation"
    );
    assert!(
        wait_until(1_000, || idle(&scheduler)),
        "the cached fill asks for no work"
    );
    assert_eq!(
        renderer.start_order().len(),
        18,
        "the changed pass set re-dedups against the cache"
    );
}
