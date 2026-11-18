//! Catalogs map generator/registry ids to names.
//!
//! Hot data (grids, [`SurfaceSample`](crate::types::SurfaceSample), tiles)
//! carries plain ids only. Renderers, the inspector and search resolve those
//! ids here, so no `Arc` ever appears inside a sample and cloning a region
//! stays a flat `Vec` copy.
//!
//! Catalogs are built once by the source adapter (T08), usually from the
//! worldgen registries. Insertion is fallible because a modded world can map
//! two ids to one name; resolving such collisions is the adapter's job.

use std::collections::HashMap;

use crate::error::{SourceError, SourceResult};
use crate::types::{BiomeId, BlockId, FluidId, ZoneId};

/// Lookup entry for a biome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BiomeInfo {
    /// The generator's biome id.
    pub id: BiomeId,
    /// Display name, e.g. `"Emerald Grove"`.
    pub name: String,
    /// Packed `0x00RRGGBB` in the low 24 bits, exactly as the generator's
    /// `Biome::get_map_color()` parses it; use [`BiomeInfo::map_color_rgb`] to
    /// unpack.
    pub map_color: u32,
}

impl BiomeInfo {
    /// Unpacks [`BiomeInfo::map_color`] into `[r, g, b]`.
    ///
    /// Channel order is the decision fixed for this crate: red in bits 16–23,
    /// green in 8–15, blue in 0–7, matching the worldgen `map_image` packer.
    /// The high byte is ignored (and is zero for generator colors).
    #[must_use]
    pub fn map_color_rgb(&self) -> [u8; 3] {
        [
            ((self.map_color >> 16) & 0xFF) as u8,
            ((self.map_color >> 8) & 0xFF) as u8,
            (self.map_color & 0xFF) as u8,
        ]
    }
}

/// Biome ids to [`BiomeInfo`], plus the reverse name index.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BiomeCatalog {
    by_id: HashMap<BiomeId, BiomeInfo>,
    by_name: HashMap<String, BiomeId>,
}

impl BiomeCatalog {
    /// Adds one biome.
    ///
    /// Returns [`SourceError::Internal`] on a duplicate id or a duplicate
    /// name. The T08 adapter must resolve collisions before inserting: the
    /// default world's biome ids are globally unique, but modded worlds may
    /// reuse names or ids, and map-core will not silently pick a winner.
    pub fn insert(&mut self, info: BiomeInfo) -> SourceResult<()> {
        if self.by_id.contains_key(&info.id) {
            return Err(SourceError::Internal(format!(
                "duplicate biome id {} ({})",
                info.id.0, info.name
            )));
        }
        if self.by_name.contains_key(&info.name) {
            return Err(SourceError::Internal(format!(
                "duplicate biome name {:?} (id {})",
                info.name, info.id.0
            )));
        }
        self.by_name.insert(info.name.clone(), info.id);
        self.by_id.insert(info.id, info);
        Ok(())
    }

    /// Resolves an id to its entry.
    #[must_use]
    pub fn get(&self, id: BiomeId) -> Option<&BiomeInfo> {
        self.by_id.get(&id)
    }

    /// Resolves a display name to its id.
    #[must_use]
    pub fn get_by_name(&self, name: &str) -> Option<BiomeId> {
        self.by_name.get(name).copied()
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether the catalog has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Iterates all entries in unspecified order.
    pub fn iter(&self) -> impl Iterator<Item = &BiomeInfo> {
        self.by_id.values()
    }
}

/// Block registry ids to display names, with a reverse index.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlockCatalog {
    by_id: HashMap<BlockId, String>,
    by_name: HashMap<String, BlockId>,
}

impl BlockCatalog {
    /// Adds one block name.
    ///
    /// Returns [`SourceError::Internal`] on a duplicate id or name; see
    /// [`BiomeCatalog::insert`] for the adapter's collision duty.
    pub fn insert(&mut self, id: BlockId, name: impl Into<String>) -> SourceResult<()> {
        let name = name.into();
        if self.by_id.contains_key(&id) {
            return Err(SourceError::Internal(format!(
                "duplicate block id {} ({name})",
                id.0
            )));
        }
        if self.by_name.contains_key(&name) {
            return Err(SourceError::Internal(format!(
                "duplicate block name {name:?} (id {})",
                id.0
            )));
        }
        self.by_name.insert(name.clone(), id);
        self.by_id.insert(id, name);
        Ok(())
    }

