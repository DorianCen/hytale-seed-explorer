//! T24 acceptance tests: the feature search engine — ring alignment,
//! streaming order, dedupe, radius/result caps, cancellation, filters —
//! plus the manual release benchmark against the real generator.
//!
//! The fakes implement two matching styles the production adapter really
//! has (see the worldgen `QUERY.md` contract): [`PointSource`] matches by
//! anchor-inside-rect (spawn-style), under which the engine's ring bound is
//! exact, so order and brute-force assertions are provable;
//! [`StraddlingSource`] matches by bounds-intersection (prefab-style), under
//! which one straddling feature comes back from several adjacent regions
//! and the engine must dedupe it.
//!
//! The benchmark at the bottom reuses the fixture-skip pattern from
//! `tests/worldgen_source.rs`: without the sibling generator checkout and
//! its extracted asset pack it prints a skip message and passes (CI has no
//! pack).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use map_core::error::SourceError;
use map_core::scheduler::cancel_token;
use map_core::search::feature::{
    FeatureSearch, ProximityFilter, SearchEvent, SearchFilters, SearchResult, SearchStats,
    run_feature_search, run_feature_search_blocking,
};
use map_core::source::FeatureSource;
use map_core::source::worldgen::{WorldgenSource, WorldgenSourceConfig};
use map_core::types::{
    Feature, FeatureFilter, FeatureKind, FeatureKindMask, FeatureMetadata, Rect, Rect3,
};

// ---------------------------------------------------------------------------
// Fixture location (the worldgen-backed benchmark)
// ---------------------------------------------------------------------------

/// The world-gen folder this repository's sibling checkout ships for tests,
/// or `None` (with a skip message) on machines without it — including
/// machines without the user-supplied, git-ignored asset pack the committed
/// world's prefab references require (CI has no pack).
fn world_dir() -> Option<std::path::PathBuf> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../hytale-server-rust/testdata/Default");
    if !dir.is_dir() {
        eprintln!(
            "skipping: the worldgen checkout is not present at {}",
            dir.display()
        );
        return None;
    }
    let assets = dir.join("../../Assets/extracted");
    if !assets.is_dir() {
        eprintln!(
            "skipping: the worldgen checkout has no asset pack at {} (the committed world \
             references prefabs from the user-supplied pack)",
            assets.display()
        );
        return None;
    }
    Some(dir)
}

// ---------------------------------------------------------------------------
// Fake sources
// ---------------------------------------------------------------------------

/// An anchor-matching fake (spawn-style): a feature is returned iff its
/// anchor position lies inside the query rect and it passes the filter.
///
/// Records every query rect and kind mask, and optionally sleeps per query
/// to simulate generator latency for the spawned-handle tests.
#[derive(Debug, Default)]
struct PointSource {
    features: Vec<Feature>,
    queries: Mutex<Vec<Rect>>,
    kind_masks: Mutex<Vec<FeatureKindMask>>,
    delay: Duration,
    /// Fires in [`Drop`] so a test can observe when the spawned worker
    /// releases the source (the no-thread-leak witness).
    drop_probe: Option<Arc<AtomicUsize>>,
}
impl PointSource {
    fn new(features: Vec<Feature>) -> Self {
        Self {
            features,
            queries: Mutex::new(Vec::new()),
            kind_masks: Mutex::new(Vec::new()),
            delay: Duration::ZERO,
            drop_probe: None,
        }
    }

    fn with_delay(features: Vec<Feature>, delay: Duration) -> Self {
        Self {
            features,
            delay,
            queries: Mutex::new(Vec::new()),
            kind_masks: Mutex::new(Vec::new()),
            drop_probe: None,
        }
    }

