//! T20 acceptance: the block palette, the map-image color formula and the
//! name-keyed resolution policy.
//!
//! The formula tests pin the values recorded in `docs/assets-and-palette.md`
//! §6 against the real pack (the probe's output, reproduced by the server's
//! `map_image.rs`), so this suite fails if the explorer ever grows a second,
//! divergent color path. The policy tests exercise the parts a palette file
//! cannot prove on its own: name→id resolution through the catalog (ids are
//! schedule-dependent), the visible unknown-block colors, schema refusal and
//! byte-deterministic output.
//!
//! No asset pack and no generator are needed: every case is synthetic.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::PathBuf;

use map_core::catalog::BlockCatalog;
use map_core::palette::{
    BlockColor, BlockPalette, FALLBACK_SOURCE, PaletteError, PaletteFile, UNKNOWN_BLOCK_COLOR,
    block_color, parse_color, unknown_color,
};
use map_core::types::BlockId;

/// The §6 sample biome tint (Plains `#639726`): the one the recorded
/// `Soil_Grass` value `(99, 151, 38)` was produced with.
const SAMPLE_BIOME_TINT: [u8; 3] = [99, 151, 38];

/// A white-base, no-biome-tint entry whose only ingredient is its
/// `ParticleColor` — the shape most of the pack's blocks have.
fn particle_only(particle: [u8; 3]) -> BlockColor {
    BlockColor {
        particle: Some(particle),
        ..BlockColor::default()
    }
}

fn entry(rgb: [u8; 3], biome_tint: f32) -> BlockColor {
    BlockColor {
        rgb,
        biome_tint,
        particle: None,
    }
}

#[test]
fn palette_formula_matches_the_recorded_map_image_values() {
    // docs §6: white base, no biome tint, particle `#f9cb57`.
    assert_eq!(
        block_color(&particle_only([249, 203, 87]), SAMPLE_BIOME_TINT),
        [249, 203, 87, 255],
        "Soil_Sand is exactly its ParticleColor",
    );
    // Rock_Stone `#737055`.
    assert_eq!(
        block_color(&particle_only([115, 112, 85]), SAMPLE_BIOME_TINT),
        [115, 112, 85, 255],
        "Rock_Stone is exactly its ParticleColor",
    );
    // Soil_Snow `#ddf0f4`.
    assert_eq!(
        block_color(&particle_only([221, 240, 244]), SAMPLE_BIOME_TINT),
        [221, 240, 244, 255],
        "Soil_Snow is exactly its ParticleColor",
    );
    // Soil_Grass: `TintUp` `#67b62d`, `BiomeTintUp: 100` → m = 1.0, so the
    // runtime tint (`#639726`) wins exactly; base and particle are moot.
    let grass = entry([103, 182, 45], 1.0);
    assert_eq!(
        block_color(&grass, SAMPLE_BIOME_TINT),
        [99, 151, 38, 255],
        "Soil_Grass at BiomeTintUp 100 is exactly the runtime biome tint",
    );
}

#[test]
fn palette_formula_biome_tint_levels() {
    // White base plus a white particle: the particle multiply is an
    // identity, so these isolate the biome blend. Tint blue (0, 0, 255).
    let tint = [0, 0, 255];
    let make = |m: f32| BlockColor {
        rgb: [255, 255, 255],
        biome_tint: m,
        particle: Some([255, 255, 255]),
    };

    // m = 0.0: c = 255 + (tint − 255) × 0 = 255 on red/green; blue is
    // 255 + (255 − 255) × 0 = 255.
    assert_eq!(block_color(&make(0.0), tint), [255, 255, 255, 255]);
    // m = 0.5: red/green = 255 + (0 − 255) × 0.5 = 127.5, and the `as u8`
    // cast truncates toward zero → 127. Blue = 255 + 0 × 0.5 = 255.
    assert_eq!(block_color(&make(0.5), tint), [127, 127, 255, 255]);
    // m = 1.0: the runtime tint replaces the base outright, and the
    // particle path is skipped (its guard is `m < 1.0`).
    assert_eq!(block_color(&make(1.0), tint), [0, 0, 255, 255]);
}

#[test]
fn palette_particle_multiplies_only_below_full_tint() {
    // A black particle would zero the result if the multiply ran.
    let full = BlockColor {
        rgb: [255, 255, 255],
        biome_tint: 1.0,
        particle: Some([0, 0, 0]),
    };
    assert_eq!(
        block_color(&full, [10, 20, 30]),
        [10, 20, 30, 255],
        "at m = 1.0 map_image ignores ParticleColor entirely",
    );

    // Just below full tint it does run: the blend gives the base 255/2 =
    // 127.5 → 127, then 127 × 0 / 255 = 0.
    let below = BlockColor {
        rgb: [255, 255, 255],
        biome_tint: 0.5,
        particle: Some([0, 0, 0]),
    };
    assert_eq!(
        block_color(&below, [0, 0, 0]),
        [0, 0, 0, 255],
        "the particle multiply applies while m < 1.0",
    );
}

