//! egui texture plumbing (T13, the tile bridge wired by T14/T16):
//! map-core's RGBA bytes in, cached egui textures out.
//!
//! The headless side stays free of egui (map-core returns its RGBA
//! buffers as [`Tile`] pixels); this module is the only place pixels
//! cross into the GUI. Since T16 the map is painted per tile:
//! [`TextureCache`] mirrors map-core's
//! [`TileCache`](map_core::tiles::TileCache) — one uploaded texture per
//! cached tile, keyed by the same [`TileKey`].
//! Filtering decision (the task asked for one): **NEAREST**. Each texel
//! is one `step`-block sample — the crisp cell look is the plan's
//! chunkbase-style 2×2 default, and LINEAR would smear biome borders
//! that are supposed to be exact.

use std::collections::HashMap;

use eframe::egui;
use map_core::tiles::{Tile, TileKey};

/// The per-tile name prefix in egui's manager (the key's `Debug` form is
/// unique, so re-uploads replace instead of accumulate).
const TILE_TEXTURE_NAME: &str = "map/tile/";

// ---------------------------------------------------------------------------
// Tile texture bridge (T14)
// ---------------------------------------------------------------------------

/// The GPU-side mirror of map-core's [`TileCache`]: one uploaded egui
/// texture per cached tile, keyed by the same [`TileKey`].
///
/// The CPU cache is the source of truth. [`TextureCache::upload`] moves
/// a tile's pixels to the GPU once per key; eviction events (via
/// [`TextureCache::drop_evicted`], drained each frame) drop the handles
/// of CPU-evicted tiles, and wholesale retirement clears the map —
/// dropping an [`egui::TextureHandle`] is what frees the GPU memory.
/// The T16 frame loop drives it; the unit tests below exercise the
/// bridge headlessly with a plain `egui::Context`.
///
/// v1 policy (the plan's simplest option, per the task's risk note):
/// this map holds exactly the keys the tile cache holds, so a cached
/// tile never pays a re-upload and an evicted tile never leaks GPU
/// memory.
pub struct TextureCache {
    textures: HashMap<TileKey, egui::TextureHandle>,
}

impl TextureCache {
    /// An empty texture cache.
    #[must_use]
    pub fn new() -> Self {
        Self {
            textures: HashMap::new(),
        }
    }

    /// Uploads `tile`'s pixels under `key` (NEAREST, matching the T13
    /// filtering decision) and returns the painting id. Re-uploading a
    /// cached key allocates a fresh texture; the previous one frees when
    /// its handle is replaced here (egui never dedups by name).
    pub fn upload(&mut self, ctx: &egui::Context, key: TileKey, tile: &Tile) -> egui::TextureId {
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [tile.width as usize, tile.height as usize],
            &tile.rgba,
        );
        let handle = ctx.load_texture(
            format!("{TILE_TEXTURE_NAME}{key:?}"),
            image,
            egui::TextureOptions::NEAREST,
        );
        let id = handle.id();
        self.textures.insert(key, handle);
        id
    }

    /// The painting id of an uploaded tile.
    #[must_use]
    pub fn texture_id(&self, key: &TileKey) -> Option<egui::TextureId> {
        self.textures.get(key).map(egui::TextureHandle::id)
    }

    /// Drops the handles for the given evicted/invalidated keys — the
    /// drain of `TileCache::subscribe_evictions` — freeing their GPU
    /// memory. Returns how many handles were dropped.
    pub fn drop_evicted(&mut self, evicted: impl IntoIterator<Item = TileKey>) -> usize {
        let before = self.textures.len();
        for key in evicted {
            self.textures.remove(&key);
        }
        before - self.textures.len()
    }

    /// Drops every handle. A source change clears both caches; the
    /// tiles of a dead seed must never stay on screen.
    pub fn clear(&mut self) {
        self.textures.clear();
    }
}

impl Default for TextureCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use map_core::tiles::TileCache;

    /// A small (2×2) tile for the bridge tests — the cache is
    /// byte-weighed, so tiny tiles keep the budgets tiny too.
    fn small_tile(key: TileKey, byte: u8) -> Tile {
        Tile::new(key, 2, 2, vec![byte; 16]).expect("the buffer matches 2x2 RGBA")
    }

    /// A biome-layer, zoom-0 key at `(tx, ty)` under a synthetic identity.
    fn tile_key(tx: i32, ty: i32) -> TileKey {
        use map_core::coords::ZoomLevel;
        use map_core::render::Layer;

        TileKey {
            identity_hash: 1,
            generator_seed: 7,
            layer: Layer::Biome,
            mode: 0,
            zoom: ZoomLevel::MIN,
            tx,
            ty,
        }
    }

    #[test]
    fn texture_cache_uploads_each_tile_once_and_keys_by_tile_key() {
        let ctx = egui::Context::default();
        let mut textures = TextureCache::new();
        let missing = tile_key(9, 9);
        assert!(textures.texture_id(&missing).is_none(), "empty to start");

        let (a, b) = (tile_key(0, 0), tile_key(1, 0));
        let id_a = textures.upload(&ctx, a, &small_tile(a, 10));
        let id_b = textures.upload(&ctx, b, &small_tile(b, 20));
        assert_eq!(textures.texture_id(&a), Some(id_a));
        assert_eq!(textures.texture_id(&b), Some(id_b));
        assert_eq!(textures.texture_id(&missing), None);

        // Re-uploading the same key replaces the entry (a fresh texture;
        // egui's alloc never reuses ids), keeping one handle per key.
        let id_a_fresh = textures.upload(&ctx, a, &small_tile(a, 30));
        assert_eq!(textures.texture_id(&a), Some(id_a_fresh));
        assert_ne!(id_a_fresh, id_a, "egui allocated a fresh texture");
        assert_eq!(textures.texture_id(&b), Some(id_b), "B keeps its handle");
    }

    #[test]
    fn texture_cache_drops_handles_for_evicted_tiles() {
        // A one-tile CPU budget: inserting B evicts A, and the eviction
        // event must drop A's handle — GPU memory follows the CPU cache.
        let ctx = egui::Context::default();
        let (a, b) = (tile_key(0, 0), tile_key(1, 0));
        let tile_cache = TileCache::new(small_tile(a, 10).byte_size());
        let events = tile_cache.subscribe_evictions();
        let mut textures = TextureCache::new();

        tile_cache.insert(std::sync::Arc::new(small_tile(a, 10)));
        textures.upload(&ctx, a, &small_tile(a, 10));
        tile_cache.insert(std::sync::Arc::new(small_tile(b, 20)));
        textures.upload(&ctx, b, &small_tile(b, 20));
        assert!(textures.texture_id(&a).is_some());
        assert!(textures.texture_id(&b).is_some());

        let dropped = textures.drop_evicted(events.try_iter());
        assert_eq!(dropped, 1, "exactly A's eviction event arrived");
        assert_eq!(textures.texture_id(&a), None, "A's handle is gone");
        assert!(textures.texture_id(&b).is_some(), "B stays uploaded");
    }

    #[test]
    fn texture_cache_clear_empties_every_handle() {
        let ctx = egui::Context::default();
        let mut textures = TextureCache::new();
        let (a, b) = (tile_key(0, 0), tile_key(1, 0));
        textures.upload(&ctx, a, &small_tile(a, 10));
        textures.upload(&ctx, b, &small_tile(b, 20));
        assert!(textures.texture_id(&a).is_some());

        textures.clear();
        assert_eq!(textures.texture_id(&a), None);
        assert_eq!(textures.texture_id(&b), None);
    }
}
