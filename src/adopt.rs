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

/// Directories that may hold the adopted-MAC file, first match wins on load.
///
/// DynamicUser bind-mounts `StateDirectory=catprinter` over `/var/lib/private/catprinter`.
/// A root `adopt` that only wrote `/var/lib/catprinter` is invisible inside that namespace
/// unless we also write the private path; load therefore searches every candidate.
pub fn candidate_dirs(explicit: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut push = |p: PathBuf| {
        if !p.as_os_str().is_empty() && !out.iter().any(|e| e == &p) {
            out.push(p);
        }
    };
    if let Some(p) = explicit {
        push(p.to_path_buf());
        return out;
    }
    if let Ok(s) = std::env::var("CATPRINTER_STATE_DIR") {
        if !s.is_empty() {
            push(PathBuf::from(s));
        }
    }
    if let Ok(s) = std::env::var("STATE_DIRECTORY") {
        if let Some(first) = s.split(':').next().filter(|p| !p.is_empty()) {
            push(PathBuf::from(first));
        }
    }
    push(PathBuf::from(PRIVATE_STATE_DIR));
    push(PathBuf::from(DEFAULT_STATE_DIR));
    out
}

/// Directory used when we must pick one place to create a file (auto-adopt).
pub fn resolve_store_dir(explicit: Option<&Path>) -> PathBuf {
    candidate_dirs(explicit)
        .into_iter()
        .next()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_STATE_DIR))
}

/// First adopted MAC found in `dirs`.
pub fn load_from_dirs<P: AsRef<Path>>(dirs: impl IntoIterator<Item = P>) -> Option<String> {
    for d in dirs {
        if let Some(m) = load(d.as_ref()) {
            return Some(m);
        }
    }
    None
}

pub fn load_any(explicit: Option<&Path>) -> Option<String> {
    load_from_dirs(candidate_dirs(explicit))
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

/// Record on first successful live connect. Does not overwrite an existing MAC
/// in `dir` (the daemon's writable StateDirectory).
pub fn persist_if_empty(dir: &Path, address: &str) -> io::Result<Option<String>> {
    if load(dir).is_some() {
        return Ok(None);
    }
    persist(dir, address).map(Some)
}

/// Write the MAC into every given directory. Last successful write wins as the
/// returned value. Used so a host-namespace `adopt` is visible inside DynamicUser.
pub fn persist_to_all<P: AsRef<Path>>(dirs: &[P], address: &str) -> io::Result<String> {
    let mut last_err: Option<io::Error> = None;
    let mut mac = None;
    for d in dirs {
        match persist(d.as_ref(), address) {
            Ok(m) => mac = Some(m),
            Err(e) => last_err = Some(e),
        }
    }
    mac.ok_or_else(|| last_err.unwrap_or_else(|| io::Error::other("no adopt store directory")))
}

/// Persist for the CLI: explicit dir, else both DynamicUser-private and public paths.
pub fn persist_visible(explicit: Option<&Path>, address: &str) -> io::Result<String> {
    if let Some(p) = explicit {
        return persist(p, address);
    }
    persist_to_all(
        &[Path::new(PRIVATE_STATE_DIR), Path::new(DEFAULT_STATE_DIR)],
        address,
    )
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

pub fn clear_all(explicit: Option<&Path>) -> Option<String> {
    let mut had = None;
    for d in candidate_dirs(explicit) {
        if let Ok(Some(m)) = clear(&d) {
            had = Some(m);
        }
    }
    had
}

pub fn status_of(dir: &Path) -> AdoptOutcome {
    AdoptOutcome::Status { address: load(dir) }
}

pub fn status_any(explicit: Option<&Path>) -> AdoptOutcome {
    AdoptOutcome::Status {
        address: load_any(explicit),
    }
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
        assert_eq!(candidate_dirs(Some(p)), vec![p.to_path_buf()]);
    }

    #[test]
    fn load_finds_mac_in_any_candidate_so_dynamicuser_does_not_hide_it() {
        let public = tempfile::tempdir().unwrap();
        let private = tempfile::tempdir().unwrap();
        // Empty private dir (daemon has started) must not hide a public adopt.
        persist(public.path(), "AA:BB:CC:DD:EE:FF").unwrap();
        assert_eq!(load(private.path()), None);
        assert_eq!(
            load_from_dirs([private.path(), public.path()]).as_deref(),
            Some("AA:BB:CC:DD:EE:FF")
        );
        assert_eq!(
            status_any_from(&[private.path(), public.path()]).message(),
            "adopted AA:BB:CC:DD:EE:FF"
        );
        persist_to_all(&[private.path(), public.path()], "11:22:33:44:55:66").unwrap();
        assert_eq!(load(private.path()).as_deref(), Some("11:22:33:44:55:66"));
        assert_eq!(load(public.path()).as_deref(), Some("11:22:33:44:55:66"));
    }

    fn status_any_from(dirs: &[&Path]) -> AdoptOutcome {
        AdoptOutcome::Status {
            address: load_from_dirs(dirs.iter().copied()),
        }
    }
}
