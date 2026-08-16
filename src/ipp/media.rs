//! Media (paper size) tables and PWG self-describing media names (PWG 5101.1).
//!
//! Sizes are in hundredths of a millimetre, as IPP `media-size` wants them.
//! CUPS's everywhere PPD generator names PageSizes by *dimensions* (`48x297mm`), and turns the
//! rangeOfInteger entry into `*CustomPageSize`. Margins: left/right 0 (full 384-dot head), top/bottom
//! 1 mm — with all four at 0 the generator emits `48x297mm.Borderless` choices but a `*DefaultPageSize`
//! without the suffix (no match); 1 mm keeps the plain names and a matching default.

use crate::ipp::codec::{v_int, v_kw, v_range, Coll};
use ipp::value::IppValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaSize {
    /// PWG self-describing name (`class_name_WxHunits`).
    pub name: &'static str,
    pub x_hmm: i32,
    pub y_hmm: i32,
    /// Human label (printer-strings-uri).
    pub label: &'static str,
}

pub const TAPE: MediaSize = MediaSize {
    name: "custom_cat-tape_48x297mm",
    x_hmm: 4800,
    y_hmm: 29700,
    label: "Cat tape 48 mm",
};
pub const TAPE_LONG: MediaSize = MediaSize {
    name: "custom_cat-tape-long_48x500mm",
    x_hmm: 4800,
    y_hmm: 50000,
    label: "Cat tape 48 mm, long",
};
pub const A4: MediaSize = MediaSize {
    name: "iso_a4_210x297mm",
    x_hmm: 21000,
    y_hmm: 29700,
    label: "A4 (shrunk to tape)",
};
pub const LETTER: MediaSize = MediaSize {
    name: "na_letter_8.5x11in",
    x_hmm: 21590,
    y_hmm: 27940,
    label: "Letter (shrunk to tape)",
};

pub const ALL: [MediaSize; 4] = [TAPE, TAPE_LONG, A4, LETTER];

/// Custom length range for the tape (1 in .. 5 m) at the fixed 48 mm width.
pub const TAPE_X_HMM: i32 = 4800;
pub const CUSTOM_MIN_Y_HMM: i32 = 2540;
pub const CUSTOM_MAX_Y_HMM: i32 = 508_000;
pub const MARGIN_LEFT_RIGHT_HMM: i32 = 0;
pub const MARGIN_TOP_BOTTOM_HMM: i32 = 100;
pub const MEDIA_SOURCE: &str = "main";
pub const MEDIA_TYPE: &str = "stationery";

/// Anything at most this wide is "tape" (roll) layout; wider pages are "sheets".
pub const TAPE_MAX_WIDTH_HMM: i32 = 6000;

pub fn media_names() -> Vec<&'static str> {
    ALL.iter().map(|m| m.name).collect()
}

/// `media-col` collection for a concrete size (or the range entry when `size` is None).
pub fn media_col(size: Option<&MediaSize>) -> IppValue {
    let msize = match size {
        Some(s) => Coll::new()
            .add("x-dimension", v_int(s.x_hmm))
            .add("y-dimension", v_int(s.y_hmm))
            .build(),
        None => Coll::new()
            .add("x-dimension", v_int(TAPE_X_HMM))
            .add("y-dimension", v_range(CUSTOM_MIN_Y_HMM, CUSTOM_MAX_Y_HMM))
            .build(),
    };
    let mut c = Coll::new()
        .add("media-size", msize)
        .add("media-source", v_kw(MEDIA_SOURCE))
        .add("media-type", v_kw(MEDIA_TYPE))
        .add("media-left-margin", v_int(MARGIN_LEFT_RIGHT_HMM))
        .add("media-right-margin", v_int(MARGIN_LEFT_RIGHT_HMM))
        .add("media-top-margin", v_int(MARGIN_TOP_BOTTOM_HMM))
        .add("media-bottom-margin", v_int(MARGIN_TOP_BOTTOM_HMM));
    if let Some(s) = size {
        c = c.add("media-size-name", v_kw(s.name));
    }
    c.build()
}

/// media-col-database: every fixed size + the custom-length tape range.
pub fn media_col_database() -> IppValue {
    let mut v: Vec<IppValue> = ALL.iter().map(|m| media_col(Some(m))).collect();
    v.push(media_col(None));
    IppValue::Array(v)
}

/// media-size-supported: same shapes as the database, media-size collections only.
pub fn media_size_supported() -> IppValue {
    let mut v: Vec<IppValue> = ALL
        .iter()
        .map(|m| {
            Coll::new()
                .add("x-dimension", v_int(m.x_hmm))
                .add("y-dimension", v_int(m.y_hmm))
                .build()
        })
        .collect();
    v.push(
        Coll::new()
            .add("x-dimension", v_int(TAPE_X_HMM))
            .add("y-dimension", v_range(CUSTOM_MIN_Y_HMM, CUSTOM_MAX_Y_HMM))
            .build(),
    );
    IppValue::Array(v)
}

