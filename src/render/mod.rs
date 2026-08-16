//! Image pipeline: gray pages → trimmed/fitted strip → dithered/quantized packed rows.
//!
//! Two stages so the job engine can decode/validate a document *before* the printer is found and
//! pack for the detected model afterwards:
//!   `prepare(pages, opts)` → `GrayStrip`   (trim, fit width, unsharp, concat, length cap)
//!   `pack(strip, opts, mode, width)` → `Packed` (tone/dither/quantize, rotate 180°, segment, pack)
//! Semantics ported from catprinter/render.py + img.py.

pub mod dither;
pub mod imagein;

use std::io::Cursor;
use std::ops::Range;

use image::{imageops::FilterType, GrayImage, ImageFormat};
use thiserror::Error;

use crate::protocol::PrintMode;
use crate::raster::GrayPage;

/// Style presets (render.py QUALITY_PRESETS). Selected from IPP print-quality / CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// FS dither, intensity 0x5D, trim, unsharp, curve 1.1/1.12, gray dither. IPP print-quality 4 (Normal).
    Default,
    /// FS dither, intensity 0x78, trim, unsharp, curve 1.6/1.22, gray dither. IPP print-quality 5 (High).
    Picture,
    /// Threshold, intensity 0x68, trim, no unsharp, curve 2.4/1.35. IPP print-quality 3 (Draft).
    Text,
    /// Threshold, intensity 0x68, NO trim (whole sheet miniature), unsharp, curve 2.4/1.35.
    Document,
}

impl Preset {
    /// render.py `normalize_quality` incl. aliases (auto/normal → default, photo/graphics/high → picture,
    /// draft → text, doc → document). Case-insensitive.
    pub fn parse(name: &str) -> Option<Preset> {
        match name.trim().to_ascii_lowercase().as_str() {
            "default" | "auto" | "normal" => Some(Preset::Default),
            "picture" | "photo" | "graphics" | "high" => Some(Preset::Picture),
            "text" | "draft" => Some(Preset::Text),
            "document" | "doc" => Some(Preset::Document),
            _ => None,
        }
    }
    pub fn dither(self) -> Dither {
        match self {
            Preset::Default | Preset::Picture => Dither::FloydSteinberg,
            Preset::Text | Preset::Document => Dither::Threshold,
        }
    }
    pub fn intensity(self) -> u8 {
        match self {
            Preset::Default => 0x5D,
            Preset::Picture => 0x78,
            Preset::Text | Preset::Document => 0x68,
        }
    }
    pub fn trim(self) -> bool {
        !matches!(self, Preset::Document)
    }
    pub fn unsharp(self) -> bool {
        !matches!(self, Preset::Text)
    }
    /// (contrast, midtone) for the thermal curve used in grayscale tone.
    pub fn curve(self) -> (f64, f64) {
        match self {
            Preset::Default => (1.1, 1.12),
            Preset::Picture => (1.6, 1.22),
            Preset::Text | Preset::Document => (2.4, 1.35),
        }
    }
    /// Whether grayscale tone uses serpentine 16-level FS (true) or plain quantization (false).
    pub fn gray_dither(self) -> bool {
        matches!(self, Preset::Default | Preset::Picture)
    }
    pub fn name(self) -> &'static str {
        match self {
            Preset::Default => "default",
            Preset::Picture => "picture",
            Preset::Text => "text",
            Preset::Document => "document",
        }
    }
}

/// Tone axis (render.py TONE_PRESETS): 1-bit or 16-level grayscale (only on models with 4bpp).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    BlackWhite,
    Grayscale,
}

impl Tone {
    /// render.py `normalize_tone` incl. aliases (bw, mono, monochrome, bilevel/bi-level, gray/grey/greyscale…).
    pub fn parse(name: &str) -> Option<Tone> {
        let k: String = name
            .trim()
            .to_ascii_lowercase()
            .chars()
            .filter(|c| !matches!(c, '_' | ' ' | '-'))
            .collect();
        match k.as_str() {
            "blackwhite" | "bw" | "blackandwhite" | "mono" | "monochrome" | "bilevel" => {
                Some(Tone::BlackWhite)
            }
            "grayscale" | "gray" | "grey" | "greyscale" => Some(Tone::Grayscale),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Tone::BlackWhite => "blackwhite",
            Tone::Grayscale => "grayscale",
        }
    }
}

/// 1-bit binarization method (render.py `binarize`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dither {
    FloydSteinberg,
    Atkinson,
    /// gray < 180 → black
    Threshold,
    /// gray < mean → black
    Mean,
    /// gray < 128 → black
    None,
}

impl Dither {
    pub fn parse(name: &str) -> Option<Dither> {
        match name.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "floyd-steinberg" | "fs" | "floydsteinberg" => Some(Dither::FloydSteinberg),
            "atkinson" => Some(Dither::Atkinson),
            "threshold" => Some(Dither::Threshold),
            "mean-threshold" | "mean" => Some(Dither::Mean),
            "none" => Some(Dither::None),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Dither::FloydSteinberg => "floyd-steinberg",
            Dither::Atkinson => "atkinson",
            Dither::Threshold => "threshold",
            Dither::Mean => "mean-threshold",
            Dither::None => "none",
        }
    }
}

/// How to lay pages out on the tape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Decide from the raster: page width ≤ 60 mm → Tape, else Sheet. Images → Tape.
    Auto,
    /// Trim white on all sides, then fit width (drawings, text on the roll).
    Tape,
    /// No trim, shrink the whole page to the head width (A4/Letter miniature).
    Sheet,
}

