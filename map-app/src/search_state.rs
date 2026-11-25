//! Headless search state for the left-panel search (T26): the form's request,
//! the engine lifecycle, per-frame event draining, result ordering/dedupe and
//! the click/flash state — all without egui, a window or a generator.
//!
//! # Engines, not threads, in the state machine
//!
//! [`SearchState`] talks to a [`SearchDriver`] trait. The production driver
//! ([`RealSearchDriver`]) owns one worker thread per search and drives T24's
//! and T25's blocking engines; tests substitute a fake driver that hands out
//! a channel and records cancellation. The state never touches the source on
//! the UI thread.
//!
//! # Generation guard
//!
//! Every start bumps a generation. The active [`SearchSession`] carries the
//! generation it was spawned under, and [`SearchState::apply_event`] ignores
//! events from any other generation. Starting a new search drops the old
//! session first (which cancels its worker), so results never mix.
//!
//! # Frame budget
//!
//! [`SearchState::poll`] drains at most [`EVENTS_PER_FRAME`] events. The
//! engines' bounded channels provide the backpressure; progress events are
//! dropped under pressure and results block the worker rather than queueing
//! without bound (the T24/T25 contract).

use std::cmp::Ordering;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded};

use map_core::scheduler::{CancelToken, cancel_token};
use map_core::search::biome::{
    BiomeMatch, BiomeSearch, BiomeSearchEvent, BiomeSearchProgress, run_biome_search_blocking,
};
use map_core::search::feature::{
    FeatureSearch, SEARCH_EVENT_CAPACITY, SearchEvent as FeatureEvent, SearchFilters,
    SearchProgress, SearchResult, run_feature_search_blocking,
};
use map_core::source::FeatureSource;
use map_core::source::WorldSource;
use map_core::types::{BiomeId, FeatureFilter, FeatureKind};

/// The radius choices the panel offers, in blocks (the task's 1k/5k/10k/25k).
pub const RADIUS_CHOICES: [f64; 4] = [1_000.0, 5_000.0, 10_000.0, 25_000.0];

/// The biome search's coarse sampling steps the panel offers.
pub const COARSE_STEPS: [u32; 3] = [8, 16, 32];

/// How many events one frame may drain (the task's 16 ms guard).
pub const EVENTS_PER_FRAME: usize = 64;

/// The feature-search region edges for the UI (T26).
///
/// The engines' default 512-block region is a benchmark tradeoff: a
/// biome-prefab region can run for tens of seconds, during which a cancel
/// cannot take effect (the engine checks between regions). Expensive kinds
/// (biome prefabs, cave entrances) therefore use 128-block regions; the cheap
/// kinds use 2048-block regions so a 10k search is ~100 pool round trips
/// instead of tens of thousands (each `WorldgenSource` region is a pool job).
const UI_SMALL_REGION_SIZE: u32 = 128;
const UI_CHEAP_REGION_SIZE: u32 = 2_048;

/// The default result cap; `0` would mean unlimited to the engines, so the
/// UI never offers it.
pub const DEFAULT_MAX_RESULTS: usize = 200;

/// How long a clicked result keeps its flash ring.
pub const FLASH_DURATION: Duration = Duration::from_secs(1);

/// What the search looks for.
#[derive(Debug, Clone, PartialEq)]
pub enum SearchTarget {
    /// Features matching the filter (kind mask plus optional category/name/
    /// size filters).
    Features(FeatureFilter),
    /// One biome.
    Biome(BiomeId),
}

/// One search run's parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchRequest {
    /// What to find.
    pub target: SearchTarget,
    /// World XZ center in blocks.
    pub center: (f64, f64),
    /// Maximum distance in blocks.
    pub max_radius: f64,
    /// Result cap; the panel keeps this at least one.
    pub max_results: usize,
    /// Feature-search proximity filter ("within N blocks of another
    /// feature"), if enabled.
    pub proximity: Option<map_core::search::feature::ProximityFilter>,
    /// Biome coarse step (ignored by feature searches).
    pub coarse_step: u32,
    /// Biome patch-area minimum in blocks² (ignored by feature searches).
    pub min_patch_blocks: Option<u32>,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            target: SearchTarget::Features(FeatureFilter {
                kinds: map_core::types::FeatureKindMask::ALL,
                categories: None,
                name_contains: None,
                min_size: None,
            }),
            center: (0.0, 0.0),
            max_radius: 10_000.0,
            max_results: DEFAULT_MAX_RESULTS,
            proximity: None,
            coarse_step: map_core::search::biome::DEFAULT_COARSE_STEP,
            min_patch_blocks: None,
        }
    }
}

/// A source the state may search. Both variants come from one
/// `WorldgenSource` in production; the split is what lets the two engines
/// take their own trait object.
pub enum SearchSource {
    /// Feature placement queries (T24).
    Features(Arc<dyn FeatureSource>),
    /// Biome column queries (T25).
    Biomes(Arc<dyn WorldSource>),
}

/// The lifecycle the panel displays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchStatus {
    /// No search has run, or the last one was cleared.
    #[default]
    Idle,
    /// A search is streaming events.
    Running,
    /// A search ended normally (including a cancelled one).
    Done,
    /// The search failed (source error or a driver/worker failure).
    Failed,
}

impl SearchStatus {
    /// Whether a search is in flight.
    #[must_use]
    pub fn is_running(self) -> bool {
        matches!(self, SearchStatus::Running)
    }
}

/// Which engine produced a hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitKind {
    /// A feature placement (T24).
    Feature(FeatureKind),
    /// A biome match (T25).
    Biome(BiomeId),
}

