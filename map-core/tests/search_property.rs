//! T24 property tests: the feature search engine against brute force.
//!
//! The integration suite pins the engine on hand-picked cases; this suite
//! hunts the scattering and boundary combinations those tables miss. The
//! acceptance property: the stream equals a global sort of every in-radius
//! anchor by (distance, kind bit, position, name) — no candidate dropped,
//! duplicated, reordered or reported beyond the radius.
//!
//! The fake is anchor-matching (`PointSource`), so the ring bound is exact
//! (see the engine module docs). Anchors scatter across ±1.5 × radius, and
//! small regions (8..=64 blocks) keep dozens of rings in play per case.

use map_core::scheduler::cancel_token;
use map_core::search::feature::{
    FeatureSearch, SearchEvent, SearchFilters, SearchResult, SearchStats,
    run_feature_search_blocking,
};
use map_core::source::FeatureSource;
use map_core::types::{
    Feature, FeatureFilter, FeatureKind, FeatureKindMask, FeatureMetadata, Rect,
};
use proptest::prelude::*;

/// Distance tolerance. The engine and the mirror run the same `dx.hypot(dz)`
/// over the same inputs, so this is slack against a future refactor, not an
/// expected error.
const DISTANCE_EPSILON: f64 = 1e-9;

/// All five kinds, in the order the case strategy picks them.
const ALL_KINDS: [FeatureKind; 5] = [
    FeatureKind::UniquePrefab,
    FeatureKind::BiomePrefab,
    FeatureKind::CaveEntrance,
    FeatureKind::Spawn,
    FeatureKind::Poi,
];

// Helpers mirrored from tests/feature_search.rs (do not drift apart).

/// An anchor-matching fake (spawn-style): anchor-in-rect and filter-passing.
struct PointSource {
    features: Vec<Feature>,
}

impl PointSource {
    fn new(features: Vec<Feature>) -> Self {
        Self { features }
    }
}

