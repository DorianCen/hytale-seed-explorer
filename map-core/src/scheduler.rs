//! The background tile pipeline (T15): queueing, prioritization,
//! cancellation and result streaming.
//!
//! The scheduler computes no pixels itself. Each frame the app calls
//! [`TileScheduler::schedule_passes`] with one [`TilePass`] per sampling
//! step (T21's progressive refinement: a coarse fill, then the finer
//! detail) plus the viewport's center tiles; the scheduler queues every
//! tile that is neither cached ([`TileCache`], T14) nor already
//! queued/in-flight, **nearest the center first**. Plain worker threads
//! pop the highest-priority job, run a [`TileRenderer`] with a
//! [`CancelToken`], insert the finished tile into the cache and send a
//! [`TileEvent`] — so scheduling and the app's event drain never block a
//! frame. [`TileScheduler::schedule`] stays as the single-fill-pass
//! shorthand for callers with one step.
//!
//! # Contract
//!
//! - **Truth lives in the cache.** Events are hints for the UI (status
//!   counts, texture uploads); a completed tile is *already* in the
//!   cache when its `Ready` event arrives, and [`TileScheduler::stats`]
//!   / [`TileScheduler::is_pending`] expose what is cached-side for T16's
//!   placeholder painting.
//! - **Dedup is exact.** Scheduling skips cached, queued and in-flight
//!   tiles, so no tile is rendered twice concurrently. A queued job
//!   whose tile scrolled away and came back is *revived* (its token is
//!   cleared) rather than duplicated.
//! - **Refinement never starves the fill (T21).** A pass marked
//!   [`TilePass::refine`] queues every job at
//!   `chebyshev(coord, center) + REFINE_PENALTY` ([`REFINE_PENALTY`]), far
//!   above any distance a real viewport produces, so every fill job pops
//!   before every refine job. The coarse pass therefore covers the view
//!   first and the detail only starts afterwards — on any worker count.
//! - **Cancellation is aggressive (v1).** [`TileScheduler::cancel_outside`]
//!   sets the token of every queued/in-flight tile outside the view;
//!   [`TileScheduler::cancel_not_serving`] does the same for tiles no
//!   pass claims (subject or position). Renderers check tokens between
//!   work units and return [`SourceError::Cancelled`]. A tile that
//!   finishes despite its token is cached (its pixels are valid and
//!   keyed) but reported as `Cancelled`, so the app does not upload a
//!   texture for a view that moved on. T33 revisits the "almost done,
//!   keep it" policy.
//! - **Panic containment.** A renderer panic is caught at the worker
//!   boundary and reported as `Failed(key, SourceError::Generation)`;
//!   the worker lives on. Like the worldgen loader boundary (T08), this
//!   is a documented style-guide deviation: the alternative is losing
//!   the whole pipeline to one hostile tile.
//!
//! # Threading shape (per the task's risk note)
//!
//! Deliberately *not* rayon: the generator already owns worker pools, a
//! parallel iterator whose items block on channels can deadlock
//! work-stealing, and plain threads + a priority heap make ownership and
//! shutdown obvious. Workers wait on a condvar when the queue is empty;
//! [`Drop`] flags shutdown, cancels every token and joins.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::coords::{TileCoord, TileRect, ZoomLevel};
use crate::error::{SourceError, SourceResult, panic_message};
use crate::render::Layer;
use crate::tiles::{Tile, TileCache, TileKey};

/// A shared cancellation flag. Structurally identical to the worldgen
/// pool's token (T08), so renderer implementations can pass it straight
/// through to `WorldSource` region calls.
pub type CancelToken = Arc<AtomicBool>;

/// A fresh, uncancelled token.
#[must_use]
pub fn cancel_token() -> CancelToken {
    Arc::new(AtomicBool::new(false))
}

// ---------------------------------------------------------------------------
// What to render
// ---------------------------------------------------------------------------

