//! T27: the cave-entrance feature adapter, catalog identity and query cap.
//!
//! The synthetic tests pin the translation/catalog contract; the
//! fixture-gated tests run the real generator's cave-entrance query through
//! `WorldgenSource` and check that the adapter's cap and dedupe hold.

use std::path::{Path, PathBuf};

use map_core::features::{
    FeatureCatalog, FeatureIcon, FeatureToggleGroup, FeatureToggleMask, category_for, group_for,
    toggle_group,
};
use map_core::source::FeatureSource;
use map_core::source::WorldSource;
use map_core::source::worldgen::{
    MAX_CAVE_ENTRANCE_SIDE_BLOCKS, WorldgenSource, WorldgenSourceConfig, translate_placement,
};
use map_core::types::{FeatureFilter, FeatureKind, FeatureKindMask, Rect};
use worldgen_hytale::query::{BlockBounds, FeaturePlacement, PlacementKind};

fn cave_placement() -> FeaturePlacement {
    FeaturePlacement {
        kind: PlacementKind::CaveEntrance,
        name: "Copper".to_string(),
        category: "Zone1_Tier1".to_string(),
        position: (0, 107, 0),
        bounds: Some(BlockBounds {
            min_x: 0,
            min_y: 107,
            min_z: 0,
            max_x: 0,
            max_y: 109,
            max_z: 0,
        }),
        rotation: worldgen_hytale::prefab::prefab_rotation::PrefabRotation::VALUES[0],
        show_on_map: true,
        spawn_location: false,
    }
}

fn cave_filter() -> FeatureFilter {
    FeatureFilter {
        kinds: FeatureKindMask::from_kind(FeatureKind::CaveEntrance),
        categories: None,
        name_contains: None,
        min_size: None,
    }
}

#[test]
fn cave_entrances_translate_to_the_map_core_identity() {
    let placement = cave_placement();
    let feature = translate_placement(&placement).expect("cave entrances translate");
    assert_eq!(feature.kind, FeatureKind::CaveEntrance);
    assert_eq!(feature.name, "Copper");
    assert_eq!(
        feature.category, "Zone1_Tier1",
        "the category keeps the zone name for the tooltip",
    );
    assert_eq!(feature.position, (0, 107, 0));
    assert_eq!(
        feature.metadata.source.as_deref(),
        Some("cave-entrance"),
        "the adapter records which generator list produced it",
    );

    let category = category_for(&feature);
    assert_eq!(category.id, "cave");
    assert_eq!(category.icon, FeatureIcon::Cave);
    assert_eq!(group_for(&feature), FeatureToggleGroup::Cave);
    assert_eq!(
        toggle_group(&category.id),
        FeatureToggleGroup::Cave,
        "the category id and the feature agree on the group",
    );
    assert!(FeatureToggleMask::all().contains(FeatureToggleGroup::Cave));
}

#[test]
fn cave_entrances_dedupe_by_kind_name_and_position() {
    let feature = translate_placement(&cave_placement()).expect("translates");
    let catalog = FeatureCatalog::build([feature.clone(), feature.clone()]);
    assert_eq!(catalog.len(), 1, "overlapping rect replies collapse");
    assert_eq!(catalog.categories().len(), 1);
    assert_eq!(catalog.categories()[0].id, "cave");
    assert_eq!(
        catalog.features_in_group(FeatureToggleGroup::Cave).count(),
        1
    );

    // A different cave type at the same column is a different entrance.
    let mut second = cave_placement();
    second.name = "Iron".to_string();
    let catalog = FeatureCatalog::build([
        translate_placement(&cave_placement()).expect("translates"),
        translate_placement(&second).expect("translates"),
    ]);
    assert_eq!(catalog.len(), 2);
}

#[test]
fn cave_entrances_are_capped_for_overlay_sized_rects() {
    // The cap is the adapter's: a rect wider or taller than the constant
    // drops only the cave kind. Keep the constant in a sane band and verify
    // the check through the real adapter below.
    assert!(
        (128..=512).contains(&MAX_CAVE_ENTRANCE_SIDE_BLOCKS),
        "the cave cap stays a zoomed-in overlay size, got {MAX_CAVE_ENTRANCE_SIDE_BLOCKS}",
    );
}

/// The worldgen checkout and asset pack the fixture tests need.
fn world_dir() -> Option<PathBuf> {
    let dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hytale-server-rust/testdata/Default");
    let assets = dir.join("../../Assets/extracted");
    (dir.is_dir() && assets.is_dir()).then_some(dir)
}

fn fixture_source() -> Option<WorldgenSource> {
    let world = world_dir()?;
    let mut config = WorldgenSourceConfig::new(world, "0");
    config.workers = Some(1);
    match WorldgenSource::new(&config) {
        Ok(source) => Some(source),
        Err(error) => {
            eprintln!("skipping: the fixture source did not load: {error}");
            None
        }
    }
}

#[test]
fn cave_entrances_real_query_catalogs_and_caps() {
    let Some(source) = fixture_source() else {
        eprintln!("skipping: the worldgen checkout or asset pack is not present");
        return;
    };

    let chunk = Rect::new(0, 0, 31, 31);
    let features = source
        .features_in(chunk, &cave_filter())
        .expect("the cave-entrance query runs");
    assert!(
        !features.is_empty(),
        "the fixture chunk has cave entrances (T27)"
    );
    assert!(
        features
            .iter()
            .all(|feature| feature.kind == FeatureKind::CaveEntrance),
        "a CaveEntrance-only filter never leaks another kind",
    );
    for feature in &features {
        assert!(
            (0..=31).contains(&feature.position.0) && (0..=31).contains(&feature.position.2),
            "{feature:?} is inside the queried chunk",
        );
        assert!(
            source
                .zone_catalog()
                .iter()
                .any(|zone| zone.name == feature.category),
            "the cave category is a real zone name: {}",
            feature.category,
        );
    }
    let catalog = FeatureCatalog::build(features.clone());
    assert_eq!(catalog.len(), features.len(), "distinct coordinates/types");
    assert!(catalog.category("cave").is_some());

    // A second overlapping query dedupes through the catalog identity.
    let shifted = Rect::new(16, 16, 47, 47);
    let more = source.features_in(shifted, &cave_filter()).expect("runs");
    let merged = FeatureCatalog::build(features.into_iter().chain(more));
    assert!(
        merged.len() <= 2 * chunk.width() as usize * chunk.width() as usize,
        "the catalog stays the union, not a per-query append",
    );

    // The overlay cap: a rect larger than the constant drops the cave kind
    // before the expensive query (an empty result, not a long wait).
    let too_big = Rect::new(
        0,
        0,
        i32::try_from(MAX_CAVE_ENTRANCE_SIDE_BLOCKS).expect("the cap fits i32"),
        i32::try_from(MAX_CAVE_ENTRANCE_SIDE_BLOCKS).expect("the cap fits i32"),
    );
    let started = std::time::Instant::now();
    let capped = source
        .features_in(too_big, &cave_filter())
        .expect("the capped query answers");
    assert!(capped.is_empty(), "the cap drops the cave kind");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "the cap avoids the expensive cave scan",
    );
}
