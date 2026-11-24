//! T25 acceptance tests: the biome search engine — ring sampling, frontier
//! filtering, boundary refinement, patch area, cancellation, streaming —
//! against a synthetic `WorldSource`. The worldgen-backed fixture check and
//! benchmark live in `tests/biome_search_fixture.rs`.
//!
//! The synthetic [`FieldSource`] answers only the queries the biome search
//! makes (`biome_at`/`biome_region`); every other `WorldSource` method is
//! [`SourceError::Unsupported`]. Its field is a pure function of (x, z), so
//! a result's "true reading" can be re-verified directly against the same
//! closure.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use map_core::catalog::{BiomeCatalog, BlockCatalog, FluidCatalog, ZoneCatalog};
use map_core::error::{SourceError, SourceResult};
use map_core::scheduler::{CancelToken, cancel_token};
use map_core::search::biome::{
    BiomeMatch, BiomeSearch, BiomeSearchEvent, BiomeSearchStats, run_biome_search,
    run_biome_search_blocking,
};
use map_core::source::SummaryFidelity;
use map_core::source::WorldSource;
use map_core::types::{
    BiomeGrid, BiomeId, GeneratorIdentity, HeightGrid, Rect, Seed, SliceGrid, SpawnPoint,
    SurfaceGrid, SurfaceSample, ZoneGrid,
};

// ---------------------------------------------------------------------------
// The synthetic source
// ---------------------------------------------------------------------------

/// A pure-function biome field: the queries the biome search makes are
/// served exactly per the `WorldSource` contract; everything else is
/// [`SourceError::Unsupported`].
struct FieldSource {
    field: Box<dyn Fn(i32, i32) -> BiomeId + Send + Sync>,
    /// Optional per-query sleep, for the spawned-handle tests.
    delay: Duration,
    /// Fires in [`Drop`] so a test can observe when the spawned worker
    /// releases the source (the no-thread-leak witness).
    drop_probe: Option<Arc<AtomicUsize>>,
    identity: GeneratorIdentity,
    seed: Seed,
    biome_catalog: BiomeCatalog,
    block_catalog: BlockCatalog,
    fluid_catalog: FluidCatalog,
    zone_catalog: ZoneCatalog,
}

impl FieldSource {
    fn new(field: impl Fn(i32, i32) -> BiomeId + Send + Sync + 'static) -> Self {
        Self {
            field: Box::new(field),
            delay: Duration::ZERO,
            drop_probe: None,
            identity: GeneratorIdentity {
                generator_version: 1,
                world_assets: "Default/0.0.0".to_string(),
                palette_version: 1,
            },
            seed: Seed::parse("1").expect("the fake's seed parses"),
            biome_catalog: BiomeCatalog::default(),
            block_catalog: BlockCatalog::default(),
            fluid_catalog: FluidCatalog::default(),
            zone_catalog: ZoneCatalog::default(),
        }
    }

    /// The same field with a per-query sleep — the spawned-handle tests
    /// need a scan slow enough to cancel or drop while it is running.
    fn with_delay(
        field: impl Fn(i32, i32) -> BiomeId + Send + Sync + 'static,
        delay: Duration,
    ) -> Self {
        let mut source = Self::new(field);
        source.delay = delay;
        source
    }

    fn unsupported<T>(&self) -> SourceResult<T> {
        Err(SourceError::Unsupported(
            "the synthetic field serves biome queries only",
        ))
    }
}