#[test]
fn palette_parse_color_accepts_pack_and_legacy_forms() {
    let parse = |text: &str| parse_color(Some(&serde_json::json!(text)));

    assert_eq!(parse("#f9cb57"), Some([249, 203, 87]), "pack form");
    assert_eq!(
        parse_color(Some(&serde_json::json!(["#67b62d"]))),
        Some([103, 182, 45]),
        "arrays use the first element",
    );
    assert_eq!(parse("5b9e28"), Some([91, 158, 40]), "legacy bare form");
    assert_eq!(parse("0xd5b656"), Some([213, 182, 86]), "legacy 0x form");
    assert_eq!(parse("#abc"), Some([170, 187, 204]), "3 digits double");
    // The 8-digit form keeps the LAST six digits (docs §3): 11223344 →
    // 223344.
    assert_eq!(
        parse("#11223344"),
        Some([34, 51, 68]),
        "8 digits drop the top pair"
    );

    assert_eq!(
        parse_color(Some(&serde_json::json!(42))),
        None,
        "not a string"
    );
    assert_eq!(parse(""), None, "empty string");
    assert_eq!(parse("#12"), None, "unsupported length");
}

#[test]
fn palette_fallback_loads_and_covers_common_blocks() {
    let palette = BlockPalette::fallback();
    assert_eq!(palette.len(), 20, "the hand-picked fallback table");
    assert!(!palette.is_empty());

    let grass = palette.get("Soil_Grass").expect("grass is in the fallback");
    assert_eq!(
        grass.biome_tint, 1.0,
        "grass follows the runtime biome tint",
    );

    // Round-trip through the file format: the serialized header and every
    // entry come back identical, and the u8 channels stay in range (the
    // sanity row; a real byte is always 0..=255).
    let bytes = palette.to_json_bytes().expect("the fallback serializes");
    let file: PaletteFile =
        serde_json::from_slice(&bytes).expect("the fallback output is schema-1 JSON");
    assert_eq!(file.schema, BlockPalette::SCHEMA);
    assert_eq!(file.source, FALLBACK_SOURCE);
    assert_eq!(file.pack_hash, None, "the fallback is not pack-derived");
    assert_eq!(
        file.generated_by,
        "hand-picked original approximations (T20)"
    );
    assert_eq!(file.entries.len(), palette.len());
    for (name, color) in &file.entries {
        assert!(
            color
                .rgb
                .iter()
                .all(|&channel| (0..=255).contains(&channel)),
            "{name} has an out-of-range channel: {:?}",
            color.rgb,
        );
        assert_eq!(
            palette.get(name),
            Some(color),
            "{name} round-trips through the palette file",
        );
    }
}

#[test]
fn palette_unknown_blocks_are_visible_and_distinct() {
    // Made-up modded names: no pack, no metadata, still visible.
    let names = [
        "Mod_Crystal_Blue",
        "Mod_Obsidian_Glass",
        "Mod_Moss_Carpet",
        "Mod_Bronze_Grate",
        "Mod_Cloudstone",
    ];
    let colors: Vec<[u8; 4]> = names.iter().map(|name| unknown_color(name)).collect();
    for (name, color) in names.iter().zip(&colors) {
        assert_eq!(
            color[3], 255,
            "{name}: unknown blocks are never transparent"
        );
        for channel in &color[..3] {
            assert!(
                (80..=230).contains(channel),
                "{name} channel {channel} is outside the readable band",
            );
        }
    }
    let distinct: BTreeSet<[u8; 4]> = colors.iter().copied().collect();
    assert!(
        distinct.len() >= 4,
        "distinct names must read apart, got {distinct:?}",
    );

    assert_eq!(
        UNKNOWN_BLOCK_COLOR,
        [255, 0, 255, 255],
        "magenta is reserved for an id with no metadata at all",
    );

    // Policy through the palette: an id the catalog cannot name is magenta.
    let palette = BlockPalette::from_entries(BTreeMap::new());
    assert!(palette.is_empty());
    let catalog = BlockCatalog::default();
    assert_eq!(
        palette.color(BlockId(7), [0, 0, 0], &catalog),
        UNKNOWN_BLOCK_COLOR,
        "an unnamed id has no metadata at all",
    );

    // A name the catalog knows but the palette lacks gets the hue policy.
    let mut catalog = BlockCatalog::default();
    catalog
        .insert(BlockId(7), "Mod_Crystal_Blue")
        .expect("unique id and name");
    assert_eq!(
        palette.color(BlockId(7), [0, 0, 0], &catalog),
        unknown_color("Mod_Crystal_Blue"),
        "a catalogued name without a palette entry stays visible",
    );
}

