//! T15 acceptance: the tile scheduler end to end, with a fake renderer.
//!
//! Covers the task's test list: nearest-first order, dedup, cached
//! tiles skipped, view-exit cancellation, generation marking, panic
//! containment, clean shutdown, the 100-tile cancel timing row, and the
//! 30-view-change panning stress.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use map_core::coords::{TileCoord, TileRect, ZoomLevel};
use map_core::error::SourceError;
use map_core::render::Layer;
use map_core::scheduler::{
    CancelToken, TileEvent, TilePass, TileRenderer, TileScheduler, TileSubject,
};
use map_core::source::SummaryFidelity;
use map_core::tiles::{Tile, TileCache, TileKey};

/// One recorded fake render: when it started and ended, at what fidelity.
#[derive(Debug, Clone, Copy)]
struct RenderRecord {
    key: TileKey,
    fidelity: SummaryFidelity,
    start: Instant,
    end: Instant,
}

/// The fake renderer: configurable cost, optional panic keys, records
/// every render's interval and honors cancellation between sleep slices.
#[derive(Clone)]
struct FakeRenderer {
    /// Simulated render cost per tile.
    render_ms: u64,
    /// Keys whose render panics (panic-containment tests).
    panic_keys: HashSet<TileKey>,
    /// Every render, in completion order.
    records: Arc<Mutex<Vec<RenderRecord>>>,
}