/// The "what" of a tile request: every key field except the tile's
/// position. A [`TilePass`] copies it across that pass's visible range,
/// filling in `(tx, ty)` per tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileSubject {
    /// Stable hash of the generator identity (T07/T14).
    pub identity_hash: u64,
    /// The generator-visible seed (low 32 bits, T02).
    pub generator_seed: i32,
    /// The layer to render.
    pub layer: Layer,
    /// Layer-specific mode (e.g. a cave Y bucket); 0 when unused.
    pub mode: u16,
    /// The zoom level whose step samples the tile.
    pub zoom: ZoomLevel,
}

impl TileSubject {
    /// A subject for the given identity, seed, layer/mode and zoom.
    #[must_use]
    pub fn new(
        identity_hash: u64,
        generator_seed: i32,
        layer: Layer,
        mode: u16,
        zoom: ZoomLevel,
    ) -> Self {
        Self {
            identity_hash,
            generator_seed,
            layer,
            mode,
            zoom,
        }
    }

    /// The subject of an existing key, without its position.
    #[must_use]
    pub fn from_key(key: &TileKey) -> Self {
        Self::new(
            key.identity_hash,
            key.generator_seed,
            key.layer,
            key.mode,
            key.zoom,
        )
    }

    /// The key for this subject at tile position `(tx, ty)`.
    #[must_use]
    pub fn key_at(&self, tx: i32, ty: i32) -> TileKey {
        TileKey {
            identity_hash: self.identity_hash,
            generator_seed: self.generator_seed,
            layer: self.layer,
            mode: self.mode,
            zoom: self.zoom,
            tx,
            ty,
        }
    }
}

/// How far a refinement job sits behind the fill jobs of its frame:
/// comfortably above any Chebyshev distance a viewport can produce
/// (the visible range spans tens of tiles), so EVERY fill job pops
/// before EVERY refine job regardless of position.
pub const REFINE_PENALTY: u64 = 1_000_000_000;

/// One pass of a frame's tile request (T21's progressive refinement):
/// the enabled layers' subjects at ONE sampling step, over that step's
/// visible range. A `refine` pass queues behind the fill passes — its
/// tiles are the progressive detail, not the first coverage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TilePass {
    /// The layers to render at this pass's step, bottom → top paint order.
    pub subjects: Vec<TileSubject>,
    /// The pass's visible tile range, in the subjects' zoom step.
    pub visible: TileRect,
    /// Priority origin for this pass (its viewport center tile).
    pub center: TileCoord,
    /// True for a refinement pass (priority-penalized).
    pub refine: bool,
}

impl TilePass {
    /// A fill pass at its natural nearest-first priority.
    #[must_use]
    pub fn fill(subjects: Vec<TileSubject>, visible: TileRect, center: TileCoord) -> Self {
        Self {
            subjects,
            visible,
            center,
            refine: false,
        }
    }

    /// A refinement pass: same shape, flagged for the priority penalty.
    #[must_use]
    pub fn refine(subjects: Vec<TileSubject>, visible: TileRect, center: TileCoord) -> Self {
        Self {
            subjects,
            visible,
            center,
            refine: true,
        }
    }

    /// The heap penalty this pass's jobs carry: the refine offset for
    /// progressive detail, zero for fill.
    fn penalty(&self) -> u64 {
        if self.refine { REFINE_PENALTY } else { 0 }
    }
}

