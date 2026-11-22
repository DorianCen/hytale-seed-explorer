//! Feature overlay (T23): the fetch state machine, planner, draw list and
//! marker styles behind the map's vector layer.
//!
//! The overlay has three layers of responsibility:
//!
//! 1. [`FeatureFetchPlanner`] — pure decisions: when a view has settled, how
//!    large a rectangle to ask for, and whether a reply is still wanted. No
//!    source, no egui, so the debounce/cap rules are unit-tested directly.
//! 2. [`FeatureOverlayState`] — the app-side state an [`crate::state::AppState`]
//!    owns: the active catalog, the `map-core` query cache, the single
//!    in-flight query, the viewport signature the last query served, and the
//!    selected/flashing feature. One `std::thread` per query calls the
//!    synchronous `FeatureSource::features_in`; the UI thread only polls the
//!    reply channel.
//! 3. Drawing and interaction helpers — [`marker_list`] projects, filters and
//!    clusters; [`paint_items`] draws; [`hit_test`] and [`tooltip_text`] feed
//!    the map widget. These are in this module so `map_view.rs` stays thin.
//!
//! # Toggle behavior (the documented T23 choice)
//!
//! Group checkboxes are **draw-time** filters first: [`marker_list`] drops
//! every feature whose group is disabled, so toggling hides/shows icons from
//! the already-loaded catalog without a query. The checkbox state also
//! becomes the **next fetch's** `FeatureKindMask`: a disabled group is never
//! queried again, but a toggle itself never starts a fetch (the viewport
//! signature did not move). This is the map-core packet's chosen answer to
//! "not drawn or fetched": fetches carry a kind filter, drawing carries the
//! finer group filter.
//!
//! # Why the rect cap
//!
//! `docs/benchmarks.md` §"Feature enumeration (T22)" prices a 2048-block-side
//! `features_in` at ~5.4 minutes and multi-GiB peak RSS on the generator
//! worker (10,000 blocks extrapolates to ~1.8 hours). A zoomed-out view is a
//! cache-only view: the last catalog keeps drawing (and clustering), and no
//! new query is issued until the user zooms back in. See
//! [`MAX_FETCH_SIDE_BLOCKS`].

use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Color32, FontId, Pos2, Stroke};

use map_core::coords::ScreenPos;
use map_core::error::SourceResult;
use map_core::features::{
    ClusterItem, FeatureCatalog, FeatureIcon, FeatureOverlayState as CoreOverlayState,
    FeatureToggleGroup, FeatureToggleMask, category_for, cluster, group_for,
};
use map_core::types::{Feature, FeatureFilter, FeatureKind, GeneratorIdentity, Rect};

use crate::labels;
use crate::state::OverlaySettings;
use crate::view_state::{MapViewState, block_to_screen};

/// The debounce window: a view must stop changing for this long before the
/// overlay queries it (T23's 150 ms figure).
pub const SETTLE_DEBOUNCE_MS: u64 = 150;

/// A hard cap on the requested rectangle's side, in blocks.
///
/// The T22 measurements (`docs/benchmarks.md` §"Feature enumeration (T22)")
/// put a 2048×2048 query at ~5.4 minutes and ~5 GiB peak RSS in the generator
/// worker; larger rects are batch jobs, not UI interactions (T33 owns the
/// memory lever). When the margin-expanded desired rect exceeds this, the
/// overlay does not fetch — it keeps drawing whatever the last catalog holds
/// until the user zooms in far enough that the query fits.
pub const MAX_FETCH_SIDE_BLOCKS: i32 = 2048;

/// The hard per-frame cap on individually drawn features. Clustering normally
/// keeps the single count far below this; the cap is the "thousands of
/// features" guard T23's risk note asks for. T33 profiles the real number.
pub const MAX_DRAWN_SINGLES: usize = 1500;

/// Marker geometry: icon radius, cluster disc radius and pointer hit radius
/// (all in egui points).
const MARKER_RADIUS_PX: f32 = 5.0;
const CLUSTER_RADIUS_PX: f32 = 9.0;
const HIT_RADIUS_PX: f32 = 10.0;
/// Markers whose centre is just outside the map rect still draw their edge;
/// culling uses this slack so a marker does not pop at the viewport border.
const CULL_MARGIN_PX: f32 = 12.0;

/// How long a selected feature keeps its flashing ring.
const FLASH_DURATION: Duration = Duration::from_secs(1);

/// The colour of every marker outline (near-black for contrast on tiles).
const OUTLINE: Color32 = Color32::from_rgb(10, 10, 14);

/// A viewport signature: `(center x, center z, smooth zoom, packed size)`.
///
/// The fourth slot packs both dimensions into one `f32` because the
/// inspectable shape is `(f64, f64, f32, f32)`. egui sizes stay far below
/// 4096 points, so `width * 4096 + height` is exact enough for change
/// detection (a whole-point resize always changes it).
pub type ViewSignature = (f64, f64, f32, f32);

/// The identity of one feature in the catalog, for selection/flash state:
/// `(kind, name, position)` exactly as `FeatureCatalog::build` dedupes.
pub type FeatureKey = (FeatureKind, String, (i32, i32, i32));

/// A settled query the app should issue: the rect and the kind filter.
#[derive(Debug, Clone, PartialEq)]
pub struct FeatureFetch {
    pub rect: Rect,
    pub filter: FeatureFilter,
}

/// What one poll of the in-flight channel produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeatureReplyOutcome {
    /// No query is in flight.
    Idle,
    /// The worker has not replied yet.
    Pending,
    /// A reply arrived for a rect that no longer covers the visible view; it
    /// was dropped (never cached, never drawn).
    Stale,
    /// A reply was built into a catalog and cached. Carries its feature count.
    Applied(usize),
    /// The query failed; the last catalog stays active. Carries the message.
    Failed(String),
    /// The worker vanished without sending (it panicked or the app is going
    /// away); the in-flight slot is free again.
    WorkerDied,
}

/// Pure fetch decisions for the overlay.
///
/// The app owns the mutable deadline; every method here is a function of the
/// view, the clock and the last signature served.
pub struct FeatureFetchPlanner;

