//! Feature catalog, overlay state, clustering and query caching (T23).
//!
//! This module is plain, GUI-free data and pure functions: categories and
//! their icon ids/colors, a deduplicated [`FeatureCatalog`], the
//! [`FeatureOverlayState`] that drives the fetch filter and draw-time
//! toggles, screen-space [`cluster`]ing, and the small [`FeatureCache`].
//! The eframe shell maps [`FeatureIcon`] to a shape and
//! [`FeatureCategory::color`] to an `egui` color; no GUI type appears here.
//!
//! The production query adapter lives in
//! [`crate::source::worldgen`] (the only place worldgen types cross into
//! map-core). This module operates on the plain [`Feature`] values that
//! adapter returns.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::types::{Feature, FeatureFilter, FeatureKind, FeatureKindMask, GeneratorIdentity, Rect};

/// A simple vector icon the GUI draws for a category.
///
/// Plain data so map-core stays GUI-free: the shell maps each variant to an
/// `egui` shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FeatureIcon {
    /// A filled circle (biome prefabs).
    Circle,
    /// A rotated square (reserved for the shell's generic marker).
    Diamond,
    /// A house/gable outline (unique prefabs).
    House,
    /// A spawn crosshair.
    Cross,
    /// A four-point star (points of interest).
    Star,
    /// An arch/cave mouth (reserved until T27 emits cave entrances).
    Cave,
}

/// One stable feature category: an icon, a color and a default visibility.
///
/// The `id` is the stable key; display names and colors may be improved
/// without invalidating saved UI state that keys on the id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureCategory {
    /// Stable category id (see [`category_for`]).
    pub id: String,
    /// Human-readable label for the left panel and tooltips.
    pub display_name: String,
    /// Icon color as opaque RGBA. The GUI maps this onto its canvas.
    pub color: [u8; 4],
    /// Vector shape the GUI draws.
    pub icon: FeatureIcon,
    /// Whether the category starts visible when no state is saved.
    pub default_visible: bool,
}

/// The left-panel toggle group a category belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FeatureToggleGroup {
    /// Unique-prefab categories (landmarks, monuments, …).
    Structures,
    /// Biome-prefab pattern categories (biome decoration).
    Prefabs,
    /// The world spawn point(s).
    Spawn,
    /// Map pins split out of unique prefabs via `show_on_map`.
    Poi,
    /// Cave entrances (reserved until T27).
    Cave,
}

impl FeatureToggleGroup {
    /// Every group, in the left panel's display order.
    pub const ALL: [FeatureToggleGroup; 5] = [
        FeatureToggleGroup::Structures,
        FeatureToggleGroup::Prefabs,
        FeatureToggleGroup::Spawn,
        FeatureToggleGroup::Poi,
        FeatureToggleGroup::Cave,
    ];

    /// The group's slot in a [`FeatureToggleMask`], fixed explicitly so
    /// reordering variants cannot silently scramble persisted masks.
    const fn index(self) -> usize {
        match self {
            FeatureToggleGroup::Structures => 0,
            FeatureToggleGroup::Prefabs => 1,
            FeatureToggleGroup::Spawn => 2,
            FeatureToggleGroup::Poi => 3,
            FeatureToggleGroup::Cave => 4,
        }
    }
}

/// The enabled/disabled state of every [`FeatureToggleGroup`].
///
/// A plain five-slot bitset: `Default` is every group on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeatureToggleMask([bool; 5]);

impl FeatureToggleMask {
    /// Every group enabled (the [`Default`]).
    #[must_use]
    pub const fn all() -> Self {
        Self([true; 5])
    }

    /// Every group disabled.
    #[must_use]
    pub const fn none() -> Self {
        Self([false; 5])
    }

    /// A mask with exactly `group` enabled.
    #[must_use]
    pub fn from_group(group: FeatureToggleGroup) -> Self {
        let mut mask = Self::none();
        mask.set(group, true);
        mask
    }

    /// Whether `group` is enabled.
    #[must_use]
    pub fn contains(&self, group: FeatureToggleGroup) -> bool {
        self.0[group.index()]
    }

