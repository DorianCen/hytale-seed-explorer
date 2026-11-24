//! Biome search: coarse ring sampling, boundary refinement and optional
//! patch area (T25).
//!
//! Given a biome id, a center and a radius, the engine walks the same ring
//! geometry the feature search does (see [`super::rings`]) — but over
//! **sampling cells** of `coarse_step` blocks instead of query regions:
//!
//! 1. **Coarse pass** — ring by ring, batched into four strip queries per
//!    ring ([`WorldSource::biome_region`], never one round-trip per cell).
//!    A cell is represented by its lattice point — the multiple-of-step
//!    block at its lower-left corner. A biome thinner than the step that
//!    dodges the lattice can be missed; that is the documented coarse
//!    limitation, and `coarse_step` is the thorough-mode lever (halving the
//!    step quadruples the scan).
//! 2. **Frontier filter** — a matched cell with a strictly closer matched
//!    8-neighbor is skipped: inside one big biome only the cells facing the
//!    center survive, so a search does not emit a pin for every interior
//!    cell of the same patch, and — just as important — does not pay a
//!    step-1 refinement per interior cell. This is a *cost heuristic*, and
//!    it is sound for one shadow step: a block within the step − 1
//!    coverage of a shadowed cell is at most `2·step − 1` from its
//!    shadower, and the refinement window is that wide, so the shadower
//!    inherits the coverage (a shadower from a *different* patch finds the
//!    shadowed patch's nearest block, not its own). Multi-step shadow
//!    chains — matched cells strictly decreasing in distance while marching
//!    8-adjacent around the annulus of the nearest block — could in
//!    principle out-walk the window; the property tests hunt such fields
//!    (thousands of generated cases) and none has surfaced.
//! 3. **Refinement** — each surviving cell is verified at step 1 over its
//!    `±(2·coarse_step − 1)` neighborhood, and the block nearest the
//!    *search center* is chosen. The result is a true `biome_at` reading,
//!    not a coarse cell corner, and never beyond `max_radius` (the radius
//!    cut is local — the ring walk bounds the scan, not each result).
//! 4. **Patch area** (opt-in, [`BiomeSearch::min_patch_blocks`]) — a
//!    4-connected flood fill over matched cells around the candidate, capped
//!    by [`PATCH_RING_CAP`]/[`PATCH_CELL_CAP`]; the area is
//!    `cells × step²` blocks², an estimate that assumes each matched cell's
//!    footprint is fully the biome. Candidates below the minimum are
//!    dropped, not emitted.
//!
//! Ordering is exact, not look-ahead-approximate: after ring `r` is
//! refined, a pending result strictly closer than
//! `ring_min(r + 1) − (2·step − 1)·√2` (the closest block any later-ring
//! refinement could reach) is safe to emit; everything else waits in a
//! pending heap. So the stream is globally nearest-first, mirroring T24's
//! feature engine.
//!
//! The event/handle/channel/cancellation contract is deliberately the same
//! shape as [`super::feature`]'s — one search model for the UI (T26): a
//! bounded event channel (results block, progress drops), a cancel token
//! checked between strips, before every refinement and before every
//! emission, `Done { cancelled: true }` on cancel, `Error` with no `Done`,
//! and a handle whose `Drop` cancels *and* closes the stream so the worker
//! thread always terminates.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, TrySendError, bounded};

use crate::error::{SourceError, panic_message};
use crate::scheduler::{CancelToken, cancel_token};
use crate::source::WorldSource;
use crate::types::{BiomeGrid, BiomeId, Rect};

use super::feature::SEARCH_EVENT_CAPACITY;
use super::rings::{RingGeometry, clamp_to_i32, distance_2d, ring_min_distance};

/// Default coarse sampling step in blocks (the plan's 16–32 band, low end:
/// halving it quadruples the scan cost).
pub const DEFAULT_COARSE_STEP: u32 = 16;

/// How many rings of cells around a candidate the patch-area flood fill
/// walks before declaring the area capped (32 rings at step 16 = ±512
/// blocks — a patch wider than that reports `area_capped`).
pub const PATCH_RING_CAP: u32 = 32;