impl FeatureFetchPlanner {
    /// The viewport signature a fetch depends on: center, zoom and size.
    #[must_use]
    pub fn viewport_signature(view: &MapViewState) -> ViewSignature {
        let viewport = view.viewport();
        (
            viewport.center.x,
            viewport.center.z,
            view.smooth_zoom(),
            viewport.width * 4096.0 + viewport.height,
        )
    }

    /// True when `now_ms` is past `deadline_ms` (the view has settled) **and**
    /// this exact signature has not been served yet.
    ///
    /// `fetched_signature` is cleared on a source change, so the first settled
    /// frame of a new world fetches even if the view did not move.
    #[must_use]
    pub fn should_fetch(
        now_ms: u64,
        deadline_ms: Option<u64>,
        current_signature: &ViewSignature,
        fetched_signature: Option<&ViewSignature>,
    ) -> bool {
        deadline_ms.is_some_and(|deadline| now_ms >= deadline)
            && fetched_signature != Some(current_signature)
    }

    /// The visible block rect: the viewport's four corners mapped to their
    /// inclusive block bounds. A zero-sized viewport collapses to the center's
    /// one-block rect (still a valid `Rect`).
    #[must_use]
    pub fn visible_rect(view: &MapViewState) -> Rect {
        Self::desired_rect(view, 0.0)
    }

    /// The visible rect expanded by `margin_blocks` on every side.
    ///
    /// Corner blocks come from the canonical [`MapViewState::screen_to_block`];
    /// a corner outside the `i32` block space is clamped to its edge, and the
    /// expansion saturates rather than overflowing. `Rect::new`'s inclusive
    /// max semantics are what the generator query expects (T22).
    #[must_use]
    pub fn desired_rect(view: &MapViewState, margin_blocks: f32) -> Rect {
        let viewport = view.viewport();
        let corners = [
            (0.0, 0.0),
            (viewport.width, 0.0),
            (0.0, viewport.height),
            (viewport.width, viewport.height),
        ];
        let mut min_x = i32::MAX;
        let mut min_z = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_z = i32::MIN;
        for (screen_x, screen_y) in corners {
            let (x, z) = corner_block(view, screen_x, screen_y);
            min_x = min_x.min(x);
            min_z = min_z.min(z);
            max_x = max_x.max(x);
            max_z = max_z.max(z);
        }
        let margin = if margin_blocks.is_finite() {
            margin_blocks.max(0.0).ceil() as i64
        } else {
            0
        };
        Rect::new(
            saturate_i64(i64::from(min_x).saturating_sub(margin)),
            saturate_i64(i64::from(min_z).saturating_sub(margin)),
            saturate_i64(i64::from(max_x).saturating_add(margin)),
            saturate_i64(i64::from(max_z).saturating_add(margin)),
        )
    }

    /// One viewport of margin, in blocks, shrunk to whatever
    /// [`MAX_FETCH_SIDE_BLOCKS`] leaves room for.
    ///
    /// The plan asks for a one-viewport margin so a small pan reuses the
    /// catalog. At the app's finest zoom (1 pt/blk) a 1280-point window already
    /// spans 1280 blocks; a full viewport on every side would be 3840 and
    /// exceed the cap, so no default-sized window could ever fetch. The margin
    /// therefore targets one viewport but clamps to roughly
    /// `(cap - visible) / 2` (383 blocks at 1280): panning still gets a
    /// margin, and the cap keeps its meaning. When the visible rect alone exceeds the cap, the margin is
    /// zero and [`Self::oversized`] correctly makes the view cache-only.
    #[must_use]
    pub fn margin_blocks(view: &MapViewState) -> f32 {
        let viewport = view.viewport();
        let block_width = f64::from(viewport.width) / viewport.scale;
        let block_height = f64::from(viewport.height) / viewport.scale;
        let visible = block_width.max(block_height) as f32;
        // The desired rect counts inclusive bounds and ceils the margin, so
        // leave room for the extra column before splitting the remainder.
        let room = ((MAX_FETCH_SIDE_BLOCKS as f32 - visible - 1.0) / 2.0)
            .floor()
            .max(0.0);
        visible.min(room)
    }

    /// True when the rect exceeds [`MAX_FETCH_SIDE_BLOCKS`] on either side.
    ///
    /// Counted in `i64`: a zoomed-out desired rect can span more than
    /// `i32::MAX` blocks after expansion, and `Rect::width()` (an `i32`
    /// subtraction) would overflow before the cap could reject it.
    #[must_use]
    pub fn oversized(rect: &Rect) -> bool {
        let cap = i64::from(MAX_FETCH_SIDE_BLOCKS);
        let width = i64::from(rect.max_x) - i64::from(rect.min_x) + 1;
        let height = i64::from(rect.max_z) - i64::from(rect.min_z) + 1;
        width > cap || height > cap
    }

    /// True when a reply for `reply_rect` still covers everything the user
    /// currently sees (an in-flight reply is wanted only then).
    #[must_use]
    pub fn wants_reply(reply_rect: Rect, visible_rect: Rect) -> bool {
        visible_rect.min_x >= reply_rect.min_x
            && visible_rect.max_x <= reply_rect.max_x
            && visible_rect.min_z >= reply_rect.min_z
            && visible_rect.max_z <= reply_rect.max_z
    }
}

/// A corner's block column, falling back to the world coordinate clamped onto
/// the `i32` block space when the conversion overflows.
fn corner_block(view: &MapViewState, screen_x: f32, screen_y: f32) -> (i32, i32) {
    match view.screen_to_block((screen_x, screen_y)) {
        Some(pos) => (pos.x, pos.z),
        None => {
            let world = view
                .viewport()
                .screen_to_world(ScreenPos::new(screen_x, screen_y));
            (saturate_floor(world.x), saturate_floor(world.z))
        }
    }
}

/// Floor a world coordinate into the inclusive `i32` block space.
fn saturate_floor(value: f64) -> i32 {
    if value.is_nan() {
        0
    } else {
        saturate_i64(value.floor() as i64)
    }
}