#[derive(Debug, Clone)]
pub struct RenderOptions {
    pub preset: Preset,
    pub tone: Tone,
    pub layout: Layout,
    pub dither_override: Option<Dither>,
    pub trim_override: Option<bool>,
    pub intensity_override: Option<u8>,
    /// Rotate the whole strip 180° before packing (the head prints "upside down"; default true).
    pub rotate_180: bool,
    /// Head width the strip is prepared for (384 for every known model).
    pub width_px: u32,
    /// Hard cap on the total strip length; exceeding it is an error, never a truncation.
    pub max_lines_total: u32,
    /// Max lines per print request (A9 line count is u16; conservative default 4000).
    pub max_lines_per_request: u32,
    /// White lines inserted between pages.
    pub page_gap_lines: u32,
}

impl Default for RenderOptions {
    fn default() -> Self {
        RenderOptions {
            preset: Preset::Default,
            tone: Tone::BlackWhite,
            layout: Layout::Auto,
            dither_override: None,
            trim_override: None,
            intensity_override: None,
            rotate_180: true,
            width_px: 384,
            max_lines_total: 8000,
            max_lines_per_request: 4000,
            page_gap_lines: 24,
        }
    }
}

impl RenderOptions {
    pub fn intensity(&self) -> u8 {
        self.intensity_override.unwrap_or(self.preset.intensity())
    }
}

/// Prepared (still 8-bit gray, unrotated, top-first) strip exactly `width_px` wide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrayStrip {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    /// Number of source pages that contributed.
    pub pages: u32,
    /// Layout that was actually applied.
    pub layout: Layout,
}

impl GrayStrip {
    pub fn as_page(&self) -> GrayPage {
        GrayPage {
            width: self.width,
            height: self.height,
            dpi: 203,
            data: self.data.clone(),
        }
    }
}

/// One print request worth of packed rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub lines: u32,
    /// Byte range into `Packed::data` (already padded to the family minimum, whole rows).
    pub bytes: Range<usize>,
}

/// Packed image data ready for a model driver.
///
/// `segments` are in SEND ORDER: the strip is rotated 180° as a whole (like the Python driver, which
/// sent one rotated buffer in one request), then cut into consecutive slices of the rotated buffer.
/// Sending them one after another reproduces exactly the physical row order of a single request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packed {
    pub mode: PrintMode,
    pub width: u32,
    pub lines: u32,
    pub row_bytes: usize,
    pub data: Vec<u8>,
    /// In print order (first element is sent first).
    pub segments: Vec<Segment>,
    /// What will come out of the printer, unrotated, for previews / fake printer (0 black, 255 white).
    pub preview: GrayPage,
}

#[derive(Debug, Error)]
pub enum RenderError {
    #[error("nothing to print (no pages)")]
    Empty,
    #[error("print would be {lines} lines ({mm} mm) long; limit is {max_lines} lines ({max_mm} mm) — split the document or pick a shorter page size")]
    TooLong {
        lines: u32,
        mm: u32,
        max_lines: u32,
        max_mm: u32,
    },
    #[error("unsupported image: {0}")]
    Image(String),
    #[error("internal render error: {0}")]
    Internal(String),
}

/// render.py TRIM_WHITE: pixels darker than this count as ink.
pub const TRIM_WHITE: u8 = 245;
/// render.py TRIM_PAD at 203 dpi.
pub const TRIM_PAD: u32 = 8;
/// A page whose width is at most this many millimetres is tape (roll) media.
pub const TAPE_MAX_WIDTH_MM: f64 = 60.0;
/// MXW01 minimum print length in lines (protocol.py MIN_DATA_BYTES = 90 rows).
pub const MIN_LINES: u32 = 90;
/// Hard protocol limit on lines per print request (A9 line count is u16).
pub const MAX_LINES_PER_REQUEST_HW: u32 = 65535;
/// Row count of the strip produced for an all-white document (render.py trim_whitespace).
pub const BLANK_STRIP_LINES: u32 = 16;

/// Lines → millimetres at 203 dpi, rounded.
pub fn lines_to_mm(lines: u32) -> u32 {
    (lines as f64 * 25.4 / 203.0).round() as u32
}