impl Drop for FieldSource {
    fn drop(&mut self) {
        if let Some(probe) = self.drop_probe.take() {
            probe.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl WorldSource for FieldSource {
    fn identity(&self) -> &GeneratorIdentity {
        &self.identity
    }
    fn seed(&self) -> &Seed {
        &self.seed
    }

    fn biome_at(&self, x: i32, z: i32) -> SourceResult<BiomeId> {
        Ok((self.field)(x, z))
    }
    fn biome_region(&self, rect: Rect, step: u32) -> SourceResult<BiomeGrid> {
        if !self.delay.is_zero() {
            thread::sleep(self.delay);
        }
        let (width, height) = rect.sample_dims(step);
        let mut biomes = Vec::with_capacity(width as usize * height as usize);
        for (x, z) in rect.iter_blocks(step) {
            biomes.push((self.field)(x, z));
        }
        Ok(BiomeGrid::new(
            rect.min_x, rect.min_z, step, width, height, biomes,
        ))
    }

    fn height_at(&self, _x: i32, _z: i32) -> SourceResult<i32> {
        self.unsupported()
    }
    fn height_region(&self, _rect: Rect, _step: u32) -> SourceResult<HeightGrid> {
        self.unsupported()
    }
    fn surface_at(&self, _x: i32, _z: i32) -> SourceResult<SurfaceSample> {
        self.unsupported()
    }
    fn surface_region(
        &self,
        _rect: Rect,
        _step: u32,
        _fidelity: SummaryFidelity,
    ) -> SourceResult<SurfaceGrid> {
        self.unsupported()
    }
    fn slice_region(&self, _rect: Rect, _step: u32, _y: i32) -> SourceResult<SliceGrid> {
        self.unsupported()
    }
    fn spawn_points(&self) -> SourceResult<Vec<SpawnPoint>> {
        self.unsupported()
    }
    fn zone_at(&self, _x: i32, _z: i32) -> SourceResult<Option<map_core::types::ZoneId>> {
        self.unsupported()
    }
    fn zone_region(&self, _rect: Rect, _step: u32) -> SourceResult<ZoneGrid> {
        self.unsupported()
    }

    fn biome_catalog(&self) -> &BiomeCatalog {
        &self.biome_catalog
    }
    fn block_catalog(&self) -> &BlockCatalog {
        &self.block_catalog
    }
    fn fluid_catalog(&self) -> &FluidCatalog {
        &self.fluid_catalog
    }
    fn zone_catalog(&self) -> &ZoneCatalog {
        &self.zone_catalog
    }
}

// ---------------------------------------------------------------------------
// Query-boundary wrappers
// ---------------------------------------------------------------------------

/// What a wrapper does to the biome queries it forwards — the three injected
/// faults the engine's cancellation and error paths must survive.
enum Intercept {
    /// Flips `token` once the query count reaches `after`: the mid-scan
    /// cancellation hook (the engine checks the token between strips).
    CancelAfter { after: usize, token: CancelToken },
    /// Fails the `on`-th query (1-based) with a generation error.
    FailOn { on: usize },
    /// Panics inside the query boundary.
    Panic,
}

/// Wraps a shared [`FieldSource`], counting its `biome_region` calls and
/// applying [`Intercept`] to them. Every other query method is
/// [`SourceError::Unsupported`]: the biome search never calls them, so the
/// wrappers only need the boundary under test. Identity and the catalogs
/// cannot fail and are borrowed, so they pass straight through.
struct InterceptSource {
    inner: Arc<FieldSource>,
    intercept: Intercept,
    calls: AtomicUsize,
}

impl InterceptSource {
    fn new(inner: Arc<FieldSource>, intercept: Intercept) -> Self {
        Self {
            inner,
            intercept,
            calls: AtomicUsize::new(0),
        }
    }

    /// The number of `biome_region` calls the wrapper has seen.
    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    fn unsupported<T>(&self) -> SourceResult<T> {
        Err(SourceError::Unsupported(
            "the intercepting wrapper serves biome queries only",
        ))
    }
}

impl WorldSource for InterceptSource {
    fn identity(&self) -> &GeneratorIdentity {
        self.inner.identity()
    }
    fn seed(&self) -> &Seed {
        self.inner.seed()
    }

    fn biome_at(&self, x: i32, z: i32) -> SourceResult<BiomeId> {
        self.inner.biome_at(x, z)
    }
    fn biome_region(&self, rect: Rect, step: u32) -> SourceResult<BiomeGrid> {
        let count = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        match &self.intercept {
            Intercept::Panic => panic!("injected panic at the biome query boundary"),
            Intercept::FailOn { on } => {
                if count == *on {
                    return Err(SourceError::Generation(
                        "injected query failure".to_string(),
                    ));
                }
                self.inner.biome_region(rect, step)
            }
            Intercept::CancelAfter { after, token } => {
                let answer = self.inner.biome_region(rect, step);
                if count >= *after {
                    token.store(true, Ordering::Relaxed);
                }
                answer
            }
        }
    }

    fn height_at(&self, _x: i32, _z: i32) -> SourceResult<i32> {
        self.unsupported()
    }
    fn height_region(&self, _rect: Rect, _step: u32) -> SourceResult<HeightGrid> {
        self.unsupported()
    }
    fn surface_at(&self, _x: i32, _z: i32) -> SourceResult<SurfaceSample> {
        self.unsupported()
    }
    fn surface_region(
        &self,
        _rect: Rect,
        _step: u32,
        _fidelity: SummaryFidelity,
    ) -> SourceResult<SurfaceGrid> {
        self.unsupported()
    }
    fn slice_region(&self, _rect: Rect, _step: u32, _y: i32) -> SourceResult<SliceGrid> {
        self.unsupported()
    }
    fn spawn_points(&self) -> SourceResult<Vec<SpawnPoint>> {
        self.unsupported()
    }
    fn zone_at(&self, _x: i32, _z: i32) -> SourceResult<Option<map_core::types::ZoneId>> {
        self.unsupported()
    }
    fn zone_region(&self, _rect: Rect, _step: u32) -> SourceResult<ZoneGrid> {
        self.unsupported()
    }

    fn biome_catalog(&self) -> &BiomeCatalog {
        self.inner.biome_catalog()
    }
    fn block_catalog(&self) -> &BlockCatalog {
        self.inner.block_catalog()
    }
    fn fluid_catalog(&self) -> &FluidCatalog {
        self.inner.fluid_catalog()
    }
    fn zone_catalog(&self) -> &ZoneCatalog {
        self.inner.zone_catalog()
    }
}

// ---------------------------------------------------------------------------
// Field shapes
// ---------------------------------------------------------------------------

const BACKGROUND: BiomeId = BiomeId(1);
const TARGET: BiomeId = BiomeId(7);

/// A field whose only biome-7 ground is the axis-aligned block rectangle
/// `[min_x, max_x] × [min_z, max_z]` (inclusive). Patches at least one
/// coarse step wide on both axes are always coarse-detectable: any span of
/// `step` consecutive blocks contains a lattice point.
fn rect_field(min_x: i32, min_z: i32, max_x: i32, max_z: i32) -> impl Fn(i32, i32) -> BiomeId {
    move |x, z| {
        if (min_x..=max_x).contains(&x) && (min_z..=max_z).contains(&z) {
            TARGET
        } else {
            BACKGROUND
        }
    }
}

/// A field whose biome-7 ground is the union of inclusive block rectangles
/// (everything else is [`BACKGROUND`]) — the multi-patch shapes the ordering,
/// lookahead and patch-area tests need.
fn union_field(rects: Vec<(i32, i32, i32, i32)>) -> impl Fn(i32, i32) -> BiomeId {
    move |x, z| {
        let matched = rects.iter().any(|&(min_x, min_z, max_x, max_z)| {
            (min_x..=max_x).contains(&x) && (min_z..=max_z).contains(&z)
        });
        if matched { TARGET } else { BACKGROUND }
    }
}

// ---------------------------------------------------------------------------
// Event helpers
// ---------------------------------------------------------------------------

/// Runs a search synchronously and collects every event.
fn collect(
    source: &dyn WorldSource,
    search: &BiomeSearch,
) -> (Vec<BiomeSearchEvent>, BiomeSearchStats) {
    let mut events = Vec::new();
    let stats = run_biome_search_blocking(source, search, &cancel_token(), |event| {
        events.push(event);
        true
    });
    (events, stats)
}

/// Runs a search synchronously with a caller-supplied token — the
/// cancellation test's source flips the same token mid-scan.
fn collect_with_token(
    source: &dyn WorldSource,
    search: &BiomeSearch,
    token: &CancelToken,
) -> (Vec<BiomeSearchEvent>, BiomeSearchStats) {
    let mut events = Vec::new();
    let stats = run_biome_search_blocking(source, search, token, |event| {
        events.push(event);
        true
    });
    (events, stats)
}

/// Every result event, in arrival order.
fn results_of(events: &[BiomeSearchEvent]) -> Vec<BiomeMatch> {
    events
        .iter()
        .filter_map(|event| match event {
            BiomeSearchEvent::Result(result) => Some(*result),
            _ => None,
        })
        .collect()
}

/// The single `Done` of a completed search.
fn done_of(events: &[BiomeSearchEvent]) -> BiomeSearchStats {
    events
        .iter()
        .find_map(|event| match event {
            BiomeSearchEvent::Done(stats) => Some(*stats),
            _ => None,
        })
        .expect("a search that was not cancelled or failed sends exactly one Done")
}

/// A default search for [`TARGET`] around `center` within `radius`.
fn search_around(center: (f64, f64), radius: f64) -> BiomeSearch {
    BiomeSearch {
        biome: TARGET,
        center,
        max_radius: radius,
        ..BiomeSearch::default()
    }
}

// ---------------------------------------------------------------------------
// The exactness core
// ---------------------------------------------------------------------------

#[test]
fn biome_search_finds_the_nearest_block_not_the_lattice_point() {
    // One 160×160 patch northeast of the center: every matched interior
    // cell is shadowed except the frontier, and the refinement must return
    // the patch's true nearest block (its corner), not the matched lattice
    // point — a real `biome_at` reading by construction.
    let source = FieldSource::new(rect_field(40, 40, 199, 199));
    let (events, stats) = collect(&source, &search_around((0.0, 0.0), 500.0));

    let results = results_of(&events);
    assert_eq!(
        results.len(),
        1,
        "the frontier filter collapses one contiguous patch to its near edge",
    );
    let result = results[0];
    assert_eq!(
        result.position,
        (40, 40),
        "the patch's nearest block is its corner"
    );
    assert!(
        (result.distance - 40.0_f64 * std::f64::consts::SQRT_2).abs() < 1e-9,
        "the distance is measured to the search center, got {}",
        result.distance,
    );
    // The refined position is a true reading, verified against the field.
    assert_eq!(
        source.biome_at(result.position.0, result.position.1),
        Ok(TARGET),
        "the emitted position really is the requested biome",
    );
    assert_eq!(
        source.biome_at(39, 39),
        Ok(BACKGROUND),
        "just outside is not"
    );
    assert_eq!(result.patch_area, None, "no patch minimum was requested");
    assert!(!result.area_capped);

    let done = done_of(&events);
    assert_eq!(done.results, 1);
    assert!(!done.truncated, "the scan exhausted the radius");
    assert!(!done.cancelled);
    assert_eq!(stats.results, 1);
    assert!(
        matches!(events.first(), Some(BiomeSearchEvent::Started)),
        "the stream opens with Started",
    );
}

#[test]
fn biome_search_handles_negative_and_fractional_centers() {
    // The patch surrounds a negative, fractional center; the nearest block
    // is unique because neither fraction is exactly one half.
    let source = FieldSource::new(rect_field(-100, -100, 29, 29));
    let (events, _) = collect(&source, &search_around((-30.4, -30.25), 400.0));

    let results = results_of(&events);
    assert_eq!(
        results.len(),
        1,
        "one contiguous patch, one frontier result"
    );
    assert_eq!(
        results[0].position,
        (-30, -30),
        "the block nearest the fractional center",
    );
    assert!(
        (results[0].distance - (0.4_f64 * 0.4 + 0.25 * 0.25).sqrt()).abs() < 1e-9,
        "distance to the fractional center, got {}",
        results[0].distance,
    );
}

// ---------------------------------------------------------------------------
// Streaming order: the safe-emission lookahead
// ---------------------------------------------------------------------------

#[test]
fn biome_search_lookahead_emits_a_closer_later_ring_first() {
    // Two patches straddle the lattice:
    // P1 = [81, 111]² holds exactly one lattice point, (96, 96) — ring 6 —
    //      and refines to its corner (81, 81) at 81·√2 ≈ 114.55.
    // P2 = x ∈ [97, 127], z ∈ [-4, 20] holds (112, 0) and (112, 16) — ring
    //      7 — where (112, 16) is shadowed by (112, 0); (112, 0) refines to
    //      (97, 0) at 97.
    // A ring-by-ring emitter would have emitted P1 the moment ring 6 closed.
    // The threshold after ring 6 is ring_min(7) − 15·√2 = 97 − 21.21 = 75.79,
    // so 114.55 must wait; ring 7's strip then proves the nearer 97.
    let source = FieldSource::new(union_field(vec![(81, 81, 111, 111), (97, -4, 127, 20)]));
    let (events, stats) = collect(&source, &search_around((0.0, 0.0), 400.0));

    let results = results_of(&events);
    assert_eq!(
        results.len(),
        2,
        "one frontier result per patch, got {results:?}",
    );
    assert_eq!(
        results[0].position,
        (97, 0),
        "the later ring's provably nearer block is emitted first",
    );
    assert!(
        (results[0].distance - 97.0).abs() < 1e-6,
        "P2's nearest block is 97 blocks out, got {}",
        results[0].distance,
    );
    assert_eq!(results[1].position, (81, 81), "P1's corner follows");
    assert!(
        (results[1].distance - 81.0 * std::f64::consts::SQRT_2).abs() < 1e-6,
        "P1's corner is 81·√2 ≈ 114.5513 blocks out, got {}",
        results[1].distance,
    );
    for result in &results {
        assert_eq!(
            source.biome_at(result.position.0, result.position.1),
            Ok(TARGET),
            "{:?} is a true reading of the field",
            result.position,
        );
        assert_eq!(result.patch_area, None, "no patch minimum was requested");
        assert!(!result.area_capped);
    }

    let done = done_of(&events);
    assert_eq!(done.results, 2);
    assert!(!done.truncated, "the scan exhausted the radius");
    assert!(!done.cancelled);
    assert_eq!(stats.results, 2);
}

#[test]
fn biome_search_orders_multiple_patches_nearest_first() {
    // Three separated patches; each patch's four lattice cells collapse to
    // its near corner, so the stream holds exactly one result per patch:
    // (60, 60) at 60·√2, (-150, -150) at 150·√2 and (300, 300) at 300·√2.
    let source = FieldSource::new(union_field(vec![
        (60, 60, 80, 80),
        (-170, -170, -150, -150),
        (300, 300, 320, 320),
    ]));
    let (events, _) = collect(&source, &search_around((0.0, 0.0), 500.0));

    let results = results_of(&events);
    assert_eq!(
        results.len(),
        3,
        "one frontier result per patch: {results:?}"
    );
    let positions: Vec<(i32, i32)> = results.iter().map(|result| result.position).collect();
    assert_eq!(
        positions,
        vec![(60, 60), (-150, -150), (300, 300)],
        "each patch contributes its near corner",
    );
    assert_eq!(positions[0], (60, 60), "the nearest patch comes first");
    let distances: Vec<f64> = results.iter().map(|result| result.distance).collect();
    assert!(
        distances.windows(2).all(|pair| pair[0] < pair[1]),
        "the stream is strictly nearest-first, got {distances:?}",
    );
    for result in &results {
        assert_eq!(
            source.biome_at(result.position.0, result.position.1),
            Ok(TARGET),
            "{:?} is a true reading of the field",
            result.position,
        );
        assert_eq!(result.patch_area, None, "no patch minimum was requested");
    }
    assert!(!done_of(&events).truncated);
}

// ---------------------------------------------------------------------------
// The coarse lattice and its documented limits
// ---------------------------------------------------------------------------

#[test]
fn biome_search_narrow_strip_documents_the_coarse_limitation() {
    // (a) A 4-block-wide strip parked between two lattice rows: a sampling
    // cell only reads its lattice point, and no z-multiple of 16 lies in
    // [4, 8], so at step 16 the strip is invisible. That is the documented
    // coarse limitation, not a bug — `coarse_step` is the lever, and halving
    // it quadruples the scan (which (c) demonstrates at step 4).
    let strip = FieldSource::new(rect_field(100, 4, 300, 8));
    let (events, _) = collect(&strip, &search_around((0.0, 0.0), 400.0));

    assert!(
        results_of(&events).is_empty(),
        "a strip that dodges every z-lattice point is missed at step 16",
    );
    let done = done_of(&events);
    assert_eq!(done.results, 0);
    assert_eq!(
        done.rings_scanned, 28,
        "rings 0..=27 cover a 400-block radius plus the (step−1)·√2 lattice \
         reach the walk must outrun it by"
    );
    assert!(!done.truncated, "a miss is a completed scan");
    assert!(!done.cancelled);

    // (b) The same strip moved down to z ∈ [0, 3] includes the lattice row
    // z = 0, so it is found — and refinement returns its true nearest block
    // (100, 0), not the first matched lattice point (112, 0).
    let on_lattice = FieldSource::new(rect_field(100, 0, 300, 3));
    let (events, _) = collect(&on_lattice, &search_around((0.0, 0.0), 400.0));
    let results = results_of(&events);
    assert_eq!(
        results.len(),
        1,
        "one strip, one frontier result: {results:?}"
    );
    assert_eq!(
        results[0].position,
        (100, 0),
        "the strip's true nearest block"
    );
    assert!(
        (results[0].distance - 100.0).abs() < 1e-6,
        "the near end of the strip is 100 blocks out, got {}",
        results[0].distance,
    );

    // (c) The same invisible strip at coarse_step 4: the lattice now holds
    // z = 4 and z = 8, so the near cell (lattice (100, 4)) matches and
    // refines to the true nearest block (100, 4) at √(100² + 4²).
    let stepped = BiomeSearch {
        coarse_step: 4,
        ..search_around((0.0, 0.0), 400.0)
    };
    let (events, _) = collect(&strip, &stepped);
    let results = results_of(&events);
    assert_eq!(
        results.len(),
        1,
        "the thorough-mode step finds the strip: {results:?}",
    );
    assert_eq!(results[0].position, (100, 4));
    assert!(
        (results[0].distance - (100.0_f64 * 100.0 + 16.0).sqrt()).abs() < 1e-6,
        "the near corner of the strip is √10016 blocks out, got {}",
        results[0].distance,
    );
}

// ---------------------------------------------------------------------------
// Patch area
// ---------------------------------------------------------------------------

#[test]
fn biome_search_min_patch_filters_islands_without_hanging() {
    // The island's only lattice point (1008, 1008) floods to one cell — a
    // 256-block² estimate — below the 1,000-block² minimum, so it is
    // dropped. The 13×13-cell blob ([2000, 2200]² holds lattice points
    // 2000..=2192) floods to 169 cells = 43,264 blocks², inside both caps,
    // so its estimate is exact and it survives.
    let source = FieldSource::new(union_field(vec![
        (1000, 1000, 1015, 1015),
        (2000, 2000, 2200, 2200),
    ]));
    let search = BiomeSearch {
        min_patch_blocks: Some(1000),
        ..search_around((0.0, 0.0), 4000.0)
    };
    let (events, _) = collect(&source, &search);

    let results = results_of(&events);
    assert_eq!(
        results.len(),
        1,
        "the island is filtered out, the blob is kept: {results:?}",
    );
    let result = results[0];
    assert_eq!(result.position, (2000, 2000), "the blob's near corner");
    assert_eq!(
        result.patch_area,
        Some(43_264),
        "169 cells × 16² blocks² is the closed patch's estimate",
    );
    assert!(
        !result.area_capped,
        "the flood fill closed inside the ring and cell caps",
    );
    assert!(!done_of(&events).truncated);
}

#[test]
fn biome_search_patch_area_caps_on_huge_patches() {
    // An unbounded-looking blob around the frontier cell: the flood fill
    // walks its 32 rings and stops at PATCH_RING_CAP, so the estimate is a
    // capped lower bound rather than the patch's true area.
    let search = BiomeSearch {
        min_patch_blocks: Some(100),
        ..search_around((0.0, 0.0), 300.0)
    };

    // The corner blob: the origin *is* the patch's corner, so no fill cell
    // lacks an already-accepted inward 4-neighbour and the count is exact —
    // 1 + Σ(2r + 1) for r = 1..=32 = 1,089 cells.
    let (events, _) = collect(&FieldSource::new(rect_field(0, 0, 2000, 2000)), &search);
    let results = results_of(&events);
    assert_eq!(
        results.len(),
        1,
        "one frontier cell, one result: {results:?}"
    );
    assert_eq!(results[0].position, (0, 0));
    assert!(results[0].distance < 1e-6, "the center block is the match");
    assert_eq!(
        results[0].patch_area,
        Some(278_784),
        "1,089 cells × 16² blocks² inside the ring cap",
    );
    assert!(
        results[0].area_capped,
        "PATCH_RING_CAP stopped the fill before the patch closed",
    );

    // The same shape centred on the origin: the true patch within the caps
    // is the full 33×33-cell square (4,225 cells — past PATCH_CELL_CAP), and
    // the fill accepts every one of them: acceptance is a fixpoint closure,
    // not an ordered per-ring pass, so a ring's corners (whose only
    // 4-neighbours arrive later in the strip walk) still join through their
    // in-ring neighbours.
    let (events, _) = collect(
        &FieldSource::new(rect_field(-2000, -2000, 2000, 2000)),
        &search,
    );
    let results = results_of(&events);
    assert_eq!(
        results.len(),
        1,
        "one frontier cell, one result: {results:?}"
    );
    assert_eq!(results[0].position, (0, 0));
    assert_eq!(
        results[0].patch_area,
        Some(1_081_600),
        "4,225 cells (1 + 8·(1 + 2 + … + 32)) × 16² blocks²",
    );
    assert!(
        results[0].area_capped,
        "PATCH_CELL_CAP stopped the fill before the patch closed",
    );

    // Without the criterion there is no flood fill at all: the same field
    // reports no estimate and no cap.
    let (events, _) = collect(
        &FieldSource::new(rect_field(-2000, -2000, 2000, 2000)),
        &search_around((0.0, 0.0), 300.0),
    );
    let results = results_of(&events);
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].patch_area, None,
        "no patch minimum was requested"
    );
    assert!(!results[0].area_capped);
}

// ---------------------------------------------------------------------------
// Caps and termination
// ---------------------------------------------------------------------------

#[test]
fn biome_search_max_results_cap_is_conservative_truncated() {
    // Two patches at 60·√2 and 150·√2; the cap of one is reached at ring 7,
    // when (60, 60) is first provably nearest — rings 8.. still hold the far
    // patch's lattice row, so the scan stopped over unscanned area. That is
    // exactly what the conservative flag claims, and nothing more.
    let source = FieldSource::new(union_field(vec![(60, 60, 80, 80), (150, 150, 180, 180)]));
    let search = BiomeSearch {
        max_results: 1,
        ..search_around((0.0, 0.0), 500.0)
    };
    let (events, _) = collect(&source, &search);

    let results = results_of(&events);
    assert_eq!(results.len(), 1, "the cap emits one result: {results:?}");
    assert_eq!(results[0].position, (60, 60), "the near patch's corner");
    assert!(
        (results[0].distance - 60.0 * std::f64::consts::SQRT_2).abs() < 1e-6,
        "the near patch is 60·√2 blocks out, got {}",
        results[0].distance,
    );
    assert!(
        done_of(&events).truncated,
        "a cap that stopped over unscanned area is truncated",
    );
}

#[test]
fn biome_search_empty_field_completes_with_zero_results() {
    // No match anywhere: every ring inside the radius is queried (rings 0..=25
    // for a 400-block radius at step 16) and the search still terminates —
    // this test running to completion is the no-hang proof.
    let source = FieldSource::new(|_x, _z| BACKGROUND);
    let (events, stats) = collect(&source, &search_around((0.0, 0.0), 400.0));

    let done = done_of(&events);
    assert_eq!(done.results, 0);
    assert!(!done.truncated);
    assert!(!done.cancelled);
    assert_eq!(
        done.rings_scanned, 28,
        "rings 0..=27 cover the radius plus the lattice reach"
    );
    assert!(done.rings_scanned > 0);
    assert_eq!(stats.results, 0);
    assert!(
        matches!(events.first(), Some(BiomeSearchEvent::Started)),
        "the stream opens with Started",
    );
    assert!(results_of(&events).is_empty());
}

// ---------------------------------------------------------------------------
// Failure and cancellation
// ---------------------------------------------------------------------------

#[test]
fn biome_search_cancellation_mid_scan_stops_promptly() {
    // The wrapper flips the token after its fifth strip query. Ring 0 is one
    // strip and ring 1 is four, so the token is set as ring 1's last strip
    // returns; the engine's next check (the refinement loop) stops the scan
    // before anything is refined or emitted.
    let token = cancel_token();
    let mut patches = Vec::new();
    for row in 0..8 {
        for column in 0..8 {
            let x = -2000 + column * 500;
            let z = -2000 + row * 500;
            patches.push((x, z, x + 40, z + 40));
        }
    }
    let source = InterceptSource::new(
        Arc::new(FieldSource::new(union_field(patches))),
        Intercept::CancelAfter {
            after: 5,
            token: Arc::clone(&token),
        },
    );

    let (events, stats) = collect_with_token(&source, &search_around((0.0, 0.0), 4000.0), &token);

    let done = done_of(&events);
    assert!(done.cancelled, "the Done reports the cancellation");
    assert_eq!(done.results, 0, "a cancelled search emits no results");
    assert!(results_of(&events).is_empty());
    assert!(stats.cancelled);

    let queries = source.calls();
    assert_eq!(
        queries, 5,
        "the engine stops at the first check after the fifth query",
    );
    thread::sleep(Duration::from_millis(10));
    assert_eq!(
        source.calls(),
        queries,
        "no query runs after the cancellation",
    );
}

#[test]
fn biome_search_source_error_ends_the_stream_without_done() {
    // Fail the third strip query: ring 0 (one lattice point) and ring 1's top
    // row (three) succeed first, so `samples` counts 4. The field's patch sits
    // outside the radius, so no refinement query intervenes between the two
    // strips.
    let source = InterceptSource::new(
        Arc::new(FieldSource::new(rect_field(5000, 5000, 5100, 5100))),
        Intercept::FailOn { on: 3 },
    );
    let (events, stats) = collect(&source, &search_around((0.0, 0.0), 400.0));

    match events.last() {
        Some(BiomeSearchEvent::Error(SourceError::Generation(message))) => {
            assert!(
                message.contains("injected"),
                "the injected failure survives: {message}",
            );
        }
        other => panic!("a failing source must end with a Generation error, got {other:?}"),
    }
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, BiomeSearchEvent::Done(_))),
        "Done and Error are mutually exclusive terminals",
    );
    assert_eq!(
        stats.samples, 4,
        "1 + 3 lattice points from the two good strips"
    );
    assert_eq!(source.calls(), 3, "the failing query is the third");
    let progress = events
        .iter()
        .filter(|event| matches!(event, BiomeSearchEvent::Progress(_)))
        .count();
    assert_eq!(progress, 2, "one heartbeat per successful strip");
}