/// Renders one tile. The scheduler is layer-agnostic; the biome, height,
/// relief, surface and cave layers provide implementations (T13/T17/T20/T28).
///
/// Implementations must honor `cancel` between work units and return
/// [`SourceError::Cancelled`] promptly; the world rect for `key` is
/// `tile_rect(TileCoord::new(key.tx.into(), key.ty.into()),
/// key.zoom.step())` (T10), so no rect is passed here.
pub trait TileRenderer: Send + Sync {
    /// Renders the tile for `key`, or the typed failure.
    fn render(&self, key: &TileKey, cancel: &CancelToken) -> SourceResult<Tile>;
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// One scheduler notification for the UI thread.
///
/// Events are *hints*: `Ready` tiles are already cached; the cache plus
/// [`TileScheduler::is_pending`] are the state of record.
#[derive(Debug, Clone, PartialEq)]
pub enum TileEvent {
    /// A worker picked the tile up (for "generating N tiles" status).
    Started(TileKey),
    /// The tile finished rendering and is in the cache. `generation` is
    /// the request generation the tile was queued under; the app may
    /// drop events from generations older than its current view.
    Ready { tile: Arc<Tile>, generation: u64 },
    /// The render failed (panics included); the tile is no longer pending.
    Failed(TileKey, SourceError),
    /// Cancelled: the tile left the view (or shut down) before
    /// completing. Queued jobs that were never started die silently —
    /// this variant only fires for tiles that did start.
    Cancelled(TileKey),
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// A snapshot of scheduler activity — the instrumentation hook for T33.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SchedulerStats {
    /// Jobs waiting for a worker right now.
    pub queued: u64,
    /// Jobs being rendered right now.
    pub in_flight: u64,
    /// Tiles completed and cached (delivered or superseded).
    pub completed: u64,
    /// Tiles cancelled — in-flight renders that saw their token, jobs
    /// dropped from the queue, and completions after a cancel.
    pub cancelled: u64,
    /// Tiles that failed, including contained renderer panics.
    pub failed: u64,
    /// Total render time across completed and failed tiles.
    pub render_nanos: u64,
    /// How many times the visible request changed (pans/zooms).
    pub generations: u64,
}

// ---------------------------------------------------------------------------
// The scheduler
// ---------------------------------------------------------------------------

/// The background tile pipeline: a priority queue, plain worker threads,
/// exact dedup and aggressive cancellation.
///
/// Construct with [`TileScheduler::new`] (workers + the shared
/// [`TileCache`]); call [`TileScheduler::schedule_passes`] each frame
/// (or [`TileScheduler::schedule`] for a single fill pass) and drain
/// [`TileScheduler::events`] non-blockingly. `Drop` shuts the workers
/// down and joins them.
pub struct TileScheduler {
    /// Completed tiles land here; `schedule` dedups against it.
    cache: Arc<TileCache>,
    /// Worker-shared state, woken by `cond`.
    shared: Arc<Mutex<Shared>>,
    cond: Arc<Condvar>,
    /// Worker handles, joined by `Drop`.
    workers: Vec<JoinHandle<()>>,
    /// The event stream receiver; `events()` hands out clones (one
    /// consumer — the app — is the intended use; clones compete).
    events: Receiver<TileEvent>,
}

/// Everything behind the scheduler lock.
struct Shared {
    /// Jobs waiting for a worker, nearest-center first.
    queue: BinaryHeap<TileRequest>,
    /// Keys currently in `queue`, with their tokens (dedup + revocation).
    queued: HashMap<TileKey, CancelToken>,
    /// Keys currently being rendered, with their tokens.
    in_flight: HashMap<TileKey, CancelToken>,
    /// The latest renderer `schedule` was given; workers always use it.
    renderer: Option<Arc<dyn TileRenderer>>,
    /// The request generation; bumped whenever the visible request
    /// changes so the app can drop stale events.
    generation: u64,
    /// Monotonic job sequence (FIFO tie-breaker in the heap).
    next_seq: u64,
    /// The last request — every pass the frame asked for — for change
    /// detection.
    last_request: Option<Vec<TilePass>>,
    completed: u64,
    cancelled: u64,
    failed: u64,
    render_nanos: u64,
    shutdown: bool,
}

/// One queued job (the task's `TileRequest`). The world rect is derived
/// from the key at render time, not stored.
struct TileRequest {
    key: TileKey,
    /// Chebyshev tile distance to the viewport center; lower = nearer.
    priority: u64,
    /// Request sequence; FIFO among equal priorities.
    seq: u64,
    /// The request generation this job was queued under.
    generation: u64,
    cancel: CancelToken,
}

impl Ord for TileRequest {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap pops the *greatest*: nearest (lowest distance)
        // first, then the earliest request among equals.
        other
            .priority
            .cmp(&self.priority)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

impl PartialOrd for TileRequest {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for TileRequest {
    fn eq(&self, other: &Self) -> bool {
        // `seq` is unique per job, so this stays consistent with `cmp`.
        self.seq == other.seq
    }
}

impl Eq for TileRequest {}

impl TileScheduler {
    /// Spawns `workers` plain threads over the shared cache.
    ///
    /// See [`TileScheduler::default_workers`] for the plan's default
    /// pool size; the scheduler pool is intentionally small — the
    /// generator source runs workers of its own (the task's
    /// oversubscription risk).
    ///
    /// # Panics
    ///
    /// Panics when `workers` is zero: a scheduler without a worker can
    /// never make progress, a bug in the caller.
    pub fn new(workers: usize, cache: Arc<TileCache>) -> Self {
        assert!(workers >= 1, "the tile scheduler needs at least one worker");
        let (sender, events) = unbounded();
        let shared = Arc::new(Mutex::new(Shared {
            queue: BinaryHeap::new(),
            queued: HashMap::new(),
            in_flight: HashMap::new(),
            renderer: None,
            generation: 0,
            next_seq: 0,
            last_request: None,
            completed: 0,
            cancelled: 0,
            failed: 0,
            render_nanos: 0,
            shutdown: false,
        }));
        let cond = Arc::new(Condvar::new());

        let worker_handles = (0..workers)
            .map(|index| {
                spawn_worker(
                    index,
                    Arc::clone(&shared),
                    Arc::clone(&cond),
                    Arc::clone(&cache),
                    sender.clone(),
                )
            })
            .collect();
        TileScheduler {
            cache,
            shared,
            cond,
            workers: worker_handles,
            events,
        }
    }