/// The dedupe identity of a hit.
///
/// Features use the T23 catalog identity `(kind, name, position)`, so a
/// feature returned by overlapping rings appears once. Biome matches have no
/// name of their own; the block position is their identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SearchHitKey {
    /// `(kind, name, position)`.
    Feature {
        /// Feature kind.
        kind: FeatureKind,
        /// Feature name.
        name: String,
        /// Anchor position.
        position: (i32, i32, i32),
    },
    /// `(x, z)` of the refined block.
    Biome { x: i32, z: i32 },
}

/// One row of the results table.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// Dedupe identity.
    pub key: SearchHitKey,
    /// Which engine/kind produced it.
    pub kind: HitKind,
    /// Display name (feature name, or `Biome #id` until the panel resolves
    /// the catalog name).
    pub name: String,
    /// Feature category / zone name, or `"Biome"`.
    pub category: String,
    /// Block x.
    pub x: i32,
    /// Block y when the engine knows it (features do, biome matches do not).
    pub y: Option<i32>,
    /// Block z.
    pub z: i32,
    /// Distance from the request center in blocks.
    pub distance: f64,
    /// The shared teleport string; biome matches use `~` for the unknown Y.
    pub teleport: String,
    /// Biome patch estimate, when requested.
    pub patch_area: Option<u64>,
    /// Whether the patch estimate hit a cap (a lower bound).
    pub area_capped: bool,
}

impl SearchHit {
    /// Builds a feature hit from T24's streamed result.
    #[must_use]
    pub fn from_feature(result: SearchResult) -> Self {
        let feature = result.feature;
        Self {
            key: SearchHitKey::Feature {
                kind: feature.kind,
                name: feature.name.clone(),
                position: feature.position,
            },
            kind: HitKind::Feature(feature.kind),
            name: feature.name,
            category: feature.category,
            x: feature.position.0,
            y: Some(feature.position.1),
            z: feature.position.2,
            distance: result.distance,
            teleport: result.teleport,
            patch_area: None,
            area_capped: false,
        }
    }

    /// Builds a biome hit from T25's streamed match.
    #[must_use]
    pub fn from_biome(matched: BiomeMatch, biome: BiomeId) -> Self {
        Self {
            key: SearchHitKey::Biome {
                x: matched.position.0,
                z: matched.position.1,
            },
            kind: HitKind::Biome(biome),
            name: format!("Biome #{}", biome.0),
            category: "Biome".to_string(),
            x: matched.position.0,
            y: None,
            z: matched.position.1,
            distance: matched.distance,
            // The biome engine finds a column, not a height; `~` keeps the
            // copied command valid in the game without inventing a Y.
            teleport: format!("/tp {} ~ {}", matched.position.0, matched.position.1),
            patch_area: matched.patch_area,
            area_capped: matched.area_capped,
        }
    }
}

/// A heartbeat from either engine, normalized for the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchProgressView {
    /// Ring (feature) or ring-with-strips (biome) just worked on.
    pub ring: u32,
    /// Regions queried (features) or coarse samples taken (biomes).
    pub work: u64,
    /// Results emitted so far.
    pub results: usize,
}

/// How a finished search ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchOutcome {
    /// Results emitted.
    pub results: usize,
    /// Regions queried (features) or coarse samples taken (biomes).
    pub work: u64,
    /// Rings fully scanned.
    pub rings: u32,
    /// The result cap cut the scan short.
    pub truncated: bool,
    /// Cancellation stopped the search.
    pub cancelled: bool,
    /// Engine wall time.
    pub elapsed: Duration,
}

/// One event a driver streams to the state.
#[derive(Debug, Clone, PartialEq)]
pub enum SearchEvent {
    /// The worker started.
    Started,
    /// One result, nearest-first by contract.
    Hit(SearchHit),
    /// A progress heartbeat.
    Progress(SearchProgressView),
    /// The search ended.
    Done(SearchOutcome),
    /// The source failed; nothing follows.
    Error(String),
}

/// One spawned search: its event stream, generation and cancel flag.
pub struct SearchSession {
    generation: u64,
    receiver: Receiver<SearchEvent>,
    cancel: CancelToken,
}

impl SearchSession {
    /// Wraps a driver's channel. The generation is echoed by
    /// [`SearchState::apply_event`]'s guard.
    #[must_use]
    pub fn new(
        generation: u64,
        receiver: Receiver<SearchEvent>,
        cancel: CancelToken,
    ) -> SearchSession {
        SearchSession {
            generation,
            receiver,
            cancel,
        }
    }

    /// The generation this session was started under.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Non-blocking receive.
    pub fn try_recv(&self) -> Result<SearchEvent, TryRecvError> {
        self.receiver.try_recv()
    }

    /// Requests cancellation from the worker.
    pub fn cancel(&self) {
        self.cancel.store(true, AtomicOrdering::Relaxed);
    }
}

impl Drop for SearchSession {
    fn drop(&mut self) {
        // Dropping the session closes the receiver; the worker's next send
        // fails and the engine stops. Cancelling first also stops a worker
        // blocked inside a long region query at its next boundary.
        self.cancel();
    }
}

/// The seam between the state machine and the two engines.
pub trait SearchDriver: Send + Sync {
    /// Spawns a feature search over `source`.
    fn start_feature(
        &self,
        generation: u64,
        source: Arc<dyn FeatureSource>,
        search: FeatureSearch,
    ) -> SearchSession;

    /// Spawns a biome search over `source`.
    fn start_biome(
        &self,
        generation: u64,
        source: Arc<dyn WorldSource>,
        search: BiomeSearch,
    ) -> SearchSession;
}

