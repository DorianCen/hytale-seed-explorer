//! egui texture plumbing (T13, extended by T14): map-core's RGBA bytes
//! in, cached egui textures out.
//!
//! The headless side stays free of egui (map-core returns
//! [`RgbaImage`]); this module is the only place pixels cross into the
//! GUI. [`MapTexture`] covers the current whole view — the T15/T16 tile
//! pipeline replaces it with per-tile textures, cached by the
//! [`TextureCache`] bridge below.
//!
//! Filtering decision (the task asked for one): **NEAREST**. Each texel
//! is one `step`-block sample — the crisp cell look is the plan's
//! chunkbase-style 2×2 default, and LINEAR would smear biome borders
//! that are supposed to be exact.

use std::collections::HashMap;

use eframe::egui;
use map_core::render::RgbaImage;
use map_core::tiles::{Tile, TileCache, TileKey};
use map_core::types::Rect;

/// The texture's name in egui's manager (one slot; replaced on upload).
const TEXTURE_NAME: &str = "map/biome";

/// The per-tile name prefix in egui's manager (the key's `Debug` form is
/// unique, so re-uploads replace instead of accumulate).
#[allow(dead_code)] // reserved: the TextureCache bridge (T15/T16)
const TILE_TEXTURE_NAME: &str = "map/tile/";

/// Converts the headless RGBA buffer into egui's RAM image.
///
/// `from_rgba_unmultiplied` copies row-major — the same orientation
/// contract as [`RgbaImage`] (grid row 0 / north at image top), so no
/// flip happens here.
#[must_use]
pub fn to_color_image(image: &RgbaImage) -> egui::ColorImage {
    egui::ColorImage::from_rgba_unmultiplied(
        [image.width() as usize, image.height() as usize],
        image.as_raw(),
    )
}

/// The uploaded map texture plus the world it covers.
///
/// The egui [`egui::TextureHandle`] is app-side by design; this pairs it
/// with the world-space rect and sampling step it was rendered for, so
/// painting can map it back to the screen (and T16 can invalidate it when
/// the view moves).
pub struct MapTexture {
    handle: egui::TextureHandle,
    rect: Rect,
    step: u32,
}

impl MapTexture {
    /// Uploads `image` (rendered for `rect` at `step`) as a new
    /// NEAREST-filtered texture.
    pub fn upload(ctx: &egui::Context, image: &RgbaImage, rect: Rect, step: u32) -> MapTexture {
        let handle = ctx.load_texture(
            TEXTURE_NAME,
            to_color_image(image),
            egui::TextureOptions::NEAREST,
        );
        MapTexture { handle, rect, step }
    }

    /// The texture id for painting.
    #[must_use]
    pub fn id(&self) -> egui::TextureId {
        self.handle.id()
    }

    /// The world rect (inclusive) the texture covers.
    #[must_use]
    pub fn rect(&self) -> Rect {
        self.rect
    }

    /// The sampling step the texture was rendered at.
    #[allow(dead_code)] // reserved: T16 invalidates tiles when the step changes
    #[must_use]
    pub fn step(&self) -> u32 {
        self.step
    }
}

/// The screen rect the texture paints into, for the view
/// (`center`, `scale`) inside the map `area`.
///
/// The world→screen mapping is T10's: the view center sits at the
/// area's center, one block is `scale` points, and world Z increases
/// south (down the screen). The rect's bounds are *inclusive*, so the
/// pixel grid spans exactly `min ..= max + 1` blocks per axis.
#[must_use]
pub fn texture_screen_rect(
    texture: &MapTexture,
    center: (f64, f64),
    scale: f64,
    area: egui::Rect,
) -> egui::Rect {
    let to_screen = |world_x: f64, world_z: f64| -> egui::Pos2 {
        egui::pos2(
            ((world_x - center.0) * scale) as f32 + area.center().x,
            ((world_z - center.1) * scale) as f32 + area.center().y,
        )
    };
    let rect = texture.rect();
    let min = to_screen(f64::from(rect.min_x), f64::from(rect.min_z));
    let max = to_screen(f64::from(rect.max_x) + 1.0, f64::from(rect.max_z) + 1.0);
    egui::Rect::from_min_max(min, max)
}

