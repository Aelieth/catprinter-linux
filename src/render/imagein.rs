//! PNG/JPEG → GrayPage (Rec.709 luma, alpha flattened onto white, EXIF orientation honoured).
//!
//! Port of catprinter/img.py `flatten_to_rgb` + `image_to_luma`.

use std::io::Cursor;

use image::{DynamicImage, ImageDecoder, ImageReader};

use crate::raster::GrayPage;
use crate::render::RenderError;

/// Decoder guards for client-supplied PNG/JPEG. These mirror `raster::Limits::default()` but are
/// fixed here so `load` keeps its one-argument signature (used from the engine and the `print`
/// bring-up tool). Without them a declared 11k×11k image is a ~1 GB allocation on a kid's laptop.
const MAX_IMG_W: u32 = 8192;
const MAX_IMG_H: u32 = 65535;
const MAX_IMG_PIXELS: u64 = 400_000_000;
const MAX_IMG_ALLOC: u64 = 256 * 1024 * 1024;

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
///
/// A client can POST any PNG/JPEG (we advertise them for IPP Everywhere). Without limits a
/// declared-11k×11k image is a ~1 GB allocation on a kid's laptop, so we bound the decoder's
/// dimensions and allocation and reject anything past the raster pixel budget before decoding.
pub fn load(bytes: &[u8]) -> Result<GrayPage, RenderError> {
    // Cheap header read first: reject oversized images before allocating the pixel buffer.
    let peek = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| RenderError::Image(format!("cannot sniff image format: {e}")))?;
    if let Ok((w, h)) = peek.into_dimensions() {
        if w as u64 * h as u64 > MAX_IMG_PIXELS {
            return Err(RenderError::Image(format!(
                "image is {w}×{h} = {} pixels; limit is {MAX_IMG_PIXELS}",
                w as u64 * h as u64
            )));
        }
    }
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| RenderError::Image(format!("cannot sniff image format: {e}")))?;
    let mut lim = image::Limits::default();
    lim.max_image_width = Some(MAX_IMG_W);
    lim.max_image_height = Some(MAX_IMG_H);
    lim.max_alloc = Some(MAX_IMG_ALLOC);
    reader.limits(lim);
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

/// Flatten any alpha onto white and convert to Rec. 709 luma. Opaque images skip the RGBA copy.
pub fn dynamic_to_gray(img: &DynamicImage) -> GrayPage {
    use image::GenericImageView;
    let (w, h) = img.dimensions();
    let mut data = Vec::with_capacity((w as usize).saturating_mul(h as usize));
    if img.color().has_alpha() {
        let rgba = img.to_rgba8();
        for p in rgba.pixels() {
            let [r, g, b, a] = p.0;
            // Composite onto white: c' = c*a + 255*(1-a)
            let a = a as f64 / 255.0;
            let comp = |c: u8| (c as f64 * a + 255.0 * (1.0 - a)).round().clamp(0.0, 255.0) as u8;
            data.push(luma(comp(r), comp(g), comp(b)));
        }
    } else {
        let rgb = img.to_rgb8();
        for p in rgb.pixels() {
            let [r, g, b] = p.0;
            data.push(luma(r, g, b));
        }
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

    #[test]
    fn a_reasonable_image_still_loads() {
        // The pixel/width guards must not reject an ordinary image.
        let img = ImageBuffer::from_pixel(2000, 1500, Rgb([0u8, 0, 0]));
        assert!(load(&png_bytes(&img)).is_ok());
    }

    #[test]
    fn opaque_rgb_skips_alpha_path() {
        let img = ImageBuffer::from_pixel(4, 4, Rgb([0u8, 255, 0]));
        let page = load(&png_bytes(&img)).unwrap();
        assert!(page.data.iter().all(|&v| v == 182));
    }
}
