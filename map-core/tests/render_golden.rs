//! T11 golden-image tests: the whole headless pipeline in one hash.
//!
//! `WorldgenSource` (worker pool) → `biome_region` → `render_biome_rgba`
//! → `encode_png`, then a SHA-256 over the exact PNG bytes. Any change —
//! one pixel of color, the channel order, the encoder configuration, the
//! generator — breaks the hash.
//!
//! The fixed cases and expected hashes live in [`common`], shared with
//! `tests/biome_render.rs` (one definition of the expected bytes).
//! The hashes are regenerated only for an intentional output change (a
//! `GENERATOR_VERSION` bump or a deliberate palette decision), following
//! the fidelity-fixture update policy; anything else is a regression.

mod common;

use common::{
    GOLDEN_CENTER, GOLDEN_CENTER_SHA256, GOLDEN_NEGATIVE_CENTER, GOLDEN_NEGATIVE_SHA256,
    distinct_colors, render_region, sha256, world_dir,
};

#[test]
fn golden_biome_seed_0_center_origin() {
    if world_dir().is_none() {
        return;
    }
    let bytes = render_region(GOLDEN_CENTER);
    assert_eq!(
        sha256(&bytes),
        GOLDEN_CENTER_SHA256,
        "the seed-0 origin render changed; see the module docs before \
         touching this hash"
    );
    // A one-pixel change already breaks the hash; the color count keeps
    // the golden honest in the other direction — a flat image would
    // prove the pipeline stopped varying with the world.
    assert!(
        distinct_colors(&bytes) > 1,
        "the golden image must show at least two biome colors"
    );
    // The same command twice must produce identical bytes: a second full
    // pass through the pool, not a cached buffer.
    assert_eq!(
        render_region(GOLDEN_CENTER),
        bytes,
        "a repeat render must be byte-identical"
    );
}

#[test]
fn golden_hash_is_sensitive_to_a_single_pixel() {
    if world_dir().is_none() {
        return;
    }
    let bytes = render_region(GOLDEN_CENTER);
    // Flip exactly one pixel and re-encode through the same encoder: the
    // hash must move. This pins that the golden covers pixel content, so
    // an encoder change that dropped or ignored pixel data would fail too.
    let mut image = image::load_from_memory(&bytes)
        .expect("the golden PNG decodes")
        .to_rgba8();
    image.put_pixel(0, 0, image::Rgba([255, 255, 255, 255]));
    let tampered = map_core::render::encode_png(&image).expect("encoding the tampered image");
    assert_ne!(
        sha256(&tampered),
        sha256(&bytes),
        "a one-pixel change must break the golden hash"
    );
}

#[test]
fn golden_biome_seed_0_negative_center() {
    if world_dir().is_none() {
        return;
    }
    let bytes = render_region(GOLDEN_NEGATIVE_CENTER);
    assert_eq!(
        sha256(&bytes),
        GOLDEN_NEGATIVE_SHA256,
        "the seed-0 negative-center render changed; see the module docs \
         before touching this hash"
    );
    assert!(
        distinct_colors(&bytes) > 1,
        "the negative-center golden must show at least two biome colors"
    );
    // And the two cases must not be the same view of the world.
    assert_ne!(
        sha256(&render_region(GOLDEN_CENTER)),
        sha256(&bytes),
        "the two golden cases cover different world areas"
    );
}
