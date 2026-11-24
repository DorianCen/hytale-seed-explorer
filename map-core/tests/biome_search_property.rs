//! T25 property tests: the biome search engine against brute force.
//!
//! The integration suite pins the engine on hand-picked shapes; this suite
//! hunts the blob scatterings those tables miss. The acceptance property:
//! the stream's first result is the *true* nearest target block within the
//! radius — no closer block was skipped by the coarse lattice, the frontier
//! filter or the refinement window — and the whole stream is nearest-first,
//! in-radius and reads the requested biome at every emitted position.
//!
//! The fake is a pure (x, z) biome field ([`FieldSource`]), so brute force
//! is an independent double loop over every block inside the radius; the
//! blobs are axis-aligned rectangles at least one coarse step wide on both
//! axes, which is what makes a `coarse_step` lattice sample land in each of
//! them (any `step` consecutive blocks contain a multiple of `step`).
//!
//! Deliberately NOT asserted: the number of results. The frontier filter
//! legitimately emits several points per blob (a blob's near edge can hold
//! more than one unshadowed lattice cell), so only the nearest one and the
//! stream's order are contract.

use map_core::catalog::{BiomeCatalog, BlockCatalog, FluidCatalog, ZoneCatalog};
use map_core::error::{SourceError, SourceResult};
use map_core::scheduler::cancel_token;
use map_core::search::biome::{
    BiomeMatch, BiomeSearch, BiomeSearchEvent, BiomeSearchStats, run_biome_search_blocking,
};
use map_core::source::SummaryFidelity;
use map_core::source::WorldSource;
use map_core::types::{
    BiomeGrid, BiomeId, GeneratorIdentity, HeightGrid, Rect, Seed, SliceGrid, SpawnPoint,
    SurfaceGrid, SurfaceSample, ZoneGrid,
};
use proptest::prelude::*;

/// Distance tolerance. The engine and the mirror run the same `dx.hypot(dz)`
/// over the same inputs, so this is slack against a future refactor, not an
/// expected error.
const DISTANCE_EPSILON: f64 = 1e-9;

const BACKGROUND: BiomeId = BiomeId(1);
const TARGET: BiomeId = BiomeId(7);

// ---------------------------------------------------------------------------
// The synthetic source
// ---------------------------------------------------------------------------

/// A pure-function biome field: the queries the biome search makes are
/// served exactly per the `WorldSource` contract; everything else is
/// [`SourceError::Unsupported`]. Mirrors `tests/biome_search.rs` (trimmed to
/// the fields this suite uses — no delay or drop probe without the spawned
/// handle tests).
struct FieldSource {
    field: Box<dyn Fn(i32, i32) -> BiomeId + Send + Sync>,
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