impl FeatureSource for PointSource {
    fn features_in(
        &self,
        rect: Rect,
        filter: &FeatureFilter,
    ) -> map_core::error::SourceResult<Vec<Feature>> {
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

/// The engine's pending tie-break: kind bit, then position, then name.
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

/// One generated search: the request's geometry plus the anchor set.
#[derive(Debug)]
struct Case {
    center: (f64, f64),
    radius: f64,
    region_size: u32,
    features: Vec<Feature>,
}

/// A center around ±10,000 with a tenths fraction, so centers parked between
/// blocks are exercised alongside integral ones.
fn center_strategy() -> impl Strategy<Value = f64> {
    (-10_000i32..=10_000, 0u8..=9)
        .prop_map(|(whole, tenths)| f64::from(whole) + f64::from(tenths) * 0.1)
}

/// Per-feature `(kind index, east thousandths, south thousandths)`, 0..=60.
/// Offsets are thousandths of the radius, so ±1,500 is ±1.5 × radius.
fn feature_specs_strategy() -> impl Strategy<Value = Vec<(u8, i32, i32)>> {
    prop::collection::vec((0u8..5, -1_500i32..=1_500, -1_500i32..=1_500), 0..=60)
}

fn case_strategy() -> impl Strategy<Value = Case> {
    (
        center_strategy(),
        center_strategy(),
        50i32..=3_000,
        8u32..=64,
        feature_specs_strategy(),
    )
        .prop_map(|(center_x, center_z, radius, region_size, specs)| {
            let center = (center_x, center_z);
            let radius = f64::from(radius);
            let features = specs
                .into_iter()
                .enumerate()
                .map(|(index, (kind, east, south))| {
                    // Roughly a third of the anchors land outside the radius.
                    let x = (center.0 + radius * f64::from(east) / 1_000.0).round() as i32;
                    let z = (center.1 + radius * f64::from(south) / 1_000.0).round() as i32;
                    feature(
                        ALL_KINDS[usize::from(kind)],
                        &format!("f{index}"),
                        (x, 40, z),
                    )
                })
                .collect();
            Case {
                center,
                radius,
                region_size,
                features,
            }
        })
}

/// One run: the streamed results, its terminal stats and the independent
/// brute-force order over the same anchors.
struct Run {
    results: Vec<SearchResult>,
    done: SearchStats,
    expected: Vec<Feature>,
}

fn run(case: &Case, max_results: usize) -> Run {
    let source = PointSource::new(case.features.clone());
    let search = FeatureSearch {
        kinds: FeatureKindMask::ALL,
        center: case.center,
        max_radius: case.radius,
        max_results,
        region_size: case.region_size,
        filters: SearchFilters::default(),
    };
    let (events, _) = collect(&source, &search);
    Run {
        results: results_of(&events).into_iter().cloned().collect(),
        done: done_of(&events),
        expected: brute_force_order(&case.features, case.center, case.radius),
    }
}

fn names<'a>(names: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    names.collect()
}

fn anchor_distance(center: (f64, f64), position: (i32, i32, i32)) -> f64 {
    (center.0 - f64::from(position.0)).hypot(center.1 - f64::from(position.2))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Unlimited search: the streamed order is exactly the brute-force order
    /// of every in-radius anchor, each distance is the anchor distance and
    /// never exceeds the radius, and the scan reports nothing withheld.
    #[test]
    fn search_property_engine_matches_brute_force_unlimited(case in case_strategy()) {
        let run = run(&case, 0);

        prop_assert_eq!(
            names(run.results.iter().map(|result| result.feature.name.as_str())),
            names(run.expected.iter().map(|feature| feature.name.as_str())),
            "unlimited search around {:?} at radius {} (regions {}, {} features) \
             must stream the brute-force order",
            case.center,
            case.radius,
            case.region_size,
            case.features.len(),
        );
        for (result, expected) in run.results.iter().zip(&run.expected) {
            let distance = anchor_distance(case.center, expected.position);
            prop_assert!(
                (result.distance - distance).abs() < DISTANCE_EPSILON,
                "{} at {:?} is {distance} blocks out, reported {}",
                expected.name,
                expected.position,
                result.distance,
            );
            prop_assert!(
                result.distance <= case.radius,
                "{} at distance {} exceeds the radius {}",
                expected.name,
                result.distance,
                case.radius,
            );
        }
        prop_assert_eq!(
            run.done.results,
            run.expected.len(),
            "an unlimited scan reports every in-radius feature",
        );
        prop_assert!(!run.done.truncated, "an unlimited scan exhausts the radius");
        prop_assert!(!run.done.cancelled, "nothing cancelled this search");
    }

    /// Capped search: the stream is the brute-force top `cap`, in order; a
    /// candidate beyond the cap always makes the run report truncation, and a
    /// run that reports no truncation withheld nothing.
    #[test]
    fn search_property_engine_matches_brute_force_capped(
        case in case_strategy(),
        cap in 1usize..=30,
    ) {
        let run = run(&case, cap);
        let brute_count = run.expected.len();
        let emitted = brute_count.min(cap);

        prop_assert_eq!(
            names(run.results.iter().map(|result| result.feature.name.as_str())),
            names(
                run.expected[..emitted]
                    .iter()
                    .map(|feature| feature.name.as_str()),
            ),
            "capped search around {:?} at radius {} (regions {}, cap {}, \
             {} features) must stream the brute-force top {}",
            case.center,
            case.radius,
            case.region_size,
            cap,
            case.features.len(),
            cap,
        );
        prop_assert_eq!(
            run.done.results,
            emitted,
            "the cap emits min(cap, candidates) results",
        );
        // `truncated` is deliberately conservative: when the cap stops the
        // scan before the radius edge it is set even if the unqueried area is
        // empty, so `truncated == (brute_count > cap)` fails when the cap
        // lands exactly on the candidate count. These two directions are the
        // contract the engine can actually keep.
        prop_assert!(
            run.done.truncated || brute_count <= cap,
            "brute force found {} candidates beyond cap {}, but the capped \
             search did not report truncation",
            brute_count,
            cap,
        );
        prop_assert!(
            run.done.truncated || run.done.results == brute_count,
            "a non-truncated capped search withheld results: {} of {} \
             candidates emitted",
            run.done.results,
            brute_count,
        );
    }
}
