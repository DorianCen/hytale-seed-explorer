//! Feature search: streaming nearest-first results over expanding rings of
//! regions (T24).
//!
//! Given a kind mask, a center and a maximum radius, the engine queries
//! [`FeatureSource::features_in`] over square rings of regions around the
//! center's region and streams [`SearchEvent`]s: results arrive nearest-first
//! while the scan is still expanding, [`SearchEvent::Progress`] ticks report
//! the rings, and a cancel token stops the worker promptly.
//!
//! Ordering guarantee: a result is emitted only once no *not-yet-queried*
//! region can hold a closer feature. After ring `r` is queried, the closest
//! point of ring `r + 1` is `ring_min_distance(r + 1)` blocks away, so pending
//! results closer than that are safe to emit; the rest wait in a buffer and
//! merge with later rings. The bound assumes a feature's anchor (the
//! `position` the source reports) lies within its bounds — true for every
//! kind the generator emits today (spawn anchors are the matched point,
//! unique-prefab bounds surround the entry position, biome-prefab anchors are
//! the footprint pivot inside the padded extent). If a future kind placed
//! anchors outside their bounds, results would still stream ring by ring but
//! a farther anchor could in principle be emitted before a closer one
//! arrives; the property tests use anchor-matching fake sources where the
//! bound is exact.
//!
//! The engine is deliberately headless: [`run_feature_search`] owns one
//! worker thread and an event channel, and
//! [`run_feature_search_blocking`] is the pure core beneath it, so tests and
//! future engines (T25 biome search) can drive a search synchronously.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, TrySendError, bounded};

use crate::error::{SourceError, panic_message};
use crate::scheduler::{CancelToken, cancel_token};
use crate::source::FeatureSource;
use crate::types::{Feature, FeatureFilter, FeatureKind, FeatureKindMask, Rect};

// The ring walk itself lives in the shared `rings` module (T25 extracted it
// so the biome search walks sampling cells with the same math).
use super::rings::{
    RingGeometry, cell_rect, distance_2d, ring_entirely_off_world, ring_min_distance, ring_regions,
};

/// The default ring region edge in blocks — one search tile.
///
/// Region size is a streaming-granularity lever, not a correctness one:
/// smaller regions emit sooner but cost one `features_in` call each, and the
/// generator's query cost is linear in rect area (see
/// `docs/benchmarks.md` §"Feature enumeration (T22)"), so the total work for
/// a full radius is roughly region-size independent.
pub const DEFAULT_REGION_SIZE: u32 = 512;

/// Capacity of the spawned search's event channel.
///
/// Backpressure policy (documented once, enforced by the emit closure in
/// [`run_feature_search`]): [`SearchEvent::Result`] and terminal events block
/// until the receiver catches up — a slow UI slows the search instead of
/// growing memory — while [`SearchEvent::Progress`] is dropped when the
/// channel is full. Results are never dropped.
pub const SEARCH_EVENT_CAPACITY: usize = 64;

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// What to search for: kinds, a center, a radius, and optional filters.
///
/// The center is world XZ in blocks (fractional values are legal; the search
/// space is the `i32` block world, so centers beyond it saturate). The
/// search distance is the anchor distance: the 2-D Euclidean distance from
/// the center to the feature's reported `position` in the XZ plane — see the
/// module docs for the anchor assumption.
#[derive(Debug, Clone, PartialEq)]
pub struct FeatureSearch {
    /// Which feature kinds to look for.
    pub kinds: FeatureKindMask,
    /// Search center, world XZ in blocks.
    pub center: (f64, f64),
    /// Maximum anchor distance in blocks; results farther away are never
    /// emitted or even buffered.
    pub max_radius: f64,
    /// Stop after this many results. `0` means no limit.
    pub max_results: usize,
    /// Region edge in blocks for the ring walk (`DEFAULT_REGION_SIZE`).
    pub region_size: u32,
    /// Additional filters; see [`SearchFilters`].
    pub filters: SearchFilters,
}