    /// Enables or disables one group.
    pub fn set(&mut self, group: FeatureToggleGroup, enabled: bool) {
        self.0[group.index()] = enabled;
    }

    /// Flips one group.
    pub fn toggle(&mut self, group: FeatureToggleGroup) {
        let slot = &mut self.0[group.index()];
        *slot = !*slot;
    }
}

impl Default for FeatureToggleMask {
    fn default() -> Self {
        Self::all()
    }
}

/// The overlay's persistent state: which groups are drawn, and the cluster
/// cell size.
///
/// The fetch filter is coarser than the draw filter. [`Self::kinds_mask`]
/// turns the enabled groups into a [`FeatureKindMask`] so a group that is
/// fully disabled is never fetched at all (T23's chosen answer to "not
/// drawn or fetched": fetched with a kind filter, so a disabled group costs
/// no query). Fine-grained category visibility is a draw-time concern.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FeatureOverlayState {
    /// The enabled left-panel groups.
    pub enabled_groups: FeatureToggleMask,
    /// Grid cell size for clustering, in screen pixels.
    pub cluster_cell_px: f32,
}

impl FeatureOverlayState {
    /// The default cluster cell: 28 px (the plan's 24–32 px band).
    pub const DEFAULT_CLUSTER_CELL_PX: f32 = 28.0;

    /// Whether `group` is enabled.
    #[must_use]
    pub fn group_enabled(&self, group: FeatureToggleGroup) -> bool {
        self.enabled_groups.contains(group)
    }

    /// The feature kinds whose group is enabled, for [`FeatureFilter::kinds`].
    #[must_use]
    pub fn kinds_mask(&self) -> FeatureKindMask {
        let mut mask = FeatureKindMask(0);
        for group in FeatureToggleGroup::ALL {
            if !self.enabled_groups.contains(group) {
                continue;
            }
            let kind = match group {
                FeatureToggleGroup::Structures => FeatureKind::UniquePrefab,
                FeatureToggleGroup::Prefabs => FeatureKind::BiomePrefab,
                FeatureToggleGroup::Spawn => FeatureKind::Spawn,
                FeatureToggleGroup::Poi => FeatureKind::Poi,
                FeatureToggleGroup::Cave => FeatureKind::CaveEntrance,
            };
            mask = mask.union(FeatureKindMask::from_kind(kind));
        }
        mask
    }
}

impl Default for FeatureOverlayState {
    fn default() -> Self {
        Self {
            enabled_groups: FeatureToggleMask::all(),
            cluster_cell_px: Self::DEFAULT_CLUSTER_CELL_PX,
        }
    }
}

// ---------------------------------------------------------------------------
// Category assignment
// ---------------------------------------------------------------------------

/// Hand-picked colors for the categories the fixtures and the plan name.
const SPAWN_COLOR: [u8; 4] = [255, 209, 82, 255];
const POI_COLOR: [u8; 4] = [255, 138, 61, 255];
const CAVE_COLOR: [u8; 4] = [176, 132, 255, 255];
const UNIQUE_PREFAB_COLOR: [u8; 4] = [86, 204, 255, 255];
const BIOME_PREFAB_COLOR: [u8; 4] = [120, 230, 158, 255];

