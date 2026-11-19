//! The block palette: the block-id → color data full-block mode draws with
//! (T20; the renderer that consumes it is T21).
//!
//! **Where the colors come from.** A palette's real entries are *the user's
//! own pack data*, derived at runtime by `map-cli palette` from the asset
//! tree the user supplies and cached on their machine — that file is never
//! committed (proprietary assets; see `docs/assets-and-palette.md` §2). The
//! committed fallback shipped in `assets/palette/fallback.json` is a small
//! table of hand-picked **original approximations** (§2 rule 3, §8.2), so the
//! app works without any pack. It is approximate by construction, and the UI
//! says so when the fallback is active (§8, T21/T23).
//!
//! **Keying.** Palette entries are keyed by block **name** (`Soil_Sand`, …),
//! never by id: numeric block ids are schedule-dependent and differ between
//! packs and even between decodes of the same pack, while names are stable
//! (§4). Ids are resolved at draw time through the source's own catalog
//! ([`BlockCatalog`](crate::catalog::BlockCatalog)), so a palette file is
//! independent of any registry snapshot and no id is ever persisted.
//!
//! **Unknown blocks are visible, never transparent** (§8.3): a catalogued
//! name with no palette entry gets [`unknown_color`] — neutral grey plus a
//! deterministic hue from the name — so modded blocks still read apart; only
//! an id the catalog cannot name at all gets [`UNKNOWN_BLOCK_COLOR`] magenta
//! ("no metadata at all").
//!
//! **The color formula** is `map_image.rs`'s `block_color` (§3), the proven
//! in-game-map path, reproduced here rather than re-invented: per channel,
//! `c = self + (tint - self) * m` with `m = BiomeTintUp / 100`, then — only
//! while `m < 1.0` — a `ParticleColor` multiply by `/255`; alpha is always
//! opaque 255. The base color is what the `TintUp`/`Tint` field parses to
//! (white when absent), the biome tint is a runtime, position-dependent value
//! the renderer supplies.
//!
//! **Versioning.** [`PALETTE_VERSION`] is the cache-invalidation knob carried
//! in `GeneratorIdentity::palette_version`:
//! bump it when the *committed fallback data* changes — a bump is a cache
//! invalidation recorded in the commit message, the same policy as
//! [`GENERATOR_VERSION`](crate::GENERATOR_VERSION). The user's pack changing
//! does not bump it; that is covered by the file's `pack_hash`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::catalog::BlockCatalog;
use crate::types::BlockId;

/// Palette data version: bumped when the committed fallback palette's
/// data changes. The bump is a cache-invalidation action recorded in
/// the commit message; the worldgen identity carries it so tiles keyed
/// under the old version never serve (task item 5).
pub const PALETTE_VERSION: u32 = 1;

/// The committed fallback's provenance string.
pub const FALLBACK_SOURCE: &str = "fallback";

/// The magenta for "no metadata at all" (a block id the catalog cannot
/// even name): visible, never transparent.
pub const UNKNOWN_BLOCK_COLOR: [u8; 4] = [255, 0, 255, 255];

/// One block's color ingredients — exactly the fields map_image.rs reads.
///
/// `rgb` is the `TintUp`/`Tint` base (default white); `biome_tint` is
/// `BiomeTintUp / 100` (1.0 → the runtime biome tint wins outright);
/// `particle` is the `ParticleColor` multiply of the `m < 1` path.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BlockColor {
    /// The block's base (tint) color, parsed from `TintUp`/`Tint`; white
    /// when the pack declares neither.
    pub rgb: [u8; 3],
    /// `BiomeTintUp / 100`, in `0.0..=1.0`: how much of the runtime biome
    /// tint replaces the base color (`1.0` → the base is irrelevant).
    #[serde(default)]
    pub biome_tint: f32,
    /// `ParticleColor`, the extra multiply applied only while
    /// `biome_tint < 1.0`; absent for entries the pack has no particle
    /// color for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub particle: Option<[u8; 3]>,
}

