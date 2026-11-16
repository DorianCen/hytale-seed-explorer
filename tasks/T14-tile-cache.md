# T14 — Tile keys, cache and eviction

**Status:** proposed
**Milestone:** 3 — Tiling and navigation
**Depends on:** T07, T10
**Blocks:** T15, T16
**Plan refs:** [§5.1](../hytale-seed-map-plan.md#51-tiles-and-zoom), [§4](../hytale-seed-map-plan.md#4-tech-choices), [§9](../hytale-seed-map-plan.md#9-performance-correctness-and-testing)
**Agent fit:** high

## Goal

Implement the tile identity and bounded cache that makes pan/zoom fast and
correct. A tile must never be shown for the wrong seed, generator version,
layer, resolution or position; eviction must free both CPU buffers and the
matching GPU textures.

## Context

The plan's key is:

```text
(generator_version, seed, layer, mode, zoom, tx, ty)
```

In this implementation, `mode` can be folded into the layer (for example
`LayerId::Surface` vs `LayerId::Biome`), but keep the field if a layer has modes
such as `Solid`/`Density`. `zoom` is the discrete `ZoomLevel` from T10; the
effective `step` is derived from it but should still be stored so two zooms
that map to the same step do not duplicate pixels unintentionally (or,
conversely, so they can share a tile deliberately).

Tile size is 256×256 pixels, RGBA8 = 262,144 bytes per tile before overhead.
The plan's cache budget example is 512 MB.

## Scope

### In

- `map-core/src/tiles.rs`:
  - `LayerId` enum (`Biome`, `Height`, `Relief`, `Surface`, `Fluid`, `CaveSlice`,
    `CaveDensity`, `Zone`, ...) and optional mode;
  - `TileKey` with `GeneratorIdentity`/seed/layer/zoom/tx/ty;
  - `Tile` with 256×256 RGBA bytes and a generation/priority stamp;
  - `TileCache` backed by `moka::sync::Cache` or a small weighted LRU;
  - byte accounting and eviction notifications;
  - invalidation by seed/version/layer.
- `map-app/src/textures.rs` or a cache bridge that drops `TextureHandle`s when
  the corresponding tile is evicted.
- Tests for key equality, negative coordinates, eviction and invalidation.

### Out

- Scheduling/priority/cancellation (T15).
- Painting/placeholders (T16).
- The actual layer renderers (T13/T17/T21/T28).

## Design

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TileKey {
    pub identity_hash: u64, // stable hash of generator version + assets + palette
    pub seed: i32,          // generator-visible seed
    pub layer: LayerId,
    pub mode: u16,          // layer-specific (e.g. cave Y bucket); 0 if unused
    pub zoom: u8,
    pub tx: i32,
    pub ty: i32,
}

pub struct Tile {
    pub key: TileKey,
    pub width: u32,  // 256
    pub height: u32, // 256
    pub rgba: Arc<[u8]>,
    pub created: Instant,
}
```

If `Instant`/serialization is an issue, keep `created` outside the Eq/Hash
implementation (manual impl or a wrapper).

`TileCache` API:

```rust
impl TileCache {
    pub fn new(byte_budget: u64) -> Self;
    pub fn get(&self, key: &TileKey) -> Option<Arc<Tile>>;
    pub fn insert(&self, tile: Arc<Tile>);
    pub fn contains(&self, key: &TileKey) -> bool;
    pub fn remove(&self, key: &TileKey);
    pub fn invalidate_seed(&self, seed: i32);
    pub fn invalidate_version(&self, identity_hash: u64);
    pub fn clear(&self);
    pub fn stats(&self) -> CacheStats; // entries, bytes, hits, misses, evictions
    pub fn subscribe_evictions(&self) -> Receiver<TileKey>;
}
```

`moka` supports a weigher and an eviction listener; use them. If the listener
cannot send on a channel after the cache is dropped, ignore the send error.

The app texture cache should be keyed by the same `TileKey` and periodically
drop handles whose key is no longer in `TileCache`; the eviction channel makes
this immediate.

## Work

1. Implement `LayerId` and `TileKey`. Use a manual `Hash`/`Eq` if `mode` is
   layer-specific. Include the generator identity hash, not just the version
   number, so an asset/palette change invalidates tiles.
2. Implement `Tile`:
   - validate `rgba.len() == width * height * 4` in `new`;
   - provide `pixel(x, y) -> [u8; 4]` for tests;
   - `byte_size() = rgba.len() + fixed overhead`.
3. Implement `TileCache` with a byte budget:
   - `moka::sync::Cache<TileKey, Arc<Tile>>` with `weigher = tile.byte_size()`;
   - `max_capacity = byte_budget`;
   - eviction listener sends `TileKey` to a `crossbeam`/`std` channel;
   - stats counters for hit/miss/eviction.
   If `moka`'s byte weigher semantics are awkward, use a small custom
   `HashMap + VecDeque` LRU; correctness first.
4. Implement invalidation:
   - `invalidate_seed`: iterate keys and remove matching seed (or use a cache
     generation: bump a namespace per source);
   - `invalidate_version`: clear;
   - `clear` also drains the eviction channel.
5. App bridge:
   - `TextureCache { textures: HashMap<TileKey, egui::TextureHandle> }`;
   - on a new tile, upload once and store;
   - on an eviction event, remove the handle (dropping it frees GPU memory);
   - on source change, clear both caches.
6. Add tests:
   - two keys differ when seed/version/layer/zoom/tx/ty differ;
   - negative `tx`/`ty` key equality;
   - inserting enough tiles evicts and reports bytes under budget;
   - eviction channel receives the evicted key;
   - invalidating a seed leaves other seeds intact;
   - a stale tile key is not returned after a version change;
   - `Tile::new` rejects a wrong buffer length.
7. Add a debug stats overlay hook (`CacheStats`) for T33.

## Acceptance

```sh
cargo test -p map-core tile_cache
cargo test -p map-app texture_cache
cargo clippy -p map-core -p map-app --all-targets -- -D warnings
```

- Cache memory stays under the configured byte budget in a stress test.
- Evicted tiles are removed from the app texture cache.
- Seed/version/layer/zoom changes always produce different keys.
- Negative coordinates round-trip through the key.
- No cache lookup can return a tile from a previous source identity.

## Tests

- `map-core/tests/tile_cache.rs` with a synthetic 256×256 tile generator and a
  1 MB budget (four tiles), then stress 100 inserts.
- A `map-app` unit test for the texture-key bridge using a fake texture map
  (no GPU required; use a `HashMap<TileKey, ()>` if `TextureHandle` is hard to
  construct).

## Risks / notes

- `moka`'s eviction listener may fire on a background thread after the app is
  gone; never panic in the listener.
- If the tile cache and texture cache budgets are separate, a texture eviction
  can cause a re-upload even though the CPU tile remains. Document the policy;
  the simplest v1 is to keep them in sync.
- Do not key only by `(tx, ty)`. That is the classic seed-change stale-tile
  bug.