    /// The plan's default pool size (start small, tune in T33):
    /// `min(4, available_parallelism)`, or 2 when unknown.
    #[must_use]
    pub fn default_workers() -> usize {
        std::thread::available_parallelism()
            .map_or(2, |cpus| cpus.get())
            .min(4)
    }

    /// Queues every visible tile of every subject of every pass that is
    /// neither cached nor pending, nearest each pass's `center` first.
    /// Cheap by construction: heap inserts under one short lock, never a
    /// render.
    ///
    /// T21's contract: a frame asks for several sampling steps at once —
    /// typically a coarse [`TilePass::fill`] that covers the viewport
    /// immediately and a finer [`TilePass::refine`] that paints the
    /// detail. Each pass carries its own `subjects`, `visible` (tile-space
    /// of *that* pass's zoom step, T10's `visible_tiles`) and `center`;
    /// layers inside a pass share the pass's range and priority origin.
    /// Refine jobs queue [`REFINE_PENALTY`] behind fill jobs, so coarse
    /// coverage always lands before any detail work is popped. The latest
    /// `renderer` wins (one renderer per source in practice).
    ///
    /// Returns the request generation: 1 for the first request, bumped
    /// whenever the pass list changes (order included — passes carry the
    /// paint-relevant subject order); events carry their job's generation
    /// so the app can drop stale ones. Repeating an unchanged request
    /// re-dedups against the cache and revives queued-but-cancelled tiles
    /// without new jobs.
    pub fn schedule_passes(&self, passes: &[TilePass], renderer: Arc<dyn TileRenderer>) -> u64 {
        let mut guard = lock(&self.shared);
        guard.renderer = Some(renderer);
        let changed = guard
            .last_request
            .as_ref()
            .is_none_or(|last| last.as_slice() != passes);
        if changed {
            guard.generation += 1;
        }
        guard.last_request = Some(passes.to_vec());
        let generation = guard.generation;

        for pass in passes {
            let penalty = pass.penalty();
            for subject in &pass.subjects {
                for coord in pass.visible.iter() {
                    // Tile keys carry i32 indices; a visible range beyond
                    // them cannot be keyed, so those coordinates are
                    // skipped (the reachable world is far inside, per
                    // T10's bounds analysis).
                    let (Ok(tx), Ok(ty)) = (i32::try_from(coord.x), i32::try_from(coord.z)) else {
                        continue;
                    };
                    let key = subject.key_at(tx, ty);
                    if self.cache.contains(&key) {
                        continue;
                    }
                    if let Some(token) = guard.queued.get(&key) {
                        // Already queued: revive it if it was cancelled (the
                        // tile scrolled away and came back) instead of piling
                        // a second job on. Its stale generation rides along;
                        // the cache is the truth when the event lands.
                        token.store(false, Relaxed);
                        continue;
                    }
                    if guard.in_flight.contains_key(&key) {
                        continue;
                    }
                    // Saturating: only an absurd center could sit near
                    // `u64::MAX` away, and wrapping there would let a
                    // refine job outrank fill jobs.
                    let request = TileRequest {
                        key,
                        priority: chebyshev(coord, pass.center).saturating_add(penalty),
                        seq: guard.next_seq,
                        generation,
                        cancel: cancel_token(),
                    };
                    guard.next_seq += 1;
                    guard.queued.insert(key, Arc::clone(&request.cancel));
                    guard.queue.push(request);
                }
            }
        }
        drop(guard);
        self.cond.notify_all();
        generation
    }

