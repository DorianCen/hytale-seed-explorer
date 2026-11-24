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
            let Some(rect) = region_rect(rx, rz, geometry.size) else {
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

/// The ring walk's fixed frame: the clamped center and its region.
#[derive(Debug, Clone, Copy)]
struct RingGeometry {
    size: i64,
    rx0: i64,
    rz0: i64,
    cx: f64,
    cz: f64,
}

impl RingGeometry {
    /// Clamps the center into the i32 block world (features live at i32
    /// positions; a center beyond the world saturates to its edge) and
    /// finds the region containing it: `floor(center / size)`.
    fn new(center: (f64, f64), region_size: u32) -> Self {
        let cx = center.0.clamp(f64::from(i32::MIN), f64::from(i32::MAX));
        let cz = center.1.clamp(f64::from(i32::MIN), f64::from(i32::MAX));
        let size = i64::from(region_size);
        let rx0 = (cx / size as f64).floor() as i64;
        let rz0 = (cz / size as f64).floor() as i64;
        Self {
            size,
            rx0,
            rz0,
            cx,
            cz,
        }
    }
}

/// The on-world part of region (`rx`, `rz`), or [`None`] when the region lies
/// entirely outside the i32 block world.
///
/// A region straddling the world edge clamps to it — the part of the region
/// that exists is still worth one query. The off-world check happens on the
/// raw i64 bounds *before* clamping: clamping first would collapse an
/// off-world region into a degenerate one-block rect.
fn region_rect(rx: i64, rz: i64, size: i64) -> Option<Rect> {
    let min_x = rx.checked_mul(size)?;
    let max_x = min_x.checked_add(size - 1)?;
    let min_z = rz.checked_mul(size)?;
    let max_z = min_z.checked_add(size - 1)?;
    let world_min = i64::from(i32::MIN);
    let world_max = i64::from(i32::MAX);
    if max_x < world_min || min_x > world_max || max_z < world_min || min_z > world_max {
        return None;
    }
    Some(Rect::new(
        clamp_to_i32(min_x),
        clamp_to_i32(min_z),
        clamp_to_i32(max_x),
        clamp_to_i32(max_z),
    ))
}

/// `value` clamped into the i32 range; exact for values already in range.
fn clamp_to_i32(value: i64) -> i32 {
    value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// The closest any point of ring `ring`'s regions can be to the center.
///
/// O(1). Ring 0 is the center region itself — a fractional center can sit
/// between blocks, *outside* its region's inclusive bounds, so its minimum
/// is the plain point-to-rect distance into that region. Ring `r ≥ 1` is
/// covered by four strips (the region columns `±r` steps east/west and the
/// rows `±r` steps south/north); a strip's minimum is the hypotenuse of its
/// axis clearance and the center's gap to the nearest row-rect of the
/// orthogonal axis — a fractional center parked between blocks is that gap
/// away from both adjacent rows, and the corner regions make the diagonal
/// the true minimum, not the bare axis clearance. The value is a lower
/// bound for the *bounds* of features not yet returned; the module docs
/// state the anchor assumption that turns it into a lower bound for their
/// anchors.
fn ring_min_distance(geometry: &RingGeometry, ring: u32) -> f64 {
    let size = geometry.size;
    if ring == 0 {
        let dx = axis_distance_into_rect(geometry.cx, geometry.rx0, size);
        let dz = axis_distance_into_rect(geometry.cz, geometry.rz0, size);
        return dx.hypot(dz);
    }
    let r = i64::from(ring);
    let size = size as f64;
    let x_gap = axis_gap_to_adjacent_rows(geometry.cx, geometry.rx0, geometry.size);
    let z_gap = axis_gap_to_adjacent_rows(geometry.cz, geometry.rz0, geometry.size);
    let east = (geometry.rx0 + r) as f64 * size - geometry.cx;
    let west = geometry.cx - ((geometry.rx0 - r) as f64 * size + size - 1.0);
    let south = (geometry.rz0 + r) as f64 * size - geometry.cz;
    let north = geometry.cz - ((geometry.rz0 - r) as f64 * size + size - 1.0);
    east.hypot(z_gap)
        .min(west.hypot(z_gap))
        .min(south.hypot(x_gap))
        .min(north.hypot(x_gap))
        .max(0.0)
}

/// Distance from `v` to the rect of region `index` along one axis: zero
/// inside, else the distance to the nearer edge (ring 0's bound).
fn axis_distance_into_rect(v: f64, index: i64, size: i64) -> f64 {
    let row_min = index as f64 * size as f64;
    let row_max = row_min + (size - 1) as f64;
    (row_min - v).max(0.0).max(v - row_max)
}

/// Distance from `v` to the nearest point of the row-rects of `index` and
/// `index + 1` along one axis. Zero inside a rect; a fractional `v` parked
/// in the crack between the top block of `index` and the bottom block of
/// `index + 1` is the smaller clearance to either side. Both rows are part
/// of every ring-`r ≥ 1` strip, so this is the strip's orthogonal gap.
fn axis_gap_to_adjacent_rows(v: f64, index: i64, size: i64) -> f64 {
    let own_gap = axis_distance_into_rect(v, index, size);
    if own_gap == 0.0 {
        return 0.0;
    }
    let next_row_gap = axis_distance_into_rect(v, index + 1, size);
    own_gap.min(next_row_gap)
}

/// Whether every region of ring `ring` lies outside the i32 world.
///
/// O(1): each ring edge's midpoint region shares the center region's range
/// on the other axis (always on-world), so a midpoint being off-world means
/// the whole edge is beyond that world bound — and the four edges are the
/// ring. Ring 0 contains the center and is never off-world.
fn ring_entirely_off_world(geometry: &RingGeometry, ring: u32) -> bool {
    if ring == 0 {
        return false;
    }
    let r = i64::from(ring);
    region_rect(geometry.rx0 - r, geometry.rz0, geometry.size).is_none()
        && region_rect(geometry.rx0 + r, geometry.rz0, geometry.size).is_none()
        && region_rect(geometry.rx0, geometry.rz0 - r, geometry.size).is_none()
        && region_rect(geometry.rx0, geometry.rz0 + r, geometry.size).is_none()
}

/// The regions of ring `ring`: Chebyshev index exactly `ring` from the
/// center's region, in a deterministic perimeter walk (top edge left to
/// right, bottom edge, then the two side interiors).
fn ring_regions(rx0: i64, rz0: i64, ring: u32) -> impl Iterator<Item = (i64, i64)> {
    let r = i64::from(ring);
    let top = (-r..=r).map(move |dx| (rx0 + dx, rz0 - r));
    // Ring 0 is the center region alone; the other three edges would
    // duplicate it.
    let bottom = if r == 0 {
        None
    } else {
        Some((-r..=r).map(move |dx| (rx0 + dx, rz0 + r)))
    };
    let left = (-r + 1..=r - 1).map(move |dz| (rx0 - r, rz0 + dz));
    let right = if r == 0 {
        None
    } else {
        Some((-r + 1..=r - 1).map(move |dz| (rx0 + r, rz0 + dz)))
    };
    top.chain(bottom.into_iter().flatten())
        .chain(left)
        .chain(right.into_iter().flatten())
}

/// 2-D Euclidean distance in the XZ plane.
fn distance_2d(cx: f64, cz: f64, x: i32, z: i32) -> f64 {
    let dx = cx - f64::from(x);
    let dz = cz - f64::from(z);
    dx.hypot(dz)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Point-to-rect XZ distance, the brute force `ring_min_distance` is
    /// compared against.
    fn point_rect_distance(cx: f64, cz: f64, rect: &Rect) -> f64 {
        let dx = (f64::from(rect.min_x) - cx)
            .max(0.0)
            .max(cx - f64::from(rect.max_x));
        let dz = (f64::from(rect.min_z) - cz)
            .max(0.0)
            .max(cz - f64::from(rect.max_z));
        dx.hypot(dz)
    }

    #[test]
    fn feature_search_ring_regions_count_and_uniqueness() {
        let expected_counts = [(0u32, 1usize), (1, 8), (2, 16), (3, 24), (7, 56)];
        for (ring, count) in expected_counts {
            assert_eq!(
                ring_regions(10, -4, ring).count(),
                count,
                "ring {ring} must hold 8r regions (1 for the center)",
            );
        }
        for ring in [1u32, 2, 5] {
            let regions: Vec<(i64, i64)> = ring_regions(0, 0, ring).collect();
            let unique: HashSet<(i64, i64)> = regions.iter().copied().collect();
            assert_eq!(
                regions.len(),
                unique.len(),
                "ring {ring} must not repeat a region",
            );
            for &(rx, rz) in &regions {
                assert_eq!(
                    (rx.abs(), rz.abs()).0.max((rx.abs(), rz.abs()).1),
                    i64::from(ring),
                    "every region of ring {ring} is at Chebyshev index {ring}",
                );
            }
        }
        // Ring 0 is exactly the center region.
        assert_eq!(ring_regions(3, 7, 0).collect::<Vec<_>>(), vec![(3, 7)]);
    }

    #[test]
    fn feature_search_ring_min_distance_matches_brute_force() {
        // Centers inside a region, exactly on a boundary, near an edge, and
        // negative — the four-clearance shortcut must equal the true minimum
        // point-to-region distance for every early ring.
        let centers: [(f64, f64); 4] = [(100.5, 200.25), (0.0, 0.0), (511.9, -0.1), (-0.5, -512.0)];
        for center in centers {
            let geometry = RingGeometry::new(center, 512);
            for ring in 0u32..8 {
                let brute = ring_regions(geometry.rx0, geometry.rz0, ring)
                    .filter_map(|(rx, rz)| region_rect(rx, rz, geometry.size))
                    .map(|rect| point_rect_distance(geometry.cx, geometry.cz, &rect))
                    .fold(f64::INFINITY, f64::min);
                let fast = ring_min_distance(&geometry, ring);
                assert!(
                    (fast - brute).abs() < 1e-9,
                    "ring_min({ring}) from {center:?}: {fast} must match the \
                     brute-force {brute}",
                );
            }
        }
    }

    #[test]
    fn feature_search_region_rect_alignment_and_edges() {
        // Inclusive max: a 512 region is 512 blocks wide.
        assert_eq!(
            region_rect(0, 0, 512),
            Some(Rect::new(0, 0, 511, 511)),
            "region (0, 0) covers 0..=511",
        );
        assert_eq!(
            region_rect(-1, 0, 512),
            Some(Rect::new(-512, 0, -1, 511)),
            "negative regions floor toward minus infinity",
        );
        // A center exactly on a region boundary belongs to the next region.
        assert_eq!(RingGeometry::new((512.0, 0.0), 512).rx0, 1);
        assert_eq!(RingGeometry::new((511.9, 0.0), 512).rx0, 0);
        // Centers beyond the i32 world saturate to its edge.
        let saturated = RingGeometry::new((1e12, -1e12), 512);
        assert_eq!(saturated.cx, f64::from(i32::MAX));
        assert_eq!(saturated.cz, f64::from(i32::MIN));
        // Regions fully outside the world are None; straddling ones clamp.
        assert!(
            region_rect(10_000_000, 0, 512).is_none(),
            "a region fully east of the i32 world has nothing to query",
        );
        assert_eq!(
            region_rect(4_194_303, 0, 512),
            Some(Rect::new(i32::MAX - 511, 0, i32::MAX, 511)),
            "the region containing i32::MAX clamps to it",
        );
    }

    #[test]
    fn feature_search_ring_off_world_detection_matches_per_region_rects() {
        let geometry = RingGeometry::new((0.0, 0.0), 512);
        // Ring 2 is fully on-world; a ring past every world edge is not.
        assert!(!ring_entirely_off_world(&geometry, 2));
        let far_ring = u32::try_from(9_000_000).expect("fits u32");
        assert!(
            ring_entirely_off_world(&geometry, far_ring),
            "a ring beyond the i32 world on all four sides is off-world",
        );
        // The O(1) check must agree with the per-region truth on a ring that
        // only partially leaves the world: every sampled region rect of a
        // ring the shortcut calls off-world is really None. Sampled (every
        // 1,000th region) because this ring holds ~33M regions.
        let mut sampled = 0usize;
        for (index, (rx, rz)) in ring_regions(geometry.rx0, geometry.rz0, far_ring).enumerate() {
            if index % 1_000 != 0 {
                continue;
            }
            sampled += 1;
            assert!(
                region_rect(rx, rz, geometry.size).is_none(),
                "shortcut claimed off-world but ({rx}, {rz}) has a rect",
            );
        }
        assert!(sampled > 1_000, "the sample must cover the ring's edges");
    }

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