    fn unsupported<T>(&self) -> SourceResult<T> {
        Err(SourceError::Unsupported(
            "the synthetic field serves biome queries only",
        ))
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
// Event helpers (mirrored from tests/biome_search.rs)
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

// ---------------------------------------------------------------------------
// The generated world
// ---------------------------------------------------------------------------

/// An inclusive block rectangle `[min_x, max_x] × [min_z, max_z]` that reads
/// [`TARGET`]; everything else reads [`BACKGROUND`].
type Blob = (i32, i32, i32, i32);

/// The ground-truth field value — the same function the fake's closure runs,
/// so brute force never drifts from the source.
fn field_value(blobs: &[Blob], x: i32, z: i32) -> BiomeId {
    let inside = blobs.iter().any(|&(min_x, min_z, max_x, max_z)| {
        (min_x..=max_x).contains(&x) && (min_z..=max_z).contains(&z)
    });
    if inside { TARGET } else { BACKGROUND }
}

/// The fake source over one case's field.
fn field_source(blobs: &[Blob]) -> FieldSource {
    let blobs = blobs.to_vec();
    FieldSource::new(move |x, z| field_value(&blobs, x, z))
}

/// One generated search: the request's geometry plus the target blobs.
#[derive(Debug, Clone)]
struct Case {
    center: (f64, f64),
    radius: f64,
    coarse_step: u32,
    blobs: Vec<Blob>,
}

/// A center around ±5,000 with a tenths fraction, so centers parked between
/// blocks are exercised alongside integral ones.
fn center_strategy() -> impl Strategy<Value = f64> {
    (-5_000i32..=5_000, 0u8..=9)
        .prop_map(|(whole, tenths)| f64::from(whole) + f64::from(tenths) * 0.1)
}

/// The plan's 4..=16 coarse-step band, sampled at its powers of two.
fn coarse_step_strategy() -> impl Strategy<Value = u32> {
    prop::sample::select(vec![4u32, 8, 16])
}

/// Per-blob `(east, south, width, height)`: offsets are thousandths of the
/// radius (±1.5 × radius), extents are block counts in `step..=5 × step` —
/// never thinner than the lattice can detect.
fn case_strategy() -> impl Strategy<Value = Case> {
    (
        center_strategy(),
        center_strategy(),
        64i32..=256,
        coarse_step_strategy(),
    )
        .prop_flat_map(|(center_x, center_z, radius, coarse_step)| {
            let step = i32::try_from(coarse_step).expect("the step strategy yields 4, 8 or 16");
            let extent = step..=5 * step;
            let blob = (-1_500i32..=1_500, -1_500i32..=1_500, extent.clone(), extent);
            (
                Just((center_x, center_z, radius, coarse_step)),
                prop::collection::vec(blob, 0..=6),
            )
        })
        .prop_map(|((center_x, center_z, radius, coarse_step), specs)| {
            let center = (center_x, center_z);
            let radius_blocks = f64::from(radius);
            let blobs = specs
                .into_iter()
                .map(|(east, south, width, height)| {
                    // The blob is centred on its offset point, so blobs
                    // straddling the radius edge and overlapping each other
                    // both come up naturally.
                    let x = (center.0 + radius_blocks * f64::from(east) / 1_000.0).round() as i32;
                    let z = (center.1 + radius_blocks * f64::from(south) / 1_000.0).round() as i32;
                    let min_x = x - width / 2;
                    let min_z = z - height / 2;
                    (min_x, min_z, min_x + width - 1, min_z + height - 1)
                })
                .collect();
            Case {
                center,
                radius: radius_blocks,
                coarse_step,
                blobs,
            }
        })
}

/// The true nearest target block: every block whose center distance is
/// inside the radius, the same `dx.hypot(dz)` the engine measures with.
/// `None` when the radius holds no target block at all.
fn brute_force_nearest(case: &Case) -> Option<f64> {
    let (cx, cz) = case.center;
    let radius = case.radius;
    let min_x = (cx - radius).floor() as i32;
    let max_x = (cx + radius).ceil() as i32;
    let min_z = (cz - radius).floor() as i32;
    let max_z = (cz + radius).ceil() as i32;
    let mut nearest = f64::INFINITY;
    for x in min_x..=max_x {
        for z in min_z..=max_z {
            let distance = (cx - f64::from(x)).hypot(cz - f64::from(z));
            if distance <= radius && distance < nearest && field_value(&case.blobs, x, z) == TARGET
            {
                nearest = distance;
            }
        }
    }
    nearest.is_finite().then_some(nearest)
}

/// A search over the case's field, unlimited results and no patch minimum.
fn case_search(case: &Case) -> BiomeSearch {
    BiomeSearch {
        biome: TARGET,
        center: case.center,
        max_radius: case.radius,
        min_patch_blocks: None,
        max_results: 0,
        coarse_step: case.coarse_step,
    }
}

// ---------------------------------------------------------------------------
// The properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// The head of the stream is the true nearest target block: when brute
    /// force finds one, the search emits a result at exactly that distance and
    /// the position is a real `biome_at` reading; when it finds none, the
    /// search reports nothing and did not truncate.
    #[test]
    fn biome_search_property_first_result_matches_brute_force(case in case_strategy()) {
        let source = field_source(&case.blobs);
        let (events, _) = collect(&source, &case_search(&case));
        let results = results_of(&events);
        let done = done_of(&events);

        match brute_force_nearest(&case) {
            Some(expected) => {
                prop_assert!(
                    !results.is_empty(),
                    "brute force found a target block at {expected} blocks from {:?} \
                     (radius {}, step {}), but the search emitted nothing: {:?}",
                    case.center,
                    case.radius,
                    case.coarse_step,
                    case.blobs,
                );
                let first = results[0];
                prop_assert!(
                    (first.distance - expected).abs() < DISTANCE_EPSILON,
                    "the first result is {} blocks out, but a target block sits at \
                     {expected} — the search skipped a closer block (center {:?}, \
                     radius {}, step {}, blobs {:?})",
                    first.distance,
                    case.center,
                    case.radius,
                    case.coarse_step,
                    case.blobs,
                );
                prop_assert_eq!(
                    source.biome_at(first.position.0, first.position.1),
                    Ok(TARGET),
                    "the first result at {:?} is not a true target reading (center {:?}, \
                     radius {}, step {}, blobs {:?})",
                    first.position,
                    case.center,
                    case.radius,
                    case.coarse_step,
                    case.blobs,
                );
            }
            None => {
                prop_assert!(
                    results.is_empty(),
                    "no target block is inside the radius {} of {:?}, but the search \
                     emitted {:?} (step {}, blobs {:?})",
                    case.radius,
                    case.center,
                    results,
                    case.coarse_step,
                    case.blobs,
                );
                prop_assert!(
                    !done.truncated,
                    "an empty unlimited scan cannot be truncated (center {:?}, radius {}, \
                     step {}, blobs {:?})",
                    case.center,
                    case.radius,
                    case.coarse_step,
                    case.blobs,
                );
            }
        }
    }

    /// The whole stream is nearest-first, inside the radius, honest and
    /// completely accounted for by the terminal stats.
    #[test]
    fn biome_search_property_stream_is_sorted_and_within_radius(case in case_strategy()) {
        let source = field_source(&case.blobs);
        let (events, _) = collect(&source, &case_search(&case));
        let results = results_of(&events);
        let done = done_of(&events);

        let mut previous = f64::NEG_INFINITY;
        for result in &results {
            prop_assert!(
                result.distance >= previous,
                "the stream regressed from {previous} to {} at {:?} (center {:?}, \
                 radius {}, step {}, blobs {:?})",
                result.distance,
                result.position,
                case.center,
                case.radius,
                case.coarse_step,
                case.blobs,
            );
            previous = result.distance;
            prop_assert!(
                result.distance <= case.radius,
                "the result at {:?} is {} blocks out, beyond the radius {} (center {:?}, \
                 step {}, blobs {:?})",
                result.position,
                result.distance,
                case.radius,
                case.center,
                case.coarse_step,
                case.blobs,
            );
            prop_assert_eq!(
                source.biome_at(result.position.0, result.position.1),
                Ok(TARGET),
                "the result at {:?} is not a true target reading (center {:?}, radius {}, \
                 step {}, blobs {:?})",
                result.position,
                case.center,
                case.radius,
                case.coarse_step,
                case.blobs,
            );
        }
        prop_assert_eq!(
            done.results,
            results.len(),
            "Done.results must count every emitted result (center {:?}, radius {}, \
             step {}, blobs {:?})",
            case.center,
            case.radius,
            case.coarse_step,
            case.blobs,
        );
    }
}
