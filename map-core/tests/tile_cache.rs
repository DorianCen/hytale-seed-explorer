//! T14 acceptance: the bounded tile cache under stress, with a
//! synthetic 256×256 tile generator.
//!
//! Covers the task's acceptance list: budget eviction in LRU order, the
//! eviction channel, seed/version invalidation, negative tile
//! coordinates, and the ~1 MB budget / 100-insert stress run.

use std::sync::Arc;

use map_core::coords::ZoomLevel;
use map_core::render::Layer;
use map_core::tiles::{Tile, TileCache, TileKey};
use map_core::types::GeneratorIdentity;

/// The generator-visible seed the base keys use.
const SEED: i32 = 7;

/// A biome-layer, zoom-0 tile key under identity `hash` at `(tx, ty)`.
fn key_at(identity_hash: u64, seed: i32, tx: i32, ty: i32) -> TileKey {
    TileKey {
        identity_hash,
        generator_seed: seed,
        layer: Layer::Biome,
        mode: 0,
        zoom: ZoomLevel::MIN,
        tx,
        ty,
    }
}

/// A synthetic 256×256 tile: deterministic bytes derived from the key,
/// exact in length, cheap to build a hundred of.
fn synthetic_tile(key: TileKey) -> Arc<Tile> {
    let byte = key.tx.rem_euclid(i32::from(u8::MAX)) as u8;
    Arc::new(Tile::new(key, 256, 256, vec![byte; 256 * 256 * 4]).expect("the length matches"))
}

/// The stable hash of an identity with the given generator version
/// (the same `stable_hash` the worldgen source's keys use).
fn identity_hash(version: u32) -> u64 {
    GeneratorIdentity {
        generator_version: version,
        world_assets: String::from("Default/0.0.0"),
        palette_version: 0,
    }
    .stable_hash()
}

#[test]
fn tile_cache_negative_tile_coordinates_round_trip() {
    // Tile indices can be negative (T10 floor semantics); the key must
    // carry them without loss, and the cache must serve by them.
    let cache = TileCache::new(1024 * 1024);
    let key = key_at(identity_hash(1), SEED, -2, -3);
    cache.insert(synthetic_tile(key));

    // The same hand-built key retrieves the same tile — key equality
    // round-trips the negatives — and its neighbors do not hit.
    let tile = cache.get(&key).expect("the negative-coordinate key hits");
    assert_eq!(tile.key, key);
    assert_eq!(tile.key.tx, -2, "the coordinates survive the round trip");
    assert_eq!(tile.pixel(0, 0), Some([253, 253, 253, 253])); // -2 as u8
    assert!(!cache.contains(&key_at(key.identity_hash, SEED, -2, -2)));
    assert!(!cache.contains(&key_at(key.identity_hash, SEED, 0, 0)));
}

#[test]
fn tile_cache_evicts_least_recently_used_first() {
    // A two-tile budget: touching A keeps it alive when C arrives, so
    // the untouched B is the one evicted.
    let hash = identity_hash(1);
    let (a, b, c) = (
        key_at(hash, SEED, 0, 0),
        key_at(hash, SEED, 1, 0),
        key_at(hash, SEED, 2, 0),
    );
    let per_tile = synthetic_tile(a).byte_size();
    let cache = TileCache::new(2 * per_tile);

    cache.insert(synthetic_tile(a));
    cache.insert(synthetic_tile(b));
    assert_eq!(cache.stats().entries, 2);

    // Touch A, so B becomes the least recently used tile.
    assert!(cache.get(&a).is_some());
    cache.insert(synthetic_tile(c));

    let stats = cache.stats();
    assert!(
        stats.bytes <= stats.byte_budget,
        "the budget invariant holds"
    );
    assert!(cache.contains(&a), "the touched tile survives");
    assert!(!cache.contains(&b), "the untouched tile was evicted");
    assert!(cache.contains(&c), "the newcomer stays");
    assert_eq!(stats.evictions, 1);
}

#[test]
fn tile_cache_stress_hundred_inserts_stay_within_budget() {
    // The task's stress row: a ~1 MB budget (four 256×256 tiles), then
    // 100 inserts — the budget invariant must hold after every one.
    let hash = identity_hash(1);
    let per_tile = synthetic_tile(key_at(hash, SEED, 0, 0)).byte_size();
    let cache = TileCache::new(4 * per_tile);
    // Four tiles plus their overhead: within a kilobyte of the plan's
    // 1 MB example budget.
    assert!(
        (i64::try_from(4 * per_tile).expect("fits i64") - 1024 * 1024).abs() < 1024,
        "the budget approximates the plan's 1 MB example"
    );

    for index in 0..100 {
        cache.insert(synthetic_tile(key_at(hash, SEED, index, 0)));
        let stats = cache.stats();
        assert!(
            stats.bytes <= stats.byte_budget,
            "insert {index} pushed the cache to {} of {} bytes",
            stats.bytes,
            stats.byte_budget,
        );
    }

    let stats = cache.stats();
    assert_eq!(stats.entries, 4, "the budget admits exactly four tiles");
    assert_eq!(
        stats.evictions, 96,
        "every tile past the fourth was evicted"
    );
    // The four most recent inserts are the survivors.
    for index in 0..96 {
        assert!(
            !cache.contains(&key_at(hash, SEED, index, 0)),
            "tile {index} was evicted"
        );
    }
    for index in 96..100 {
        assert!(
            cache.contains(&key_at(hash, SEED, index, 0)),
            "tile {index} is still cached"
        );
    }
}

