//! Persist the adopted printer MAC in a daemon-writable directory.
//!
//! `DynamicUser` cannot write `/etc/catprinter/env`. systemd's `StateDirectory=catprinter`
//! (`STATE_DIRECTORY`, usually `/var/lib/catprinter` or `/var/lib/private/catprinter`) is
//! the store. One line, `AA:BB:CC:DD:EE:FF`.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::ble::bluez::compact_bdaddr;
use crate::ble::discovery::DeviceHint;

pub const STORE_NAME: &str = "adopted";
pub const DEFAULT_STATE_DIR: &str = "/var/lib/catprinter";
pub const PRIVATE_STATE_DIR: &str = "/var/lib/private/catprinter";

/// Operator message when adopt cannot see the printer.
pub fn printer_off_message() -> &'static str {
    "Cat printer not found — switch it on and re-run"
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdoptOutcome {
    Adopted { address: String, already: bool },
    Status { address: Option<String> },
    Forgotten { address: Option<String> },
    PrinterOff,
    NeedExperimental,
}

impl AdoptOutcome {
    pub fn message(&self) -> String {
        match self {
            Self::Adopted { address, already: true } => {
                format!("already adopted {address}")
            }
            Self::Adopted { address, already: false } => format!("adopted {address}"),
            Self::Status { address: Some(a) } => format!("adopted {a}"),
            Self::Status { address: None } => "not adopted".into(),
            Self::Forgotten { address: Some(a) } => format!("forgot {a}"),
            Self::Forgotten { address: None } => "not adopted".into(),
            Self::PrinterOff => printer_off_message().into(),
            Self::NeedExperimental => {
                "Bluetooth on this computer cannot force an LE connect (BlueZ Experimental is off). An adult needs to run: sudo ./install.sh update".into()
            }
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Adopted { .. } | Self::Forgotten { .. } => 0,
            Self::Status { address } => i32::from(address.is_none()),
            Self::PrinterOff | Self::NeedExperimental => 1,
        }
    }
}

pub fn store_path(dir: &Path) -> PathBuf {
    dir.join(STORE_NAME)
}

/// Directory used for the adopted-MAC file.
///
/// Order: explicit flag, `CATPRINTER_STATE_DIR`, systemd `STATE_DIRECTORY` (first entry),
/// an existing DynamicUser private dir, then `/var/lib/catprinter`.
pub fn resolve_store_dir(explicit: Option<&Path>) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Ok(s) = std::env::var("CATPRINTER_STATE_DIR") {
        if !s.is_empty() {
            return PathBuf::from(s);
        }
    }
    if let Ok(s) = std::env::var("STATE_DIRECTORY") {
        if let Some(first) = s.split(':').next().filter(|p| !p.is_empty()) {
            return PathBuf::from(first);
        }
    }
    for p in [PRIVATE_STATE_DIR, DEFAULT_STATE_DIR] {
        if Path::new(p).is_dir() {
            return PathBuf::from(p);
        }
    }
    PathBuf::from(DEFAULT_STATE_DIR)
}

/// `AA:BB:CC:DD:EE:FF` or `None` if this is not a BD_ADDR.
pub fn normalize_mac(s: &str) -> Option<String> {
    match DeviceHint::parse(s) {
        DeviceHint::Address(a) => Some(a),
        DeviceHint::Name(_) => compact_to_colon(s),
    }
}

fn compact_to_colon(s: &str) -> Option<String> {
    let hex = compact_bdaddr(s);
    if hex.len() != 12 {
        return None;
    }
    Some(format!(
        "{}:{}:{}:{}:{}:{}",
        &hex[0..2],
        &hex[2..4],
        &hex[4..6],
        &hex[6..8],
        &hex[8..10],
        &hex[10..12]
    ))
}

pub fn load(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(store_path(dir)).ok()?;
    let line = text.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return None;
    }
    normalize_mac(line)
}