/// Clamp an `i64` into `i32`'s range (`i64::clamp` keeps the cast lossless).
fn saturate_i64(value: i64) -> i32 {
    value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// The left panel's checkbox state as a map-core toggle mask. `cave_entrances`
/// maps to [`FeatureToggleGroup::Cave`] now but stays a draw/fetch no-op until
/// T27 starts emitting entrances.
#[must_use]
pub fn toggle_mask(settings: &OverlaySettings) -> FeatureToggleMask {
    let mut mask = FeatureToggleMask::none();
    mask.set(FeatureToggleGroup::Structures, settings.structures);
    mask.set(FeatureToggleGroup::Prefabs, settings.prefabs);
    mask.set(FeatureToggleGroup::Spawn, settings.spawn);
    mask.set(FeatureToggleGroup::Poi, settings.poi);
    mask.set(FeatureToggleGroup::Cave, settings.cave_entrances);
    mask
}

/// The query filter for the current toggles: disabled groups' kinds are never
/// fetched (see the module docs).
#[must_use]
pub fn fetch_filter(settings: &OverlaySettings) -> FeatureFilter {
    let overlay = CoreOverlayState {
        enabled_groups: toggle_mask(settings),
        cluster_cell_px: CoreOverlayState::DEFAULT_CLUSTER_CELL_PX,
    };
    FeatureFilter {
        kinds: overlay.kinds_mask(),
        categories: None,
        name_contains: None,
        min_size: None,
    }
}

/// The feature's selection key.
#[must_use]
pub fn feature_key(feature: &Feature) -> FeatureKey {
    (feature.kind, feature.name.clone(), feature.position)
}

fn key_matches(key: &FeatureKey, feature: &Feature) -> bool {
    key.0 == feature.kind && key.1 == feature.name && key.2 == feature.position
}

fn placeholder_identity() -> GeneratorIdentity {
    GeneratorIdentity {
        generator_version: map_core::GENERATOR_VERSION,
        world_assets: String::new(),
        palette_version: map_core::palette::PALETTE_VERSION,
    }
}

/// The app-side overlay state: active catalog, cache, one in-flight query and
/// the selection.
pub struct FeatureOverlayState {
    /// Monotonic origin for the planner's millisecond clock.
    epoch: Instant,
    cache: map_core::features::FeatureCache,
    catalog: Option<Arc<FeatureCatalog>>,
    catalog_rect: Option<Rect>,
    /// The single in-flight query. Dropping it detaches the worker: its send
    /// fails and the thread (which owns the source `Arc`) exits on its own.
    in_flight: Option<Receiver<SourceResult<Vec<Feature>>>>,
    in_flight_rect: Option<Rect>,
    in_flight_filter: Option<FeatureFilter>,
    deadline_ms: Option<u64>,
    observed_signature: Option<ViewSignature>,
    fetched_signature: Option<ViewSignature>,
    selected: Option<FeatureKey>,
    flash_until: Option<Instant>,
}

impl Default for FeatureOverlayState {
    fn default() -> Self {
        Self::new()
    }
}

impl FeatureOverlayState {
    /// Empty state bound to the placeholder identity (before any source).
    #[must_use]
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
            cache: map_core::features::FeatureCache::new(placeholder_identity()),
            catalog: None,
            catalog_rect: None,
            in_flight: None,
            in_flight_rect: None,
            in_flight_filter: None,
            deadline_ms: None,
            observed_signature: None,
            fetched_signature: None,
            selected: None,
            flash_until: None,
        }
    }

    /// Rebinds the cache to a new source identity and drops every trace of the
    /// previous world. Called when a load turns `Ready`.
    pub fn set_source(&mut self, identity: &GeneratorIdentity) {
        self.cache.set_identity(identity.clone());
        self.reset();
    }

    /// Drops the in-flight query, the cache and the active catalog. The next
    /// settled frame re-plans from scratch; a reply that arrives after this is
    /// stale by construction (its receiver is gone).
    pub fn reset(&mut self) {
        self.in_flight = None;
        self.in_flight_rect = None;
        self.in_flight_filter = None;
        self.cache.reset();
        self.catalog = None;
        self.catalog_rect = None;
        self.deadline_ms = None;
        self.observed_signature = None;
        self.fetched_signature = None;
        self.selected = None;
        self.flash_until = None;
    }

    /// The active catalog, when a query or cache hit produced one.
    #[must_use]
    pub fn catalog(&self) -> Option<&FeatureCatalog> {
        self.catalog.as_deref()
    }

    /// The rect the active catalog covers (`None` before the first query).
    #[allow(dead_code)] // reserved: T24/T26 search highlighting
    #[must_use]
    pub fn catalog_rect(&self) -> Option<Rect> {
        self.catalog_rect
    }

    /// Whether a query is in flight.
    #[must_use]
    pub fn has_fetch_in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    /// The selected feature key, when one was clicked.
    #[must_use]
    pub fn selected_key(&self) -> Option<&FeatureKey> {
        self.selected.as_ref()
    }

    /// Whether the selected feature's flash ring is still inside its ~1 s
    /// window.
    #[must_use]
    pub fn is_flashing(&self, now: Instant) -> bool {
        self.flash_until.is_some_and(|until| now < until)
    }

    /// Selects `feature` and starts its flash.
    pub fn select(&mut self, feature: &Feature, now: Instant) {
        self.selected = Some(feature_key(feature));
        self.flash_until = Some(now + FLASH_DURATION);
    }

    /// Notes the current view: a changed signature restarts the debounce
    /// deadline; an unchanged one leaves it alone.
    pub fn observe(&mut self, now: Instant, view: &MapViewState) {
        let signature = FeatureFetchPlanner::viewport_signature(view);
        if self.observed_signature != Some(signature) {
            self.observed_signature = Some(signature);
            self.deadline_ms = Some(self.elapsed_ms(now).saturating_add(SETTLE_DEBOUNCE_MS));
        }
    }

    /// Whether the settled view should be queried (the pure planner's decision
    /// with this state's deadline and last-served signature).
    #[must_use]
    pub fn fetch_due(&self, now: Instant, view: &MapViewState) -> bool {
        FeatureFetchPlanner::should_fetch(
            self.elapsed_ms(now),
            self.deadline_ms,
            &FeatureFetchPlanner::viewport_signature(view),
            self.fetched_signature.as_ref(),
        )
    }

    /// Plans the frame's query, if one is due:
    ///
    /// - a query already in flight means wait;
    /// - an unsettled or already-served view means nothing;
    /// - an oversized desired rect means cache-only (marking the signature
    ///   served so the check does not repeat every frame);
    /// - a cache hit adopts the cached catalog and counts as served;
    /// - otherwise the rect and kind filter are returned for the app to query.
    pub fn plan_fetch(
        &mut self,
        now: Instant,
        view: &MapViewState,
        settings: &OverlaySettings,
    ) -> Option<FeatureFetch> {
        if self.in_flight.is_some() {
            return None;
        }
        let signature = FeatureFetchPlanner::viewport_signature(view);
        if !self.fetch_due(now, view) {
            return None;
        }
        let rect =
            FeatureFetchPlanner::desired_rect(view, FeatureFetchPlanner::margin_blocks(view));
        if FeatureFetchPlanner::oversized(&rect) {
            // Cache-only view: keep drawing the last catalog, and do not
            // re-run this decision every frame until the view changes.
            self.fetched_signature = Some(signature);
            return None;
        }
        let filter = fetch_filter(settings);
        if let Some(catalog) = self.cache.get(rect, &filter) {
            self.adopt(catalog, rect);
            self.fetched_signature = Some(signature);
            return None;
        }
        Some(FeatureFetch { rect, filter })
    }

    /// Records `request` and its reply channel as the single in-flight fetch.
    /// The caller has already spawned the worker that sends into `receiver`.
    pub fn begin_fetch(
        &mut self,
        request: FeatureFetch,
        receiver: Receiver<SourceResult<Vec<Feature>>>,
    ) {
        self.in_flight = Some(receiver);
        self.in_flight_rect = Some(request.rect);
        self.in_flight_filter = Some(request.filter);
    }

    /// Polls the in-flight channel without blocking and applies a wanted reply.
    ///
    /// A reply is wanted only while its rect still contains the current
    /// visible rect; otherwise it is [`FeatureReplyOutcome::Stale`] and never
    /// reaches the cache or the screen. `Ok` builds, caches and activates the
    /// catalog; `Err` keeps the last catalog and reports the message.
    pub fn poll(&mut self, _now: Instant, view: &MapViewState) -> FeatureReplyOutcome {
        let Some(receiver) = self.in_flight.as_ref() else {
            return FeatureReplyOutcome::Idle;
        };
        match receiver.try_recv() {
            Ok(result) => {
                let rect = self.in_flight_rect.take();
                let filter = self.in_flight_filter.take();
                self.in_flight = None;
                let Some(rect) = rect else {
                    return FeatureReplyOutcome::Stale;
                };
                let visible = FeatureFetchPlanner::visible_rect(view);
                if !FeatureFetchPlanner::wants_reply(rect, visible) {
                    return FeatureReplyOutcome::Stale;
                }
                match result {
                    Ok(features) => {
                        let catalog = Arc::new(FeatureCatalog::build(features));
                        let count = catalog.len();
                        if let Some(filter) = filter {
                            self.cache.put(rect, filter, Arc::clone(&catalog));
                        }
                        self.adopt(catalog, rect);
                        self.fetched_signature =
                            Some(FeatureFetchPlanner::viewport_signature(view));
                        self.prune_selection();
                        FeatureReplyOutcome::Applied(count)
                    }
                    Err(error) => {
                        // Keep the last catalog; mark this view served so a
                        // failing query is not retried every frame (a pan or
                        // zoom retries).
                        self.fetched_signature =
                            Some(FeatureFetchPlanner::viewport_signature(view));
                        FeatureReplyOutcome::Failed(error.to_string())
                    }
                }
            }
            Err(TryRecvError::Empty) => FeatureReplyOutcome::Pending,
            Err(TryRecvError::Disconnected) => {
                self.clear_in_flight();
                // Do not respawn every frame for a worker that died: treat
                // this view as served until it moves.
                self.fetched_signature = Some(FeatureFetchPlanner::viewport_signature(view));
                FeatureReplyOutcome::WorkerDied
            }
        }
    }

    /// Frees the in-flight slot without touching the catalog or cache.
    pub fn cancel_fetch(&mut self) {
        self.clear_in_flight();
    }

    fn clear_in_flight(&mut self) {
        self.in_flight = None;
        self.in_flight_rect = None;
        self.in_flight_filter = None;
    }

    fn adopt(&mut self, catalog: Arc<FeatureCatalog>, rect: Rect) {
        self.catalog = Some(catalog);
        self.catalog_rect = Some(rect);
    }

    /// Drops the selection when the new catalog no longer contains it.
    fn prune_selection(&mut self) {
        let Some(key) = &self.selected else {
            return;
        };
        let present = self
            .catalog
            .as_ref()
            .is_some_and(|catalog| catalog.features().iter().any(|f| key_matches(key, f)));
        if !present {
            self.selected = None;
            self.flash_until = None;
        }
    }

    fn elapsed_ms(&self, now: Instant) -> u64 {
        u64::try_from(now.saturating_duration_since(self.epoch).as_millis()).unwrap_or(u64::MAX)
    }
}

