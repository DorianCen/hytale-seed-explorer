//! `map-cli palette` — builds the schema-1 block palette from the user's own
//! asset pack (T20; `docs/assets-and-palette.md` §7/§11).
//!
//! **Why the raw pack API.** The full `hytale-assets` registry decodes ~500 MiB
//! and a second of work for stores the palette never reads (§7), so this tool
//! goes through [`AssetPack`]'s lightweight `list`/`read_many` and only touches
//! `Server/Item/Items/**` — no textures, no models, no PNG decoding.
//!
//! **What it mirrors.** The extraction and parent resolution reproduce the
//! worldgen block store (`hytale-assets/src/registry.rs`'s
//! `extract_item_block_types`/`extract_block_type_definitions` and
//! `resolve_assets`): an item's `BlockType` is one record keyed by the file
//! stem, its object `State.Definitions` entries are contained records keyed
//! `*<item>_State_Definitions_<name>` (nested levels add leading `*`), parents
//! come from `Parent` or Java's `INHERIT_ID_AND_PARENT` fallback, and the merge
//! is the block store's `MergeHook::Shallow`.
//!
//! **Output.** A name-keyed, sorted, deterministic JSON palette whose header
//! records a SHA-256 over the exact JSON files read (the pack itself is ~GB, so
//! hashing the manifest of what was read is both cheap and sufficient). The
//! generated file is derived Licensed Asset data: it is never committed, no
//! matter what `--allow-commit` says (`docs/assets-and-palette.md` §2).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use clap::Args;
use hytale_assets::{AssetKey, AssetPack, MergeHook, ParentProvider, ParentResolver};
use map_core::palette::{BlockColor, BlockPalette, parse_color};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The pack subtree the palette reads: block metadata lives in item files, and
/// nothing else (item states are only read for their contained block types).
const ITEMS_PREFIX: &str = "Server/Item/Items/";

/// The provenance string written into generated palettes.
const PALETTE_SOURCE: &str = "hytale-assets";

/// Arguments of the `palette` subcommand.
#[derive(Debug, Args)]
pub(crate) struct PaletteArgs {
    /// The asset pack to read: an extracted tree or `Assets.zip`.
    #[arg(long, default_value_os_t = PathBuf::from("Assets/extracted"))]
    pub(crate) input: PathBuf,

    /// Output palette JSON path (parent directories are created).
    #[arg(long, default_value_os_t = PathBuf::from("target/palette.json"))]
    pub(crate) out: PathBuf,

    /// A registries snapshot JSON (e.g. `testdata/golden/registries.json`):
    /// every `blocks.assets[].name` gets an entry, so the palette covers the
    /// full block id space even where the pack has no color data.
    #[arg(long)]
    pub(crate) registries: Option<PathBuf>,

    /// A palette file whose entries fill names the pack gave no color data
    /// for, instead of the plain white default.
    #[arg(long)]
    pub(crate) fallback: Option<PathBuf>,

    /// Documentation only: generated palettes are derived Licensed Assets and
    /// are never committed to this repository regardless of this flag
    /// (`docs/assets-and-palette.md` §2).
    #[arg(long)]
    pub(crate) allow_commit: bool,
}

/// What the generated palette's entries were derived from, for the run
/// summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Coverage {
    /// Number of entries in the palette.
    pub(crate) total: usize,
    /// Entries whose color data came from a resolved pack record.
    pub(crate) pack: usize,
    /// Entries filled from `--fallback` because the pack had no record.
    pub(crate) fallback: usize,
    /// Entries left at [`BlockColor::default`] (white) because neither the
    /// pack nor a fallback supplied them.
    pub(crate) defaulted: usize,
    /// `"sha256:<hex>"` over the read-file manifest, copied into the header.
    pub(crate) pack_hash: String,
}

/// One block-type record before parent resolution.
#[derive(Debug)]
struct RawRecord {
    /// Palette/provider key: the item file stem, or a starred state key.
    key: String,
    /// The block-type JSON with its computed `Parent` annotated.
    value: Value,
    /// Whether this record is a contained state definition. State records are
    /// inserted into the palette first so a plain record wins a name collision
    /// (the item's own block type is the authoritative one).
    kind: RecordKind,
}