impl Default for FeatureSearch {
    /// Every kind around the world origin, within 10,000 blocks, up to 200
    /// results, 512-block regions, no filters. Arbitrary but stable — the
    /// search UI (T26) sets every field explicitly.
    fn default() -> Self {
        Self {
            kinds: FeatureKindMask::ALL,
            center: (0.0, 0.0),
            max_radius: 10_000.0,
            max_results: 200,
            region_size: DEFAULT_REGION_SIZE,
            filters: SearchFilters::default(),
        }
    }
}

/// Optional feature filters beyond the kind mask.
///
/// `categories`, `name_contains` and `min_size` are pushed down into the
/// [`FeatureFilter`] the source sees (the generator applies them after
/// placement, so they never change which features exist); `proximity` is a
/// local post-filter over the results.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SearchFilters {
    /// Only these exact category names (source-side).
    pub categories: Option<Vec<String>>,
    /// Case-insensitive substring on the feature name (source-side).
    pub name_contains: Option<String>,
    /// Minimum effective footprint size (source-side).
    pub min_size: Option<i32>,
    /// Drop results that have another feature too close by
    /// ([`ProximityFilter`], local).
    pub proximity: Option<ProximityFilter>,
}

/// The "within N blocks of another feature" filter: a candidate is dropped
/// when any other feature of [`Self::neighbor_kinds`] lies within
/// [`Self::min_separation`] blocks (2-D XZ anchor distance) of it.
///
/// The neighbor pool is exactly the features the search has *queried*: every
/// filter-passing feature in the regions of rings walked so far — including
/// features beyond `max_radius` that share those regions, and including
/// candidates the filter itself drops (mutually close features exclude each
/// other). Features in never-queried regions are invisible to the filter.
/// A candidate is only compared once every possible neighbor is known: with
/// this filter active the emission threshold tightens by
/// `min_separation` blocks, so "no closer valid result is dropped due to ring
/// order" holds here too.
#[derive(Debug, Clone, PartialEq)]
pub struct ProximityFilter {
    /// Separation in blocks (2-D XZ).
    pub min_separation: f64,
    /// Which kinds count as "another feature".
    pub neighbor_kinds: FeatureKindMask,
}

impl Default for ProximityFilter {
    /// No separation demanded: the filter matches everything. A no-op by
    /// construction, so forgetting to set `min_separation` cannot hide
    /// results.
    fn default() -> Self {
        Self {
            min_separation: 0.0,
            neighbor_kinds: FeatureKindMask::ALL,
        }
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// One streamed search result.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    /// The feature itself.
    pub feature: Feature,
    /// Anchor distance from the search center, world XZ in blocks.
    pub distance: f64,
    /// Teleport command for the feature's position (`/tp x y z`) — the
    /// shared formatter every copy-to-clipboard surface reuses (T31).
    pub teleport: String,
}

impl SearchResult {
    /// A one-line label: name, category, distance.
    #[must_use]
    pub fn label(&self) -> String {
        format!(
            "{} ({}) — {:.0} blocks",
            self.feature.name, self.feature.category, self.distance
        )
    }
}

/// Formats a teleport command for a block position.
///
/// Pure so the right-click copy (T26) and the future share-string surfaces
/// (T31) cannot drift apart.
#[must_use]
pub fn teleport_string(position: (i32, i32, i32)) -> String {
    format!("/tp {} {} {}", position.0, position.1, position.2)
}

/// A heartbeat after each region query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchProgress {
    /// The ring the scan just finished a region of.
    pub ring: u32,
    /// Total regions queried so far.
    pub regions_queried: u64,
    /// Results emitted so far.
    pub results_so_far: usize,
}

/// How the search ended, delivered in the final [`SearchEvent::Done`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchStats {
    /// Results emitted.
    pub results: usize,
    /// On-world regions queried.
    pub regions_queried: u64,
    /// Rings whose regions were fully queried.
    pub rings_scanned: u32,
    /// True when the result cap cut the search short while more results
    /// existed (unemitted buffer entries or unscanned area within the
    /// radius). False when the scan exhausted the radius.
    pub truncated: bool,
    /// True when a cancellation stopped the search.
    pub cancelled: bool,
    /// Wall time from the first region query to the end.
    pub elapsed: Duration,
}