// ---------------------------------------------------------------------------
// Draw list
// ---------------------------------------------------------------------------

/// Projects the enabled catalog features to screen points, clusters them and
/// returns the frame's draw list, singles capped at [`MAX_DRAWN_SINGLES`].
///
/// `viewport` is the map widget's size in points. Features outside the
/// viewport (plus [`CULL_MARGIN_PX`]) are culled before clustering, so a
/// small catalog that mostly sits off-screen costs little.
#[must_use]
pub fn marker_list(
    catalog: &FeatureCatalog,
    enabled_groups: FeatureToggleMask,
    view: &MapViewState,
    viewport: (f32, f32),
    cluster_cell_px: f32,
) -> Vec<ClusterItem> {
    let mut points = Vec::new();
    let mut indices = Vec::new();
    for (index, feature) in catalog.features().iter().enumerate() {
        if !enabled_groups.contains(group_for(feature)) {
            continue;
        }
        let Some((x, y)) = block_to_screen(view, feature.position.0, feature.position.2) else {
            continue;
        };
        if x < -CULL_MARGIN_PX
            || x > viewport.0 + CULL_MARGIN_PX
            || y < -CULL_MARGIN_PX
            || y > viewport.1 + CULL_MARGIN_PX
        {
            continue;
        }
        points.push((x, y));
        indices.push(index);
    }
    let mapped = cluster(&points, cluster_cell_px)
        .into_iter()
        .map(|item| match item {
            ClusterItem::Single { feature, pos } => ClusterItem::Single {
                feature: indices[feature],
                pos,
            },
            ClusterItem::Cluster {
                center,
                count,
                members,
            } => ClusterItem::Cluster {
                center,
                count,
                members: members.into_iter().map(|member| indices[member]).collect(),
            },
        });
    cap_singles(mapped)
}