/// Which family a [`RawRecord`] belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordKind {
    /// The item's top-level `BlockType`, keyed by the item name.
    Plain,
    /// A `*<container>_State_Definitions_<name>` contained block type.
    State,
}

/// Runs the `palette` subcommand: build, serialize, write, summarize.
pub(crate) fn run(args: &PaletteArgs) -> Result<(), String> {
    let (palette, coverage) = build_palette(
        &args.input,
        args.registries.as_deref(),
        args.fallback.as_deref(),
    )?;
    let bytes = palette
        .to_json_bytes()
        .map_err(|error| format!("serializing the palette: {error}"))?;
    write_palette(&args.out, &bytes)?;
    eprintln!(
        "palette: {} entries ({} from the pack, {} from the fallback, {} white defaults), \
         pack {}, wrote {}",
        coverage.total,
        coverage.pack,
        coverage.fallback,
        coverage.defaulted,
        coverage.pack_hash,
        args.out.display()
    );
    if args.allow_commit {
        // Deliberately no enforcement: the flag is documentation. The legal
        // rule is in docs/assets-and-palette.md §2, not in this binary.
        eprintln!(
            "note: a generated palette is derived from Licensed Assets under the Shared Source \
             Agreement; it is never committed to this repository, with or without --allow-commit \
             (docs/assets-and-palette.md §2)"
        );
    }
    Ok(())
}

/// Builds the palette and its coverage from a pack, without any file output —
/// the testable half of [`run`].
///
/// `registries` names the full name set to cover; `fallback` supplies colors
/// for names the pack has no record for. Without `registries` the palette is
/// exactly the pack's records.
pub(crate) fn build_palette(
    input: &Path,
    registries: Option<&Path>,
    fallback: Option<&Path>,
) -> Result<(BlockPalette, Coverage), String> {
    let pack = AssetPack::open(input)
        .map_err(|error| format!("opening asset pack {}: {error}", input.display()))?;
    let (pack_colors, pack_hash) = read_pack_colors(&pack)?;
    let fallback_palette = match fallback {
        Some(path) => Some(
            BlockPalette::load(path)
                .map_err(|error| format!("loading fallback palette {}: {error}", path.display()))?,
        ),
        None => None,
    };

    let (entries, coverage) = match registries {
        Some(path) => {
            let names = registry_names(path)?;
            let mut entries = BTreeMap::new();
            let mut from_pack = 0;
            let mut from_fallback = 0;
            let mut defaulted = 0;
            for name in names {
                let color = if let Some(color) = pack_colors.get(&name) {
                    from_pack += 1;
                    *color
                } else if let Some(color) = fallback_palette
                    .as_ref()
                    .and_then(|palette| palette.get(&name))
                {
                    from_fallback += 1;
                    *color
                } else {
                    defaulted += 1;
                    BlockColor::default()
                };
                // Duplicate registry names collapse here; the counts above
                // stay insertion counts, which is what the summary means.
                entries.insert(name, color);
            }
            let coverage = Coverage {
                total: entries.len(),
                pack: from_pack,
                fallback: from_fallback,
                defaulted,
                pack_hash: pack_hash.clone(),
            };
            (entries, coverage)
        }
        None => {
            let total = pack_colors.len();
            let coverage = Coverage {
                total,
                pack: total,
                fallback: 0,
                defaulted: 0,
                pack_hash: pack_hash.clone(),
            };
            (pack_colors, coverage)
        }
    };

    let generated_by = format!("map-cli palette {}", env!("CARGO_PKG_VERSION"));
    let palette = BlockPalette::from_entries(entries).with_header(
        PALETTE_SOURCE,
        Some(&pack_hash),
        &generated_by,
    );
    Ok((palette, coverage))
}