#[test]
fn palette_resolves_ids_through_the_catalog_not_the_file() {
    // The same names under swapped ids: exactly the schedule dependence
    // docs §4 warns about.
    let mut first = BlockCatalog::default();
    first
        .insert(BlockId(1), "Soil_Sand")
        .expect("unique id and name");
    first
        .insert(BlockId(2), "Rock_Stone")
        .expect("unique id and name");
    first
        .insert(BlockId(3), "Soil_Clay")
        .expect("unique id and name");

    let mut swapped = BlockCatalog::default();
    swapped
        .insert(BlockId(2), "Soil_Sand")
        .expect("unique id and name");
    swapped
        .insert(BlockId(1), "Rock_Stone")
        .expect("unique id and name");

    let entries = BTreeMap::from([
        (String::from("Soil_Sand"), entry([222, 205, 153], 0.0)),
        (String::from("Rock_Stone"), entry([128, 128, 128], 0.0)),
    ]);
    let palette = BlockPalette::from_entries(entries);
    let tint = [0, 0, 0];

    assert_eq!(
        palette.color(BlockId(1), tint, &first),
        [222, 205, 153, 255],
        "id 1 is Soil_Sand in the first catalog",
    );
    assert_eq!(
        palette.color(BlockId(2), tint, &first),
        [128, 128, 128, 255],
        "id 2 is Rock_Stone in the first catalog",
    );
    assert_eq!(
        palette.color(BlockId(1), tint, &swapped),
        [128, 128, 128, 255],
        "id 1 is Rock_Stone after the swap — the name resolves, not the id",
    );
    assert_eq!(
        palette.color(BlockId(2), tint, &swapped),
        [222, 205, 153, 255],
        "id 2 is Soil_Sand after the swap",
    );

    // A catalogued name the palette lacks falls to the unknown policy.
    assert_eq!(
        palette.color(BlockId(3), tint, &first),
        unknown_color("Soil_Clay"),
        "known name, missing entry → deterministic visible grey",
    );
}

/// A unique temp path for the schema/IO cases; the caller removes the file.
fn temp_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "hytale-seed-explorer-palette-{}-{tag}.json",
        std::process::id(),
    ))
}

#[test]
fn palette_load_rejects_wrong_schema_and_reports_io() {
    let path = temp_path("schema");
    std::fs::write(
        &path,
        r#"{"schema":99,"source":"x","generated_by":"y","entries":{}}"#,
    )
    .expect("the temp palette writes");
    let result = BlockPalette::load(&path);
    std::fs::remove_file(&path).expect("the temp palette is removed");
    assert!(
        matches!(&result, Err(PaletteError::Schema { found: 99 })),
        "a wrong schema must be refused loudly, got {result:?}",
    );

    let missing = temp_path("missing");
    let result = BlockPalette::load(&missing);
    assert!(
        matches!(&result, Err(PaletteError::Io(_))),
        "a missing file is an IO error, got {result:?}",
    );
}

#[test]
fn palette_json_bytes_are_deterministic() {
    let expected = BTreeMap::from([
        (String::from("Soil_Sand"), entry([222, 205, 153], 0.0)),
        (String::from("Soil_Grass"), entry([106, 168, 79], 1.0)),
        (String::from("Rock_Stone"), entry([128, 128, 128], 0.0)),
    ]);
    let palette = BlockPalette::from_entries(expected.clone()).with_header(
        "hytale-assets",
        Some("sha256:deadbeef"),
        "test",
    );

    let first = palette.to_json_bytes().expect("serializes");
    let second = palette.to_json_bytes().expect("serializes");
    assert_eq!(first, second, "the same palette is byte-identical");

    let file: PaletteFile = serde_json::from_slice(&first).expect("output is a palette file");
    assert_eq!(file.schema, BlockPalette::SCHEMA);
    assert_eq!(file.source, "hytale-assets");
    assert_eq!(file.pack_hash.as_deref(), Some("sha256:deadbeef"));
    assert_eq!(file.generated_by, "test");
    assert_eq!(file.entries, expected);

    // serde_json preserves map insertion order, and the BTreeMap inserts
    // sorted, so the raw bytes must list the names alphabetically.
    let text = std::str::from_utf8(&first).expect("palette JSON is UTF-8");
    let stone = text.find("\"Rock_Stone\"").expect("Rock_Stone is present");
    let grass = text.find("\"Soil_Grass\"").expect("Soil_Grass is present");
    let sand = text.find("\"Soil_Sand\"").expect("Soil_Sand is present");
    assert!(
        stone < grass && grass < sand,
        "entries must serialize in sorted name order",
    );
}