/// Everything a search can tell its caller, in arrival order:
/// [`Started`](SearchEvent::Started), then any mixture of
/// [`Result`](SearchEvent::Result) and [`Progress`](SearchEvent::Progress),
/// ending in exactly one terminal event.
///
/// The terminal event is [`Done`] (both for completion and for cancellation —
/// a cancelled search sends `Done { cancelled: true }` and discards its
/// buffer; the caller never has to guess whether more is coming) or
/// [`Error`] (the source failed; nothing follows an `Error`). The only way to
/// see no terminal event is dropping the [`SearchHandle`] first.
#[derive(Debug, Clone, PartialEq)]
pub enum SearchEvent {
    /// The search began; the first region query is about to run.
    Started,
    /// One result, nearest-first across the whole stream.
    Result(SearchResult),
    /// A region query completed; dropped freely under backpressure.
    Progress(SearchProgress),
    /// The search ended: completed, capped or cancelled.
    Done(SearchStats),
    /// The source failed; the stream ends here (no `Done` follows an
    /// `Error`, so the UI can stop receiving).
    Error(SourceError),
}

// ---------------------------------------------------------------------------
// Handle
// ---------------------------------------------------------------------------

/// The caller's side of a spawned search: the cancel flag and the event
/// stream.
///
/// Dropping the handle cancels the search *and* closes the channel, so the
/// worker always terminates: its next `send` fails and the loop exits — no
/// thread leak even if the UI forgets to call [`SearchHandle::cancel`]. A
/// live handle whose receiver is never drained can still block the worker on
/// a full channel; the UI owns draining (a `recv_timeout` loop, T26).
#[must_use]
pub struct SearchHandle {
    cancel: CancelToken,
    events: Receiver<SearchEvent>,
}

impl SearchHandle {
    /// Requests cancellation. The worker stops at its next cancellation
    /// point (between regions, before an emission) and sends
    /// `Done { cancelled: true }`; results already delivered stay valid.
    pub fn cancel(&self) {
        self.cancel.store(true, AtomicOrdering::Relaxed);
    }

    /// Whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(AtomicOrdering::Relaxed)
    }

    /// The event stream. See [`SearchEvent`] for the terminal-event contract.
    #[must_use]
    pub fn events(&self) -> &Receiver<SearchEvent> {
        &self.events
    }
}