/// Reads every item file, resolves block types, and returns name → color plus
/// the manifest hash of the files read.
fn read_pack_colors(pack: &AssetPack) -> Result<(BTreeMap<String, BlockColor>, String), String> {
    let paths: Vec<String> = pack
        .list()
        .map_err(|error| format!("listing asset pack {}: {error}", pack.path().display()))?
        .into_iter()
        .filter(|path| path.starts_with(ITEMS_PREFIX) && path.ends_with(".json"))
        .collect();
    let contents = pack.read_many(&paths).map_err(|error| {
        format!(
            "reading {} item files from {}: {error}",
            paths.len(),
            pack.path().display()
        )
    })?;

    let mut records = Vec::new();
    let mut manifest = Sha256::new();
    for (path, bytes) in paths.iter().zip(&contents) {
        // The manifest line fixes both the path and the exact bytes read, in
        // sorted path order, so any pack edit changes the provenance hash
        // without hashing the whole (multi-GB) archive.
        let file_hash = Sha256::digest(bytes);
        manifest.update(format!("{path}\n{}\n", hex_bytes(&file_hash)).as_bytes());
        let item: Value = serde_json::from_slice(bytes)
            .map_err(|error| format!("parsing {path} from {}: {error}", pack.path().display()))?;
        let key = AssetKey::from_source(path).key;
        extract_records(&item, &key, &mut records);
    }

    let colors = resolve_records(&records)?;
    Ok((
        colors,
        format!("sha256:{}", hex_bytes(&manifest.finalize())),
    ))
}

/// Extracts an item's own block type and its contained state definitions.
///
/// Mirrors `extract_item_block_types`: the block's parent is its own `Parent`
/// or the item's (`INHERIT_ID_AND_PARENT`); a contained definition's parent is
/// its own `Parent` or the containing block key.
fn extract_records(item: &Value, key: &str, out: &mut Vec<RawRecord>) {
    let Some(block_type) = item.get("BlockType").filter(|value| value.is_object()) else {
        return;
    };
    let item_parent = item.get("Parent").and_then(Value::as_str);
    let parent = block_type
        .get("Parent")
        .and_then(Value::as_str)
        .or(item_parent);
    out.push(RawRecord {
        key: key.to_string(),
        value: with_parent(block_type, parent),
        kind: RecordKind::Plain,
    });
    extract_definitions(block_type, key, out);
}

/// Extracts `State.Definitions` records, recursing like the reference (each
/// nesting level adds a leading `*` to the contained key).
fn extract_definitions(block_type: &Value, key: &str, out: &mut Vec<RawRecord>) {
    let Some(definitions) = block_type
        .get("State")
        .and_then(|state| state.get("Definitions"))
        .and_then(Value::as_object)
    else {
        return;
    };
    for (name, definition) in definitions {
        // A string references an existing block type; it is not a new asset.
        if !definition.is_object() {
            continue;
        }
        let definition_key = format!("*{key}_State_Definitions_{name}");
        let parent = definition
            .get("Parent")
            .and_then(Value::as_str)
            .unwrap_or(key);
        out.push(RawRecord {
            key: definition_key.clone(),
            value: with_parent(definition, Some(parent)),
            kind: RecordKind::State,
        });
        extract_definitions(definition, &definition_key, out);
    }
}

/// Clones an object block-type JSON with `Parent` set to the computed parent.
///
/// The parent may come from the containing item rather than the block's own
/// JSON; annotating the provider document is how the shared resolver learns it
/// (`resolve_assets`' `object.entry("Parent").or_insert_with(...)`), and
/// normalizing an existing `Parent` keeps the two paths indistinguishable.
fn with_parent(value: &Value, parent: Option<&str>) -> Value {
    let mut object = value
        .as_object()
        .cloned()
        .expect("callers pass an object block type");
    if let Some(parent) = parent {
        object.insert("Parent".to_string(), Value::String(parent.to_string()));
    }
    Value::Object(object)
}

