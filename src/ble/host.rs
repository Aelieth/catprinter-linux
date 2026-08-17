//! Read-only host facts for `catprinterd check`: BlueZ TemporaryTimeout and USB
//! Bluetooth interface power/combo heuristics. Never writes udev, main.conf, or rfkill.

use std::fs;
use std::path::Path;
use std::time::Duration;

/// BlueZ default when `TemporaryTimeout` is absent or commented in main.conf.
pub const BLUEZ_DEFAULT_TEMPORARY_TIMEOUT: u32 = 30;

pub const DEFAULT_MAIN_CONF: &str = "/etc/bluetooth/main.conf";
pub const DEFAULT_USB_DEVICES: &str = "/sys/bus/usb/devices";

/// Kit and image both ship this name. Kit copies it to /etc; the image puts it in /usr/lib.
pub const UDEV_RULE_NAME: &str = "61-catprinter-btusb.rules";
pub const DEFAULT_UDEV_RULE_PATHS: &[&str] = &[
    "/etc/udev/rules.d/61-catprinter-btusb.rules",
    "/usr/lib/udev/rules.d/61-catprinter-btusb.rules",
];

/// How long to wait for a live advertisement (RSSI) or a held ACL before Connect.
/// Combo radios take longer to leave LPS / patchram after abort; keep this
/// short — it overlaps the inter-attempt pause, and a 5 s wait made a live
/// printer miss the kid-facing 20 s target.
pub fn advert_wait_for(chip: ChipFamily) -> Duration {
    match chip {
        ChipFamily::Realtek
        | ChipFamily::Mediatek
        | ChipFamily::Qualcomm
        | ChipFamily::Broadcom => Duration::from_millis(2000),
        _ => Duration::from_millis(800),
    }
}

/// True when a shipped rule matches Wireless Controller *interfaces* (e0/01/01).
/// A device-class-only e0 match is the combo-card trap and does not count.
pub fn udev_rule_present_in(paths: &[&Path]) -> bool {
    paths.iter().any(|p| {
        fs::read_to_string(p).is_ok_and(|t| {
            let t = t.to_ascii_lowercase();
            t.contains("binterfaceclass") && t.contains("e0")
        })
    })
}

pub fn udev_rule_present() -> bool {
    let paths: Vec<&Path> = DEFAULT_UDEV_RULE_PATHS.iter().map(Path::new).collect();
    udev_rule_present_in(&paths)
}

/// Wireless Controller (Bluetooth) interface: bInterfaceClass/SubClass/Protocol e0/01/01.
const IFACE_WIRELESS: (&str, &str, &str) = ("e0", "01", "01");
/// Broadcom OEM remaps in btusb_table: USB_VENDOR_AND_INTERFACE_INFO(vid, 0xff, 0x01, 0x01).
const IFACE_VENDOR_BT: (&str, &str, &str) = ("ff", "01", "01");
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
    Broadcom,
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
            Self::Broadcom => "broadcom",
            Self::Generic => "generic",
            Self::None => "none",
        }
    }
}

/// Classify from sysfs `idVendor`/`idProduct` (hex) and the interface's bound driver.
///
/// Driver name wins (`btmtk` / `btrtl` / `btintel` / `btqca` / `btbcm`). OEM VIDs
/// that share silicon (Foxconn 0489, Azurewave 13d3, Lite-On 04ca, ASUS 0b05,
/// Toshiba 0930) are mapped by PID the way `btusb` `quirks_table` does — never
/// by VID alone. Native VIDs (0bda / 0e8d / 8087 / 0cf3 / 0a5c) are one family.
///
/// IDs: Linux `drivers/bluetooth/btusb.c` (`quirks_table` + `btusb_table`).
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
    if drv.contains("btbcm") {
        return ChipFamily::Broadcom;
    }
    let vid = u16::from_str_radix(vendor.trim().trim_start_matches("0x"), 16).unwrap_or(0);
    let pid = u16::from_str_radix(product.trim().trim_start_matches("0x"), 16).unwrap_or(0);
    if let Some(f) = quirk_family(vid, pid) {
        return f;
    }
    match vid {
        0x0bda => ChipFamily::Realtek,
        0x0e8d => ChipFamily::Mediatek,
        0x8087 => ChipFamily::Intel,
        0x0cf3 => ChipFamily::Qualcomm,
        0x0a5c | 0x05ac | 0x105b | 0x19ff => ChipFamily::Broadcom,
        _ if vid != 0 => ChipFamily::Generic,
        _ => ChipFamily::None,
    }
}

