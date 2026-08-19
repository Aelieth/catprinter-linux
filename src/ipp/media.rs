//! Media (paper size) tables and PWG self-describing media names (PWG 5101.1).
//!
//! Sizes are in hundredths of a millimetre, as IPP `media-size` wants them.
//! CUPS's everywhere PPD generator names PageSizes by *dimensions* (`48x297mm`), and turns the
//! rangeOfInteger entry into `*CustomPageSize`. Margins: left/right **0** on every size (full
//! 384-dot head). Top/bottom 1 mm — all four at 0 makes the generator emit `48x297mm.Borderless`
//! with a `*DefaultPageSize` that does not match. KDE/Gwenview still *defaults* L/R to ~0.17 in
//! when the hardware min is 0; Sheet layout trims leftover side white.

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

/// Custom length range for the tape (1 in .. 5 m) at the fixed 48 mm width.
pub const TAPE_X_HMM: i32 = 4800;

/// True ISO A4 / NA Letter. Document must be advertised at these sizes so the app
/// emits a full page; `Layout::Sheet` then scales that entire raster to 384 dots.
pub const A4_X_HMM: i32 = 21000;
pub const A4_Y_HMM: i32 = 29700;
pub const LETTER_X_HMM: i32 = 21590;
pub const LETTER_Y_HMM: i32 = 27940;

pub const TAPE: MediaSize = MediaSize {
    name: "custom_cat-tape_48x297mm",
    x_hmm: TAPE_X_HMM,
    y_hmm: 29700,
    label: "Cat Tape short",
};
pub const TAPE_LONG: MediaSize = MediaSize {
    name: "custom_cat-tape-long_48x500mm",
    x_hmm: TAPE_X_HMM,
    y_hmm: 50000,
    label: "Cat Tape long",
};
pub const A4: MediaSize = MediaSize {
    name: "iso_a4_210x297mm",
    x_hmm: A4_X_HMM,
    y_hmm: A4_Y_HMM,
    label: "Cat Minidoc A4",
};
pub const LETTER: MediaSize = MediaSize {
    name: "na_letter_8.5x11in",
    x_hmm: LETTER_X_HMM,
    y_hmm: LETTER_Y_HMM,
    label: "Cat Minidoc Letter",
};

pub const ALL: [MediaSize; 4] = [TAPE, TAPE_LONG, A4, LETTER];
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
    /// Homework miniature: PWG/PPD name (`A4`, `iso_a4_…`) or true A4/Letter dimensions.
    /// Width > 60 mm is also a sheet (`is_tape` is false). Name/size pick Sheet layout, not a style.
    pub fn is_document(&self) -> bool {
        if self.name.as_deref().is_some_and(is_document_media_name) {
            return true;
        }
        find_size(self.x_hmm, self.y_hmm).is_some_and(|m| is_document_media_name(m.name))
    }

    pub fn is_tape(&self) -> bool {
        !self.is_document() && self.x_hmm <= TAPE_MAX_WIDTH_HMM
    }
}

