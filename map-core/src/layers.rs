//! The planned layer stack and its one compositing primitive (T19).
//!
//! The stack is session state: the full set of planned layers
//! ([`Layer`], from bottom to top) with an enabled flag, an opacity and a
//! paint order each, kept sorted bottom → top. It survives a source
//! reload and serde round-trips it for T31's persistence; the settings
//! are the app's only source of truth for what the frame paints.
//!
//! **Opacity is paint-time, never baked into tiles.** A tile's key
//! ([`crate::tiles::TileKey`]) carries the layer, seed and generator
//! identity but not the opacity: the same rendered pixels are reused at
//! every slider position, and the blend happens when the frame (or an
//! export) draws them. That keeps the cache small and, more importantly,
//! makes the blend independent of the tile pipeline.
//!
//! [`blend_over`] is therefore THE compositing primitive: the app's
//! screen path applies it to each pixel exactly as an export does, so a
//! written PNG and the window agree by construction. T32 must reuse this
//! function (in the app's case through the identical per-pixel math),
//! never reimplement the blend.
//!
//! Task ownership: [`LayerStack`]/[`LayerSettings`] and [`blend_over`]
//! land with T19; T31 persists the stack, T32 composites with
//! [`blend_over`].

use serde::{Deserialize, Serialize};

use crate::render::Layer;

/// The planned paint order, bottom → top, which is also the default
/// `order` value of each entry (its index).
const DEFAULT_ORDER: [Layer; 8] = [
    Layer::Biome,
    Layer::Height,
    Layer::Relief,
    Layer::Fluid,
    Layer::Surface,
    Layer::CaveSlice,
    Layer::CaveDensity,
    Layer::Zone,
];

/// One planned layer's settings. Order is the stack's paint order
/// (bottom → top).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayerSettings {
    /// The layer these settings drive.
    pub id: Layer,
    /// Whether the frame paints (and requests tiles for) the layer.
    pub enabled: bool,
    /// Paint-time opacity in 0..=1 (clamped on every write).
    pub opacity: f32,
    /// Paint order, ascending: lower entries are painted first.
    pub order: i16,
}

impl LayerSettings {
    /// The default for `id` at `order`: disabled at full opacity, except
    /// the biome layer, which the plan's default shows alone.
    fn default_for(id: Layer, order: i16) -> Self {
        Self {
            id,
            enabled: id == Layer::Biome,
            opacity: 1.0,
            order,
        }
    }
}

/// The planned layers with their settings, kept sorted by `order`
/// ascending (bottom → top).
///
/// The stack is session state: it survives source reloads, and serde
/// round-trips it for T31's persistence. `dirty` marks any settings
/// change since the last [`LayerStack::take_dirty`] — the app's frame
/// request follows the settings anyway; the flag is the persistence
/// hook T31 will drain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayerStack {
    settings: Vec<LayerSettings>,
    #[serde(skip)]
    dirty: bool,
}

impl LayerStack {
    /// The full planned set, bottom → top: Biome, Height, Relief,
    /// Fluid, Surface, CaveSlice, CaveDensity, Zone — the plan's
    /// default is Biome alone, enabled at full opacity; every other
    /// layer starts disabled at opacity 1.0.
    #[must_use]
    pub fn new() -> Self {
        let settings = DEFAULT_ORDER
            .iter()
            .enumerate()
            .map(|(index, &id)| {
                // The set is the plan's fixed eight layers, so the index
                // always fits i16.
                LayerSettings::default_for(id, i16::try_from(index).expect("eight layers fit i16"))
            })
            .collect();
        Self {
            settings,
            dirty: false,
        }
    }

    /// One layer's settings, when the stack plans it.
    #[must_use]
    pub fn get(&self, id: Layer) -> Option<&LayerSettings> {
        self.settings.iter().find(|settings| settings.id == id)
    }

    /// Every planned layer's settings, bottom → top.
    #[must_use = "the iterator yields the settings only when consumed"]
    pub fn iter(&self) -> impl Iterator<Item = &LayerSettings> {
        self.settings.iter()
    }

    /// The enabled layers, bottom → top — the frame's paint order.
    #[must_use]
    pub fn enabled_layers(&self) -> Vec<Layer> {
        self.settings
            .iter()
            .filter(|settings| settings.enabled)
            .map(|settings| settings.id)
            .collect()
    }

    /// One layer's opacity (1.0 when the stack does not plan it).
    #[must_use]
    pub fn opacity(&self, id: Layer) -> f32 {
        self.get(id).map_or(1.0, |settings| settings.opacity)
    }

    /// Enables/disables a layer; the return value says whether the
    /// setting changed (a no-op write does not dirty the stack).
    pub fn set_enabled(&mut self, id: Layer, enabled: bool) -> bool {
        // The set is fixed, so an id the stack does not plan is a silent
        // no-op rather than a growth point.
        let Some(settings) = self.settings.iter_mut().find(|settings| settings.id == id) else {
            return false;
        };
        if settings.enabled == enabled {
            return false;
        }
        settings.enabled = enabled;
        self.dirty = true;
        true
    }

