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
    pub vendor: Option<String>,
    pub product: Option<String>,
    pub driver: Option<String>,
}

/// USB Bluetooth firmware family from btusb.c id tables + bound driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipFamily {
    Mediatek,
    Realtek,
    Qualcomm,
    Intel,
    Generic,
    None,
}

impl ChipFamily {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mediatek => "mediatek",
            Self::Realtek => "realtek",
            Self::Qualcomm => "qca",
            Self::Intel => "intel",
            Self::Generic => "generic",
            Self::None => "none",
        }
    }
}

/// Classify from sysfs `idVendor`/`idProduct` (hex) and the interface's bound driver.
/// Driver name wins (btmtk / btrtl / btintel / btqca). Vendor 0489 is Foxconn OEM
/// for both MediaTek and Qualcomm — those use the btusb quirk table, not vendor alone.
pub fn chip_family_from_ids(vendor: &str, product: &str, driver: Option<&str>) -> ChipFamily {
    let drv = driver.unwrap_or("").to_ascii_lowercase();
    if drv.contains("btmtk") {
        return ChipFamily::Mediatek;
    }
    if drv.contains("btrtl") {
        return ChipFamily::Realtek;
    }
    if drv.contains("btintel") {
        return ChipFamily::Intel;
    }
    if drv.contains("btqca") || drv.contains("ath3k") || drv.contains("hci_qca") {
        return ChipFamily::Qualcomm;
    }
    let vid = u16::from_str_radix(vendor.trim().trim_start_matches("0x"), 16).unwrap_or(0);
    let pid = u16::from_str_radix(product.trim().trim_start_matches("0x"), 16).unwrap_or(0);
    match vid {
        0x0bda => ChipFamily::Realtek,
        0x0e8d => ChipFamily::Mediatek,
        0x8087 => ChipFamily::Intel,
        0x0cf3 => ChipFamily::Qualcomm,
        0x0489 => foxconn_family(pid),
        _ if vid != 0 => ChipFamily::Generic,
        _ => ChipFamily::None,
    }
}

/// Foxconn 0489:e14e is MT7925 (btusb BTUSB_MEDIATEK); 0489:e0e3 is WCN6855 (QCA).
fn foxconn_family(pid: u16) -> ChipFamily {
    match pid {
        0xe0c7 | 0xe0c9 | 0xe0ca | 0xe0cb | 0xe0cc | 0xe0ce | 0xe0d0 | 0xe0d6 | 0xe0de | 0xe0df
        | 0xe0e1 | 0xe0e3 | 0xe0ea | 0xe0ec | 0xe0fc | 0xe0f3 | 0xe100 => ChipFamily::Qualcomm,
        0xe0c8 | 0xe0cd | 0xe0e0 | 0xe0f2 | 0xe0d8 | 0xe0d9 | 0xe0e2 | 0xe0e4 | 0xe0f1 | 0xe0f5
        | 0xe0f6 | 0xe102 | 0xe111 | 0xe113 | 0xe118 | 0xe11e | 0xe124 | 0xe134 | 0xe135
        | 0xe14e | 0xe14f | 0xe150 | 0xe151 => ChipFamily::Mediatek,
        _ => ChipFamily::Generic,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFacts {
    pub temporary_timeout: u32,
    /// True when the value is BlueZ's default (key absent, commented, or unreadable).
    pub temporary_timeout_is_default: bool,
    pub combo: bool,
    pub chip: ChipFamily,
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
    pub fn check_lines(&self) -> [(&'static str, String); 4] {
        [
            ("TemporaryTimeout", self.temporary_timeout_line()),
            ("combo", self.combo_line().to_string()),
            ("bt chip", self.chip.as_str().to_string()),
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
        let (vendor, product, driver) = if let Some(par) = parent {
            let parent_dir = usb_devices.join(par);
            (
                read_trim(&parent_dir.join("idVendor")),
                read_trim(&parent_dir.join("idProduct")),
                read_driver_name(&p.join("driver")),
            )
        } else {
            (None, None, read_driver_name(&p.join("driver")))
        };
        ifaces.push(BtUsbInterface {
            name: name.into_owned(),
            power_control: power,
            vendor,
            product,
            driver,
        });
    }
    ifaces.sort_by(|a, b| a.name.cmp(&b.name));
    (combo, ifaces)
}

fn read_driver_name(link: &Path) -> Option<String> {
    fs::read_link(link)
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
        .or_else(|| read_trim(link))
}

fn chip_from_ifaces(ifaces: &[BtUsbInterface]) -> ChipFamily {
    ifaces
        .iter()
        .map(|i| {
            chip_family_from_ids(
                i.vendor.as_deref().unwrap_or(""),
                i.product.as_deref().unwrap_or(""),
                i.driver.as_deref(),
            )
        })
        .find(|f| !matches!(f, ChipFamily::None | ChipFamily::Generic))
        .unwrap_or(if ifaces.is_empty() {
            ChipFamily::None
        } else {
            ChipFamily::Generic
        })
}

pub fn collect_host_facts(usb_devices: &Path, main_conf: &Path) -> HostFacts {
    let (temporary_timeout, temporary_timeout_is_default) = temporary_timeout_from_path(main_conf);
    let (combo, bt_interfaces) = usb_bt_facts(usb_devices);
    let chip = chip_from_ifaces(&bt_interfaces);
    HostFacts {
        temporary_timeout,
        temporary_timeout_is_default,
        combo,
        chip,
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
        assert_eq!(lines[2].0, "bt chip");
        assert!(lines[3].1.contains("3-4:1.0=on"));
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
        assert_eq!(lines[2], ("bt chip", "none".into()));
        assert_eq!(lines[3], ("power/control", "no BT USB interfaces".into()));
    }

    #[test]
    fn chip_family_from_btusb_id_tables() {
        // kids-pc RTL8822CE USB BT (btusb BTUSB_REALTEK)
        assert_eq!(
            chip_family_from_ids("0bda", "b00c", None),
            ChipFamily::Realtek
        );
        // blue-lt MT7925 Foxconn (btusb BTUSB_MEDIATEK)
        assert_eq!(
            chip_family_from_ids("0489", "e14e", None),
            ChipFamily::Mediatek
        );
        // QCA WCN6855 Foxconn (btusb BTUSB_QCA_WCN6855) — same OEM as MTK
        assert_eq!(
            chip_family_from_ids("0489", "e0e3", None),
            ChipFamily::Qualcomm
        );
        // Driver name wins over a misleading vendor.
        assert_eq!(
            chip_family_from_ids("0489", "e0e3", Some("btmtk")),
            ChipFamily::Mediatek
        );
        assert_eq!(
            chip_family_from_ids("8087", "0033", Some("btintel")),
            ChipFamily::Intel
        );
        assert_eq!(chip_family_from_ids("", "", None), ChipFamily::None);
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