/// Resolves every record's parent chain and extracts its palette ingredients.
///
/// State records are inserted before plain records so a name collision keeps
/// the item's own block type, per T20's precedence rule.
fn resolve_records(records: &[RawRecord]) -> Result<BTreeMap<String, BlockColor>, String> {
    // The reference provider is case-insensitive: keys are lowercased and the
    // wrapper lowercases lookups. First insert wins, so the lowest-sorted path
    // keeps the record for a case-duplicate key.
    let mut provider: BTreeMap<String, Value> = BTreeMap::new();
    for record in records {
        provider
            .entry(record.key.to_ascii_lowercase())
            .or_insert_with(|| record.value.clone());
    }
    let lookup = LowercaseProvider { values: &provider };
    let resolver = ParentResolver::with_hook(&lookup, MergeHook::Shallow);

    let mut colors = BTreeMap::new();
    for kind in [RecordKind::State, RecordKind::Plain] {
        for record in records.iter().filter(|record| record.kind == kind) {
            let merged = resolver
                .resolve(&record.key)
                .map_err(|error| format!("resolving block type {}: {error}", record.key))?;
            colors.insert(record.key.clone(), extract_color(&merged));
        }
    }
    Ok(colors)
}

/// `ParentProvider` with the reference's case-insensitive lookup over the
/// lowercased record map (the crate's `BTreeMap` impl matches keys exactly).
struct LowercaseProvider<'a> {
    /// Lowercased key → annotated block-type JSON.
    values: &'a BTreeMap<String, Value>,
}

impl ParentProvider for LowercaseProvider<'_> {
    fn load(&self, key: &str) -> Option<Value> {
        self.values.get(&key.to_ascii_lowercase()).cloned()
    }
}

/// The `map_image.rs` palette ingredients of one resolved block type.
fn extract_color(merged: &Value) -> BlockColor {
    let rgb =
        parse_color(merged.get("TintUp").or_else(|| merged.get("Tint"))).unwrap_or([255, 255, 255]);
    let biome_tint = (merged
        .get("BiomeTintUp")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        / 100.0) as f32;
    let particle = parse_color(merged.get("ParticleColor"));
    BlockColor {
        rgb,
        biome_tint,
        particle,
    }
}

/// Reads the block names a registries snapshot covers (`blocks.assets[].name`).
fn registry_names(path: &Path) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("reading registries {}: {error}", path.display()))?;
    let root: Value = serde_json::from_str(&text)
        .map_err(|error| format!("parsing registries {}: {error}", path.display()))?;
    let assets = root
        .get("blocks")
        .and_then(|blocks| blocks.get("assets"))
        .and_then(Value::as_array)
        .ok_or_else(|| format!("registries {} has no `blocks.assets` array", path.display()))?;
    let mut names = Vec::with_capacity(assets.len());
    for asset in assets {
        let name = asset.get("name").and_then(Value::as_str).ok_or_else(|| {
            format!(
                "registries {} has a block asset without a string `name`",
                path.display()
            )
        })?;
        names.push(name.to_string());
    }
    Ok(names)
}

/// Lowercase hex for the manifest and pack hash (SHA-256 digests).
fn hex_bytes(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(text, "{byte:02x}").expect("writing hex into a String cannot fail");
    }
    text
}

