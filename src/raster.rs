//! PWG / CUPS raster (v2, big- or little-endian, RLE) decoder → 8-bit gray pages.
//!
//! Header layout = `cups_page_header2_t` (1796 bytes). Line coding per PWG 5102.4 §4.3.
//! Everything CUPS's `rastertopwg` (and `gstoraster`/`pdftoraster` for cups-raster) emits for
//! sgray_8 / black_1 / srgb_24 must decode; anything else → `Unsupported`.

use thiserror::Error;

/// One decoded page. `data` is row-major, `width*height` bytes, 0 = black, 255 = white.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrayPage {
    pub width: u32,
    pub height: u32,
    /// Horizontal resolution in dpi (203 for native tape, 406 for supersampled, …).
    pub dpi: u32,
    pub data: Vec<u8>,
}

impl GrayPage {
    pub fn new_white(width: u32, height: u32, dpi: u32) -> Self {
        GrayPage { width, height, dpi, data: vec![255; (width as usize) * (height as usize)] }
    }
    pub fn row(&self, y: u32) -> &[u8] {
        let w = self.width as usize;
        &self.data[y as usize * w..(y as usize + 1) * w]
    }
    /// Physical page width in millimetres (0 if dpi unknown).
    pub fn width_mm(&self) -> f64 {
        if self.dpi == 0 { 0.0 } else { self.width as f64 / self.dpi as f64 * 25.4 }
    }
}

/// Byte order of the stream (sync word).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sync {
    /// "RaS2" — v2 big-endian, RLE compressed (this is PWG raster).
    V2BigEndian,
    /// "2SaR" — v2 little-endian, RLE compressed (CUPS raster on LE hosts).
    V2LittleEndian,
    /// "RaS3" / "3SaR" — v3, uncompressed.
    V3BigEndian,
    V3LittleEndian,
}

/// The subset of `cups_page_header2_t` we care about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PwgHeader {
    pub sync: Sync,
    pub media_type: String,
    pub output_type: String,
    pub hw_resolution: (u32, u32),
    pub num_copies: u32,
    pub orientation: u32,
    pub page_size_pt: (u32, u32),
    pub width: u32,
    pub height: u32,
    pub bits_per_color: u32,
    pub bits_per_pixel: u32,
    pub bytes_per_line: u32,
    pub color_order: u32,
    pub color_space: u32,
    pub compression: u32,
    /// PWG: cupsInteger[0] TotalPageCount, [1] CrossFeedTransform, [2] FeedTransform.
    pub total_page_count: u32,
    pub cross_feed_transform: i32,
    pub feed_transform: i32,
    /// cupsString[1] (PWG PageSizeName), e.g. "custom_cat-tape_48x297mm" / "48x297mm".
    pub page_size_name: String,
}

/// Safety limits applied before allocating.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_pages: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub max_pixels_total: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_pages: 32, max_width: 8192, max_height: 65535, max_pixels_total: 400_000_000 }
    }
}

#[derive(Debug, Error)]
pub enum RasterError {
    #[error("not a PWG/CUPS raster stream (bad sync word)")]
    BadSync,
    #[error("truncated raster stream at page {page}, line {line}")]
    Truncated { page: u32, line: u32 },
    #[error("unsupported raster format: colorspace {cspace}, {bpp} bpp, order {order}")]
    Unsupported { cspace: u32, bpp: u32, order: u32 },
    #[error("raster header inconsistent: {0}")]
    BadHeader(String),
    #[error("raster exceeds limits: {0}")]
    TooLarge(String),
}

/// The CUPS raster header is always this size.
pub const HEADER_LEN: usize = 1796;

/// Sniff: does this buffer start with a raster sync word?
pub fn is_raster(bytes: &[u8]) -> bool {
    matches!(bytes.get(0..4), Some(b"RaS2" | b"2SaR" | b"RaS3" | b"3SaR"))
}

/// Parse all page headers without decoding pixels (for `catprinterd inspect`).
pub fn inspect(bytes: &[u8]) -> Result<Vec<PwgHeader>, RasterError> {
    let _ = bytes;
    todo!("raster::inspect — implemented in the raster milestone")
}

/// Decode every page to 8-bit gray, honouring `limits`.
pub fn decode(bytes: &[u8], limits: &Limits) -> Result<Vec<GrayPage>, RasterError> {
    let _ = (bytes, limits);
    todo!("raster::decode — implemented in the raster milestone")
}
