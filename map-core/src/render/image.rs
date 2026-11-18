//! Pixel buffers and PNG encoding for the headless renderers (T11).
//!
//! The buffer type is [`RgbaImage`] — the workspace already depends on the
//! `image` crate, so pixels are stored in its standard 8-bit RGBA buffer
//! (row-major, `y = 0` at the top) instead of a parallel home-grown wrapper.
//!
//! PNG encoding lives here, on the `png` crate directly, so the output is
//! **deterministic by construction**: no timestamps, no encoder-dependent
//! auxiliary chunks, just the IHDR/IDAT/IEND skeleton. The golden-image
//! tests hash the exact bytes this module produces, and the CLI writes the
//! same bytes to disk.
//!
//! The map's screen-Y-down / world-Z-south convention is handled by the
//! painting side (T16), not here: renderers fill row-major `z = 0` first,
//! which is image row `y = 0`.

use crate::error::{SourceError, SourceResult};

/// The pixel buffer every layer renders into: 8-bit RGBA, row-major.
///
/// Re-exported from the `image` crate — the single buffer type shared by
/// renderers, the golden tests and PNG export. [`Rgba`] rides along so
/// renderers never import the image crate directly.
pub use image::{Rgba, RgbaImage};

/// Encodes `image` as a PNG, returning the exact bytes.
///
/// The encoder is configured once, here, with no auxiliary chunks; the same
/// image always encodes to the same bytes, which is what the golden test
/// (`tests/render_golden.rs`) hashes. Errors are [`SourceError::Internal`]:
/// encoding into memory can only fail through a bug, never through user
/// input.
pub fn encode_png(image: &RgbaImage) -> SourceResult<Vec<u8>> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, image.width(), image.height());
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|error| SourceError::Internal(format!("png header: {error}")))?;
        writer
            .write_image_data(image.as_raw())
            .map_err(|error| SourceError::Internal(format!("png pixels: {error}")))?;
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_by_two() -> RgbaImage {
        let mut image = RgbaImage::new(2, 2);
        image.put_pixel(0, 0, Rgba([1, 2, 3, 255]));
        image.put_pixel(1, 0, Rgba([4, 5, 6, 255]));
        image.put_pixel(0, 1, Rgba([7, 8, 9, 255]));
        image.put_pixel(1, 1, Rgba([10, 11, 12, 255]));
        image
    }

    #[test]
    fn rgba_image_dimensions_and_indexing_are_row_major() {
        let image = two_by_two();
        assert_eq!((image.width(), image.height()), (2, 2));
        assert_eq!(image.get_pixel(0, 0), &Rgba([1, 2, 3, 255]));
        assert_eq!(image.get_pixel(1, 0), &Rgba([4, 5, 6, 255]));
        assert_eq!(image.get_pixel(0, 1), &Rgba([7, 8, 9, 255]));
        assert_eq!(image.get_pixel(1, 1), &Rgba([10, 11, 12, 255]));
        // The raw buffer is row-major: width * y + x * 4.
        assert_eq!(
            image.as_raw(),
            &[1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]
        );
    }

    #[test]
    fn encode_png_round_trips_through_the_image_crate() {
        let image = two_by_two();
        let bytes = encode_png(&image).expect("encoding an in-memory image");
        // The bytes are a valid PNG the image crate can decode back.
        let decoded = image::load_from_memory(&bytes).expect("the encoded PNG decodes");
        let decoded = decoded.to_rgba8();
        assert_eq!((decoded.width(), decoded.height()), (2, 2));
        assert_eq!(decoded.as_raw(), image.as_raw());
    }

    #[test]
    fn encode_png_is_byte_identical_across_calls() {
        let image = two_by_two();
        let first = encode_png(&image).expect("first encode");
        let second = encode_png(&image).expect("second encode");
        assert_eq!(first, second, "the encoder must be deterministic");
    }
}
