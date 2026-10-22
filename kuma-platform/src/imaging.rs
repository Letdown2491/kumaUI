//! The decode-to-`RenderImage` family: every path from image bytes
//! into what gpui paints goes through here, so the BGRA byte order
//! gpui expects is handled in exactly one place (the wire formats
//! feed RGBA8; gpui's frames want B,G,R,A). Decoding stays in the
//! caller where the format is special (svg, pdf, video), and this
//! module is the convert-and-fit it lands on.

use smallvec::SmallVec;
use std::path::Path;

/// The decode limits for untrusted input: without them, `image` 0.25
/// caps allocation (512 MiB) but not dimensions, so a hostile
/// 40000×40000 PNG in any browsed directory spikes ~1.5 GB per decode
/// before the allocation cap notices. No desktop screen is near these
/// bounds; a 16K×16K image still decodes whole.
fn limits() -> image::Limits {
    // non_exhaustive: build through the fields, not a literal
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    limits.max_alloc = Some(64 << 20);
    limits
}

/// The byte-order lore, one home: RGBA8 pixels in, the `RenderImage`
/// gpui paints from out.
pub fn render_image(rgba: image::RgbaImage) -> gpui::RenderImage {
    let mut bgra = rgba;
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    gpui::RenderImage::new(SmallVec::from_buf([image::Frame::new(bgra)]))
}

/// Open and decode a file, full size.
pub fn open(path: &Path) -> Option<image::DynamicImage> {
    let mut reader = image::ImageReader::open(path).ok()?;
    reader.limits(limits());
    reader.decode().ok()
}

/// Decode bytes in memory, full size.
pub fn decode_bytes(bytes: &[u8]) -> Option<gpui::RenderImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.limits(limits());
    let image = reader.decode().ok()?;
    Some(render_image(image.to_rgba8()))
}

/// A decoded image, downscaled to the bounds and converted. The
/// special-format decoders (svg, pdf, video posters) land here.
pub fn fit(
    image: image::DynamicImage,
    max_width: u32,
    max_height: u32,
) -> gpui::RenderImage {
    render_image(image.thumbnail(max_width, max_height).to_rgba8())
}

/// A file, downscaled to the bounds and converted.
pub fn decode_fit(path: &Path, max_width: u32, max_height: u32) -> Option<gpui::RenderImage> {
    Some(fit(open(path)?, max_width, max_height))
}

/// A file, full size and converted.
pub fn decode_file(path: &Path) -> Option<gpui::RenderImage> {
    Some(render_image(open(path)?.to_rgba8()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView;

    #[test]
    fn red_and_blue_swap_and_alpha_stays() {
        // the whole module's point: RGBA8 in, BGRA8 painted. A red
        // pixel (255, 0, 0, 255) becomes blue in the frame; alpha
        // rides untouched.
        let image = image::RgbaImage::from_raw(1, 1, vec![255, 0, 0, 255]).expect("1x1 buffer");
        let rendered = render_image(image);
        assert_eq!(rendered.as_bytes(0), Some(&[0u8, 0, 255, 255][..]));
    }

    #[test]
    fn fit_downscales_to_the_bounds() {
        // 128 wide fits into 48
        let big = image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(128, 128, |_, _| {
            image::Rgba([255u8, 0, 0, 255])
        }));
        let rendered = fit(big, 48, 48);
        let size = rendered.size(0);
        assert!(size.width.0 <= 48 && size.height.0 <= 48);
    }

    #[test]
    fn ordinary_images_still_decode_through_the_limits() {
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(8, 8, |_, _| {
            image::Rgba([1u8, 2, 3, 255])
        }));
        let mut bytes = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .expect("encoding an 8x8 png");
        let mut reader = image::ImageReader::new(std::io::Cursor::new(&bytes))
            .with_guessed_format()
            .expect("the magic bytes are a valid png");
        reader.limits(limits());
        let decoded = match reader.decode() {
            Ok(decoded) => decoded,
            Err(err) => panic!("decode failed: {err}; head: {:x?}", &bytes[..8.min(bytes.len())]),
        };
        assert_eq!(decoded.dimensions(), (8, 8));
        let rendered = decode_bytes(&bytes).expect("decode_bytes matches the raw reader");
        let size = rendered.size(0);
        assert_eq!((size.width.0, size.height.0), (8, 8));
    }
}