    /// Single-fill-pass shorthand for [`TileScheduler::schedule_passes`]:
    /// one [`TilePass::fill`] over `subjects`, `visible` and `center`.
    ///
    /// `subjects` is the frame's enabled layer set, bottom → top paint
    /// order (T19); every subject shares `visible` (one step serves all
    /// layers). Callers with progressive steps use
    /// [`TileScheduler::schedule_passes`] instead.
    pub fn schedule(
        &self,
        subjects: &[TileSubject],
        visible: TileRect,
        center: TileCoord,
        renderer: Arc<dyn TileRenderer>,
    ) -> u64 {
        let pass = TilePass::fill(subjects.to_vec(), visible, center);
        self.schedule_passes(&[pass], renderer)
    }

    /// Cancels every queued and in-flight tile the frame no longer
    /// serves: no pass claims it — either its subject left every
    /// request, or its position is outside every pass's visible range
    /// (each pass steps the visible range differently, so the union is
    /// the serving set). T19's disabled layers and T21's dropped
    /// refinement both stop this way. Cheap: token writes only.
    ///
    /// Subsumes [`TileScheduler::cancel_outside`]: empty `passes` cancels
    /// everything pending. A tile that returns to the request is
    /// *revived* by the next schedule; a tile that finishes despite its
    /// token stays cached, so a re-enabled layer serves instantly.
    pub fn cancel_not_serving(&self, passes: &[TilePass]) {
        let guard = lock(&self.shared);
        let serving = |key: &TileKey| {
            let coord = key_coord(key);
            let subject = TileSubject::from_key(key);
            passes
                .iter()
                .any(|pass| pass.visible.contains(coord) && pass.subjects.contains(&subject))
        };
        for (key, token) in &guard.queued {
            if !serving(key) {
                token.store(true, Relaxed);
            }
        }
        for (key, token) in &guard.in_flight {
            if !serving(key) {
                token.store(true, Relaxed);
            }
        }
    }

    /// Cancels every queued and in-flight tile outside `visible` (the
    /// plan's aggressive v1 policy; T33 tunes). Cheap: token writes only.
    ///
    /// Queued jobs die lazily when a worker pops them; in-flight renders
    /// see the token at their next check and report `Cancelled`.
    pub fn cancel_outside(&self, visible: &TileRect) {
        let guard = lock(&self.shared);
        for (key, token) in &guard.queued {
            if !visible.contains(key_coord(key)) {
                token.store(true, Relaxed);
            }
        }
        for (key, token) in &guard.in_flight {
            if !visible.contains(key_coord(key)) {
                token.store(true, Relaxed);
            }
        }
    }

    /// The event stream: one clone per consumer (clones compete, so one
    /// consumer — the app — is the intended use). Never blocks; drain
    /// with `try_iter`/`try_recv`.
    #[must_use]
    pub fn events(&self) -> Receiver<TileEvent> {
        self.events.clone()
    }

    /// Whether `key` is queued or in-flight — T16's placeholder hook.
    #[must_use]
    pub fn is_pending(&self, key: &TileKey) -> bool {
        let guard = lock(&self.shared);
        guard.queued.contains_key(key) || guard.in_flight.contains_key(key)
    }