impl Default for BlockColor {
    fn default() -> Self {
        Self {
            rgb: [255, 255, 255],
            biome_tint: 0.0,
            particle: None,
        }
    }
}

/// The palette file (schema 1), name-keyed and sorted for deterministic
/// bytes. Ids are NEVER persisted (docs §4); provenance travels in the
/// header, `pack_hash` absent only for the hand-picked fallback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaletteFile {
    /// The file schema version; a reader must refuse anything it does not
    /// understand rather than guess at entry shapes.
    pub schema: u32,
    /// Provenance, e.g. `"hytale-assets"` for a pack-derived file or
    /// `"fallback"` for the committed hand-picked table.
    pub source: String,
    /// `"sha256:<hex>"` over the input manifest(s) for pack-derived files;
    /// absent for the fallback, whose provenance is this repository itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack_hash: Option<String>,
    /// Who wrote the file, e.g. `"map-cli palette 0.1.0"`.
    pub generated_by: String,
    /// The entries themselves, keyed by block name. A `BTreeMap` so the
    /// serialized order is sorted and therefore byte-stable.
    pub entries: BTreeMap<String, BlockColor>,
}

/// Palette loading failures, all user-readable.
#[derive(Debug, thiserror::Error)]
pub enum PaletteError {
    /// The file could not be read (missing, unreadable, not text).
    #[error("palette read error: {0}")]
    Io(#[from] std::io::Error),
    /// The file is not the expected JSON shape.
    #[error("palette JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// The file declares a schema this build does not read.
    #[error(
        "palette schema mismatch: the file declares {found}, this build reads {}",
        BlockPalette::SCHEMA
    )]
    Schema { found: u32 },
}

/// The block palette: name-keyed entries plus the pure color policy.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockPalette {
    entries: BTreeMap<String, BlockColor>,
    /// Provenance copied into [`BlockPalette::to_json_bytes`]; starts as an
    /// unprovenanced placeholder until [`BlockPalette::with_header`] sets it.
    source: String,
    pack_hash: Option<String>,
    generated_by: String,
}

impl BlockPalette {
    /// The supported schema version.
    pub const SCHEMA: u32 = 1;

    /// The committed fallback: hand-picked, approximate, original
    /// values (docs §2 §8) — the app works without any asset pack.
    ///
    /// The data lives in `assets/palette/fallback.json` and is compiled in;
    /// a file that does not parse or declares another schema is a build bug,
    /// so this panics with an explanatory message rather than degrading at
    /// runtime.
    #[must_use]
    pub fn fallback() -> Self {
        let file: PaletteFile = serde_json::from_str(include_str!(
            "../../assets/palette/fallback.json"
        ))
        .expect("the committed fallback palette is valid schema-1 JSON (a build bug otherwise)");
        Self::from_file(file).expect(
            "the committed fallback palette declares the supported schema (a build bug otherwise)",
        )
    }

    /// Loads a palette file, refusing schema mismatches loudly (a wrong
    /// schema means wrong ids/names — never a silent color change).
    pub fn load(path: &Path) -> Result<Self, PaletteError> {
        let text = std::fs::read_to_string(path)?;
        let file: PaletteFile = serde_json::from_str(&text)?;
        Self::from_file(file)
    }

    /// Wraps a parsed file, enforcing the schema before anything reads it.
    fn from_file(file: PaletteFile) -> Result<Self, PaletteError> {
        if file.schema != Self::SCHEMA {
            return Err(PaletteError::Schema { found: file.schema });
        }
        Ok(Self {
            entries: file.entries,
            source: file.source,
            pack_hash: file.pack_hash,
            generated_by: file.generated_by,
        })
    }