impl FakeRenderer {
    fn new(render_ms: u64) -> Self {
        Self {
            render_ms,
            panic_keys: HashSet::new(),
            records: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_panics(render_ms: u64, panic_keys: &[TileKey]) -> Self {
        Self {
            render_ms,
            panic_keys: panic_keys.iter().copied().collect(),
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
    fn render(
        &self,
        key: &TileKey,
        cancel: &CancelToken,
        fidelity: SummaryFidelity,
    ) -> Result<Tile, SourceError> {
        let start = Instant::now();
        if self.panic_keys.contains(key) {
            panic!("fake renderer panic at {key:?}");
        }
        let deadline = start + Duration::from_millis(self.render_ms);
        while Instant::now() < deadline {
            if cancel.load(Relaxed) {
                return Err(SourceError::Cancelled);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let end = Instant::now();
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(RenderRecord {
                key: *key,
                fidelity,
                start,
                end,
            });
        let byte = key.tx.rem_euclid(i32::from(u8::MAX)) as u8;
        Ok(Tile::new(*key, 2, 2, vec![byte; 16])
            .expect("the buffer matches 2x2 RGBA")
            .with_fidelity(fidelity))
    }
}

/// A biome-layer, zoom-0 subject under a synthetic identity.
fn subject() -> TileSubject {
    TileSubject::new(1, 7, Layer::Biome, 0, ZoomLevel::MIN)
}

/// An `n × n` tile view centered on `(cx, cz)` (inclusive bounds).
fn view(cx: i64, cz: i64, n: i64) -> TileRect {
    let half = n / 2;
    TileRect::new(
        TileCoord::new(cx - half, cz - half),
        TileCoord::new(cx + half, cz + half),
    )
}

/// Collects every event currently waiting on the channel.
fn drain(events: &crossbeam_channel::Receiver<TileEvent>) -> Vec<TileEvent> {
    events.try_iter().collect()
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

#[test]
fn scheduler_renders_visible_tiles_nearest_center_first() {
    // A 5×5 grid around the origin; one worker so start order is the
    // queue's pop order.
    let cache = Arc::new(TileCache::new(1024 * 1024));
    let scheduler = TileScheduler::new(1, Arc::clone(&cache));
    let events = scheduler.events();
    let renderer = Arc::new(FakeRenderer::new(3));
    let visible = view(0, 0, 5);
    let subject = subject();

    let generation = scheduler.schedule(
        &[subject],
        visible,
        TileCoord::new(0, 0),
        Arc::clone(&renderer) as Arc<dyn TileRenderer>,
    );
    assert_eq!(generation, 1, "the first request is generation 1");

    assert!(
        wait_until(30_000, || {
            let stats = scheduler.stats();
            stats.completed == 25 && idle(&scheduler)
        }),
        "all 25 tiles complete eventually: {:?}",
        scheduler.stats()
    );

    // Start order is by Chebyshev distance to the center, never
    // decreasing; every visible tile rendered exactly once.
    let order = renderer.start_order();
    assert_eq!(order.len(), 25, "each tile rendered exactly once");
    let distances: Vec<u64> = order
        .iter()
        .map(|record| {
            let dx = i64::from(record.key.tx).wrapping_sub(0).unsigned_abs();
            let dz = i64::from(record.key.ty).wrapping_sub(0).unsigned_abs();
            dx.max(dz)
        })
        .collect();
    assert!(
        distances.windows(2).all(|pair| pair[0] <= pair[1]),
        "starts are nearest-first, got {distances:?}"
    );
    // No two renders for the same key overlapped (nothing renders twice
    // concurrently).
    for (index, record) in order.iter().enumerate() {
        assert!(
            order[index + 1..]
                .iter()
                .all(|other| other.key != record.key || other.start >= record.end),
            "two renders of {:?} overlapped",
            record.key
        );
    }

    // Every tile is cached, and the events say so.
    for tx in -2..=2 {
        for ty in -2..=2 {
            assert!(
                cache.get(&subject.key_at(tx, ty)).is_some(),
                "tile ({tx}, {ty}) is cached"
            );
        }
    }
    let collected = drain(&events);
    assert_eq!(
        collected
            .iter()
            .filter(|event| matches!(event, TileEvent::Started(_)))
            .count(),
        25
    );
    let ready = collected
        .iter()
        .filter_map(|event| match event {
            TileEvent::Ready { tile, generation } => Some((tile.key, *generation)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(ready.len(), 25);
    assert!(
        ready.iter().all(|(_, generation)| *generation == 1),
        "every tile belongs to generation 1"
    );
}

#[test]
fn scheduler_duplicate_schedule_calls_do_not_duplicate_jobs() {
    let cache = Arc::new(TileCache::new(1024 * 1024));
    let scheduler = TileScheduler::new(2, Arc::clone(&cache));
    let renderer = Arc::new(FakeRenderer::new(2));
    let visible = view(0, 0, 3);
    let subject = subject();

    // Three identical requests in a row: one generation, one job each.
    let generations: Vec<u64> = (0..3)
        .map(|_| {
            scheduler.schedule(
                &[subject],
                visible,
                TileCoord::new(0, 0),
                Arc::clone(&renderer) as Arc<dyn TileRenderer>,
            )
        })
        .collect();
    assert_eq!(
        generations,
        vec![1, 1, 1],
        "an unchanged view keeps its generation"
    );

    assert!(
        wait_until(30_000, || {
            let stats = scheduler.stats();
            stats.completed == 9 && idle(&scheduler)
        }),
        "the 3x3 view completes: {:?}",
        scheduler.stats()
    );

    // Re-scheduling after completion re-dedups against the cache.
    let after = scheduler.schedule(
        &[subject],
        visible,
        TileCoord::new(0, 0),
        Arc::clone(&renderer) as Arc<dyn TileRenderer>,
    );
    assert_eq!(after, 1, "still the same unchanged view");
    assert!(
        wait_until(1_000, || idle(&scheduler)),
        "nothing new was queued"
    );
    assert_eq!(renderer.start_order().len(), 9, "no tile rendered twice");
}

#[test]
fn scheduler_cached_tiles_are_not_requested_again() {
    let cache = Arc::new(TileCache::new(1024 * 1024));
    let scheduler = TileScheduler::new(2, Arc::clone(&cache));
    let renderer = Arc::new(FakeRenderer::new(2));
    let subject = subject();

    // Warm the cache for the center tile before scheduling.
    let center = subject.key_at(0, 0);
    let preexisting = Tile::new(center, 2, 2, vec![42u8; 16]).expect("the buffer matches");
    cache.insert(Arc::new(preexisting));

    scheduler.schedule(
        &[subject],
        view(0, 0, 3),
        TileCoord::new(0, 0),
        Arc::clone(&renderer) as Arc<dyn TileRenderer>,
    );
    assert!(
        wait_until(30_000, || {
            let stats = scheduler.stats();
            stats.completed == 8 && idle(&scheduler)
        }),
        "only the eight uncached tiles render: {:?}",
        scheduler.stats()
    );

    let order = renderer.start_order();
    assert!(
        order.iter().all(|record| record.key != center),
        "the cached center tile never reached a renderer"
    );
    // The cache still serves the pre-existing tile, not a replacement.
    let tile = cache.get(&center).expect("the center tile is cached");
    assert_eq!(
        tile.pixel(0, 0),
        Some([42, 42, 42, 42]),
        "the original pixels survive"
    );
}

/// T29: a cached surface-only placeholder must not short-circuit an
/// authoritative full pass. The pass re-renders and overwrites the entry
/// with the stronger provenance.
#[test]
fn fidelity_weaker_cached_tile_does_not_satisfy_a_full_pass() {
    let cache = Arc::new(TileCache::new(1024 * 1024));
    let scheduler = TileScheduler::new(1, Arc::clone(&cache));
    let renderer = Arc::new(FakeRenderer::new(0));
    let subject = subject();
    let center = subject.key_at(0, 0);

    // A placeholder-quality tile already sits in the cache.
    let weak = Tile::new(center, 2, 2, vec![42u8; 16])
        .expect("the buffer matches")
        .with_fidelity(SummaryFidelity::SurfaceOnly);
    cache.insert(Arc::new(weak));
    assert!(
        !cache.contains_serving(&center, SummaryFidelity::Full),
        "a surface-only tile is a miss for a full pass"
    );

    let pass = TilePass::fill(
        vec![subject],
        TileRect::new(TileCoord::new(0, 0), TileCoord::new(0, 0)),
        TileCoord::new(0, 0),
    )
    .with_fidelity(SummaryFidelity::Full);
    scheduler.schedule_passes(&[pass], Arc::clone(&renderer) as Arc<dyn TileRenderer>);
    assert!(
        wait_until(30_000, || {
            let stats = scheduler.stats();
            stats.completed == 1 && idle(&scheduler)
        }),
        "the weak cache hit re-renders: {:?}",
        scheduler.stats()
    );

    let records = renderer.start_order();
    assert_eq!(records.len(), 1, "exactly one re-render");
    assert_eq!(
        records[0].fidelity,
        SummaryFidelity::Full,
        "the re-render runs at the pass's full fidelity"
    );
    let tile = cache
        .get(&center)
        .expect("the re-render overwrote the entry");
    assert_eq!(
        tile.fidelity,
        SummaryFidelity::Full,
        "the entry now carries the stronger provenance"
    );
    assert_eq!(
        tile.pixel(0, 0),
        Some([0, 0, 0, 0]),
        "the placeholder pixels were replaced by the full render"
    );
    assert!(
        cache.contains_serving(&center, SummaryFidelity::Full),
        "the upgraded entry serves later full passes"
    );
}

/// T29: the converse — a full-fidelity cached tile is at least as good as
/// a surface-only pass, so it serves it without re-rendering.
#[test]
fn fidelity_full_cached_tile_satisfies_a_surface_only_pass() {
    let cache = Arc::new(TileCache::new(1024 * 1024));
    let scheduler = TileScheduler::new(1, Arc::clone(&cache));
    let renderer = Arc::new(FakeRenderer::new(0));
    let subject = subject();
    let center = subject.key_at(0, 0);

    let strong = Tile::new(center, 2, 2, vec![42u8; 16]).expect("the buffer matches");
    assert_eq!(
        strong.fidelity,
        SummaryFidelity::Full,
        "Tile::new defaults to full provenance"
    );
    cache.insert(Arc::new(strong));
    assert!(
        cache.contains_serving(&center, SummaryFidelity::SurfaceOnly),
        "full fidelity serves a surface-only pass"
    );

    let pass = TilePass::fill(
        vec![subject],
        TileRect::new(TileCoord::new(0, 0), TileCoord::new(0, 0)),
        TileCoord::new(0, 0),
    )
    .with_fidelity(SummaryFidelity::SurfaceOnly);
    scheduler.schedule_passes(&[pass], Arc::clone(&renderer) as Arc<dyn TileRenderer>);
    assert!(idle(&scheduler), "a serving cache hit queues nothing");

    // No renderer call can arrive later: the job was never queued.
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        renderer.start_order().is_empty(),
        "a full tile satisfies the surface-only pass without re-rendering"
    );
    let tile = cache.get(&center).expect("the original entry stays cached");
    assert_eq!(tile.fidelity, SummaryFidelity::Full);
    assert_eq!(
        tile.pixel(0, 0),
        Some([42, 42, 42, 42]),
        "the original pixels survive"
    );
}

#[test]
fn scheduler_cancel_outside_stops_in_flight_and_queued_tiles() {
    // Long renders so the cancellation lands mid-flight; two workers so
    // the nearest two tiles are in flight when the view shrinks.
    let cache = Arc::new(TileCache::new(1024 * 1024));
    let scheduler = TileScheduler::new(2, Arc::clone(&cache));
    let events = scheduler.events();
    let renderer = Arc::new(FakeRenderer::new(60));
    let subject = subject();

    scheduler.schedule(
        &[subject],
        view(0, 0, 5),
        TileCoord::new(0, 0),
        Arc::clone(&renderer) as Arc<dyn TileRenderer>,
    );
    std::thread::sleep(Duration::from_millis(30));

    // The view shrinks to just the center tile.
    let center_view = TileRect::new(TileCoord::new(0, 0), TileCoord::new(0, 0));
    scheduler.cancel_outside(&center_view);

    assert!(
        wait_until(30_000, || idle(&scheduler)),
        "the pipeline settles after cancellation: {:?}",
        scheduler.stats()
    );
    let stats = scheduler.stats();
    assert_eq!(stats.completed, 1, "only the center tile completes");
    assert_eq!(stats.cancelled, 24, "the 24 outside tiles were cancelled");
    assert_eq!(stats.failed, 0);

    // The center tile is cached; no outside tile produced a Ready event
    // (a completion racing the cancel is cached but still reported as
    // cancelled, so this holds regardless of timing).
    assert!(cache.get(&subject.key_at(0, 0)).is_some());
    let collected = drain(&events);
    assert_eq!(
        collected
            .iter()
            .filter(|event| matches!(event, TileEvent::Ready { .. }))
            .count(),
        1,
        "exactly one Ready event"
    );
    let ready_keys: HashSet<TileKey> = collected
        .iter()
        .filter_map(|event| match event {
            TileEvent::Ready { tile, .. } => Some(tile.key),
            _ => None,
        })
        .collect();
    assert_eq!(ready_keys, HashSet::from([subject.key_at(0, 0)]));
    assert_eq!(
        collected
            .iter()
            .filter(|event| matches!(event, TileEvent::Failed(..)))
            .count(),
        0
    );

    // The cancelled tiles are no longer pending; neither is the
    // completed center.
    assert!(!scheduler.is_pending(&subject.key_at(2, 2)));
    assert!(
        !scheduler.is_pending(&subject.key_at(0, 0)),
        "completed tiles leave the pipeline"
    );
}

#[test]
fn scheduler_cancel_not_serving_stops_a_disabled_layer_only() {
    // T19's toggle row: two layers over the same view, long renders so
    // the cancellation lands mid-flight. Dropping one layer from the
    // request cancels exactly that layer's tiles; the other layer keeps
    // serving, and re-requesting the disabled layer revives its
    // queued tile (the cache-dedup path never duplicated it).
    let cache = Arc::new(TileCache::new(1024 * 1024));
    let scheduler = TileScheduler::new(2, Arc::clone(&cache));
    let events = scheduler.events();
    let renderer = Arc::new(FakeRenderer::new(60));
    let biome = subject();
    let height = TileSubject::new(1, 7, Layer::Height, 0, ZoomLevel::MIN);

    scheduler.schedule(
        &[biome, height],
        view(0, 0, 3),
        TileCoord::new(0, 0),
        Arc::clone(&renderer) as Arc<dyn TileRenderer>,
    );
    std::thread::sleep(Duration::from_millis(30));

    // The height layer's checkbox unticks: only biome tiles are served.
    scheduler.cancel_not_serving(&[TilePass::fill(
        vec![biome],
        view(0, 0, 3),
        TileCoord::new(0, 0),
    )]);

    assert!(
        wait_until(30_000, || idle(&scheduler)),
        "the pipeline settles after the layer toggle: {:?}",
        scheduler.stats()
    );
    let stats = scheduler.stats();
    assert_eq!(
        stats.cancelled, 9,
        "exactly the height layer's nine tiles stopped"
    );
    assert_eq!(stats.failed, 0, "no failures, only cancellations");

    // Re-requesting both layers revives the height tiles (revive, not
    // duplicate) and every tile of both layers eventually completes.
    scheduler.schedule(
        &[biome, height],
        view(0, 0, 3),
        TileCoord::new(0, 0),
        Arc::clone(&renderer) as Arc<dyn TileRenderer>,
    );
    assert!(
        wait_until(30_000, || {
            let stats = scheduler.stats();
            stats.completed == 18 && idle(&scheduler)
        }),
        "both layers complete after the toggle-back: {:?}",
        scheduler.stats()
    );
    assert_eq!(
        renderer.start_order().len(),
        18,
        "no tile rendered twice across the toggle cycle"
    );
    // Every height tile landed in the cache with exactly one Ready.
    for tx in -1..=1 {
        for ty in -1..=1 {
            assert!(
                cache.contains(&height.key_at(tx, ty)),
                "height tile ({tx}, {ty}) is cached after the revive"
            );
        }
    }
    assert_eq!(
        drain(&events)
            .iter()
            .filter(|event| matches!(event, TileEvent::Ready { .. }))
            .count(),
        18
    );
}

#[test]
fn scheduler_stale_generation_events_are_marked_not_fresh() {
    // Two disjoint one-tile views: the tile of view A finishes under
    // generation 1 while the app has already moved to generation 2.
    let cache = Arc::new(TileCache::new(1024 * 1024));
    let scheduler = TileScheduler::new(1, Arc::clone(&cache));
    let events = scheduler.events();
    let renderer = Arc::new(FakeRenderer::new(5));
    let subject = subject();

    let gen_a = scheduler.schedule(
        &[subject],
        TileRect::new(TileCoord::new(0, 0), TileCoord::new(0, 0)),
        TileCoord::new(0, 0),
        Arc::clone(&renderer) as Arc<dyn TileRenderer>,
    );
    let gen_b = scheduler.schedule(
        &[subject],
        TileRect::new(TileCoord::new(10, 10), TileCoord::new(10, 10)),
        TileCoord::new(10, 10),
        Arc::clone(&renderer) as Arc<dyn TileRenderer>,
    );
    assert_eq!(gen_a, 1);
    assert_eq!(gen_b, 2, "the changed view bumps the generation");

    assert!(
        wait_until(30_000, || {
            let stats = scheduler.stats();
            stats.completed == 2 && idle(&scheduler)
        }),
        "both tiles complete: {:?}",
        scheduler.stats()
    );

    let generations: HashMap<TileKey, u64> = drain(&events)
        .into_iter()
        .filter_map(|event| match event {
            TileEvent::Ready { tile, generation } => Some((tile.key, generation)),
            _ => None,
        })
        .collect();
    assert_eq!(
        generations.get(&subject.key_at(0, 0)),
        Some(&1),
        "view A's event carries its own (stale) generation"
    );
    assert_eq!(
        generations.get(&subject.key_at(10, 10)),
        Some(&2),
        "view B's event carries the fresh generation"
    );
}

#[test]
fn scheduler_panic_is_contained_and_reported() {
    let cache = Arc::new(TileCache::new(1024 * 1024));
    let scheduler = TileScheduler::new(1, Arc::clone(&cache));
    let events = scheduler.events();
    let subject = subject();
    let panics = vec![subject.key_at(1, 1), subject.key_at(-1, -1)];
    let renderer = Arc::new(FakeRenderer::with_panics(2, &panics));

    scheduler.schedule(
        &[subject],
        view(0, 0, 5),
        TileCoord::new(0, 0),
        Arc::clone(&renderer) as Arc<dyn TileRenderer>,
    );
    assert!(
        wait_until(30_000, || {
            let stats = scheduler.stats();
            stats.completed == 23 && stats.failed == 2 && idle(&scheduler)
        }),
        "the panicking tiles fail and the rest complete: {:?}",
        scheduler.stats()
    );

    // Both panics became typed Failed events; the worker survived them
    // (23 subsequent tiles still rendered).
    let failures: HashMap<TileKey, SourceError> = drain(&events)
        .into_iter()
        .filter_map(|event| match event {
            TileEvent::Failed(key, error) => Some((key, error)),
            _ => None,
        })
        .collect();
    assert_eq!(failures.len(), 2);
    for key in &panics {
        assert!(
            matches!(failures.get(key), Some(SourceError::Generation(_))),
            "the panic at {key:?} is a contained Generation failure"
        );
        assert!(cache.get(key).is_none(), "a failed tile is not cached");
    }
}

#[test]
fn scheduler_shutdown_joins_all_workers_cleanly() {
    let cache = Arc::new(TileCache::new(1024 * 1024));
    let scheduler = TileScheduler::new(2, Arc::clone(&cache));
    let renderer = Arc::new(FakeRenderer::new(30));
    scheduler.schedule(
        &[subject()],
        view(0, 0, 3),
        TileCoord::new(0, 0),
        Arc::clone(&renderer) as Arc<dyn TileRenderer>,
    );
    // Drop mid-render: shutdown cancels the tokens, well-behaved
    // renderers abort at their next slice check, and Drop joins without
    // hanging.
    std::thread::sleep(Duration::from_millis(10));
    let started = Instant::now();
    drop(scheduler);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "shutdown joined in {:?}, not a hang",
        elapsed
    );
}

#[test]
fn scheduler_cancelling_one_hundred_tiles_mid_render_stops_early() {
    // The task's timing row: 100 fake 5 ms tiles, cancel at 20 ms —
    // fewer than 100 must complete, with no panics.
    let cache = Arc::new(TileCache::new(1024 * 1024));
    let scheduler = TileScheduler::new(4, Arc::clone(&cache));
    let renderer = Arc::new(FakeRenderer::new(5));

    scheduler.schedule(
        &[subject()],
        view(0, 0, 10),
        TileCoord::new(0, 0),
        Arc::clone(&renderer) as Arc<dyn TileRenderer>,
    );
    std::thread::sleep(Duration::from_millis(20));
    // A view far from every tile cancels all of them.
    let nowhere = TileRect::new(TileCoord::new(1_000, 1_000), TileCoord::new(1_000, 1_000));
    scheduler.cancel_outside(&nowhere);

    assert!(
        wait_until(30_000, || idle(&scheduler)),
        "the pipeline settles: {:?}",
        scheduler.stats()
    );
    let stats = scheduler.stats();
    assert!(
        stats.completed < 100,
        "cancellation stopped the run: {} completed",
        stats.completed
    );
    assert!(stats.completed >= 1, "at least the first slice finished");
    assert!(stats.cancelled > 0);
    assert_eq!(stats.failed, 0, "no panics, no failures");
}

#[test]
fn scheduler_thirty_view_changes_settle_on_the_final_view() {
    // The panning stress: 30 view changes, each cancelling what scrolled
    // away; the app keeps requesting (its frame loop) until the final
    // view is fully rendered.
    let cache = Arc::new(TileCache::new(1024 * 1024));
    let scheduler = TileScheduler::new(2, Arc::clone(&cache));
    let renderer = Arc::new(FakeRenderer::new(1));
    let subject = subject();

    for step in 0..30 {
        let visible = view(i64::from(step), 0, 3);
        scheduler.schedule(
            &[subject],
            visible,
            TileCoord::new(i64::from(step), 0),
            Arc::clone(&renderer) as Arc<dyn TileRenderer>,
        );
        scheduler.cancel_outside(&visible);
    }

    // The final 3×3 view, re-requested like a frame loop until every
    // tile is cached.
    let final_view = view(29, 0, 3);
    let all_cached = || {
        (0..=2).all(|dx| {
            (0..=2).all(|dz| {
                let (tx, ty) = (29 - 1 + dx, -1 + dz);
                cache.contains(&subject.key_at(tx, ty))
            })
        })
    };
    assert!(
        wait_until(30_000, || {
            scheduler.schedule(
                &[subject],
                final_view,
                TileCoord::new(29, 0),
                Arc::clone(&renderer) as Arc<dyn TileRenderer>,
            );
            all_cached()
        }),
        "the final view is eventually fully rendered: {:?}",
        scheduler.stats()
    );

    // Everything in the pipeline settled, and the last generation is 30
    // (one bump per view change).
    assert!(wait_until(10_000, || idle(&scheduler)));
    assert_eq!(scheduler.stats().generations, 30);
}