#[test]
fn tile_cache_eviction_channel_receives_every_removed_key() {
    let hash = identity_hash(1);
    let (a, b, c) = (
        key_at(hash, SEED, 0, 0),
        key_at(hash, SEED, 1, 0),
        key_at(hash, SEED, 2, 0),
    );
    let cache = TileCache::new(2 * synthetic_tile(a).byte_size());
    let events = cache.subscribe_evictions();

    cache.insert(synthetic_tile(a));
    cache.insert(synthetic_tile(b));
    cache.insert(synthetic_tile(c)); // exceeds the budget: evicts A
    assert_eq!(
        events.try_recv().ok(),
        Some(a),
        "the capacity eviction notifies"
    );

    // Explicit removal notifies too (the texture bridge drops handles
    // for exactly these events).
    cache.remove(&b);
    assert_eq!(events.try_recv().ok(), Some(b));
    assert!(events.try_recv().is_err(), "nothing else was removed");
}

#[test]
fn tile_cache_invalidate_seed_leaves_other_seeds_intact() {
    let hash = identity_hash(1);
    let (seed7_a, seed7_b, seed8) = (
        key_at(hash, 7, 0, 0),
        key_at(hash, 7, 1, 0),
        key_at(hash, 8, 0, 0),
    );
    let cache = TileCache::new(1024 * 1024);
    cache.insert(synthetic_tile(seed7_a));
    cache.insert(synthetic_tile(seed7_b));
    cache.insert(synthetic_tile(seed8));
    let events = cache.subscribe_evictions();

    cache.invalidate_seed(7);

    assert!(!cache.contains(&seed7_a), "seed 7's tiles are gone");
    assert!(!cache.contains(&seed7_b));
    assert!(cache.contains(&seed8), "seed 8's tiles stay cached");
    // Both removals are notified, in the HashMap's (unspecified) order.
    let first = events.try_recv().expect("a seed-7 removal was notified");
    let second = events.try_recv().expect("the other seed-7 removal");
    assert!(
        (first == seed7_a && second == seed7_b) || (first == seed7_b && second == seed7_a),
        "both seed-7 keys were notified, got {first:?} and {second:?}"
    );
    assert!(events.try_recv().is_err());
    assert_eq!(cache.stats().entries, 1);
    assert_eq!(
        cache.stats().bytes,
        synthetic_tile(seed8).byte_size(),
        "byte accounting stays exact after invalidation"
    );
}

#[test]
fn tile_cache_version_change_never_serves_the_old_identity() {
    let hash_v1 = identity_hash(1);
    let hash_v2 = identity_hash(2);
    let cache = TileCache::new(1024 * 1024);
    cache.insert(synthetic_tile(key_at(hash_v1, SEED, 0, 0)));

    // A lookup under the new identity is a different key and can never
    // hit the old entry, however identical the rest of the key is.
    assert!(
        cache.get(&key_at(hash_v2, SEED, 0, 0)).is_none(),
        "the new identity cannot see v1's tile"
    );

    // Invalidation frees the stranded v1 tiles without touching v2's.
    cache.insert(synthetic_tile(key_at(hash_v2, SEED, 1, 0)));
    cache.invalidate_version(hash_v1);
    assert!(
        !cache.contains(&key_at(hash_v1, SEED, 0, 0)),
        "v1's tile freed"
    );
    assert!(
        cache.contains(&key_at(hash_v2, SEED, 1, 0)),
        "v2's tile stays warm"
    );
    assert!(cache.get(&key_at(hash_v2, SEED, 1, 0)).is_some());
}

#[test]
fn tile_cache_clear_empties_and_drains_the_channel() {
    let hash = identity_hash(1);
    let a = key_at(hash, SEED, 0, 0);
    let cache = TileCache::new(1024 * 1024);
    let events = cache.subscribe_evictions();

    cache.insert(synthetic_tile(a));
    cache.remove(&a); // one notification is now pending
    cache.insert(synthetic_tile(key_at(hash, SEED, 1, 0)));

    cache.clear();
    assert!(
        events.try_recv().is_err(),
        "clear drained the pending event"
    );
    let stats = cache.stats();
    assert_eq!(stats.entries, 0);
    assert_eq!(stats.bytes, 0);

    // The cache keeps working after a clear.
    cache.insert(synthetic_tile(a));
    assert!(cache.get(&a).is_some());
    assert_eq!(cache.stats().entries, 1);
}

#[test]
fn tile_cache_counts_hits_and_misses() {
    let hash = identity_hash(1);
    let a = key_at(hash, SEED, 0, 0);
    let cache = TileCache::new(1024 * 1024);

    assert!(cache.get(&a).is_none());
    assert!(cache.get(&a).is_none());
    cache.insert(synthetic_tile(a));
    assert!(cache.get(&a).is_some());
    assert!(cache.get(&a).is_some());
    assert!(cache.get(&a).is_some());
    // contains is a peek: no hit/miss accounting, no recency touch.
    assert!(cache.contains(&a));

    let stats = cache.stats();
    assert_eq!(stats.hits, 3);
    assert_eq!(stats.misses, 2);
    assert_eq!(stats.entries, 1);
}

#[test]
fn tile_cache_zero_budget_keeps_nothing() {
    // The degenerate budget must not wedge the cache: the tile evicts
    // itself, and the eviction says so.
    let hash = identity_hash(1);
    let a = key_at(hash, SEED, 0, 0);
    let cache = TileCache::new(0);
    let events = cache.subscribe_evictions();

    cache.insert(synthetic_tile(a));

    assert_eq!(cache.stats().entries, 0, "the tile evicted itself");
    assert!(!cache.contains(&a));
    assert_eq!(
        events.try_recv().ok(),
        Some(a),
        "the self-eviction was notified"
    );
}