/// Stage 1: trim / fit / unsharp / concatenate; validates length. Pages must share `dpi` semantics.
pub fn prepare(pages: Vec<GrayPage>, opts: &RenderOptions) -> Result<GrayStrip, RenderError> {
    if pages.is_empty() || opts.width_px == 0 {
        return Err(RenderError::Empty);
    }
    for p in &pages {
        if p.width == 0 || p.height == 0 || p.data.len() != (p.width as usize) * (p.height as usize)
        {
            return Err(RenderError::Internal(format!(
                "page buffer {}x{} has {} bytes",
                p.width,
                p.height,
                p.data.len()
            )));
        }
    }
    let layout = match opts.layout {
        Layout::Auto => {
            let first = &pages[0];
            if first.dpi == 0 || first.width_mm() <= TAPE_MAX_WIDTH_MM {
                Layout::Tape
            } else {
                Layout::Sheet
            }
        }
        l => l,
    };
    let do_trim = opts
        .trim_override
        .unwrap_or(layout == Layout::Tape && opts.preset.trim());

    // Blank pages carry nothing; drop them unless everything is blank.
    let inked: Vec<&GrayPage> = pages.iter().filter(|p| has_ink(&p.data)).collect();
    if inked.is_empty() {
        let w = opts.width_px;
        return Ok(GrayStrip {
            width: w,
            height: BLANK_STRIP_LINES,
            data: vec![255; (w * BLANK_STRIP_LINES) as usize],
            pages: 1,
            layout,
        });
    }

    // Column bounds are unioned across pages so every page shares one horizontal scale.
    let mut crops: Vec<(Range<u32>, Range<u32>)> = Vec::with_capacity(inked.len()); // (rows, cols) per page
    if do_trim {
        let mut c0 = u32::MAX;
        let mut c1 = 0u32;
        let mut rows_per_page = Vec::with_capacity(inked.len());
        for p in &inked {
            let pad = trim_pad(p.dpi);
            let (r, c) = ink_bounds(p, pad).expect("inked page has bounds");
            c0 = c0.min(c.start);
            c1 = c1.max(c.end);
            rows_per_page.push(r);
        }
        // Union of column ranges must be applied per page but clamped to each page's own width.
        for (p, r) in inked.iter().zip(rows_per_page) {
            crops.push((r, c0..c1.min(p.width)));
        }
    } else {
        for p in &inked {
            crops.push((0..p.height, 0..p.width));
        }
    }

    // Per page: crop → fit width → unsharp.
    let mut fitted: Vec<(u32, u32, Vec<u8>)> = Vec::with_capacity(inked.len());
    for (p, (rows, cols)) in inked.iter().zip(crops) {
        let cropped = crop(p, rows.clone(), cols.clone());
        let (w, h, data) = fit_width(cropped.0, cropped.1, &cropped.2, opts.width_px);
        let data = if opts.preset.unsharp() {
            dither::unsharp(&data, w as usize, h as usize, 1.0, 55, 2)
        } else {
            data
        };
        fitted.push((w, h, data));
    }

    // Concatenate with gaps; enforce the total cap (never truncate).
    let gap = if fitted.len() > 1 {
        opts.page_gap_lines
    } else {
        0
    };
    let total_lines: u64 = fitted.iter().map(|(_, h, _)| *h as u64).sum::<u64>()
        + gap as u64 * (fitted.len() as u64 - 1);
    if total_lines > opts.max_lines_total as u64 {
        let lines = total_lines.min(u32::MAX as u64) as u32;
        return Err(RenderError::TooLong {
            lines,
            mm: lines_to_mm(lines),
            max_lines: opts.max_lines_total,
            max_mm: lines_to_mm(opts.max_lines_total),
        });
    }
    let w = opts.width_px;
    let mut data = Vec::with_capacity((total_lines as usize) * (w as usize));
    for (i, (_, _, page_data)) in fitted.iter().enumerate() {
        if i > 0 && gap > 0 {
            data.extend(std::iter::repeat_n(255u8, (gap * w) as usize));
        }
        data.extend_from_slice(page_data);
    }
    Ok(GrayStrip {
        width: w,
        height: total_lines as u32,
        data,
        pages: fitted.len() as u32,
        layout,
    })
}

/// Stage 2: tone/dither/quantize for `mode`, rotate, segment, pack for a head `width_px` wide
/// (resample if the strip width differs). `Gray4` is only valid on models with 4bpp support.
pub fn pack(
    strip: &GrayStrip,
    opts: &RenderOptions,
    mode: PrintMode,
    width_px: u32,
) -> Result<Packed, RenderError> {
    if strip.width == 0 || strip.height == 0 || width_px == 0 {
        return Err(RenderError::Empty);
    }
    if strip.data.len() != (strip.width as usize) * (strip.height as usize) {
        return Err(RenderError::Internal("strip buffer size mismatch".into()));
    }
    let (w, h, gray) = if strip.width == width_px {
        (strip.width, strip.height, strip.data.clone())
    } else {
        fit_width(strip.width, strip.height, &strip.data, width_px)
    };
    let (wu, hu) = (w as usize, h as usize);

    // Tone → per-pixel level 0 (white) .. 15 (black); mono is 0/15.
    let (levels, preview): (Vec<u8>, Vec<u8>) = match mode {
        PrintMode::Gray4 => {
            let (contrast, midtone) = opts.preset.curve();
            let curved = dither::thermal_curve(&gray, contrast, midtone);
            let levels = if opts.preset.gray_dither() {
                dither::quantize_16_serpentine_fs(&curved, wu, hu)
            } else {
                dither::quantize_16(&curved)
            };
            let preview = levels.iter().map(|&l| 255 - l * 17).collect();
            (levels, preview)
        }
        PrintMode::Mono => {
            let d = opts.dither_override.unwrap_or(opts.preset.dither());
            let black = binarize(&gray, wu, hu, d);
            let levels = black.iter().map(|&b| if b { 15u8 } else { 0 }).collect();
            let preview = black.iter().map(|&b| if b { 0u8 } else { 255 }).collect();
            (levels, preview)
        }
    };
    let preview = GrayPage {
        width: w,
        height: h,
        dpi: 203,
        data: preview,
    };

    // Rotate 180° (rows reversed and each row reversed) — the head prints "upside down".
    let levels = if opts.rotate_180 {
        levels.into_iter().rev().collect::<Vec<u8>>()
    } else {
        levels
    };

    let row_bytes = mode.bytes_per_row(w);
    let max_per_req = opts
        .max_lines_per_request
        .clamp(1, MAX_LINES_PER_REQUEST_HW);
    let mut data = Vec::with_capacity((hu + MIN_LINES as usize) * row_bytes);
    let mut segments = Vec::new();
    let mut y = 0u32;
    while y < h {
        let take = (h - y).min(max_per_req);
        let start = data.len();
        for row in y..y + take {
            let row_levels = &levels[row as usize * wu..(row as usize + 1) * wu];
            pack_row(mode, row_levels, &mut data);
        }
        let mut lines = take;
        if lines < MIN_LINES {
            data.extend(std::iter::repeat_n(
                0u8,
                ((MIN_LINES - lines) as usize) * row_bytes,
            ));
            lines = MIN_LINES;
        }
        segments.push(Segment {
            lines,
            bytes: start..data.len(),
        });
        y += take;
    }
    let lines = segments.iter().map(|s| s.lines).sum();
    Ok(Packed {
        mode,
        width: w,
        lines,
        row_bytes,
        data,
        segments,
        preview,
    })
}

