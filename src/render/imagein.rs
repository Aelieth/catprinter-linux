//! PNG/JPEG → GrayPage (Rec.709 luma, alpha flattened onto white, EXIF orientation honoured).
//!
//! Port of catprinter/img.py `flatten_to_rgb` + `image_to_luma`.

use std::io::Cursor;

use image::{DynamicImage, ImageDecoder, ImageReader};

use crate::raster::GrayPage;
use crate::render::RenderError;

/// Rec. 709 luma weights (Pillow's convert("L") is Rec. 601 and composites RGBA onto black — we don't).
const LUMA_R: f64 = 0.2126;
const LUMA_G: f64 = 0.7152;
const LUMA_B: f64 = 0.0722;

/// Sniff PNG / JPEG magic.
pub fn is_image(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x89, b'P', b'N', b'G']) || bytes.starts_with(&[0xFF, 0xD8, 0xFF])
}

/// Rec. 709 luma of an 8-bit RGB triple, rounded (54/182/18 for pure R/G/B).
pub fn luma(r: u8, g: u8, b: u8) -> u8 {
    (LUMA_R * r as f64 + LUMA_G * g as f64 + LUMA_B * b as f64)
        .round()
        .clamp(0.0, 255.0) as u8
}

/// Decode to gray. Images have no physical size, so `dpi` is 0 ("unknown"), which makes
/// `render::Layout::Auto` treat them as tape (trim + fit width) regardless of pixel width.
pub fn load(bytes: &[u8]) -> Result<GrayPage, RenderError> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| RenderError::Image(format!("cannot sniff image format: {e}")))?;
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| RenderError::Image(format!("cannot decode image: {e}")))?;
    // EXIF orientation (phones!). Formats without it report NoTransforms; errors are non-fatal.
    let orientation = decoder.orientation().ok();
    let mut img = DynamicImage::from_decoder(decoder)
        .map_err(|e| RenderError::Image(format!("cannot decode image: {e}")))?;
    if let Some(o) = orientation {
        img.apply_orientation(o);
    }
    Ok(dynamic_to_gray(&img))
}

/// Flatten any alpha onto white and convert to Rec. 709 luma.
pub fn dynamic_to_gray(img: &DynamicImage) -> GrayPage {
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut data = Vec::with_capacity((w * h) as usize);
    for p in rgba.pixels() {
        let [r, g, b, a] = p.0;
        // Composite onto white: c' = c*a + 255*(1-a)
        let a = a as f64 / 255.0;
        let comp = |c: u8| (c as f64 * a + 255.0 * (1.0 - a)).round().clamp(0.0, 255.0) as u8;
        data.push(luma(comp(r), comp(g), comp(b)));
    }
    GrayPage {
        width: w,
        height: h,
        dpi: 0,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, ImageFormat, Rgb, Rgba};

    fn png_bytes<P>(img: &ImageBuffer<P, Vec<u8>>) -> Vec<u8>
    where
        P: image::PixelWithColorType<Subpixel = u8> + 'static,
    {
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    #[test]
    fn rec709_weights() {
        assert_eq!(luma(255, 0, 0), 54);
        assert_eq!(luma(0, 255, 0), 182);
        assert_eq!(luma(0, 0, 255), 18);
        assert_eq!(luma(255, 255, 255), 255);
        assert_eq!(luma(0, 0, 0), 0);
    }

    #[test]
    fn rgb_png_roundtrip_is_luma() {
        let img = ImageBuffer::from_pixel(8, 4, Rgb([255u8, 0, 0]));
        let bytes = png_bytes(&img);
        assert!(is_image(&bytes));
        let page = load(&bytes).unwrap();
        assert_eq!((page.width, page.height, page.dpi), (8, 4, 0));
        assert!(page.data.iter().all(|&v| v == 54));
    }

    #[test]
    fn rgba_transparent_flattens_onto_white() {
        let img = ImageBuffer::from_pixel(16, 16, Rgba([0u8, 0, 0, 0]));
        let page = load(&png_bytes(&img)).unwrap();
        assert!(page.data.iter().all(|&v| v == 255));
        // half-transparent black → mid gray
        let img = ImageBuffer::from_pixel(4, 4, Rgba([0u8, 0, 0, 128]));
        let page = load(&png_bytes(&img)).unwrap();
        assert!(
            page.data.iter().all(|&v| (126..=129).contains(&v)),
            "{:?}",
            page.data[0]
        );
    }

    #[test]
    fn garbage_is_an_image_error() {
        assert!(!is_image(b"hello"));
        assert!(matches!(load(b"hello world"), Err(RenderError::Image(_))));
    }

    #[test]
    fn repo_jpeg_loads() {
        let bytes =
            std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/media/hackoclock.jpg")).unwrap();
        assert!(is_image(&bytes));
        let page = load(&bytes).unwrap();
        assert!(page.width > 100 && page.height > 100);
        assert!(page.data.iter().any(|&v| v < 128));
    }
}