/// PWG self-describing names we advertise, plus the PPD keywords CUPS may send (`A4`, `Letter`).
pub fn is_document_media_name(name: &str) -> bool {
    let n = name.trim();
    if n.eq_ignore_ascii_case(A4.name) || n.eq_ignore_ascii_case(LETTER.name) {
        return true;
    }
    let l = n.to_ascii_lowercase();
    matches!(
        l.as_str(),
        "a4" | "letter" | "iso-a4" | "na-letter" | "iso_a4" | "na_letter"
    ) || l.starts_with("iso_a4_")
        || l.starts_with("na_letter_")
        || l.starts_with("a4.")
        || l.starts_with("letter.")
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
    s.push_str("\"media-type.stationery\" = \"Paper\";\n");
    s.push_str("\"media-type.labels\" = \"Sticker\";\n");
    s.push_str("\"print-quality.3\" = \"Text\";\n");
    s.push_str("\"print-quality.4\" = \"Default\";\n");
    s.push_str("\"print-quality.5\" = \"Picture\";\n");
    s.push_str("\"print-color-mode.monochrome\" = \"Grayscale\";\n");
    s.push_str("\"print-color-mode.bi-level\" = \"Black and white\";\n");
    // CUPS driverless bakes Draft/Normal/High and Gray/FastGray into the PPD; map those too.
    s.push_str("\"cupsPrintQuality.Draft\" = \"Text\";\n");
    s.push_str("\"cupsPrintQuality.Normal\" = \"Default\";\n");
    s.push_str("\"cupsPrintQuality.High\" = \"Picture\";\n");
    s.push_str("\"ColorModel.Gray\" = \"Grayscale\";\n");
    s.push_str("\"ColorModel.FastGray\" = \"Black and white\";\n");
    s.push_str("\"PageSize.A4\" = \"Cat Minidoc A4\";\n");
    s.push_str("\"PageSize.Letter\" = \"Cat Minidoc Letter\";\n");
    // driverless names the roll by millimetres when the PWG name is custom_*.
    s.push_str("\"PageSize.48x297mm\" = \"Cat Tape short\";\n");
    s.push_str("\"PageSize.48x500mm\" = \"Cat Tape long\";\n");
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
        assert_eq!(
            parse_media_name("na_letter_8.5x11in"),
            Some((LETTER.x_hmm, LETTER.y_hmm))
        );
        assert_eq!(
            parse_media_name("iso_a4_210x297mm"),
            Some((A4.x_hmm, A4.y_hmm))
        );
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
        let s = strings_en();
        assert!(s.contains("\"media.custom_cat-tape_48x297mm\" = \"Cat Tape short\";"));
        assert!(s.contains("\"media.iso_a4_210x297mm\" = \"Cat Minidoc A4\";"));
        assert!(s.contains("\"PageSize.A4\" = \"Cat Minidoc A4\";"));
        assert!(s.contains("\"PageSize.48x297mm\" = \"Cat Tape short\";"));
        assert!(s.contains("\"print-quality.4\" = \"Default\";"));
        assert!(s.contains("\"print-quality.5\" = \"Picture\";"));
        assert!(s.contains("\"print-color-mode.bi-level\" = \"Black and white\";"));
        assert!(s.contains("\"media-type.labels\" = \"Sticker\";"));
        assert!(s.contains("\"cupsPrintQuality.Draft\" = \"Text\";"));
        assert!(s.contains("\"ColorModel.FastGray\" = \"Black and white\";"));
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

    #[test]
    fn document_pages_are_physical_a4_and_letter() {
        assert_eq!((A4.x_hmm, A4.y_hmm), (A4_X_HMM, A4_Y_HMM));
        assert_eq!((LETTER.x_hmm, LETTER.y_hmm), (LETTER_X_HMM, LETTER_Y_HMM));
        assert_eq!(A4.x_hmm, 21000);
        assert_eq!(LETTER.x_hmm, 21590);
        assert_eq!(A4.y_hmm, 29700);
        assert_eq!(LETTER.y_hmm, 27940);
        match media_col(Some(&A4)) {
            IppValue::Collection(c) => {
                let size = c
                    .iter()
                    .find(|(n, _)| n.as_str() == "media-size")
                    .map(|(_, v)| v)
                    .unwrap();
                let IppValue::Collection(ms) = size else {
                    panic!("{size:?}")
                };
                let get = |k: &str| {
                    ms.iter()
                        .find(|(n, _)| n.as_str() == k)
                        .and_then(|(_, v)| match v {
                            IppValue::Integer(i) => Some(*i),
                            _ => None,
                        })
                };
                assert_eq!(get("x-dimension"), Some(21000));
                assert_eq!(get("y-dimension"), Some(29700));
            }
            other => panic!("{other:?}"),
        }
        let a4 = MediaHint {
            x_hmm: A4.x_hmm,
            y_hmm: A4.y_hmm,
            name: Some(A4.name.into()),
        };
        assert!(a4.is_document());
        assert!(!a4.is_tape());
        // CUPS often sends media-col size with no media-size-name.
        let unnamed = MediaHint {
            x_hmm: 21000,
            y_hmm: 29700,
            name: None,
        };
        assert!(unnamed.is_document(), "true A4 dimensions are Document");
        assert!(!unnamed.is_tape());
        let letter = MediaHint {
            x_hmm: LETTER.x_hmm,
            y_hmm: LETTER.y_hmm,
            name: Some("Letter".into()),
        };
        assert!(letter.is_document());
        let tape = MediaHint {
            x_hmm: TAPE.x_hmm,
            y_hmm: TAPE.y_hmm,
            name: Some(TAPE.name.into()),
        };
        assert!(tape.is_tape());
        assert!(!tape.is_document());
        assert!(is_document_media_name("iso_a4_210x297mm"));
        assert!(is_document_media_name("A4"));
        assert!(is_document_media_name("A4.Borderless"));
        assert!(is_document_media_name("Letter.Borderless"));
        assert!(!is_document_media_name("custom_cat-tape_48x297mm"));
    }
}