/// Convenience: choose the wire mode from tone + model capability.
pub fn mode_for(tone: Tone, grayscale_4bpp: bool) -> PrintMode {
    match (tone, grayscale_4bpp) {
        (Tone::Grayscale, true) => PrintMode::Gray4,
        _ => PrintMode::Mono,
    }
}

/// Encode a gray page as PNG bytes (previews over HTTP, `print --preview-only`).
pub fn preview_png_bytes(page: &GrayPage) -> Result<Vec<u8>, RenderError> {
    let img = GrayImage::from_raw(page.width, page.height, page.data.clone())
        .ok_or_else(|| RenderError::Internal("preview buffer size mismatch".into()))?;
    let mut buf = Cursor::new(Vec::new());
    img.write_to(&mut buf, ImageFormat::Png)
        .map_err(|e| RenderError::Internal(format!("png encode: {e}")))?;
    Ok(buf.into_inner())
}

/// render.py `binarize`: gray → true = black.
pub fn binarize(gray: &[u8], width: usize, height: usize, d: Dither) -> Vec<bool> {
    match d {
        Dither::FloydSteinberg => dither::floyd_steinberg(gray, width, height),
        Dither::Atkinson => dither::atkinson(gray, width, height),
        Dither::Threshold => dither::threshold(gray, 180),
        Dither::Mean => dither::mean_threshold(gray),
        Dither::None => dither::threshold(gray, 128),
    }
}

/// Pack one row of levels (0 white .. 15 black) for `mode` and append to `out`.
fn pack_row(mode: PrintMode, levels: &[u8], out: &mut Vec<u8>) {
    match mode {
        PrintMode::Mono => {
            for chunk in levels.chunks(8) {
                let mut b = 0u8;
                for (bit, &l) in chunk.iter().enumerate() {
                    if l >= 8 {
                        b |= 1 << bit; // LSB = leftmost pixel
                    }
                }
                out.push(b);
            }
        }
        PrintMode::Gray4 => {
            for pair in levels.chunks(2) {
                let hi = pair[0] & 0x0F;
                let lo = pair.get(1).copied().unwrap_or(0) & 0x0F;
                out.push((hi << 4) | lo); // even x = high nibble
            }
        }
    }
}

fn has_ink(data: &[u8]) -> bool {
    data.iter().any(|&v| v < TRIM_WHITE)
}

/// render.py TRIM_PAD scaled with resolution, never below 8 px.
fn trim_pad(dpi: u32) -> u32 {
    if dpi == 0 {
        TRIM_PAD
    } else {
        ((TRIM_PAD as f64 * dpi as f64 / 203.0).round() as u32).max(TRIM_PAD)
    }
}

/// Ink bounding box (rows, cols) with `pad`, clamped to the page. None if the page is blank.
fn ink_bounds(p: &GrayPage, pad: u32) -> Option<(Range<u32>, Range<u32>)> {
    let w = p.width as usize;
    let mut r0 = None;
    let mut r1 = 0usize;
    let mut c0 = w;
    let mut c1 = 0usize;
    for y in 0..p.height as usize {
        let row = &p.data[y * w..(y + 1) * w];
        let first = row.iter().position(|&v| v < TRIM_WHITE);
        if let Some(f) = first {
            let last = row.iter().rposition(|&v| v < TRIM_WHITE).unwrap();
            if r0.is_none() {
                r0 = Some(y);
            }
            r1 = y;
            c0 = c0.min(f);
            c1 = c1.max(last);
        }
    }
    let r0 = r0?;
    let pad = pad as usize;
    let rows = r0.saturating_sub(pad) as u32..((r1 + pad + 1).min(p.height as usize)) as u32;
    let cols = c0.saturating_sub(pad) as u32..((c1 + pad + 1).min(w)) as u32;
    Some((rows, cols))
}

/// Crop `p` to rows × cols → (w, h, data).
fn crop(p: &GrayPage, rows: Range<u32>, cols: Range<u32>) -> (u32, u32, Vec<u8>) {
    let w = (cols.end - cols.start) as usize;
    let h = (rows.end - rows.start) as usize;
    let mut out = Vec::with_capacity(w * h);
    for y in rows {
        let row = p.row(y);
        out.extend_from_slice(&row[cols.start as usize..cols.end as usize]);
    }
    (w as u32, h as u32, out)
}