/// How many cells the patch-area flood fill counts before declaring the
/// area capped (4,096 cells = at most 4096 × step² blocks² of estimate).
pub const PATCH_CELL_CAP: u32 = 4_096;

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// What to search for: a biome around a center.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BiomeSearch {
    /// The biome to find.
    pub biome: BiomeId,
    /// Search center, world XZ in blocks (fractional values are legal; the
    /// search space is the i32 block world, so centers beyond it saturate).
    pub center: (f64, f64),
    /// Maximum distance in blocks.
    pub max_radius: f64,
    /// Only report patches of at least this many blocks² (estimated; see
    /// the module docs). Runs the flood fill per candidate — opt-in.
    pub min_patch_blocks: Option<u32>,
    /// Stop after this many results. `0` means no limit.
    pub max_results: usize,
    /// Coarse sampling step in blocks — the thorough-mode lever.
    pub coarse_step: u32,
}

impl Default for BiomeSearch {
    /// Biome 0 around the world origin, within 10,000 blocks, up to 200
    /// results, no patch minimum, step-16 sampling. Arbitrary but stable —
    /// the search UI (T26) sets every field explicitly.
    fn default() -> Self {
        Self {
            biome: BiomeId(0),
            center: (0.0, 0.0),
            max_radius: 10_000.0,
            min_patch_blocks: None,
            max_results: 200,
            coarse_step: DEFAULT_COARSE_STEP,
        }
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// One streamed search result: a real block whose `biome_at` reading is the
/// requested biome, with the distance to the search center.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BiomeMatch {
    /// A block position whose `biome_at` is the requested biome (a step-1
    /// sample, not a coarse cell corner).
    pub position: (i32, i32),
    /// Distance from the search center, world XZ in blocks.
    pub distance: f64,
    /// Estimated patch area in blocks² around the match, when
    /// [`BiomeSearch::min_patch_blocks`] was requested; [`None`] otherwise.
    pub patch_area: Option<u64>,
    /// True when the flood fill hit a cap before closing the patch, so
    /// `patch_area` is a lower bound.
    pub area_capped: bool,
}

/// A heartbeat after every coarse strip query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BiomeSearchProgress {
    /// The ring the scan just finished a strip of.
    pub ring: u32,
    /// Coarse samples taken so far.
    pub samples: u64,
    /// Results emitted so far.
    pub results_so_far: usize,
}

/// How the search ended, delivered in the final
/// [`BiomeSearchEvent::Done`]. Same contract as T24's [`SearchStats`]:
/// `truncated` is the conservative "the cap cut the scan over unscanned
/// area" flag, not a precise "more results existed".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BiomeSearchStats {
    /// Results emitted.
    pub results: usize,
    /// Coarse samples taken (strip queries only; refinements excluded).
    pub samples: u64,
    /// Rings whose strips were fully queried.
    pub rings_scanned: u32,
    /// True when the result cap cut the scan short over unscanned area or
    /// an unemitted buffer.
    pub truncated: bool,
    /// True when a cancellation stopped the search.
    pub cancelled: bool,
    /// Wall time from the first strip query to the end.
    pub elapsed: Duration,
}

/// Everything a biome search can tell its caller — same terminal contract
/// as [`super::feature::SearchEvent`]: one `Started`, then a mixture of
/// `Result` and `Progress`, ending in exactly one terminal event, which is
/// `Done` (also for cancellation) or `Error` (the source failed; nothing
/// follows).
#[derive(Debug, Clone, PartialEq)]
pub enum BiomeSearchEvent {
    /// The search began; the first strip query is about to run.
    Started,
    /// One result, globally nearest-first across the whole stream.
    Result(BiomeMatch),
    /// A strip query completed; dropped freely under backpressure.
    Progress(BiomeSearchProgress),
    /// The search ended: completed, capped or cancelled.
    Done(BiomeSearchStats),
    /// The source failed; the stream ends here.
    Error(SourceError),
}

// ---------------------------------------------------------------------------
// Handle
// ---------------------------------------------------------------------------

/// The caller's side of a spawned biome search — the same contract as
/// [`super::feature::SearchHandle`]: cancel stops the worker at its next
/// check, dropping the handle cancels *and* closes the channel so the
/// worker always terminates, and a live receiver must keep draining (a
/// `recv_timeout` loop, T26).
#[must_use]
pub struct BiomeSearchHandle {
    cancel: CancelToken,
    events: Receiver<BiomeSearchEvent>,
}