    fn query_count(&self) -> usize {
        self.queries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

impl Drop for PointSource {
    fn drop(&mut self) {
        if let Some(probe) = self.drop_probe.take() {
            probe.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl FeatureSource for PointSource {
    fn features_in(
        &self,
        rect: Rect,
        filter: &FeatureFilter,
    ) -> map_core::error::SourceResult<Vec<Feature>> {
        if !self.delay.is_zero() {
            thread::sleep(self.delay);
        }
        self.queries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(rect);
        self.kind_masks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(filter.kinds);
        Ok(self
            .features
            .iter()
            .filter(|feature| {
                rect.contains(feature.position.0, feature.position.2) && filter.matches(feature)
            })
            .cloned()
            .collect())
    }
}

/// Wraps a [`PointSource`] and flips `token` once the query count reaches
/// `after` — the mid-ring cancellation hook (the engine checks the token
/// between regions).
struct CancelOnCount {
    inner: Arc<PointSource>,
    after: usize,
    token: map_core::scheduler::CancelToken,
}

impl FeatureSource for CancelOnCount {
    fn features_in(
        &self,
        rect: Rect,
        filter: &FeatureFilter,
    ) -> map_core::error::SourceResult<Vec<Feature>> {
        let answer = self.inner.features_in(rect, filter);
        if self.inner.query_count() >= self.after {
            self.token.store(true, Ordering::Relaxed);
        }
        answer
    }
}

/// Wraps a [`PointSource`] and fails on the `on`-th query (1-based): the
/// source-error path must end the stream with [`SearchEvent::Error`] and no
/// panic.
struct FailOnCount {
    inner: Arc<PointSource>,
    on: usize,
}

impl FeatureSource for FailOnCount {
    fn features_in(
        &self,
        rect: Rect,
        filter: &FeatureFilter,
    ) -> map_core::error::SourceResult<Vec<Feature>> {
        let count = self.inner.query_count() + 1;
        self.inner.features_in(rect, filter)?;
        if count == self.on {
            return Err(SourceError::Generation(
                "injected query failure".to_string(),
            ));
        }
        Ok(self
            .inner
            .features
            .iter()
            .filter(|feature| {
                rect.contains(feature.position.0, feature.position.2) && filter.matches(feature)
            })
            .cloned()
            .collect())
    }
}

/// A bounds-matching fake (prefab-style): a feature is returned iff its
/// bounds intersect the query rect — the contract under which one
/// straddling feature is returned by several adjacent regions.
struct StraddlingSource {
    features: Vec<Feature>,
}

impl FeatureSource for StraddlingSource {
    fn features_in(
        &self,
        rect: Rect,
        filter: &FeatureFilter,
    ) -> map_core::error::SourceResult<Vec<Feature>> {
        Ok(self
            .features
            .iter()
            .filter(|feature| {
                feature.bounds.is_some_and(|bounds| {
                    rect.min_x <= bounds.max_x
                        && bounds.min_x <= rect.max_x
                        && rect.min_z <= bounds.max_z
                        && bounds.min_z <= rect.max_z
                }) && filter.matches(feature)
            })
            .cloned()
            .collect())
    }
}

/// A source that panics inside `features_in`: the engine's query boundary
/// must map the panic to an [`SearchEvent::Error`], not kill the worker
/// silently (the documented `catch_unwind` deviation).
struct PanickingSource;

impl FeatureSource for PanickingSource {
    fn features_in(
        &self,
        _rect: Rect,
        _filter: &FeatureFilter,
    ) -> map_core::error::SourceResult<Vec<Feature>> {
        panic!("injected panic at the query boundary");
    }
}

// ---------------------------------------------------------------------------
// Feature and event helpers
// ---------------------------------------------------------------------------

fn feature(kind: FeatureKind, name: &str, position: (i32, i32, i32)) -> Feature {
    Feature {
        kind,
        name: name.to_string(),
        category: "Test".to_string(),
        position,
        bounds: None,
        metadata: FeatureMetadata {
            variant: None,
            size: None,
            source: Some("test".to_string()),
        },
    }
}

fn feature_with_bounds(
    kind: FeatureKind,
    name: &str,
    position: (i32, i32, i32),
    bounds: Rect3,
) -> Feature {
    Feature {
        bounds: Some(bounds),
        ..feature(kind, name, position)
    }
}

/// Runs a search synchronously and collects every event.
fn collect(source: &dyn FeatureSource, search: &FeatureSearch) -> (Vec<SearchEvent>, SearchStats) {
    let mut events = Vec::new();
    let stats = run_feature_search_blocking(source, search, &cancel_token(), |event| {
        events.push(event);
        true
    });
    (events, stats)
}

/// Every result event, in arrival order.
fn results_of(events: &[SearchEvent]) -> Vec<&SearchResult> {
    events
        .iter()
        .filter_map(|event| match event {
            SearchEvent::Result(result) => Some(result),
            _ => None,
        })
        .collect()
}

/// The single `Done` of a completed search.
fn done_of(events: &[SearchEvent]) -> SearchStats {
    events
        .iter()
        .find_map(|event| match event {
            SearchEvent::Done(stats) => Some(*stats),
            _ => None,
        })
        .expect("a search that was not cancelled or failed sends exactly one Done")
}

/// A no-filters search around `center` within `radius`, unlimited results,
/// 512-block regions.
fn search_around(center: (f64, f64), radius: f64) -> FeatureSearch {
    FeatureSearch {
        center,
        max_radius: radius,
        ..FeatureSearch::default()
    }
}

/// The exact order the engine's pending comparator produces, computed by
/// hand over a known feature set: distance, then the tie-break
/// (kind bit, position, name). The property test reuses this mirror.
fn tie_key(feature: &Feature) -> (u16, (i32, i32, i32), &str) {
    (
        FeatureKindMask::from_kind(feature.kind).0,
        feature.position,
        feature.name.as_str(),
    )
}

fn brute_force_order(features: &[Feature], center: (f64, f64), radius: f64) -> Vec<Feature> {
    let mut candidates: Vec<(f64, Feature)> = features
        .iter()
        .filter_map(|feature| {
            let dx = center.0 - f64::from(feature.position.0);
            let dz = center.1 - f64::from(feature.position.2);
            let distance = dx.hypot(dz);
            (distance <= radius).then_some((distance, feature.clone()))
        })
        .collect();
    candidates.sort_by(|left, right| {
        left.0
            .total_cmp(&right.0)
            .then_with(|| tie_key(&left.1).cmp(&tie_key(&right.1)))
    });
    candidates.into_iter().map(|(_, feature)| feature).collect()
}

// ---------------------------------------------------------------------------
// Engine behavior
// ---------------------------------------------------------------------------

#[test]
fn feature_search_streams_known_features_in_exact_nearest_order() {
    let features = vec![
        feature(FeatureKind::Spawn, "spawn", (0, 64, 0)),
        feature(FeatureKind::UniquePrefab, "tower", (100, 70, 0)),
        feature(FeatureKind::Poi, "ruin", (300, 65, 200)),
        feature(FeatureKind::UniquePrefab, "keep", (-700, 80, -700)),
        feature(FeatureKind::Poi, "far-monument", (2000, 60, 0)),
    ];
    let source = PointSource::new(features);
    let (events, stats) = collect(&source, &search_around((0.0, 0.0), 2500.0));

    let results = results_of(&events);
    let expected = [
        ("spawn", 0.0),
        ("tower", 100.0),
        ("ruin", (300.0_f64 * 300.0 + 200.0 * 200.0).sqrt()),
        ("keep", (700.0_f64 * 700.0 + 700.0 * 700.0).sqrt()),
        ("far-monument", 2000.0),
    ];
    assert_eq!(
        results.len(),
        expected.len(),
        "every feature within the radius is emitted exactly once",
    );
    for (result, (name, distance)) in results.iter().zip(expected) {
        assert_eq!(result.feature.name, name, "results arrive nearest-first");
        assert!(
            (result.distance - distance).abs() < 1e-9,
            "{name} is {distance} blocks out, reported {}",
            result.distance,
        );
        assert_eq!(
            result.teleport,
            format!(
                "/tp {} {} {}",
                result.feature.position.0, result.feature.position.1, result.feature.position.2
            ),
            "the teleport string matches the anchor",
        );
    }
    let done = done_of(&events);
    assert_eq!(done.results, 5);
    assert!(!done.truncated, "an unlimited search over full radius");
    assert!(!done.cancelled);
    assert_eq!(stats.results, 5, "the blocking return agrees with Done");
}

#[test]
fn feature_search_first_result_precedes_the_full_scan() {
    // The near tower sits in ring 0, but it only becomes safe to emit after
    // ring 1 (the threshold opens to ring_min(2) = 513): exactly 9 region
    // queries. The far monument forces rings 2..4 to run, so the full scan
    // is 81 queries — the first result must arrive long before it.
    let features = vec![
        feature(FeatureKind::UniquePrefab, "tower", (100, 70, 0)),
        feature(FeatureKind::Poi, "far-monument", (2000, 60, 0)),
    ];
    let source = PointSource::new(features);
    let queries_at_first_result = Arc::new(AtomicUsize::new(0));
    let query_probe = Arc::clone(&queries_at_first_result);

    let mut saw_first = false;
    let search = search_around((0.0, 0.0), 2048.0);
    run_feature_search_blocking(&source, &search, &cancel_token(), |event| {
        if !saw_first && matches!(event, SearchEvent::Result(_)) {
            saw_first = true;
            queries_at_first_result.store(source.query_count(), Ordering::Relaxed);
        }
        true
    });

    assert!(saw_first, "the near tower must be emitted");
    let first = query_probe.load(Ordering::Relaxed);
    assert_eq!(first, 9, "emitted as soon as ring 1 made it safe");
    let total = source.query_count();
    assert_eq!(
        total, 81,
        "rings 0..4 of 512-block regions: 1 + 8 + 16 + 24 + 32",
    );
    assert!(
        first < total,
        "the first result precedes the full ring scan",
    );
}

#[test]
fn feature_search_matches_brute_force_on_a_small_area() {
    // Deterministic pseudo-random scatter (a tiny LCG — no rand dependency
    // for one test), mostly within a 64-block-region grid so dozens of
    // rings participate.
    let mut state = 0x0d24_1eed_u64;
    let mut next = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as i64
    };
    let center = (123.4, -567.8);
    let kinds = [
        FeatureKind::UniquePrefab,
        FeatureKind::BiomePrefab,
        FeatureKind::Spawn,
        FeatureKind::Poi,
        FeatureKind::CaveEntrance,
    ];
    let features: Vec<Feature> = (0..40)
        .map(|index| {
            let x = 123 + (next() % 4000) as i32 - 2000;
            let z = -568 + (next() % 4000) as i32 - 2000;
            feature(kinds[index % kinds.len()], &format!("f{index}"), (x, 40, z))
        })
        .collect();

    let search = FeatureSearch {
        region_size: 64,
        ..search_around(center, 1500.0)
    };
    let (events, _) = collect(&PointSource::new(features.clone()), &search);

    let expected = brute_force_order(&features, center, 1500.0);
    let results = results_of(&events);
    assert_eq!(
        results.len(),
        expected.len(),
        "the engine finds exactly the brute-force candidate set",
    );
    for (result, expected_feature) in results.iter().zip(&expected) {
        assert_eq!(
            result.feature.name, expected_feature.name,
            "the engine's order matches the brute-force order",
        );
    }
}

#[test]
fn feature_search_respects_radius_and_caps_results() {
    let features = vec![
        feature(FeatureKind::Poi, "near", (100, 60, 0)),
        feature(FeatureKind::Poi, "edge", (1400, 60, 0)),
        feature(FeatureKind::Poi, "outside", (1600, 60, 0)),
        feature(FeatureKind::Poi, "far-outside", (2500, 60, 0)),
    ];

    // Unlimited: everything within 1500 blocks, nothing farther.
    let (events, _) = collect(
        &PointSource::new(features.clone()),
        &search_around((0.0, 0.0), 1500.0),
    );
    let names: Vec<&str> = results_of(&events)
        .iter()
        .map(|result| result.feature.name.as_str())
        .collect();
    assert_eq!(names, vec!["near", "edge"]);
    assert!(!done_of(&events).truncated, "the scan exhausted the radius");

    // Capped at one: the nearest result only, marked truncated.
    let capped = FeatureSearch {
        max_results: 1,
        ..search_around((0.0, 0.0), 1500.0)
    };
    let (events, _) = collect(&PointSource::new(features), &capped);
    let results = results_of(&events);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].feature.name, "near");
    assert!(
        done_of(&events).truncated,
        "a capped search that stopped over unscanned area is truncated",
    );
}

#[test]
fn feature_search_dedupes_features_straddling_region_boundaries() {
    // Prefab-style matching: the straddler's bounds cross the 512 corner,
    // so four region queries each return it once.
    let straddler = feature_with_bounds(
        FeatureKind::UniquePrefab,
        "straddler",
        (510, 64, 0),
        Rect3::new(508, 60, -4, 515, 68, 3),
    );
    let lone = feature_with_bounds(
        FeatureKind::Poi,
        "lone",
        (-900, 60, -900),
        Rect3::new(-902, 60, -902, -898, 68, -898),
    );
    let (events, _) = collect(
        &StraddlingSource {
            features: vec![straddler, lone],
        },
        &search_around((0.0, 0.0), 2000.0),
    );

    let straddler_results: Vec<_> = results_of(&events)
        .into_iter()
        .filter(|result| result.feature.name == "straddler")
        .collect();
    assert_eq!(
        straddler_results.len(),
        1,
        "a feature returned by four adjacent regions is emitted once",
    );
    assert_eq!(done_of(&events).results, 2);
}

#[test]
fn feature_search_handles_negative_and_boundary_centers() {
    // Center in the crack between blocks -1 and 0: its region is
    // (-1, -1), and both diagonally adjacent regions are ring 1.
    let features = vec![
        feature(FeatureKind::Spawn, "crack-spawn", (-1, 64, -1)),
        feature(FeatureKind::Poi, "origin", (0, 64, 0)),
        feature(FeatureKind::Poi, "southwest", (-600, 64, -600)),
        feature(FeatureKind::UniquePrefab, "west", (-1500, 64, 0)),
    ];
    let (events, _) = collect(
        &PointSource::new(features),
        &search_around((-1.0, -1.0), 2000.0),
    );
    let names: Vec<&str> = results_of(&events)
        .iter()
        .map(|result| result.feature.name.as_str())
        .collect();
    assert_eq!(names, vec!["crack-spawn", "origin", "southwest", "west"]);

    // A center exactly on a region boundary belongs to the next region.
    let features = vec![
        feature(FeatureKind::Spawn, "boundary-spawn", (512, 64, 0)),
        feature(FeatureKind::Poi, "behind", (0, 64, 0)),
    ];
    let (events, _) = collect(
        &PointSource::new(features),
        &search_around((512.0, 0.0), 2000.0),
    );
    let names: Vec<&str> = results_of(&events)
        .iter()
        .map(|result| result.feature.name.as_str())
        .collect();
    assert_eq!(names, vec!["boundary-spawn", "behind"]);
}

#[test]
fn feature_search_cancellation_mid_ring_sends_cancelled_done_and_stops_querying() {
    let token = cancel_token();
    let inner = Arc::new(PointSource::new(vec![feature(
        FeatureKind::Poi,
        "unreached",
        (100, 60, 100),
    )]));
    let source = CancelOnCount {
        inner: Arc::clone(&inner),
        after: 3,
        token: Arc::clone(&token),
    };

    let (events, stats) =
        run_blocking_with_token(&source, &search_around((0.0, 0.0), 4000.0), &token);

    let done = done_of(&events);
    assert!(done.cancelled, "the Done reports the cancellation");
    assert_eq!(done.results, 0, "the buffer is discarded, not flushed");
    assert_eq!(done.rings_scanned, 1, "only ring 0 completed");
    assert!(
        results_of(&events).is_empty(),
        "the ring-0 feature was still waiting for its safe threshold",
    );
    assert!(stats.cancelled);
    // The worker stops at the first token check after the third query —
    // no further region is ever queried.
    assert_eq!(inner.query_count(), 3);
}

/// `run_feature_search_blocking` with a caller-supplied token (the
/// cancellation tests need the source to flip the same token).
fn run_blocking_with_token(
    source: &dyn FeatureSource,
    search: &FeatureSearch,
    token: &map_core::scheduler::CancelToken,
) -> (Vec<SearchEvent>, SearchStats) {
    let mut events = Vec::new();
    let stats = run_feature_search_blocking(source, search, token, |event| {
        events.push(event);
        true
    });
    (events, stats)
}

#[test]
fn feature_search_empty_source_finishes_with_zero_results() {
    // Radius 512: ring 2's closest point (513 blocks west) is already out,
    // so the scan is exactly rings 0 and 1.
    let (events, stats) = collect(
        &PointSource::new(Vec::new()),
        &search_around((0.0, 0.0), 512.0),
    );

    let done = done_of(&events);
    assert_eq!(done.results, 0);
    assert_eq!(done.regions_queried, 9, "rings 0 and 1 only");
    assert!(!done.truncated);
    assert!(!done.cancelled);
    assert_eq!(stats.results, 0);
    assert!(
        matches!(events.first(), Some(SearchEvent::Started)),
        "the stream opens with Started",
    );
}

#[test]
fn feature_search_source_error_ends_the_stream_without_done() {
    let inner = Arc::new(PointSource::new(Vec::new()));
    let source = FailOnCount { inner, on: 3 };

    let mut events = Vec::new();
    let stats = run_feature_search_blocking(
        &source,
        &search_around((0.0, 0.0), 4000.0),
        &cancel_token(),
        |event| {
            events.push(event);
            true
        },
    );

    assert!(
        matches!(events.last(), Some(SearchEvent::Error(_))),
        "an Error terminates the stream; no Done follows it",
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, SearchEvent::Done(_))),
        "Done and Error are mutually exclusive terminals",
    );
    assert_eq!(stats.regions_queried, 2, "the failing query is not counted");
    let progress = events
        .iter()
        .filter(|event| matches!(event, SearchEvent::Progress(_)))
        .count();
    assert_eq!(progress, 2);
}