    /// Builds a palette over already-parsed entries (the tool's path).
    ///
    /// The header starts as an unprovenanced placeholder; the palette tool
    /// follows up with [`BlockPalette::with_header`] so the emitted file
    /// carries its real source and pack hash.
    #[must_use]
    pub fn from_entries(entries: BTreeMap<String, BlockColor>) -> Self {
        Self {
            entries,
            source: String::from("unnamed"),
            pack_hash: None,
            generated_by: String::from("unnamed (no header set)"),
        }
    }

    /// The header a `to_json_bytes` output carries (provenance for the
    /// generated file; the fallback's is `fallback`/no hash).
    #[must_use]
    pub fn with_header(
        mut self,
        source: &str,
        pack_hash: Option<&str>,
        generated_by: &str,
    ) -> Self {
        self.source = source.to_string();
        self.pack_hash = pack_hash.map(str::to_string);
        self.generated_by = generated_by.to_string();
        self
    }

    /// One block's ingredients, by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&BlockColor> {
        self.entries.get(name)
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the palette has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Serializes deterministically: sorted (BTreeMap) entries, stable
    /// key order, one JSON document — the same palette is byte-identical
    /// every time (an acceptance row).
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, PaletteError> {
        let file = PaletteFile {
            schema: Self::SCHEMA,
            source: self.source.clone(),
            pack_hash: self.pack_hash.clone(),
            generated_by: self.generated_by.clone(),
            entries: self.entries.clone(),
        };
        Ok(serde_json::to_vec_pretty(&file)?)
    }

    /// The full color policy for one block id, resolved through the
    /// source's own catalog (the generator's registry names — ids are
    /// schedule-dependent, docs §4):
    ///
    /// 1. id → name → entry → [`block_color`] with the runtime tint;
    /// 2. a name the catalog knows but the palette lacks →
    ///    [`unknown_color`] (grey + deterministic hue — visible, not
    ///    transparent, modded blocks stay distinct);
    /// 3. an id the catalog cannot name at all → [`UNKNOWN_BLOCK_COLOR`].
    #[must_use]
    pub fn color(&self, id: BlockId, biome_tint_rgb: [u8; 3], catalog: &BlockCatalog) -> [u8; 4] {
        match catalog.get(id) {
            Some(name) => self.get(name).map_or_else(
                || unknown_color(name),
                |color| block_color(color, biome_tint_rgb),
            ),
            None => UNKNOWN_BLOCK_COLOR,
        }
    }
}

/// map_image.rs `block_color`: the exact formula (see the module docs),
/// pure. Alpha 255.
///
/// The two steps are order-sensitive and copied literally from the server
/// reference: the biome blend uses a truncating `as u8` cast (saturating at
/// 0/255), and the particle multiply is integer `/255`, applied only while
/// `m < 1.0` — at full biome tint the base and particle colors are moot.
#[must_use]
pub fn block_color(color: &BlockColor, biome_tint_rgb: [u8; 3]) -> [u8; 4] {
    let m = f64::from(color.biome_tint);
    let blend = |self_c: u8, tint_c: u8| -> u8 {
        (f64::from(self_c) + (f64::from(tint_c) - f64::from(self_c)) * m) as u8
    };
    let mut rgb = [
        blend(color.rgb[0], biome_tint_rgb[0]),
        blend(color.rgb[1], biome_tint_rgb[1]),
        blend(color.rgb[2], biome_tint_rgb[2]),
    ];

    if color.biome_tint < 1.0
        && let Some(particle) = color.particle
    {
        for (channel, &particle_c) in rgb.iter_mut().zip(&particle) {
            *channel = (u16::from(*channel) * u16::from(particle_c) / 255) as u8;
        }
    }

    [rgb[0], rgb[1], rgb[2], 255]
}