/// The production driver: one worker thread per search, running T24's/T25's
/// blocking engines and forwarding normalized events.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealSearchDriver;

impl SearchDriver for RealSearchDriver {
    fn start_feature(
        &self,
        generation: u64,
        source: Arc<dyn FeatureSource>,
        search: FeatureSearch,
    ) -> SearchSession {
        let (sender, receiver) = bounded(SEARCH_EVENT_CAPACITY);
        let cancel = cancel_token();
        let worker_cancel = Arc::clone(&cancel);
        thread::Builder::new()
            .name("feature-search-ui".to_string())
            .spawn(move || {
                run_feature_search_blocking(&*source, &search, &worker_cancel, |event| {
                    let event = match event {
                        FeatureEvent::Started => SearchEvent::Started,
                        FeatureEvent::Result(result) => {
                            SearchEvent::Hit(SearchHit::from_feature(result))
                        }
                        FeatureEvent::Progress(progress) => {
                            SearchEvent::Progress(feature_progress(progress))
                        }
                        FeatureEvent::Done(stats) => SearchEvent::Done(SearchOutcome {
                            results: stats.results,
                            work: stats.regions_queried,
                            rings: stats.rings_scanned,
                            truncated: stats.truncated,
                            cancelled: stats.cancelled,
                            elapsed: stats.elapsed,
                        }),
                        FeatureEvent::Error(error) => SearchEvent::Error(error.to_string()),
                    };
                    send_event(&sender, event)
                });
            })
            .expect(
                "spawning the feature-search UI worker requires only normal OS \
                 resources; failure is unrecoverable for the caller anyway",
            );
        SearchSession::new(generation, receiver, cancel)
    }

    fn start_biome(
        &self,
        generation: u64,
        source: Arc<dyn WorldSource>,
        search: BiomeSearch,
    ) -> SearchSession {
        let (sender, receiver) = bounded(SEARCH_EVENT_CAPACITY);
        let cancel = cancel_token();
        let worker_cancel = Arc::clone(&cancel);
        thread::Builder::new()
            .name("biome-search-ui".to_string())
            .spawn(move || {
                run_biome_search_blocking(&*source, &search, &worker_cancel, |event| {
                    let event = match event {
                        BiomeSearchEvent::Started => SearchEvent::Started,
                        BiomeSearchEvent::Result(result) => {
                            SearchEvent::Hit(SearchHit::from_biome(result, search.biome))
                        }
                        BiomeSearchEvent::Progress(progress) => {
                            SearchEvent::Progress(biome_progress(progress))
                        }
                        BiomeSearchEvent::Done(stats) => SearchEvent::Done(SearchOutcome {
                            results: stats.results,
                            work: stats.samples,
                            rings: stats.rings_scanned,
                            truncated: stats.truncated,
                            cancelled: stats.cancelled,
                            elapsed: stats.elapsed,
                        }),
                        BiomeSearchEvent::Error(error) => SearchEvent::Error(error.to_string()),
                    };
                    send_event(&sender, event)
                });
            })
            .expect(
                "spawning the biome-search UI worker requires only normal OS \
                 resources; failure is unrecoverable for the caller anyway",
            );
        SearchSession::new(generation, receiver, cancel)
    }
}

fn feature_progress(progress: SearchProgress) -> SearchProgressView {
    SearchProgressView {
        ring: progress.ring,
        work: progress.regions_queried,
        results: progress.results_so_far,
    }
}

fn biome_progress(progress: BiomeSearchProgress) -> SearchProgressView {
    SearchProgressView {
        ring: progress.ring,
        work: progress.samples,
        results: progress.results_so_far,
    }
}

/// Forwards one event, dropping a progress heartbeat when the UI is behind
/// (the engine's documented backpressure policy) and blocking on results and
/// terminal events. Returns `false` when the UI is gone.
fn send_event(sender: &Sender<SearchEvent>, event: SearchEvent) -> bool {
    if matches!(event, SearchEvent::Progress(_)) {
        match sender.try_send(event) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => true,
            Err(TrySendError::Disconnected(_)) => false,
        }
    } else {
        sender.send(event).is_ok()
    }
}

/// The selected result's flash ring.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Flash {
    position: (i32, i32),
    until: Instant,
}

/// The headless search state.
pub struct SearchState {
    driver: Arc<dyn SearchDriver>,
    status: SearchStatus,
    /// The newest generation; the stale-event guard.
    generation: u64,
    active: Option<SearchSession>,
    last_request: Option<SearchRequest>,
    results: Vec<SearchHit>,
    seen: HashSet<SearchHitKey>,
    progress: Option<SearchProgressView>,
    outcome: Option<SearchOutcome>,
    error: Option<String>,
    cancel_requested: bool,
    /// The selected hit's identity (not its row index): an out-of-order
    /// result inserted above it must not move the highlight to another row.
    selected: Option<SearchHitKey>,
    flash: Option<Flash>,
}

impl Default for SearchState {
    fn default() -> Self {
        Self::new(Arc::new(RealSearchDriver))
    }
}

impl SearchState {
    /// A state driven by `driver` (production: [`RealSearchDriver`]).
    #[must_use]
    pub fn new(driver: Arc<dyn SearchDriver>) -> Self {
        Self {
            driver,
            status: SearchStatus::Idle,
            generation: 0,
            active: None,
            last_request: None,
            results: Vec::new(),
            seen: HashSet::new(),
            progress: None,
            outcome: None,
            error: None,
            cancel_requested: false,
            selected: None,
            flash: None,
        }
    }