#[test]
fn feature_search_panicking_source_maps_to_an_error_event() {
    let mut events = Vec::new();
    run_feature_search_blocking(
        &PanickingSource,
        &search_around((0.0, 0.0), 512.0),
        &cancel_token(),
        |event| {
            events.push(event);
            true
        },
    );
    match events.last() {
        Some(SearchEvent::Error(SourceError::Generation(message))) => {
            assert!(
                message.contains("injected panic"),
                "the panic payload survives the boundary: {message}",
            );
        }
        other => panic!("a panicking source must end with a Generation error, got {other:?}"),
    }
}

#[test]
fn feature_search_proximity_filter_drops_neighbors_in_ring_order() {
    // A (ring 0, distance 450) and B (ring 1, distance 906) sit within 500
    // blocks of each other: they must drop each other, and A — though
    // closer than C — must NOT be emitted before B is known. C has no
    // neighbor within 500 blocks and survives.
    let features = vec![
        feature(FeatureKind::UniquePrefab, "A", (450, 64, 0)),
        feature(FeatureKind::UniquePrefab, "B", (900, 64, 100)),
        feature(FeatureKind::UniquePrefab, "C", (0, 64, 700)),
    ];
    let search = FeatureSearch {
        filters: SearchFilters {
            proximity: Some(ProximityFilter {
                min_separation: 500.0,
                neighbor_kinds: FeatureKindMask::ALL,
            }),
            ..SearchFilters::default()
        },
        ..search_around((0.0, 0.0), 2000.0)
    };
    let (events, _) = collect(&PointSource::new(features), &search);

    let results = results_of(&events);
    assert_eq!(results.len(), 1, "A and B exclude each other; C is lone");
    assert_eq!(results[0].feature.name, "C");
    assert!(
        (results[0].distance - 700.0).abs() < 1e-9,
        "the surviving result carries its true distance",
    );
    assert!(!done_of(&events).truncated);
}