/// Assigns the stable category (id, label, color, icon) for one feature.
///
/// Ids are namespaced by kind:
///
/// - [`FeatureKind::Spawn`] → `"spawn"`,
/// - [`FeatureKind::Poi`] → `"poi"`,
/// - [`FeatureKind::CaveEntrance`] → `"cave"` (reserved until T27),
/// - [`FeatureKind::UniquePrefab`] → `"prefab/unique/<category>"`,
/// - [`FeatureKind::BiomePrefab`] → `"prefab/biome/<category>"`.
///
/// The kind segment is deliberate: the same worldgen category name can occur
/// on both prefab kinds, and [`toggle_group`] must place them in different
/// left-panel groups from the id alone.
///
/// Colors are chosen for contrast on the dark canvas. Unknown prefab
/// categories get a color derived from a stable FNV-1a hash of the id (not
/// [`std::collections::hash_map::DefaultHasher`], which makes no
/// cross-build stability promise), so a category keeps its color across
/// sessions.
#[must_use]
pub fn category_for(feature: &Feature) -> FeatureCategory {
    let (id, display_name, icon) = match feature.kind {
        FeatureKind::Spawn => ("spawn".to_string(), "Spawn".to_string(), FeatureIcon::Cross),
        FeatureKind::Poi => (
            "poi".to_string(),
            "Points of Interest".to_string(),
            FeatureIcon::Star,
        ),
        FeatureKind::CaveEntrance => (
            "cave".to_string(),
            "Cave Entrances".to_string(),
            FeatureIcon::Cave,
        ),
        FeatureKind::UniquePrefab => (
            format!("prefab/unique/{}", feature.category),
            feature.category.clone(),
            FeatureIcon::House,
        ),
        FeatureKind::BiomePrefab => (
            format!("prefab/biome/{}", feature.category),
            feature.category.clone(),
            FeatureIcon::Circle,
        ),
    };
    FeatureCategory {
        color: category_color(&id),
        id,
        display_name,
        icon,
        // Every category is on by default, including the reserved cave layer:
        // when T27 starts emitting entrances they appear without extra setup.
        default_visible: true,
    }
}

/// The color for a category id: hand-picked for known categories, otherwise
/// a deterministic, high-contrast hash color.
fn category_color(id: &str) -> [u8; 4] {
    match id {
        "spawn" => SPAWN_COLOR,
        "poi" => POI_COLOR,
        "cave" => CAVE_COLOR,
        _ => {
            if let Some(category) = id.strip_prefix("prefab/unique/") {
                match category {
                    "Unique" => UNIQUE_PREFAB_COLOR,
                    "Spawn" => SPAWN_COLOR,
                    _ => hashed_color(id),
                }
            } else if let Some(category) = id.strip_prefix("prefab/biome/") {
                match category {
                    "Biome" => BIOME_PREFAB_COLOR,
                    _ => hashed_color(id),
                }
            } else {
                hashed_color(id)
            }
        }
    }
}

/// A stable color from a category id: FNV-1a → hue, with fixed saturation
/// and value so every generated color reads on the dark canvas.
fn hashed_color(id: &str) -> [u8; 4] {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET;
    for byte in id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    let hue = (hash % 360) as f32;
    hsv_to_rgba(hue, 0.62, 0.95)
}

/// HSV (hue degrees, saturation/value in `0.0..=1.0`) → opaque RGBA.
fn hsv_to_rgba(hue: f32, saturation: f32, value: f32) -> [u8; 4] {
    let chroma = value * saturation;
    let sector = (hue / 60.0).rem_euclid(6.0);
    let secondary = chroma * (1.0 - (sector.rem_euclid(2.0) - 1.0).abs());
    let (red, green, blue) = match sector as u32 {
        0 => (chroma, secondary, 0.0),
        1 => (secondary, chroma, 0.0),
        2 => (0.0, chroma, secondary),
        3 => (0.0, secondary, chroma),
        4 => (secondary, 0.0, chroma),
        _ => (chroma, 0.0, secondary),
    };
    let offset = value - chroma;
    let channel = |component: f32| ((component + offset) * 255.0).round() as u8;
    [channel(red), channel(green), channel(blue), 255]
}

/// The left-panel group for a category id.
///
/// Total: every id [`category_for`] can produce maps to one group; an id
/// with an unrecognized prefix falls back to [`FeatureToggleGroup::Structures`]
/// so the static map never fails.
#[must_use]
pub fn toggle_group(category_id: &str) -> FeatureToggleGroup {
    if category_id == "spawn" {
        FeatureToggleGroup::Spawn
    } else if category_id == "poi" {
        FeatureToggleGroup::Poi
    } else if category_id == "cave" {
        FeatureToggleGroup::Cave
    } else if category_id.starts_with("prefab/biome/") {
        FeatureToggleGroup::Prefabs
    } else {
        // `prefab/unique/...` and any future prefab namespace.
        FeatureToggleGroup::Structures
    }
}

