//! Per-band streaming tile publication: the scheduler-side plumbing.
//!
//! A streaming [`TileRenderer`] publishes full-size partial tiles through
//! the trait's `render_streaming` hook; the worker is expected to cache
//! every publication and report it through the same `Ready` event path as
//! a finished tile, to keep exactly one job per key in flight, and to stop
//! publishing when the token fires. These tests use a fake renderer that
//! blocks between bands, so the cache/event state can be observed
//! mid-stream instead of racing the pipeline.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use map_core::coords::{TileCoord, TileRect, ZoomLevel};
use map_core::error::{SourceError, SourceResult};
use map_core::render::Layer;
use map_core::scheduler::{
    CancelToken, TileEvent, TilePass, TileRenderer, TileScheduler, TileSubject,
};
use map_core::source::SummaryFidelity;
use map_core::tiles::{Tile, TileCache, TileKey, covering_placeholder, pooled_placeholder};

/// The band byte a published tile carries in every pixel: `band + 1`, so a
/// publication's position in the stream is visible in the cached pixels.
/// Band publications are partial (`complete == false`); the final tile is
/// complete.
fn band_tile(key: &TileKey, band: u8, fidelity: SummaryFidelity, complete: bool) -> Tile {
    Tile::new(*key, 2, 2, vec![band.wrapping_add(1); 16])
        .expect("the buffer matches 2x2 RGBA")
        .with_fidelity(fidelity)
        .with_complete(complete)
}

/// A renderer that streams `bands` progress tiles and blocks after each one
/// until the test releases it. `published` signals (after `progress`
/// returned, i.e. after the worker cached and reported that band) and
/// `release` unblocks the next band.
struct StreamingRenderer {
    bands: u8,
    published: Sender<u8>,
    release: Receiver<()>,
    /// Every `render_streaming` entry, for the dedup test.
    renders: Arc<Mutex<Vec<TileKey>>>,
}

impl TileRenderer for StreamingRenderer {
    fn render(
        &self,
        key: &TileKey,
        _cancel: &CancelToken,
        fidelity: SummaryFidelity,
    ) -> SourceResult<Tile> {
        Ok(band_tile(key, self.bands, fidelity, true))
    }

    fn render_streaming(
        &self,
        key: &TileKey,
        cancel: &CancelToken,
        fidelity: SummaryFidelity,
        progress: &mut dyn FnMut(Tile),
    ) -> SourceResult<Tile> {
        self.renders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(*key);
        for band in 0..self.bands {
            progress(band_tile(key, band, fidelity, false));
            let _ = self.published.send(band);
            // The test inspects the pipeline between bands; a stuck test
            // must not wedge a worker forever.
            if self.release.recv_timeout(Duration::from_secs(10)).is_err() {
                return Err(SourceError::Cancelled);
            }
            // The production renderers check the token between bands; this
            // fake does the same so the cancellation test exercises the
            // scheduler's publish/stop contract.
            if cancel.load(Relaxed) {
                return Err(SourceError::Cancelled);
            }
        }
        Ok(band_tile(key, self.bands, fidelity, true))
    }
}

/// A biome-layer, zoom-0 subject under a synthetic identity.
fn subject() -> TileSubject {
    TileSubject::new(1, 7, Layer::Biome, 0, ZoomLevel::MIN)
}

/// One scheduler job for `key`'s single-tile view.
fn schedule_one(
    scheduler: &TileScheduler,
    renderer: Arc<dyn TileRenderer>,
    subject: TileSubject,
) -> u64 {
    scheduler.schedule_passes(
        &[TilePass::fill(
            vec![subject],
            TileRect::new(TileCoord::new(0, 0), TileCoord::new(0, 0)),
            TileCoord::new(0, 0),
        )],
        renderer,
    )
}

/// Drains the event stream.
fn drain(events: &crossbeam_channel::Receiver<TileEvent>) -> Vec<TileEvent> {
    events.try_iter().collect()
}