/// OEM / remap PIDs from `btusb` `quirks_table`. `None` = fall through to VID.
fn quirk_family(vid: u16, pid: u16) -> Option<ChipFamily> {
    match vid {
        0x0489 => foxconn_family(pid),
        0x13d3 => azurewave_family(pid),
        0x04ca => liteon_family(pid),
        0x0b05 => asus_family(pid),
        0x0930 => toshiba_family(pid),
        0x04c5 => match pid {
            0x165c | 0x1675 | 0x161f => Some(ChipFamily::Realtek),
            0x1330 => Some(ChipFamily::Qualcomm),
            _ => None,
        },
        0x10ab => match pid {
            0x9108 | 0x9109 | 0x9208 | 0x9209 | 0x9308 | 0x9309 | 0x9408 | 0x9409 | 0x9508
            | 0x9509 | 0x9608 | 0x9609 | 0x9f09 => Some(ChipFamily::Qualcomm),
            _ => None,
        },
        0x2c7c => match pid {
            0x0130..=0x0132 => Some(ChipFamily::Qualcomm),
            0x7009 => Some(ChipFamily::Mediatek),
            _ => None,
        },
        0x413c => match pid {
            0x8126 | 0x8152 | 0x8156 => Some(ChipFamily::Broadcom),
            _ => None,
        },
        0x050d => match pid {
            0x0012 | 0x0013 => Some(ChipFamily::Broadcom),
            _ => None,
        },
        _ => None,
    }
}

/// Foxconn / Hon Hai 0489: QCA, MediaTek, Realtek, ATH3012 — never VID-wide.
fn foxconn_family(pid: u16) -> Option<ChipFamily> {
    match pid {
        0xe092 | 0xe09f | 0xe0a2 | 0xe0c7 | 0xe0c9 | 0xe0ca | 0xe0cb | 0xe0cc | 0xe0ce | 0xe0d0
        | 0xe0d6 | 0xe0de | 0xe0df | 0xe0e1 | 0xe0e3 | 0xe0ea | 0xe0ec | 0xe0fc | 0xe0f3
        | 0xe100 | 0xe103 | 0xe10a | 0xe10d | 0xe11b | 0xe11c | 0xe11f | 0xe141 | 0xe14a
        | 0xe14b | 0xe14d | 0xe04d | 0xe04e | 0xe056 | 0xe057 | 0xe05f | 0xe076 | 0xe078
        | 0xe095 | 0xe036 | 0xe03c => Some(ChipFamily::Qualcomm),
        0xe0c8 | 0xe0cd | 0xe0e0 | 0xe0f2 | 0xe0d8 | 0xe0d9 | 0xe0e2 | 0xe0e4 | 0xe0f1 | 0xe0f5
        | 0xe0f6 | 0xe102 | 0xe111 | 0xe113 | 0xe118 | 0xe11e | 0xe124 | 0xe134 | 0xe135
        | 0xe14e | 0xe14f | 0xe150 | 0xe151 | 0xe158 | 0xe11d | 0xe152 | 0xe153 | 0xe170
        | 0xe174 | 0xe139 | 0xe13a | 0xe0fa | 0xe10f | 0xe110 | 0xe116 => {
            Some(ChipFamily::Mediatek)
        }
        0xe085 | 0xe08b | 0xe112 | 0xe122 | 0xe123 | 0xe125 | 0xe12f | 0xe130 => {
            Some(ChipFamily::Realtek)
        }
        _ => None,
    }
}