    /// Starts a search. The previous session (if any) is cancelled and
    /// dropped first, and all result state is cleared.
    ///
    /// Returns `false` (and leaves a failed status) when the request is
    /// malformed or the source does not match the target kind — the panel
    /// never builds either, but a panic on the UI thread would be worse.
    pub fn start(&mut self, request: SearchRequest, source: SearchSource) -> bool {
        if !request.center.0.is_finite() || !request.center.1.is_finite() {
            self.fail("the search center must be finite".to_string());
            return false;
        }
        if !request.max_radius.is_finite() || request.max_radius <= 0.0 {
            self.fail("the search radius must be a positive number".to_string());
            return false;
        }
        if request.max_results == 0 {
            self.fail("the result cap must be at least one".to_string());
            return false;
        }

        // Drop the old session before the new generation: its receiver
        // closes and any late event is stale by construction.
        self.active = None;
        self.generation = self.generation.wrapping_add(1);
        self.results.clear();
        self.seen.clear();
        self.progress = None;
        self.outcome = None;
        self.error = None;
        self.cancel_requested = false;
        self.selected = None;
        self.flash = None;

        let generation = self.generation;
        let session = match (&request.target, source) {
            (SearchTarget::Features(filter), SearchSource::Features(source)) => {
                // Expensive kinds use small regions (a biome-prefab region
                // can run for tens of seconds, and the engine only checks
                // cancellation between regions); cheap kinds use large
                // regions to keep the pool round-trip count low. Cave
                // entrances also stay inside the T27 adapter's cap.
                let region_size = if filter.kinds.contains(FeatureKind::CaveEntrance)
                    || filter.kinds.contains(FeatureKind::BiomePrefab)
                {
                    UI_SMALL_REGION_SIZE
                } else {
                    UI_CHEAP_REGION_SIZE
                };
                let search = FeatureSearch {
                    kinds: filter.kinds,
                    center: request.center,
                    max_radius: request.max_radius,
                    max_results: request.max_results,
                    region_size,
                    filters: SearchFilters {
                        categories: filter.categories.clone(),
                        name_contains: filter.name_contains.clone(),
                        min_size: filter.min_size,
                        proximity: request.proximity.clone(),
                    },
                };
                self.driver.start_feature(generation, source, search)
            }
            (SearchTarget::Biome(biome), SearchSource::Biomes(source)) => {
                let search = BiomeSearch {
                    biome: *biome,
                    center: request.center,
                    max_radius: request.max_radius,
                    min_patch_blocks: request.min_patch_blocks,
                    max_results: request.max_results,
                    coarse_step: request.coarse_step.max(1),
                };
                self.driver.start_biome(generation, source, search)
            }
            (SearchTarget::Features(_), SearchSource::Biomes(_))
            | (SearchTarget::Biome(_), SearchSource::Features(_)) => {
                self.fail("the search target and source kind do not match".to_string());
                return false;
            }
        };

        self.status = SearchStatus::Running;
        self.active = Some(session);
        self.last_request = Some(request);
        true
    }