#[test]
fn feature_search_kind_mask_is_passed_to_the_source() {
    let features = vec![
        feature(FeatureKind::UniquePrefab, "hidden-unique", (10, 64, 10)),
        feature(FeatureKind::Spawn, "spawn", (20, 64, 20)),
    ];
    let expected_mask = FeatureKindMask::from_kind(FeatureKind::Spawn)
        .union(FeatureKindMask::from_kind(FeatureKind::Poi));
    let search = FeatureSearch {
        kinds: expected_mask,
        ..search_around((0.0, 0.0), 512.0)
    };
    let source = PointSource::new(features);
    let (events, _) = collect(&source, &search);

    let masks = source
        .kind_masks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    assert!(!masks.is_empty(), "the search queried its regions");
    for mask in &masks {
        assert_eq!(*mask, expected_mask, "every query carries the mask");
    }
    // The spawn passes the source-side kind filter; the unique prefab does
    // not, so the engine never even sees it.
    let results = results_of(&events);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].feature.name, "spawn");
}

#[test]
fn feature_search_progress_reports_every_region_and_ring() {
    let source = PointSource::new(Vec::new());
    // Radius 512: rings 0 and 1 only (9 regions).
    let (events, _) = collect(&source, &search_around((0.0, 0.0), 512.0));

    let progress: Vec<SearchProgressLike> = events
        .iter()
        .filter_map(|event| match event {
            SearchEvent::Progress(progress) => Some((*progress).into()),
            _ => None,
        })
        .collect();
    assert_eq!(progress.len() as u64, done_of(&events).regions_queried);
    assert_eq!(progress.first().map(|p| p.ring), Some(0));
    assert_eq!(progress.last().map(|p| p.ring), Some(1));
    for pair in progress.windows(2) {
        assert!(
            pair[0].regions_queried < pair[1].regions_queried,
            "regions_queried is monotonic across the stream",
        );
    }
}