/// A media request as we understood it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaHint {
    pub x_hmm: i32,
    pub y_hmm: i32,
    pub name: Option<String>,
}

impl MediaHint {
    pub fn is_tape(&self) -> bool {
        self.x_hmm <= TAPE_MAX_WIDTH_HMM
    }
}

/// Parse a PWG self-describing media name (`custom_cat-tape_48x297mm`, `na_letter_8.5x11in`,
/// `iso_a4_210x297mm`, also CUPS-style `Custom.48x1000mm` / `custom_48x1000mm`).
/// Returns (x, y) in hundredths of a mm.
pub fn parse_media_name(name: &str) -> Option<(i32, i32)> {
    let n = name.trim();
    if let Some(known) = ALL.iter().find(|m| m.name.eq_ignore_ascii_case(n)) {
        return Some((known.x_hmm, known.y_hmm));
    }
    // last '_'-separated (or '.'-separated for Custom.WxHunit) token holds WxHunits
    let dims = n.rsplit(['_', '.']).next()?;
    let lower = dims.to_ascii_lowercase();
    let (body, unit_scale) = if let Some(b) = lower.strip_suffix("mm") {
        (b.to_string(), 100.0)
    } else if let Some(b) = lower.strip_suffix("in") {
        (b.to_string(), 2540.0)
    } else {
        // CUPS Custom.WxH without units = points
        (lower.clone(), 2540.0 / 72.0)
    };
    let (w, h) = body.split_once('x')?;
    let w: f64 = w.parse().ok()?;
    let h: f64 = h.parse().ok()?;
    if !(w > 0.0 && h > 0.0) {
        return None;
    }
    Some((
        (w * unit_scale).round() as i32,
        (h * unit_scale).round() as i32,
    ))
}

/// Match a requested (x,y) to one of our fixed sizes (tolerance 1 mm), for labels/logging.
pub fn find_size(x_hmm: i32, y_hmm: i32) -> Option<&'static MediaSize> {
    ALL.iter()
        .find(|m| m.x_hmm.abs_diff(x_hmm) <= 100 && m.y_hmm.abs_diff(y_hmm) <= 100)
}

/// Contents of /strings/en.strings (Apple .strings format used by printer-strings-uri).
pub fn strings_en() -> String {
    let mut s = String::new();
    for m in ALL.iter() {
        s.push_str(&format!("\"media.{}\" = \"{}\";\n", m.name, m.label));
    }
    s.push_str("\"media-source.main\" = \"Roll\";\n");
    s.push_str("\"media-type.stationery\" = \"Thermal paper\";\n");
    s.push_str("\"print-quality.3\" = \"Draft (sharp text)\";\n");
    s.push_str("\"print-quality.4\" = \"Normal (drawings)\";\n");
    s.push_str("\"print-quality.5\" = \"High (photos, grayscale)\";\n");
    s.push_str("\"print-color-mode.monochrome\" = \"Grayscale\";\n");
    s.push_str("\"print-color-mode.bi-level\" = \"Black and white\";\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pwg_names() {
        assert_eq!(
            parse_media_name("custom_cat-tape_48x297mm"),
            Some((4800, 29700))
        );
        assert_eq!(parse_media_name("na_letter_8.5x11in"), Some((21590, 27940)));
        assert_eq!(parse_media_name("iso_a4_210x297mm"), Some((21000, 29700)));
        assert_eq!(parse_media_name("custom_48x1000mm"), Some((4800, 100000)));
        assert_eq!(parse_media_name("Custom.48x1000mm"), Some((4800, 100000)));
        assert_eq!(
            parse_media_name("om_whatever_100x150mm"),
            Some((10000, 15000))
        );
        assert_eq!(parse_media_name("nonsense"), None);
        assert_eq!(parse_media_name("custom_0x10mm"), None);
    }

    #[test]
    fn database_has_range_entry_and_labels() {
        match media_col_database() {
            IppValue::Array(v) => assert_eq!(v.len(), ALL.len() + 1),
            _ => panic!(),
        }
        assert!(strings_en().contains("\"media.custom_cat-tape_48x297mm\" = \"Cat tape 48 mm\";"));
        assert!(MediaHint {
            x_hmm: 4800,
            y_hmm: 1,
            name: None
        }
        .is_tape());
        assert!(!MediaHint {
            x_hmm: 21000,
            y_hmm: 1,
            name: None
        }
        .is_tape());
        assert_eq!(find_size(4800, 29700).map(|m| m.name), Some(TAPE.name));
    }
}
