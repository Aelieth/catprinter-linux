//! Read-only host facts for `catprinterd check`: BlueZ TemporaryTimeout and USB
//! Bluetooth interface power/combo heuristics. Never writes udev, main.conf, or rfkill.

use std::fs;
use std::path::Path;

/// BlueZ default when `TemporaryTimeout` is absent or commented in main.conf.
pub const BLUEZ_DEFAULT_TEMPORARY_TIMEOUT: u32 = 30;

pub const DEFAULT_MAIN_CONF: &str = "/etc/bluetooth/main.conf";
pub const DEFAULT_USB_DEVICES: &str = "/sys/bus/usb/devices";

/// Wireless Controller (Bluetooth) interface: bInterfaceClass/SubClass/Protocol e0/01/01.
const IFACE_WIRELESS: (&str, &str, &str) = ("e0", "01", "01");
/// Composite / IAD parent: bDeviceClass ef. Combo cards use this, not device-class e0.
const DEVICE_MISC_IAD: &str = "ef";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtUsbInterface {
    pub name: String,
    pub power_control: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFacts {
    pub temporary_timeout: u32,
    /// True when the value is BlueZ's default (key absent, commented, or unreadable).
    pub temporary_timeout_is_default: bool,
    pub combo: bool,
    pub bt_interfaces: Vec<BtUsbInterface>,
}

impl HostFacts {
    /// `3-4:1.0=on 3-4:1.1=auto`, or the explicit empty-set line.
    pub fn power_control_line(&self) -> String {
        if self.bt_interfaces.is_empty() {
            return "no BT USB interfaces".into();
        }
        self.bt_interfaces
            .iter()
            .map(|i| match &i.power_control {
                Some(v) => format!("{}={v}", i.name),
                None => format!("{}=?", i.name),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn temporary_timeout_line(&self) -> String {
        if self.temporary_timeout_is_default {
            format!("{} (default)", self.temporary_timeout)
        } else {
            self.temporary_timeout.to_string()
        }
    }

    pub fn combo_line(&self) -> &'static str {
        if self.combo {
            "yes"
        } else {
            "no"
        }
    }

    /// Labels + values printed by `catprinterd check` (does not affect READY).
    pub fn check_lines(&self) -> [(&'static str, String); 3] {
        [
            ("TemporaryTimeout", self.temporary_timeout_line()),
            ("combo", self.combo_line().to_string()),
            ("power/control", self.power_control_line()),
        ]
    }
}

/// Parse TemporaryTimeout from main.conf text. Commented or absent → BlueZ default 30.
pub fn temporary_timeout_from_text(conf: &str) -> (u32, bool) {
    for raw in conf.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let code = line.split_once('#').map(|(a, _)| a.trim()).unwrap_or(line);
        let Some(rest) = code.strip_prefix("TemporaryTimeout") else {
            continue;
        };
        let val = rest.trim().trim_start_matches('=').trim();
        if let Ok(n) = val.parse::<u32>() {
            return (n, false);
        }
    }
    (BLUEZ_DEFAULT_TEMPORARY_TIMEOUT, true)
}

pub fn temporary_timeout_from_path(path: &Path) -> (u32, bool) {
    match fs::read_to_string(path) {
        Ok(text) => temporary_timeout_from_text(&text),
        Err(_) => (BLUEZ_DEFAULT_TEMPORARY_TIMEOUT, true),
    }
}

fn read_trim(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn eq_hex(got: &str, want: &str) -> bool {
    got.trim().eq_ignore_ascii_case(want)
}

fn is_wireless_controller(class: &str, subclass: &str, protocol: &str) -> bool {
    eq_hex(class, IFACE_WIRELESS.0)
        && eq_hex(subclass, IFACE_WIRELESS.1)
        && eq_hex(protocol, IFACE_WIRELESS.2)
}

/// Parent USB device name of an interface entry (`3-4:1.0` → `3-4`, `3-4.1:1.0` → `3-4.1`).
fn usb_parent_name(iface: &str) -> Option<&str> {
    iface
        .split_once(':')
        .map(|(p, _)| p)
        .filter(|p| !p.is_empty())
}

/// Scan a sysfs usb-devices tree. Combo = composite (`ef`) parent **and** e0/01/01
/// *interfaces*. A device-class-only `e0` match is the udev trap and is not combo.
pub fn usb_bt_facts(usb_devices: &Path) -> (bool, Vec<BtUsbInterface>) {
    let entries = match fs::read_dir(usb_devices) {
        Ok(rd) => rd,
        Err(_) => return (false, Vec::new()),
    };
    let mut combo = false;
    let mut ifaces = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Interfaces are `bus-port:config.iface`. Device nodes have no colon.
        if !name.contains(':') {
            continue;
        }
        let p = entry.path();
        let (Some(class), Some(sub), Some(proto)) = (
            read_trim(&p.join("bInterfaceClass")),
            read_trim(&p.join("bInterfaceSubClass")),
            read_trim(&p.join("bInterfaceProtocol")),
        ) else {
            continue;
        };
        if !is_wireless_controller(&class, &sub, &proto) {
            continue;
        }
        let parent = usb_parent_name(&name);
        let power = read_trim(&p.join("power").join("control")).or_else(|| {
            parent.and_then(|par| read_trim(&usb_devices.join(par).join("power").join("control")))
        });
        if let Some(par) = parent {
            let parent_class = read_trim(&usb_devices.join(par).join("bDeviceClass"));
            if parent_class
                .as_deref()
                .is_some_and(|c| eq_hex(c, DEVICE_MISC_IAD))
            {
                combo = true;
            }
        }
        ifaces.push(BtUsbInterface {
            name: name.into_owned(),
            power_control: power,
        });
    }
    ifaces.sort_by(|a, b| a.name.cmp(&b.name));
    (combo, ifaces)
}

pub fn collect_host_facts(usb_devices: &Path, main_conf: &Path) -> HostFacts {
    let (temporary_timeout, temporary_timeout_is_default) = temporary_timeout_from_path(main_conf);
    let (combo, bt_interfaces) = usb_bt_facts(usb_devices);
    HostFacts {
        temporary_timeout,
        temporary_timeout_is_default,
        combo,
        bt_interfaces,
    }
}

pub fn collect_default() -> HostFacts {
    collect_host_facts(Path::new(DEFAULT_USB_DEVICES), Path::new(DEFAULT_MAIN_CONF))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn temporary_timeout_explicit_commented_absent() {
        assert_eq!(
            temporary_timeout_from_text("TemporaryTimeout = 0\n"),
            (0, false)
        );
        assert_eq!(
            temporary_timeout_from_text("[General]\nTemporaryTimeout=45 # seconds\n"),
            (45, false)
        );
        assert_eq!(
            temporary_timeout_from_text("#TemporaryTimeout = 30\n"),
            (30, true)
        );
        assert_eq!(
            temporary_timeout_from_text("[General]\n# TemporaryTimeout = 30\nAutoEnable=true\n"),
            (30, true)
        );
        assert_eq!(temporary_timeout_from_text(""), (30, true));
        assert_eq!(
            temporary_timeout_from_text("DiscoverableTimeout = 0\n"),
            (30, true)
        );
        assert_eq!(
            temporary_timeout_from_text("TemporaryTimeout = banana\n"),
            (30, true)
        );
    }

    #[test]
    fn temporary_timeout_missing_file_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let (v, is_default) = temporary_timeout_from_path(&dir.path().join("nope.conf"));
        assert_eq!(v, 30);
        assert!(is_default);
    }

    fn write(p: &Path, body: &str) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, body).unwrap();
    }

    fn combo_tree(root: &Path) {
        // Composite parent (IAD) + Wireless Controller interfaces — the real combo shape.
        write(&root.join("3-4/bDeviceClass"), "ef\n");
        write(&root.join("3-4/bDeviceSubClass"), "02\n");
        write(&root.join("3-4/bDeviceProtocol"), "01\n");
        write(&root.join("3-4/power/control"), "on\n");
        for iface in ["3-4:1.0", "3-4:1.1"] {
            write(&root.join(iface).join("bInterfaceClass"), "e0\n");
            write(&root.join(iface).join("bInterfaceSubClass"), "01\n");
            write(&root.join(iface).join("bInterfaceProtocol"), "01\n");
            write(&root.join(iface).join("power/control"), "on\n");
        }
    }

    fn trap_dongle(root: &Path) {
        // Single-function BT dongle: device class e0/01/01. Matching this as combo is the trap.
        write(&root.join("1-2/bDeviceClass"), "e0\n");
        write(&root.join("1-2/bDeviceSubClass"), "01\n");
        write(&root.join("1-2/bDeviceProtocol"), "01\n");
        write(&root.join("1-2/power/control"), "auto\n");
        write(&root.join("1-2:1.0/bInterfaceClass"), "e0\n");
        write(&root.join("1-2:1.0/bInterfaceSubClass"), "01\n");
        write(&root.join("1-2:1.0/bInterfaceProtocol"), "01\n");
    }

    #[test]
    fn combo_ef_parent_plus_wireless_interfaces() {
        let dir = tempfile::tempdir().unwrap();
        combo_tree(dir.path());
        write(&dir.path().join("main.conf"), "TemporaryTimeout = 0\n");
        let facts = collect_host_facts(dir.path(), &dir.path().join("main.conf"));
        assert!(facts.combo, "ef parent + e0/01/01 interfaces must be combo");
        assert_eq!(facts.bt_interfaces.len(), 2);
        assert_eq!(facts.bt_interfaces[0].name, "3-4:1.0");
        assert_eq!(facts.bt_interfaces[0].power_control.as_deref(), Some("on"));
        assert_eq!(facts.temporary_timeout, 0);
        assert!(!facts.temporary_timeout_is_default);
        let lines = facts.check_lines();
        assert_eq!(lines[0], ("TemporaryTimeout", "0".into()));
        assert_eq!(lines[1], ("combo", "yes".into()));
        assert!(lines[2].1.contains("3-4:1.0=on"));
    }

    #[test]
    fn device_class_e0_alone_is_not_combo() {
        let dir = tempfile::tempdir().unwrap();
        trap_dongle(dir.path());
        let facts = collect_host_facts(dir.path(), &dir.path().join("missing.conf"));
        assert!(
            !facts.combo,
            "device-class e0/01/01 must not be treated as combo"
        );
        assert_eq!(facts.bt_interfaces.len(), 1);
        assert_eq!(facts.bt_interfaces[0].name, "1-2:1.0");
        // Interface has no power/control; fall back to the parent device node.
        assert_eq!(
            facts.bt_interfaces[0].power_control.as_deref(),
            Some("auto")
        );
        assert_eq!(facts.combo_line(), "no");
        assert_eq!(facts.power_control_line(), "1-2:1.0=auto");
        assert_eq!(facts.temporary_timeout, 30);
        assert!(facts.temporary_timeout_is_default);
        assert_eq!(facts.temporary_timeout_line(), "30 (default)");
    }

    #[test]
    fn no_bt_usb_interfaces_line() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("4-1/bDeviceClass"), "09\n"); // hub
        let facts = collect_host_facts(dir.path(), Path::new("/no/such/main.conf"));
        assert!(!facts.combo);
        assert!(facts.bt_interfaces.is_empty());
        assert_eq!(facts.power_control_line(), "no BT USB interfaces");
        let lines = facts.check_lines();
        assert_eq!(lines[2], ("power/control", "no BT USB interfaces".into()));
    }

    #[test]
    fn combo_tree_plus_e0_dongle_still_combo_from_ef_parent() {
        let dir = tempfile::tempdir().unwrap();
        combo_tree(dir.path());
        trap_dongle(dir.path());
        let (combo, ifaces) = usb_bt_facts(dir.path());
        assert!(combo);
        assert_eq!(ifaces.len(), 3);
    }
}