/// A local shape for progress assertions (keeps the helper list simple).
#[derive(Debug, Clone, Copy)]
struct SearchProgressLike {
    ring: u32,
    regions_queried: u64,
}

impl From<map_core::search::feature::SearchProgress> for SearchProgressLike {
    fn from(progress: map_core::search::feature::SearchProgress) -> Self {
        Self {
            ring: progress.ring,
            regions_queried: progress.regions_queried,
        }
    }
}

// ---------------------------------------------------------------------------
// The spawned handle
// ---------------------------------------------------------------------------

#[test]
fn feature_search_handle_cancel_stops_the_spawned_worker() {
    // A slow source makes the window between events wide enough to cancel
    // inside it; the worker must deliver its cancelled Done promptly.
    let source: Arc<dyn FeatureSource> = Arc::new(PointSource::with_delay(
        Vec::new(),
        Duration::from_millis(2),
    ));
    let handle = run_feature_search(
        source,
        FeatureSearch {
            max_radius: 100_000.0,
            ..FeatureSearch::default()
        },
    );

    let mut done = None;
    let deadline = Instant::now() + Duration::from_secs(5);
    while done.is_none() {
        let Ok(event) = handle
            .events()
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        else {
            panic!("the worker did not report within its deadline");
        };
        match event {
            SearchEvent::Started => handle.cancel(),
            SearchEvent::Done(stats) => done = Some(stats),
            _ => {}
        }
    }

    let done = done.expect("the loop only exits with a Done");
    assert!(done.cancelled, "the spawned Done reports the cancellation");
}