/// Keeps the first [`MAX_DRAWN_SINGLES`] singles and every cluster.
fn cap_singles(items: impl IntoIterator<Item = ClusterItem>) -> Vec<ClusterItem> {
    let mut kept = Vec::new();
    let mut singles = 0;
    for item in items {
        if matches!(item, ClusterItem::Single { .. }) {
            if singles >= MAX_DRAWN_SINGLES {
                continue;
            }
            singles += 1;
        }
        kept.push(item);
    }
    kept
}

/// One hit-tested draw item: singles win over clusters at the same pointer,
/// and the nearest single wins among singles (the draw list's index is
/// included so click handling can match the item it drew).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureHit {
    /// A lone marker: the draw-list item and its catalog index.
    Single { item: usize, feature: usize },
    /// A cluster marker.
    Cluster { item: usize },
}

/// Hit-tests `pointer` (viewport-local points) against the draw list.
#[must_use]
pub fn hit_test(items: &[ClusterItem], pointer: (f32, f32)) -> Option<FeatureHit> {
    let limit = HIT_RADIUS_PX * HIT_RADIUS_PX;
    let mut nearest_single: Option<(f32, usize, usize)> = None;
    for (item_index, item) in items.iter().enumerate() {
        if let ClusterItem::Single { feature, pos } = item {
            let distance = distance_squared(*pos, pointer);
            if distance <= limit && nearest_single.is_none_or(|(best, _, _)| distance < best) {
                nearest_single = Some((distance, item_index, *feature));
            }
        }
    }
    if let Some((_, item, feature)) = nearest_single {
        return Some(FeatureHit::Single { item, feature });
    }
    let mut nearest_cluster: Option<(f32, usize)> = None;
    for (item_index, item) in items.iter().enumerate() {
        if let ClusterItem::Cluster { center, .. } = item {
            let distance = distance_squared(*center, pointer);
            if distance <= limit && nearest_cluster.is_none_or(|(best, _)| distance < best) {
                nearest_cluster = Some((distance, item_index));
            }
        }
    }
    nearest_cluster.map(|(_, item)| FeatureHit::Cluster { item })
}

fn distance_squared(a: (f32, f32), b: (f32, f32)) -> f32 {
    let dx = a.0 - b.0;
    let dy = a.1 - b.1;
    dx * dx + dy * dy
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

/// Paints the draw list at `origin` (the map rect's min corner), with the
/// selected feature ringed — pulsing while its flash window is open.
pub fn paint_items(
    painter: &egui::Painter,
    items: &[ClusterItem],
    catalog: &FeatureCatalog,
    origin: egui::Pos2,
    selected: Option<&FeatureKey>,
    flashing: bool,
) {
    for item in items {
        match item {
            ClusterItem::Single { feature, pos } => {
                let Some(feature) = catalog.features().get(*feature) else {
                    continue;
                };
                let category = category_for(feature);
                let center = origin + egui::vec2(pos.0, pos.1);
                paint_icon(painter, category.icon, center, color32(category.color));
                if selected.is_some_and(|key| key_matches(key, feature)) {
                    let radius = MARKER_RADIUS_PX + if flashing { 6.0 } else { 4.0 };
                    let stroke = if flashing {
                        Stroke::new(2.0_f32, Color32::WHITE)
                    } else {
                        Stroke::new(1.5_f32, Color32::from_rgb(210, 210, 220))
                    };
                    painter.circle_stroke(center, radius, stroke);
                }
            }
            ClusterItem::Cluster {
                center,
                count,
                members,
            } => {
                let pos = origin + egui::vec2(center.0, center.1);
                let color = members
                    .first()
                    .and_then(|member| catalog.features().get(*member))
                    .map_or(Color32::from_rgb(220, 220, 230), |feature| {
                        color32(category_for(feature).color)
                    });
                painter.circle_filled(pos, CLUSTER_RADIUS_PX, color);
                painter.circle_stroke(pos, CLUSTER_RADIUS_PX, Stroke::new(1.5_f32, OUTLINE));
                painter.text(
                    pos,
                    egui::Align2::CENTER_CENTER,
                    count.to_string(),
                    FontId::proportional(10.0),
                    OUTLINE,
                );
                if flashing
                    && selected.is_some_and(|key| {
                        members.iter().any(|member| {
                            catalog
                                .features()
                                .get(*member)
                                .is_some_and(|feature| key_matches(key, feature))
                        })
                    })
                {
                    painter.circle_stroke(
                        pos,
                        CLUSTER_RADIUS_PX + 4.0,
                        Stroke::new(2.0_f32, Color32::WHITE),
                    );
                }
            }
        }
    }
}

fn color32(rgba: [u8; 4]) -> Color32 {
    Color32::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], rgba[3])
}