/// Azurewave / IMC 13d3: Realtek, MediaTek, QCA, ATH3012.
fn azurewave_family(pid: u16) -> Option<ChipFamily> {
    match pid {
        0x3529 | 0x3533 | 0x3548 | 0x3549 | 0x3553 | 0x3555 | 0x3570 | 0x3571 | 0x3572 | 0x3586
        | 0x3587 | 0x3591 | 0x3592 | 0x3600 | 0x3601 | 0x3612 | 0x3616 | 0x3617 | 0x3618
        | 0x3619 | 0x3394 | 0x3410 | 0x3414 | 0x3416 | 0x3458 | 0x3459 | 0x3461 | 0x3462
        | 0x3494 | 0x3526 => Some(ChipFamily::Realtek),
        0x3560 | 0x3563 | 0x3564 | 0x3567 | 0x3568 | 0x3576 | 0x3578 | 0x3579 | 0x3580 | 0x3583
        | 0x3584 | 0x3585 | 0x3588 | 0x3594 | 0x3596 | 0x3602 | 0x3603 | 0x3604 | 0x3605
        | 0x3606 | 0x3607 | 0x3608 | 0x3609 | 0x3610 | 0x3613 | 0x3614 | 0x3615 | 0x3620
        | 0x3621 | 0x3622 | 0x3627 | 0x3628 | 0x3630 | 0x3633 => Some(ChipFamily::Mediatek),
        0x3362 | 0x3375 | 0x3393 | 0x3395 | 0x3402 | 0x3408 | 0x3423 | 0x3432 | 0x3472 | 0x3474
        | 0x3487 | 0x3490 | 0x3491 | 0x3496 | 0x3501 | 0x3623 | 0x3624 => {
            Some(ChipFamily::Qualcomm)
        }
        _ => None,
    }
}

/// Lite-On 04ca: QCA, MediaTek, Realtek, ATH3012.
fn liteon_family(pid: u16) -> Option<ChipFamily> {
    match pid {
        0x4005..=0x4007 => Some(ChipFamily::Realtek),
        0x3801 | 0x3802 | 0x3804 | 0x3807 | 0x38e4 => Some(ChipFamily::Mediatek),
        0x3004 | 0x3005 | 0x3006 | 0x3007 | 0x3008 | 0x300b | 0x300d | 0x300f | 0x3010 | 0x3011
        | 0x3014 | 0x3015 | 0x3016 | 0x3018 | 0x301a | 0x3021 | 0x3022 | 0x3023 | 0x3024
        | 0x3a22 | 0x3a24 | 0x3a26 | 0x3a27 => Some(ChipFamily::Qualcomm),
        _ => None,
    }
}

/// ASUSTek 0b05: Realtek, ATH3012, a few Broadcom dongles.
fn asus_family(pid: u16) -> Option<ChipFamily> {
    match pid {
        0x17dc | 0x185c | 0x18ef | 0x190e => Some(ChipFamily::Realtek),
        0x17d0 => Some(ChipFamily::Qualcomm),
        0x1715 => Some(ChipFamily::Broadcom),
        _ => None,
    }
}

