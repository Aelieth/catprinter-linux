//! PNG/JPEG → GrayPage (Rec.709 luma, alpha flattened onto white, EXIF orientation honoured).

use crate::raster::GrayPage;
use crate::render::RenderError;

/// Sniff PNG / JPEG magic.
pub fn is_image(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x89, b'P', b'N', b'G']) || bytes.starts_with(&[0xFF, 0xD8, 0xFF])
}

/// Decode to gray at nominal 203 dpi (images have no physical size; layout is always Tape).
pub fn load(bytes: &[u8]) -> Result<GrayPage, RenderError> {
    let _ = bytes;
    todo!("render::imagein::load — implemented in the render milestone")
}