    /// Drains at most [`EVENTS_PER_FRAME`] events; returns how many were
    /// applied. Stale generations are ignored.
    pub fn poll(&mut self) -> usize {
        let Some(session) = self.active.take() else {
            return 0;
        };
        let generation = session.generation();
        let mut events = Vec::new();
        let mut disconnected = false;
        while events.len() < EVENTS_PER_FRAME {
            match session.try_recv() {
                Ok(event) => events.push(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        let terminal = events
            .iter()
            .any(|event| matches!(event, SearchEvent::Done(_) | SearchEvent::Error(_)));
        let mut applied = 0;
        for event in events {
            if self.apply_event(generation, event) {
                applied += 1;
            }
        }
        if disconnected && !terminal && self.status == SearchStatus::Running {
            // The worker vanished without a terminal event. A user-cancel
            // still ends in `Done` by contract; anything else is a failure.
            self.fail(crate::labels::SEARCH_WORKER_DIED.to_string());
        }
        if terminal {
            self.active = None;
        } else {
            self.active = Some(session);
        }
        applied
    }

    /// Applies one event if it belongs to the current generation. Public so
    /// the stale-generation guard is directly testable; the panel only uses
    /// [`Self::poll`].
    pub fn apply_event(&mut self, generation: u64, event: SearchEvent) -> bool {
        if generation != self.generation {
            return false;
        }
        match event {
            SearchEvent::Started => {}
            SearchEvent::Hit(hit) => {
                if self.status == SearchStatus::Running {
                    self.insert_hit(hit);
                }
            }
            SearchEvent::Progress(progress) => {
                if self.status == SearchStatus::Running {
                    self.progress = Some(progress);
                }
            }
            SearchEvent::Done(outcome) => {
                self.status = SearchStatus::Done;
                self.outcome = Some(outcome);
                self.cancel_requested = outcome.cancelled;
                self.progress = None;
            }
            SearchEvent::Error(message) => {
                self.status = SearchStatus::Failed;
                self.error = Some(message);
                self.progress = None;
            }
        }
        true
    }

    /// Inserts a hit unless it is a duplicate, keeping the result list sorted
    /// by `(distance, name, x, z)`.
    ///
    /// The engines stream nearest-first, so the common case appends; an
    /// out-of-order correction binary-searches its place instead of
    /// re-sorting the whole list.
    fn insert_hit(&mut self, hit: SearchHit) {
        if !self.seen.insert(hit.key.clone()) {
            return;
        }
        let order = |left: &SearchHit, right: &SearchHit| {
            left.distance
                .partial_cmp(&right.distance)
                .unwrap_or(Ordering::Equal)
                .then_with(|| left.name.cmp(&right.name))
                .then_with(|| left.x.cmp(&right.x))
                .then_with(|| left.z.cmp(&right.z))
        };
        let index = self
            .results
            .binary_search_by(|existing| order(existing, &hit))
            .unwrap_or_else(|index| index);
        self.results.insert(index, hit);
    }

    /// Requests cancellation; the worker still ends with a `Done` event, so
    /// the status stays `Running` until that arrives.
    pub fn cancel(&mut self) {
        if self.status != SearchStatus::Running {
            return;
        }
        if let Some(session) = &self.active {
            session.cancel();
        }
        self.cancel_requested = true;
    }

    /// Cancels any running search and clears every result and status. Used
    /// on a source change, on `Esc` while idle, and on panel close.
    pub fn reset(&mut self) {
        self.active = None;
        self.generation = self.generation.wrapping_add(1);
        self.status = SearchStatus::Idle;
        self.last_request = None;
        self.results.clear();
        self.seen.clear();
        self.progress = None;
        self.outcome = None;
        self.error = None;
        self.cancel_requested = false;
        self.selected = None;
        self.flash = None;
    }

    fn fail(&mut self, message: String) {
        self.status = SearchStatus::Failed;
        self.error = Some(message);
        self.progress = None;
        self.active = None;
    }

    /// Selects a result (row click): highlights it and starts its flash.
    pub fn select(&mut self, index: usize, now: Instant) {
        let Some(hit) = self.results.get(index) else {
            return;
        };
        self.selected = Some(hit.key.clone());
        self.flash = Some(Flash {
            position: (hit.x, hit.z),
            until: now + FLASH_DURATION,
        });
    }

    /// Whether `key` is the selected row.
    #[must_use]
    pub fn is_selected(&self, key: &SearchHitKey) -> bool {
        self.selected.as_ref() == Some(key)
    }

    /// The flashing result's XZ, while its window is open.
    #[must_use]
    pub fn flash_position(&self, now: Instant) -> Option<(i32, i32)> {
        self.flash
            .filter(|flash| now < flash.until)
            .map(|flash| flash.position)
    }

    /// The selected row's identity.
    #[allow(dead_code)] // reserved: the status bar's selection readout (T29)
    #[must_use]
    pub fn selected(&self) -> Option<&SearchHitKey> {
        self.selected.as_ref()
    }

    /// The last search's request, for the retry button and the panel's
    /// restore-on-reopen behavior.
    #[must_use]
    pub fn last_request(&self) -> Option<&SearchRequest> {
        self.last_request.as_ref()
    }

    /// The lifecycle status.
    #[must_use]
    pub fn status(&self) -> SearchStatus {
        self.status
    }

    /// Whether a cancel was requested but the worker has not answered yet.
    #[must_use]
    pub fn cancel_requested(&self) -> bool {
        self.cancel_requested
    }

    /// The sorted, deduplicated results.
    #[must_use]
    pub fn results(&self) -> &[SearchHit] {
        &self.results
    }

    /// The latest heartbeat, while running.
    #[must_use]
    pub fn progress(&self) -> Option<SearchProgressView> {
        self.progress
    }

    /// The terminal stats, after a completed/cancelled search.
    #[must_use]
    pub fn outcome(&self) -> Option<SearchOutcome> {
        self.outcome
    }

    /// The failure message, after a failed search.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Whether a search is in flight.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.status.is_running()
    }
}

/// Formats a distance: whole blocks below a kilometer, `1.2 km` above.
#[must_use]
pub fn format_distance(distance: f64) -> String {
    if !distance.is_finite() {
        return "?".to_string();
    }
    if distance >= 1_000.0 {
        format!("{:.1} km", distance / 1_000.0)
    } else {
        format!("{:.0} blocks", distance)
    }
}

/// The row's coordinate text: X/Z (plus Y when the engine knows it).
#[must_use]
pub fn format_coordinates(hit: &SearchHit) -> String {
    match hit.y {
        Some(y) => format!("X:{} Y:{} Z:{}", hit.x, y, hit.z),
        None => format!("X:{} Z:{}", hit.x, hit.z),
    }
}

/// The row's patch-area text, when the biome engine estimated one.
#[must_use]
pub fn format_patch_area(hit: &SearchHit) -> Option<String> {
    hit.patch_area.map(|area| {
        if hit.area_capped {
            format!("{area} blocks² (capped)")
        } else {
            format!("{area} blocks²")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use map_core::search::feature::{SearchResult, teleport_string};
    use map_core::test_support::FakeFeatureSource;
    use map_core::types::{Feature, FeatureMetadata};

    /// A driver that hands the test the sender of every session it starts,
    /// and records the cancel token.
    struct FakeDriver {
        senders: Mutex<Vec<Sender<SearchEvent>>>,
        cancels: Mutex<Vec<CancelToken>>,
        features: Mutex<Vec<FeatureSearch>>,
        biomes: Mutex<Vec<BiomeSearch>>,
        force_generation: Option<u64>,
    }

    impl FakeDriver {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                senders: Mutex::new(Vec::new()),
                cancels: Mutex::new(Vec::new()),
                features: Mutex::new(Vec::new()),
                biomes: Mutex::new(Vec::new()),
                force_generation: None,
            })
        }

        fn with_forced_generation(generation: u64) -> Arc<Self> {
            Arc::new(Self {
                senders: Mutex::new(Vec::new()),
                cancels: Mutex::new(Vec::new()),
                features: Mutex::new(Vec::new()),
                biomes: Mutex::new(Vec::new()),
                force_generation: Some(generation),
            })
        }

        fn sender(&self, index: usize) -> Sender<SearchEvent> {
            self.senders
                .lock()
                .expect("test lock")
                .get(index)
                .cloned()
                .expect("the session was started")
        }

        fn cancel(&self, index: usize) -> CancelToken {
            self.cancels.lock().expect("test lock")[index].clone()
        }

        fn feature_search(&self, index: usize) -> FeatureSearch {
            self.features.lock().expect("test lock")[index].clone()
        }

        fn session(&self, generation: u64) -> SearchSession {
            let (sender, receiver) = bounded(EVENTS_PER_FRAME * 4);
            self.senders.lock().expect("test lock").push(sender);
            let cancel = cancel_token();
            self.cancels
                .lock()
                .expect("test lock")
                .push(Arc::clone(&cancel));
            SearchSession::new(
                self.force_generation.unwrap_or(generation),
                receiver,
                cancel,
            )
        }
    }

    impl SearchDriver for FakeDriver {
        fn start_feature(
            &self,
            generation: u64,
            _source: Arc<dyn FeatureSource>,
            search: FeatureSearch,
        ) -> SearchSession {
            self.features.lock().expect("test lock").push(search);
            self.session(generation)
        }

        fn start_biome(
            &self,
            generation: u64,
            _source: Arc<dyn WorldSource>,
            search: BiomeSearch,
        ) -> SearchSession {
            self.biomes.lock().expect("test lock").push(search);
            self.session(generation)
        }
    }

    fn feature_source() -> SearchSource {
        SearchSource::Features(Arc::new(FakeFeatureSource::new()))
    }

    fn hit(name: &str, x: i32, z: i32, distance: f64) -> SearchHit {
        SearchHit {
            key: SearchHitKey::Feature {
                kind: FeatureKind::UniquePrefab,
                name: name.to_string(),
                position: (x, 64, z),
            },
            kind: HitKind::Feature(FeatureKind::UniquePrefab),
            name: name.to_string(),
            category: "Unique".to_string(),
            x,
            y: Some(64),
            z,
            distance,
            teleport: teleport_string((x, 64, z)),
            patch_area: None,
            area_capped: false,
        }
    }

    fn started_state(driver: Arc<dyn SearchDriver>) -> SearchState {
        let mut state = SearchState::new(driver);
        let request = SearchRequest {
            max_radius: 1_000.0,
            ..SearchRequest::default()
        };
        assert!(state.start(request, feature_source()));
        state
    }

    #[test]
    fn search_state_transitions_through_running_done_and_failure() {
        let driver = FakeDriver::new();
        let mut state = started_state(Arc::clone(&driver) as Arc<dyn SearchDriver>);
        assert_eq!(state.status(), SearchStatus::Running);
        assert!(state.is_running());

        let sender = driver.sender(0);
        sender.send(SearchEvent::Started).expect("send");
        sender
            .send(SearchEvent::Hit(hit("A", 10, 0, 10.0)))
            .expect("send");
        state.poll();
        assert_eq!(state.results().len(), 1);
        assert_eq!(state.status(), SearchStatus::Running);

        sender
            .send(SearchEvent::Done(SearchOutcome {
                results: 1,
                work: 3,
                rings: 1,
                truncated: false,
                cancelled: false,
                elapsed: Duration::from_millis(5),
            }))
            .expect("send");
        state.poll();
        assert_eq!(state.status(), SearchStatus::Done);
        assert_eq!(state.outcome().expect("outcome").results, 1);
        assert!(!state.is_running());

        // A worker failure is the Failed terminal state.
        let mut failed = started_state(Arc::clone(&driver) as Arc<dyn SearchDriver>);
        driver
            .sender(1)
            .send(SearchEvent::Error("boom".to_string()))
            .expect("send");
        failed.poll();
        assert_eq!(failed.status(), SearchStatus::Failed);
        assert_eq!(failed.error(), Some("boom"));
    }

    #[test]
    fn search_state_rejects_stale_generations() {
        // The session echoes generation 0 while the state is already on
        // generation 1; its events must be ignored by both the explicit
        // guard and `poll`.
        let driver = FakeDriver::with_forced_generation(0);
        let mut state = started_state(Arc::clone(&driver) as Arc<dyn SearchDriver>);
        assert!(!state.apply_event(0, SearchEvent::Hit(hit("Old", 0, 0, 1.0))));
        assert!(state.results().is_empty());
        driver
            .sender(0)
            .send(SearchEvent::Hit(hit("Polled old", 0, 0, 1.0)))
            .expect("send");
        state.poll();
        assert!(
            state.results().is_empty(),
            "poll drops a stale session event"
        );

        // A second start bumps the generation; the first generation's
        // events are stale from then on.
        assert!(state.start(
            SearchRequest {
                max_radius: 1_000.0,
                ..SearchRequest::default()
            },
            feature_source(),
        ));
        let second_generation = state.generation;
        let first_generation = second_generation - 1;
        assert!(!state.apply_event(first_generation, SearchEvent::Hit(hit("Stale", 1, 1, 1.0))));
        assert!(state.apply_event(second_generation, SearchEvent::Hit(hit("Fresh", 1, 1, 2.0))));
        assert_eq!(state.results().len(), 1);
        assert_eq!(state.results()[0].name, "Fresh");
    }

    #[test]
    fn search_state_drains_a_bounded_batch_per_poll() {
        let driver = FakeDriver::new();
        let mut state = started_state(Arc::clone(&driver) as Arc<dyn SearchDriver>);
        let sender = driver.sender(0);
        // The fake channel holds 4x the batch, enough for one full batch
        // plus some; send a full batch and poll it.
        for index in 0..EVENTS_PER_FRAME {
            sender
                .send(SearchEvent::Hit(hit(
                    &format!("F{index}"),
                    index as i32,
                    0,
                    index as f64,
                )))
                .expect("send");
        }
        let applied = state.poll();
        assert_eq!(applied, EVENTS_PER_FRAME, "one frame is one bounded batch");
        assert_eq!(state.results().len(), EVENTS_PER_FRAME);
        assert!(
            state
                .results()
                .windows(2)
                .all(|pair| pair[0].distance <= pair[1].distance)
        );
    }

    #[test]
    fn search_state_dedupes_and_sorts_out_of_order_hits() {
        let driver = FakeDriver::new();
        let mut state = started_state(Arc::clone(&driver) as Arc<dyn SearchDriver>);
        let sender = driver.sender(0);
        sender
            .send(SearchEvent::Hit(hit("Far", 30, 0, 30.0)))
            .expect("send");
        sender
            .send(SearchEvent::Hit(hit("Near", 10, 0, 10.0)))
            .expect("send");
        // The same feature twice (overlapping rings) and a second feature
        // at the same position.
        sender
            .send(SearchEvent::Hit(hit("Near", 10, 0, 10.0)))
            .expect("send");
        state.poll();

        assert_eq!(state.results().len(), 2, "duplicates collapse");
        assert_eq!(state.results()[0].name, "Near");
        assert_eq!(state.results()[1].name, "Far");
        assert_eq!(format_distance(state.results()[0].distance), "10 blocks");
        assert_eq!(format_distance(1_250.0), "1.2 km");
        assert_eq!(format_coordinates(&state.results()[0]), "X:10 Y:64 Z:0");
    }

    #[test]
    fn search_state_selection_tracks_the_hit_not_the_row() {
        // Results stream nearest-first, but an out-of-order correction can
        // insert above the selected row. The highlight must follow the hit.
        let driver = FakeDriver::new();
        let mut state = started_state(Arc::clone(&driver) as Arc<dyn SearchDriver>);
        let sender = driver.sender(0);
        sender
            .send(SearchEvent::Hit(hit("Far", 30, 0, 30.0)))
            .expect("send");
        state.poll();
        state.select(0, Instant::now());
        let far_key = state.results()[0].key.clone();
        assert!(state.is_selected(&far_key));

        sender
            .send(SearchEvent::Hit(hit("Near", 10, 0, 10.0)))
            .expect("send");
        state.poll();
        assert_eq!(state.results()[0].name, "Near");
        assert!(
            state.is_selected(&far_key),
            "the selected identity still points at Far, now row 1",
        );
        assert!(!state.is_selected(&state.results()[0].key));
    }

    #[test]
    fn search_state_cancel_reaches_the_worker_and_keeps_results() {
        let driver = FakeDriver::new();
        let mut state = started_state(Arc::clone(&driver) as Arc<dyn SearchDriver>);
        let sender = driver.sender(0);
        sender
            .send(SearchEvent::Hit(hit("Kept", 5, 5, 7.0)))
            .expect("send");
        state.poll();
        assert!(!driver.cancel(0).load(AtomicOrdering::Relaxed));

        state.cancel();
        assert!(driver.cancel(0).load(AtomicOrdering::Relaxed));
        assert!(state.cancel_requested());
        assert_eq!(state.status(), SearchStatus::Running);
        assert_eq!(state.results().len(), 1, "delivered results stay valid");

        sender
            .send(SearchEvent::Done(SearchOutcome {
                results: 1,
                work: 1,
                rings: 1,
                truncated: false,
                cancelled: true,
                elapsed: Duration::from_millis(1),
            }))
            .expect("send");
        state.poll();
        assert_eq!(state.status(), SearchStatus::Done);
        assert!(state.outcome().expect("outcome").cancelled);
    }

    #[test]
    fn search_state_reset_cancels_and_clears() {
        let driver = FakeDriver::new();
        let mut state = started_state(Arc::clone(&driver) as Arc<dyn SearchDriver>);
        state.apply_event(state.generation, SearchEvent::Hit(hit("Gone", 1, 1, 1.0)));
        state.select(0, Instant::now());
        assert!(state.selected().is_some());
        assert!(state.flash_position(Instant::now()).is_some());

        state.reset();
        assert_eq!(state.status(), SearchStatus::Idle);
        assert!(state.results().is_empty());
        assert!(state.selected().is_none());
        assert!(state.flash_position(Instant::now()).is_none());
        assert!(
            driver.cancel(0).load(AtomicOrdering::Relaxed),
            "drop cancels"
        );
    }

    #[test]
    fn search_state_select_flashes_the_result_position_and_expires() {
        let driver = FakeDriver::new();
        let mut state = started_state(Arc::clone(&driver) as Arc<dyn SearchDriver>);
        state.apply_event(
            state.generation,
            SearchEvent::Hit(hit("Negative", -120, -80, 144.0)),
        );
        let now = Instant::now();
        state.select(0, now);
        let selected = state.results()[0].key.clone();
        assert!(state.is_selected(&selected));
        assert_eq!(state.flash_position(now), Some((-120, -80)));
        assert_eq!(
            state.flash_position(now + FLASH_DURATION + Duration::from_millis(1)),
            None,
        );
    }

    #[test]
    fn search_state_rejects_mismatched_targets_and_bad_requests() {
        let driver = FakeDriver::new();
        let mut state = SearchState::new(Arc::clone(&driver) as Arc<dyn SearchDriver>);
        let request = SearchRequest {
            target: SearchTarget::Biome(BiomeId(3)),
            ..SearchRequest::default()
        };
        assert!(!state.start(request, feature_source()));
        assert_eq!(state.status(), SearchStatus::Failed);
        assert!(state.error().is_some());

        let mut state = SearchState::new(Arc::clone(&driver) as Arc<dyn SearchDriver>);
        assert!(!state.start(
            SearchRequest {
                max_radius: 0.0,
                ..SearchRequest::default()
            },
            feature_source(),
        ));
        assert_eq!(state.status(), SearchStatus::Failed);
    }

    #[test]
    fn search_state_cave_searches_use_capped_regions() {
        // The T27 adapter drops cave entrances above its 256-block cap; a
        // cave-inclusive feature search must therefore use cap-sized ring
        // regions or every region would silently lose the cave kind.
        let driver = FakeDriver::new();
        let mut state = SearchState::new(Arc::clone(&driver) as Arc<dyn SearchDriver>);
        let request = SearchRequest {
            target: SearchTarget::Features(FeatureFilter {
                kinds: map_core::types::FeatureKindMask::from_kind(FeatureKind::CaveEntrance),
                categories: None,
                name_contains: None,
                min_size: None,
            }),
            ..SearchRequest::default()
        };
        assert!(state.start(request, feature_source()));
        assert_eq!(
            driver.feature_search(0).region_size,
            UI_SMALL_REGION_SIZE,
            "cave searches stay inside the adapter's cap",
        );

        let mut state = SearchState::new(Arc::clone(&driver) as Arc<dyn SearchDriver>);
        let request = SearchRequest {
            target: SearchTarget::Features(FeatureFilter {
                kinds: map_core::types::FeatureKindMask::from_kind(FeatureKind::Spawn),
                categories: None,
                name_contains: None,
                min_size: None,
            }),
            ..SearchRequest::default()
        };
        assert!(state.start(request, feature_source()));
        assert_eq!(
            driver.feature_search(1).region_size,
            UI_CHEAP_REGION_SIZE,
            "cheap kinds use large regions to keep pool round trips low",
        );
    }

    #[test]
    fn search_hits_use_the_shared_teleport_format_and_biome_tilde() {
        let feature = Feature {
            kind: FeatureKind::UniquePrefab,
            name: "Keep".to_string(),
            category: "Unique".to_string(),
            position: (-3, 70, 4),
            bounds: None,
            metadata: FeatureMetadata {
                variant: None,
                size: None,
                source: None,
            },
        };
        let result = SearchResult {
            distance: 5.0,
            teleport: teleport_string(feature.position),
            feature,
        };
        let hit = SearchHit::from_feature(result);
        assert_eq!(hit.teleport, "/tp -3 70 4");
        assert_eq!(format_coordinates(&hit), "X:-3 Y:70 Z:4");

        let biome = SearchHit::from_biome(
            BiomeMatch {
                position: (-7, 9),
                distance: 11.4,
                patch_area: Some(4_096),
                area_capped: false,
            },
            BiomeId(2),
        );
        assert_eq!(biome.teleport, "/tp -7 ~ 9");
        assert_eq!(format_coordinates(&biome), "X:-7 Z:9");
        assert_eq!(format_patch_area(&biome).as_deref(), Some("4096 blocks²"));
    }

    #[test]
    fn search_state_retains_the_last_request_for_retry() {
        let driver = FakeDriver::new();
        let mut state = started_state(Arc::clone(&driver) as Arc<dyn SearchDriver>);
        assert_eq!(state.last_request().expect("request").max_radius, 1_000.0);
        state.reset();
        assert!(state.last_request().is_none());
    }

    /// Polls until a terminal event (or a generous deadline), proving the
    /// production driver actually runs the T24/T25 engine end to end.
    fn wait_for_terminal(state: &mut SearchState) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while state.is_running() && Instant::now() < deadline {
            state.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            !state.is_running(),
            "the search did not reach a terminal event: {:?}",
            state.status(),
        );
    }

    #[test]
    fn search_state_real_driver_streams_feature_results() {
        let mut state = SearchState::default();
        let request = SearchRequest {
            center: (0.0, 0.0),
            max_radius: 500.0,
            max_results: 10,
            ..SearchRequest::default()
        };
        assert!(state.start(
            request,
            SearchSource::Features(Arc::new(FakeFeatureSource::new())),
        ));
        wait_for_terminal(&mut state);
        assert_eq!(state.status(), SearchStatus::Done, "{:?}", state.error());
        assert!(
            state.results().iter().any(|hit| hit.name == "Alpha"),
            "the fake source's alpha feature is within 500 blocks",
        );
        assert!(
            state
                .results()
                .windows(2)
                .all(|pair| pair[0].distance <= pair[1].distance),
            "the real engine's stream stays nearest-first",
        );
        assert!(
            state
                .results()
                .iter()
                .all(|hit| hit.teleport.starts_with("/tp ")),
            "every feature row carries the shared teleport formatter",
        );
    }

    #[test]
    fn search_state_real_driver_streams_biome_results() {
        let mut state = SearchState::default();
        let request = SearchRequest {
            target: SearchTarget::Biome(BiomeId(0)),
            center: (0.0, 0.0),
            max_radius: 100.0,
            max_results: 10,
            coarse_step: 16,
            ..SearchRequest::default()
        };
        assert!(state.start(
            request,
            SearchSource::Biomes(Arc::new(map_core::test_support::FakeWorldSource::new("42"))),
        ));
        wait_for_terminal(&mut state);
        assert_eq!(state.status(), SearchStatus::Done, "{:?}", state.error());
        assert!(!state.results().is_empty(), "biome 0 surrounds the origin");
        assert!(matches!(
            state.results()[0].kind,
            HitKind::Biome(BiomeId(0))
        ));
        assert!(state.results()[0].teleport.contains("~"));
    }
}
