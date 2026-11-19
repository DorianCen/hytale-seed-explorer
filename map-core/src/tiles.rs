//! Tile identity, the bounded [`TileCache`] and eviction (T14).
//!
//! A tile is one [`TILE_SIZE`](crate::coords::TILE_SIZE)-square RGBA
//! image covering `TILE_SIZE * step` blocks at tile coordinates
//! `(tx, ty)`. This module owns the contract that makes pan/zoom fast
//! *and* correct:
//!
//! - **A tile is never shown for the wrong source, layer, resolution or
//!   position.** [`TileKey`] hashes the generator identity, the
//!   generator-visible seed, the layer (+ mode), the zoom and the tile
//!   coordinates — the plan's `(generator_version, seed, layer, mode,
//!   zoom, tx, ty)`. Any change to any field is a different key, so a
//!   stale tile cannot be looked up by accident (the classic
//!   seed-change bug the plan's risks section warns about).
//! - **Memory is bounded.** [`TileCache`] is a byte-weighed LRU with a
//!   fixed budget. Inserting past the budget evicts the least recently
//!   used tiles synchronously and notifies subscribers, so `map-app`
//!   can drop the matching GPU textures in the same frame.
//! - **Eviction is observable.** [`TileCache::subscribe_evictions`]
//!   hands out a channel receiver; the texture bridge (`map-app`) drops
//!   `TextureHandle`s for exactly those keys, which is what frees GPU
//!   memory.
//!
//! # Why a custom LRU instead of `moka`
//!
//! The task sanctioned either. `moka`'s maintenance is asynchronous and
//! its eviction listener fires on background threads, which makes three
//! requirements awkward at once: exact byte accounting for
//! [`TileCache::stats`] (no temporary overshoot), *synchronous*
//! eviction notifications, and [`TileCache::invalidate_seed`], which
//! must iterate keys — `moka` offers no key iteration. A small
//! mutex-guarded LRU is deterministic, has no background threads (so
//! no listener can ever fire after the app is gone — the task's risk
//! note cannot arise) and stays cheap at plan scale: a 512 MB budget is
//! ~2000 entries, and the lazy-recency design below keeps every
//! operation amortized O(1).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender, unbounded};
use serde::{Deserialize, Serialize};

use crate::coords::ZoomLevel;
use crate::render::Layer;

/// Fixed bytes [`Tile::byte_size`] charges on top of the pixel buffer:
/// an approximation covering the `Tile` struct itself plus the cache's
/// per-entry bookkeeping (one map entry and one recency record).
const FIXED_OVERHEAD_BYTES: u64 = 96;

// ---------------------------------------------------------------------------
// TileKey
// ---------------------------------------------------------------------------

/// The identity of one rendered tile — the cache key that makes stale
/// tiles impossible to serve.
///
/// Every field that can change the pixels is in the key. A lookup for a
/// changed source, layer, resolution or position is a *different key*,
/// so it can never hit an entry produced for anything else.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TileKey {
    /// Stable hash of the
    /// [`GeneratorIdentity`](crate::types::GeneratorIdentity):
    /// generator version + world assets + palette version. An asset or
    /// palette change invalidates tiles exactly like a version bump
    /// (docs/seed-semantics.md §6).
    pub identity_hash: u64,
    /// The generator-visible seed: the low 32 bits of the seed input
    /// (T02). Terrain cannot distinguish more, so the key must not either.
    pub generator_seed: i32,
    /// The rendered layer (see [`Layer`]).
    pub layer: Layer,
    /// Layer-specific mode (e.g. a cave Y bucket for
    /// [`Layer::CaveSlice`]); `0` when the layer has no modes.
    pub mode: u16,
    /// The zoom level whose [`step`](ZoomLevel::step) is the sampling
    /// step the pixels were generated at. Callers fold the user's
    /// minimum step in first ([`effective_step`](crate::coords::effective_step))
    /// and re-derive the level via [`ZoomLevel::from_step`], so two
    /// views whose effective steps agree share one tile instead of
    /// duplicating identical pixels — while two zooms with different
    /// steps never collide.
    pub zoom: ZoomLevel,
    /// Tile X index; negative blocks have negative tile indices (T10).
    pub tx: i32,
    /// Tile Z index; same floor semantics as `tx`.
    pub ty: i32,
}

// ---------------------------------------------------------------------------
// Tile
// ---------------------------------------------------------------------------