    /// A snapshot of scheduler activity (the T33 instrumentation hook).
    #[must_use]
    pub fn stats(&self) -> SchedulerStats {
        let guard = lock(&self.shared);
        SchedulerStats {
            queued: guard.queued.len() as u64,
            in_flight: guard.in_flight.len() as u64,
            completed: guard.completed,
            cancelled: guard.cancelled,
            failed: guard.failed,
            render_nanos: guard.render_nanos,
            generations: guard.generation,
        }
    }
}

impl Drop for TileScheduler {
    fn drop(&mut self) {
        {
            let mut guard = lock(&self.shared);
            guard.shutdown = true;
            // Cancel everything so well-behaved renderers return
            // promptly instead of dragging out the join.
            for token in guard.queued.values() {
                token.store(true, Relaxed);
            }
            for token in guard.in_flight.values() {
                token.store(true, Relaxed);
            }
        }
        self.cond.notify_all();
        // Workers check `shutdown` before popping and exit; the lock is
        // free, so nothing here can hang on the join.
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Workers
// ---------------------------------------------------------------------------

/// Spawns one worker thread (named for stack traces / debugging).
fn spawn_worker(
    index: usize,
    shared: Arc<Mutex<Shared>>,
    cond: Arc<Condvar>,
    cache: Arc<TileCache>,
    events: Sender<TileEvent>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("tile-worker-{index}"))
        .spawn(move || worker_main(shared, cond, cache, events))
        .expect("spawning a plain worker thread cannot fail at this size")
}

/// The worker loop: pop the nearest job, render, cache, report.
fn worker_main(
    shared: Arc<Mutex<Shared>>,
    cond: Arc<Condvar>,
    cache: Arc<TileCache>,
    events: Sender<TileEvent>,
) {
    loop {
        // -- acquire a job (or exit) ----------------------------------------
        let (job, renderer) = {
            let mut guard = lock(&shared);
            loop {
                if guard.shutdown {
                    return;
                }
                if let Some(job) = guard.queue.pop() {
                    guard.queued.remove(&job.key);
                    // Lazily drop jobs cancelled while queued; they never
                    // started, so they die without an event.
                    if job.cancel.load(Relaxed) {
                        guard.cancelled += 1;
                        continue;
                    }
                    guard.in_flight.insert(job.key, Arc::clone(&job.cancel));
                    let renderer = Arc::clone(
                        guard
                            .renderer
                            .as_ref()
                            .expect("a queued job implies schedule() set a renderer first"),
                    );
                    break (job, renderer);
                }
                guard = wait(&cond, guard);
            }
        };

        let _ = events.send(TileEvent::Started(job.key));
        let render_started = Instant::now();
        // Style-guide deviation (documented, same argument as the T08
        // loader boundary): the renderer is foreign code and must not be
        // allowed to kill the pipeline's worker thread.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            renderer.render(&job.key, &job.cancel)
        }));
        let render_nanos = u64::try_from(render_started.elapsed().as_nanos()).unwrap_or(u64::MAX);