/// Draws one category icon at `center`: a filled shape in the category colour
/// with a dark outline (T23's "vector icon with contrast" rule).
fn paint_icon(painter: &egui::Painter, icon: FeatureIcon, center: Pos2, color: Color32) {
    let dark = Stroke::new(1.0_f32, OUTLINE);
    let r = MARKER_RADIUS_PX;
    match icon {
        FeatureIcon::Circle => {
            painter.circle_filled(center, r, color);
            painter.circle_stroke(center, r, dark);
        }
        FeatureIcon::Diamond => {
            let points = vec![
                Pos2::new(center.x, center.y - r - 1.0),
                Pos2::new(center.x + r + 1.0, center.y),
                Pos2::new(center.x, center.y + r + 1.0),
                Pos2::new(center.x - r - 1.0, center.y),
            ];
            painter.add(egui::Shape::convex_polygon(points, color, dark));
        }
        FeatureIcon::House => {
            let body = egui::Rect::from_min_max(
                Pos2::new(center.x - r * 0.75, center.y - r * 0.1),
                Pos2::new(center.x + r * 0.75, center.y + r),
            );
            painter.rect_filled(body, 0.5, color);
            painter.rect_stroke(body, 0.5, dark, egui::StrokeKind::Middle);
            let roof = vec![
                Pos2::new(center.x - r, center.y - r * 0.1),
                Pos2::new(center.x, center.y - r * 1.25),
                Pos2::new(center.x + r, center.y - r * 0.1),
            ];
            painter.add(egui::Shape::convex_polygon(roof, color, dark));
        }
        FeatureIcon::Cross => {
            let arm = r + 1.5;
            for stroke in [Stroke::new(3.6_f32, OUTLINE), Stroke::new(1.8_f32, color)] {
                painter.line_segment(
                    [
                        Pos2::new(center.x - arm, center.y - arm),
                        Pos2::new(center.x + arm, center.y + arm),
                    ],
                    stroke,
                );
                painter.line_segment(
                    [
                        Pos2::new(center.x + arm, center.y - arm),
                        Pos2::new(center.x - arm, center.y + arm),
                    ],
                    stroke,
                );
            }
        }
        FeatureIcon::Star => {
            let points = star_points(center, r + 1.5);
            let mut closed = points.clone();
            closed.push(points[0]);
            painter.add(egui::Shape::line(
                closed.clone(),
                Stroke::new(3.6_f32, OUTLINE),
            ));
            painter.add(egui::Shape::line(closed, Stroke::new(1.8_f32, color)));
        }
        FeatureIcon::Cave => {
            let points = arch_points(center, r);
            painter.add(egui::Shape::line(
                points.clone(),
                Stroke::new(3.6_f32, OUTLINE),
            ));
            painter.add(egui::Shape::line(points, Stroke::new(1.8_f32, color)));
        }
    }
}

/// A four-point star: eight alternating radii, point-up.
fn star_points(center: Pos2, outer: f32) -> Vec<Pos2> {
    (0..8)
        .map(|index| {
            let angle = -std::f32::consts::FRAC_PI_2 + index as f32 * std::f32::consts::FRAC_PI_4;
            let radius = if index % 2 == 0 { outer } else { outer * 0.4 };
            Pos2::new(
                center.x + radius * angle.cos(),
                center.y + radius * angle.sin(),
            )
        })
        .collect()
}

/// A cave-mouth arch: a half circle with feet, drawn as a sampled polyline.
fn arch_points(center: Pos2, radius: f32) -> Vec<Pos2> {
    let mut points: Vec<Pos2> = (0..=8)
        .map(|index| {
            let angle = std::f32::consts::PI * (1.0 + index as f32 / 8.0);
            Pos2::new(
                center.x + radius * angle.cos(),
                center.y + radius * angle.sin(),
            )
        })
        .collect();
    // Close the arch along its base so it reads as a mouth, not a rainbow.
    points.push(Pos2::new(center.x - radius, center.y + radius * 0.4));
    points
}