#[test]
fn biome_search_panicking_source_maps_to_error() {
    // The query boundary's documented `catch_unwind` deviation: a source that
    // panics must end the stream with a Generation error, not kill the worker
    // silently and leave the caller waiting forever.
    let source = InterceptSource::new(
        Arc::new(FieldSource::new(rect_field(0, 0, 100, 100))),
        Intercept::Panic,
    );
    let (events, _) = collect(&source, &search_around((0.0, 0.0), 400.0));

    match events.last() {
        Some(BiomeSearchEvent::Error(SourceError::Generation(message))) => {
            assert!(
                message.contains("injected panic"),
                "the panic payload survives the boundary: {message}",
            );
        }
        other => panic!("a panicking source must end with a Generation error, got {other:?}"),
    }
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, BiomeSearchEvent::Done(_))),
        "a panicking source ends with Error, never Done",
    );
}

// ---------------------------------------------------------------------------
// The spawned handle
// ---------------------------------------------------------------------------

#[test]
fn biome_search_handle_cancel_stops_the_spawned_worker() {
    // A slow source widens the window between events; the far patch keeps the
    // scan long. Cancelling on `Started` must produce the cancelled Done
    // within the deadline instead of running the whole scan.
    let source = Arc::new(FieldSource::with_delay(
        rect_field(2000, 2000, 2100, 2100),
        Duration::from_millis(2),
    ));
    let handle = run_biome_search(source, search_around((0.0, 0.0), 4000.0));

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut done = None;
    while done.is_none() {
        let Ok(event) = handle
            .events()
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        else {
            panic!("the worker did not report Done within its deadline");
        };
        match event {
            BiomeSearchEvent::Started => handle.cancel(),
            BiomeSearchEvent::Done(stats) => done = Some(stats),
            _ => {}
        }
    }

    let done = done.expect("the loop only exits with a Done");
    assert!(done.cancelled, "the spawned Done reports the cancellation");
}

#[test]
fn biome_search_handle_drop_terminates_the_worker() {
    // Dropping the handle cancels and closes the channel: the worker's next
    // send fails and the thread exits, dropping its source on its own thread.
    // The drop probe is the no-thread-leak witness.
    let dropped = Arc::new(AtomicUsize::new(0));
    let mut source =
        FieldSource::with_delay(rect_field(2000, 2000, 2100, 2100), Duration::from_millis(1));
    source.drop_probe = Some(Arc::clone(&dropped));

    let handle = run_biome_search(Arc::new(source), search_around((0.0, 0.0), 4000.0));
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