impl Drop for SearchHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl std::fmt::Debug for SearchHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SearchHandle")
            .field("cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

/// Spawns a feature search on its own worker thread and returns the handle.
///
/// The worker owns `source` and drops it on its own thread when the search
/// ends (the retirement rule for heavy background objects). Events stream
/// through a channel of [`SEARCH_EVENT_CAPACITY`] under the backpressure
/// policy documented there.
pub fn run_feature_search(source: Arc<dyn FeatureSource>, search: FeatureSearch) -> SearchHandle {
    let cancel = cancel_token();
    let (sender, receiver) = bounded(SEARCH_EVENT_CAPACITY);
    let worker_cancel = Arc::clone(&cancel);

    thread::Builder::new()
        .name("feature-search".to_string())
        .spawn(move || {
            let stats = run_feature_search_blocking(&*source, &search, &worker_cancel, |event| {
                match event {
                    SearchEvent::Progress(progress) => {
                        match sender.try_send(SearchEvent::Progress(progress)) {
                            Ok(()) => true,
                            // Backpressure sacrifice: a full channel drops
                            // the heartbeat and the search keeps running.
                            Err(TrySendError::Full(_)) => true,
                            Err(TrySendError::Disconnected(_)) => false,
                        }
                    }
                    // Results and terminal events block until the receiver
                    // catches up: a slow UI slows the search instead of
                    // growing memory without bound.
                    SearchEvent::Started
                    | SearchEvent::Result(_)
                    | SearchEvent::Done(_)
                    | SearchEvent::Error(_) => sender.send(event).is_ok(),
                }
            });
            log::debug!("feature search worker finished: {stats:?}");
        })
        .expect(
            "spawning the feature-search worker thread requires only \
                 normal OS resources; failure here is unrecoverable for the \
                 caller anyway",
        );

    SearchHandle {
        cancel,
        events: receiver,
    }
}

// ---------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------

/// Runs a feature search synchronously on the calling thread, streaming
/// events through `emit`.
///
/// This is the pure core [`run_feature_search`] wraps; it exists so tests
/// drive a search deterministically and T25 can reuse the event conventions
/// without a thread. `emit` returns `false` when its receiver is gone; the
/// search then stops immediately, without a terminal event (there is nobody
/// left to receive one). Cancellation is checked between regions and before
/// every emission.
///
/// # Panics
///
/// Panics via `assert!` on malformed requests (zero region size, negative or
/// non-finite radius, non-finite center or separation) — violated call
/// contracts, not runtime failures.
pub fn run_feature_search_blocking(
    source: &dyn FeatureSource,
    search: &FeatureSearch,
    cancel: &CancelToken,
    mut emit: impl FnMut(SearchEvent) -> bool,
) -> SearchStats {
    assert!(
        search.region_size >= 1,
        "the search region size must be at least one block, got {}",
        search.region_size,
    );
    assert!(
        search.max_radius.is_finite() && search.max_radius >= 0.0,
        "the search radius must be finite and non-negative, got {}",
        search.max_radius,
    );
    assert!(
        search.center.0.is_finite() && search.center.1.is_finite(),
        "the search center must be finite, got {:?}",
        search.center,
    );
    if let Some(proximity) = &search.filters.proximity {
        assert!(
            proximity.min_separation.is_finite() && proximity.min_separation >= 0.0,
            "the proximity separation must be finite and non-negative, got {}",
            proximity.min_separation,
        );
    }

    let started = Instant::now();
    let mut stats = SearchStats {
        results: 0,
        regions_queried: 0,
        rings_scanned: 0,
        truncated: false,
        cancelled: false,
        elapsed: Duration::ZERO,
    };
    if !emit(SearchEvent::Started) {
        stats.elapsed = started.elapsed();
        return stats;
    }

    let geometry = RingGeometry::new(search.center, search.region_size);
    // The source owns filter semantics (the generator applies the filter
    // after placement); the search never re-applies it locally, so the
    // adapter's documented behavior stays the single source of truth.
    let filter = FeatureFilter {
        kinds: search.kinds,
        categories: search.filters.categories.clone(),
        name_contains: search.filters.name_contains.clone(),
        min_size: search.filters.min_size,
    };
    let proximity = search.filters.proximity.clone();
    // With a proximity filter active, a candidate is only decided once all
    // possible neighbors are known, which delays its emission by the
    // separation (see ProximityFilter).
    let threshold_margin = proximity
        .as_ref()
        .map_or(0.0, |proximity| proximity.min_separation);
    let unlimited = search.max_results == 0;

    // The neighbor pool (only maintained when a proximity filter needs it):
    // every filter-passing feature the queried regions returned.
    let mut pool: Vec<Feature> = Vec::new();
    // Dedupe across rings: a feature whose bounds straddle two regions is
    // returned by both (the generator matches prefabs by bounds
    // intersection). Same key as `features::dedupe_features` (T23).
    let mut seen: HashSet<FeatureKey> = HashSet::new();
    let mut pending: BinaryHeap<Reverse<PendingItem>> = BinaryHeap::new();

    let mut emitted = 0usize;
    let mut truncated = false;
    let mut cancelled = false;
    // The receiver disappeared: stop without a terminal event — there is
    // nobody left to tell.
    let mut gone = false;

    'rings: for ring in 0u32.. {
        if ring_min_distance(&geometry, ring) > search.max_radius {
            break;
        }
        if ring_entirely_off_world(&geometry, ring) {
            // The walk left the i32 world on all four sides; no on-world
            // region can appear in any later ring.
            break;
        }

        for (rx, rz) in ring_regions(geometry.rx0, geometry.rz0, ring) {
            if cancel.load(AtomicOrdering::Relaxed) {
                cancelled = true;
                break 'rings;
            }
            // Regions past the i32 world edge clamp to it; a region fully
            // outside has nothing to query.
            let Some(rect) = cell_rect(rx, rz, geometry.size) else {
                continue;
            };
            let features = match query_guarded(source, rect, &filter) {
                Ok(features) => features,
                Err(error) => {
                    // Terminal: an Error ends the stream; no Done follows.
                    emit(SearchEvent::Error(error));
                    stats.results = emitted;
                    stats.elapsed = started.elapsed();
                    return stats;
                }
            };
            stats.regions_queried += 1;
            for feature in features {
                let key = feature_key(&feature);
                if !seen.insert(key.clone()) {
                    continue;
                }
                if proximity.is_some() {
                    pool.push(feature.clone());
                }
                let (x, _, z) = feature.position;
                let distance = distance_2d(geometry.cx, geometry.cz, x, z);
                if distance > search.max_radius {
                    // Beyond the radius: never a candidate, but still a
                    // proximity neighbor for the region it came from.
                    continue;
                }
                pending.push(Reverse(PendingItem { distance, feature }));
            }
            if !emit(SearchEvent::Progress(SearchProgress {
                ring,
                regions_queried: stats.regions_queried,
                results_so_far: emitted,
            })) {
                gone = true;
                break 'rings;
            }
        }
        stats.rings_scanned = ring + 1;

        // Emit everything the later rings cannot beat. `safe` is the closest
        // any anchor in ring `ring + 1` (or beyond) can be, so pending
        // results strictly closer are final; the rest wait for later rings.
        let safe = ring_min_distance(&geometry, ring + 1);
        let threshold = safe - threshold_margin;
        while unlimited || emitted < search.max_results {
            if !pending
                .peek()
                .is_some_and(|Reverse(item)| item.distance < threshold)
            {
                break;
            }
            if cancel.load(AtomicOrdering::Relaxed) {
                cancelled = true;
                break 'rings;
            }
            let Some(Reverse(item)) = pending.pop() else {
                // peek() confirmed the head one expression ago.
                unreachable!("the pending heap cannot empty between peek and pop");
            };
            if let Some(proximity) = &proximity
                && has_neighbor_within(proximity, &pool, &item.feature)
            {
                continue;
            }
            let teleport = teleport_string(item.feature.position);
            emitted += 1;
            if !emit(SearchEvent::Result(SearchResult {
                feature: item.feature,
                distance: item.distance,
                teleport,
            })) {
                // Not delivered; do not count it.
                emitted -= 1;
                gone = true;
                break 'rings;
            }
        }
        if !unlimited && emitted >= search.max_results {
            // Stopped by the cap: anything still pending or any unscanned
            // area within the radius means real results went unemitted.
            truncated = !pending.is_empty() || safe <= search.max_radius;
            break;
        }
    }

    // Flush: the scan covered every region intersecting the radius, so the
    // remaining pending results are final and sorted. A capped or cancelled
    // search does not flush — the cap already emitted its allotment, and a
    // cancellation discards the buffer by contract.
    if !cancelled && !gone && (unlimited || emitted < search.max_results) {
        while unlimited || emitted < search.max_results {
            let Some(Reverse(item)) = pending.pop() else {
                break;
            };
            if cancel.load(AtomicOrdering::Relaxed) {
                cancelled = true;
                break;
            }
            if let Some(proximity) = &proximity
                && has_neighbor_within(proximity, &pool, &item.feature)
            {
                continue;
            }
            let teleport = teleport_string(item.feature.position);
            emitted += 1;
            if !emit(SearchEvent::Result(SearchResult {
                feature: item.feature,
                distance: item.distance,
                teleport,
            })) {
                emitted -= 1;
                gone = true;
                break;
            }
        }
        if !unlimited && !pending.is_empty() {
            truncated = true;
        }
    }

    stats.results = emitted;
    stats.truncated = truncated;
    stats.cancelled = cancelled;
    stats.elapsed = started.elapsed();
    if !gone {
        // Best effort: `gone` is the only path that skips the terminal
        // event, and it was checked above.
        let _ = emit(SearchEvent::Done(stats));
    }
    stats
}

