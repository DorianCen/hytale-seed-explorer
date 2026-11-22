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
//! - **A missing tile has a stand-in, in both zoom directions.**
//!   [`covering_placeholder`] covers a missing *fine* tile with the
//!   nearest cached *coarser* tile, cropped to the missing tile's world
//!   rect — zoom-in continuity, where the wider view's pixels are
//!   reused instead of the screen blanking (the plan's lower-detail
//!   placeholder, §5.1). [`pooled_placeholder`] goes the other way and
//!   derives a missing *coarse* tile by box-averaging the cached
//!   *finer* tiles that cover it — zoom-out continuity, where detail
//!   already generated is pooled in an instant instead of waiting for
//!   coarse tiles to render. Both are derived stand-ins: they are never
//!   cached under the key they stand in for, and the real tile replaces
//!   them when it arrives.
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

use crate::coords::{TILE_SIZE, TileCoord, ZoomLevel, tile_rect};
use crate::render::Layer;
use crate::source::SummaryFidelity;

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
    /// The summary fidelity this tile was rendered at (T29). A cached
    /// tile satisfies a pass only when its provenance is at least the
    /// pass's fidelity ([`SummaryFidelity::satisfies`]); placeholders are
    /// deliberately fidelity-blind. Every construction path defaults to
    /// [`SummaryFidelity::Full`]; renderers set the pass's value.
    pub fidelity: SummaryFidelity,
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
            fidelity: SummaryFidelity::Full,
            created: Instant::now(),
        })
    }

    /// Sets the summary fidelity this tile was rendered at. Builders and
    /// tests default to [`SummaryFidelity::Full`]; the tile skeleton sets
    /// the requesting pass's value.
    #[must_use]
    pub fn with_fidelity(mut self, fidelity: SummaryFidelity) -> Self {
        self.fidelity = fidelity;
        self
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
    /// Keys are content-addressed within one fidelity, so re-inserting an
    /// existing key normally refreshes identical pixels; the T29
    /// provenance upgrade (surface-only → full) replaces the pixels
    /// instead. Either way subscribers are notified (moka's "replaced"
    /// semantics, so the bridge can drop the old upload) but it does not
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

    /// Whether the cached tile at `key` is at least as authoritative as
    /// `fidelity` — the scheduler's fidelity-aware cache-hit test (T29).
    ///
    /// A weaker cached tile (surface-only where a full pass asks) is a
    /// miss: the pass must re-render and overwrite the entry. Without
    /// recency or hit/miss side effects, like [`TileCache::contains`].
    #[must_use]
    pub fn contains_serving(&self, key: &TileKey, fidelity: SummaryFidelity) -> bool {
        lock(&self.inner)
            .entries
            .get(key)
            .is_some_and(|entry| entry.tile.fidelity.satisfies(fidelity))
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
// PlaceholderTile
// ---------------------------------------------------------------------------

/// A cached, coarser tile that can stand in for a missing one, plus the
/// texture crop that paints exactly the missing tile's world rect.
#[derive(Debug, Clone)]
pub struct PlaceholderTile {
    /// The cached tile to paint: the same subject (identity, seed,
    /// layer, mode) at a coarser zoom.
    pub tile: Arc<Tile>,
    /// UV crop `(u0, v0, u1, v1)` within the placeholder's texture: the
    /// fraction of the parent that the missing tile occupies, measured
    /// from the image's top-left (north-west) corner — the orientation
    /// contract tiles already follow (tile row 0 = north = image top).
    pub uv: [f32; 4],
}

/// Finds the nearest cached coarser tile covering `key`'s world rect —
/// the plan's lower-detail placeholder (§5.1).
///
/// Searches the key's own step doubled, then quadrupled, … up to
/// [`ZoomLevel::MAX`]'s step, preferring the closest (least blurry)
/// level; the first cached hit wins. The crop is exact because both
/// tiles are power-of-two aligned: the child's block rect is a whole
/// fraction of the parent's, so the UV values are exact f32s in
/// `[0, 1]`.
///
/// Uses [`TileCache::get`] (not `contains`) so a tile actually used for
/// painting refreshes its LRU recency and counts as a hit.
///
/// **Fidelity-blind by design (T29):** any cached tile is a valid
/// stand-in for a missing one, whatever its provenance. A surface-only
/// parent is a placeholder for a full child, never an authoritative
/// answer; the scheduler's provenance check governs real hits, not this
/// helper.
///
/// Returns [`None`] when no coarser tile of the same subject is cached,
/// when the key is already at the coarsest level, or when either rect
/// falls outside the representable block space (the caller paints its
/// loading blank instead).
#[must_use]
pub fn covering_placeholder(cache: &TileCache, key: &TileKey) -> Option<PlaceholderTile> {
    // The child rect is the same for every candidate level; when it does
    // not fit the i32 block space, no parent can contain it either (a
    // parent's bounds enclose the child's), so the level scan would find
    // nothing and None is final here.
    let child_rect = tile_rect(
        TileCoord::new(i64::from(key.tx), i64::from(key.ty)),
        key.zoom.step(),
    )?;
    let base_step = u64::from(key.zoom.step());
    let max_step = u64::from(ZoomLevel::MAX.step());
    let mut factor: u64 = 2;
    while base_step * factor <= max_step {
        let parent_step = u32::try_from(base_step * factor)
            .expect("the product is bounded by ZoomLevel::MAX.step(), which fits u32");
        let Some(parent_zoom) = ZoomLevel::from_step(parent_step) else {
            // A power-of-two step times a power-of-two factor is a power
            // of two, so this is unreachable; stopping (rather than
            // panicking) keeps the helper total if ZoomLevel's step
            // contract ever changes.
            break;
        };
        // Floor division, not Rust's truncating `/`: -1 at factor 2 must
        // land in the parent at -1, not 0.
        let divisor =
            i64::try_from(factor).expect("the factor doubles from 2 and stops at MAX.step()");
        let tx = i32::try_from(i64::from(key.tx).div_euclid(divisor))
            .expect("floor division by a factor >= 2 cannot grow a tile index");
        let ty = i32::try_from(i64::from(key.ty).div_euclid(divisor))
            .expect("floor division by a factor >= 2 cannot grow a tile index");
        let parent_key = TileKey {
            zoom: parent_zoom,
            tx,
            ty,
            ..*key
        };
        factor *= 2;
        let Some(tile) = cache.get(&parent_key) else {
            continue;
        };
        // None ends the search rather than skipping the level: every
        // coarser parent encloses this one, so it cannot fit if this one
        // does not.
        let parent_rect = tile_rect(TileCoord::new(i64::from(tx), i64::from(ty)), parent_step)?;
        // Blocks per parent side, in i64 so the UV numerators can never
        // wrap; the value is at most 2^28.
        let side = i64::from(TILE_SIZE) * i64::from(parent_step);
        let u0 = (i64::from(child_rect.min_x) - i64::from(parent_rect.min_x)) as f64 / side as f64;
        let u1 =
            (i64::from(child_rect.max_x) + 1 - i64::from(parent_rect.min_x)) as f64 / side as f64;
        let v0 = (i64::from(child_rect.min_z) - i64::from(parent_rect.min_z)) as f64 / side as f64;
        let v1 =
            (i64::from(child_rect.max_z) + 1 - i64::from(parent_rect.min_z)) as f64 / side as f64;
        return Some(PlaceholderTile {
            tile,
            uv: [u0 as f32, v0 as f32, u1 as f32, v1 as f32],
        });
    }
    None
}

// ---------------------------------------------------------------------------
// PooledTile
// ---------------------------------------------------------------------------

/// The deepest pooling factor [`pooled_placeholder`] uses: three levels
/// finer than the missing tile.
///
/// Pooling one coarse tile reads `width * height * factor^2` fine
/// samples — 65 536 · `factor^2` at [`TILE_SIZE`] — and every extra level
/// back quadruples that (factor 16 is ~16.8 M samples per tile). The
/// zoom-out path asks for the level just left (factor 2) and a two-level
/// jump for factor 4; past factor 8 the pixels are already a blur the
/// pending treatment conveys about as well, so the scan stops rather than
/// walking all the way to [`ZoomLevel::MIN`].
pub const MAX_POOL_FACTOR: u32 = 8;

/// A coarse tile derived by box-averaging the cached finer tiles that
/// cover it — the zoom-out complement of [`PlaceholderTile`].
///
/// The pixels are *derived*, not generated: callers own the buffer and
/// must not insert it into the [`TileCache`] under [`PooledTile::key`]
/// (the real tile replaces it), which is also why this is not a [`Tile`].
#[derive(Debug, Clone)]
pub struct PooledTile {
    /// The key these pixels answer for — the requested coarse tile.
    pub key: TileKey,
    /// Pixel width. A pyramid keeps one pixel grid per tile ([`TILE_SIZE`]
    /// square in production), so this is the fine children's width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    /// Row-major RGBA8, north at row 0 — the [`Tile`] orientation.
    ///
    /// **No-data convention:** a coarse pixel with no cached fine sample
    /// is `[0, 0, 0, 0]`, fully transparent. Transparent means "nothing
    /// known here"; the app paints its pending treatment there, while
    /// pooled pixels keep their own alpha and paint untinted like any
    /// other tile. (A fine tile that was itself fully transparent pools
    /// to the same value, so the two are indistinguishable by design.)
    pub rgba: Vec<u8>,
    /// The zoom level the samples came from, always finer than
    /// [`PooledTile::key`]'s. The pooling factor is
    /// `key.zoom.step() / source_zoom.step()`.
    pub source_zoom: ZoomLevel,
}

/// Derives a coarse tile by box-averaging the cached finer tiles covering
/// it — instant content on zoom-out, where [`covering_placeholder`] has
/// nothing coarser to offer.
///
/// Scans finer levels nearest first: the key's step halved, then halved
/// again, … up to [`MAX_POOL_FACTOR`]. The first level with at least one
/// covering child cached is pooled and returned; deeper levels are never
/// consulted even when the nearest level is only partially cached. The
/// closest finer level dominates the result — a deeper level is blurrier
/// *and* costs `factor^2` times the reads per pixel, so pooling more of
/// it buys nothing.
///
/// The children of a coarse tile at factor `f` are the `f × f` aligned
/// fine tiles `coarse * f + 0..f` per axis; `div_euclid` semantics put
/// negative indices in the right parent too (coarse `-1` owns children
/// `-2` and `-1` at factor 2). Each coarse pixel covers an `f × f` block
/// of fine pixels: the coarse tile spans `f` children, hence `width * f`
/// fine pixels per axis, so coarse pixel `(px, py)` samples fine pixels
/// `px * f ..= px * f + f - 1` of that combined grid; dividing by
/// `width`/`height` picks the child and the remainder the pixel inside
/// it. The pooled value is the truncated average per channel, alpha
/// included; a coarse pixel whose samples were all missing stays
/// transparent (see [`PooledTile::rgba`]).
///
/// Every child read goes through [`TileCache::get`], so children actually
/// pooled count as hits and refresh their LRU recency.
///
/// **Fidelity-blind by design (T29):** a surface-only child is a valid
/// stand-in for a missing coarse tile, just as a surface-only parent is
/// for [`covering_placeholder`]. Stand-ins never satisfy a real cache hit;
/// the scheduler's provenance check owns that decision.
///
/// Returns [`None`] when the key is at [`ZoomLevel::MIN`], when no finer
/// child of the same subject (identity, seed, layer, mode) is cached, or
/// when the key's block rect falls outside the representable `i32` space
/// (such a tile cannot exist).
#[must_use]
pub fn pooled_placeholder(cache: &TileCache, key: &TileKey) -> Option<PooledTile> {
    // Same representability rule as the coarser-cover direction: a tile
    // whose block rect does not fit i32 cannot be painted or generated.
    tile_rect(
        TileCoord::new(i64::from(key.tx), i64::from(key.ty)),
        key.zoom.step(),
    )?;

    let coarse_step = key.zoom.step();
    let mut factor: u32 = 2;
    while factor <= MAX_POOL_FACTOR {
        // Halving the step stays on the level ladder while it is still at
        // least one block per sample; below that (the finest level) the
        // integer quotient is zero and `from_step` ends the scan.
        let Some(fine_zoom) = ZoomLevel::from_step(coarse_step / factor) else {
            break;
        };
        if let Some(pooled) = pool_from_level(cache, key, fine_zoom, factor) {
            return Some(pooled);
        }
        factor *= 2;
    }
    None
}

/// Pools `key` from the `factor × factor` children at `fine_zoom`, or
/// [`None`] when none of them is cached.
fn pool_from_level(
    cache: &TileCache,
    key: &TileKey,
    fine_zoom: ZoomLevel,
    factor: u32,
) -> Option<PooledTile> {
    let factor_i64 = i64::from(factor);
    // `div_euclid(child, factor) == coarse` for every child in
    // `[coarse * factor, coarse * factor + factor)`, so this base is the
    // floor-aligned first child, negatives included.
    let base_x = i64::from(key.tx) * factor_i64;
    let base_y = i64::from(key.ty) * factor_i64;
    let mut children: Vec<Option<Arc<Tile>>> = Vec::with_capacity((factor * factor) as usize);
    for cy in 0..factor {
        for cx in 0..factor {
            // Narrowing is safe: the coarse block rect fit i32 above and
            // every child index describes a block inside it.
            let tx = i32::try_from(base_x + i64::from(cx))
                .expect("the coarse rect fits i32, and its children lie inside it");
            let ty = i32::try_from(base_y + i64::from(cy))
                .expect("the coarse rect fits i32, and its children lie inside it");
            children.push(cache.get(&TileKey {
                zoom: fine_zoom,
                tx,
                ty,
                ..*key
            }));
        }
    }

    // Any present child fixes the output grid; without one this level has
    // nothing to pool and the caller may try a deeper one.
    let (width, height) = children
        .iter()
        .flatten()
        .next()
        .map(|tile| (tile.width, tile.height))?;
    let width_usize = width as usize;
    let height_usize = height as usize;
    let factor_usize = factor as usize;
    // Zeroes are the no-data value: uncovered pixels are left untouched.
    let mut rgba = vec![0u8; width_usize * height_usize * 4];
    for py in 0..height {
        for px in 0..width {
            let mut sum = [0u32; 4];
            let mut samples = 0u32;
            for sy in 0..factor {
                let gy = u64::from(py) * u64::from(factor) + u64::from(sy);
                let cy = (gy / u64::from(height)) as usize;
                let ly = (gy % u64::from(height)) as usize;
                for sx in 0..factor {
                    let gx = u64::from(px) * u64::from(factor) + u64::from(sx);
                    let cx = (gx / u64::from(width)) as usize;
                    let lx = (gx % u64::from(width)) as usize;
                    let Some(child) = children[cy * factor_usize + cx].as_ref() else {
                        continue;
                    };
                    // Production tiles share one grid; a foreign one would
                    // be sampled with the wrong stride, so it is no-data.
                    if child.width != width || child.height != height {
                        continue;
                    }
                    let index = (ly * width_usize + lx) * 4;
                    for (channel, total) in sum.iter_mut().enumerate() {
                        *total += u32::from(child.rgba[index + channel]);
                    }
                    samples += 1;
                }
            }
            if samples == 0 {
                continue;
            }
            let out = (py as usize * width_usize + px as usize) * 4;
            for (channel, total) in sum.iter().enumerate() {
                rgba[out + channel] = u8::try_from(total / samples)
                    .expect("the average of u8 samples cannot exceed u8::MAX");
            }
        }
    }

    Some(PooledTile {
        key: *key,
        width,
        height,
        rgba,
        source_zoom: fine_zoom,
    })
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

    // -- Placeholder lookup (T16) -------------------------------------------

    /// A cache that comfortably holds the handful of 2×2 fixtures below
    /// (each weighs 96 + 16 bytes).
    fn placeholder_cache() -> TileCache {
        TileCache::new(1 << 20)
    }

    /// Caches a 2×2 black tile under `key` — the placeholder helpers only
    /// read the key, never the pixels.
    fn cache_tile(cache: &TileCache, key: TileKey) {
        cache_tile_with_fidelity(cache, key, SummaryFidelity::Full);
    }

    /// Caches a 2×2 black tile tagged with `fidelity`.
    fn cache_tile_with_fidelity(cache: &TileCache, key: TileKey, fidelity: SummaryFidelity) {
        let tile = Tile::new(key, 2, 2, vec![0u8; 16])
            .expect("2x2 RGBA is exactly 16 bytes")
            .with_fidelity(fidelity);
        cache.insert(Arc::new(tile));
    }

    #[test]
    fn tile_placeholder_prefers_the_closest_coarser_level() {
        let cache = placeholder_cache();
        let child = key(1, 7, Layer::Biome, 0, 0, 1, 1);
        // Cache the far level first so insertion order cannot be what
        // makes the near level win.
        let grandparent = key(1, 7, Layer::Biome, 0, 2, 0, 0);
        let parent = key(1, 7, Layer::Biome, 0, 1, 0, 0);
        cache_tile(&cache, grandparent);
        cache_tile(&cache, parent);

        let placeholder = covering_placeholder(&cache, &child).expect("a parent level is cached");
        assert_eq!(
            placeholder.tile.key, parent,
            "the step-2 parent is less blurry than the step-4 grandparent"
        );
        assert_eq!(
            placeholder.tile.key.zoom,
            ZoomLevel::new(1).expect("level 1 is in range")
        );
    }

    /// T29: stand-ins are fidelity-blind by design — a surface-only tile is
    /// a valid placeholder in both zoom directions; only the scheduler's
    /// real cache-hit check enforces provenance.
    #[test]
    fn fidelity_placeholders_accept_weaker_tiles_as_stand_ins() {
        let parent = key(1, 7, Layer::Biome, 0, 1, 0, 0);
        let child = key(1, 7, Layer::Biome, 0, 0, 0, 0);
        let covering_cache = placeholder_cache();
        cache_tile_with_fidelity(&covering_cache, parent, SummaryFidelity::SurfaceOnly);
        let covering =
            covering_placeholder(&covering_cache, &child).expect("a weaker parent stands in");
        assert_eq!(covering.tile.key, parent);
        assert_eq!(covering.tile.fidelity, SummaryFidelity::SurfaceOnly);

        let coarse = key(1, 7, Layer::Biome, 0, 1, 0, 0);
        let pooled_cache = placeholder_cache();
        for (tx, ty) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            cache_tile_with_fidelity(
                &pooled_cache,
                key(1, 7, Layer::Biome, 0, 0, tx, ty),
                SummaryFidelity::SurfaceOnly,
            );
        }
        let pooled = pooled_placeholder(&pooled_cache, &coarse)
            .expect("weaker children pool into a coarse stand-in");
        assert_eq!(pooled.key, coarse);
    }

    #[test]
    fn tile_placeholder_crops_exact_uv_fractions() {
        let cache = placeholder_cache();
        cache_tile(&cache, key(1, 7, Layer::Biome, 0, 1, 0, 0));

        // (1,1) is the south-east quadrant of parent (0,0).
        let child = key(1, 7, Layer::Biome, 0, 0, 1, 1);
        let placeholder = covering_placeholder(&cache, &child).expect("the parent is cached");
        assert_eq!(placeholder.uv, [0.5, 0.5, 1.0, 1.0]);

        // (0,0) is the north-west quadrant.
        let child = key(1, 7, Layer::Biome, 0, 0, 0, 0);
        let placeholder = covering_placeholder(&cache, &child).expect("the parent is cached");
        assert_eq!(placeholder.uv, [0.0, 0.0, 0.5, 0.5]);

        // Negative indices floor toward the more negative parent: -1 / 2
        // truncates to 0 but floors to -1.
        cache_tile(&cache, key(1, 7, Layer::Biome, 0, 1, -1, -1));
        let child = key(1, 7, Layer::Biome, 0, 0, -1, -1);
        let placeholder = covering_placeholder(&cache, &child).expect("the parent is cached");
        assert_eq!(
            (placeholder.tile.key.tx, placeholder.tile.key.ty),
            (-1, -1),
            "the parent index uses floor semantics"
        );
        assert_eq!(placeholder.uv, [0.5, 0.5, 1.0, 1.0]);
    }

    #[test]
    fn tile_placeholder_skips_to_the_next_level_when_one_is_missing() {
        let cache = placeholder_cache();
        let child = key(1, 7, Layer::Biome, 0, 0, 1, 0);
        // Only the level-2 (step 4) grandparent exists; level 1 misses.
        cache_tile(&cache, key(1, 7, Layer::Biome, 0, 2, 0, 0));

        let placeholder =
            covering_placeholder(&cache, &child).expect("the grandparent level is cached");
        assert_eq!(
            placeholder.tile.key.zoom,
            ZoomLevel::new(2).expect("level 2 is in range")
        );
        assert_eq!(placeholder.uv, [0.25, 0.0, 0.5, 0.25]);
    }

    #[test]
    fn tile_placeholder_returns_none_without_a_matching_subject() {
        let cache = placeholder_cache();
        let child = key(1, 7, Layer::Biome, 0, 0, 1, 1);
        assert!(
            covering_placeholder(&cache, &child).is_none(),
            "an empty cache has nothing to stand in"
        );

        cache_tile(&cache, child);
        assert!(
            covering_placeholder(&cache, &child).is_none(),
            "a tile is not its own placeholder"
        );

        cache_tile(&cache, key(1, 8, Layer::Biome, 0, 1, 0, 0));
        assert!(
            covering_placeholder(&cache, &child).is_none(),
            "another seed's parent paints different terrain"
        );

        cache_tile(&cache, key(1, 7, Layer::Height, 0, 1, 0, 0));
        assert!(
            covering_placeholder(&cache, &child).is_none(),
            "another layer's parent paints different data"
        );
    }

    #[test]
    fn tile_placeholder_returns_none_at_the_coarsest_level() {
        let cache = placeholder_cache();
        let child = key(1, 7, Layer::Biome, 0, ZoomLevel::MAX.0, 0, 0);
        assert!(
            covering_placeholder(&cache, &child).is_none(),
            "no coarser level exists to cache"
        );
    }

    #[test]
    fn tile_placeholder_uv_stays_inside_the_unit_square() {
        let cache = placeholder_cache();
        for level in 0..=2u8 {
            for &(tx, ty) in &[(-7, 5), (-1, -1), (0, 0), (1, 1), (9, -3)] {
                let child = key(1, 7, Layer::Biome, 0, level, tx, ty);
                cache_tile(
                    &cache,
                    key(
                        1,
                        7,
                        Layer::Biome,
                        0,
                        level + 1,
                        tx.div_euclid(2),
                        ty.div_euclid(2),
                    ),
                );

                let placeholder =
                    covering_placeholder(&cache, &child).expect("the immediate parent is cached");
                let [u0, v0, u1, v1] = placeholder.uv;
                assert!(
                    0.0 <= u0 && u0 < u1 && u1 <= 1.0,
                    "u crop must be a non-empty fraction of the parent: {placeholder:?}"
                );
                assert!(
                    0.0 <= v0 && v0 < v1 && v1 <= 1.0,
                    "v crop must be a non-empty fraction of the parent: {placeholder:?}"
                );
            }
        }
    }

    // -- Pooled placeholder (zoom-out stand-in) -----------------------------

    /// Caches a `width × height` tile under `key`, filling each pixel from
    /// `pixel(x, y)` — lets a test lay out an exact sample grid.
    fn cache_pattern(
        cache: &TileCache,
        key: TileKey,
        width: u32,
        height: u32,
        pixel: impl Fn(u32, u32) -> [u8; 4],
    ) {
        let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height {
            for x in 0..width {
                rgba.extend_from_slice(&pixel(x, y));
            }
        }
        let tile = Tile::new(key, width, height, rgba).expect("the buffer matches the pixel grid");
        cache.insert(Arc::new(tile));
    }

    /// Caches a solid-colour `size × size` tile under `key`.
    fn cache_solid(cache: &TileCache, key: TileKey, color: [u8; 4], size: u32) {
        cache_pattern(cache, key, size, size, |_, _| color);
    }

    #[test]
    fn pooling_placeholder_averages_four_children_exactly() {
        let cache = placeholder_cache();
        // Coarse (0,0) at step 2 owns the four step-1 children below it.
        let coarse = key(1, 7, Layer::Biome, 0, 1, 0, 0);
        let colors = [
            [10, 20, 30, 255],
            [40, 50, 60, 255],
            [70, 80, 90, 127],
            [100, 110, 120, 0],
        ];
        for (index, color) in colors.iter().enumerate() {
            let (cx, cy) = (index as i32 % 2, index as i32 / 2);
            cache_solid(&cache, key(1, 7, Layer::Biome, 0, 0, cx, cy), *color, 2);
        }

        let pooled = pooled_placeholder(&cache, &coarse).expect("all four children are cached");
        assert_eq!(pooled.key, coarse);
        assert_eq!((pooled.width, pooled.height), (2, 2));
        assert_eq!(
            pooled.source_zoom,
            ZoomLevel::new(0).expect("level 0 is in range")
        );
        assert_eq!(pooled.rgba.len(), 2 * 2 * 4);
        for (index, color) in colors.iter().enumerate() {
            let (x, y) = (index % 2, index / 2);
            let offset = (y * 2 + x) * 4;
            assert_eq!(
                &pooled.rgba[offset..offset + 4],
                color,
                "quadrant ({x}, {y}) averages its child's uniform pixels"
            );
        }
    }

    #[test]
    fn pooling_placeholder_prefers_the_closest_finer_level() {
        let cache = placeholder_cache();
        // Coarse level 2 (step 4) at (0,0): its level-1 children (factor
        // 2) and level-0 grandchildren (factor 4) both cover it.
        let coarse = key(1, 7, Layer::Biome, 0, 2, 0, 0);
        for cy in 0..4 {
            for cx in 0..4 {
                cache_solid(
                    &cache,
                    key(1, 7, Layer::Biome, 0, 0, cx, cy),
                    [1, 2, 3, 255],
                    2,
                );
            }
        }
        for cy in 0..2 {
            for cx in 0..2 {
                cache_solid(
                    &cache,
                    key(1, 7, Layer::Biome, 0, 1, cx, cy),
                    [9, 8, 7, 255],
                    2,
                );
            }
        }

        let pooled = pooled_placeholder(&cache, &coarse).expect("both levels are cached");
        assert_eq!(
            pooled.source_zoom,
            ZoomLevel::new(1).expect("level 1 is in range"),
            "the step-2 children dominate; step-1 grandchildren only add blur"
        );
        assert_eq!(&pooled.rgba[0..4], &[9, 8, 7, 255]);

        // Without the level-1 children the level-0 grandchildren are the
        // closest level left, so they are pooled at factor 4.
        let cache = placeholder_cache();
        for cy in 0..4 {
            for cx in 0..4 {
                cache_solid(
                    &cache,
                    key(1, 7, Layer::Biome, 0, 0, cx, cy),
                    [1, 2, 3, 255],
                    2,
                );
            }
        }
        let pooled = pooled_placeholder(&cache, &coarse).expect("the grandchildren are cached");
        assert_eq!(
            pooled.source_zoom,
            ZoomLevel::new(0).expect("level 0 is in range")
        );
        assert_eq!(&pooled.rgba[0..4], &[1, 2, 3, 255]);
    }

    #[test]
    fn pooling_placeholder_marks_missing_children_as_no_data() {
        let cache = placeholder_cache();
        let coarse = key(1, 7, Layer::Biome, 0, 1, 0, 0);
        cache_solid(
            &cache,
            key(1, 7, Layer::Biome, 0, 0, 0, 0),
            [12, 34, 56, 255],
            2,
        );

        let pooled = pooled_placeholder(&cache, &coarse).expect("one child suffices");
        assert_eq!(
            &pooled.rgba[0..4],
            &[12, 34, 56, 255],
            "the cached quadrant pools"
        );
        for offset in [4, 8, 12] {
            assert_eq!(
                &pooled.rgba[offset..offset + 4],
                &[0, 0, 0, 0],
                "an uncovered quadrant is transparent: no data"
            );
        }
    }

    #[test]
    fn pooling_placeholder_handles_negative_tile_indices() {
        let cache = placeholder_cache();
        // Coarse (-1,-1) at step 2 owns the step-1 children (-2,-2),
        // (-1,-2), (-2,-1), (-1,-1): floor semantics, not truncation.
        let coarse = key(1, 7, Layer::Biome, 0, 1, -1, -1);
        for (cx, cy) in [(-2, -2), (-1, -2), (-2, -1), (-1, -1)] {
            cache_solid(
                &cache,
                key(1, 7, Layer::Biome, 0, 0, cx, cy),
                [7, 7, 7, 255],
                2,
            );
        }
        // A positive-index child must not contribute to a negative key.
        cache_solid(
            &cache,
            key(1, 7, Layer::Biome, 0, 0, 0, 0),
            [200, 0, 0, 255],
            2,
        );

        let pooled = pooled_placeholder(&cache, &coarse).expect("the negative children are cached");
        assert_eq!(pooled.key, coarse);
        assert!(
            pooled.rgba.chunks(4).all(|pixel| pixel == [7, 7, 7, 255]),
            "every quadrant pools from children -2..=-1: {pooled:?}"
        );
    }

    #[test]
    fn pooling_placeholder_without_children_returns_none() {
        let cache = placeholder_cache();
        let coarse = key(1, 7, Layer::Biome, 0, 1, 0, 0);
        assert!(
            pooled_placeholder(&cache, &coarse).is_none(),
            "an empty cache has nothing to pool"
        );

        // Another subject's children are different terrain: identity,
        // seed, layer and mode all have to agree.
        cache_solid(
            &cache,
            key(1, 8, Layer::Biome, 0, 0, 0, 0),
            [1, 1, 1, 255],
            2,
        );
        cache_solid(
            &cache,
            key(1, 7, Layer::Height, 0, 0, 1, 0),
            [1, 1, 1, 255],
            2,
        );
        cache_solid(
            &cache,
            key(1, 7, Layer::CaveSlice, 64, 0, 0, 1),
            [1, 1, 1, 255],
            2,
        );
        cache_solid(
            &cache,
            key(2, 7, Layer::Biome, 0, 0, 1, 1),
            [1, 1, 1, 255],
            2,
        );
        assert!(
            pooled_placeholder(&cache, &coarse).is_none(),
            "other subjects never pool in"
        );

        // Coarser tiles belong to the covering direction, and a tile is
        // not its own child.
        cache_solid(
            &cache,
            key(1, 7, Layer::Biome, 0, 2, 0, 0),
            [1, 1, 1, 255],
            2,
        );
        cache_solid(&cache, coarse, [1, 1, 1, 255], 2);
        assert!(pooled_placeholder(&cache, &coarse).is_none());
    }

    #[test]
    fn pooling_placeholder_is_none_at_the_finest_level() {
        let cache = placeholder_cache();
        let coarse = key(1, 7, Layer::Biome, 0, ZoomLevel::MIN.0, 0, 0);
        cache_solid(&cache, coarse, [1, 1, 1, 255], 2);
        assert!(
            pooled_placeholder(&cache, &coarse).is_none(),
            "no finer level exists to pool from"
        );
    }

    #[test]
    fn pooling_placeholder_prefers_a_partial_closer_level_over_a_complete_deeper_one() {
        let cache = placeholder_cache();
        let coarse = key(1, 7, Layer::Biome, 0, 2, 0, 0);
        // One level-1 child (step 2), the complete level-0 set (step 4).
        cache_solid(
            &cache,
            key(1, 7, Layer::Biome, 0, 1, 1, 1),
            [3, 4, 5, 255],
            2,
        );
        for cy in 0..4 {
            for cx in 0..4 {
                cache_solid(
                    &cache,
                    key(1, 7, Layer::Biome, 0, 0, cx, cy),
                    [9, 9, 9, 255],
                    2,
                );
            }
        }

        let pooled = pooled_placeholder(&cache, &coarse).expect("the level-1 child suffices");
        assert_eq!(
            pooled.source_zoom,
            ZoomLevel::new(1).expect("level 1 is in range")
        );
        // Child (1,1) of the coarse tile is its south-east quadrant.
        assert_eq!(&pooled.rgba[12..16], &[3, 4, 5, 255]);
        assert_eq!(
            &pooled.rgba[0..4],
            &[0, 0, 0, 0],
            "the rest is pending, not filled in from the deeper level"
        );
    }

    #[test]
    fn pooling_placeholder_handles_non_square_pixel_grids() {
        let cache = placeholder_cache();
        let coarse = key(1, 7, Layer::Biome, 0, 1, 0, 0);
        // 4 x 2 fine pixels per child; at factor 2 the coarse grid is also
        // 4 x 2 and a coarse pixel averages a 2 x 2 block of the fine grid,
        // whose row stride is the *width* (4), not the factor.
        for cy in 0..2i32 {
            for cx in 0..2i32 {
                let base = 50 * (cy * 2 + cx) as u8;
                cache_pattern(
                    &cache,
                    key(1, 7, Layer::Biome, 0, 0, cx, cy),
                    4,
                    2,
                    |x, y| [base + (y * 4 + x) as u8, 0, 0, 255],
                );
            }
        }

        let pooled = pooled_placeholder(&cache, &coarse).expect("all children are cached");
        assert_eq!((pooled.width, pooled.height), (4, 2));
        // Output pixel (x, y) starts at this byte; the row stride is 4.
        let at = |x: usize, y: usize| (y * 4 + x) * 4;
        // (0,0) pools child (0,0) pixels 0, 1, 4, 5 -> 2 (truncated).
        assert_eq!(&pooled.rgba[at(0, 0)..at(0, 0) + 4], &[2, 0, 0, 255]);
        // (1,1) pools child (0,1) (base 100) pixels 2, 3, 6, 7 -> 104.
        assert_eq!(&pooled.rgba[at(1, 1)..at(1, 1) + 4], &[104, 0, 0, 255]);
        // (2,0) opens child (1,0) (base 50): 50, 51, 54, 55 -> 52.
        assert_eq!(&pooled.rgba[at(2, 0)..at(2, 0) + 4], &[52, 0, 0, 255]);
        // (3,1) pools child (1,1) (base 150): 152, 153, 156, 157 -> 154.
        assert_eq!(&pooled.rgba[at(3, 1)..at(3, 1) + 4], &[154, 0, 0, 255]);
    }

    #[test]
    fn pooling_placeholder_ignores_children_with_other_pixel_grids() {
        let cache = placeholder_cache();
        let coarse = key(1, 7, Layer::Biome, 0, 1, 0, 0);
        // The first present child (row-major) fixes the grid; a child with
        // a foreign grid would be sampled with the wrong stride, so it is
        // treated as no data.
        cache_solid(
            &cache,
            key(1, 7, Layer::Biome, 0, 0, 0, 0),
            [5, 5, 5, 255],
            2,
        );
        cache_solid(
            &cache,
            key(1, 7, Layer::Biome, 0, 0, 1, 1),
            [9, 9, 9, 255],
            4,
        );

        let pooled = pooled_placeholder(&cache, &coarse).expect("one child fixes the grid");
        assert_eq!((pooled.width, pooled.height), (2, 2));
        assert_eq!(&pooled.rgba[0..4], &[5, 5, 5, 255]);
        assert_eq!(
            &pooled.rgba[12..16],
            &[0, 0, 0, 0],
            "the 4x4 child is skipped"
        );
    }

    #[test]
    fn pooling_placeholder_stops_at_the_pool_factor_cap() {
        let cache = placeholder_cache();
        // Factor 16 (four levels finer) is past MAX_POOL_FACTOR: the read
        // cost would dwarf the blur it adds.
        let too_deep = key(1, 7, Layer::Biome, 0, 4, 0, 0);
        for cy in 0..16 {
            for cx in 0..16 {
                cache_solid(
                    &cache,
                    key(1, 7, Layer::Biome, 0, 0, cx, cy),
                    [1, 1, 1, 255],
                    1,
                );
            }
        }
        assert!(
            pooled_placeholder(&cache, &too_deep).is_none(),
            "factor 16 is past the cap; the app shows its pending treatment"
        );
        // Factor 8 (three levels finer) is still inside the cap.
        let within_cap = key(1, 7, Layer::Biome, 0, 3, 0, 0);
        let pooled = pooled_placeholder(&cache, &within_cap).expect("factor 8 is within the cap");
        assert_eq!(
            pooled.source_zoom,
            ZoomLevel::new(0).expect("level 0 is in range")
        );
    }
}