pub fn persist(dir: &Path, address: &str) -> io::Result<String> {
    let mac = normalize_mac(address).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a Bluetooth address: {address}"),
        )
    })?;
    std::fs::create_dir_all(dir)?;
    let dest = store_path(dir);
    let tmp = dest.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        writeln!(f, "{mac}")?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &dest)?;
    Ok(mac)
}

/// Record on first successful live connect. Does not overwrite an existing MAC.
pub fn persist_if_empty(dir: &Path, address: &str) -> io::Result<Option<String>> {
    if load(dir).is_some() {
        return Ok(None);
    }
    persist(dir, address).map(Some)
}

pub fn clear(dir: &Path) -> io::Result<Option<String>> {
    let had = load(dir);
    let path = store_path(dir);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    Ok(had)
}

pub fn status_of(dir: &Path) -> AdoptOutcome {
    AdoptOutcome::Status { address: load(dir) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persist_load_status_roundtrip_and_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), None);
        assert_eq!(status_of(dir.path()).message(), "not adopted");
        assert_eq!(status_of(dir.path()).exit_code(), 1);

        let mac = persist(dir.path(), "aa:bb:cc:dd:ee:ff").unwrap();
        assert_eq!(mac, "AA:BB:CC:DD:EE:FF");
        let again = persist(dir.path(), "AA:BB:CC:DD:EE:FF").unwrap();
        assert_eq!(again, mac);
        assert_eq!(load(dir.path()).as_deref(), Some("AA:BB:CC:DD:EE:FF"));

        let st = status_of(dir.path());
        assert_eq!(st.message(), "adopted AA:BB:CC:DD:EE:FF");
        assert_eq!(st.exit_code(), 0);
        assert_eq!(
            std::fs::read_to_string(store_path(dir.path()))
                .unwrap()
                .trim(),
            "AA:BB:CC:DD:EE:FF"
        );
    }

    #[test]
    fn persist_if_empty_does_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        persist(dir.path(), "AA:BB:CC:DD:EE:FF").unwrap();
        assert!(persist_if_empty(dir.path(), "11:22:33:44:55:66")
            .unwrap()
            .is_none());
        assert_eq!(load(dir.path()).as_deref(), Some("AA:BB:CC:DD:EE:FF"));
        let empty = tempfile::tempdir().unwrap();
        let wrote = persist_if_empty(empty.path(), "11:22:33:44:55:66")
            .unwrap()
            .unwrap();
        assert_eq!(wrote, "11:22:33:44:55:66");
    }

    #[test]
    fn printer_off_message_tells_operator_to_switch_on() {
        let m = printer_off_message();
        assert!(m.contains("switch it on"), "{m}");
        assert!(m.to_ascii_lowercase().contains("re-run"), "{m}");
        let o = AdoptOutcome::PrinterOff;
        assert_eq!(o.message(), m);
        assert_ne!(o.exit_code(), 0);
    }

    #[test]
    fn normalize_accepts_colon_underscore_and_bare_hex() {
        assert_eq!(
            normalize_mac("48:0f:57:17:06:9d").as_deref(),
            Some("48:0F:57:17:06:9D")
        );
        assert_eq!(
            normalize_mac("48_0F_57_17_06_9D").as_deref(),
            Some("48:0F:57:17:06:9D")
        );
        assert_eq!(
            normalize_mac("480f5717069d").as_deref(),
            Some("48:0F:57:17:06:9D")
        );
        assert_eq!(normalize_mac("MXW01"), None);
        assert_eq!(normalize_mac("not-a-mac"), None);
    }

    #[test]
    fn clear_removes_store() {
        let dir = tempfile::tempdir().unwrap();
        persist(dir.path(), "AA:BB:CC:DD:EE:FF").unwrap();
        assert_eq!(
            clear(dir.path()).unwrap().as_deref(),
            Some("AA:BB:CC:DD:EE:FF")
        );
        assert_eq!(load(dir.path()), None);
        assert!(clear(dir.path()).unwrap().is_none());
    }

    #[test]
    fn resolve_store_dir_honours_explicit() {
        let p = Path::new("/tmp/explicit-catprinter-state");
        assert_eq!(resolve_store_dir(Some(p)), p);
    }
}
