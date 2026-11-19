//! The zone debug layer (T19): one flat color per generator zone.
//!
//! Zones are worldgen's coarse regions ("Emerald Grove", "Howling Sands"),
//! so the layer exists to make the region grammar visible, not to depict
//! terrain. Each zone is colored from its NAME — the user-visible identity,
//! where ids may be renumbered between generator versions — by a stable
//! FNV-1a hash spread over the hue wheel. Lightness and saturation are
//! fixed, so the layer stays flat (a debug overlay, not a shaded map).
//!
//! Data gaps: a sample the generator reports no zone for stays fully
//! transparent (no zone is no data; the layers below show through), and an
//! id the catalog cannot resolve still gets an honest hash color from a
//! `zone#<id>` fallback name — visible, never silent, but without the
//! biome layer's magenta alarm (a missing zone is a catalog gap, not a
//! generation bug).

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use crate::catalog::ZoneCatalog;
use crate::error::{SourceError, SourceResult};
use crate::render::tiles::render_layer_tile;
use crate::scheduler::{CancelToken, TileRenderer};
use crate::source::WorldSource;
use crate::tiles::{Tile, TileKey};
use crate::types::{Rect, ZoneGrid};

use super::image::{Rgba, RgbaImage};

/// The layer's fixed saturation and lightness: every zone lands in the
/// same readable band (roughly 104..=211 per channel) whatever its hue.
const ZONE_SATURATION: f32 = 0.55;
const ZONE_LIGHTNESS: f32 = 0.62;