/// The group for one feature directly, without allocating its category id.
///
/// Kept consistent with [`toggle_group`] by construction; the unit tests
/// check both agree for every kind.
#[must_use]
pub fn group_for(feature: &Feature) -> FeatureToggleGroup {
    match feature.kind {
        FeatureKind::UniquePrefab => FeatureToggleGroup::Structures,
        FeatureKind::BiomePrefab => FeatureToggleGroup::Prefabs,
        FeatureKind::Spawn => FeatureToggleGroup::Spawn,
        FeatureKind::Poi => FeatureToggleGroup::Poi,
        FeatureKind::CaveEntrance => FeatureToggleGroup::Cave,
    }
}

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

/// The features known for one source identity, deduplicated, plus the
/// distinct categories they use.
///
/// Overlapping query rects (the overlay's margin-expanded fetches) can
/// deliver the same placement twice; [`FeatureCatalog::build`] removes the
/// duplicate by `(kind, name, position)` and keeps the first occurrence.
#[derive(Debug, Clone, Default)]
pub struct FeatureCatalog {
    features: Vec<Feature>,
    categories: Vec<FeatureCategory>,
}

impl FeatureCatalog {
    /// Builds a catalog from any feature stream (typically one adapter
    /// query or several overlapping ones), deduplicating by
    /// `(kind, name, position)` and keeping the first occurrence.
    #[must_use]
    pub fn build(features: impl IntoIterator<Item = Feature>) -> Self {
        let features = dedupe_features(features);
        let mut categories: Vec<FeatureCategory> = Vec::new();
        for feature in &features {
            let category = category_for(feature);
            if !categories.iter().any(|existing| existing.id == category.id) {
                categories.push(category);
            }
        }
        // Deterministic order: the GUI groups by `toggle_group`, then shows
        // categories by id within a group.
        categories.sort_by(|left, right| left.id.cmp(&right.id));
        Self {
            features,
            categories,
        }
    }

    /// Every deduplicated feature, in first-occurrence order.
    #[must_use]
    pub fn features(&self) -> &[Feature] {
        &self.features
    }

    /// The distinct categories present, sorted by id.
    #[must_use]
    pub fn categories(&self) -> &[FeatureCategory] {
        &self.categories
    }

    /// The category with `id`, when the catalog contains one.
    #[must_use]
    pub fn category(&self, id: &str) -> Option<&FeatureCategory> {
        self.categories.iter().find(|category| category.id == id)
    }

    /// The features belonging to one left-panel group.
    pub fn features_in_group(
        &self,
        group: FeatureToggleGroup,
    ) -> impl Iterator<Item = &Feature> + '_ {
        self.features
            .iter()
            .filter(move |feature| group_for(feature) == group)
    }

    /// The number of deduplicated features.
    #[must_use]
    pub fn len(&self) -> usize {
        self.features.len()
    }

    /// Whether the catalog is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.features.is_empty()
    }
}

/// Removes duplicate features by `(kind, name, position)`, keeping the first
/// occurrence; see [`FeatureCatalog::build`].
#[must_use]
pub fn dedupe_features(features: impl IntoIterator<Item = Feature>) -> Vec<Feature> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for feature in features {
        let key = (feature.kind, feature.name.clone(), feature.position);
        if seen.insert(key) {
            deduped.push(feature);
        }
    }
    deduped
}

// ---------------------------------------------------------------------------
// Clustering
// ---------------------------------------------------------------------------

/// One drawable overlay item after screen-space clustering.
#[derive(Debug, Clone, PartialEq)]
pub enum ClusterItem {
    /// A lone feature at its projected screen position.
    Single {
        /// Index into the catalog/point slice.
        feature: usize,
        /// Projected screen position in pixels.
        pos: (f32, f32),
    },
    /// Several nearby features collapsed into one marker.
    Cluster {
        /// Centroid of the members' screen positions.
        center: (f32, f32),
        /// Number of members (`members.len()`).
        count: usize,
        /// Indices into the catalog/point slice, in input order.
        members: Vec<usize>,
    },
}