/// The hover tooltip's body: name, category, coordinates and the distance
/// from the view center in blocks.
#[must_use]
pub fn tooltip_text(feature: &Feature, view: &MapViewState) -> String {
    let category = category_for(feature);
    let (x, _, z) = feature.position;
    let dx = f64::from(x) - view.center().x;
    let dz = f64::from(z) - view.center().z;
    let distance = (dx * dx + dz * dz).sqrt().round() as u64;
    format!(
        "{name}\n{category}\nX:{x} Z:{z}\n{distance} {unit}",
        name = feature.name,
        category = category.display_name,
        unit = labels::FEATURE_BLOCKS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use map_core::coords::BlockPos;
    use map_core::types::{FeatureKindMask, FeatureMetadata};
    use std::sync::mpsc;

    fn feature(
        kind: FeatureKind,
        name: &str,
        category: &str,
        position: (i32, i32, i32),
    ) -> Feature {
        Feature {
            kind,
            name: name.to_string(),
            category: category.to_string(),
            position,
            bounds: None,
            metadata: FeatureMetadata {
                variant: None,
                size: None,
                source: Some("test".to_string()),
            },
        }
    }

    fn view_at(center: (f64, f64), smooth_zoom: f32, width: f32, height: f32) -> MapViewState {
        let mut view = MapViewState::new();
        view.set_size(width, height);
        view.set_center(center.0, center.1);
        if smooth_zoom != 0.0 {
            view.zoom_at_center(smooth_zoom);
        }
        view
    }

    fn all_settings() -> OverlaySettings {
        OverlaySettings::default()
    }

    #[test]
    fn features_planner_debounces_until_the_view_settles() {
        let view = view_at((0.0, 0.0), 0.0, 800.0, 600.0);
        let signature = FeatureFetchPlanner::viewport_signature(&view);
        let deadline = Some(SETTLE_DEBOUNCE_MS);

        assert!(
            !FeatureFetchPlanner::should_fetch(0, deadline, &signature, None),
            "a signature that just changed is not settled"
        );
        assert!(
            !FeatureFetchPlanner::should_fetch(SETTLE_DEBOUNCE_MS - 1, deadline, &signature, None),
            "one millisecond before the deadline nothing is wanted"
        );
        assert!(
            FeatureFetchPlanner::should_fetch(SETTLE_DEBOUNCE_MS, deadline, &signature, None),
            "the settled view fetches"
        );
        assert!(
            !FeatureFetchPlanner::should_fetch(10_000, deadline, &signature, Some(&signature)),
            "an already-served signature never fetches twice"
        );

        // The state machine's observe() restarts the window on a change.
        let mut state = FeatureOverlayState::new();
        let t0 = Instant::now() + Duration::from_secs(1);
        state.observe(t0, &view);
        assert!(!state.fetch_due(t0, &view));
        assert!(!state.fetch_due(t0 + Duration::from_millis(SETTLE_DEBOUNCE_MS - 1), &view));
        assert!(state.fetch_due(t0 + Duration::from_millis(SETTLE_DEBOUNCE_MS), &view));
    }

    #[test]
    fn features_planner_replaces_after_a_second_change() {
        let first = view_at((0.0, 0.0), 0.0, 800.0, 600.0);
        let mut second = first;
        second.pan_by_screen((2000.0, 0.0));
        let mut state = FeatureOverlayState::new();
        let t0 = Instant::now() + Duration::from_secs(1);
        state.observe(t0, &first);
        // A second change 100 ms later must move the deadline to +250 ms.
        state.observe(t0 + Duration::from_millis(100), &second);
        assert!(
            !state.fetch_due(t0 + Duration::from_millis(200), &second),
            "the first deadline must not fire for the moved view"
        );
        assert!(state.fetch_due(
            t0 + Duration::from_millis(SETTLE_DEBOUNCE_MS + 100),
            &second
        ));

        // A reply issued for the old view is not wanted once the view moved.
        let old_visible = FeatureFetchPlanner::visible_rect(&first);
        let new_visible = FeatureFetchPlanner::visible_rect(&second);
        let reply_rect =
            FeatureFetchPlanner::desired_rect(&first, FeatureFetchPlanner::margin_blocks(&first));
        assert!(
            !FeatureFetchPlanner::wants_reply(reply_rect, new_visible),
            "the old reply rect no longer covers the new visible rect"
        );
        // The same rect does cover the view it was issued for.
        assert!(FeatureFetchPlanner::wants_reply(reply_rect, old_visible));
    }

    #[test]
    fn features_planner_skips_oversized_rects() {
        // The app's default 1280x800 window at the finest zoom must still be
        // fetchable: the margin clamps to the cap instead of pushing the
        // desired rect past it (a full one-viewport margin would be 3840).
        let default_view = view_at((0.0, 0.0), 0.0, 1280.0, 800.0);
        let default_rect = FeatureFetchPlanner::desired_rect(
            &default_view,
            FeatureFetchPlanner::margin_blocks(&default_view),
        );
        assert!(
            !FeatureFetchPlanner::oversized(&default_rect),
            "the default window fits the cap: {default_rect:?}"
        );

        // Far out, one viewport is millions of blocks wide.
        let view = view_at((0.0, 0.0), 20.0, 800.0, 600.0);
        let rect =
            FeatureFetchPlanner::desired_rect(&view, FeatureFetchPlanner::margin_blocks(&view));
        assert!(
            FeatureFetchPlanner::oversized(&rect),
            "the zoomed-out rect exceeds the cap: {rect:?}"
        );

        // The last catalog survives: plan_fetch declines and keeps it.
        let mut state = FeatureOverlayState::new();
        let kept = Arc::new(FeatureCatalog::build(vec![feature(
            FeatureKind::UniquePrefab,
            "Keep",
            "Unique",
            (0, 0, 0),
        )]));
        state.adopt(Arc::clone(&kept), Rect::new(0, 0, 0, 0));
        let t0 = Instant::now() + Duration::from_secs(1);
        state.observe(t0, &view);
        assert_eq!(
            state.plan_fetch(
                t0 + Duration::from_millis(SETTLE_DEBOUNCE_MS + 1),
                &view,
                &all_settings()
            ),
            None,
            "no query is planned beyond the cap"
        );
        assert!(
            state
                .catalog()
                .is_some_and(|catalog| catalog.features()[0].name == "Keep"),
            "the previous catalog is still active"
        );
    }

    #[test]
    fn features_fetch_filter_drops_disabled_groups() {
        let settings = OverlaySettings {
            structures: true,
            prefabs: false,
            spawn: false,
            poi: true,
            cave_entrances: false,
            chunk_grid: false,
        };
        let filter = fetch_filter(&settings);
        assert!(
            filter.kinds.contains(FeatureKind::UniquePrefab),
            "structures stay fetched"
        );
        assert!(
            !filter.kinds.contains(FeatureKind::BiomePrefab),
            "disabled prefabs are never queried"
        );
        assert!(!filter.kinds.contains(FeatureKind::Spawn));
        assert!(filter.kinds.contains(FeatureKind::Poi));
        assert!(!filter.kinds.contains(FeatureKind::CaveEntrance));

        // All-on is the map-core default: every kind.
        let all = fetch_filter(&OverlaySettings::default());
        assert_eq!(all.kinds, FeatureKindMask::ALL);

        // The mask mirrors the checkbox rows one-for-one.
        let mask = toggle_mask(&settings);
        for group in FeatureToggleGroup::ALL {
            let expected = match group {
                FeatureToggleGroup::Structures => settings.structures,
                FeatureToggleGroup::Prefabs => settings.prefabs,
                FeatureToggleGroup::Spawn => settings.spawn,
                FeatureToggleGroup::Poi => settings.poi,
                FeatureToggleGroup::Cave => settings.cave_entrances,
            };
            assert_eq!(mask.contains(group), expected, "{group:?}");
        }
    }

    #[test]
    fn features_screen_projection_matches_the_viewport_transform() {
        // The T10 view used across the view-state tests: center (-1203, 455),
        // scale 0.5, 800x600. The center block maps to the viewport center.
        let view = view_at((-1203.0, 455.0), 1.0, 800.0, 600.0);
        let center = block_to_screen(&view, -1203, 455).expect("the center is representable");
        assert_eq!(center, (400.0, 300.0));

        // A negative-coordinate neighbour: at 0.5 pt/blk one block is half a
        // point, and the inverse floor lands back on the same block.
        let neighbour = block_to_screen(&view, -1204, 454).expect("inside the block space");
        assert!((neighbour.0 - 399.5).abs() < 1e-4, "{neighbour:?}");
        assert!((neighbour.1 - 299.5).abs() < 1e-4, "{neighbour:?}");
        assert_eq!(
            view.screen_to_block(neighbour),
            Some(BlockPos::new(-1204, 454)),
            "projection and screen_to_block invert on the same point"
        );

        // And it matches the viewport formula the tile painter uses.
        let expected = view
            .viewport()
            .world_to_screen(map_core::coords::WorldPos::new(
                f64::from(-1204),
                f64::from(454),
            ));
        assert_eq!(neighbour, (expected.x, expected.y));
    }

    #[test]
    fn features_draw_list_excludes_disabled_groups_and_culls_outside() {
        let catalog = FeatureCatalog::build(vec![
            feature(FeatureKind::UniquePrefab, "Temple", "Unique", (0, 0, 0)),
            feature(FeatureKind::BiomePrefab, "Hut", "Biome", (10, 0, 0)),
            feature(FeatureKind::UniquePrefab, "Far", "Unique", (10_000, 0, 0)),
        ]);
        let view = view_at((0.0, 0.0), 0.0, 800.0, 600.0);
        let items = marker_list(
            &catalog,
            FeatureToggleMask::from_group(FeatureToggleGroup::Structures),
            &view,
            (800.0, 600.0),
            CoreOverlayState::DEFAULT_CLUSTER_CELL_PX,
        );
        let singles: Vec<usize> = items
            .iter()
            .filter_map(|item| match item {
                ClusterItem::Single { feature, .. } => Some(*feature),
                ClusterItem::Cluster { .. } => None,
            })
            .collect();
        assert_eq!(
            singles,
            vec![0],
            "only the enabled, on-screen structure draws"
        );

        // With prefabs on, the nearby hut joins; the far temple stays culled.
        let items = marker_list(
            &catalog,
            FeatureToggleMask::all(),
            &view,
            (800.0, 600.0),
            CoreOverlayState::DEFAULT_CLUSTER_CELL_PX,
        );
        let drawn: Vec<usize> = items
            .iter()
            .flat_map(|item| match item {
                ClusterItem::Single { feature, .. } => vec![*feature],
                ClusterItem::Cluster { members, .. } => members.clone(),
            })
            .collect();
        assert!(drawn.contains(&0) && drawn.contains(&1));
        assert!(!drawn.contains(&2), "the off-screen feature is culled");
    }

    #[test]
    fn features_draw_list_clusters_and_caps() {
        // Two features four pixels apart cluster into one count-2 marker.
        let catalog = FeatureCatalog::build(vec![
            feature(FeatureKind::UniquePrefab, "A", "Unique", (0, 0, 0)),
            feature(FeatureKind::UniquePrefab, "B", "Unique", (4, 0, 0)),
        ]);
        let view = view_at((0.0, 0.0), 0.0, 800.0, 600.0);
        let items = marker_list(
            &catalog,
            FeatureToggleMask::all(),
            &view,
            (800.0, 600.0),
            CoreOverlayState::DEFAULT_CLUSTER_CELL_PX,
        );
        assert_eq!(items.len(), 1);
        assert!(
            matches!(&items[0], ClusterItem::Cluster { count: 2, .. }),
            "two nearby features share one cluster: {items:?}"
        );

        // A wide field of singles caps at MAX_DRAWN_SINGLES.
        let mut features = Vec::new();
        for index in 0..(MAX_DRAWN_SINGLES + 101) {
            features.push(feature(
                FeatureKind::UniquePrefab,
                &format!("F{index}"),
                "Unique",
                ((index as i32 - 800) * 30, 0, 0),
            ));
        }
        let wide = FeatureCatalog::build(features);
        let wide_view = view_at((0.0, 0.0), 0.0, 50_000.0, 600.0);
        let items = marker_list(
            &wide,
            FeatureToggleMask::all(),
            &wide_view,
            (50_000.0, 600.0),
            CoreOverlayState::DEFAULT_CLUSTER_CELL_PX,
        );
        let singles = items
            .iter()
            .filter(|item| matches!(item, ClusterItem::Single { .. }))
            .count();
        assert_eq!(
            singles, MAX_DRAWN_SINGLES,
            "the draw list caps singles at the per-frame budget"
        );
        assert_eq!(items.len(), MAX_DRAWN_SINGLES, "no clusters in this field");
    }

    #[test]
    fn features_source_change_drops_the_catalog_and_cache() {
        // The acceptance rule: no feature from a previous seed/version may
        // survive a source change.
        let mut state = FeatureOverlayState::new();
        let rect = Rect::new(0, 0, 100, 100);
        let filter = fetch_filter(&OverlaySettings::default());
        let catalog = Arc::new(FeatureCatalog::build(vec![feature(
            FeatureKind::UniquePrefab,
            "Old World",
            "Unique",
            (1, 0, 1),
        )]));
        state.cache.put(rect, filter.clone(), Arc::clone(&catalog));
        state.adopt(Arc::clone(&catalog), rect);
        state.select(&catalog.features()[0], Instant::now());
        assert!(state.catalog().is_some());

        state.set_source(&GeneratorIdentity {
            generator_version: 2,
            world_assets: "Default/0.0.0".to_string(),
            palette_version: 1,
        });
        assert!(state.catalog().is_none(), "the old catalog is gone");
        assert_eq!(state.catalog_rect(), None);
        assert!(state.selected_key().is_none());
        assert!(
            state.cache.get(rect, &filter).is_none(),
            "the old world's cached query is gone"
        );
    }

    #[test]
    fn features_state_schedules_and_applies_a_fake_reply() {
        let mut state = FeatureOverlayState::new();
        let settings = all_settings();
        // Small enough that one-viewport margin stays under the 2048 cap.
        let mut view = view_at((0.0, 0.0), 0.0, 400.0, 300.0);
        let t0 = Instant::now() + Duration::from_secs(1);

        state.observe(t0, &view);
        assert!(
            state.plan_fetch(t0, &view, &settings).is_none(),
            "the first frame is still inside the debounce window"
        );

        let settled = t0 + Duration::from_millis(SETTLE_DEBOUNCE_MS + 1);
        let request = state
            .plan_fetch(settled, &view, &settings)
            .expect("a settled, under-cap view plans one query");
        let (sender, receiver) = mpsc::channel();
        state.begin_fetch(request.clone(), receiver);
        assert!(state.has_fetch_in_flight());

        // The worker's fake reply goes through the same apply path the UI
        // polls (no real source needed).
        sender
            .send(Ok(vec![feature(
                FeatureKind::UniquePrefab,
                "Temple",
                "Unique",
                (0, 0, 0),
            )]))
            .expect("the poll receiver is alive");
        assert_eq!(
            state.poll(settled + Duration::from_millis(1), &view),
            FeatureReplyOutcome::Applied(1)
        );
        assert_eq!(state.catalog().map(FeatureCatalog::len), Some(1));
        assert_eq!(state.catalog_rect(), Some(request.rect));
        assert!(!state.has_fetch_in_flight(), "the slot is free again");
        assert!(
            !state.fetch_due(settled + Duration::from_secs(1), &view),
            "the just-served signature needs no second query"
        );

        // A pan restarts the debounce and eventually plans a new rect.
        view.pan_by_screen((500.0, 0.0));
        state.observe(settled + Duration::from_millis(2), &view);
        assert!(
            state
                .plan_fetch(settled + Duration::from_millis(3), &view, &settings)
                .is_none()
        );
        assert!(
            state
                .plan_fetch(
                    settled + Duration::from_millis(SETTLE_DEBOUNCE_MS + 3),
                    &view,
                    &settings
                )
                .is_some(),
            "the moved view fetches once settled"
        );
    }
}