    /// Resolves an id to its block name.
    #[must_use]
    pub fn get(&self, id: BlockId) -> Option<&str> {
        self.by_id.get(&id).map(String::as_str)
    }

    /// Resolves a block name to its id.
    #[must_use]
    pub fn get_id(&self, name: &str) -> Option<BlockId> {
        self.by_name.get(name).copied()
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether the catalog has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Iterates `(id, name)` pairs in unspecified order.
    pub fn iter(&self) -> impl Iterator<Item = (BlockId, &str)> {
        self.by_id.iter().map(|(id, name)| (*id, name.as_str()))
    }
}

/// Fluid registry ids to display names, with a reverse index.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FluidCatalog {
    by_id: HashMap<FluidId, String>,
    by_name: HashMap<String, FluidId>,
}

impl FluidCatalog {
    /// Adds one fluid name.
    ///
    /// Returns [`SourceError::Internal`] on a duplicate id or name; see
    /// [`BiomeCatalog::insert`] for the adapter's collision duty.
    pub fn insert(&mut self, id: FluidId, name: impl Into<String>) -> SourceResult<()> {
        let name = name.into();
        if self.by_id.contains_key(&id) {
            return Err(SourceError::Internal(format!(
                "duplicate fluid id {} ({name})",
                id.0
            )));
        }
        if self.by_name.contains_key(&name) {
            return Err(SourceError::Internal(format!(
                "duplicate fluid name {name:?} (id {})",
                id.0
            )));
        }
        self.by_name.insert(name.clone(), id);
        self.by_id.insert(id, name);
        Ok(())
    }

    /// Resolves an id to its fluid name.
    #[must_use]
    pub fn get(&self, id: FluidId) -> Option<&str> {
        self.by_id.get(&id).map(String::as_str)
    }

    /// Resolves a fluid name to its id.
    #[must_use]
    pub fn get_id(&self, name: &str) -> Option<FluidId> {
        self.by_name.get(name).copied()
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether the catalog has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Iterates `(id, name)` pairs in unspecified order.
    pub fn iter(&self) -> impl Iterator<Item = (FluidId, &str)> {
        self.by_id.iter().map(|(id, name)| (*id, name.as_str()))
    }
}

/// Lookup entry for a zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneInfo {
    /// The generator's zone id.
    pub id: ZoneId,
    /// Display name, e.g. `"Emerald Grove"`.
    pub name: String,
}

/// Zone ids to [`ZoneInfo`], plus the reverse name index.
///
/// Zone colors are not stored yet; the mapping arrives with the biome layer
/// task (T13).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ZoneCatalog {
    by_id: HashMap<ZoneId, ZoneInfo>,
    by_name: HashMap<String, ZoneId>,
}

impl ZoneCatalog {
    /// Adds one zone.
    ///
    /// Returns [`SourceError::Internal`] on a duplicate id or name; see
    /// [`BiomeCatalog::insert`] for the adapter's collision duty.
    pub fn insert(&mut self, info: ZoneInfo) -> SourceResult<()> {
        if self.by_id.contains_key(&info.id) {
            return Err(SourceError::Internal(format!(
                "duplicate zone id {} ({})",
                info.id.0, info.name
            )));
        }
        if self.by_name.contains_key(&info.name) {
            return Err(SourceError::Internal(format!(
                "duplicate zone name {:?} (id {})",
                info.name, info.id.0
            )));
        }
        self.by_name.insert(info.name.clone(), info.id);
        self.by_id.insert(info.id, info);
        Ok(())
    }

    /// Resolves an id to its entry.
    #[must_use]
    pub fn get(&self, id: ZoneId) -> Option<&ZoneInfo> {
        self.by_id.get(&id)
    }

