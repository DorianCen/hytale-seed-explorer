//! T23 acceptance tests: feature catalog, dedupe, clustering, overlay state,
//! cache, and the worldgen `FeatureSource` adapter.
//!
//! The fixture-backed adapter tests reuse the T08 pattern from
//! `tests/worldgen_source.rs`: without the sibling generator checkout and its
//! extracted asset pack they print a skip message and pass (CI has no pack).
//! One [`WorldgenSource`] is shared per test process; every test touching it
//! holds [`SHARED_LOCK`] because the source's region cancellation token is
//! shared.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Instant;

use map_core::features::{
    ClusterItem, FEATURE_CACHE_CAPACITY, FeatureCache, FeatureCatalog, FeatureIcon,
    FeatureOverlayState, FeatureToggleGroup, FeatureToggleMask, category_for, cluster,
    dedupe_features, group_for, toggle_group,
};
use map_core::source::FeatureSource;
use map_core::source::worldgen::{WorldgenSource, WorldgenSourceConfig, translate_placement};
use map_core::types::{
    Feature, FeatureFilter, FeatureKind, FeatureKindMask, FeatureMetadata, GeneratorIdentity, Rect,
    Rect3, Seed,
};

use hytale_math::math_util;
use worldgen_hytale::chunk::chunk_generator::ChunkGenerator;
use worldgen_hytale::loader::chunk_generator_json_loader::ChunkGeneratorJsonLoader;
use worldgen_hytale::prefab::prefab_rotation::PrefabRotation;
use worldgen_hytale::query::{
    BlockBounds, BlockRect, FeaturePlacement, PlacementFilter, PlacementKind, PlacementKindMask,
};
use worldgen_hytale::world_gen_config::WorldGenConfig;
use worldgen_hytale::world_gen_provider::{HytaleWorldGenProvider, Semver};

const TEST_SEED: &str = "12345";

static SHARED_LOCK: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// Pure catalog / overlay / cache tests
// ---------------------------------------------------------------------------

