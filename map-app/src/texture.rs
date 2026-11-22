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
//!
//! Since the pooling packet the bridge also owns **derived** pooled
//! placeholders ([`TextureCache::upload_pooled`]): coarse pixels averaged
//! from the finer tiles already on screen, which the real tile replaces
//! when it renders. They live in their own map, keyed by
//! `(tile, source level)`, and are swept per frame — never per-frame
//! uploads, never GPU leaks.

use std::collections::HashMap;

use eframe::egui;
use map_core::tiles::{PooledTile, Tile, TileKey};

/// The per-tile name prefix in egui's manager (the key's `Debug` form is
/// unique, so re-uploads replace instead of accumulate).
const TILE_TEXTURE_NAME: &str = "map/tile/";

/// The per-pooled-tile name prefix (with the source level appended, since
/// one tile key may be derived from several finer levels over a session).
const POOLED_TEXTURE_NAME: &str = "map/pooled/";

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
///
/// The pooled half of the bridge is deliberately separate: derived
/// pixels are never cached in map-core's [`TileCache`], so their handles
/// are swept here each frame instead of following eviction events.
pub struct TextureCache {
    textures: HashMap<TileKey, egui::TextureHandle>,
    /// Derived pooled placeholders, keyed by `(the tile they stand in
    /// for, source level step)`: a different source level is different
    /// pixels, while the same pair is the same image and uploads once.
    pooled: HashMap<(TileKey, u32), PooledEntry>,
    /// The frame the pooled sweep is currently collecting for; bumped by
    /// [`TextureCache::retire_unused_pooled`] after it ran.
    frame: u64,
}

/// One uploaded pooled placeholder: its GPU handle and the frame it was
/// last painted in.
struct PooledEntry {
    handle: egui::TextureHandle,
    last_used: u64,
}

impl TextureCache {
    /// An empty texture cache.
    #[must_use]
    pub fn new() -> Self {
        Self {
            textures: HashMap::new(),
            pooled: HashMap::new(),
            frame: 0,
        }
    }