/// 32-bit FNV-1a over the name's bytes.
///
/// Chosen over [`std::collections::hash_map::DefaultHasher`] because the
/// layer's pixels are content-addressed: the same zone name must keep its
/// color across builds, platforms and runs, which `DefaultHasher`
/// explicitly does not promise.
fn fnv1a(bytes: &[u8]) -> u32 {
    const OFFSET_BASIS: u32 = 0x811c_9dc5;
    const PRIME: u32 = 0x0100_0193;
    let mut hash = OFFSET_BASIS;
    for &byte in bytes {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// The classic HSL hue → RGB conversion, hue in degrees (`[0, 360)`).
///
/// `c = (1 - |2l - 1|) * s` is the chroma, `x = c * (1 - |(h/60) mod 2 -
/// 1|)` the second-largest channel, `m = l - c/2` the offset that lifts
/// the result to the lightness. The sector index (`0..=5`) picks the
/// channel permutation. Local on purpose: the layer needs exactly one
/// formula, not a color crate.
fn hue_to_rgb(hue: f32, saturation: f32, lightness: f32) -> [u8; 3] {
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let sector = hue / 60.0;
    let second = chroma * (1.0 - (sector % 2.0 - 1.0).abs());
    let (r, g, b) = match sector as u32 {
        0 => (chroma, second, 0.0),
        1 => (second, chroma, 0.0),
        2 => (0.0, chroma, second),
        3 => (0.0, second, chroma),
        4 => (second, 0.0, chroma),
        // Sector 5 (300..360 degrees); `hue` never reaches 360, so the
        // index is always 0..=5.
        _ => (chroma, 0.0, second),
    };
    let offset = lightness - chroma / 2.0;
    // The lightness/saturation constants keep every channel inside
    // 0..=255; the clamp proves the range before the narrowing cast.
    let channel = |value: f32| ((value + offset) * 255.0).round().clamp(0.0, 255.0) as u8;
    [channel(r), channel(g), channel(b)]
}

/// The debug color of a zone: a stable hash of its NAME (names are the
/// user-visible identity; ids may be renumbered between generator
/// versions). Deterministic pure function, spread over the hue wheel so
/// neighbouring zones read apart, lightness/saturation fixed so the layer
/// stays flat (no shading — it is a debug overlay).
#[must_use]
pub fn zone_color(name: &str) -> [u8; 3] {
    let hue = (fnv1a(name.as_bytes()) % 360) as f32;
    hue_to_rgb(hue, ZONE_SATURATION, ZONE_LIGHTNESS)
}

/// Colors `grid` one pixel per sample, row-major (north at image row 0),
/// `cancel` checked every 16 rows like the other layers:
///
/// - `Some(id)` known to the catalog → [`zone_color`] of its name, alpha 255;
/// - `Some(id)` missing from the catalog → [`zone_color`] of its
///   `zone#<id>` fallback name, alpha 255 (a missing id never panics and
///   never disappears — mirroring the biome layer's "visible, never
///   silent" rule, but any hash is honest here, so no alarm color);
/// - `None` → transparent (alpha 0): no zone is no data, the layers below
///   show through.
pub fn zone_image(
    grid: &ZoneGrid,
    catalog: &ZoneCatalog,
    cancel: &CancelToken,
) -> SourceResult<RgbaImage> {
    let width = grid.width as usize;
    let mut image = RgbaImage::new(grid.width, grid.height);
    for y in 0..grid.height {
        if y.is_multiple_of(16) && cancel.load(Relaxed) {
            return Err(SourceError::Cancelled);
        }
        // The grid's row stride is `width`; `x` is a u32 sample index
        // (grids come from rects, far below usize::MAX).
        let row = (y as usize) * width;
        for x in 0..grid.width {
            let Some(id) = grid.zones[row + x as usize] else {
                // Zeroed is transparent: no zone reported for this column.
                image.put_pixel(x, y, Rgba([0, 0, 0, 0]));
                continue;
            };
            let [r, g, b] = match catalog.get(id) {
                Some(info) => zone_color(&info.name),
                None => zone_color(&format!("zone#{}", id.0)),
            };
            image.put_pixel(x, y, Rgba([r, g, b, 255]));
        }
    }
    Ok(image)
}

/// The zone layer's tile renderer (border 0), mirroring
/// [`crate::render::biome::BiomeTileRenderer`]: one tile is one batched
/// zone region query, colored with [`zone_image`]. Out-of-world margins
/// stay transparent (the shared skeleton).
pub struct ZoneTileRenderer {
    source: Arc<dyn WorldSource>,
    world: Option<Rect>,
}

impl ZoneTileRenderer {
    /// `world` is the generatable block rect (inclusive); `None` = unbounded.
    #[must_use]
    pub fn new(source: Arc<dyn WorldSource>, world: Option<Rect>) -> Self {
        Self { source, world }
    }
}

impl TileRenderer for ZoneTileRenderer {
    fn render(&self, key: &TileKey, cancel: &CancelToken) -> SourceResult<Tile> {
        // Border 0: a zone sample is independent of its neighbours.
        render_layer_tile(
            self.source.as_ref(),
            key,
            cancel,
            self.world,
            0,
            |source, rect, step| source.zone_region(rect, step),
            |grid, query, cancel| {
                zone_image(grid, self.source.zone_catalog(), cancel)
                    .map(|image| (image, query.origin))
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::AtomicBool;

    use crate::catalog::ZoneInfo;
    use crate::scheduler::cancel_token;
    use crate::types::ZoneId;

    #[test]
    fn zone_color_is_stable_and_spreads_names() {
        // Stable: the same name always maps to the same pixel.
        assert_eq!(zone_color("Emerald Grove"), zone_color("Emerald Grove"));

        // Spread: plausible zone names must read apart. Six distinct
        // colors out of eight leaves room for a hash collision without
        // letting a degenerate hash (everything one hue) pass.
        let names = [
            "Emerald Grove",
            "Desert",
            "Snow",
            "Ocean",
            "Jungle",
            "Savanna",
            "Tundra",
            "Swamp",
        ];
        let colors: Vec<[u8; 3]> = names.iter().map(|name| zone_color(name)).collect();
        let mut distinct = colors.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert!(
            distinct.len() >= 6,
            "eight zone names must produce at least six distinct colors, got {distinct:?}"
        );

        // Readable: never black or white, always a mid-tone channel.
        for (name, color) in names.iter().zip(&colors) {
            for (channel_index, &channel) in color.iter().enumerate() {
                assert!(
                    (30..=225).contains(&channel),
                    "{name:?} channel {channel_index} is {channel}, outside the readable 30..=225 band"
                );
            }
        }
    }

    #[test]
    fn zone_image_colors_known_ids_falls_back_on_unknown_and_keeps_none_transparent() {
        let mut catalog = ZoneCatalog::default();
        catalog
            .insert(ZoneInfo {
                id: ZoneId(1),
                name: "Known Zone".to_string(),
            })
            .expect("the test catalog starts empty");

        // Row-major: known id, unknown id, absent zone.
        let grid = ZoneGrid::new(0, 0, 1, 3, 1, vec![Some(ZoneId(1)), Some(ZoneId(99)), None]);
        let image = zone_image(&grid, &catalog, &cancel_token()).expect("an uncancelled image");

        let known = zone_color("Known Zone");
        assert_eq!(
            image.get_pixel(0, 0),
            &Rgba([known[0], known[1], known[2], 255]),
            "a catalogued id colors by its name"
        );
        let fallback = zone_color("zone#99");
        assert_eq!(
            image.get_pixel(1, 0),
            &Rgba([fallback[0], fallback[1], fallback[2], 255]),
            "an uncatalogued id colors by its honest fallback name"
        );
        assert_eq!(
            image.get_pixel(2, 0),
            &Rgba([0, 0, 0, 0]),
            "no zone is transparent, never a fabricated color"
        );

        let cancelled: CancelToken = Arc::new(AtomicBool::new(true));
        assert_eq!(
            zone_image(&grid, &catalog, &cancelled),
            Err(SourceError::Cancelled),
            "a cancelled image is a typed error, never a partial result"
        );
    }
}