/// The unknown-block policy: a block the palette has no entry for is
/// visible — neutral grey `#8a8a8a` shifted by a deterministic hue from
/// the Java string hash of its name (`java_string_hash_code(name) & 0x3F`),
/// so distinct modded blocks read apart without any metadata. Alpha 255.
///
/// The shift keeps every channel in `80..=206` (a readable, never
/// near-black or near-white band): one channel — chosen by `hue % 3` — gets
/// `80 + 2 * hue`, the other two stay grey. This is a debug-visibility
/// scheme, not a color anyone should mistake for a real block color.
#[must_use]
pub fn unknown_color(name: &str) -> [u8; 4] {
    let hue = (java_string_hash_code(name) as u32) & 0x3F;
    let mut rgb = [138_u8; 3];
    rgb[(hue % 3) as usize] = (80 + hue * 2) as u8;
    [rgb[0], rgb[1], rgb[2], 255]
}

/// Parses one pack-JSON color value (string or first array element) with
/// the `#`/`0x` prefix forms, 3/6/8-digit rules — the tool's parser,
/// exported for its tests and for the fixture comparisons.
///
/// The accepted shapes are exactly `docs/assets-and-palette.md` §3: a string
/// with an optional leading `#` or `0x` (case-insensitive), three digits
/// expanding to doubled digits, six digits taken directly, eight digits
/// taking the **last** six. Anything else — another length, a non-string
/// value, a bad hex digit — is `None`, never a guess.
#[must_use]
pub fn parse_color(value: Option<&serde_json::Value>) -> Option<[u8; 3]> {
    let value = value?;
    let text = match value {
        serde_json::Value::String(text) => text.as_str(),
        serde_json::Value::Array(items) => items.first()?.as_str()?,
        _ => return None,
    };

    let digits = text
        .strip_prefix('#')
        .or_else(|| text.strip_prefix("0x"))
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    let bytes = digits.as_bytes();
    match bytes.len() {
        3 => {
            let r = hex_digit(bytes[0])?;
            let g = hex_digit(bytes[1])?;
            let b = hex_digit(bytes[2])?;
            Some([r * 17, g * 17, b * 17])
        }
        6 => Some([
            hex_pair(bytes[0], bytes[1])?,
            hex_pair(bytes[2], bytes[3])?,
            hex_pair(bytes[4], bytes[5])?,
        ]),
        // 8-digit values are ARGB in the legacy configs: the alpha byte is
        // dropped, the last six digits carry the color.
        8 => Some([
            hex_pair(bytes[2], bytes[3])?,
            hex_pair(bytes[4], bytes[5])?,
            hex_pair(bytes[6], bytes[7])?,
        ]),
        _ => None,
    }
}

/// One hex digit's value, or `None` for a non-hex byte.
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Two hex digits as one channel value.
fn hex_pair(high: u8, low: u8) -> Option<u8> {
    Some(hex_digit(high)? << 4 | hex_digit(low)?)
}

/// Java `String.hashCode`: `(31 * h + char) as i32` wrapping. The
/// unknown-block hue derives from it because that is the value every
/// Java-side tooling already produces for a name.
#[must_use]
pub fn java_string_hash_code(name: &str) -> i32 {
    // UTF-16 code units, like Java's `char`, so non-BMP names hash the same
    // way they do on the server side.
    name.encode_utf16().fold(0_i32, |hash, unit| {
        hash.wrapping_mul(31).wrapping_add(i32::from(unit))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_string_hash_code_matches_java() {
        // Java: `"Soil_Grass".hashCode()`. Hand-computed as
        // 83, 2684, 83309, 2582687, 80063392, 2481965223 (wraps to
        // -368489301), 1461733654, 2364070429, 271739382 — then the
        // final `(31 * h + 115)` wrapped into i32.
        assert_eq!(java_string_hash_code("Soil_Grass"), 271_739_382);
        // The empty string is the identity element of the fold.
        assert_eq!(java_string_hash_code(""), 0);
    }

    #[test]
    fn block_color_serde_defaults_fill_the_optional_fields() {
        let color: BlockColor =
            serde_json::from_str(r#"{"rgb":[1,2,3]}"#).expect("rgb is the only required field");
        assert_eq!(
            color,
            BlockColor {
                rgb: [1, 2, 3],
                biome_tint: 0.0,
                particle: None,
            }
        );
    }
}