/// One rendered tile: `width * height` RGBA8 pixels, row-major, north at
/// row 0 — the same orientation contract as
/// [`RgbaImage`](crate::render::RgbaImage). The scheduler's tiles are
/// [`TILE_SIZE`](crate::coords::TILE_SIZE)-square.
#[derive(Debug, Clone, PartialEq)]
pub struct Tile {
    pub key: TileKey,
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    pub rgba: Arc<[u8]>,
    /// When the tile was created. A generation/priority stamp for T15's
    /// scheduling and T33's tuning reports; not part of any key.
    pub created: Instant,
}

impl Tile {
    /// Creates a tile, or [`None`] when the buffer length does not match
    /// `width * height * 4`, or either dimension is zero — a renderer
    /// bug, rejected rather than panicked on.
    #[must_use]
    pub fn new(key: TileKey, width: u32, height: u32, rgba: impl Into<Arc<[u8]>>) -> Option<Tile> {
        if width == 0 || height == 0 {
            return None;
        }
        let rgba = rgba.into();
        let expected = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|pixels| pixels.checked_mul(4));
        if expected != Some(rgba.len() as u64) {
            return None;
        }
        Some(Tile {
            key,
            width,
            height,
            rgba,
            created: Instant::now(),
        })
    }

    /// The RGBA pixel at `(x, y)` (row-major), or [`None`] outside the
    /// image. A test helper; painting uploads the whole buffer.
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        // `new` validated width * height * 4 against the buffer length,
        // so this index is in bounds by construction.
        let index = y as usize * self.width as usize + x as usize;
        Some(
            self.rgba[index * 4..index * 4 + 4]
                .try_into()
                .expect("four bytes remain; new() validated the buffer length"),
        )
    }

    /// Resident bytes this tile charges against a cache budget: the
    /// pixel buffer plus fixed overhead.
    #[must_use]
    pub fn byte_size(&self) -> u64 {
        FIXED_OVERHEAD_BYTES + self.rgba.len() as u64
    }
}

// ---------------------------------------------------------------------------
// CacheStats
// ---------------------------------------------------------------------------

/// A snapshot of cache state — the debug overlay's hook (T33) and the
/// test witness for the budget invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheStats {
    /// Cached entries right now.
    pub entries: u64,
    /// Bytes charged right now; never above `byte_budget` after any
    /// cache call returns.
    pub bytes: u64,
    /// The configured byte budget.
    pub byte_budget: u64,
    /// Lookups that hit.
    pub hits: u64,
    /// Lookups that missed.
    pub misses: u64,
    /// Entries evicted to stay within the budget. Explicit removals and
    /// invalidations are not counted.
    pub evictions: u64,
}

// ---------------------------------------------------------------------------
// TileCache
// ---------------------------------------------------------------------------

/// A bounded, thread-safe tile cache: byte-weighed LRU with eviction
/// notifications.
///
/// The budget is bytes of tile data, not entry count. [`TileCache::get`]
/// refreshes recency; [`TileCache::insert`] evicts least-recently-used
/// tiles synchronously until the cache is back within budget, sending
/// every removed [`TileKey`] to the eviction channel so the app can drop
/// the matching GPU textures immediately.
///
/// Deterministic and lock-guarded: no background threads, no timers, no
/// panicking callbacks — the cache cannot notify or account bytes after
/// it is dropped.
pub struct TileCache {
    byte_budget: u64,
    /// The entries, recency records and counters, behind one lock:
    /// tile workers (T15) insert while the UI thread reads.
    inner: Mutex<CacheInner>,
    /// Sends one key per removed/evicted tile. Sends are `let _ =`-ed:
    /// with every receiver dropped they fail, which is fine (the task's
    /// "ignore the send error" rule, structurally impossible to violate
    /// here — sends happen inline, never on a background thread).
    notify: Sender<TileKey>,
    /// The receiving end, kept so [`TileCache::clear`] can drain pending
    /// notifications; [`TileCache::subscribe_evictions`] hands out clones.
    events: Mutex<Receiver<TileKey>>,
}

/// Everything behind the cache lock.
struct CacheInner {
    entries: HashMap<TileKey, CacheEntry>,
    /// Recency records, oldest first. Stale records — superseded touches,
    /// removed entries — are discarded lazily by the eviction sweep, so
    /// every record is pushed once and popped once: amortized O(1).
    order: VecDeque<(TileKey, u64)>,
    /// Monotonic recency stamp.
    next_tick: u64,
    bytes: u64,
    hits: u64,
    misses: u64,
    evictions: u64,
}

