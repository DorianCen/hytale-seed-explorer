//! Layer renderers: grids in, RGBA pixels out (T11 and successors).
//!
//! The first one — the flat biome layer — lives in [`biome`], along with
//! the optional subtle hillshade hook (disabled by default; T17 wires
//! heights in). [`image`] owns the pixel-buffer contract and the
//! deterministic PNG encoder. The later layers (height, relief, surface
//! blocks, caves: T17, T19–T21, T28) plug into the [`LayerRenderer`]
//! shape. Everything here is headless: pure functions over map-core's
//! own grid types, testable without a GPU (the golden test
//! `tests/render_golden.rs` runs the real generator and hashes the PNG).
//!
//! Color rules for the biome layer (the plan's first golden, kept flat
//! on purpose — the hillshade hook is opt-in):
//!
//! - [`BiomeInfo::map_color_rgb`] — the generator's own
//!   `Biome::get_map_color()`, channel order fixed by the T08 catalog;
//! - alpha 255 everywhere;
//! - an id missing from the catalog renders magenta
//!   ([`UNKNOWN_BIOME_COLOR`]) and is logged once, so missing data is
//!   visible, never silent.

pub mod biome;
pub mod image;

use std::fmt;
use std::str::FromStr;

use crate::error::{SourceError, SourceResult};

pub use biome::{BiomeRenderer, UNKNOWN_BIOME_COLOR, biome_pixel, render_biome_rgba};
pub use image::{RgbaImage, encode_png};

/// A renderable layer. Tile keys (T14) and the CLI `--layer` flag carry
/// this; only the biome layer exists yet — T17/T20/T28 extend the enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Layer {
    /// The flat biome map color layer.
    Biome,
}

impl fmt::Display for Layer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Layer::Biome => "biome",
        })
    }
}

impl FromStr for Layer {
    type Err = SourceError;

    /// Parses the layer names the CLI and share strings use. An unknown
    /// name is [`SourceError::Unsupported`], so the caller reports it the
    /// same way as any other unsupported query.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "biome" => Ok(Layer::Biome),
            _ => Err(SourceError::Unsupported(
                "unknown layer: only 'biome' exists (T17/T20/T28 add more)",
            )),
        }
    }
}

/// Renders one layer's grid data into a fresh [`RgbaImage`].
///
/// Minimal T11 shape: renderers own nothing (their inputs are borrowed),
/// dimensions come from the grid itself, and every renderer produces
/// exactly one pixel per sample. Later compositing tasks (T19) build on
/// this; if they need a richer contract the trait grows with them.
pub trait LayerRenderer {
    /// The grid type this layer consumes (e.g. [`crate::types::BiomeGrid`]).
    type Input;

    /// Renders `input` into an RGBA image with one pixel per sample,
    /// row-major.
    fn render(&self, input: &Self::Input) -> SourceResult<RgbaImage>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_parses_and_displays_its_cli_names() {
        assert_eq!("biome".parse::<Layer>(), Ok(Layer::Biome));
        assert_eq!(Layer::Biome.to_string(), "biome");
        // Unknown names are typed errors, not panics.
        assert!(matches!(
            "height".parse::<Layer>(),
            Err(SourceError::Unsupported(_))
        ));
    }
}
