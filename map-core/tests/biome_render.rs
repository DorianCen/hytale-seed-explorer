//! T13 tests: the biome layer's contract against the real generator.
//!
//! The synthetic color/mapping/orientation cases live as unit tests in
//! `map_core::render::biome`; this file pins the integration invariants:
//! the refactored [`render_biome_rgba`] stays byte-identical to T11's
//! golden (shared hashes, never duplicated), and every pixel of a real
//! render equals the catalog's color for its grid sample — no index
//! skew between grid and image.

mod common;

use std::collections::HashSet;

use map_core::render::{biome_pixel, render_biome_rgba};
use map_core::source::WorldSource;
use map_core::types::BiomeId;

use common::{
    GOLDEN_CENTER, GOLDEN_CENTER_SHA256, GOLDEN_NEGATIVE_CENTER, GOLDEN_NEGATIVE_SHA256,
    GOLDEN_STEP, center_rect, distinct_colors, render_region, sha256, world_dir,
};

#[test]
fn biome_render_matches_the_t11_golden_bytes() {
    if world_dir().is_none() {
        return;
    }
    // The T13 refactor (render_biome -> render_biome_rgba, the move to
    // render/biome.rs) must not change a single pixel of the headless
    // output. Same helper, same hash constants as the golden test —
    // both fixed cases, center and negative-center.
    let bytes = render_region(GOLDEN_CENTER);
    assert_eq!(
        sha256(&bytes),
        GOLDEN_CENTER_SHA256,
        "the biome layer diverged from the T11 golden bytes"
    );
    assert_eq!(
        sha256(&render_region(GOLDEN_NEGATIVE_CENTER)),
        GOLDEN_NEGATIVE_SHA256,
        "the negative-center case diverged from the T11 golden bytes"
    );
    // And the center case is a real map, not a flat fill.
    assert!(
        distinct_colors(&bytes) > 1,
        "the golden bytes must show at least two biome colors"
    );
}

#[test]
fn every_pixel_equals_its_grid_samples_catalog_color() {
    if world_dir().is_none() {
        return;
    }
    let source = common::source();
    let rect = center_rect(GOLDEN_CENTER);
    let grid = source
        .biome_region(rect, GOLDEN_STEP)
        .expect("the golden region serves");

    let image = render_biome_rgba(&grid, source.biome_catalog());
    assert_eq!(
        (image.width(), image.height()),
        (grid.width, grid.height),
        "the image is exactly one pixel per sample"
    );
    assert_eq!(
        image.as_raw().len(),
        (grid.width * grid.height * 4) as usize,
        "the byte buffer is width * height * 4"
    );

    // Row-major, no skew: pixel (x, y) is the catalog color of grid
    // sample (x, y), for every sample of the real region.
    let catalog = source.biome_catalog();
    for y in 0..grid.height {
        for x in 0..grid.width {
            let id = grid.biomes[(y as usize) * (grid.width as usize) + (x as usize)];
            assert_eq!(
                image.get_pixel(x, y),
                &biome_pixel(id, catalog),
                "pixel ({x}, {y}) must be sample ({x}, {y})'s color"
            );
        }
    }
}

#[test]
fn the_real_world_uses_only_catalog_biomes_no_magenta() {
    if world_dir().is_none() {
        return;
    }
    // The Default world's catalog covers every id its generator emits
    // around spawn; magenta (an unknown id) in this render would mean a
    // catalog bug. A regression here is exactly what the once-per-id
    // warning would log in the app.
    let source = common::source();
    let rect = center_rect(GOLDEN_CENTER);
    let grid = source
        .biome_region(rect, GOLDEN_STEP)
        .expect("the golden region serves");

    let unknown: HashSet<BiomeId> = grid
        .biomes
        .iter()
        .copied()
        .filter(|id| source.biome_catalog().get(*id).is_none())
        .collect();
    assert!(
        unknown.is_empty(),
        "ids missing from the catalog (would render magenta): {unknown:?}"
    );
}