/// One cached entry; `tick` is the stamp of the freshest recency record
/// for this key.
struct CacheEntry {
    tile: Arc<Tile>,
    tick: u64,
}

impl TileCache {
    /// An empty cache keeping at most `byte_budget` bytes of tile data.
    /// A budget of zero keeps nothing: every insert evicts the tile it
    /// just inserted.
    #[must_use]
    pub fn new(byte_budget: u64) -> Self {
        let (notify, events) = unbounded();
        TileCache {
            byte_budget,
            inner: Mutex::new(CacheInner {
                entries: HashMap::new(),
                order: VecDeque::new(),
                next_tick: 0,
                bytes: 0,
                hits: 0,
                misses: 0,
                evictions: 0,
            }),
            notify,
            events: Mutex::new(events),
        }
    }

    /// The tile for `key`, refreshing its recency, or [`None`]. Hits and
    /// misses are counted for [`TileCache::stats`].
    #[must_use]
    pub fn get(&self, key: &TileKey) -> Option<Arc<Tile>> {
        let mut inner = lock(&self.inner);
        // The recency stamp is taken up front: the guard's DerefMut does
        // not split borrows, so the entry borrow below must end before
        // the shared counter writes. (A miss "wastes" a tick; stamps only
        // need to be monotonic.)
        let tick = inner.next_tick;
        inner.next_tick += 1;
        let Some(entry) = inner.entries.get_mut(key) else {
            inner.misses += 1;
            return None;
        };
        entry.tick = tick;
        let tile = Arc::clone(&entry.tile);
        inner.order.push_back((*key, tick));
        inner.hits += 1;
        Some(tile)
    }

    /// Stores `tile` under its key, evicting least-recently-used tiles —
    /// possibly the new one, if it alone exceeds the budget — until the
    /// cache is within budget again.
    ///
    /// Keys are content-addressed, so re-inserting an existing key is a
    /// refresh of identical pixels: subscribers are notified (moka's
    /// "replaced" semantics, so the bridge re-uploads) but it does not
    /// count as an eviction.
    pub fn insert(&self, tile: Arc<Tile>) {
        let mut inner = lock(&self.inner);
        inner.put(tile, &self.notify);
        evict_to_budget(&mut inner, self.byte_budget, &self.notify);
    }

    /// Whether `key` is cached — without touching recency or counting a
    /// hit/miss (moka's `contains_key` semantics).
    #[must_use]
    pub fn contains(&self, key: &TileKey) -> bool {
        lock(&self.inner).entries.contains_key(key)
    }

    /// Removes one key explicitly. Subscribers are notified (so the
    /// texture bridge drops the handle); this is not counted as an
    /// eviction.
    pub fn remove(&self, key: &TileKey) {
        let mut inner = lock(&self.inner);
        if inner.entries.remove(key).is_some() {
            let _ = self.notify.send(*key);
        }
    }

    /// Removes every tile generated from `generator_seed`; other seeds'
    /// tiles stay cached. Subscribers are notified per removed key.
    pub fn invalidate_seed(&self, generator_seed: i32) {
        let mut inner = lock(&self.inner);
        let matching: Vec<TileKey> = inner
            .entries
            .keys()
            .filter(|key| key.generator_seed == generator_seed)
            .copied()
            .collect();
        inner.remove_all(&matching, &self.notify);
    }

    /// Removes every tile produced by the generator identity whose stable
    /// hash is `identity_hash` — the tiles a version, asset or palette
    /// bump strands. New tiles carry a different hash, so the old ones
    /// are unreachable by lookup anyway; this frees them on the spot and
    /// keeps other identities' tiles warm (docs/seed-semantics.md §6).
    pub fn invalidate_version(&self, identity_hash: u64) {
        let mut inner = lock(&self.inner);
        let matching: Vec<TileKey> = inner
            .entries
            .keys()
            .filter(|key| key.identity_hash == identity_hash)
            .copied()
            .collect();
        inner.remove_all(&matching, &self.notify);
    }

