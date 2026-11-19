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
/// this.
///
/// The full planned set exists already so tile identity (and, later, T31
/// share strings) is frozen before the renderers arrive: only
/// [`Layer::Biome`] has a renderer today (T13); T17/T20/T28 wire the
/// rest. Parsing a name succeeds for every variant; a renderer-less one
/// is rejected where it is used, not where it is named.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Layer {
    /// The flat biome map color layer.
    Biome,
    /// Terrain surface height (T17).
    Height,
    /// Height-derived hillshade relief (T17).
    Relief,
    /// Topmost surface blocks (T20/T21).
    Surface,
    /// Fluids and their depth (T17).
    Fluid,
    /// One horizontal cave slice; the Y bucket travels in the tile
    /// key's `mode` (T28).
    CaveSlice,
    /// Cave-volume density heat map (T28).
    CaveDensity,
    /// Zone regions (T19-era; `zone_at` exists since T08).
    Zone,
}

impl fmt::Display for Layer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Layer::Biome => "biome",
            Layer::Height => "height",
            Layer::Relief => "relief",
            Layer::Surface => "surface",
            Layer::Fluid => "fluid",
            Layer::CaveSlice => "cave-slice",
            Layer::CaveDensity => "cave-density",
            Layer::Zone => "zone",
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
            "height" => Ok(Layer::Height),
            "relief" => Ok(Layer::Relief),
            "surface" => Ok(Layer::Surface),
            "fluid" => Ok(Layer::Fluid),
            "cave-slice" => Ok(Layer::CaveSlice),
            "cave-density" => Ok(Layer::CaveDensity),
            "zone" => Ok(Layer::Zone),
            _ => Err(SourceError::Unsupported(
                "unknown layer name (expected biome, height, relief, surface, \
                 fluid, cave-slice, cave-density or zone)",
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
        // Every planned layer parses and round-trips its name (tile keys
        // and share strings know the full set); renderers arrive with
        // T17/T20/T28.
        for (name, layer) in [
            ("biome", Layer::Biome),
            ("height", Layer::Height),
            ("relief", Layer::Relief),
            ("surface", Layer::Surface),
            ("fluid", Layer::Fluid),
            ("cave-slice", Layer::CaveSlice),
            ("cave-density", Layer::CaveDensity),
            ("zone", Layer::Zone),
        ] {
            assert_eq!(name.parse::<Layer>(), Ok(layer));
            assert_eq!(layer.to_string(), name);
        }
        // Unknown names are typed errors, not panics.
        assert!(matches!(
            "nope".parse::<Layer>(),
            Err(SourceError::Unsupported(_))
        ));
    }
}
