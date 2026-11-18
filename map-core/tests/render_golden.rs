//! T11 golden-image tests: the whole headless pipeline in one hash.
//!
//! `WorldgenSource` (worker pool) → `biome_region` → `render_biome` →
//! `encode_png`, then a SHA-256 over the exact PNG bytes. Any change —
//! one pixel of color, the channel order, the encoder configuration, the
//! generator — breaks the hash.
//!
//! The real generator checkout lives next to this repository (see
//! [`world_dir`]); when it is absent every test prints a skip message and
//! returns, like `tests/worldgen_source.rs`. One shared source (seed 0)
//! loads the pool once and serves both cases.
//!
//! The hashes are regenerated only for an intentional output change (a
//! `GENERATOR_VERSION` bump or a deliberate palette decision), following
//! the fidelity-fixture update policy; anything else is a regression.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use map_core::render::{encode_png, render_biome};
use map_core::source::WorldSource;
use map_core::source::worldgen::{WorldgenSource, WorldgenSourceConfig};
use map_core::types::Rect;
use sha2::{Digest, Sha256};

/// Seed of the golden cases, exactly as the CLI would type it.
const GOLDEN_SEED: &str = "0";

/// The task's fixed case: 64×64 samples at step 2 around the origin.
const GOLDEN_SIZE: u32 = 64;
const GOLDEN_STEP: u32 = 2;
const GOLDEN_CENTER: (i32, i32) = (0, 0);

/// Second case, echoing T10's table-test center, so the two coordinate
/// tasks stay locked to the same example.
const GOLDEN_NEGATIVE_CENTER: (i32, i32) = (-1203, 455);

/// SHA-256 of the center-case PNG bytes (64×64, step 2, seed 0).
const GOLDEN_CENTER_SHA256: &str =
    "a71236edde2ac1db70fac1613e882359a75e607412612b7744136dd86d17a41e";

/// SHA-256 of the negative-center PNG bytes (same size/step/seed).
const GOLDEN_NEGATIVE_SHA256: &str =
    "b0dda0aa704f2334a1e9fb75f189b9a08eca195de5558aba090b01c105df3e0b";

/// The world-gen folder this repository's sibling checkout ships for
/// tests, or `None` (with a skip message) on machines without it.
fn world_dir() -> Option<PathBuf> {
    let dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hytale-server-rust/testdata/Default");
    if dir.is_dir() {
        Some(dir)
    } else {
        eprintln!(
            "skipping: the worldgen checkout is not present at {}",
            dir.display()
        );
        None
    }
}

/// The shared, already-loaded golden source (seed 0, default 2 workers).
fn source() -> &'static WorldgenSource {
    static SOURCE: OnceLock<WorldgenSource> = OnceLock::new();
    SOURCE.get_or_init(|| {
        let world = world_dir().expect("the shared source is only built after world_dir()");
        let config = WorldgenSourceConfig::new(world, GOLDEN_SEED);
        WorldgenSource::new(&config).expect("the golden worldgen source loads")
    })
}

/// The block rect behind a render: `size` samples per axis at `step`
/// blocks, centered — the same math the CLI's `center_rect` does.
fn center_rect(center: (i32, i32), size: u32, step: u32) -> Rect {
    let blocks = i64::from(size) * i64::from(step);
    let half = blocks / 2;
    let min_x = i64::from(center.0) - half;
    let min_z = i64::from(center.1) - half;
    Rect::new(
        i32::try_from(min_x).expect("the golden cases fit i32"),
        i32::try_from(min_z).expect("the golden cases fit i32"),
        i32::try_from(min_x + blocks - 1).expect("the golden cases fit i32"),
        i32::try_from(min_z + blocks - 1).expect("the golden cases fit i32"),
    )
}

/// Renders one golden case through the full pipeline and returns the PNG
/// bytes.
fn render_region(center: (i32, i32)) -> Vec<u8> {
    let source = source();
    let grid = source
        .biome_region(center_rect(center, GOLDEN_SIZE, GOLDEN_STEP), GOLDEN_STEP)
        .expect("the golden biome region succeeds");
    let image = render_biome(&grid, source.biome_catalog());
    encode_png(&image).expect("encoding the golden image")
}

fn sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// The distinct opaque colors of an encoded image, proving the render is
/// not one flat fill.
fn distinct_colors(png_bytes: &[u8]) -> usize {
    let decoded = image::load_from_memory(png_bytes)
        .expect("the golden PNG decodes")
        .to_rgba8();
    let colors: HashSet<[u8; 3]> = decoded
        .pixels()
        .map(|pixel| [pixel[0], pixel[1], pixel[2]])
        .collect();
    colors.len()
}

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
    let tampered = encode_png(&image).expect("encoding the tampered image");
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