    /// Empties the cache. No per-entry notifications are sent — a bulk
    /// clear supersedes them — and the eviction channel is drained, so
    /// subscribers see a clean slate and never act on pre-clear events.
    /// (The texture bridge mirrors this with its own clear, or its
    /// periodic [`retain`](TileCache::contains)-based sweep.)
    pub fn clear(&self) {
        let mut inner = lock(&self.inner);
        inner.entries.clear();
        inner.order.clear();
        inner.bytes = 0;
        drop(inner);
        // Discard notifications still pending from earlier removals:
        // after a clear, every such event describes a tile that is gone
        // anyway, and acting on it could drop a freshly re-inserted
        // handle.
        let events = lock(&self.events);
        while events.try_recv().is_ok() {}
    }

    /// A snapshot of the cache state (the T33 overlay hook).
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        let inner = lock(&self.inner);
        CacheStats {
            entries: inner.entries.len() as u64,
            bytes: inner.bytes,
            byte_budget: self.byte_budget,
            hits: inner.hits,
            misses: inner.misses,
            evictions: inner.evictions,
        }
    }

    /// The eviction-notification receiver: one [`TileKey`] per removed
    /// tile (capacity eviction, explicit removal and invalidation).
    ///
    /// Each call hands out a clone that **competes** for messages with
    /// earlier clones, so one subscriber — the app's texture bridge — is
    /// the intended use.
    ///
    /// [`TileCache::clear`] drains the shared channel, absorbing events
    /// a subscriber has not consumed yet; that is safe precisely because
    /// a clear is always paired with the bridge clearing its own map.
    #[must_use]
    pub fn subscribe_evictions(&self) -> Receiver<TileKey> {
        lock(&self.events).clone()
    }
}

impl CacheInner {
    /// Inserts (or refreshes) an entry and its recency record.
    fn put(&mut self, tile: Arc<Tile>, notify: &Sender<TileKey>) {
        let key = tile.key;
        let size = tile.byte_size();
        let tick = self.next_tick;
        self.next_tick += 1;
        if let Some(old) = self.entries.insert(key, CacheEntry { tile, tick }) {
            self.bytes -= old.tile.byte_size();
            let _ = notify.send(key);
        }
        self.bytes += size;
        self.order.push_back((key, tick));
    }

    /// Removes the given keys, keeping `bytes` exact and notifying per
    /// key.
    fn remove_all(&mut self, keys: &[TileKey], notify: &Sender<TileKey>) {
        for key in keys {
            if let Some(entry) = self.entries.remove(key) {
                self.bytes -= entry.tile.byte_size();
                let _ = notify.send(*key);
            }
        }
    }
}

/// Evicts least-recently-used entries until `inner` is within `budget`,
/// notifying per evicted key. Stale recency records are discarded on the
/// way — each entry always keeps its freshest record in the queue, so an
/// empty queue means `entries` is empty and `bytes` is zero.
fn evict_to_budget(inner: &mut CacheInner, budget: u64, notify: &Sender<TileKey>) {
    while inner.bytes > budget {
        let Some((key, tick)) = inner.order.pop_front() else {
            return;
        };
        if !inner
            .entries
            .get(&key)
            .is_some_and(|entry| entry.tick == tick)
        {
            continue;
        }
        if let Some(entry) = inner.entries.remove(&key) {
            inner.bytes -= entry.tile.byte_size();
            inner.evictions += 1;
            let _ = notify.send(key);
        }
    }
}

