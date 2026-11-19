//! Shared golden-case helpers for the render tests (T11's `render_golden`
//! and T13's `biome_render`).
//!
//! One definition of the fixed cases, the pipeline and the expected
//! hashes — a second test file must never duplicate the expected bytes.
//! Each test binary links its own copy (and loads its own generator);
//! skip-aware helpers make that safe on machines without the checkout.

// Each linking test binary uses a subset of this fixture (T17's
// `layers`, for instance, needs only the source, the step-2 rect
// helper and the hash functions); the unused remainder would read as
// dead code in that binary under `-D warnings`, so the shared fixture
// opts out of that one lint here, once, instead of per item.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use map_core::render::{encode_png, render_biome_rgba};
use map_core::source::WorldSource;
use map_core::source::worldgen::{WorldgenSource, WorldgenSourceConfig};
use map_core::types::Rect;
use sha2::{Digest, Sha256};

/// Seed of the golden cases, exactly as the CLI would type it.
pub const GOLDEN_SEED: &str = "0";

/// The task's fixed case: 64×64 samples at step 2.
pub const GOLDEN_SIZE: u32 = 64;
pub const GOLDEN_STEP: u32 = 2;

/// Center-case coordinates (the origin).
pub const GOLDEN_CENTER: (i32, i32) = (0, 0);

/// Negative-center case, echoing T10's table-test center.
pub const GOLDEN_NEGATIVE_CENTER: (i32, i32) = (-1203, 455);

/// SHA-256 of the center-case PNG bytes.
pub const GOLDEN_CENTER_SHA256: &str =
    "a71236edde2ac1db70fac1613e882359a75e607412612b7744136dd86d17a41e";

/// SHA-256 of the negative-center PNG bytes.
pub const GOLDEN_NEGATIVE_SHA256: &str =
    "b0dda0aa704f2334a1e9fb75f189b9a08eca195de5558aba090b01c105df3e0b";

/// The world-gen folder this repository's sibling checkout ships for
/// tests, or `None` (with a skip message) on machines without it.
///
/// Also requires the user-supplied, git-ignored asset pack: the committed
/// `testdata/Default` world references prefabs from it, so the generator
/// cannot load the world without the pack. Tests skip instead of panicking
/// (CI has no pack — it must not, the assets are proprietary).
pub fn world_dir() -> Option<PathBuf> {
    let dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hytale-server-rust/testdata/Default");
    if !dir.is_dir() {
        eprintln!(
            "skipping: the worldgen checkout is not present at {}",
            dir.display()
        );
        return None;
    }
    let assets = dir.join("../../Assets/extracted");
    if !assets.is_dir() {
        eprintln!(
            "skipping: the worldgen checkout has no asset pack at {} (the committed world \
             references prefabs from the user-supplied pack)",
            assets.display()
        );
        return None;
    }
    Some(dir)
}

/// The shared, already-loaded golden source (seed 0). Only call this
/// after [`world_dir`] returned [`Some`].
pub fn source() -> &'static WorldgenSource {
    static SOURCE: OnceLock<WorldgenSource> = OnceLock::new();
    SOURCE.get_or_init(|| {
        let world = world_dir().expect("callers check world_dir() first");
        let config = WorldgenSourceConfig::new(world, GOLDEN_SEED);
        WorldgenSource::new(&config).expect("the golden worldgen source loads")
    })
}

/// The block rect behind a golden case: `size` samples per axis at
/// `step` blocks, centered.
pub fn center_rect(center: (i32, i32)) -> Rect {
    let blocks = i64::from(GOLDEN_SIZE) * i64::from(GOLDEN_STEP);
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
pub fn render_region(center: (i32, i32)) -> Vec<u8> {
    let source = source();
    let grid = source
        .biome_region(center_rect(center), GOLDEN_STEP)
        .expect("the golden biome region succeeds");
    let image = render_biome_rgba(&grid, source.biome_catalog());
    encode_png(&image).expect("encoding the golden image")
}

/// SHA-256 of `bytes`, lowercase hex.
pub fn sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// The distinct opaque colors of an encoded image.
pub fn distinct_colors(png_bytes: &[u8]) -> usize {
    let decoded = image::load_from_memory(png_bytes)
        .expect("the golden PNG decodes")
        .to_rgba8();
    let colors: std::collections::HashSet<[u8; 3]> = decoded
        .pixels()
        .map(|pixel| [pixel[0], pixel[1], pixel[2]])
        .collect();
    colors.len()
}