    /// Resolves a display name to its id.
    #[must_use]
    pub fn get_by_name(&self, name: &str) -> Option<ZoneId> {
        self.by_name.get(name).copied()
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether the catalog has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Iterates all entries in unspecified order.
    pub fn iter(&self) -> impl Iterator<Item = &ZoneInfo> {
        self.by_id.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn biome(id: i32, name: &str) -> BiomeInfo {
        BiomeInfo {
            id: BiomeId(id),
            name: name.to_string(),
            map_color: 0x11_22_33,
        }
    }

    #[test]
    fn empty_catalogs_report_empty() {
        let biomes = BiomeCatalog::default();
        assert!(biomes.is_empty());
        assert_eq!(biomes.len(), 0);
        assert!(biomes.get(BiomeId(0)).is_none());

        let blocks = BlockCatalog::default();
        assert!(blocks.is_empty());
        assert_eq!(blocks.len(), 0);
        assert!(blocks.get(BlockId(0)).is_none());
        assert!(blocks.get_id("stone").is_none());

        let fluids = FluidCatalog::default();
        assert!(fluids.is_empty());
        assert_eq!(fluids.len(), 0);

        let zones = ZoneCatalog::default();
        assert!(zones.is_empty());
        assert_eq!(zones.len(), 0);
    }

    #[test]
    fn biome_catalog_round_trips_by_id_and_name() {
        let mut catalog = BiomeCatalog::default();
        catalog
            .insert(biome(3, "Emerald Grove"))
            .expect("first insert is unique");
        catalog
            .insert(biome(7, "Howling Sands"))
            .expect("second insert is unique");

        assert_eq!(catalog.len(), 2);
        assert!(!catalog.is_empty());
        assert_eq!(
            catalog.get(BiomeId(3)).map(|i| i.name.as_str()),
            Some("Emerald Grove")
        );
        assert_eq!(catalog.get_by_name("Howling Sands"), Some(BiomeId(7)));
        assert_eq!(catalog.get(BiomeId(42)), None);
        assert_eq!(catalog.get_by_name("nope"), None);
        assert_eq!(catalog.iter().count(), 2);
    }

    #[test]
    fn biome_info_unpacks_map_color_in_rgb_channel_order() {
        let info = BiomeInfo {
            id: BiomeId(0),
            name: "x".to_string(),
            map_color: 0x00_11_22_33,
        };
        assert_eq!(info.map_color_rgb(), [0x11, 0x22, 0x33]);
    }

    #[test]
    fn duplicate_biome_inserts_are_errors() {
        let mut catalog = BiomeCatalog::default();
        catalog.insert(biome(1, "One")).expect("unique");
        assert!(matches!(
            catalog.insert(biome(1, "Other")),
            Err(SourceError::Internal(_))
        ));
        assert!(matches!(
            catalog.insert(biome(2, "One")),
            Err(SourceError::Internal(_))
        ));
        assert_eq!(catalog.len(), 1);
    }

    #[test]
    fn block_catalog_round_trips_and_rejects_duplicates() {
        let mut catalog = BlockCatalog::default();
        catalog.insert(BlockId(1), "Stone").expect("unique");
        catalog.insert(BlockId(2), "Dirt").expect("unique");

        assert_eq!(catalog.len(), 2);
        assert_eq!(catalog.get(BlockId(1)), Some("Stone"));
        assert_eq!(catalog.get_id("Dirt"), Some(BlockId(2)));
        assert_eq!(catalog.get(BlockId(9)), None);
        assert!(matches!(
            catalog.insert(BlockId(1), "Granite"),
            Err(SourceError::Internal(_))
        ));
        assert!(matches!(
            catalog.insert(BlockId(3), "Stone"),
            Err(SourceError::Internal(_))
        ));
        assert_eq!(catalog.iter().count(), 2);
    }

    #[test]
    fn fluid_catalog_round_trips_and_rejects_duplicates() {
        let mut catalog = FluidCatalog::default();
        catalog.insert(FluidId(1), "Water").expect("unique");

        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog.get(FluidId(1)), Some("Water"));
        assert_eq!(catalog.get_id("Water"), Some(FluidId(1)));
        assert!(matches!(
            catalog.insert(FluidId(1), "Lava"),
            Err(SourceError::Internal(_))
        ));
        assert!(matches!(
            catalog.insert(FluidId(2), "Water"),
            Err(SourceError::Internal(_))
        ));
        assert_eq!(catalog.iter().count(), 1);
    }

    #[test]
    fn zone_catalog_round_trips_and_rejects_duplicates() {
        let mut catalog = ZoneCatalog::default();
        catalog
            .insert(ZoneInfo {
                id: ZoneId(0),
                name: "Emerald Grove".to_string(),
            })
            .expect("unique");

        assert_eq!(catalog.len(), 1);
        assert_eq!(
            catalog.get(ZoneId(0)).map(|z| z.name.as_str()),
            Some("Emerald Grove")
        );
        assert_eq!(catalog.get_by_name("Emerald Grove"), Some(ZoneId(0)));
        assert!(matches!(
            catalog.insert(ZoneInfo {
                id: ZoneId(0),
                name: "Other".to_string(),
            }),
            Err(SourceError::Internal(_))
        ));
        assert_eq!(catalog.iter().count(), 1);
    }
}