/// Toshiba 0930: Realtek 8723AE + ATH3012.
fn toshiba_family(pid: u16) -> Option<ChipFamily> {
    match pid {
        0x021d => Some(ChipFamily::Realtek),
        0x0219 | 0x021c | 0x0220 | 0x0227 => Some(ChipFamily::Qualcomm),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFacts {
    pub temporary_timeout: u32,
    /// True when the value is BlueZ's default (key absent, commented, or unreadable).
    pub temporary_timeout_is_default: bool,
    pub combo: bool,
    pub chip: ChipFamily,
    pub udev: bool,
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
    pub fn check_lines(&self) -> [(&'static str, String); 5] {
        [
            ("TemporaryTimeout", self.temporary_timeout_line()),
            ("combo", self.combo_line().to_string()),
            ("bt chip", self.chip.as_str().to_string()),
            ("power/control", self.power_control_line()),
            ("udev", if self.udev { "present" } else { "absent" }.into()),
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

/// Broadcom combo remaps: vendor-specific 0xff/0x01/0x01 (btusb_table BCM_PATCHRAM).
fn is_vendor_bt_iface(class: &str, subclass: &str, protocol: &str) -> bool {
    eq_hex(class, IFACE_VENDOR_BT.0)
        && eq_hex(subclass, IFACE_VENDOR_BT.1)
        && eq_hex(protocol, IFACE_VENDOR_BT.2)
}

fn is_bt_usb_iface(class: &str, subclass: &str, protocol: &str) -> bool {
    is_wireless_controller(class, subclass, protocol)
        || is_vendor_bt_iface(class, subclass, protocol)
}

/// Parent USB device name of an interface entry (`3-4:1.0` → `3-4`, `3-4.1:1.0` → `3-4.1`).
fn usb_parent_name(iface: &str) -> Option<&str> {
    iface
        .split_once(':')
        .map(|(p, _)| p)
        .filter(|p| !p.is_empty())
}

/// Scan a sysfs usb-devices tree. Combo = composite (`ef`) parent **and** a BT
/// interface (`e0/01/01` or Broadcom's `ff/01/01`). A device-class-only `e0`
/// match is the udev trap and is not combo.
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
        if !is_bt_usb_iface(&class, &sub, &proto) {
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
        udev: udev_rule_present(),
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
        assert_eq!(lines[4].0, "udev");
    }

    #[test]
    fn advert_wait_is_longer_on_combo_firmware() {
        let combo = advert_wait_for(ChipFamily::Realtek);
        assert_eq!(combo, advert_wait_for(ChipFamily::Mediatek));
        assert_eq!(combo, advert_wait_for(ChipFamily::Qualcomm));
        assert_eq!(combo, advert_wait_for(ChipFamily::Broadcom));
        assert!(combo > advert_wait_for(ChipFamily::Intel));
        assert!(combo > advert_wait_for(ChipFamily::Generic));
        assert!(combo > advert_wait_for(ChipFamily::None));
        assert_eq!(
            advert_wait_for(ChipFamily::Intel),
            Duration::from_millis(800)
        );
        assert_eq!(combo, Duration::from_millis(2000));
    }

    #[test]
    fn udev_rule_requires_interface_class_e0() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.rules");
        let trap = dir.path().join("trap.rules");
        write(
            &good,
            r#"ATTR{bInterfaceClass}=="e0", ATTR{bInterfaceSubClass}=="01", ATTR{bInterfaceProtocol}=="01", ATTR{power/control}="on"
"#,
        );
        write(
            &trap,
            r#"# device-class only — the combo-card trap
ATTR{bDeviceClass}=="e0", ATTR{power/control}="on"
"#,
        );
        assert!(udev_rule_present_in(&[good.as_path()]));
        assert!(!udev_rule_present_in(&[trap.as_path()]));
        assert!(!udev_rule_present_in(&[dir
            .path()
            .join("missing")
            .as_path()]));
        let shipped = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/packaging/61-catprinter-btusb.rules"
        ));
        assert!(
            udev_rule_present_in(&[shipped]),
            "kit/image rule must match Wireless Controller interfaces"
        );
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
        assert_eq!(
            chip_family_from_ids("0a5c", "21e1", Some("btbcm")),
            ChipFamily::Broadcom
        );
        assert_eq!(chip_family_from_ids("", "", None), ChipFamily::None);
    }

    /// Survey samples from `btusb` quirks_table. Expected family is the kernel
    /// firmware class, not a copy of our match arms.
    #[test]
    fn survey_samples_match_kernel_firmware_family() {
        let samples: &[(&str, &str, ChipFamily)] = &[
            ("0bda", "b00c", ChipFamily::Realtek),  // RTL8822CE
            ("0489", "e14e", ChipFamily::Mediatek), // Foxconn MT7925
            ("0489", "e0e3", ChipFamily::Qualcomm), // Foxconn WCN6855
            ("0489", "e123", ChipFamily::Realtek),  // Foxconn RTL8852BE
            ("13d3", "3571", ChipFamily::Realtek),  // Azurewave RTL8852BE
            ("13d3", "3602", ChipFamily::Mediatek), // Azurewave MT7925
            ("13d3", "3491", ChipFamily::Qualcomm), // Azurewave QCA ROME
            ("04ca", "4005", ChipFamily::Realtek),  // Lite-On RTL8822CE
            ("04ca", "3802", ChipFamily::Mediatek), // Lite-On MT7921
            ("04ca", "3022", ChipFamily::Qualcomm), // Lite-On WCN6855
            ("0b05", "18ef", ChipFamily::Realtek),  // ASUS RTL8822CE
            ("0a5c", "21e1", ChipFamily::Broadcom), // Broadcom SoftSailing
            ("05ac", "8213", ChipFamily::Broadcom), // Apple MBP BCM
            ("8087", "0033", ChipFamily::Intel),    // Intel AX211
            ("0cf3", "e007", ChipFamily::Qualcomm), // QCA ROME
            ("0e8d", "223c", ChipFamily::Mediatek), // MT7922A
            ("10ab", "9308", ChipFamily::Qualcomm), // USI WCN6855
            ("2c7c", "7009", ChipFamily::Mediatek), // Quectel MT7925
            ("413c", "8126", ChipFamily::Broadcom), // Dell Broadcom
        ];
        for &(vid, pid, want) in samples {
            assert_eq!(
                chip_family_from_ids(vid, pid, None),
                want,
                "{vid}:{pid} must follow the kernel quirk family"
            );
            assert_eq!(
                advert_wait_for(chip_family_from_ids(vid, pid, None)),
                advert_wait_for(want),
                "{vid}:{pid} wait follows the same family"
            );
        }
        // Unknown PID on a shared OEM VID is not Broadcom-by-VID.
        assert_eq!(
            chip_family_from_ids("0489", "ffff", None),
            ChipFamily::Generic
        );
        assert_eq!(
            chip_family_from_ids("13d3", "0001", None),
            ChipFamily::Generic
        );
    }

    #[test]
    fn check_chip_label_follows_classifier_on_combo_sysfs() {
        let dir = tempfile::tempdir().unwrap();
        combo_tree(dir.path());
        write(&dir.path().join("3-4/idVendor"), "13d3\n");
        write(&dir.path().join("3-4/idProduct"), "3571\n");
        write(&dir.path().join("main.conf"), "TemporaryTimeout = 0\n");
        let facts = collect_host_facts(dir.path(), &dir.path().join("main.conf"));
        assert!(facts.combo);
        assert_eq!(facts.chip, ChipFamily::Realtek);
        assert_eq!(
            facts.check_lines()[2],
            ("bt chip", "realtek".into()),
            "check label must come from the shipped classifier"
        );
    }

    #[test]
    fn broadcom_ff_iface_on_ef_parent_is_combo() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("3-4/bDeviceClass"), "ef\n");
        write(&dir.path().join("3-4/idVendor"), "0a5c\n");
        write(&dir.path().join("3-4/idProduct"), "21e1\n");
        write(&dir.path().join("3-4/power/control"), "auto\n");
        write(&dir.path().join("3-4:1.0/bInterfaceClass"), "ff\n");
        write(&dir.path().join("3-4:1.0/bInterfaceSubClass"), "01\n");
        write(&dir.path().join("3-4:1.0/bInterfaceProtocol"), "01\n");
        let facts = collect_host_facts(dir.path(), Path::new("/no/such/main.conf"));
        assert!(facts.combo, "ef + ff/01/01 is the Broadcom combo shape");
        assert_eq!(facts.chip, ChipFamily::Broadcom);
        assert_eq!(facts.check_lines()[2], ("bt chip", "broadcom".into()));
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
