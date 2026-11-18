//! egui texture plumbing (T13): map-core's RGBA bytes in, one cached
//! texture out.
//!
//! The headless side stays free of egui (map-core returns
//! [`RgbaImage`]); this module is the only place pixels cross into the
//! GUI. One texture covers the current view — T14/T16 replace it with a
//! tiled cache.
//!
//! Filtering decision (the task asked for one): **NEAREST**. Each texel
//! is one `step`-block sample — the crisp cell look is the plan's
//! chunkbase-style 2×2 default, and LINEAR would smear biome borders
//! that are supposed to be exact.

use eframe::egui;
use map_core::render::RgbaImage;
use map_core::types::Rect;

/// The texture's name in egui's manager (one slot; replaced on upload).
const TEXTURE_NAME: &str = "map/biome";

/// Converts the headless RGBA buffer into egui's RAM image.
///
/// `from_rgba_unmultiplied` copies row-major — the same orientation
/// contract as [`RgbaImage`] (grid row 0 / north at image top), so no
/// flip happens here.
#[must_use]
pub fn to_color_image(image: &RgbaImage) -> egui::ColorImage {
    egui::ColorImage::from_rgba_unmultiplied(
        [image.width() as usize, image.height() as usize],
        image.as_raw(),
    )
}

/// The uploaded map texture plus the world it covers.
///
/// The egui [`egui::TextureHandle`] is app-side by design; this pairs it
/// with the world-space rect and sampling step it was rendered for, so
/// painting can map it back to the screen (and T16 can invalidate it when
/// the view moves).
pub struct MapTexture {
    handle: egui::TextureHandle,
    rect: Rect,
    step: u32,
}

impl MapTexture {
    /// Uploads `image` (rendered for `rect` at `step`) as a new
    /// NEAREST-filtered texture.
    pub fn upload(ctx: &egui::Context, image: &RgbaImage, rect: Rect, step: u32) -> MapTexture {
        let handle = ctx.load_texture(
            TEXTURE_NAME,
            to_color_image(image),
            egui::TextureOptions::NEAREST,
        );
        MapTexture { handle, rect, step }
    }

    /// The texture id for painting.
    #[must_use]
    pub fn id(&self) -> egui::TextureId {
        self.handle.id()
    }

    /// The world rect (inclusive) the texture covers.
    #[must_use]
    pub fn rect(&self) -> Rect {
        self.rect
    }

    /// The sampling step the texture was rendered at.
    #[allow(dead_code)] // reserved: T16 invalidates tiles when the step changes
    #[must_use]
    pub fn step(&self) -> u32 {
        self.step
    }
}

/// The screen rect the texture paints into, for the view
/// (`center`, `scale`) inside the map `area`.
///
/// The world→screen mapping is T10's: the view center sits at the
/// area's center, one block is `scale` points, and world Z increases
/// south (down the screen). The rect's bounds are *inclusive*, so the
/// pixel grid spans exactly `min ..= max + 1` blocks per axis.
#[must_use]
pub fn texture_screen_rect(
    texture: &MapTexture,
    center: (f64, f64),
    scale: f64,
    area: egui::Rect,
) -> egui::Rect {
    let to_screen = |world_x: f64, world_z: f64| -> egui::Pos2 {
        egui::pos2(
            ((world_x - center.0) * scale) as f32 + area.center().x,
            ((world_z - center.1) * scale) as f32 + area.center().y,
        )
    };
    let rect = texture.rect();
    let min = to_screen(f64::from(rect.min_x), f64::from(rect.min_z));
    let max = to_screen(f64::from(rect.max_x) + 1.0, f64::from(rect.max_z) + 1.0);
    egui::Rect::from_min_max(min, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_image() -> RgbaImage {
        use map_core::render::image::Rgba;

        let mut image = RgbaImage::new(2, 1);
        image.put_pixel(0, 0, Rgba([10, 20, 30, 255]));
        image.put_pixel(1, 0, Rgba([40, 50, 60, 255]));
        image
    }

    #[test]
    fn to_color_image_keeps_dimensions_pixels_and_orientation() {
        let image = sample_image();
        let color_image = to_color_image(&image);

        assert_eq!(color_image.size, [2, 1]);
        assert_eq!(color_image.pixels.len(), 2);
        // Row-major, no flip: the first raw pixel is the first Color32.
        assert_eq!(
            color_image.pixels[0],
            egui::Color32::from_rgb(10, 20, 30),
            "grid row 0 (north) stays at the image top"
        );
        assert_eq!(color_image.pixels[1], egui::Color32::from_rgb(40, 50, 60));
        // The byte-length invariant the conversion relies on (2 * 1 * 4).
        assert_eq!(image.as_raw().len(), 8);
    }

    #[test]
    fn texture_screen_rect_maps_world_to_screen_centered() {
        let ctx = egui::Context::default();
        let image = sample_image();
        // A 2x1-sample texture at step 2 covers 4x2 blocks: (0,0)..(3,1).
        let texture = MapTexture::upload(&ctx, &image, Rect::new(0, 0, 3, 1), 2);

        // A 100x100 map area, view centered on (0,0) at 2 points/block:
        // the texture's min corner sits at the area center, its size is
        // blocks * scale = 4*2 x 2*2.
        let area = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(100.0, 100.0));
        let screen = texture_screen_rect(&texture, (0.0, 0.0), 2.0, area);
        assert_eq!(screen.min, egui::pos2(50.0, 50.0));
        assert!((screen.width() - 8.0).abs() < 0.001);
        assert!((screen.height() - 4.0).abs() < 0.001);

        // A shifted center slides the texture left/up in screen space,
        // and north (min z) stays at the smaller y (image top).
        let screen = texture_screen_rect(&texture, (2.0, 1.0), 2.0, area);
        assert_eq!(screen.min, egui::pos2(50.0 - 4.0, 50.0 - 2.0));
        assert!((screen.width() - 8.0).abs() < 0.001);
    }
}