impl BiomeSearchHandle {
    /// Requests cancellation. The worker stops at its next check and sends
    /// `Done { cancelled: true }`; results already delivered stay valid.
    pub fn cancel(&self) {
        self.cancel.store(true, AtomicOrdering::Relaxed);
    }

    /// Whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(AtomicOrdering::Relaxed)
    }

    /// The event stream. See [`BiomeSearchEvent`] for the terminal contract.
    #[must_use]
    pub fn events(&self) -> &Receiver<BiomeSearchEvent> {
        &self.events
    }
}

impl Drop for BiomeSearchHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl std::fmt::Debug for BiomeSearchHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BiomeSearchHandle")
            .field("cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

/// Spawns a biome search on its own worker thread and returns the handle.
///
/// The worker owns `source` and drops it on its own thread when the search
/// ends (the retirement rule for heavy background objects). Events stream
/// through the shared [`SEARCH_EVENT_CAPACITY`] channel under the same
/// backpressure policy as the feature search: results and terminal events
/// block, progress is dropped when full.
pub fn run_biome_search(source: Arc<dyn WorldSource>, search: BiomeSearch) -> BiomeSearchHandle {
    let cancel = cancel_token();
    let (sender, receiver) = bounded(SEARCH_EVENT_CAPACITY);
    let worker_cancel = Arc::clone(&cancel);

    thread::Builder::new()
        .name("biome-search".to_string())
        .spawn(move || {
            let stats = run_biome_search_blocking(&*source, &search, &worker_cancel, |event| {
                match event {
                    BiomeSearchEvent::Progress(progress) => {
                        match sender.try_send(BiomeSearchEvent::Progress(progress)) {
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
                    BiomeSearchEvent::Started
                    | BiomeSearchEvent::Result(_)
                    | BiomeSearchEvent::Done(_)
                    | BiomeSearchEvent::Error(_) => sender.send(event).is_ok(),
                }
            });
            log::debug!("biome search worker finished: {stats:?}");
        })
        .expect(
            "spawning the biome-search worker thread requires only \
                 normal OS resources; failure here is unrecoverable for the \
                 caller anyway",
        );

    BiomeSearchHandle {
        cancel,
        events: receiver,
    }
}

// ---------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------

/// Runs a biome search synchronously on the calling thread, streaming
/// events through `emit` — the pure core [`run_biome_search`] wraps, the
/// twin of [`super::feature::run_feature_search_blocking`].
///
/// `emit` returns `false` when its receiver is gone; the search then stops
/// immediately without a terminal event. Cancellation is checked between
/// strips, before every refinement and before every emission.
///
/// # Panics
///
/// Panics via `assert!` on malformed requests (zero step, negative or
/// non-finite radius, non-finite center) — violated call contracts, not
/// runtime failures.
pub fn run_biome_search_blocking(
    source: &dyn WorldSource,
    search: &BiomeSearch,
    cancel: &CancelToken,
    mut emit: impl FnMut(BiomeSearchEvent) -> bool,
) -> BiomeSearchStats {
    assert!(
        search.coarse_step >= 1,
        "the coarse sampling step must be at least one block, got {}",
        search.coarse_step,
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

    let started = Instant::now();
    let mut stats = BiomeSearchStats {
        results: 0,
        samples: 0,
        rings_scanned: 0,
        truncated: false,
        cancelled: false,
        elapsed: Duration::ZERO,
    };
    if !emit(BiomeSearchEvent::Started) {
        stats.elapsed = started.elapsed();
        return stats;
    }

    let geometry = RingGeometry::new(search.center, search.coarse_step);
    let step = geometry.size;
    // The closest block any later-ring refinement can reach: its lattice
    // point is at least ring_min(r+1) out, and refinement searches at most
    // `refinement_span` blocks around that point on each axis. The span is
    // deliberately one shadow step WIDER than the coverage a matched cell
    // itself needs (step−1): the frontier filter is a cost heuristic, and a
    // shadowed cell's coverage must be inherited by its shadower — a block
    // within step−1 of the shadowed cell is within 2·step−1 of the
    // 8-neighbor that shadowed it, so the shadower's window provably
    // contains it (see `refine`).
    let refinement_span = 2 * geometry.size - 1;
    let refinement_slack = refinement_span as f64 * std::f64::consts::SQRT_2;
    // A block within the radius is detected only by a matched lattice
    // within (step − 1)·√2 of it (the blob coverage bound), so the walk
    // must outrun the radius by exactly that reach — stopping at the
    // radius would skip the ring whose lattices see blocks still inside
    // it. The radius cut on results stays local, so the extra rings cost
    // queries but never emit beyond the radius.
    let lattice_reach = f64::from(search.coarse_step - 1) * std::f64::consts::SQRT_2;
    let stop_bound = search.max_radius + lattice_reach;
    let unlimited = search.max_results == 0;

    // Refined positions already buffered (push-time dedupe: two frontier
    // cells of the same patch can refine to the same nearest block).
    let mut refined_positions: HashSet<(i32, i32)> = HashSet::new();
    let mut pending: BinaryHeap<Reverse<PendingMatch>> = BinaryHeap::new();
    // The previous ring's matched cells with their lattice distances: a
    // matched cell is shadowed when a matched 8-neighbor sits strictly
    // closer to the center, and neighbors can only be in the same ring or
    // one ring inward.
    let mut prev_matched: HashMap<(i64, i64), f64> = HashMap::new();

    let mut emitted = 0usize;
    let mut truncated = false;
    let mut cancelled = false;
    // The receiver disappeared: stop without a terminal event.
    let mut gone = false;

    'rings: for ring in 0u32.. {
        if ring_min_distance(&geometry, ring) > stop_bound {
            break;
        }
        if ring_walk_off_world(&geometry, ring) {
            break;
        }

        // The coarse pass for this ring: four strip queries, one row or
        // column of lattice points each. `matched` keeps the strips'
        // deterministic order; `matched_distances` is the O(1) lookup the
        // frontier filter needs.
        let mut matched: Vec<Sample> = Vec::new();
        let mut matched_distances: HashMap<(i64, i64), f64> = HashMap::new();
        {
            let mut seen = HashSet::new();
            for rect in ring_strip_rects(geometry.rx0, geometry.rz0, ring, step)
                .into_iter()
                .flatten()
            {
                if cancel.load(AtomicOrdering::Relaxed) {
                    cancelled = true;
                    break 'rings;
                }
                let grid = match region_guarded(source, rect, step as u32) {
                    Ok(grid) => grid,
                    Err(error) => {
                        // Terminal: an Error ends the stream; no Done follows.
                        emit(BiomeSearchEvent::Error(error));
                        stats.results = emitted;
                        stats.elapsed = started.elapsed();
                        return stats;
                    }
                };
                stats.samples += u64::from(grid.width) * u64::from(grid.height);
                for sample in matched_samples(&grid, step, search.biome) {
                    // Clamped world-edge strips can make two strips overlap
                    // at a corner; a cell is a cell once.
                    if seen.insert(sample.cell) {
                        matched_distances.insert(sample.cell, sample.lattice_distance(&geometry));
                        matched.push(sample);
                    }
                }
                if !emit(BiomeSearchEvent::Progress(BiomeSearchProgress {
                    ring,
                    samples: stats.samples,
                    results_so_far: emitted,
                })) {
                    gone = true;
                    break 'rings;
                }
            }
        }
        stats.rings_scanned = ring + 1;

        // Refine the frontier cells: skip any matched cell a matched
        // neighbor already represents better.
        for sample in &matched {
            if cancel.load(AtomicOrdering::Relaxed) {
                cancelled = true;
                break 'rings;
            }
            if shadowed(sample, &matched_distances, &prev_matched, &geometry) {
                continue;
            }
            let found = match refine(source, &geometry, sample, search.biome) {
                Ok(found) => found,
                Err(error) => {
                    emit(BiomeSearchEvent::Error(error));
                    stats.results = emitted;
                    stats.elapsed = started.elapsed();
                    return stats;
                }
            };
            let Some((position, distance)) = found else {
                // A world-edge artifact: the coarse sample matched but the
                // step-1 neighborhood (clamped) holds nothing.
                continue;
            };
            if distance > search.max_radius {
                // The ring walk bounds the scan, not each result: a
                // far-corner cell of the last scanned ring can refine past
                // the radius. The radius cut is local, like the feature
                // engine's.
                continue;
            }
            if !refined_positions.insert(position) {
                continue;
            }
            let (patch_area, area_capped) = match search.min_patch_blocks {
                None => (None, false),
                Some(min_blocks) => {
                    let (cells, capped) =
                        match measure_patch(source, &geometry, sample.cell, search.biome) {
                            Ok(measured) => measured,
                            Err(error) => {
                                emit(BiomeSearchEvent::Error(error));
                                stats.results = emitted;
                                stats.elapsed = started.elapsed();
                                return stats;
                            }
                        };
                    let area = u64::from(cells) * u64::from(search.coarse_step).pow(2);
                    if area < u64::from(min_blocks) {
                        // Below the minimum: dropped, never emitted.
                        continue;
                    }
                    (Some(area), capped)
                }
            };
            pending.push(Reverse(PendingMatch {
                distance,
                position,
                patch_area,
                area_capped,
            }));
        }
        prev_matched = matched_distances;

        // Emit everything later rings cannot beat: a later-ring refinement
        // cannot come closer than ring_min(r+1) minus the slack.
        let safe = ring_min_distance(&geometry, ring + 1) - refinement_slack;
        while unlimited || emitted < search.max_results {
            if !pending
                .peek()
                .is_some_and(|Reverse(item)| item.distance < safe)
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
            emitted += 1;
            if !emit(BiomeSearchEvent::Result(BiomeMatch {
                position: item.position,
                distance: item.distance,
                patch_area: item.patch_area,
                area_capped: item.area_capped,
            })) {
                // Not delivered; do not count it.
                emitted -= 1;
                gone = true;
                break 'rings;
            }
        }
        if !unlimited && emitted >= search.max_results {
            truncated = !pending.is_empty() || safe <= stop_bound;
            break;
        }
    }

    // Flush: the scan covered every ring whose lattices can see blocks
    // inside the radius, so the remaining pending results are final and
    // sorted. A capped or cancelled search does not flush — the cap
    // already emitted its allotment, and a cancellation discards the
    // buffer by contract.
    if !cancelled && !gone && (unlimited || emitted < search.max_results) {
        while unlimited || emitted < search.max_results {
            let Some(Reverse(item)) = pending.pop() else {
                break;
            };
            if cancel.load(AtomicOrdering::Relaxed) {
                cancelled = true;
                break;
            }
            emitted += 1;
            if !emit(BiomeSearchEvent::Result(BiomeMatch {
                position: item.position,
                distance: item.distance,
                patch_area: item.patch_area,
                area_capped: item.area_capped,
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
        let _ = emit(BiomeSearchEvent::Done(stats));
    }
    stats
}

// ---------------------------------------------------------------------------
// Engine internals
// ---------------------------------------------------------------------------

/// One matched coarse sample: the cell it proves and the block it sampled.
///
/// The frontier filter compares neighbors by *lattice-point* distance
/// ([`Sample::lattice_distance`]), so the sample position itself is only
/// the refinement anchor.
#[derive(Debug, Clone, Copy)]
struct Sample {
    /// Cell index on the sampling grid (`floor(position / step)`).
    cell: (i64, i64),
    /// The lattice (or world-edge-clamped) block that matched.
    position: (i32, i32),
}

/// One buffered match awaiting its turn in the streamed order.
#[derive(Debug, Clone, Copy)]
struct PendingMatch {
    distance: f64,
    position: (i32, i32),
    patch_area: Option<u64>,
    area_capped: bool,
}

impl PartialEq for PendingMatch {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for PendingMatch {}

impl PartialOrd for PendingMatch {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PendingMatch {
    fn cmp(&self, other: &Self) -> Ordering {
        // Distance first; ties break on the position so the stream is
        // deterministic. `total_cmp` keeps NaN out (the center is
        // validated finite).
        self.distance
            .total_cmp(&other.distance)
            .then_with(|| self.position.cmp(&other.position))
    }
}

/// Whether a matched cell has a matched 8-neighbor strictly closer to the
/// center — the frontier filter: interior cells of a large patch shadow
/// each other so one patch yields its near edge, not a pin per cell, and
/// no interior cell pays a refinement. A cost heuristic with one-step
/// coverage inheritance: the shadower's refinement window is wide enough to
/// contain every block the shadowed cell covered (see the module docs for
/// the residual multi-step caveat).
///
/// The neighbor's distance is the *lattice-point* distance (not the
/// world-edge-clamped sample's), so the comparison stays consistent across
/// clamped strips.
fn shadowed(
    sample: &Sample,
    matched: &HashMap<(i64, i64), f64>,
    prev_matched: &HashMap<(i64, i64), f64>,
    geometry: &RingGeometry,
) -> bool {
    for dx in -1..=1 {
        for dz in -1..=1 {
            if dx == 0 && dz == 0 {
                continue;
            }
            let neighbor = (sample.cell.0 + dx, sample.cell.1 + dz);
            let neighbor_distance = if let Some(distance) = matched.get(&neighbor) {
                *distance
            } else if let Some(distance) = prev_matched.get(&neighbor) {
                *distance
            } else {
                continue;
            };
            if neighbor_distance < sample.lattice_distance(geometry) {
                return true;
            }
        }
    }
    false
}

impl Sample {
    /// The distance from the center to this cell's lattice point — the
    /// shadow-comparison distance, consistent even when the sampled block
    /// was clamped at the world edge.
    fn lattice_distance(&self, geometry: &RingGeometry) -> f64 {
        let x = clamp_to_i32(self.cell.0 * geometry.size);
        let z = clamp_to_i32(self.cell.1 * geometry.size);
        distance_2d(geometry.cx, geometry.cz, x, z)
    }
}

/// The matched samples of one strip grid: every grid cell whose sample is
/// the target biome.
fn matched_samples(grid: &BiomeGrid, step: i64, target: BiomeId) -> impl Iterator<Item = Sample> {
    let width = grid.width as usize;
    let step_u = i64::from(grid.step);
    let origin_x = i64::from(grid.origin_x);
    let origin_z = i64::from(grid.origin_z);
    grid.biomes
        .iter()
        .enumerate()
        .filter_map(move |(index, biome)| {
            if *biome != target {
                return None;
            }
            let sample_x = origin_x + (index % width) as i64 * step_u;
            let sample_z = origin_z + (index / width) as i64 * step_u;
            Some(Sample {
                cell: (sample_x.div_euclid(step), sample_z.div_euclid(step)),
                position: (sample_x as i32, sample_z as i32),
            })
        })
}

/// One `biome_region` call across the foreign-code boundary.
///
/// Documented `catch_unwind` deviation, the same boundary the feature
/// search guards (see [`super::feature`]): the production worldgen adapter
/// maps its own panics, but the search runs against *any* `dyn
/// WorldSource`, and a panic here must end the search with an
/// [`BiomeSearchEvent::Error`] instead of killing the worker silently.
fn region_guarded(
    source: &dyn WorldSource,
    rect: Rect,
    step: u32,
) -> crate::error::SourceResult<BiomeGrid> {
    catch_unwind(AssertUnwindSafe(|| source.biome_region(rect, step)))
        .unwrap_or_else(|payload| Err(SourceError::Generation(panic_message(payload))))
}

/// The four on-world query rects covering ring `ring`'s lattice points
/// around cell (`qx`, `qz`): one thin row rect for each edge of the ring.
///
/// Each rect is aligned to the lattice (its min corner is a lattice
/// point), so `biome_region(rect, step)` samples exactly the ring's
/// points — no overlap between edges, no re-query of inner rings.
fn ring_strip_rects(qx: i64, qz: i64, ring: u32, step: i64) -> [Option<Rect>; 4] {
    let r = i64::from(ring);
    let row = |min_x: i64, min_z: i64, max_x: i64, max_z: i64| {
        world_clamped_rect(min_x, min_z, max_x, max_z)
    };
    let top = row(
        (qx - r) * step,
        (qz - r) * step,
        (qx + r) * step,
        (qz - r) * step,
    );
    if r == 0 {
        // Ring 0 is the single center point; the other edges would
        // duplicate it.
        return [top, None, None, None];
    }
    let bottom = row(
        (qx - r) * step,
        (qz + r) * step,
        (qx + r) * step,
        (qz + r) * step,
    );
    let left = row(
        (qx - r) * step,
        (qz - r + 1) * step,
        (qx - r) * step,
        (qz + r - 1) * step,
    );
    let right = row(
        (qx + r) * step,
        (qz - r + 1) * step,
        (qx + r) * step,
        (qz + r - 1) * step,
    );
    [top, bottom, left, right]
}

/// A lattice-aligned rect clamped into the i32 world, or [`None`] when it
/// lies entirely outside it.
fn world_clamped_rect(min_x: i64, min_z: i64, max_x: i64, max_z: i64) -> Option<Rect> {
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

/// Whether the ring walk has left the i32 world on all four sides — the
/// sampling-grid twin of [`super::rings::ring_entirely_off_world`], which
/// works on cell *rects* while strips are thin rows.
fn ring_walk_off_world(geometry: &RingGeometry, ring: u32) -> bool {
    let step = geometry.size;
    ring_strip_rects(geometry.rx0, geometry.rz0, ring, step)
        .into_iter()
        .all(|rect| rect.is_none())
}

/// The refinement outcome: the nearest matching block and its center
/// distance, or [`None`] when the window holds no match.
type Refined = Option<((i32, i32), f64)>;

/// Refines one matched sample at step 1: the block nearest the *search
/// center* within `±(2·step − 1)` of the sample whose biome matches.
///
/// The span is the coverage guarantee: any block of a patch wider than the
/// step has a matched lattice point within step − 1 on both axes, and the
/// frontier filter may drop that matched cell in favor of a strictly closer
/// 8-neighbor — a block step−1 from the shadowed cell is at most
/// `step−1 + step = 2·step − 1` from the shadower, so the shadower's window
/// still contains it. A result is therefore a true `biome_at` reading and
/// the nearest block is always inside *some* surviving window. Returns
/// [`None`] when the neighborhood holds no match (a world-edge artifact).
fn refine(
    source: &dyn WorldSource,
    geometry: &RingGeometry,
    sample: &Sample,
    target: BiomeId,
) -> Result<Refined, SourceError> {
    let span = 2 * geometry.size - 1;
    let Some(rect) = world_clamped_rect(
        i64::from(sample.position.0) - span,
        i64::from(sample.position.1) - span,
        i64::from(sample.position.0) + span,
        i64::from(sample.position.1) + span,
    ) else {
        return Ok(None);
    };
    let grid = region_guarded(source, rect, 1)?;
    let mut best: Option<((i32, i32), f64)> = None;
    for (index, biome) in grid.biomes.iter().enumerate() {
        if *biome != target {
            continue;
        }
        let x = grid.origin_x + (index % grid.width as usize) as i32 * grid.step as i32;
        let z = grid.origin_z + (index / grid.width as usize) as i32 * grid.step as i32;
        let distance = distance_2d(geometry.cx, geometry.cz, x, z);
        let better = match best {
            None => true,
            Some((_, best_distance)) => distance < best_distance,
        };
        if better {
            best = Some(((x, z), distance));
        }
    }
    Ok(best)
}

/// 4-connected flood fill over matched coarse cells around `origin`,
/// expanding ring by ring (four strip queries per ring), capped by
/// [`PATCH_RING_CAP`] and [`PATCH_CELL_CAP`].
///
/// Returns `(cells, capped)`; the area estimate is
/// `cells × step²` blocks². The fill is bounded by the caps (at most
/// 4 × [`PATCH_RING_CAP`] strip queries of ≤ 65 samples), so it does not
/// need its own cancellation checks — the caps are the bound.
///
/// Acceptance is a *fixpoint closure*, not a per-ring pass: a revealed
/// matched cell joins the patch whenever any 4-neighbor already has, and
/// the sweep repeats until nothing new appears. A single ordered pass would
/// be strip-order-dependent — the corners of each ring sit before their
/// same-ring neighbors in the walk and would be rejected, wedging a
/// growing gap into the count. The closure also lets a connection wind
/// outward and back (a spiral patch), which a per-ring pass cannot.
///
/// Early exit: a ring with no matched cells at all disconnects everything
/// beyond it — a 4-connected path from the origin crosses every Chebyshev
/// ring — so the fill stops and reports the patch as closed.
fn measure_patch(
    source: &dyn WorldSource,
    geometry: &RingGeometry,
    origin: (i64, i64),
    target: BiomeId,
) -> Result<(u32, bool), SourceError> {
    let step = geometry.size;
    let mut accepted: HashSet<(i64, i64)> = HashSet::from([origin]);
    // Matched cells per ring, kept until the closure is done so a later
    // ring can still connect cells from earlier ones.
    let mut revealed: Vec<Vec<(i64, i64)>> = vec![vec![origin]];
    for ring in 1..=PATCH_RING_CAP {
        let mut matched: Vec<(i64, i64)> = Vec::new();
        let mut seen = HashSet::new();
        for rect in ring_strip_rects(origin.0, origin.1, ring, step)
            .into_iter()
            .flatten()
        {
            let grid = region_guarded(source, rect, step as u32)?;
            for sample in matched_cells(&grid, step, target) {
                // Clamped world-edge strips can make two strips overlap at
                // a corner; a cell is a cell once.
                if seen.insert(sample) {
                    matched.push(sample);
                }
            }
        }
        if matched.is_empty() {
            // Nothing beyond this ring can be 4-connected to the origin.
            break;
        }
        revealed.push(matched);

        // Accept to a fixpoint over everything revealed so far.
        loop {
            let mut progress = false;
            for ring_cells in &revealed {
                for &cell in ring_cells {
                    if accepted.contains(&cell) {
                        continue;
                    }
                    let neighbors = [
                        (cell.0 - 1, cell.1),
                        (cell.0 + 1, cell.1),
                        (cell.0, cell.1 - 1),
                        (cell.0, cell.1 + 1),
                    ];
                    if neighbors.iter().any(|neighbor| accepted.contains(neighbor)) {
                        accepted.insert(cell);
                        progress = true;
                    }
                }
            }
            if !progress {
                break;
            }
        }
        if accepted.len() >= PATCH_CELL_CAP as usize {
            return Ok((patch_cell_count(&accepted), true));
        }
    }
    let capped = revealed.len() > PATCH_RING_CAP as usize;
    Ok((patch_cell_count(&accepted), capped))
}

/// The accepted-cell count as `u32` — the caps bound it far below overflow.
fn patch_cell_count(accepted: &HashSet<(i64, i64)>) -> u32 {
    u32::try_from(accepted.len()).expect("the caps bound the fill to a few thousand cells")
}

/// The matched cells of one strip grid, as cell indices.
fn matched_cells(grid: &BiomeGrid, step: i64, target: BiomeId) -> impl Iterator<Item = (i64, i64)> {
    let width = grid.width as usize;
    let step_u = i64::from(grid.step);
    let origin_x = i64::from(grid.origin_x);
    let origin_z = i64::from(grid.origin_z);
    grid.biomes
        .iter()
        .enumerate()
        .filter_map(move |(index, biome)| {
            if *biome != target {
                return None;
            }
            let sample_x = origin_x + (index % width) as i64 * step_u;
            let sample_z = origin_z + (index / width) as i64 * step_u;
            Some((sample_x.div_euclid(step), sample_z.div_euclid(step)))
        })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn biome_search_strip_rects_cover_each_ring_once() {
        // Ring 1 around cell (0, 0) at step 16: four thin rects whose
        // lattice points number exactly the ring's 8 cells.
        let step = 16_i64;
        let rects = ring_strip_rects(0, 0, 1, step);
        let points: Vec<(i32, i32)> = rects
            .iter()
            .flatten()
            .flat_map(|rect| {
                let (samples_x, samples_z) = rect.sample_dims(step as u32);
                rect.iter_blocks(step as u32)
                    .take((samples_x * samples_z) as usize)
            })
            .collect();
        assert_eq!(points.len(), 8, "ring 1 has exactly 8 lattice points");
        let unique: HashSet<(i32, i32)> = points.iter().copied().collect();
        assert_eq!(unique.len(), 8, "no lattice point is sampled twice");
        // Ring 0 is the single center point.
        let rects = ring_strip_rects(3, -2, 0, step);
        assert_eq!(
            rects[0],
            Some(Rect::new(
                3 * step as i32,
                -2 * step as i32,
                3 * step as i32,
                -2 * step as i32
            ))
        );
        assert!(rects[1..].iter().all(|rect| rect.is_none()));
    }

    #[test]
    fn biome_search_pending_order_is_total() {
        let near = PendingMatch {
            distance: 5.0,
            position: (0, 0),
            patch_area: None,
            area_capped: false,
        };
        let far = PendingMatch {
            distance: 6.0,
            position: (10, 0),
            patch_area: Some(1_000),
            area_capped: true,
        };
        assert!(near < far);
        let tie_a = PendingMatch {
            distance: 5.0,
            position: (1, 0),
            ..near
        };
        let tie_b = PendingMatch {
            distance: 5.0,
            position: (2, 0),
            ..near
        };
        assert!(tie_a < tie_b, "equal distances break on the position");
    }
}