    /// Uploads `tile`'s pixels under `key` (NEAREST, matching the T13
    /// filtering decision) and returns the painting id. Re-uploading a
    /// cached key allocates a fresh texture; the previous one frees when
    /// its handle is replaced here (egui never dedups by name).
    ///
    /// Every pooled derivation for `key` is dropped: the real tile is
    /// what the pool stood in for, so no derived handle may outlive it.
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
        // At most a handful of derivations (one per level scanned) exist
        // per key; dropping them all is the retirement contract.
        self.pooled.retain(|(tile, _), _| *tile != key);
        id
    }

    /// The painting id of an uploaded tile.
    #[must_use]
    pub fn texture_id(&self, key: &TileKey) -> Option<egui::TextureId> {
        self.textures.get(key).map(egui::TextureHandle::id)
    }

    /// The painting id of a pooled placeholder already uploaded for
    /// `key`, whichever source level derived it; marks it used this frame
    /// so [`TextureCache::retire_unused_pooled`] keeps it one more frame.
    ///
    /// The caller paints it with the full UV — a pooled tile covers
    /// exactly its key's world rect.
    #[must_use]
    pub fn pooled_texture_id(&mut self, key: &TileKey) -> Option<egui::TextureId> {
        let entry = self
            .pooled
            .iter_mut()
            .find(|((tile, _), _)| tile == key)
            .map(|(_, entry)| entry)?;
        entry.last_used = self.frame;
        Some(entry.handle.id())
    }

    /// Uploads a derived pooled tile's pixels, keyed by
    /// `(tile key, source level)` so repeated frames reuse one handle
    /// instead of re-deriving and re-uploading. Returns the painting id.
    ///
    /// Queued for the next sweep like any other pooled entry: a caller
    /// that never paints it again in the same frame loses it.
    pub fn upload_pooled(&mut self, ctx: &egui::Context, pooled: &PooledTile) -> egui::TextureId {
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [pooled.width as usize, pooled.height as usize],
            &pooled.rgba,
        );
        let level = pooled.source_zoom.step();
        let handle = ctx.load_texture(
            format!("{POOLED_TEXTURE_NAME}{:?}/{level}", pooled.key),
            image,
            egui::TextureOptions::NEAREST,
        );
        let id = handle.id();
        self.pooled.insert(
            (pooled.key, level),
            PooledEntry {
                handle,
                last_used: self.frame,
            },
        );
        id
    }

    /// Drops every pooled placeholder that was not painted since the last
    /// sweep — its tile stopped being pending (the real upload already
    /// retired it) or the viewport moved on. Returns how many handles
    /// were dropped. The app calls this once per frame, after painting.
    pub fn retire_unused_pooled(&mut self) -> usize {
        let used = self.frame;
        let before = self.pooled.len();
        self.pooled.retain(|_, entry| entry.last_used == used);
        self.frame = self.frame.wrapping_add(1);
        before - self.pooled.len()
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
        self.pooled.clear();
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
    use map_core::coords::ZoomLevel;
    use map_core::tiles::TileCache;

    /// A small (2×2) tile for the bridge tests — the cache is
    /// byte-weighed, so tiny tiles keep the budgets tiny too.
    fn small_tile(key: TileKey, byte: u8) -> Tile {
        Tile::new(key, 2, 2, vec![byte; 16]).expect("the buffer matches 2x2 RGBA")
    }

    /// A biome-layer, zoom-0 key at `(tx, ty)` under a synthetic identity.
    fn tile_key(tx: i32, ty: i32) -> TileKey {
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

        // A pooled placeholder of a tile that was never cached clears too.
        let pooled_key = tile_key(2, 0);
        textures.upload_pooled(&ctx, &pooled_tile(pooled_key, ZoomLevel::MIN));
        assert_eq!(textures.pooled.len(), 1);

        textures.clear();
        assert_eq!(textures.texture_id(&a), None);
        assert_eq!(textures.texture_id(&b), None);
        assert!(
            textures.pooled.is_empty(),
            "derived handles clear with the rest"
        );
        assert_eq!(textures.pooled_texture_id(&pooled_key), None);
    }

    /// A derived 2×2 pooled tile for `key`, sourced from `source_zoom`.
    fn pooled_tile(key: TileKey, source_zoom: ZoomLevel) -> PooledTile {
        PooledTile {
            key,
            width: 2,
            height: 2,
            rgba: [40, 50, 60, 255].repeat(4).to_vec(),
            source_zoom,
        }
    }

    #[test]
    fn texture_cache_pooled_uploads_once_and_is_swept_when_unused() {
        let ctx = egui::Context::default();
        let mut textures = TextureCache::new();
        let key = tile_key(0, 0);
        let fine = ZoomLevel::MIN;
        assert!(
            textures.pooled_texture_id(&key).is_none(),
            "nothing uploaded"
        );

        // One upload, then the painted frame's sweep keeps it.
        let id = textures.upload_pooled(&ctx, &pooled_tile(key, fine));
        assert_eq!(textures.pooled_texture_id(&key), Some(id));
        assert_eq!(
            textures.retire_unused_pooled(),
            0,
            "it was painted this frame"
        );

        // A frame that does not paint it (the viewport moved on) retires it.
        assert_eq!(textures.retire_unused_pooled(), 1);
        assert!(textures.pooled_texture_id(&key).is_none());
    }

    #[test]
    fn texture_cache_pooled_keys_carry_the_source_level() {
        // The same tile derived from two finer levels is two entries: a
        // different source is different pixels, so neither may answer for
        // the other.
        let ctx = egui::Context::default();
        let mut textures = TextureCache::new();
        let key = tile_key(0, 0);
        let (coarse, finer) = (
            ZoomLevel::new(1).expect("in range"),
            ZoomLevel::new(0).expect("in range"),
        );
        let first = textures.upload_pooled(&ctx, &pooled_tile(key, finer));
        let second = textures.upload_pooled(&ctx, &pooled_tile(key, coarse));
        assert_ne!(first, second, "egui allocated a fresh texture");
        assert_eq!(textures.pooled.len(), 2, "one entry per (tile, level)");

        // Re-uploading one pair replaces that entry and keeps the other.
        let replaced = textures.upload_pooled(&ctx, &pooled_tile(key, finer));
        assert_eq!(
            textures.pooled.len(),
            2,
            "the pair, not the tile, is the key"
        );
        assert_ne!(replaced, first);
    }

    #[test]
    fn texture_cache_pooled_retires_when_the_real_tile_arrives() {
        let ctx = egui::Context::default();
        let mut textures = TextureCache::new();
        let key = tile_key(0, 0);
        let fine = ZoomLevel::MIN;
        textures.upload_pooled(&ctx, &pooled_tile(key, fine));
        assert!(textures.pooled_texture_id(&key).is_some());

        textures.upload(&ctx, key, &small_tile(key, 9));
        assert!(
            textures.pooled.is_empty(),
            "the real tile replaces every derivation of its key"
        );
        assert_eq!(textures.pooled_texture_id(&key), None);
        assert!(textures.texture_id(&key).is_some());
    }
}
