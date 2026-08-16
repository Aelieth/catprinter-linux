//! Printer model registry + autodetection. Two protocol families share the AE30 GATT service:
//!
//! * `Mxw01` — 22 21 frames, AE01 control / AE02 notify / AE03 bulk data, 4bpp grayscale.
//! * `Classic` — 51 78 frames, AE01 for everything (rows included), AE02 notify.
//!   GB01/GB02/GB03/GT01/MX05/MX06/MX08/MX09/MX10/MX11/YT01/X5/X6 (upstream rbaron/catprinter set).
//!
//! Detection: advertised name → registry; unknown name + AE03 present ⇒ Mxw01, else Classic.

pub mod classic;
pub mod mxw01;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    Mxw01,
    Classic,
}

impl Family {
    pub fn name(self) -> &'static str {
        match self {
            Family::Mxw01 => "mxw01",
            Family::Classic => "classic",
        }
    }
    pub fn parse(s: &str) -> Option<Family> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mxw01" => Some(Family::Mxw01),
            "classic" | "gb01" | "gb02" | "gb03" | "gt01" => Some(Family::Classic),
            _ => None,
        }
    }
}

/// What a model can do. Everything we know is 384 dots wide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub width_px: u32,
    pub grayscale_4bpp: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelInfo {
    /// Advertised BLE name (exact, case-insensitive match).
    pub name: &'static str,
    pub family: Family,
    pub caps: Caps,
    /// Confirmed on real hardware by this project.
    pub verified: bool,
}

const MXW01_CAPS: Caps = Caps {
    width_px: 384,
    grayscale_4bpp: true,
};
const CLASSIC_CAPS: Caps = Caps {
    width_px: 384,
    grayscale_4bpp: false,
};

macro_rules! classic {
    ($n:literal) => {
        ModelInfo {
            name: $n,
            family: Family::Classic,
            caps: CLASSIC_CAPS,
            verified: false,
        }
    };
}

/// Known advertised names.
pub const REGISTRY: &[ModelInfo] = &[
    ModelInfo {
        name: "MXW01",
        family: Family::Mxw01,
        caps: MXW01_CAPS,
        verified: true,
    },
    classic!("GB01"),
    classic!("GB02"),
    classic!("GB03"),
    classic!("GT01"),
    classic!("MX05"),
    classic!("MX06"),
    classic!("MX08"),
    classic!("MX09"),
    classic!("MX10"),
    classic!("MX11"),
    classic!("YT01"),
    classic!("X5"),
    classic!("X6"),
];

/// Exact (case-insensitive) registry lookup by advertised name.
pub fn lookup(name: &str) -> Option<&'static ModelInfo> {
    let n = name.trim();
    REGISTRY.iter().find(|m| m.name.eq_ignore_ascii_case(n))
}

/// Result of autodetection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    pub family: Family,
    pub caps: Caps,
    /// Human label, e.g. "MXW01" or "unknown (GATT looks like MXW01)".
    pub label: String,
    pub known: bool,
}

/// Decide the driver for a device. `has_ae03` = the GATT table exposes the AE03 data characteristic.
/// `forced` (from --model) wins over everything.
pub fn detect(name: Option<&str>, has_ae03: bool, forced: Option<Family>) -> Detected {
    let by_name = name.and_then(lookup);
    let family = forced.or(by_name.map(|m| m.family)).unwrap_or(if has_ae03 {
        Family::Mxw01
    } else {
        Family::Classic
    });
    let caps = match by_name {
        Some(m) if forced.is_none() || forced == Some(m.family) => m.caps,
        _ => match family {
            Family::Mxw01 => MXW01_CAPS,
            Family::Classic => CLASSIC_CAPS,
        },
    };
    let label = match (by_name, name) {
        (Some(m), _) => m.name.to_string(),
        (None, Some(n)) if !n.is_empty() => {
            format!("{n} (unknown model, driving as {})", family.name())
        }
        _ => format!("unknown model (driving as {})", family.name()),
    };
    Detected {
        family,
        caps,
        label,
        known: by_name.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_lookup_is_case_insensitive() {
        assert_eq!(lookup("mxw01").unwrap().family, Family::Mxw01);
        assert_eq!(lookup(" GB03 ").unwrap().family, Family::Classic);
        assert!(lookup("Phomemo").is_none());
    }

    #[test]
    fn detect_prefers_name_then_gatt_shape_then_force() {
        let d = detect(Some("GT01"), true, None);
        assert_eq!(d.family, Family::Classic);
        assert!(d.known);
        let d = detect(Some("CAT-9000"), true, None);
        assert_eq!(d.family, Family::Mxw01);
        assert!(!d.known);
        assert!(d.label.contains("unknown"));
        let d = detect(None, false, None);
        assert_eq!(d.family, Family::Classic);
        let d = detect(Some("MXW01"), true, Some(Family::Classic));
        assert_eq!(d.family, Family::Classic);
        assert!(!d.caps.grayscale_4bpp);
    }
}