        // -- publish ---------------------------------------------------------
        let event = {
            let mut guard = lock(&shared);
            guard.in_flight.remove(&job.key);
            guard.render_nanos = guard.render_nanos.saturating_add(render_nanos);
            match result {
                Ok(Ok(tile)) => {
                    // The pixels are valid and content-addressed, so the
                    // cache takes them even under cancellation; the
                    // event tells the app what its *current* view wants.
                    let tile = Arc::new(tile);
                    cache.insert(Arc::clone(&tile));
                    if job.cancel.load(Relaxed) {
                        guard.cancelled += 1;
                        TileEvent::Cancelled(job.key)
                    } else {
                        guard.completed += 1;
                        TileEvent::Ready {
                            tile,
                            generation: job.generation,
                        }
                    }
                }
                Ok(Err(SourceError::Cancelled)) => {
                    guard.cancelled += 1;
                    TileEvent::Cancelled(job.key)
                }
                Ok(Err(error)) => {
                    guard.failed += 1;
                    TileEvent::Failed(job.key, error)
                }
                Err(panic) => {
                    guard.failed += 1;
                    TileEvent::Failed(job.key, SourceError::Generation(panic_message(panic)))
                }
            }
        };
        // Send outside the lock; a dropped receiver just fails the send,
        // which is fine (nobody is listening).
        let _ = events.send(event);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The priority key: Chebyshev (king-move) tile distance to the
/// viewport center. `unsigned_abs` makes the `i64::MIN` edge impossible
/// to trip over.
fn chebyshev(tile: TileCoord, center: TileCoord) -> u64 {
    let dx = tile.x.wrapping_sub(center.x).unsigned_abs();
    let dz = tile.z.wrapping_sub(center.z).unsigned_abs();
    dx.max(dz)
}

/// The tile-space coordinate of a key, for range membership.
fn key_coord(key: &TileKey) -> TileCoord {
    TileCoord::new(i64::from(key.tx), i64::from(key.ty))
}

/// Locks a scheduler mutex; poisoning is unreachable (no critical
/// section here can panic), and recovering keeps a failed *test* from
/// cascading into its neighbors.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Waits on the workers' condvar, with the same poisoning recovery as
/// [`lock`].
fn wait<'a, T>(cond: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    cond.wait(guard)
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduler_heap_orders_nearest_first_then_fifo() {
        // A center plus two rings of neighbors around it; pushed out of
        // order, popped nearest-first with FIFO among equals.
        let center = TileCoord::new(0, 0);
        let mut heap = BinaryHeap::new();
        let mut push = |tx: i32, ty: i32, seq: u64| {
            let coord = TileCoord::new(i64::from(tx), i64::from(ty));
            heap.push(TileRequest {
                key: TileSubject::new(1, 7, Layer::Biome, 0, ZoomLevel::MIN).key_at(tx, ty),
                priority: chebyshev(coord, center),
                seq,
                generation: 1,
                cancel: cancel_token(),
            });
        };

        // Ring 2 first, then ring 1, then the center; each ring shuffled.
        push(2, 2, 7);
        push(-2, 0, 8);
        push(0, 1, 3);
        push(1, 0, 2);
        push(0, 0, 1);

        let popped = |heap: &mut BinaryHeap<TileRequest>| {
            heap.pop()
                .map(|job| (job.priority, job.seq))
                .expect("every push is poppable")
        };
        // Nearest first...
        assert_eq!(popped(&mut heap), (0, 1), "the center tile");
        // ...then ring 1 in request order (FIFO among equal priorities).
        assert_eq!(popped(&mut heap), (1, 2));
        assert_eq!(popped(&mut heap), (1, 3));
        // ...then ring 2 in request order.
        assert_eq!(popped(&mut heap), (2, 7));
        assert_eq!(popped(&mut heap), (2, 8));
        assert!(heap.is_empty());
    }

    #[test]
    fn scheduler_subject_fills_only_the_tile_position() {
        let subject = TileSubject::new(99, 7, Layer::CaveSlice, 64, ZoomLevel::MIN);
        let key = subject.key_at(-2, -3);
        assert_eq!(key.identity_hash, 99);
        assert_eq!(key.generator_seed, 7);
        assert_eq!(key.layer, Layer::CaveSlice);
        assert_eq!(key.mode, 64);
        assert_eq!(key.zoom, ZoomLevel::MIN);
        assert_eq!((key.tx, key.ty), (-2, -3), "only the position moved");

        // The round trip: a key's subject reconstructs its non-position
        // fields exactly.
        assert_eq!(TileSubject::from_key(&key), subject);
        // Two positions of the same subject stay distinct keys.
        assert_ne!(key, subject.key_at(-2, -2));
    }

    #[test]
    fn scheduler_chebyshev_distance_matches_hand_computed_values() {
        let center = TileCoord::new(-1203, 455);
        assert_eq!(chebyshev(TileCoord::new(-1203, 455), center), 0);
        assert_eq!(chebyshev(TileCoord::new(-1202, 455), center), 1);
        // King-move: a diagonal neighbor is as near as an orthogonal one.
        assert_eq!(chebyshev(TileCoord::new(-1204, 456), center), 1);
        assert_eq!(chebyshev(TileCoord::new(0, 0), center), 1203);
        // dx = |-1_000_000 + 1203| = 998_797, dz = |1_000_000 - 455| = 999_545.
        assert_eq!(
            chebyshev(TileCoord::new(-1_000_000, 1_000_000), center),
            999_545
        );
    }
}