/// Buckets projected feature positions into a screen-space grid and emits one
/// [`ClusterItem`] per non-empty cell.
///
/// Bucketing is `floor(pos / cell)` on both axes, so a point exactly on a
/// cell boundary belongs to the cell that starts at it (e.g. with `cell = 10`,
/// x = 10 buckets with x = 19, not with x = 9). That tie rule is applied by
/// the same floor for negative coordinates. Cells are emitted in first-point
/// order; members keep their input order. `cell` must be positive and
/// finite — the app owns the "too many singles" cap, not this function.
#[must_use]
pub fn cluster(points: &[(f32, f32)], cell: f32) -> Vec<ClusterItem> {
    assert!(
        cell.is_finite() && cell > 0.0,
        "the cluster cell must be a positive finite pixel size, got {cell}",
    );

    let mut cells: Vec<Vec<usize>> = Vec::new();
    let mut slots: HashMap<(i64, i64), usize> = HashMap::new();
    for (index, &(x, y)) in points.iter().enumerate() {
        let key = ((x / cell).floor() as i64, (y / cell).floor() as i64);
        match slots.get(&key) {
            Some(&slot) => cells[slot].push(index),
            None => {
                slots.insert(key, cells.len());
                cells.push(vec![index]);
            }
        }
    }

    cells
        .into_iter()
        .map(|members| {
            if let [feature] = members[..] {
                return ClusterItem::Single {
                    feature,
                    pos: points[feature],
                };
            }
            let count = members.len();
            let sum = members.iter().fold((0.0_f32, 0.0_f32), |sum, &index| {
                (sum.0 + points[index].0, sum.1 + points[index].1)
            });
            ClusterItem::Cluster {
                center: (sum.0 / count as f32, sum.1 / count as f32),
                count,
                members,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Query cache
// ---------------------------------------------------------------------------

/// How many `(rect, filter)` catalogs a [`FeatureCache`] keeps.
pub const FEATURE_CACHE_CAPACITY: usize = 4;

#[derive(Debug)]
struct FeatureCacheEntry {
    rect: Rect,
    filter: FeatureFilter,
    catalog: Arc<FeatureCatalog>,
}

/// A tiny cache of the last few adapter queries for one source identity.
///
/// Entries are insertion-ordered: a `put` appends (replacing an existing
/// identical key in place) and evicts the oldest entry beyond
/// [`FEATURE_CACHE_CAPACITY`]. Hits do not refresh recency — the overlay
/// re-fetches margin-expanded rects, so recency churn buys little.
///
/// The cache is bound to a [`GeneratorIdentity`]. Feature placement depends
/// on the generator seed and identity, so a failed seed/version switch must
/// never serve an older world's catalog: call [`Self::set_identity`] with the
/// new identity, which drops every entry wholesale when it differs, or
/// [`Self::reset`] for an explicit invalidation.
#[derive(Debug)]
pub struct FeatureCache {
    identity: GeneratorIdentity,
    entries: Vec<FeatureCacheEntry>,
}

impl FeatureCache {
    /// An empty cache for `identity`.
    #[must_use]
    pub fn new(identity: GeneratorIdentity) -> Self {
        Self {
            identity,
            entries: Vec::new(),
        }
    }

    /// The identity this cache's entries belong to.
    #[must_use]
    pub fn identity(&self) -> &GeneratorIdentity {
        &self.identity
    }

    /// The cached catalog for exactly `(rect, filter)`, when present.
    #[must_use]
    pub fn get(&self, rect: Rect, filter: &FeatureFilter) -> Option<Arc<FeatureCatalog>> {
        self.entries
            .iter()
            .find(|entry| entry.rect == rect && &entry.filter == filter)
            .map(|entry| Arc::clone(&entry.catalog))
    }

    /// Inserts (or replaces) one query result, evicting the oldest entry past
    /// capacity.
    pub fn put(&mut self, rect: Rect, filter: FeatureFilter, catalog: Arc<FeatureCatalog>) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.rect == rect && entry.filter == filter)
        {
            entry.catalog = catalog;
            return;
        }
        if self.entries.len() >= FEATURE_CACHE_CAPACITY {
            self.entries.remove(0);
        }
        self.entries.push(FeatureCacheEntry {
            rect,
            filter,
            catalog,
        });
    }

    /// Drops every entry; the identity binding is kept.
    pub fn reset(&mut self) {
        self.entries.clear();
    }

    /// Rebinds the cache to `identity`, dropping every entry when the
    /// identity differs. A no-op for the same identity.
    pub fn set_identity(&mut self, identity: GeneratorIdentity) {
        if self.identity != identity {
            self.identity = identity;
            self.entries.clear();
        }
    }

    /// The number of cached queries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache holds no query.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FeatureMetadata;

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

    fn identity(generator_version: u32) -> GeneratorIdentity {
        GeneratorIdentity {
            generator_version,
            world_assets: "Default/0.0.0".to_string(),
            palette_version: 1,
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

    #[test]
    fn features_toggle_group_agrees_with_group_for_every_kind() {
        let kinds = [
            (FeatureKind::UniquePrefab, "Unique"),
            (FeatureKind::BiomePrefab, "Biome"),
            (FeatureKind::Spawn, "spawn"),
            (FeatureKind::Poi, "Unique"),
            (FeatureKind::CaveEntrance, "Caves"),
        ];
        for (kind, category) in kinds {
            let feature = feature(kind, "x", category, (0, 0, 0));
            let id = category_for(&feature).id;
            assert_eq!(
                toggle_group(&id),
                group_for(&feature),
                "category id {id:?} must map to the same group as its feature",
            );
        }
    }

    #[test]
    fn features_unknown_category_color_is_stable_and_distinct() {
        let first = hashed_color("prefab/unique/NoSuchCategory");
        let second = hashed_color("prefab/unique/NoSuchCategory");
        assert_eq!(first, second, "the hash color must be stable");
        assert_eq!(first[3], 255, "hash colors are opaque");
        assert_ne!(
            first,
            hashed_color("prefab/unique/AnotherCategory"),
            "different names should usually differ in hue",
        );
    }

    #[test]
    fn features_catalog_dedupes_and_counts_groups() {
        let features = vec![
            feature(FeatureKind::UniquePrefab, "Keep", "Unique", (1, 2, 3)),
            feature(FeatureKind::UniquePrefab, "Keep", "Unique", (1, 2, 3)),
            feature(FeatureKind::BiomePrefab, "Hut", "Biome", (4, 5, 6)),
        ];
        let catalog = FeatureCatalog::build(features);
        assert_eq!(catalog.len(), 2, "the duplicate placement is dropped");
        assert_eq!(
            catalog
                .features_in_group(FeatureToggleGroup::Structures)
                .count(),
            1
        );
        assert_eq!(
            catalog
                .features_in_group(FeatureToggleGroup::Prefabs)
                .count(),
            1
        );
        assert_eq!(catalog.categories().len(), 2);
    }

    #[test]
    fn features_cluster_boundary_buckets_upward() {
        let items = cluster(&[(9.0, 9.0), (10.0, 10.0)], 10.0);
        assert_eq!(items.len(), 2, "a boundary point starts the next cell");
        assert!(matches!(items[0], ClusterItem::Single { feature: 0, .. }));
        assert!(matches!(items[1], ClusterItem::Single { feature: 1, .. }));
    }

    #[test]
    fn features_cache_evicts_oldest_and_resets_on_identity_change() {
        let mut cache = FeatureCache::new(identity(1));
        let rects = [
            Rect::from_dims(0, 0, 10, 10),
            Rect::from_dims(10, 0, 10, 10),
            Rect::from_dims(20, 0, 10, 10),
            Rect::from_dims(30, 0, 10, 10),
            Rect::from_dims(40, 0, 10, 10),
        ];
        for (index, rect) in rects.iter().enumerate() {
            let catalog = Arc::new(FeatureCatalog::build(vec![feature(
                FeatureKind::UniquePrefab,
                &format!("F{index}"),
                "Unique",
                (index as i32, 0, 0),
            )]));
            cache.put(*rect, all_filter(), catalog);
        }
        assert_eq!(cache.len(), FEATURE_CACHE_CAPACITY);
        assert!(
            cache.get(rects[0], &all_filter()).is_none(),
            "the oldest query is evicted",
        );
        assert!(cache.get(rects[4], &all_filter()).is_some());

        cache.set_identity(identity(2));
        assert!(cache.is_empty(), "a changed identity drops every entry");
        assert_eq!(cache.identity().generator_version, 2);
    }
}