// ---------------------------------------------------------------------------
// Tile texture bridge (T14)
// ---------------------------------------------------------------------------

/// The GPU-side mirror of map-core's [`TileCache`]: one uploaded egui
/// texture per cached tile, keyed by the same [`TileKey`].
///
/// The CPU cache is the source of truth. [`TextureCache::upload`] moves
/// a tile's pixels to the GPU once per key; eviction events (via
/// [`TextureCache::drop_evicted`]) and the periodic
/// [`TextureCache::retain_cached`] sweep keep the two in step — dropping
/// an [`egui::TextureHandle`] is what frees the GPU memory, which is why
/// every tile-cache removal must reach this map. T15/T16 wire the frame
/// loop; the unit tests below drive the bridge headlessly with a plain
/// `egui::Context`.
///
/// v1 policy (the plan's simplest option, per the task's risk note):
/// this map holds exactly the keys the tile cache holds, so a cached
/// tile never pays a re-upload and an evicted tile never leaks GPU
/// memory.
#[allow(dead_code)] // reserved: T15/T16 wire the tile frame loop
pub struct TextureCache {
    textures: HashMap<TileKey, egui::TextureHandle>,
}

#[allow(dead_code)] // reserved: T15/T16 wire the tile frame loop
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

    /// Drops handles whose tile is no longer in the CPU cache — the
    /// periodic sweep that also covers `TileCache::clear`, which sends
    /// no per-entry events. Returns how many handles were dropped.
    pub fn retain_cached(&mut self, cache: &TileCache) -> usize {
        let before = self.textures.len();
        self.textures.retain(|key, _| cache.contains(key));
        before - self.textures.len()
    }

    /// Drops every handle. A source change clears both caches; the
    /// tiles of a dead seed must never stay on screen.
    pub fn clear(&mut self) {
        self.textures.clear();
    }

    /// How many tile textures are uploaded right now.
    #[must_use]
    pub fn len(&self) -> usize {
        self.textures.len()
    }

    /// Whether no tile textures are uploaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.textures.is_empty()
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

    fn sample_image() -> RgbaImage {
        use map_core::render::image::Rgba;

        let mut image = RgbaImage::new(2, 1);
        image.put_pixel(0, 0, Rgba([10, 20, 30, 255]));
        image.put_pixel(1, 0, Rgba([40, 50, 60, 255]));
        image
    }

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
    fn to_color_image_keeps_dimensions_pixels_and_orientation() {
        let image = sample_image();
        let color_image = to_color_image(&image);

        assert_eq!(color_image.size, [2, 1]);
        assert_eq!(color_image.pixels.len(), 2);
        // Row-major, no flip: the first raw pixel is the first Color32.
        assert_eq!(
            color_image.pixels[0],
            egui::Color32::from_rgb(10, 20, 30),
            "grid row 0 (north) stays at the image top"
        );
        assert_eq!(color_image.pixels[1], egui::Color32::from_rgb(40, 50, 60));
        // The byte-length invariant the conversion relies on (2 * 1 * 4).
        assert_eq!(image.as_raw().len(), 8);
    }

    #[test]
    fn texture_screen_rect_maps_world_to_screen_centered() {
        let ctx = egui::Context::default();
        let image = sample_image();
        // A 2x1-sample texture at step 2 covers 4x2 blocks: (0,0)..(3,1).
        let texture = MapTexture::upload(&ctx, &image, Rect::new(0, 0, 3, 1), 2);

        // A 100x100 map area, view centered on (0,0) at 2 points/block:
        // the texture's min corner sits at the area center, its size is
        // blocks * scale = 4*2 x 2*2.
        let area = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(100.0, 100.0));
        let screen = texture_screen_rect(&texture, (0.0, 0.0), 2.0, area);
        assert_eq!(screen.min, egui::pos2(50.0, 50.0));
        assert!((screen.width() - 8.0).abs() < 0.001);
        assert!((screen.height() - 4.0).abs() < 0.001);

        // A shifted center slides the texture left/up in screen space,
        // and north (min z) stays at the smaller y (image top).
        let screen = texture_screen_rect(&texture, (2.0, 1.0), 2.0, area);
        assert_eq!(screen.min, egui::pos2(50.0 - 4.0, 50.0 - 2.0));
        assert!((screen.width() - 8.0).abs() < 0.001);
    }

    // -- TextureCache (the T14 tile bridge) ----------------------------------

    #[test]
    fn texture_cache_uploads_each_tile_once_and_keys_by_tile_key() {
        let ctx = egui::Context::default();
        let mut textures = TextureCache::new();
        assert!(textures.is_empty());

        let (a, b) = (tile_key(0, 0), tile_key(1, 0));
        let id_a = textures.upload(&ctx, a, &small_tile(a, 10));
        let id_b = textures.upload(&ctx, b, &small_tile(b, 20));
        assert_eq!(textures.len(), 2);
        assert_eq!(textures.texture_id(&a), Some(id_a));
        assert_eq!(textures.texture_id(&b), Some(id_b));
        assert_eq!(textures.texture_id(&tile_key(9, 9)), None);

        // Re-uploading the same key replaces the entry (a fresh texture;
        // egui's alloc never reuses ids), keeping one handle per key.
        let id_a_fresh = textures.upload(&ctx, a, &small_tile(a, 30));
        assert_eq!(textures.len(), 2, "the key is still cached once");
        assert_eq!(textures.texture_id(&a), Some(id_a_fresh));
        assert_ne!(id_a_fresh, id_a, "egui allocated a fresh texture");
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
        assert_eq!(textures.len(), 2);

        let dropped = textures.drop_evicted(events.try_iter());
        assert_eq!(dropped, 1, "exactly A's eviction event arrived");
        assert_eq!(textures.texture_id(&a), None, "A's handle is gone");
        assert!(textures.texture_id(&b).is_some(), "B stays uploaded");
        assert_eq!(textures.len(), 1);
    }

    #[test]
    fn texture_cache_retain_cached_drops_uncached_handles() {
        // The periodic sweep catches removals whose event was missed —
        // including TileCache::clear, which sends no per-entry events.
        let ctx = egui::Context::default();
        let (a, b) = (tile_key(0, 0), tile_key(1, 0));
        let tile_cache = TileCache::new(1024);
        tile_cache.insert(std::sync::Arc::new(small_tile(a, 10)));
        tile_cache.insert(std::sync::Arc::new(small_tile(b, 20)));

        let mut textures = TextureCache::new();
        textures.upload(&ctx, a, &small_tile(a, 10));
        textures.upload(&ctx, b, &small_tile(b, 20));

        // Consume no eviction events at all; the sweep still syncs.
        tile_cache.remove(&a);
        assert_eq!(textures.retain_cached(&tile_cache), 1, "A's handle dropped");
        assert_eq!(textures.texture_id(&a), None);
        assert_eq!(textures.len(), 1);

        // And clear() in the CPU cache sweeps the GPU side clean.
        tile_cache.clear();
        assert_eq!(textures.retain_cached(&tile_cache), 1, "B's handle dropped");
        assert!(textures.is_empty());
    }

    #[test]
    fn texture_cache_clear_empties_every_handle() {
        let ctx = egui::Context::default();
        let mut textures = TextureCache::new();
        let a = tile_key(0, 0);
        textures.upload(&ctx, a, &small_tile(a, 10));
        textures.upload(&ctx, tile_key(1, 0), &small_tile(tile_key(1, 0), 20));
        assert_eq!(textures.len(), 2);

        textures.clear();
        assert!(textures.is_empty());
        assert_eq!(textures.texture_id(&a), None);
    }
}
