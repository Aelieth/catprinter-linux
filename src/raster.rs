//! PWG / CUPS raster (v2, big- or little-endian, RLE; v3 uncompressed) decoder → 8-bit gray pages.
//!
//! Header layout = `cups_page_header2_t` (1796 bytes). Line coding per PWG 5102.4 §4.3 (the CUPS
//! "modified PackBits"). Everything CUPS's `rastertopwg` (and `gstoraster`/`pdftoraster` for
//! cups-raster) emits for sgray_8 / black_1 / srgb_24 must decode; anything else → `Unsupported`.
//!
//! Empirically verified against fixtures produced by CUPS 2.4.19 + cups-filters 2.0.1
//! (`tests/fixtures/*.pwg`, see `scripts/make-fixtures.sh`): sync `RaS2` (big-endian),
//! `MediaClass = "PwgRaster"`, `cupsColorSpace = 18` (sGray), 8 bpp, `cupsCompression = 0` even
//! though the data *is* RLE (v2 streams are always compressed), the 48 mm roll comes out as
//! `cupsWidth = 383`, and the PWG PageSizeName is stored in `cupsPageSizeName` (offset 1732;
//! the `cupsString[16]` slots are all empty), e.g. `"47.98x297.04mm.Borderless"` / `"A4.Borderless"`.

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
        GrayPage {
            width,
            height,
            dpi,
            data: vec![255; (width as usize) * (height as usize)],
        }
    }
    pub fn row(&self, y: u32) -> &[u8] {
        let w = self.width as usize;
        &self.data[y as usize * w..(y as usize + 1) * w]
    }
    /// Physical page width in millimetres (0 if dpi unknown).
    pub fn width_mm(&self) -> f64 {
        if self.dpi == 0 {
            0.0
        } else {
            self.width as f64 / self.dpi as f64 * 25.4
        }
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

impl Sync {
    fn from_bytes(b: &[u8]) -> Option<Sync> {
        match b.get(0..4)? {
            b"RaS2" => Some(Sync::V2BigEndian),
            b"2SaR" => Some(Sync::V2LittleEndian),
            b"RaS3" => Some(Sync::V3BigEndian),
            b"3SaR" => Some(Sync::V3LittleEndian),
            _ => None,
        }
    }
    fn big_endian(self) -> bool {
        matches!(self, Sync::V2BigEndian | Sync::V3BigEndian)
    }
    fn compressed(self) -> bool {
        matches!(self, Sync::V2BigEndian | Sync::V2LittleEndian)
    }
    /// The 4 sync bytes for this variant.
    pub fn magic(self) -> &'static [u8; 4] {
        match self {
            Sync::V2BigEndian => b"RaS2",
            Sync::V2LittleEndian => b"2SaR",
            Sync::V3BigEndian => b"RaS3",
            Sync::V3LittleEndian => b"3SaR",
        }
    }
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
    /// `cupsPageSizeName` (offset 1732; falls back to `cupsString[0]`), e.g. "47.98x297.04mm.Borderless" / "A4.Borderless".
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
        Limits {
            max_pages: 32,
            max_width: 8192,
            max_height: 65535,
            max_pixels_total: 400_000_000,
        }
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

// --- cups_page_header2_t byte offsets (all u32 unless noted) ---------------------------------
const OFF_MEDIA_TYPE: usize = 128; // char[64]
const OFF_OUTPUT_TYPE: usize = 192; // char[64]
const OFF_HW_RESOLUTION: usize = 276; // u32[2]
const OFF_NUM_COPIES: usize = 340;
const OFF_ORIENTATION: usize = 344;
const OFF_PAGE_SIZE: usize = 352; // u32[2]
const OFF_CUPS_WIDTH: usize = 372;
const OFF_CUPS_HEIGHT: usize = 376;
const OFF_BITS_PER_COLOR: usize = 384;
const OFF_BITS_PER_PIXEL: usize = 388;
const OFF_BYTES_PER_LINE: usize = 392;
const OFF_COLOR_ORDER: usize = 396;
const OFF_COLOR_SPACE: usize = 400;
const OFF_COMPRESSION: usize = 404;
const OFF_CUPS_INTEGER: usize = 452; // u32[16] (PWG: [0] TotalPageCount, [1] CrossFeed, [2] Feed)
const OFF_CUPS_STRING: usize = 580; // char[16][64]
const OFF_PAGE_SIZE_NAME: usize = 1732; // char[64] (v2 field; PWG PageSizeName)

// --- cups_cspace_t values we understand -----------------------------------------------------
const CSPACE_W: u32 = 0; // luminance, 0 = black
const CSPACE_RGB: u32 = 1;
const CSPACE_K: u32 = 3; // black ink, 0 = white
const CSPACE_SW: u32 = 18; // sGray
const CSPACE_SRGB: u32 = 19;
const CSPACE_ADOBERGB: u32 = 20;
const ORDER_CHUNKED: u32 = 0;

/// Sniff: does this buffer start with a raster sync word?
pub fn is_raster(bytes: &[u8]) -> bool {
    Sync::from_bytes(bytes).is_some()
}

/// Parse all page headers without decoding pixels (for `catprinterd inspect`).
/// The RLE stream still has to be walked to find the next page; pixels are discarded.
pub fn inspect(bytes: &[u8]) -> Result<Vec<PwgHeader>, RasterError> {
    let mut out = Vec::new();
    walk_stream(bytes, None, false, |hdr, _| {
        out.push(hdr);
        Ok(())
    })?;
    Ok(out)
}

/// Decode every page to 8-bit gray, honouring `limits`.
pub fn decode(bytes: &[u8], limits: &Limits) -> Result<Vec<GrayPage>, RasterError> {
    let mut pages: Vec<GrayPage> = Vec::new();
    walk_stream(bytes, Some(limits), true, |hdr, gray| {
        pages.push(GrayPage {
            width: hdr.width,
            height: hdr.height,
            dpi: hdr.hw_resolution.0,
            data: gray.unwrap_or_default(),
        });
        Ok(())
    })?;
    Ok(pages)
}

/// Shared stream walker used by `inspect` and `decode`: parses and validates each page header,
/// walks the RLE/raw rows (converting them to gray when `store` is true), and calls `on_page`
/// once per page with the header and — if stored — the `width*height` gray buffer.
fn walk_stream<F>(
    bytes: &[u8],
    limits: Option<&Limits>,
    store: bool,
    mut on_page: F,
) -> Result<(), RasterError>
where
    F: FnMut(PwgHeader, Option<Vec<u8>>) -> Result<(), RasterError>,
{
    let sync = Sync::from_bytes(bytes).ok_or(RasterError::BadSync)?;
    let be = sync.big_endian();
    let mut pos = 4usize;
    let mut page_index: u32 = 0;
    let mut total_pixels: u64 = 0;
    while pos < bytes.len() {
        let remaining = bytes.len() - pos;
        if remaining < HEADER_LEN {
            // Trailing padding (some writers pad with zeros); stop quietly if it is all zero,
            // otherwise it is a truncated header.
            if bytes[pos..].iter().all(|&b| b == 0) {
                break;
            }
            return Err(RasterError::Truncated {
                page: page_index + 1,
                line: 0,
            });
        }
        let hdr = parse_header(&bytes[pos..pos + HEADER_LEN], sync, be);
        if hdr.width == 0 && hdr.height == 0 && hdr.bits_per_pixel == 0 {
            // A zeroed header after the last page = padding.
            if bytes[pos..].iter().all(|&b| b == 0) {
                break;
            }
            return Err(RasterError::BadHeader("zero-sized page".into()));
        }
        pos += HEADER_LEN;
        page_index += 1;

        // --- validate the geometry / format before allocating anything ---
        let fmt = PixelFormat::for_header(&hdr)?;
        if hdr.width == 0 || hdr.height == 0 {
            return Err(RasterError::BadHeader(format!(
                "empty page {}x{}",
                hdr.width, hdr.height
            )));
        }
        let expected_bpl = (hdr.width as u64 * hdr.bits_per_pixel as u64).div_ceil(8);
        if hdr.bytes_per_line as u64 != expected_bpl {
            return Err(RasterError::BadHeader(format!(
                "cupsBytesPerLine {} but width {} × {} bpp needs {}",
                hdr.bytes_per_line, hdr.width, hdr.bits_per_pixel, expected_bpl
            )));
        }
        if let Some(l) = limits {
            if page_index > l.max_pages {
                return Err(RasterError::TooLarge(format!(
                    "more than {} pages",
                    l.max_pages
                )));
            }
            if hdr.width > l.max_width || hdr.height > l.max_height {
                return Err(RasterError::TooLarge(format!(
                    "page {} is {}x{} px (max {}x{})",
                    page_index, hdr.width, hdr.height, l.max_width, l.max_height
                )));
            }
            total_pixels += hdr.width as u64 * hdr.height as u64;
            if total_pixels > l.max_pixels_total {
                return Err(RasterError::TooLarge(format!(
                    "{} pixels in total (max {})",
                    total_pixels, l.max_pixels_total
                )));
            }
        }

        // --- decode rows ---
        let bpl = hdr.bytes_per_line as usize;
        let width = hdr.width as usize;
        let height = hdr.height as usize;
        let mut row = vec![0u8; bpl];
        let mut gray_rows: Vec<u8> = if store {
            vec![255u8; width * height]
        } else {
            Vec::new()
        };
        let mut y = 0usize;

        if sync.compressed() {
            while y < height {
                // Line repeat count.
                let Some(&rep) = bytes.get(pos) else {
                    return Err(RasterError::Truncated {
                        page: page_index,
                        line: y as u32,
                    });
                };
                pos += 1;
                let mut repeat = rep as usize + 1;
                if repeat > height - y {
                    repeat = height - y; // CUPS clamps to the remaining lines
                }
                // Runs until the row is full.
                let mut filled = 0usize;
                while filled < bpl {
                    let Some(&code) = bytes.get(pos) else {
                        return Err(RasterError::Truncated {
                            page: page_index,
                            line: y as u32,
                        });
                    };
                    pos += 1;
                    if code >= 128 {
                        // Literal pixels.
                        let mut n = (257 - code as usize) * fmt.pixel_bytes;
                        if n > bpl - filled {
                            n = bpl - filled;
                        }
                        let Some(src) = bytes.get(pos..pos + n) else {
                            return Err(RasterError::Truncated {
                                page: page_index,
                                line: y as u32,
                            });
                        };
                        row[filled..filled + n].copy_from_slice(src);
                        pos += n;
                        filled += n;
                    } else {
                        // One pixel repeated.
                        let mut n = (code as usize + 1) * fmt.pixel_bytes;
                        if n > bpl - filled {
                            n = bpl - filled;
                        }
                        let Some(px) = bytes.get(pos..pos + fmt.pixel_bytes) else {
                            return Err(RasterError::Truncated {
                                page: page_index,
                                line: y as u32,
                            });
                        };
                        pos += fmt.pixel_bytes;
                        if fmt.pixel_bytes == 1 {
                            row[filled..filled + n].fill(px[0]);
                        } else {
                            let mut off = filled;
                            let end = filled + n;
                            while off + fmt.pixel_bytes <= end {
                                row[off..off + fmt.pixel_bytes].copy_from_slice(px);
                                off += fmt.pixel_bytes;
                            }
                            // A clamped run may leave a partial pixel; copy what fits.
                            if off < end {
                                let k = end - off;
                                row[off..end].copy_from_slice(&px[..k]);
                            }
                        }
                        filled += n;
                    }
                }
                if store {
                    let first = y * width;
                    fmt.row_to_gray(&row, be, &mut gray_rows[first..first + width]);
                    // Repeated lines: memcpy the converted row.
                    for r in 1..repeat {
                        let (head, tail) = gray_rows.split_at_mut(first + r * width);
                        tail[..width].copy_from_slice(&head[first..first + width]);
                    }
                }
                y += repeat;
            }
        } else {
            // v3: raw rows.
            let need = bpl * height;
            let Some(raw) = bytes.get(pos..pos + need) else {
                let have_lines = (bytes.len().saturating_sub(pos)) / bpl.max(1);
                return Err(RasterError::Truncated {
                    page: page_index,
                    line: have_lines as u32,
                });
            };
            if store {
                for (yy, src) in raw.chunks_exact(bpl).enumerate() {
                    fmt.row_to_gray(src, be, &mut gray_rows[yy * width..(yy + 1) * width]);
                }
            }
            pos += need;
        }

        if store {
            apply_transforms(&mut gray_rows, width, height, &hdr);
            on_page(hdr, Some(gray_rows))?;
        } else {
            on_page(hdr, None)?;
        }
    }
    Ok(())
}

/// Pixel format of a page and how to turn one raw row into gray.
#[derive(Debug, Clone, Copy)]
struct PixelFormat {
    kind: Kind,
    /// Bytes per "pixel unit" as CUPS defines it (1 for ≤ 8 bpp, else bpp/8).
    pixel_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// 8-bit luminance (W / sGray): value = gray.
    Gray8,
    /// 8-bit black ink (K): gray = 255 - value.
    Black8,
    /// 16-bit luminance: high byte.
    Gray16,
    /// 16-bit black: 255 - high byte.
    Black16,
    /// 1-bit luminance, MSB first, 1 = white.
    Gray1,
    /// 1-bit black, MSB first, 1 = black.
    Black1,
    /// 24-bit RGB chunky → Rec.709 luma.
    Rgb24,
    /// 48-bit RGB chunky → high bytes → Rec.709 luma.
    Rgb48,
}

impl PixelFormat {
    fn for_header(h: &PwgHeader) -> Result<PixelFormat, RasterError> {
        let unsupported = || RasterError::Unsupported {
            cspace: h.color_space,
            bpp: h.bits_per_pixel,
            order: h.color_order,
        };
        let kind = match (h.color_space, h.bits_per_color, h.bits_per_pixel) {
            (CSPACE_W | CSPACE_SW, 8, 8) => Kind::Gray8,
            (CSPACE_K, 8, 8) => Kind::Black8,
            (CSPACE_W | CSPACE_SW, 16, 16) => Kind::Gray16,
            (CSPACE_K, 16, 16) => Kind::Black16,
            (CSPACE_W | CSPACE_SW, 1, 1) => Kind::Gray1,
            (CSPACE_K, 1, 1) => Kind::Black1,
            (CSPACE_RGB | CSPACE_SRGB | CSPACE_ADOBERGB, 8, 24) => Kind::Rgb24,
            (CSPACE_RGB | CSPACE_SRGB | CSPACE_ADOBERGB, 16, 48) => Kind::Rgb48,
            _ => return Err(unsupported()),
        };
        if matches!(kind, Kind::Rgb24 | Kind::Rgb48) && h.color_order != ORDER_CHUNKED {
            return Err(unsupported());
        }
        let pixel_bytes = if h.bits_per_pixel >= 8 {
            (h.bits_per_pixel / 8) as usize
        } else {
            1
        };
        Ok(PixelFormat { kind, pixel_bytes })
    }

    /// Convert one raw row (`bytes_per_line` bytes) into `out` (`width` gray bytes).
    fn row_to_gray(&self, row: &[u8], big_endian: bool, out: &mut [u8]) {
        let width = out.len();
        match self.kind {
            Kind::Gray8 => out.copy_from_slice(&row[..width]),
            Kind::Black8 => {
                for (o, &v) in out.iter_mut().zip(row.iter()) {
                    *o = 255 - v;
                }
            }
            Kind::Gray16 | Kind::Black16 => {
                let hi = if big_endian { 0 } else { 1 };
                let invert = self.kind == Kind::Black16;
                for (o, px) in out.iter_mut().zip(row.chunks_exact(2)) {
                    let v = px[hi];
                    *o = if invert { 255 - v } else { v };
                }
            }
            Kind::Gray1 | Kind::Black1 => {
                let one_is_black = self.kind == Kind::Black1;
                for (x, o) in out.iter_mut().enumerate() {
                    let bit = (row[x >> 3] >> (7 - (x & 7))) & 1;
                    let black = if one_is_black { bit == 1 } else { bit == 0 };
                    *o = if black { 0 } else { 255 };
                }
            }
            Kind::Rgb24 => {
                for (o, px) in out.iter_mut().zip(row.chunks_exact(3)) {
                    *o = luma709(px[0], px[1], px[2]);
                }
            }
            Kind::Rgb48 => {
                let hi = if big_endian { 0 } else { 1 };
                for (o, px) in out.iter_mut().zip(row.chunks_exact(6)) {
                    *o = luma709(px[hi], px[2 + hi], px[4 + hi]);
                }
            }
        }
    }
}

/// Rec.709 luma, rounded (matches catprinter/img.py `image_to_luma`).
#[inline]
fn luma709(r: u8, g: u8, b: u8) -> u8 {
    let y = 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;
    (y + 0.5).clamp(0.0, 255.0) as u8
}

/// PWG CrossFeedTransform / FeedTransform: -1 mirrors horizontally / flips vertically.
fn apply_transforms(data: &mut [u8], width: usize, height: usize, hdr: &PwgHeader) {
    if hdr.cross_feed_transform == -1 {
        for y in 0..height {
            data[y * width..(y + 1) * width].reverse();
        }
    }
    if hdr.feed_transform == -1 {
        let mut top = 0usize;
        let mut bot = height.saturating_sub(1);
        while top < bot {
            let (a, b) = data.split_at_mut(bot * width);
            a[top * width..(top + 1) * width].swap_with_slice(&mut b[..width]);
            top += 1;
            bot -= 1;
        }
    }
}

fn parse_header(h: &[u8], sync: Sync, be: bool) -> PwgHeader {
    let u32_at = |off: usize| -> u32 {
        let b = [h[off], h[off + 1], h[off + 2], h[off + 3]];
        if be {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        }
    };
    let i32_at = |off: usize| -> i32 { u32_at(off) as i32 };
    let cstr = |off: usize| -> String {
        let s = &h[off..off + 64];
        let end = s.iter().position(|&b| b == 0).unwrap_or(64);
        String::from_utf8_lossy(&s[..end]).into_owned()
    };
    let mut page_size_name = cstr(OFF_PAGE_SIZE_NAME);
    if page_size_name.is_empty() {
        page_size_name = cstr(OFF_CUPS_STRING);
    }
    PwgHeader {
        sync,
        media_type: cstr(OFF_MEDIA_TYPE),
        output_type: cstr(OFF_OUTPUT_TYPE),
        hw_resolution: (u32_at(OFF_HW_RESOLUTION), u32_at(OFF_HW_RESOLUTION + 4)),
        num_copies: u32_at(OFF_NUM_COPIES),
        orientation: u32_at(OFF_ORIENTATION),
        page_size_pt: (u32_at(OFF_PAGE_SIZE), u32_at(OFF_PAGE_SIZE + 4)),
        width: u32_at(OFF_CUPS_WIDTH),
        height: u32_at(OFF_CUPS_HEIGHT),
        bits_per_color: u32_at(OFF_BITS_PER_COLOR),
        bits_per_pixel: u32_at(OFF_BITS_PER_PIXEL),
        bytes_per_line: u32_at(OFF_BYTES_PER_LINE),
        color_order: u32_at(OFF_COLOR_ORDER),
        color_space: u32_at(OFF_COLOR_SPACE),
        compression: u32_at(OFF_COMPRESSION),
        total_page_count: u32_at(OFF_CUPS_INTEGER),
        cross_feed_transform: i32_at(OFF_CUPS_INTEGER + 4),
        feed_transform: i32_at(OFF_CUPS_INTEGER + 8),
        page_size_name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal header description for the test encoder.
    #[derive(Clone)]
    struct Spec {
        width: u32,
        height: u32,
        bits_per_color: u32,
        bits_per_pixel: u32,
        color_space: u32,
        color_order: u32,
        dpi: u32,
        cross_feed: i32,
        feed: i32,
        page_size_name: &'static str,
        num_copies: u32,
    }

    impl Spec {
        fn gray8(width: u32, height: u32) -> Spec {
            Spec {
                width,
                height,
                bits_per_color: 8,
                bits_per_pixel: 8,
                color_space: CSPACE_SW,
                color_order: 0,
                dpi: 203,
                cross_feed: 1,
                feed: 1,
                page_size_name: "47.98x297.04mm.Borderless",
                num_copies: 1,
            }
        }
        fn bytes_per_line(&self) -> usize {
            (self.width as usize * self.bits_per_pixel as usize).div_ceil(8)
        }
        fn pixel_bytes(&self) -> usize {
            if self.bits_per_pixel >= 8 {
                (self.bits_per_pixel / 8) as usize
            } else {
                1
            }
        }
    }

    fn put_u32(h: &mut [u8], off: usize, v: u32, be: bool) {
        let b = if be { v.to_be_bytes() } else { v.to_le_bytes() };
        h[off..off + 4].copy_from_slice(&b);
    }

    fn put_str(h: &mut [u8], off: usize, s: &str) {
        let b = s.as_bytes();
        h[off..off + b.len()].copy_from_slice(b);
    }

    fn header_bytes(spec: &Spec, be: bool) -> Vec<u8> {
        let mut h = vec![0u8; HEADER_LEN];
        put_str(&mut h, 0, "PwgRaster");
        put_str(&mut h, OFF_OUTPUT_TYPE, "automatic");
        put_u32(&mut h, OFF_HW_RESOLUTION, spec.dpi, be);
        put_u32(&mut h, OFF_HW_RESOLUTION + 4, spec.dpi, be);
        put_u32(&mut h, OFF_NUM_COPIES, spec.num_copies, be);
        put_u32(&mut h, OFF_PAGE_SIZE, 136, be);
        put_u32(&mut h, OFF_PAGE_SIZE + 4, 842, be);
        put_u32(&mut h, OFF_CUPS_WIDTH, spec.width, be);
        put_u32(&mut h, OFF_CUPS_HEIGHT, spec.height, be);
        put_u32(&mut h, OFF_BITS_PER_COLOR, spec.bits_per_color, be);
        put_u32(&mut h, OFF_BITS_PER_PIXEL, spec.bits_per_pixel, be);
        put_u32(&mut h, OFF_BYTES_PER_LINE, spec.bytes_per_line() as u32, be);
        put_u32(&mut h, OFF_COLOR_ORDER, spec.color_order, be);
        put_u32(&mut h, OFF_COLOR_SPACE, spec.color_space, be);
        put_u32(&mut h, OFF_CUPS_INTEGER, 1, be);
        put_u32(&mut h, OFF_CUPS_INTEGER + 4, spec.cross_feed as u32, be);
        put_u32(&mut h, OFF_CUPS_INTEGER + 8, spec.feed as u32, be);
        put_str(&mut h, OFF_PAGE_SIZE_NAME, spec.page_size_name);
        h
    }

    /// PWG/CUPS "modified PackBits": line repeats + pixel-repeat/literal runs.
    fn encode_rows_rle(spec: &Spec, rows: &[Vec<u8>], out: &mut Vec<u8>) {
        let bpl = spec.bytes_per_line();
        let pb = spec.pixel_bytes();
        assert!(
            bpl % pb == 0,
            "test spec: bpl must be a multiple of pixel bytes"
        );
        let mut i = 0;
        while i < rows.len() {
            // count identical following lines (max 256 per repeat byte)
            let mut n = 1;
            while i + n < rows.len() && rows[i + n] == rows[i] && n < 256 {
                n += 1;
            }
            out.push((n - 1) as u8);
            let row = &rows[i];
            assert_eq!(row.len(), bpl);
            let pixels: Vec<&[u8]> = row.chunks_exact(pb).collect();
            let mut p = 0;
            while p < pixels.len() {
                // repeat run?
                let mut run = 1;
                while p + run < pixels.len() && pixels[p + run] == pixels[p] && run < 128 {
                    run += 1;
                }
                if run >= 2 {
                    out.push((run - 1) as u8);
                    out.extend_from_slice(pixels[p]);
                    p += run;
                } else {
                    // literal run: until a repeat of ≥2 begins or 128 pixels
                    let start = p;
                    let mut len = 0;
                    while p < pixels.len() && len < 128 {
                        if p + 1 < pixels.len() && pixels[p + 1] == pixels[p] {
                            break;
                        }
                        p += 1;
                        len += 1;
                    }
                    assert!(len > 0);
                    out.push((257 - len) as u8);
                    for px in &pixels[start..start + len] {
                        out.extend_from_slice(px);
                    }
                }
            }
            i += n;
        }
    }

    fn encode_stream(sync: Sync, pages: &[(Spec, Vec<Vec<u8>>)]) -> Vec<u8> {
        let be = sync.big_endian();
        let mut out = Vec::new();
        out.extend_from_slice(sync.magic());
        for (spec, rows) in pages {
            assert_eq!(rows.len(), spec.height as usize);
            out.extend_from_slice(&header_bytes(spec, be));
            if sync.compressed() {
                encode_rows_rle(spec, rows, &mut out);
            } else {
                for r in rows {
                    out.extend_from_slice(r);
                }
            }
        }
        out
    }

    /// 384×10 gray page with a mix of flat rows (repeat), a gradient (literal) and mixed runs.
    fn sample_gray_rows() -> Vec<Vec<u8>> {
        let w = 384usize;
        let mut rows = Vec::new();
        for _ in 0..3 {
            rows.push(vec![255u8; w]); // three identical white rows → repeat byte 2
        }
        rows.push((0..w).map(|x| (x % 256) as u8).collect()); // literal-heavy gradient
        let mut mixed = vec![0u8; w];
        for (x, v) in mixed.iter_mut().enumerate() {
            *v = if (x / 7) % 2 == 0 {
                40
            } else {
                (x % 251) as u8
            };
        }
        rows.push(mixed.clone());
        rows.push(mixed); // one repeat of the mixed row
        rows.push(vec![0u8; w]); // solid black
        rows.push(vec![128u8; w]);
        let mut edge = vec![255u8; w];
        edge[0] = 0;
        edge[w - 1] = 0;
        rows.push(edge);
        rows.push(vec![17u8; w]);
        assert_eq!(rows.len(), 10);
        rows
    }

    #[test]
    fn sniff_sync_words() {
        assert!(is_raster(b"RaS2xxxx"));
        assert!(is_raster(b"2SaRxxxx"));
        assert!(is_raster(b"RaS3xxxx"));
        assert!(is_raster(b"3SaRxxxx"));
        assert!(!is_raster(b"%PDF-1.4"));
        assert!(!is_raster(b"Ra"));
        assert!(matches!(
            decode(b"%PDF", &Limits::default()),
            Err(RasterError::BadSync)
        ));
    }

    #[test]
    fn roundtrip_gray8_big_endian_with_repeats_and_literals() {
        let rows = sample_gray_rows();
        let spec = Spec::gray8(384, 10);
        let stream = encode_stream(Sync::V2BigEndian, &[(spec.clone(), rows.clone())]);
        let pages = decode(&stream, &Limits::default()).unwrap();
        assert_eq!(pages.len(), 1);
        let p = &pages[0];
        assert_eq!((p.width, p.height, p.dpi), (384, 10, 203));
        for (y, row) in rows.iter().enumerate() {
            assert_eq!(p.row(y as u32), &row[..], "row {y}");
        }
        let hdrs = inspect(&stream).unwrap();
        assert_eq!(hdrs.len(), 1);
        assert_eq!(hdrs[0].sync, Sync::V2BigEndian);
        assert_eq!(hdrs[0].width, 384);
        assert_eq!(hdrs[0].bytes_per_line, 384);
        assert_eq!(hdrs[0].color_space, CSPACE_SW);
        assert_eq!(hdrs[0].page_size_name, "47.98x297.04mm.Borderless");
        assert_eq!(hdrs[0].output_type, "automatic");
        assert_eq!(hdrs[0].total_page_count, 1);
        assert_eq!(
            (hdrs[0].cross_feed_transform, hdrs[0].feed_transform),
            (1, 1)
        );
    }

    #[test]
    fn roundtrip_little_endian_and_two_pages() {
        let rows = sample_gray_rows();
        let a = Spec::gray8(384, 10);
        let mut b = Spec::gray8(200, 4);
        b.dpi = 406;
        let rows_b: Vec<Vec<u8>> = (0..4).map(|y| vec![(y * 60) as u8; 200]).collect();
        let stream = encode_stream(
            Sync::V2LittleEndian,
            &[(a, rows.clone()), (b, rows_b.clone())],
        );
        let pages = decode(&stream, &Limits::default()).unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].row(3), &rows[3][..]);
        assert_eq!(
            (pages[1].width, pages[1].height, pages[1].dpi),
            (200, 4, 406)
        );
        assert_eq!(pages[1].row(2), &rows_b[2][..]);
        let hdrs = inspect(&stream).unwrap();
        assert_eq!(hdrs.len(), 2);
        assert_eq!(hdrs[1].sync, Sync::V2LittleEndian);
        assert_eq!(hdrs[1].hw_resolution, (406, 406));
    }

    #[test]
    fn roundtrip_v3_uncompressed_both_orders() {
        let rows = sample_gray_rows();
        for sync in [Sync::V3BigEndian, Sync::V3LittleEndian] {
            let stream = encode_stream(sync, &[(Spec::gray8(384, 10), rows.clone())]);
            let pages = decode(&stream, &Limits::default()).unwrap();
            assert_eq!(pages.len(), 1);
            for (y, row) in rows.iter().enumerate() {
                assert_eq!(pages[0].row(y as u32), &row[..]);
            }
            assert_eq!(inspect(&stream).unwrap()[0].sync, sync);
        }
    }

    #[test]
    fn one_bit_white_and_black_polarity_msb_first() {
        // 16 px wide, 2 rows. Row 0: 0b1010_0000 0b0000_0001 ; row 1: all zero.
        let mut spec = Spec::gray8(16, 2);
        spec.bits_per_color = 1;
        spec.bits_per_pixel = 1;
        spec.color_space = CSPACE_W; // 1 = white
        let rows = vec![vec![0b1010_0000u8, 0b0000_0001], vec![0u8, 0u8]];
        let stream = encode_stream(Sync::V2BigEndian, &[(spec.clone(), rows.clone())]);
        let p = &decode(&stream, &Limits::default()).unwrap()[0];
        assert_eq!(p.row(0)[0], 255); // MSB set → white
        assert_eq!(p.row(0)[1], 0);
        assert_eq!(p.row(0)[2], 255);
        assert_eq!(p.row(0)[3], 0);
        assert_eq!(p.row(0)[15], 255); // last bit of second byte
        assert_eq!(p.row(0)[14], 0);
        assert!(p.row(1).iter().all(|&v| v == 0)); // W: 0 = black
                                                   // Same bits as K: 1 = black.
        spec.color_space = CSPACE_K;
        let stream = encode_stream(Sync::V2BigEndian, &[(spec, rows)]);
        let p = &decode(&stream, &Limits::default()).unwrap()[0];
        assert_eq!(p.row(0)[0], 0);
        assert_eq!(p.row(0)[1], 255);
        assert_eq!(p.row(0)[15], 0);
        assert!(p.row(1).iter().all(|&v| v == 255)); // K: 0 = white
    }

    #[test]
    fn black8_is_inverted() {
        let mut spec = Spec::gray8(4, 1);
        spec.color_space = CSPACE_K;
        let stream = encode_stream(Sync::V2BigEndian, &[(spec, vec![vec![0, 255, 100, 200]])]);
        let p = &decode(&stream, &Limits::default()).unwrap()[0];
        assert_eq!(p.row(0), &[255, 0, 155, 55]);
    }

    #[test]
    fn rgb24_luma_and_rgb48_high_bytes() {
        let mut spec = Spec::gray8(4, 1);
        spec.color_space = CSPACE_SRGB;
        spec.bits_per_pixel = 24;
        // red, green, blue, white
        let row = vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
        let stream = encode_stream(Sync::V2BigEndian, &[(spec.clone(), vec![row])]);
        let p = &decode(&stream, &Limits::default()).unwrap()[0];
        assert_eq!(p.row(0), &[54, 182, 18, 255]);
        // 48-bit, big-endian: high byte first.
        let mut spec48 = spec.clone();
        spec48.bits_per_color = 16;
        spec48.bits_per_pixel = 48;
        let row = vec![
            255, 0, 0, 0, 0, 0, // red
            0, 0, 255, 0, 0, 0, // green
            0, 0, 0, 0, 255, 0, // blue
            255, 255, 255, 255, 255, 255,
        ];
        let stream = encode_stream(Sync::V2BigEndian, &[(spec48.clone(), vec![row.clone()])]);
        let p = &decode(&stream, &Limits::default()).unwrap()[0];
        assert_eq!(p.row(0), &[54, 182, 18, 255]);
        // little-endian: high byte second → the same bytes now read as black except white.
        let stream = encode_stream(Sync::V2LittleEndian, &[(spec48, vec![row])]);
        let p = &decode(&stream, &Limits::default()).unwrap()[0];
        assert_eq!(p.row(0), &[0, 0, 0, 255]);
        // planar RGB is unsupported
        let mut planar = spec;
        planar.color_order = 2;
        let stream = encode_stream(Sync::V2BigEndian, &[(planar, vec![vec![0; 12]])]);
        assert!(matches!(
            decode(&stream, &Limits::default()),
            Err(RasterError::Unsupported {
                cspace: 19,
                bpp: 24,
                order: 2
            })
        ));
    }

    #[test]
    fn gray16_takes_high_byte_per_endianness() {
        let mut spec = Spec::gray8(2, 1);
        spec.bits_per_color = 16;
        spec.bits_per_pixel = 16;
        let row = vec![0x12, 0x34, 0xAB, 0xCD];
        let be = encode_stream(Sync::V2BigEndian, &[(spec.clone(), vec![row.clone()])]);
        assert_eq!(
            decode(&be, &Limits::default()).unwrap()[0].row(0),
            &[0x12, 0xAB]
        );
        let le = encode_stream(Sync::V2LittleEndian, &[(spec, vec![row])]);
        assert_eq!(
            decode(&le, &Limits::default()).unwrap()[0].row(0),
            &[0x34, 0xCD]
        );
    }

    #[test]
    fn transforms_mirror_and_flip() {
        let mut spec = Spec::gray8(3, 2);
        spec.cross_feed = -1;
        spec.feed = -1;
        let rows = vec![vec![1, 2, 3], vec![4, 5, 6]];
        let stream = encode_stream(Sync::V2BigEndian, &[(spec, rows)]);
        let p = &decode(&stream, &Limits::default()).unwrap()[0];
        assert_eq!(p.row(0), &[6, 5, 4]);
        assert_eq!(p.row(1), &[3, 2, 1]);
        let h = &inspect(&stream).unwrap()[0];
        assert_eq!((h.cross_feed_transform, h.feed_transform), (-1, -1));
    }

    #[test]
    fn truncated_streams_are_reported_with_position() {
        let rows = sample_gray_rows();
        let stream = encode_stream(Sync::V2BigEndian, &[(Spec::gray8(384, 10), rows.clone())]);
        // Cut inside the pixel data.
        let cut = &stream[..stream.len() - 5];
        match decode(cut, &Limits::default()) {
            Err(RasterError::Truncated { page: 1, line }) => assert!(line < 10),
            other => panic!("expected Truncated, got {other:?}"),
        }
        // Cut inside the header (non-zero bytes remain).
        let cut = &stream[..4 + 100];
        assert!(matches!(
            decode(cut, &Limits::default()),
            Err(RasterError::Truncated { page: 1, line: 0 })
        ));
        // v3 truncated
        let stream3 = encode_stream(Sync::V3BigEndian, &[(Spec::gray8(384, 10), rows)]);
        let cut = &stream3[..stream3.len() - 384 * 3 - 1];
        assert!(matches!(
            decode(cut, &Limits::default()),
            Err(RasterError::Truncated { page: 1, line: 6 })
        ));
        // Zero padding after the last page is tolerated.
        let mut padded = stream.clone();
        padded.extend_from_slice(&[0u8; 300]);
        assert_eq!(decode(&padded, &Limits::default()).unwrap().len(), 1);
        let mut padded_full = stream;
        padded_full.extend_from_slice(&[0u8; HEADER_LEN + 10]);
        assert_eq!(decode(&padded_full, &Limits::default()).unwrap().len(), 1);
    }

    #[test]
    fn limits_are_enforced_before_allocating() {
        let rows = sample_gray_rows();
        let page = (Spec::gray8(384, 10), rows);
        let stream = encode_stream(
            Sync::V2BigEndian,
            &[page.clone(), page.clone(), page.clone()],
        );
        let l = Limits {
            max_pages: 2,
            ..Limits::default()
        };
        assert!(matches!(decode(&stream, &l), Err(RasterError::TooLarge(_))));
        let l = Limits {
            max_width: 383,
            ..Limits::default()
        };
        assert!(matches!(decode(&stream, &l), Err(RasterError::TooLarge(_))));
        let l = Limits {
            max_height: 9,
            ..Limits::default()
        };
        assert!(matches!(decode(&stream, &l), Err(RasterError::TooLarge(_))));
        let l = Limits {
            max_pixels_total: 384 * 10 * 2,
            ..Limits::default()
        };
        assert!(matches!(decode(&stream, &l), Err(RasterError::TooLarge(_))));
        let l = Limits {
            max_pixels_total: 384 * 10 * 3,
            ..Limits::default()
        };
        assert_eq!(decode(&stream, &l).unwrap().len(), 3);
        // A header claiming a gigantic page must fail without allocating.
        let huge = Spec::gray8(100_000, 100_000);
        let mut s = b"RaS2".to_vec();
        s.extend_from_slice(&header_bytes(&huge, true));
        assert!(matches!(
            decode(&s, &Limits::default()),
            Err(RasterError::TooLarge(_))
        ));
        // inspect() has no limits and simply reports the header (then hits truncation).
        assert!(matches!(inspect(&s), Err(RasterError::Truncated { .. })));
    }

    #[test]
    fn inconsistent_bytes_per_line_is_a_header_error() {
        let spec = Spec::gray8(384, 1);
        let mut h = header_bytes(&spec, true);
        put_u32(&mut h, OFF_BYTES_PER_LINE, 100, true);
        let mut s = b"RaS2".to_vec();
        s.append(&mut h);
        s.extend_from_slice(&[0, 0x7f, 255, 0x7f, 255, 0x7f, 255]);
        assert!(matches!(
            decode(&s, &Limits::default()),
            Err(RasterError::BadHeader(_))
        ));
        // Unsupported colourspace (CMYK = 6, 32 bpp).
        let mut cmyk = Spec::gray8(2, 1);
        cmyk.color_space = 6;
        cmyk.bits_per_pixel = 32;
        let s = encode_stream(Sync::V2BigEndian, &[(cmyk, vec![vec![0u8; 8]])]);
        assert!(matches!(
            decode(&s, &Limits::default()),
            Err(RasterError::Unsupported { cspace: 6, .. })
        ));
    }

    #[test]
    fn line_repeat_and_runs_are_clamped_like_cups() {
        // A page of 3 lines whose first line claims 255 repeats: CUPS clamps, so must we.
        let spec = Spec::gray8(4, 3);
        let mut s = b"RaS2".to_vec();
        s.extend_from_slice(&header_bytes(&spec, true));
        s.extend_from_slice(&[254, 3, 77]); // repeat 255×, run of 4 pixels value 77
        let p = &decode(&s, &Limits::default()).unwrap()[0];
        assert!(p.data.iter().all(|&v| v == 77));
        // A run longer than the line is clamped too.
        let mut s = b"RaS2".to_vec();
        s.extend_from_slice(&header_bytes(&spec, true));
        s.extend_from_slice(&[2, 100, 9]); // 3 lines, run of 101 pixels of 9 → clamped to 4
        let p = &decode(&s, &Limits::default()).unwrap()[0];
        assert!(p.data.iter().all(|&v| v == 9));
    }

    // ---- CUPS-generated fixtures (scripts/make-fixtures.sh) --------------------------------

    fn fixture(name: &str) -> Vec<u8> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");
        std::fs::read(format!("{path}{name}")).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
    }

    fn check_fixture(
        name: &str,
        pages: usize,
        width: u32,
        expect_size_name: &str,
    ) -> Vec<GrayPage> {
        let bytes = fixture(name);
        assert!(is_raster(&bytes));
        assert_eq!(&bytes[..4], b"RaS2", "{name}: PWG raster is big-endian v2");
        let hdrs = inspect(&bytes).unwrap();
        assert_eq!(hdrs.len(), pages, "{name}: page count");
        for h in &hdrs {
            assert_eq!(h.sync, Sync::V2BigEndian);
            assert_eq!(h.width, width, "{name}: cupsWidth");
            assert_eq!(h.bits_per_pixel, 8);
            assert_eq!(h.bits_per_color, 8);
            assert_eq!(h.color_space, CSPACE_SW);
            assert_eq!(h.hw_resolution, (203, 203));
            assert_eq!(h.page_size_name, expect_size_name);
            assert_eq!(h.output_type, "automatic");
        }
        let decoded = decode(&bytes, &Limits::default()).unwrap();
        assert_eq!(decoded.len(), pages);
        for (p, h) in decoded.iter().zip(&hdrs) {
            assert_eq!((p.width, p.height, p.dpi), (h.width, h.height, 203));
            assert_eq!(p.data.len(), (p.width * p.height) as usize);
            assert!(p.data.iter().any(|&v| v < 128), "{name}: page has ink");
            assert!(p.data.contains(&255), "{name}: page has white");
        }
        decoded
    }

    #[test]
    fn fixture_text_on_roll() {
        let pages = check_fixture("text-roll48.pwg", 1, 383, "47.98x297.04mm.Borderless");
        // A 48 mm roll page: 383 px ≈ 47.9 mm at 203 dpi.
        assert!((pages[0].width_mm() - 47.9).abs() < 0.2);
        assert_eq!(pages[0].height, 2374); // 297 mm
    }

    #[test]
    fn fixture_photo_on_roll() {
        check_fixture("photo-roll48.pwg", 1, 383, "47.98x297.04mm.Borderless");
    }

    #[test]
    fn fixture_a4_sheet() {
        let pages = check_fixture("onepage-a4-doc.pwg", 1, 1678, "A4.Borderless");
        assert!((pages[0].width_mm() - 210.0).abs() < 0.5);
    }

    #[test]
    fn fixture_two_up_two_copies_has_two_pages() {
        let pages = check_fixture("twoup-2copies.pwg", 2, 1678, "A4.Borderless");
        // Two copies of the same sheet must decode identically.
        assert_eq!(pages[0], pages[1]);
    }
}
