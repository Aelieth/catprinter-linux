//! Image pipeline: gray pages → trimmed/fitted strip → dithered/quantized packed rows.
//!
//! Two stages so the job engine can decode/validate a document *before* the printer is found and
//! pack for the detected model afterwards:
//!   `prepare(pages, opts)` → `GrayStrip`   (trim, fit width, unsharp, concat, length cap)
//!   `pack(strip, opts, mode, width)` → `Packed` (tone/dither/quantize, rotate 180°, segment, pack)
//! Semantics ported from catprinter/render.py + img.py.

pub mod dither;
pub mod imagein;

use std::ops::Range;

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
            "blackwhite" | "bw" | "blackandwhite" | "mono" | "monochrome" | "bilevel" => Some(Tone::BlackWhite),
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
        GrayPage { width: self.width, height: self.height, dpi: 203, data: self.data.clone() }
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
    TooLong { lines: u32, mm: u32, max_lines: u32, max_mm: u32 },
    #[error("unsupported image: {0}")]
    Image(String),
    #[error("internal render error: {0}")]
    Internal(String),
}

/// Stage 1: trim / fit / unsharp / concatenate; validates length. Pages must share `dpi` semantics.
pub fn prepare(pages: Vec<GrayPage>, opts: &RenderOptions) -> Result<GrayStrip, RenderError> {
    let _ = (pages, opts);
    todo!("render::prepare — implemented in the render milestone")
}

/// Stage 2: tone/dither/quantize for `mode`, rotate, segment, pack for a head `width_px` wide
/// (resample if the strip width differs). `Gray4` is only valid on models with 4bpp support.
pub fn pack(strip: &GrayStrip, opts: &RenderOptions, mode: PrintMode, width_px: u32) -> Result<Packed, RenderError> {
    let _ = (strip, opts, mode, width_px);
    todo!("render::pack — implemented in the render milestone")
}

/// Convenience: choose the wire mode from tone + model capability.
pub fn mode_for(tone: Tone, grayscale_4bpp: bool) -> PrintMode {
    match (tone, grayscale_4bpp) {
        (Tone::Grayscale, true) => PrintMode::Gray4,
        _ => PrintMode::Mono,
    }
}