/// Locks a cache mutex. Nothing in a critical section can panic, so
/// poisoning is unreachable in practice; recovering instead of
/// propagating keeps a failed *test* from cascading into its neighbors.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use crate::types::GeneratorIdentity;

    /// An identity with the given version, mirroring the worldgen source.
    fn identity(version: u32) -> GeneratorIdentity {
        GeneratorIdentity {
            generator_version: version,
            world_assets: String::from("Default/0.0.0"),
            palette_version: 0,
        }
    }

    fn key(
        identity_hash: u64,
        seed: i32,
        layer: Layer,
        mode: u16,
        zoom: u8,
        tx: i32,
        ty: i32,
    ) -> TileKey {
        TileKey {
            identity_hash,
            generator_seed: seed,
            layer,
            mode,
            zoom: ZoomLevel::new(zoom).expect("tests use in-range levels"),
            tx,
            ty,
        }
    }

    #[test]
    fn tile_cache_keys_differ_when_any_field_changes() {
        let base = key(1, 7, Layer::Biome, 0, 1, 3, 4);
        // Equal keys stay equal, however they were built.
        assert_eq!(base, key(1, 7, Layer::Biome, 0, 1, 3, 4));
        // Every field that can change the pixels changes the key.
        let variants = [
            key(2, 7, Layer::Biome, 0, 1, 3, 4),      // identity
            key(1, 8, Layer::Biome, 0, 1, 3, 4),      // seed
            key(1, 7, Layer::Height, 0, 1, 3, 4),     // layer
            key(1, 7, Layer::CaveSlice, 64, 1, 3, 4), // mode (Y bucket)
            key(1, 7, Layer::Biome, 0, 2, 3, 4),      // zoom
            key(1, 7, Layer::Biome, 0, 1, 4, 4),      // tx
            key(1, 7, Layer::Biome, 0, 1, 3, 5),      // ty
        ];
        for variant in variants {
            assert_ne!(base, variant, "{base:?} vs {variant:?}");
            // The derived Hash must differ too — the cache looks keys up
            // by hash before it ever compares them.
            assert_ne!(
                hash_of(base),
                hash_of(variant),
                "the hash collides for {base:?} vs {variant:?}"
            );
        }
    }

    /// Hashes a key the way a `HashMap` would, so tests can witness Hash
    /// (not just Eq) differences. `DefaultHasher` is deterministic within
    /// a build, so only genuinely different keys can differ here.
    fn hash_of(key: TileKey) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn tile_cache_version_bump_invalidates_the_key() {
        // The docs/seed-semantics.md §7 row: a version bump is a
        // different identity hash, hence a different tile key.
        let hash_v1 = identity(1).stable_hash();
        let hash_v2 = identity(2).stable_hash();
        assert_ne!(hash_v1, hash_v2, "the version must move the hash");
        let key_v1 = key(hash_v1, 7, Layer::Biome, 0, 1, 3, 4);
        let key_v2 = key(hash_v2, 7, Layer::Biome, 0, 1, 3, 4);
        assert_ne!(key_v1, key_v2);
    }

    #[test]
    fn tile_cache_tile_new_rejects_wrong_buffer_lengths() {
        let key = key(1, 7, Layer::Biome, 0, 1, 3, 4);
        assert!(
            Tile::new(key, 2, 2, vec![0u8; 15]).is_none(),
            "one byte short"
        );
        assert!(
            Tile::new(key, 2, 2, vec![0u8; 17]).is_none(),
            "one byte long"
        );
        assert!(Tile::new(key, 2, 2, Vec::new()).is_none(), "empty buffer");
        assert!(
            Tile::new(key, 0, 2, vec![0u8; 0]).is_none(),
            "zero-sized width"
        );
        let tile = Tile::new(key, 2, 2, vec![0u8; 16]).expect("the length matches");
        assert_eq!((tile.width, tile.height), (2, 2));
        assert_eq!(tile.rgba.len(), 16);
    }

    #[test]
    fn tile_cache_tile_pixel_reads_row_major_and_bounds_checks() {
        let key = key(1, 7, Layer::Biome, 0, 1, 3, 4);
        // Row-major: (0,0), (1,0), (0,1), (1,1) with distinct pixels.
        let tile = Tile::new(
            key,
            2,
            2,
            vec![
                10, 20, 30, 255, 40, 50, 60, 255, 70, 80, 90, 255, 100, 110, 120, 255,
            ],
        )
        .expect("the length matches");
        assert_eq!(tile.pixel(0, 0), Some([10, 20, 30, 255]));
        assert_eq!(tile.pixel(1, 0), Some([40, 50, 60, 255]), "x moves fastest");
        assert_eq!(tile.pixel(0, 1), Some([70, 80, 90, 255]), "y steps a row");
        assert_eq!(tile.pixel(1, 1), Some([100, 110, 120, 255]));
        // Out of bounds is None, never a panic.
        assert_eq!(tile.pixel(2, 0), None);
        assert_eq!(tile.pixel(0, 2), None);
        assert_eq!(tile.pixel(u32::MAX, 0), None);
    }

    #[test]
    fn tile_cache_tile_byte_size_is_buffer_plus_overhead() {
        let key = key(1, 7, Layer::Biome, 0, 1, 3, 4);
        let tile = Tile::new(key, 256, 256, vec![0u8; 256 * 256 * 4]).expect("the length matches");
        assert_eq!(
            tile.byte_size(),
            FIXED_OVERHEAD_BYTES + 262_144, // 256 * 256 * 4
        );
    }
}