// ---------------------------------------------------------------------------
// Engine internals
// ---------------------------------------------------------------------------

/// The dedupe key: same tuple `features::dedupe_features` (T23) uses, so the
/// overlay catalog and the search engine cannot disagree about what makes
/// two features the same placement.
type FeatureKey = (FeatureKind, String, (i32, i32, i32));

#[must_use]
fn feature_key(feature: &Feature) -> FeatureKey {
    (feature.kind, feature.name.clone(), feature.position)
}

/// One buffered candidate awaiting its turn in the streamed order.
#[derive(Debug, Clone)]
struct PendingItem {
    distance: f64,
    feature: Feature,
}

impl PendingItem {
    /// The deterministic tie-break for equal distances: kind bit, then
    /// position, then name. The property test brute-forces with this exact
    /// order, so it must stay total and cheap.
    fn tie_break(feature: &Feature) -> (u16, (i32, i32, i32), &str) {
        (
            FeatureKindMask::from_kind(feature.kind).0,
            feature.position,
            feature.name.as_str(),
        )
    }
}

impl PartialEq for PendingItem {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for PendingItem {}

impl PartialOrd for PendingItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PendingItem {
    fn cmp(&self, other: &Self) -> Ordering {
        // `total_cmp` keeps NaN out of the order (the center is validated
        // finite, so distances are finite too).
        self.distance
            .total_cmp(&other.distance)
            .then_with(|| Self::tie_break(&self.feature).cmp(&Self::tie_break(&other.feature)))
    }
}

/// Whether any pool feature of the filtered kinds other than `candidate`
/// itself lies within the filter's separation (2-D XZ anchor distance).
fn has_neighbor_within(proximity: &ProximityFilter, pool: &[Feature], candidate: &Feature) -> bool {
    let candidate_key = feature_key(candidate);
    pool.iter().any(|neighbor| {
        if feature_key(neighbor) == candidate_key {
            return false;
        }
        if !proximity.neighbor_kinds.contains(neighbor.kind) {
            return false;
        }
        let (candidate_x, _, candidate_z) = candidate.position;
        let (neighbor_x, _, neighbor_z) = neighbor.position;
        distance_2d(
            f64::from(candidate_x),
            f64::from(candidate_z),
            neighbor_x,
            neighbor_z,
        ) < proximity.min_separation
    })
}

/// One `features_in` call across the foreign-code boundary.
///
/// Documented `catch_unwind` deviation, same shape as the scheduler's
/// renderer boundary (T15): the production worldgen adapter already maps its
/// own query panics ([`SourceError::Generation`] via `catch_query_panics`),
/// but the search runs on a worker thread against *any* `dyn FeatureSource`,
/// and a panic here must end the search with an [`SearchEvent::Error`] the
/// caller can see — not kill the worker silently with the UI waiting for a
/// `Done` that never comes.
fn query_guarded(
    source: &dyn FeatureSource,
    rect: Rect,
    filter: &FeatureFilter,
) -> crate::error::SourceResult<Vec<Feature>> {
    catch_unwind(AssertUnwindSafe(|| source.features_in(rect, filter)))
        .unwrap_or_else(|payload| Err(SourceError::Generation(panic_message(payload))))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_search_teleport_string_and_label() {
        assert_eq!(teleport_string((12, 64, -345)), "/tp 12 64 -345");
        let result = SearchResult {
            feature: Feature {
                kind: FeatureKind::UniquePrefab,
                name: "Temple".to_string(),
                category: "Unique".to_string(),
                position: (1, 40, 2),
                bounds: None,
                metadata: crate::types::FeatureMetadata {
                    variant: None,
                    size: None,
                    source: None,
                },
            },
            distance: 1234.56,
            teleport: teleport_string((1, 40, 2)),
        };
        assert_eq!(result.label(), "Temple (Unique) — 1235 blocks");
    }

    #[test]
    fn feature_search_pending_order_is_total_and_deterministic() {
        let feature = |name: &str, position: (i32, i32, i32)| PendingItem {
            distance: 10.0,
            feature: Feature {
                kind: FeatureKind::Spawn,
                name: name.to_string(),
                category: "spawn".to_string(),
                position,
                bounds: None,
                metadata: crate::types::FeatureMetadata {
                    variant: None,
                    size: None,
                    source: None,
                },
            },
        };
        let near = PendingItem {
            distance: 5.0,
            feature: feature("A", (0, 0, 0)).feature,
        };
        let far = PendingItem {
            distance: 6.0,
            feature: feature("B", (0, 0, 0)).feature,
        };
        assert!(near < far, "distance dominates the order");
        let tie_a = feature("A", (0, 0, 0));
        let tie_b = feature("B", (0, 0, 0));
        assert!(
            tie_a < tie_b,
            "equal distances break on the name deterministically",
        );
        assert_eq!(tie_a.cmp(&tie_a), Ordering::Equal);
    }
}