    /// Sets a layer's opacity, clamped into 0..=1; the return value says
    /// whether the value changed.
    pub fn set_opacity(&mut self, id: Layer, opacity: f32) -> bool {
        let clamped = clamp_opacity(opacity);
        let Some(settings) = self.settings.iter_mut().find(|settings| settings.id == id) else {
            return false;
        };
        if settings.opacity == clamped {
            return false;
        }
        settings.opacity = clamped;
        self.dirty = true;
        true
    }

    /// Moves a layer to `new_order` in the paint order (re-sorting the
    /// stack; settings travel with their entries, nothing is lost).
    pub fn reorder(&mut self, id: Layer, new_order: i16) {
        let Some(settings) = self.settings.iter_mut().find(|settings| settings.id == id) else {
            return;
        };
        if settings.order == new_order {
            return;
        }
        settings.order = new_order;
        // Stable on purpose: equal orders keep their previous relative
        // position, so a collision never reshuffles unrelated entries.
        self.settings.sort_by_key(|settings| settings.order);
        self.dirty = true;
    }

    /// Whether any settings changed since the last take.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Takes the dirty flag (false afterwards).
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }
}

impl Default for LayerStack {
    fn default() -> Self {
        Self::new()
    }
}

/// Clamps a caller-supplied opacity into 0..=1.
///
/// `f32::clamp` propagates NaN, and a NaN opacity would poison both the
/// blend and the JSON persistence path; a corrupt slider value is
/// treated as fully transparent instead.
fn clamp_opacity(opacity: f32) -> f32 {
    if opacity.is_nan() {
        0.0
    } else {
        opacity.clamp(0.0, 1.0)
    }
}

/// Composites `top` over `base` with straight (non-premultiplied)
/// alpha, the top layer's alpha scaled by `opacity` — THE blending
/// primitive the app's egui tints and any export (T32) must share, so
/// the screen and a written PNG agree by construction.
///
/// `opacity` is clamped to 0..=1. A fully transparent top (or opacity
/// 0) returns `base` unchanged; a fully opaque top returns itself.
/// When the result's alpha is 0 the pixel is all zeroes.
#[must_use]
pub fn blend_over(base: [u8; 4], top: [u8; 4], opacity: f32) -> [u8; 4] {
    let opacity = clamp_opacity(opacity);
    let a_top = f32::from(top[3]) / 255.0 * opacity;
    let a_base = f32::from(base[3]) / 255.0;

    // The real-valued formula already yields these; short-circuiting
    // keeps them byte-exact (the division below can round a channel one
    // step off) and keeps a zero-alpha base all zeroes.
    if a_top == 0.0 {
        return if base[3] == 0 { [0, 0, 0, 0] } else { base };
    }
    if a_top == 1.0 {
        return top;
    }

    let a_out = a_top + a_base * (1.0 - a_top);
    if a_out <= 0.0 {
        return [0, 0, 0, 0];
    }

    let channel = |top_c: u8, base_c: u8| {
        let value = (f32::from(top_c) * a_top + f32::from(base_c) * a_base * (1.0 - a_top)) / a_out;
        value.round().clamp(0.0, 255.0) as u8
    };

    [
        channel(top[0], base[0]),
        channel(top[1], base[1]),
        channel(top[2], base[2]),
        (a_out * 255.0).round().clamp(0.0, 255.0) as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_stack_plans_every_layer_in_strict_order() {
        // The full planned set, bottom → top, with strictly increasing
        // orders: the stack's invariant is the paint order, not the
        // enum's declaration order.
        let stack = LayerStack::new();
        let planned: Vec<(Layer, i16)> = stack.iter().map(|s| (s.id, s.order)).collect();
        let expected: Vec<(Layer, i16)> = DEFAULT_ORDER
            .iter()
            .enumerate()
            .map(|(index, &id)| (id, i16::try_from(index).expect("eight layers fit i16")))
            .collect();
        assert_eq!(planned, expected, "every planned layer is present in order");
        for pair in planned.windows(2) {
            assert!(
                pair[0].1 < pair[1].1,
                "orders strictly increase bottom → top: {:?} then {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn opacity_and_get_cover_every_planned_layer() {
        // All eight layers are planned, so `get` is Some for each variant
        // and the untouched opacity reads back as 1.0 from both accessors.
        let stack = LayerStack::new();
        for id in DEFAULT_ORDER {
            assert!(
                stack.get(id).is_some(),
                "{id} must be planned by the default stack"
            );
            assert_eq!(stack.opacity(id), 1.0, "{id} starts at full opacity");
        }
        assert_eq!(stack.opacity(Layer::Zone), 1.0);
    }

    #[test]
    fn blend_over_zero_alpha_result_is_all_zeroes() {
        // Both sides invisible: straight-alpha math leaves a_out at 0,
        // and the pixel is normalized to all zeroes rather than keeping
        // a stale color under zero alpha.
        assert_eq!(blend_over([9, 9, 9, 0], [7, 7, 7, 0], 1.0), [0, 0, 0, 0]);
        assert_eq!(blend_over([0, 0, 0, 0], [0, 0, 0, 0], 1.0), [0, 0, 0, 0]);
    }
}