#[test]
fn feature_search_handle_drop_terminates_the_worker() {
    // Dropping the handle cancels and closes the channel: the worker's next
    // send fails and the thread exits, dropping its source. The drop probe
    // on the source is the no-thread-leak witness.
    let dropped = Arc::new(AtomicUsize::new(0));
    let mut source = PointSource::with_delay(Vec::new(), Duration::from_millis(1));
    source.drop_probe = Some(Arc::clone(&dropped));

    let handle = run_feature_search(
        Arc::new(source),
        FeatureSearch {
            max_radius: 100_000.0,
            ..FeatureSearch::default()
        },
    );
    // Give the worker a moment to enter its first query, then vanish.
    thread::sleep(Duration::from_millis(10));
    drop(handle);

    let deadline = Instant::now() + Duration::from_secs(5);
    while dropped.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        dropped.load(Ordering::Relaxed),
        1,
        "the worker dropped its source — the thread terminated",
    );
}

// ---------------------------------------------------------------------------
// Manual benchmark (release only, real generator)
// ---------------------------------------------------------------------------

/// The T24 benchmark, run manually in release (the generator's query cost
/// makes a debug run useless — see `docs/benchmarks.md`):
///
/// ```sh
/// cargo test -p map-core --release --test feature_search -- \
///     --ignored feature_search_bench --nocapture
/// ```
///
/// It measures the plan's search target — first results within ~500 ms —
/// for the standard 10,000-block radius on the fixture seed and the T22
/// probe seed, plus one 50,000-block sweep for a denser result stream, and
/// prints a Markdown table ready for `docs/benchmarks.md`.
///
/// Kinds are the cheap tier (spawn / unique prefab / POI): the kind mask
/// skips the generator's biome placement pass, which is what makes ring
/// searches viable at all. A biome-prefab search pays the T22 rect costs
/// (~seconds per 512² ring region) — a batch job behind the T33 memory
/// lever, not a UI-path benchmark.
#[test]
#[ignore = "the T24 feature-search benchmark — run manually in release (docs/benchmarks.md)"]
fn feature_search_bench_10k_radius() {
    let Some(world) = world_dir() else {
        return; // skip message already printed by the helper
    };

    eprintln!("| Seed | Radius | First result | 100th result | Results | Regions | Total |");
    eprintln!("|---|---|---|---|---|---|---|");
    // Two 10,000-radius runs (the plan's search target) on the fixture seed
    // and the T22 probe seed, plus one 50,000-radius sweep for a denser
    // result stream — the cheap tier is sparse at 10k on both seeds.
    for (seed_text, radius) in [("12345", 10_000.0_f64), ("0", 10_000.0), ("0", 50_000.0)] {
        let config = WorldgenSourceConfig::new(&world, seed_text);
        let source: Arc<dyn FeatureSource> = match WorldgenSource::new(&config) {
            Ok(source) => Arc::new(source),
            Err(error) => {
                eprintln!("skipping seed {seed_text}: the source did not load: {error}");
                return;
            }
        };
        // Loading the source parses the world's asset JSON (~1 s); the
        // search clock starts with the worker's first event, so load time
        // stays out of the measurements.
        let search = FeatureSearch {
            kinds: FeatureKindMask::from_kind(FeatureKind::Spawn)
                .union(FeatureKindMask::from_kind(FeatureKind::UniquePrefab))
                .union(FeatureKindMask::from_kind(FeatureKind::Poi)),
            center: (0.0, 0.0),
            max_radius: radius,
            max_results: 0,
            region_size: map_core::search::feature::DEFAULT_REGION_SIZE,
            filters: SearchFilters::default(),
        };
        let handle = run_feature_search(Arc::clone(&source), search);
        let started = Instant::now();
        let mut first: Option<Duration> = None;
        let mut hundredth: Option<Duration> = None;
        let mut count = 0usize;
        let stats = loop {
            let event = handle
                .events()
                .recv_timeout(Duration::from_secs(600))
                .expect("the bench must keep producing events");
            match event {
                SearchEvent::Result(_) => {
                    count += 1;
                    if first.is_none() {
                        first = Some(started.elapsed());
                    }
                    if count == 100 {
                        hundredth = Some(started.elapsed());
                    }
                }
                SearchEvent::Done(stats) => break stats,
                _ => {}
            }
        };
        drop(source);

        eprintln!(
            "| {} | {:?} | {} | {} | {} | {} | {:.1?} |",
            seed_text,
            radius as u64,
            first.map_or("n/a".to_string(), |duration| format!("{duration:.1?}")),
            hundredth.map_or("n/a".to_string(), |duration| format!("{duration:.1?}")),
            stats.results,
            stats.regions_queried,
            stats.elapsed,
        );
    }
}