fn feature(kind: FeatureKind, name: &str, category: &str, position: (i32, i32, i32)) -> Feature {
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

fn all_filter() -> FeatureFilter {
    FeatureFilter {
        kinds: FeatureKindMask::ALL,
        categories: None,
        name_contains: None,
        min_size: None,
    }
}

fn identity(generator_version: u32) -> GeneratorIdentity {
    GeneratorIdentity {
        generator_version,
        world_assets: "Default/0.0.0".to_string(),
        palette_version: 1,
    }
}

#[test]
fn features_catalog_maps_unique_biome_spawn_and_poi_categories() {
    let cases = [
        (
            feature(FeatureKind::UniquePrefab, "Temple", "Unique", (1, 2, 3)),
            "prefab/unique/Unique",
            FeatureToggleGroup::Structures,
            FeatureIcon::House,
        ),
        (
            feature(FeatureKind::BiomePrefab, "Hut", "Biome", (4, 5, 6)),
            "prefab/biome/Biome",
            FeatureToggleGroup::Prefabs,
            FeatureIcon::Circle,
        ),
        (
            feature(FeatureKind::Spawn, "spawn", "spawn", (0, 64, 0)),
            "spawn",
            FeatureToggleGroup::Spawn,
            FeatureIcon::Cross,
        ),
        (
            feature(FeatureKind::Poi, "Monument", "Unique", (7, 8, 9)),
            "poi",
            FeatureToggleGroup::Poi,
            FeatureIcon::Star,
        ),
        (
            feature(FeatureKind::CaveEntrance, "Delve", "Caves", (10, 11, 12)),
            "cave",
            FeatureToggleGroup::Cave,
            FeatureIcon::Cave,
        ),
    ];

    for (feature, expected_id, expected_group, expected_icon) in cases {
        assert_eq!(
            feature.metadata.source.as_deref(),
            Some("test"),
            "the catalog must preserve source metadata",
        );
        let category = category_for(&feature);
        assert_eq!(category.id, expected_id);
        assert_eq!(category.icon, expected_icon);
        assert!(category.default_visible, "categories default to visible");
        assert_eq!(category.color[3], 255, "icon colors are opaque");
        assert_eq!(toggle_group(&category.id), expected_group);
        assert_eq!(group_for(&feature), expected_group);
    }

    let catalog = FeatureCatalog::build([
        feature(FeatureKind::UniquePrefab, "Temple", "Unique", (1, 2, 3)),
        feature(FeatureKind::BiomePrefab, "Hut", "Biome", (4, 5, 6)),
        feature(FeatureKind::Spawn, "spawn", "spawn", (0, 64, 0)),
        feature(FeatureKind::Poi, "Monument", "Unique", (7, 8, 9)),
    ]);
    assert_eq!(catalog.categories().len(), 4);
    assert_eq!(
        catalog
            .category("prefab/unique/Unique")
            .expect("the unique category is present")
            .icon,
        FeatureIcon::House,
    );
    assert_eq!(
        catalog.features_in_group(FeatureToggleGroup::Spawn).count(),
        1
    );
    assert_eq!(
        catalog.features_in_group(FeatureToggleGroup::Poi).count(),
        1
    );

    // Keeping the first occurrence: the duplicate differs only in metadata.
    let mut first = feature(FeatureKind::UniquePrefab, "Temple", "Unique", (1, 2, 3));
    first.metadata.source = Some("first".to_string());
    let mut second = first.clone();
    second.metadata.source = Some("second".to_string());
    let catalog = FeatureCatalog::build([first, second]);
    assert_eq!(catalog.len(), 1);
    assert_eq!(
        catalog.features()[0].metadata.source.as_deref(),
        Some("first"),
    );
}

#[test]
fn features_dedupe_across_overlapping_query_rects() {
    let shared = feature(FeatureKind::UniquePrefab, "Keep", "Unique", (5, 6, 7));
    let only_left = feature(FeatureKind::BiomePrefab, "Hut", "Biome", (-2, 0, 3));
    let only_right = feature(FeatureKind::BiomePrefab, "Tower", "Biome", (30, 0, 3));

    // Two margin-expanded rects overlap: the shared placement arrives twice.
    let left = vec![shared.clone(), only_left.clone()];
    let right = vec![shared.clone(), only_right.clone()];
    let deduped = dedupe_features(left.into_iter().chain(right));
    assert_eq!(deduped.len(), 3);
    assert_eq!(deduped.iter().filter(|f| f.name == "Keep").count(), 1);

    let catalog = FeatureCatalog::build(deduped);
    assert_eq!(catalog.len(), 3);
    assert_eq!(
        catalog
            .features_in_group(FeatureToggleGroup::Prefabs)
            .count(),
        2,
    );
}

#[test]
fn features_cluster_buckets_by_cell_and_keeps_member_indices() {
    // Three points in the same 10 px cell, two spread elsewhere.
    let points = [
        (1.0, 1.0),
        (4.0, 2.0),
        (7.0, 6.0),
        (25.0, 1.0),
        (25.0, 25.0),
    ];
    let items = cluster(&points, 10.0);
    assert_eq!(items.len(), 3);

    let ClusterItem::Cluster {
        center,
        count,
        members,
    } = &items[0]
    else {
        panic!("the first cell holds several points, got {:?}", items[0]);
    };
    assert_eq!(*count, 3);
    assert_eq!(*members, vec![0, 1, 2]);
    assert_eq!(*center, (4.0, 3.0));

    assert!(matches!(
        items[1],
        ClusterItem::Single {
            feature: 3,
            pos: (25.0, 1.0)
        }
    ));
    assert!(matches!(
        items[2],
        ClusterItem::Single {
            feature: 4,
            pos: (25.0, 25.0)
        }
    ));
}

#[test]
fn features_cluster_boundary_tie_rule_buckets_upward() {
    // floor(pos / cell): a point exactly on the boundary belongs to the cell
    // that starts at it, and the rule is symmetric for negatives.
    let items = cluster(&[(10.0, 10.0), (-10.0, -10.0), (9.9, 9.9)], 10.0);
    assert_eq!(
        items.len(),
        3,
        "each point lands in its own cell: {items:?}"
    );
    assert!(matches!(
        items[0],
        ClusterItem::Single {
            feature: 0,
            pos: (10.0, 10.0)
        }
    ));
    assert!(matches!(
        items[1],
        ClusterItem::Single {
            feature: 1,
            pos: (-10.0, -10.0)
        }
    ));
    assert!(matches!(items[2], ClusterItem::Single { feature: 2, .. }));
}

#[test]
fn features_cache_hits_evicts_oldest_and_drops_on_identity_change() {
    let mut cache = FeatureCache::new(identity(1));
    let rects = [
        Rect::from_dims(0, 0, 10, 10),
        Rect::from_dims(10, 0, 10, 10),
        Rect::from_dims(20, 0, 10, 10),
        Rect::from_dims(30, 0, 10, 10),
        Rect::from_dims(40, 0, 10, 10),
    ];
    let catalogs: Vec<Arc<FeatureCatalog>> = rects
        .iter()
        .enumerate()
        .map(|(index, _)| {
            Arc::new(FeatureCatalog::build([feature(
                FeatureKind::UniquePrefab,
                &format!("Feature{index}"),
                "Unique",
                (index as i32, 0, 0),
            )]))
        })
        .collect();

    for ((rect, catalog), index) in rects.iter().zip(&catalogs).zip(0..) {
        cache.put(*rect, all_filter(), Arc::clone(catalog));
        let hit = cache
            .get(*rect, &all_filter())
            .expect("a just-inserted query is readable");
        assert!(
            Arc::ptr_eq(&hit, catalog),
            "put {index} is immediately readable",
        );
    }
    assert_eq!(cache.len(), FEATURE_CACHE_CAPACITY);
    assert!(
        cache.get(rects[0], &all_filter()).is_none(),
        "the oldest query was evicted",
    );
    assert!(cache.get(rects[4], &all_filter()).is_some());

    // The filter is part of the key.
    let mut filtered = all_filter();
    filtered.kinds = FeatureKindMask::from_kind(FeatureKind::BiomePrefab);
    assert!(cache.get(rects[4], &filtered).is_none());

    cache.reset();
    assert!(cache.is_empty(), "reset drops every query");

    cache.put(rects[0], all_filter(), Arc::clone(&catalogs[0]));
    cache.set_identity(identity(2));
    assert!(cache.is_empty(), "a changed identity drops every entry");
    assert_eq!(cache.identity().generator_version, 2);

    // The same identity is a no-op, keeping the cache warm.
    cache.put(rects[0], all_filter(), Arc::clone(&catalogs[0]));
    cache.set_identity(identity(2));
    assert_eq!(cache.len(), 1);
}

#[test]
fn features_translation_splits_map_pins_from_unique_prefabs() {
    let bounds = BlockBounds {
        min_x: 10,
        min_y: 64,
        min_z: -4,
        max_x: 19,
        max_y: 80,
        max_z: 3,
    };
    let placement = FeaturePlacement {
        kind: PlacementKind::UniquePrefab,
        name: "Temple".to_string(),
        category: "Unique".to_string(),
        position: (12, 64, -2),
        bounds: Some(bounds),
        rotation: PrefabRotation::VALUES[0],
        show_on_map: true,
        spawn_location: false,
    };
    let poi = translate_placement(&placement).expect("a unique prefab translates");
    assert_eq!(poi.kind, FeatureKind::Poi);
    assert_eq!(poi.metadata.source.as_deref(), Some("unique-prefab"));
    assert_eq!(
        poi.bounds,
        Some(Rect3::new(10, 64, -4, 19, 80, 3)),
        "bounds pass through with inclusive semantics",
    );
    assert_eq!(poi.metadata.size, Some(10), "the larger horizontal extent");

    let hidden = FeaturePlacement {
        show_on_map: false,
        ..placement.clone()
    };
    let landmark = translate_placement(&hidden).expect("a hidden unique prefab translates");
    assert_eq!(landmark.kind, FeatureKind::UniquePrefab);
    assert_eq!(landmark.metadata.source.as_deref(), Some("unique-prefab"));

    let spawn = FeaturePlacement {
        kind: PlacementKind::Spawn,
        name: "spawn".to_string(),
        category: "spawn".to_string(),
        position: (0, 64, 0),
        bounds: None,
        rotation: PrefabRotation::VALUES[0],
        show_on_map: true,
        spawn_location: true,
    };
    let spawn = translate_placement(&spawn).expect("a spawn translates");
    assert_eq!(spawn.kind, FeatureKind::Spawn);
    assert_eq!(spawn.metadata.source.as_deref(), Some("spawn"));
    assert_eq!(spawn.bounds, None);
    assert_eq!(spawn.metadata.size, None);
}

#[test]
fn features_overlay_kinds_mask_drops_disabled_groups_from_the_fetch() {
    let all = FeatureOverlayState::default();
    assert_eq!(all.cluster_cell_px, 28.0);
    assert_eq!(all.kinds_mask(), FeatureKindMask::ALL);

    let mut state = all;
    state
        .enabled_groups
        .set(FeatureToggleGroup::Structures, false);
    assert!(!state.group_enabled(FeatureToggleGroup::Structures));
    let mask = state.kinds_mask();
    assert!(!mask.contains(FeatureKind::UniquePrefab));
    assert!(mask.contains(FeatureKind::BiomePrefab));
    assert!(mask.contains(FeatureKind::Spawn));
    assert!(mask.contains(FeatureKind::Poi));
    assert!(mask.contains(FeatureKind::CaveEntrance));

    let mut only_spawn = state;
    only_spawn.enabled_groups = FeatureToggleMask::from_group(FeatureToggleGroup::Spawn);
    let mask = only_spawn.kinds_mask();
    assert!(mask.contains(FeatureKind::Spawn));
    for kind in [
        FeatureKind::UniquePrefab,
        FeatureKind::BiomePrefab,
        FeatureKind::Poi,
        FeatureKind::CaveEntrance,
    ] {
        assert!(!mask.contains(kind), "only spawn is fetched");
    }

    only_spawn.enabled_groups = FeatureToggleMask::none();
    assert_eq!(
        only_spawn.kinds_mask().0,
        0,
        "every disabled group is never fetched",
    );
}

// ---------------------------------------------------------------------------
// Fixture-backed adapter tests
// ---------------------------------------------------------------------------

/// See `tests/worldgen_source.rs`: `None` (with a skip message) without the
/// sibling checkout and its extracted asset pack.
fn world_dir() -> Option<PathBuf> {
    let dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hytale-server-rust/testdata/Default");
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

fn source() -> &'static WorldgenSource {
    static SOURCE: OnceLock<WorldgenSource> = OnceLock::new();
    SOURCE.get_or_init(|| {
        let world = world_dir().expect("the shared source is only built after world_dir()");
        let config = WorldgenSourceConfig {
            workers: Some(2),
            ..WorldgenSourceConfig::new(world, TEST_SEED)
        };
        WorldgenSource::new(&config).expect("the shared worldgen source loads")
    })
}

fn exclusive_source() -> MutexGuard<'static, ()> {
    SHARED_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn load_direct_generator(world: &Path) -> std::rc::Rc<ChunkGenerator> {
    let provider = HytaleWorldGenProvider {
        name: "Default".to_string(),
        version: Semver::parse("0.0.0").expect("0.0.0 is a valid semver"),
        path: None,
    };
    let resolved = provider
        .resolve_path(world)
        .expect("the testdata world folder resolves");
    let config = WorldGenConfig::new(resolved, "Default".to_string(), "0.0.0".to_string());
    let loader =
        ChunkGeneratorJsonLoader::new(procedural::json::SeedString::new("ChunkGenerator"), config);
    loader.load()
}

fn kind_rank(kind: FeatureKind) -> u8 {
    match kind {
        FeatureKind::UniquePrefab => 0,
        FeatureKind::BiomePrefab => 1,
        FeatureKind::CaveEntrance => 2,
        FeatureKind::Spawn => 3,
        FeatureKind::Poi => 4,
    }
}

fn sort_features(features: &mut [Feature]) {
    features.sort_by(|left, right| {
        (
            kind_rank(left.kind),
            left.position.0,
            left.position.2,
            left.position.1,
            &left.name,
        )
            .cmp(&(
                kind_rank(right.kind),
                right.position.0,
                right.position.2,
                right.position.1,
                &right.name,
            ))
    });
}

/// The unique/spawn path skips the expensive biome placement pass (the
/// worldgen query returns those lists in milliseconds), so this covers the
/// Poi split and spawn mapping against the generator's raw lists.
#[test]
fn features_adapter_serves_unique_and_spawn_lists() {
    let Some(world) = world_dir() else {
        return;
    };
    let _serial = exclusive_source();
    let source = source();
    let seed = Seed::parse(TEST_SEED)
        .expect("the test seed parses")
        .generator();

    // Unique entries are filtered by bounds and spawns by position; a huge
    // rect therefore means "everything", without paying the biome pass.
    let rect = Rect::new(-1_000_000, -1_000_000, 1_000_000, 1_000_000);
    let filter = FeatureFilter {
        kinds: FeatureKindMask::from_kind(FeatureKind::UniquePrefab)
            .union(FeatureKindMask::from_kind(FeatureKind::Poi))
            .union(FeatureKindMask::from_kind(FeatureKind::Spawn)),
        categories: None,
        name_contains: None,
        min_size: None,
    };

    let started = Instant::now();
    let actual = source
        .features_in(rect, &filter)
        .expect("the source serves the unique/spawn feature query");
    let elapsed = started.elapsed();
    eprintln!("adapter unique/spawn query: {elapsed:?}");

    let load_started = Instant::now();
    let generator = load_direct_generator(&world);
    let load_elapsed = load_started.elapsed();
    let unique_started = Instant::now();
    let entries = generator.get_unique_prefabs(seed);
    let unique_elapsed = unique_started.elapsed();
    let spawn_started = Instant::now();
    let spawns = generator.get_spawn_points(seed);
    let spawn_elapsed = spawn_started.elapsed();
    eprintln!(
        "direct generator load {load_elapsed:?}, unique prefabs {unique_elapsed:?}, spawns \
         {spawn_elapsed:?}"
    );

    let mut expected = Vec::new();
    for entry in &entries {
        let (low_x, low_z) = (entry.get_low_bound_x(), entry.get_low_bound_z());
        let (high_x, high_z) = (entry.get_high_bound_x(), entry.get_high_bound_z());
        if high_x < rect.min_x || low_x > rect.max_x || high_z < rect.min_z || low_z > rect.max_z {
            continue;
        }
        let position = entry.get_position();
        let kind = if entry.is_show_on_map() {
            FeatureKind::Poi
        } else {
            FeatureKind::UniquePrefab
        };
        let size_x = i64::from(high_x) - i64::from(low_x) + 1;
        let size_z = i64::from(high_z) - i64::from(low_z) + 1;
        expected.push(Feature {
            kind,
            name: entry.get_name().to_string(),
            category: entry.get_category().name.clone(),
            position,
            bounds: Some(Rect3::new(
                low_x, position.1, low_z, high_x, position.1, high_z,
            )),
            metadata: FeatureMetadata {
                variant: None,
                size: Some(i32::try_from(size_x.max(size_z)).expect("fixture bounds fit i32")),
                source: Some("unique-prefab".to_string()),
            },
        });
    }
    for transform in &spawns {
        let position = transform.get_position();
        let position = (
            math_util::floor(position.x),
            math_util::floor(position.y),
            math_util::floor(position.z),
        );
        if !rect.contains(position.0, position.2) {
            continue;
        }
        expected.push(Feature {
            kind: FeatureKind::Spawn,
            name: "spawn".to_string(),
            category: "spawn".to_string(),
            position,
            bounds: None,
            metadata: FeatureMetadata {
                variant: None,
                size: None,
                source: Some("spawn".to_string()),
            },
        });
    }
    sort_features(&mut expected);
    let mut actual_sorted = actual.clone();
    sort_features(&mut actual_sorted);
    assert_eq!(
        actual_sorted, expected,
        "the adapter must serve exactly the generator's unique + spawn lists",
    );

    // The committed Default fixture's zone configs carry no `ShowOnMap: true`
    // entry, so this count is zero on this fixture; the pure translation test
    // covers the split itself.
    let expected_poi = entries
        .iter()
        .filter(|entry| entry.is_show_on_map())
        .count();
    let actual_poi = actual.iter().filter(|f| f.kind == FeatureKind::Poi).count();
    assert_eq!(
        actual_poi, expected_poi,
        "every show_on_map unique prefab becomes a Poi map pin",
    );
    assert!(
        !entries.is_empty(),
        "the fixture seed must expose at least one unique prefab",
    );
    assert!(
        !spawns.is_empty(),
        "the fixture seed must expose a spawn point",
    );
    assert_eq!(
        actual
            .iter()
            .filter(|f| f.kind == FeatureKind::Spawn)
            .count(),
        spawns.len(),
        "every spawn point is mapped",
    );

    eprintln!(
        "features adapter unique/spawn: {} unique entries, {} spawns, {} features in {elapsed:?}",
        entries.len(),
        spawns.len(),
        actual.len(),
    );
}

/// The biome-prefab adapter path must equal the same query run directly on a
/// generator, translated by the same worker-side function. A 48×48 rect at
/// the origin is the T23-sized probe (the query pads to chunk windows); the
/// non-empty assertion keeps this from passing vacuously.
#[test]
fn features_adapter_biome_rect_matches_direct_generator() {
    let Some(world) = world_dir() else {
        return;
    };
    let _serial = exclusive_source();
    let source = source();
    let seed = Seed::parse(TEST_SEED)
        .expect("the test seed parses")
        .generator();

    let rect = Rect::from_dims(0, 0, 48, 48);
    let filter = FeatureFilter {
        kinds: FeatureKindMask::from_kind(FeatureKind::BiomePrefab),
        categories: None,
        name_contains: None,
        min_size: None,
    };

    let started = Instant::now();
    let actual = source
        .features_in(rect, &filter)
        .expect("the source serves the biome feature query");
    let elapsed = started.elapsed();

    let block_rect = BlockRect {
        min_x: rect.min_x,
        min_z: rect.min_z,
        max_x: rect.max_x,
        max_z: rect.max_z,
    };
    let placement_filter = PlacementFilter {
        kinds: PlacementKindMask::BIOME_PREFAB,
        ..PlacementFilter::default()
    };
    let generator = load_direct_generator(&world);
    let direct_started = Instant::now();
    let expected: Vec<Feature> = generator
        .features_in(seed, block_rect, &placement_filter)
        .iter()
        .filter_map(translate_placement)
        .collect();
    let direct_elapsed = direct_started.elapsed();

    assert_eq!(
        actual, expected,
        "the adapter result must equal the direct generator query, translated identically",
    );
    let biome_count = actual
        .iter()
        .filter(|feature| feature.kind == FeatureKind::BiomePrefab)
        .count();
    assert!(
        biome_count > 0,
        "the 48x48 origin rect must contain at least one biome prefab (else the check is vacuous)",
    );
    for feature in &actual {
        assert_eq!(feature.kind, FeatureKind::BiomePrefab);
        assert_eq!(feature.metadata.source.as_deref(), Some("biome-prefab"));
        assert!(
            feature.bounds.is_some(),
            "biome prefabs always carry bounds: {feature:?}",
        );
    }

    eprintln!(
        "features adapter biome rect 48x48 at origin: {} features in {elapsed:?} \
         (direct generator {direct_elapsed:?})",
        actual.len(),
    );
}
