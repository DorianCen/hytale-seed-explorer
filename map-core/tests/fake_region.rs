#![cfg(feature = "test-support")]
//! Proves the T07 acceptance bullet: "the fake source implements both traits
//! and can render a biome grid", including a 7×5 region at step 2 with a
//! negative origin and correct first/last sample coordinates.

use map_core::source::{SummaryFidelity, WorldSource};
use map_core::test_support::FakeWorldSource;
use map_core::types::{BiomeId, Rect};

/// Asks the fake source for a 7×5 region at step 2 from a negative origin
/// (`Rect::from_dims(-10, -7, 14, 10)`) and checks the grid metadata, the
/// first and last samples, and that one past the edge is out of bounds.
#[test]
fn fake_region_has_expected_metadata_and_sample_coordinates() {
    let source = FakeWorldSource::new("12345");
    let rect = Rect::from_dims(-10, -7, 14, 10);

    let biomes = source.biome_region(rect, 2).expect("fake biome region");
    assert_eq!(biomes.width, 7);
    assert_eq!(biomes.height, 5);
    assert_eq!(biomes.step, 2);
    assert_eq!((biomes.origin_x, biomes.origin_z), (-10, -7));

    // Sample coordinates are origin + index * step: (-10, -7) and (2, 1).
    assert_eq!(biomes.sample(-10, -7).copied(), Some(BiomeId(6 + 8 * 7)));
    assert_eq!(biomes.sample(2, 1).copied(), Some(BiomeId(0)));
    assert_eq!(biomes.sample(4, 1), None);

    let heights = source.height_region(rect, 2).expect("fake height region");
    // 64 + ((-10 + -7) / 8).rem_euclid(16) = 64 + (-2).rem_euclid(16) = 78.
    assert_eq!(heights.sample(-10, -7).copied(), Some(78));
    // 64 + ((2 + 1) / 8).rem_euclid(16) = 64.
    assert_eq!(heights.sample(2, 1).copied(), Some(64));
    assert_eq!(heights.sample(4, 1), None);

    let surfaces = source
        .surface_region(rect, 2, SummaryFidelity::Full)
        .expect("fake surface region");
    let first = surfaces.sample(-10, -7).expect("first surface sample");
    assert_eq!((first.x, first.z, first.height), (-10, -7, 78));
    assert_eq!(first.fluid, None, "height 78 is above the fake lake line");
}