/// Writes the palette bytes, creating parent directories so
/// `--out target/nested/dir/palette.json` works.
fn write_palette(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("creating {}: {error}", parent.display()))?;
    }
    std::fs::write(path, bytes).map_err(|error| format!("writing {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    /// Creates a unique fixture directory for one test (tests run in parallel).
    fn fixture_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("map-cli-palette-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the fixture directory is creatable");
        dir
    }

    /// Writes one fixture file, creating its parents.
    fn write_fixture(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the fixture directory is creatable");
        }
        std::fs::write(path, contents).expect("the fixture file is writable");
    }

    /// Writes the shared synthetic item tree used by the first three tests.
    fn synthetic_pack(dir: &Path) {
        let items = dir.join("Server/Item/Items");
        write_fixture(
            &items.join("Soil/Grass/Soil_Grass.json"),
            r##"{"BlockType":{"TintUp":["#67b62d"],"BiomeTintUp":100,"ParticleColor":"#eae8e8"}}"##,
        );
        write_fixture(
            &items.join("Soil/Grass/Soil_Child.json"),
            r#"{"BlockType":{"Parent":"Soil_Grass"}}"#,
        );
        write_fixture(
            &items.join("Rock/Stone/Rock_Stone.json"),
            r##"{"BlockType":{"ParticleColor":"#f9cb57"}}"##,
        );
        write_fixture(
            &items.join("Deco/Deco_Crate.json"),
            r##"{"BlockType":{"ParticleColor":"#112233","State":{"Definitions":{"Open":{}}}}}"##,
        );
    }

    /// Writes the synthetic registries snapshot: the five synthetic names plus
    /// one name with no pack file.
    fn synthetic_registries(dir: &Path) -> PathBuf {
        let path = dir.join("registries.json");
        write_fixture(
            &path,
            r#"{"blocks":{"assets":[
                {"name":"Soil_Grass"},
                {"name":"Soil_Child"},
                {"name":"Rock_Stone"},
                {"name":"Deco_Crate"},
                {"name":"*Deco_Crate_State_Definitions_Open"},
                {"name":"No_File_Block"}
            ]}}"#,
        );
        path
    }

    /// One expected block color, for readable assertions.
    fn expect_color(name: &str, palette: &BlockPalette) -> BlockColor {
        *palette
            .get(name)
            .unwrap_or_else(|| panic!("the palette has an entry for {name}"))
    }

    #[test]
    fn palette_tool_synthetic_pack_builds_entries_with_parent_inheritance() {
        let dir = fixture_dir("synthetic");
        synthetic_pack(&dir);
        let registries = synthetic_registries(&dir);

        let (palette, coverage) = build_palette(&dir, Some(&registries), None)
            .expect("the synthetic pack builds a palette");

        // Every registries name has an entry; five resolve through the pack,
        // the file-less one is the honest white default.
        assert_eq!(coverage.total, 6, "all registries names are covered");
        assert_eq!(coverage.pack, 5, "five names resolve to pack records");
        assert_eq!(coverage.fallback, 0, "no fallback was supplied");
        assert_eq!(coverage.defaulted, 1, "one name has no pack record");

        let grass = expect_color("Soil_Grass", &palette);
        assert_eq!(grass.rgb, [103, 182, 45], "TintUp #67b62d parses");
        assert_eq!(grass.biome_tint, 1.0, "BiomeTintUp 100 is full tint");

        let child = expect_color("Soil_Child", &palette);
        assert_eq!(child.rgb, grass.rgb, "the child inherits TintUp");
        assert_eq!(child.biome_tint, 1.0, "the child inherits BiomeTintUp");

        let stone = expect_color("Rock_Stone", &palette);
        assert_eq!(stone.rgb, [255, 255, 255], "no tint fields means white");
        assert_eq!(stone.biome_tint, 0.0, "no BiomeTintUp means no biome tint");
        assert_eq!(
            stone.particle,
            Some([249, 203, 87]),
            "ParticleColor #f9cb57 parses"
        );

        // A state definition that overrides nothing inherits from its
        // containing item, which is the record's parent.
        let state = expect_color("*Deco_Crate_State_Definitions_Open", &palette);
        assert_eq!(
            state.particle,
            Some([17, 34, 51]),
            "the definition inherits the container's ParticleColor"
        );

        let missing = expect_color("No_File_Block", &palette);
        assert_eq!(missing, BlockColor::default(), "a missing name is white");

        std::fs::remove_dir_all(&dir).expect("the fixture tree is removable");
    }

    #[test]
    fn palette_tool_output_is_byte_deterministic() {
        let dir = fixture_dir("deterministic");
        synthetic_pack(&dir);
        let registries = synthetic_registries(&dir);

        let (first, _) =
            build_palette(&dir, Some(&registries), None).expect("the first build succeeds");
        let (second, _) =
            build_palette(&dir, Some(&registries), None).expect("the second build succeeds");
        let first = first.to_json_bytes().expect("the first palette serializes");
        let second = second
            .to_json_bytes()
            .expect("the second palette serializes");
        assert_eq!(first, second, "the same input yields byte-identical output");

        let text = String::from_utf8(first).expect("the palette is UTF-8 JSON");
        assert!(
            text.contains("\"schema\": 1"),
            "the schema version is written"
        );
        assert!(
            text.contains("\"pack_hash\": \"sha256:"),
            "the provenance hash is written"
        );
        assert!(
            text.contains("\"source\": \"hytale-assets\""),
            "the source is written"
        );

        // Entries are sorted by name: `*…` sorts before `Deco_Crate`, which
        // sorts before the rest. Quoted keys avoid matching the starred key.
        let positions: Vec<usize> = [
            "\"*Deco_Crate_State_Definitions_Open\": {",
            "\"Deco_Crate\": {",
            "\"No_File_Block\": {",
            "\"Rock_Stone\": {",
            "\"Soil_Child\": {",
            "\"Soil_Grass\": {",
        ]
        .iter()
        .map(|key| {
            text.find(key)
                .unwrap_or_else(|| panic!("the palette contains the entry {key}"))
        })
        .collect();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "entries are serialized in sorted name order: {positions:?}"
        );

        std::fs::remove_dir_all(&dir).expect("the fixture tree is removable");
    }

    #[test]
    fn palette_tool_fallback_fills_gaps() {
        let dir = fixture_dir("fallback");
        synthetic_pack(&dir);
        let registries = dir.join("registries.json");
        write_fixture(
            &registries,
            r#"{"blocks":{"assets":[
                {"name":"Soil_Grass"},
                {"name":"No_File_Block"},
                {"name":"Also_Missing"}
            ]}}"#,
        );
        let fallback = dir.join("fallback.json");
        write_fixture(
            &fallback,
            r#"{"schema":1,"source":"fallback","generated_by":"test",
                "entries":{"No_File_Block":{"rgb":[1,2,3],"biome_tint":0.0}}}"#,
        );

        let (palette, coverage) = build_palette(&dir, Some(&registries), Some(&fallback))
            .expect("the fallback build succeeds");

        assert_eq!(coverage.pack, 1, "Soil_Grass still comes from the pack");
        assert_eq!(coverage.fallback, 1, "the fallback fills its one name");
        assert_eq!(coverage.defaulted, 1, "the uncovered name stays white");

        let filled = expect_color("No_File_Block", &palette);
        assert_eq!(filled.rgb, [1, 2, 3], "the fallback color fills the gap");
        let grass = expect_color("Soil_Grass", &palette);
        assert_eq!(grass.rgb, [103, 182, 45], "pack data is not overridden");
        let missing = expect_color("Also_Missing", &palette);
        assert_eq!(
            missing,
            BlockColor::default(),
            "a name in neither source is white"
        );

        std::fs::remove_dir_all(&dir).expect("the fixture tree is removable");
    }

    #[test]
    fn palette_tool_rejects_missing_pack() {
        let dir = fixture_dir("missing");
        let missing = dir.join("not-a-pack");
        let error =
            build_palette(&missing, None, None).expect_err("a nonexistent pack is an error");
        assert!(
            error.contains(&missing.display().to_string()),
            "the error names the missing path: {error}"
        );

        std::fs::remove_dir_all(&dir).expect("the fixture tree is removable");
    }

    /// The real pack and its registries snapshot, or `None` with the usual
    /// skip message on machines without the sibling checkout.
    fn real_pack() -> Option<(PathBuf, PathBuf)> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hytale-server-rust");
        let pack = root.join("Assets/extracted");
        if !pack.is_dir() {
            eprintln!(
                "skipping: the worldgen checkout has no asset pack at {}",
                pack.display()
            );
            return None;
        }
        let registries = root.join("testdata/golden/registries.json");
        if !registries.is_file() {
            eprintln!(
                "skipping: the worldgen checkout has no registries snapshot at {}",
                registries.display()
            );
            return None;
        }
        Some((pack, registries))
    }

    #[test]
    fn palette_tool_real_pack_matches_the_recorded_probe_values() {
        let Some((pack, registries)) = real_pack() else {
            return;
        };

        let (first, coverage) =
            build_palette(&pack, Some(&registries), None).expect("the real pack builds a palette");
        let (second, _) =
            build_palette(&pack, Some(&registries), None).expect("the real pack rebuilds");
        let first_bytes = first.to_json_bytes().expect("the real palette serializes");
        let second_bytes = second
            .to_json_bytes()
            .expect("the real palette serializes again");
        assert_eq!(
            first_bytes, second_bytes,
            "the real pack build is byte-identical across runs"
        );

        // The §6 recorded probe values (docs/assets-and-palette.md).
        let grass = expect_color("Soil_Grass", &first);
        assert_eq!(grass.rgb, [103, 182, 45], "Soil_Grass TintUp #67b62d");
        assert_eq!(grass.biome_tint, 1.0, "Soil_Grass BiomeTintUp 100");
        assert_eq!(
            expect_color("Soil_Sand", &first).particle,
            Some([249, 203, 87]),
            "Soil_Sand ParticleColor #f9cb57"
        );
        assert_eq!(
            expect_color("Rock_Stone", &first).particle,
            Some([115, 112, 85]),
            "Rock_Stone ParticleColor #737055"
        );
        assert_eq!(
            expect_color("Soil_Snow", &first).particle,
            Some([221, 240, 244]),
            "Soil_Snow ParticleColor #ddf0f4"
        );

        // Coverage is anchored on the registries: every plain name is present
        // (the four preloaded technical blocks have no item file and take the
        // white default).
        let registry_json: Value = serde_json::from_str(
            &std::fs::read_to_string(&registries).expect("the snapshot reads"),
        )
        .expect("the snapshot is JSON");
        let names = registry_json
            .get("blocks")
            .and_then(|blocks| blocks.get("assets"))
            .and_then(Value::as_array)
            .expect("the snapshot has blocks.assets");
        let plain: Vec<&str> = names
            .iter()
            .filter_map(|asset| asset.get("name").and_then(Value::as_str))
            .filter(|name| !name.starts_with('*'))
            .collect();
        assert!(!plain.is_empty(), "the snapshot lists block names");
        for name in &plain {
            assert!(
                first.get(name).is_some(),
                "every plain registries name has an entry: {name}"
            );
        }
        assert_eq!(
            coverage.total,
            names.len(),
            "every registries name (plain and starred) gets an entry"
        );
        eprintln!(
            "real pack: {} entries, {} from the pack, {} white defaults, {}",
            coverage.total, coverage.pack, coverage.defaulted, coverage.pack_hash
        );
    }

    #[derive(clap::Parser)]
    struct PaletteCli {
        #[command(subcommand)]
        command: PaletteCommand,
    }

    #[derive(clap::Subcommand)]
    enum PaletteCommand {
        Palette(PaletteArgs),
    }

    #[test]
    fn palette_tool_args_parse() {
        let cli = PaletteCli::try_parse_from([
            "map-cli",
            "palette",
            "--input",
            "/tmp/pack",
            "--out",
            "/tmp/palette.json",
            "--allow-commit",
        ])
        .expect("the palette arguments parse");
        let PaletteCommand::Palette(args) = cli.command;
        assert_eq!(args.input, PathBuf::from("/tmp/pack"));
        assert_eq!(args.out, PathBuf::from("/tmp/palette.json"));
        assert!(args.registries.is_none(), "registries is optional");
        assert!(args.fallback.is_none(), "fallback is optional");
        assert!(args.allow_commit, "--allow-commit is a flag");

        // Omitted options take the documented defaults.
        let cli =
            PaletteCli::try_parse_from(["map-cli", "palette"]).expect("the palette defaults parse");
        let PaletteCommand::Palette(args) = cli.command;
        assert_eq!(args.input, PathBuf::from("Assets/extracted"));
        assert_eq!(args.out, PathBuf::from("target/palette.json"));
        assert!(args.registries.is_none());
        assert!(args.fallback.is_none());
        assert!(!args.allow_commit);
    }
}