/// render.py `fit_width`: make the width exactly `width` (Lanczos3 both directions), except that
/// widths within ±8 px are centre-padded / cropped instead of resampled (blur-free for 383/384-px
/// tape rasters). Height keeps the aspect ratio (min 1).
pub fn fit_width(w: u32, h: u32, data: &[u8], width: u32) -> (u32, u32, Vec<u8>) {
    if w == width {
        return (w, h, data.to_vec());
    }
    let diff = w as i64 - width as i64;
    if diff.abs() <= 8 {
        let (wu, hu) = (w as usize, h as usize);
        let mut out = Vec::with_capacity(width as usize * hu);
        if diff < 0 {
            // pad: left gets floor, right gets the rest
            let pad_total = (-diff) as usize;
            let left = pad_total / 2;
            let right = pad_total - left;
            for y in 0..hu {
                out.extend(std::iter::repeat_n(255u8, left));
                out.extend_from_slice(&data[y * wu..(y + 1) * wu]);
                out.extend(std::iter::repeat_n(255u8, right));
            }
        } else {
            let cut_total = diff as usize;
            let left = cut_total / 2;
            for y in 0..hu {
                out.extend_from_slice(&data[y * wu + left..y * wu + left + width as usize]);
            }
        }
        return (width, h, out);
    }
    let new_h = ((h as f64 * width as f64 / w as f64).round() as u32).max(1);
    let img = GrayImage::from_raw(w, h, data.to_vec()).expect("buffer size checked by callers");
    let resized = image::imageops::resize(&img, width, new_h, FilterType::Lanczos3);
    (width, new_h, resized.into_raw())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn white(w: u32, h: u32, dpi: u32) -> GrayPage {
        GrayPage::new_white(w, h, dpi)
    }

    fn fill(p: &mut GrayPage, x0: u32, y0: u32, x1: u32, y1: u32, v: u8) {
        for y in y0..y1 {
            for x in x0..x1 {
                let i = (y * p.width + x) as usize;
                p.data[i] = v;
            }
        }
    }

    fn opts(preset: Preset, tone: Tone) -> RenderOptions {
        RenderOptions {
            preset,
            tone,
            ..Default::default()
        }
    }

    fn doodle_page() -> GrayPage {
        // ~A4 @ 100 dpi (800×1100): sine stroke + gray blob (mirrors test_render.py _doodle_png)
        let mut p = white(800, 1100, 100);
        for x in 200..600u32 {
            for t in -3i32..4 {
                let y = 400 + (40.0 * ((x as f64) / 30.0).sin()) as i32 + t;
                if (0..1100).contains(&y) {
                    p.data[(y as u32 * 800 + x) as usize] = 0;
                }
            }
        }
        fill(&mut p, 300, 700, 460, 820, 90);
        p
    }

    #[test]
    fn preset_and_tone_aliases() {
        assert_eq!(Preset::parse("Picture"), Some(Preset::Picture));
        assert_eq!(Preset::parse("photo"), Some(Preset::Picture));
        assert_eq!(Preset::parse("TEXT"), Some(Preset::Text));
        assert_eq!(Preset::parse("Document"), Some(Preset::Document));
        assert_eq!(Preset::parse("doc"), Some(Preset::Document));
        assert_eq!(Preset::parse("auto"), Some(Preset::Default));
        assert_eq!(Preset::parse("neon"), None);
        assert_eq!(Tone::parse("Grayscale"), Some(Tone::Grayscale));
        assert_eq!(Tone::parse("BlackWhite"), Some(Tone::BlackWhite));
        assert_eq!(Tone::parse("grey"), Some(Tone::Grayscale));
        assert_eq!(Tone::parse("mono"), Some(Tone::BlackWhite));
        assert_eq!(Tone::parse("bi-level"), Some(Tone::BlackWhite));
        assert_eq!(Tone::parse("sepia"), None);
        assert_eq!(
            Dither::parse("floyd-steinberg"),
            Some(Dither::FloydSteinberg)
        );
        assert_eq!(Dither::parse("mean_threshold"), Some(Dither::Mean));
    }

    #[test]
    fn preset_table_matches_python() {
        assert_eq!(Preset::Default.intensity(), 0x5D);
        assert_eq!(Preset::Picture.intensity(), 0x78);
        assert_eq!(Preset::Text.intensity(), 0x68);
        assert!(!Preset::Document.trim());
        assert!(!Preset::Text.unsharp());
        assert_eq!(Preset::Picture.curve(), (1.6, 1.22));
        assert!(!Preset::Text.gray_dither());
        assert_eq!(mode_for(Tone::Grayscale, true), PrintMode::Gray4);
        assert_eq!(mode_for(Tone::Grayscale, false), PrintMode::Mono);
        assert_eq!(mode_for(Tone::BlackWhite, true), PrintMode::Mono);
    }

    #[test]
    fn trim_doodle_on_a4_crops_tight() {
        // small black mark on a big white page → strip is far shorter than the fitted full page
        let mut p = white(1100, 800, 100); // 279 mm wide → Auto would be Sheet; force Tape
        fill(&mut p, 300, 400, 500, 520, 0);
        let o = RenderOptions {
            layout: Layout::Tape,
            ..opts(Preset::Default, Tone::BlackWhite)
        };
        let s = prepare(vec![p], &o).unwrap();
        assert_eq!(s.width, 384);
        // crop is 216 wide × 136 tall (200+2*8, 120+2*8) → fitted height = 136*384/216 ≈ 242
        assert!(s.height > 200 && s.height < 260, "height {}", s.height);
        assert!(s.data.iter().any(|&v| v < 10));
        assert_eq!(s.layout, Layout::Tape);
    }

    #[test]
    fn all_white_is_a_16_row_strip() {
        let s = prepare(
            vec![white(200, 200, 203)],
            &opts(Preset::Default, Tone::BlackWhite),
        )
        .unwrap();
        assert_eq!((s.width, s.height), (384, BLANK_STRIP_LINES));
        assert!(s.data.iter().all(|&v| v == 255));
        assert_eq!(s.pages, 1);
    }

    #[test]
    fn fit_width_shrinks_grows_and_pads() {
        let (w, h, _) = fit_width(800, 100, &vec![0u8; 800 * 100], 384);
        assert_eq!(w, 384);
        assert!(h > 1);
        let (w, h, d) = fit_width(40, 20, &vec![0u8; 800], 384);
        assert_eq!((w, h), (384, 192));
        assert_eq!(d.len(), 384 * 192);
        // near-384: pad, not resample (a 1-px black column stays exactly 1 px)
        let mut narrow = vec![255u8; 380 * 4];
        for y in 0..4 {
            narrow[y * 380 + 10] = 0;
        }
        let (w, h, d) = fit_width(380, 4, &narrow, 384);
        assert_eq!((w, h), (384, 4));
        assert_eq!(d[12], 0); // shifted by 2 (left pad = 4/2)
        assert_eq!(d.iter().filter(|&&v| v == 0).count(), 4);
        // near-384: crop
        let wide = vec![0u8; 388 * 2];
        let (w, h, d) = fit_width(388, 2, &wide, 384);
        assert_eq!((w, h), (384, 2));
        assert_eq!(d.len(), 384 * 2);
    }

    #[test]
    fn text_threshold_keeps_ink_solid() {
        let mut g = vec![255u8; 384 * 32];
        for y in 8..24 {
            for x in 40..80 {
                g[y * 384 + x] = 20;
            }
        }
        let bits = binarize(&g, 384, 32, Dither::Threshold);
        assert!(bits[12 * 384 + 50]);
        assert!(!bits[0]);
    }

    #[test]
    fn png_default_is_384_and_has_ink() {
        let o = RenderOptions {
            layout: Layout::Tape,
            ..opts(Preset::Default, Tone::BlackWhite)
        };
        let s = prepare(vec![doodle_page()], &o).unwrap();
        assert_eq!(s.width, 384);
        assert!(s.height < 600 && s.height > 20, "{}", s.height);
        let p = pack(&s, &o, PrintMode::Mono, 384).unwrap();
        assert!(p.preview.data.contains(&0));
        assert!(p.preview.data.iter().all(|&v| v == 0 || v == 255));
        assert_eq!(p.row_bytes, 48);
        assert_eq!(p.data.len(), p.lines as usize * 48);
    }

    #[test]
    fn document_keeps_full_page_default_trims() {
        // header + footer, nothing in the middle
        let mut sheet = white(800, 1100, 100);
        fill(&mut sheet, 100, 20, 700, 40, 0);
        fill(&mut sheet, 100, 1060, 700, 1080, 0);
        let full = prepare(
            vec![sheet.clone()],
            &opts(Preset::Document, Tone::BlackWhite),
        )
        .unwrap();
        assert_eq!(full.layout, Layout::Sheet); // 800 px @100 dpi = 203 mm → sheet
        assert_eq!((full.width, full.height), (384, 528)); // 1100 * 384/800
        let p = pack(
            &full,
            &opts(Preset::Document, Tone::BlackWhite),
            PrintMode::Mono,
            384,
        )
        .unwrap();
        assert!(p.preview.data[..30 * 384].contains(&0));
        assert!(p.preview.data[(528 - 30) * 384..].contains(&0));

        // A small mark in the middle: Tape/Default crops, Sheet/Document does not.
        let mut page = white(800, 1100, 100);
        fill(&mut page, 360, 520, 440, 580, 0);
        let trimmed = prepare(
            vec![page.clone()],
            &RenderOptions {
                layout: Layout::Tape,
                ..opts(Preset::Default, Tone::BlackWhite)
            },
        )
        .unwrap();
        let whole = prepare(vec![page], &opts(Preset::Document, Tone::BlackWhite)).unwrap();
        assert!(trimmed.height < 400, "{}", trimmed.height);
        assert_eq!(whole.height, 528);
        assert!(whole.height > trimmed.height);
    }

    #[test]
    fn auto_layout_from_page_width() {
        // 384 px @ 203 dpi = 48 mm → tape; 1678 px @ 203 = 210 mm → sheet
        let mut tape = white(384, 100, 203);
        fill(&mut tape, 10, 10, 20, 20, 0);
        assert_eq!(
            prepare(vec![tape], &opts(Preset::Default, Tone::BlackWhite))
                .unwrap()
                .layout,
            Layout::Tape
        );
        let mut a4 = white(1678, 300, 203);
        fill(&mut a4, 10, 10, 20, 20, 0);
        let s = prepare(vec![a4], &opts(Preset::Default, Tone::BlackWhite)).unwrap();
        assert_eq!(s.layout, Layout::Sheet);
        assert_eq!(s.height, (300.0 * 384.0 / 1678.0f64).round() as u32);
        // images (dpi 0 not used; imagein gives 203 but width small) → tape
        let mut img = white(100, 50, 0);
        fill(&mut img, 0, 0, 10, 10, 0);
        assert_eq!(
            prepare(vec![img], &opts(Preset::Default, Tone::BlackWhite))
                .unwrap()
                .layout,
            Layout::Tape
        );
    }

    #[test]
    fn multipage_gap_blank_drop_and_column_union() {
        let mut p1 = white(384, 100, 203);
        fill(&mut p1, 20, 10, 40, 30, 0); // ink at x 20..40
        let blank = white(384, 100, 203);
        let mut p2 = white(384, 100, 203);
        fill(&mut p2, 300, 10, 340, 30, 0); // ink at x 300..340
        let o = RenderOptions {
            page_gap_lines: 24,
            ..opts(Preset::Text, Tone::BlackWhite)
        };
        let s = prepare(vec![p1, blank, p2], &o).unwrap();
        assert_eq!(s.pages, 2);
        // union of columns = 12..348 (with pad 8) → 336 wide → fitted to 384; rows: each page 36 tall
        // (20 + 2*8) → 36*384/336 ≈ 41 lines each + 24 gap
        let per_page = (36.0 * 384.0 / 336.0f64).round() as u32;
        assert_eq!(s.height, per_page * 2 + 24, "{}", s.height);
        // ink appears near the left in the first page and near the right in the second
        let first = &s.data[..(per_page * 384) as usize];
        assert!(first.iter().any(|&v| v < 128));
        let gap = &s.data[(per_page * 384) as usize..((per_page + 24) * 384) as usize];
        assert!(gap.iter().all(|&v| v == 255));
    }

    #[test]
    fn too_long_is_an_error_not_a_truncation() {
        let mut tall = white(384, 20000, 203);
        fill(&mut tall, 0, 0, 384, 20000, 0);
        let err = prepare(vec![tall], &opts(Preset::Default, Tone::BlackWhite)).unwrap_err();
        match err {
            RenderError::TooLong {
                lines,
                max_lines,
                mm,
                max_mm,
            } => {
                assert_eq!(lines, 20000);
                assert_eq!(max_lines, 8000);
                assert_eq!(mm, lines_to_mm(20000));
                assert_eq!(max_mm, 1001);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn segments_split_pad_and_keep_send_order() {
        // 250 lines, 100 per request → 3 segments (100, 100, 50→padded 90)
        let mut strip_page = white(384, 250, 203);
        fill(&mut strip_page, 0, 0, 384, 1, 0); // first row black (top of the unrotated strip)
        let o = RenderOptions {
            max_lines_per_request: 100,
            rotate_180: true,
            trim_override: Some(false),
            ..opts(Preset::Text, Tone::BlackWhite)
        };
        let s = prepare(vec![strip_page], &o).unwrap();
        assert_eq!(s.height, 250);
        let p = pack(&s, &o, PrintMode::Mono, 384).unwrap();
        assert_eq!(p.segments.len(), 3);
        assert_eq!(p.segments[0].lines, 100);
        assert_eq!(p.segments[1].lines, 100);
        assert_eq!(p.segments[2].lines, MIN_LINES);
        assert_eq!(p.lines, 290);
        assert_eq!(p.data.len(), 290 * 48);
        for s in &p.segments {
            assert_eq!(s.bytes.len(), s.lines as usize * 48);
        }
        // rotated: the black top row is now the LAST row of the buffer → last real row of segment 3
        let last_seg = &p.data[p.segments[2].bytes.clone()];
        let last_real_row = &last_seg[49 * 48..50 * 48];
        assert!(last_real_row.iter().all(|&b| b == 0xFF));
        let first_row = &p.data[..48];
        assert!(first_row.iter().all(|&b| b == 0));
        // padding rows are white (zero bytes)
        assert!(last_seg[50 * 48..].iter().all(|&b| b == 0));
        // preview is unrotated: black row first
        assert!(p.preview.data[..384].iter().all(|&v| v == 0));
        assert!(p.preview.data[384..].iter().all(|&v| v == 255));
    }

    #[test]
    fn rotate_180_reverses_rows_and_pixels() {
        let mut page = white(384, 100, 203);
        fill(&mut page, 0, 0, 8, 1, 0); // 8 black pixels top-left
        let o = RenderOptions {
            trim_override: Some(false),
            ..opts(Preset::Text, Tone::BlackWhite)
        };
        let s = prepare(vec![page], &o).unwrap();
        let p = pack(&s, &o, PrintMode::Mono, 384).unwrap();
        // after rotation the ink is bottom-right: last row, last byte all bits set
        let last_row = &p.data[99 * 48..100 * 48];
        assert_eq!(last_row[47], 0xFF);
        assert!(last_row[..47].iter().all(|&b| b == 0));
        assert!(p.data[..99 * 48].iter().all(|&b| b == 0));
        // no rotation: first row, first byte
        let o2 = RenderOptions {
            rotate_180: false,
            ..o
        };
        let p2 = pack(&s, &o2, PrintMode::Mono, 384).unwrap();
        assert_eq!(p2.data[0], 0xFF);
        assert!(p2.data[1..].iter().all(|&b| b == 0));
    }

    #[test]
    fn mono_bit_order_is_lsb_leftmost() {
        let mut page = white(384, 100, 203);
        fill(&mut page, 0, 0, 1, 1, 0); // single black pixel at x=0
        let o = RenderOptions {
            rotate_180: false,
            trim_override: Some(false),
            ..opts(Preset::Text, Tone::BlackWhite)
        };
        let s = prepare(vec![page], &o).unwrap();
        let p = pack(&s, &o, PrintMode::Mono, 384).unwrap();
        assert_eq!(p.data[0], 0x01);
        let mut page = white(384, 100, 203);
        fill(&mut page, 7, 0, 8, 1, 0); // x=7 → MSB of first byte
        let s = prepare(vec![page], &o).unwrap();
        let p = pack(&s, &o, PrintMode::Mono, 384).unwrap();
        assert_eq!(p.data[0], 0x80);
    }

    #[test]
    fn gray4_pack_nibbles_and_padding() {
        // even x = high nibble: pixel 0 black (15), pixel 1 white (0) → 0xF0
        let mut page = white(384, 100, 203);
        fill(&mut page, 0, 0, 1, 1, 0);
        let o = RenderOptions {
            rotate_180: false,
            trim_override: Some(false),
            ..opts(Preset::Text, Tone::Grayscale)
        };
        let s = prepare(vec![page], &o).unwrap();
        let p = pack(&s, &o, PrintMode::Gray4, 384).unwrap();
        assert_eq!(p.mode, PrintMode::Gray4);
        assert_eq!(p.row_bytes, 192);
        assert_eq!(p.data[0], 0xF0);
        assert_eq!(p.data.len() % 192, 0);
        assert!(p.data.len() >= 17280);
        assert_eq!(p.lines, 100);
        // preview levels map back to gray: black pixel 0, white 255
        assert_eq!(p.preview.data[0], 0);
        assert_eq!(p.preview.data[1], 255);
    }

    #[test]
    fn picture_grayscale_is_4bpp_and_uses_levels() {
        let mut page = white(200, 80, 203);
        fill(&mut page, 0, 0, 200, 80, 130);
        fill(&mut page, 20, 40, 80, 41, 10);
        let o = opts(Preset::Picture, Tone::Grayscale);
        let s = prepare(vec![page], &o).unwrap();
        assert_eq!(s.width, 384);
        let p = pack(&s, &o, PrintMode::Gray4, 384).unwrap();
        assert_eq!(p.data.len() % 192, 0);
        assert!(p.data.len() >= 17280);
        // mid-gray background must use intermediate levels (not just 0/15)
        let mut levels: Vec<u8> = p.data.iter().flat_map(|&b| [b >> 4, b & 0x0F]).collect();
        levels.sort_unstable();
        levels.dedup();
        assert!(levels.iter().any(|&l| (1..15).contains(&l)), "{levels:?}");
    }

    #[test]
    fn text_grayscale_keeps_solid_ink() {
        let mut page = white(384, 64, 203);
        fill(&mut page, 40, 16, 80, 48, 0);
        let o = RenderOptions {
            rotate_180: false,
            trim_override: Some(false),
            ..opts(Preset::Text, Tone::Grayscale)
        };
        let s = prepare(vec![page], &o).unwrap();
        let p = pack(&s, &o, PrintMode::Gray4, 384).unwrap();
        let level = |x: usize, y: usize| {
            let b = p.data[y * 192 + x / 2];
            if x % 2 == 0 {
                b >> 4
            } else {
                b & 0x0F
            }
        };
        assert!(level(60, 32) >= 12, "{}", level(60, 32));
        assert!(level(2, 2) <= 1, "{}", level(2, 2));
    }

    #[test]
    fn default_tone_is_still_1bpp() {
        let mut page = white(80, 40, 203);
        fill(&mut page, 0, 0, 80, 40, 0);
        let o = opts(Preset::Default, Tone::BlackWhite);
        let s = prepare(vec![page], &o).unwrap();
        let p = pack(&s, &o, mode_for(o.tone, true), 384).unwrap();
        assert_eq!(p.mode, PrintMode::Mono);
        assert!(p.preview.data.iter().all(|&v| v == 0 || v == 255));
    }

    #[test]
    fn pack_resamples_to_model_width() {
        let mut page = white(384, 50, 203);
        fill(&mut page, 0, 0, 384, 50, 0);
        let o = RenderOptions {
            trim_override: Some(false),
            ..opts(Preset::Text, Tone::BlackWhite)
        };
        let s = prepare(vec![page], &o).unwrap();
        let p = pack(&s, &o, PrintMode::Mono, 576).unwrap();
        assert_eq!(p.width, 576);
        assert_eq!(p.row_bytes, 72);
        assert_eq!(p.preview.width, 576);
        assert_eq!(p.preview.height, 75);
    }

    #[test]
    fn preview_png_roundtrip() {
        let mut page = white(16, 8, 203);
        fill(&mut page, 0, 0, 8, 8, 0);
        let bytes = preview_png_bytes(&page).unwrap();
        assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G']));
        let back = imagein::load(&bytes).unwrap();
        assert_eq!(back.data, page.data);
    }

    #[test]
    fn end_to_end_repo_jpeg() {
        let bytes =
            std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/media/hackoclock.jpg")).unwrap();
        let page = imagein::load(&bytes).unwrap();
        let o = opts(Preset::Default, Tone::BlackWhite);
        let s = prepare(vec![page], &o).unwrap();
        assert_eq!(s.width, 384);
        assert_eq!(s.layout, Layout::Tape);
        assert!(s.height > 50 && s.height <= 8000, "{}", s.height);
        let p = pack(&s, &o, PrintMode::Mono, 384).unwrap();
        assert!(p.preview.data.contains(&0));
        assert_eq!(p.data.len(), p.lines as usize * 48);
        let g = pack(
            &s,
            &RenderOptions {
                preset: Preset::Picture,
                tone: Tone::Grayscale,
                ..o
            },
            PrintMode::Gray4,
            384,
        )
        .unwrap();
        assert_eq!(g.data.len(), g.lines as usize * 192);
        assert_eq!(g.preview.width, 384);
    }
}