/// Waits until `condition` holds or the deadline passes.
fn wait_until(duration_ms: u64, condition: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_millis(duration_ms);
    loop {
        if condition() || Instant::now() >= deadline {
            return condition();
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Every `Ready` tile in `events`, in arrival order.
fn ready_tiles(events: &[TileEvent]) -> Vec<Arc<Tile>> {
    events
        .iter()
        .filter_map(|event| match event {
            TileEvent::Ready { tile, .. } => Some(Arc::clone(tile)),
            _ => None,
        })
        .collect()
}

/// Each published band reaches the cache and the app through the same
/// `Ready` path as a finished tile, in order, and the finished tile still
/// lands last.
#[test]
fn streaming_progress_inserts_each_band_and_notifies() {
    let cache = Arc::new(TileCache::new(1024 * 1024));
    let scheduler = TileScheduler::new(1, Arc::clone(&cache));
    let events = scheduler.events();
    let (published_tx, published_rx) = unbounded();
    let (release_tx, release_rx) = unbounded();
    let renders = Arc::new(Mutex::new(Vec::new()));
    let renderer = Arc::new(StreamingRenderer {
        bands: 3,
        published: published_tx,
        release: release_rx,
        renders: Arc::clone(&renders),
    });
    let subject = subject();
    let key = subject.key_at(0, 0);

    schedule_one(
        &scheduler,
        Arc::clone(&renderer) as Arc<dyn TileRenderer>,
        subject,
    );

    let mut notified = Vec::new();
    let mut collected = Vec::new();
    for band in 0..3u8 {
        assert_eq!(
            published_rx.recv_timeout(Duration::from_secs(10)),
            Ok(band),
            "the renderer publishes one band at a time"
        );

        // `Ready` means "already cached": the band's pixels are visible in
        // the cache the moment the notification arrives.
        let cached = cache.get(&key).expect("the published band is cached");
        assert!(!cached.complete, "a band publication is an incomplete tile");
        assert_eq!(
            cached.pixel(0, 0),
            Some([
                band.wrapping_add(1),
                band.wrapping_add(1),
                band.wrapping_add(1),
                band.wrapping_add(1)
            ]),
            "band {band} is the cached tile while the stream is in flight",
        );
        let mut now_events = drain(&events);
        let now = ready_tiles(&now_events);
        assert_eq!(now.len(), 1, "exactly one Ready per published band");
        assert_eq!(
            now[0].pixel(0, 0),
            cached.pixel(0, 0),
            "the notification carries the band's tile"
        );
        assert_eq!(now[0].fidelity, SummaryFidelity::Full);
        notified.push(now[0].clone());
        collected.append(&mut now_events);

        release_tx
            .send(())
            .expect("the renderer is waiting for a release");
    }

    assert!(
        wait_until(10_000, || {
            let stats = scheduler.stats();
            stats.completed == 1 && stats.queued == 0 && stats.in_flight == 0
        }),
        "the stream completes after the last band: {:?}",
        scheduler.stats()
    );

    let mut final_events = drain(&events);
    let final_tiles = ready_tiles(&final_events);
    assert_eq!(
        final_tiles.len(),
        1,
        "the finished tile is one more Ready (no band was duplicated)"
    );
    assert_eq!(
        final_tiles[0].pixel(0, 0),
        Some([4, 4, 4, 4]),
        "the finished tile lands after the bands"
    );
    assert!(
        final_tiles[0].complete,
        "the finished tile is complete; only bands are partial"
    );
    collected.append(&mut final_events);
    assert_eq!(
        collected
            .iter()
            .filter(|event| matches!(event, TileEvent::Started(_)))
            .count(),
        1,
        "one Started for the whole streamed job"
    );
    // The four publications are distinct partials in order: bands 1..=3
    // then the complete tile. (The pipeline never re-rendered the key.)
    let sequence: Vec<u8> = notified
        .iter()
        .chain(final_tiles.iter())
        .map(|tile| tile.pixel(0, 0).expect("2x2 tile")[0])
        .collect();
    assert_eq!(sequence, vec![1, 2, 3, 4]);
    assert_eq!(renders.lock().expect("not poisoned").len(), 1);
}

/// The trait's default is the no-streaming path: it never calls `progress`
/// and returns exactly what `render` returns.
#[test]
fn streaming_default_method_equals_plain_render() {
    struct PlainRenderer;

    impl TileRenderer for PlainRenderer {
        fn render(
            &self,
            key: &TileKey,
            _cancel: &CancelToken,
            fidelity: SummaryFidelity,
        ) -> SourceResult<Tile> {
            Ok(band_tile(key, 9, fidelity, true))
        }
    }

    let renderer = PlainRenderer;
    let subject = subject();
    let key = subject.key_at(0, 0);
    let cancel = map_core::scheduler::cancel_token();
    let mut progress_calls = 0;
    let streamed = TileRenderer::render_streaming(
        &renderer,
        &key,
        &cancel,
        SummaryFidelity::SurfaceOnly,
        &mut |_| progress_calls += 1,
    )
    .expect("the default render cannot fail here");
    let plain = TileRenderer::render(&renderer, &key, &cancel, SummaryFidelity::SurfaceOnly)
        .expect("the plain render cannot fail here");

    assert_eq!(progress_calls, 0, "the default streams nothing");
    assert_eq!(streamed.rgba, plain.rgba);
    assert_eq!(streamed.key, plain.key);
    assert_eq!(streamed.fidelity, plain.fidelity);
}

/// Cancelling mid-stream stops further publication, reports `Cancelled`,
/// and leaves the already-published band cached as valid data.
#[test]
fn streaming_cancelled_job_keeps_published_bands_and_reports_cancelled() {
    let cache = Arc::new(TileCache::new(1024 * 1024));
    let scheduler = TileScheduler::new(1, Arc::clone(&cache));
    let events = scheduler.events();
    let (published_tx, published_rx) = unbounded();
    let (release_tx, release_rx) = unbounded();
    let renderer = Arc::new(StreamingRenderer {
        bands: 3,
        published: published_tx,
        release: release_rx,
        renders: Arc::new(Mutex::new(Vec::new())),
    });
    let subject = subject();
    let key = subject.key_at(0, 0);

    schedule_one(
        &scheduler,
        Arc::clone(&renderer) as Arc<dyn TileRenderer>,
        subject,
    );
    assert_eq!(
        published_rx.recv_timeout(Duration::from_secs(10)),
        Ok(0),
        "the first band is published before the cancel"
    );
    let published = cache.get(&key).expect("the first band is cached");
    assert_eq!(published.pixel(0, 0), Some([1, 1, 1, 1]));

    // The view moves away: the token fires, and the renderer stops at its
    // next between-bands check.
    scheduler.cancel_outside(&TileRect::new(
        TileCoord::new(1_000, 1_000),
        TileCoord::new(1_000, 1_000),
    ));
    release_tx
        .send(())
        .expect("the renderer is waiting for a release");

    assert!(
        wait_until(10_000, || {
            let stats = scheduler.stats();
            stats.cancelled == 1 && stats.queued == 0 && stats.in_flight == 0
        }),
        "the cancelled stream settles: {:?}",
        scheduler.stats()
    );
    let stats = scheduler.stats();
    assert_eq!(stats.completed, 0, "no finished tile was reported");
    assert_eq!(stats.failed, 0);

    // The published band remains cached (valid data), but the stream never
    // delivered bands 2 and 3.
    let cached = cache.get(&key).expect("the published band stays cached");
    assert_eq!(cached.pixel(0, 0), Some([1, 1, 1, 1]));

    let collected = drain(&events);
    assert_eq!(
        ready_tiles(&collected).len(),
        1,
        "only the band published before the cancel was notified"
    );
    assert_eq!(
        collected
            .iter()
            .filter(
                |event| matches!(event, TileEvent::Cancelled(key) if *key == subject.key_at(0, 0))
            )
            .count(),
        1,
        "the job reports Cancelled exactly once"
    );
}

/// The job map keeps one job per key: scheduling the same unchanged request
/// while a stream is in flight queues nothing, so a key never has two
/// concurrent renders (and thus two interleaved band streams).
#[test]
fn streaming_dedups_to_one_in_flight_job_per_key() {
    let cache = Arc::new(TileCache::new(1024 * 1024));
    let scheduler = TileScheduler::new(2, Arc::clone(&cache));
    let events = scheduler.events();
    let (published_tx, published_rx) = unbounded();
    let (release_tx, release_rx) = unbounded();
    let renders = Arc::new(Mutex::new(Vec::new()));
    let renderer = Arc::new(StreamingRenderer {
        bands: 2,
        published: published_tx,
        release: release_rx,
        renders: Arc::clone(&renders),
    });
    let subject = subject();
    let key = subject.key_at(0, 0);

    schedule_one(
        &scheduler,
        Arc::clone(&renderer) as Arc<dyn TileRenderer>,
        subject,
    );
    assert_eq!(
        published_rx.recv_timeout(Duration::from_secs(10)),
        Ok(0),
        "the stream is in flight and blocked on the first band"
    );

    // Three more identical requests while the stream is mid-flight.
    for _ in 0..3 {
        schedule_one(
            &scheduler,
            Arc::clone(&renderer) as Arc<dyn TileRenderer>,
            subject,
        );
    }
    assert_eq!(
        renders.lock().expect("not poisoned").len(),
        1,
        "the in-flight key is not queued again"
    );
    assert_eq!(
        drain(&events)
            .iter()
            .filter(|event| matches!(event, TileEvent::Started(_)))
            .count(),
        1,
        "exactly one worker picked the key up"
    );

    // Let the stream finish: the second band, then the final tile.
    release_tx.send(()).expect("release the first band");
    assert_eq!(
        published_rx.recv_timeout(Duration::from_secs(10)),
        Ok(1),
        "the stream continues after the redundant schedules"
    );
    release_tx.send(()).expect("release the second band");
    assert!(
        wait_until(10_000, || scheduler.stats().completed == 1),
        "the single streamed job completes: {:?}",
        scheduler.stats()
    );
    assert_eq!(
        renders.lock().expect("not poisoned").len(),
        1,
        "no redundant job ever rendered the key"
    );
    assert!(
        cache.get(&key).is_some(),
        "the finished tile is cached under the key"
    );
}

/// The gap the completeness marker closes: a cancelled stream's partial is
/// cached but does **not** satisfy the scheduler's short-circuit, so the
/// same pass re-queues it and the re-render ends with a complete tile.
#[test]
fn streaming_cancelled_partial_does_not_satisfy_and_the_pass_rerenders() {
    let cache = Arc::new(TileCache::new(1024 * 1024));
    let scheduler = TileScheduler::new(1, Arc::clone(&cache));
    let (published_tx, published_rx) = unbounded();
    let (release_tx, release_rx) = unbounded();
    let renders = Arc::new(Mutex::new(Vec::new()));
    let renderer = Arc::new(StreamingRenderer {
        bands: 2,
        published: published_tx,
        release: release_rx,
        renders: Arc::clone(&renders),
    });
    let subject = subject();
    let key = subject.key_at(0, 0);

    // First attempt: one band, then the view moves away mid-stream.
    schedule_one(
        &scheduler,
        Arc::clone(&renderer) as Arc<dyn TileRenderer>,
        subject,
    );
    assert_eq!(
        published_rx.recv_timeout(Duration::from_secs(10)),
        Ok(0),
        "the first attempt streams one band"
    );
    let partial = cache.get(&key).expect("the band is cached");
    assert!(!partial.complete, "a streamed band is an incomplete tile");
    assert!(
        !cache.contains_serving(&key, SummaryFidelity::Full),
        "an incomplete tile never satisfies the pass short-circuit"
    );
    scheduler.cancel_outside(&TileRect::new(
        TileCoord::new(1_000, 1_000),
        TileCoord::new(1_000, 1_000),
    ));
    release_tx.send(()).expect("release the band");
    assert!(
        wait_until(10_000, || {
            let stats = scheduler.stats();
            stats.cancelled == 1 && stats.queued == 0 && stats.in_flight == 0
        }),
        "the cancelled attempt settles: {:?}",
        scheduler.stats()
    );
    assert!(!cache.get(&key).expect("partial stays cached").complete);

    // Second attempt: the identical pass. The partial is a cache miss for
    // the scheduler, so the tile is queued and rendered again from scratch,
    // ending complete.
    schedule_one(
        &scheduler,
        Arc::clone(&renderer) as Arc<dyn TileRenderer>,
        subject,
    );
    assert_eq!(
        published_rx.recv_timeout(Duration::from_secs(10)),
        Ok(0),
        "the re-render streams from its first band"
    );
    release_tx
        .send(())
        .expect("release the re-render's first band");
    assert_eq!(
        published_rx.recv_timeout(Duration::from_secs(10)),
        Ok(1),
        "the re-render reaches its second band"
    );
    release_tx
        .send(())
        .expect("release the re-render's second band");
    assert!(
        wait_until(10_000, || {
            let stats = scheduler.stats();
            stats.completed == 1 && stats.in_flight == 0
        }),
        "the re-render completes: {:?}",
        scheduler.stats()
    );

    let finished = cache.get(&key).expect("the re-render is cached");
    assert!(finished.complete, "the finished tile is complete");
    assert_eq!(
        finished.pixel(0, 0),
        Some([3, 3, 3, 3]),
        "the finished tile's pixels overwrote the partial"
    );
    assert!(
        cache.contains_serving(&key, SummaryFidelity::Full),
        "the completed tile now satisfies the pass"
    );
    assert_eq!(
        renders.lock().expect("not poisoned").len(),
        2,
        "the partial was re-rendered, not served forever"
    );
}

/// A complete cached tile still satisfies the serving check exactly as
/// before: full fidelity serves every pass, surface-only serves only its
/// own, and an incomplete tile of either fidelity serves neither.
#[test]
fn streaming_complete_tile_still_satisfies_the_serving_check() {
    let cache = TileCache::new(1024 * 1024);
    let key = subject().key_at(0, 0);

    let complete = Tile::new(key, 2, 2, vec![7u8; 16]).expect("the buffer matches");
    assert!(complete.complete, "Tile::new defaults to complete");
    cache.insert(Arc::new(complete));
    assert!(cache.contains_serving(&key, SummaryFidelity::Full));
    assert!(cache.contains_serving(&key, SummaryFidelity::SurfaceOnly));

    let partial_full = Tile::new(key, 2, 2, vec![0u8; 16])
        .expect("the buffer matches")
        .with_complete(false);
    cache.insert(Arc::new(partial_full));
    assert!(!cache.contains_serving(&key, SummaryFidelity::Full));
    assert!(!cache.contains_serving(&key, SummaryFidelity::SurfaceOnly));

    let partial_weak = Tile::new(key, 2, 2, vec![1u8; 16])
        .expect("the buffer matches")
        .with_fidelity(SummaryFidelity::SurfaceOnly)
        .with_complete(false);
    cache.insert(Arc::new(partial_weak));
    assert!(!cache.contains_serving(&key, SummaryFidelity::SurfaceOnly));

    let complete_weak = Tile::new(key, 2, 2, vec![2u8; 16])
        .expect("the buffer matches")
        .with_fidelity(SummaryFidelity::SurfaceOnly);
    cache.insert(Arc::new(complete_weak));
    assert!(cache.contains_serving(&key, SummaryFidelity::SurfaceOnly));
    assert!(
        !cache.contains_serving(&key, SummaryFidelity::Full),
        "a complete surface-only tile still never serves a full pass"
    );
}

/// Stand-ins stay completeness-blind: a partial tile with alpha-0 rows is
/// still better than blank, in both zoom directions.
#[test]
fn streaming_placeholders_accept_partial_tiles() {
    // Zoom-in: a partial coarser parent covers a missing fine child.
    let cache = TileCache::new(1024 * 1024);
    let child_subject = TileSubject::new(1, 7, Layer::Biome, 0, ZoomLevel::MIN);
    let child = child_subject.key_at(1, 1);
    let parent =
        TileSubject::new(1, 7, Layer::Biome, 0, ZoomLevel::new(1).expect("level 1")).key_at(0, 0);
    let parent_tile = Tile::new(parent, 2, 2, vec![9u8; 16])
        .expect("the buffer matches")
        .with_complete(false);
    cache.insert(Arc::new(parent_tile));
    let covering = covering_placeholder(&cache, &child).expect("a partial parent stands in");
    assert_eq!(covering.tile.key, parent);
    assert!(
        !covering.tile.complete,
        "the stand-in is the partial itself"
    );
    assert_eq!(covering.uv, [0.5, 0.5, 1.0, 1.0]);

    // Zoom-out: partial finer children pool into a missing coarse tile.
    let coarse =
        TileSubject::new(1, 7, Layer::Biome, 0, ZoomLevel::new(1).expect("level 1")).key_at(0, 0);
    let fine_subject = TileSubject::new(1, 7, Layer::Biome, 0, ZoomLevel::MIN);
    let pooled_cache = TileCache::new(1024 * 1024);
    for (tx, ty) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
        let fine = fine_subject.key_at(tx, ty);
        let tile = Tile::new(fine, 2, 2, vec![5u8; 16])
            .expect("the buffer matches")
            .with_complete(false);
        pooled_cache.insert(Arc::new(tile));
    }
    let pooled = pooled_placeholder(&pooled_cache, &coarse).expect("partial children pool");
    assert_eq!(pooled.key, coarse);
}
