//! `catprinterd doctor`: machine-readable host facts an integrator would otherwise grep for.

use std::path::Path;
use std::time::{Duration, Instant};

use serde::Serialize;
use zbus::Connection;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::ble::{bluez, discovery, host};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DoctorReport {
    pub adopted: bool,
    pub adopted_address: Option<String>,
    pub trusted_le: bool,
    pub connect_transport: &'static str,
    pub connect_device: bool,
    pub experimental: bool,
    pub queue_name: String,
    pub queue_present: bool,
    pub queue_uri: Option<String>,
    pub queue_points_at_daemon: bool,
    pub queue_is_default: bool,
    pub port: u16,
    pub daemon_up: bool,
    pub daemon_version: Option<String>,
    pub adapter_powered: Option<bool>,
    pub adapter_blocked: Option<bool>,
    pub adapter_autosuspend: Option<bool>,
    pub adapter_path: Option<String>,
    /// Present only when the reading is live (held ACL, or observed during this run).
    pub rssi: Option<i16>,
    pub rssi_age_ms: Option<u64>,
}

impl DoctorReport {
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into())
    }

    pub fn print_prose(&self) {
        println!("catprinterd {} doctor", crate::VERSION);
        let adopted = match &self.adopted_address {
            Some(a) if self.trusted_le => format!("{a} (trusted LE)"),
            Some(a) => format!("{a} (not a trusted LE object)"),
            None => "no".into(),
        };
        println!("{:<20} {adopted}", "adopted");
        println!(
            "{:<20} {} (ConnectDevice {})",
            "connect",
            self.connect_transport,
            if self.connect_device {
                "available"
            } else {
                "missing"
            }
        );
        println!(
            "{:<20} {}",
            "Experimental",
            if self.experimental { "true" } else { "false" }
        );
        match (&self.queue_uri, self.queue_present) {
            (Some(u), true) => println!("{:<20} {} → {u}", "queue", self.queue_name),
            _ => println!("{:<20} {} missing", "queue", self.queue_name),
        }
        let daemon = if self.daemon_up {
            match &self.daemon_version {
                Some(v) => format!("up ({v}) on :{}", self.port),
                None => format!("up on :{}", self.port),
            }
        } else {
            format!("down on :{}", self.port)
        };
        println!("{:<20} {daemon}", "daemon");
        let powered = match self.adapter_powered {
            Some(true) => "powered",
            Some(false) => "off",
            None => "unknown",
        };
        let blocked = match self.adapter_blocked {
            Some(true) => "blocked",
            Some(false) => "unblocked",
            None => "block unknown",
        };
        let auto = match self.adapter_autosuspend {
            Some(true) => "autosuspend=on",
            Some(false) => "autosuspend=off",
            None => "autosuspend unknown",
        };
        println!("{:<20} {powered}, {blocked}, {auto}", "adapter");
        match (self.rssi, self.rssi_age_ms) {
            (Some(r), Some(age)) => println!("{:<20} {r} (age {age} ms)", "rssi"),
            (Some(r), None) => println!("{:<20} {r}", "rssi"),
            _ => println!("{:<20} (omitted — no live reading)", "rssi"),
        }
    }
}

/// A connect goes LE when `ConnectDevice` exists; otherwise Device1.Connect pages Classic.
pub fn connect_transport(connect_device: bool) -> &'static str {
    if connect_device {
        "le"
    } else {
        "classic"
    }
}

/// BlueZ keeps the last RSSI on a persistent object forever. Only report it when the
/// device is held (`connected`) or we observed the advertisement ourselves.
pub fn rssi_for_report(
    connected: bool,
    rssi: Option<i16>,
    observed_at: Option<Instant>,
) -> (Option<i16>, Option<u64>) {
    if connected {
        return (rssi, Some(0));
    }
    match (rssi, observed_at) {
        (Some(r), Some(t)) => (Some(r), Some(t.elapsed().as_millis() as u64)),
        _ => (None, None),
    }
}

/// `device for CatPrinter: ipp://127.0.0.1:8095/ipp/print`
pub fn parse_lpstat_v(text: &str, queue: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        let rest = line
            .strip_prefix("device for ")
            .or_else(|| line.strip_prefix("Device for "))?;
        let (name, uri) = rest.split_once(':')?;
        if name.trim().eq_ignore_ascii_case(queue) {
            let uri = uri.trim();
            if !uri.is_empty() {
                return Some(uri.to_string());
            }
        }
    }
    None
}

/// `system default destination: CatPrinter`
pub fn parse_lpstat_d(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line
            .strip_prefix("system default destination:")
            .or_else(|| line.strip_prefix("System default destination:"))
        {
            let name = rest.trim();
            if !name.is_empty() && !name.eq_ignore_ascii_case("none") {
                return Some(name.to_string());
            }
        }
    }
    None
}

pub fn uri_points_at_port(uri: &str, port: u16) -> bool {
    let u = uri.to_ascii_lowercase();
    u.contains(&format!("127.0.0.1:{port}"))
        || u.contains(&format!("localhost:{port}"))
        || u.contains(&format!("[::1]:{port}"))
}

/// `true` when any bluetooth rfkill switch is soft- or hard-blocked.
pub fn bluetooth_blocked_from_sysfs(rfkill_dir: &Path) -> Option<bool> {
    let rd = std::fs::read_dir(rfkill_dir).ok()?;
    let mut saw = false;
    let mut blocked = false;
    for e in rd.flatten() {
        let p = e.path();
        let kind = std::fs::read_to_string(p.join("type")).ok()?;
        if !kind.trim().eq_ignore_ascii_case("bluetooth") {
            continue;
        }
        saw = true;
        let soft = std::fs::read_to_string(p.join("soft"))
            .ok()
            .and_then(|s| s.trim().parse::<u8>().ok())
            .unwrap_or(0);
        let hard = std::fs::read_to_string(p.join("hard"))
            .ok()
            .and_then(|s| s.trim().parse::<u8>().ok())
            .unwrap_or(0);
        if soft != 0 || hard != 0 {
            blocked = true;
        }
    }
    saw.then_some(blocked)
}

pub fn bluetooth_blocked() -> Option<bool> {
    bluetooth_blocked_from_sysfs(Path::new("/sys/class/rfkill"))
}

pub fn autosuspend_from_facts(facts: &host::HostFacts) -> Option<bool> {
    if facts.bt_interfaces.is_empty() {
        return None;
    }
    Some(
        facts
            .bt_interfaces
            .iter()
            .any(|i| i.power_control.as_deref() == Some("auto")),
    )
}

fn cmd_stdout(bin: &str, args: &[&str]) -> String {
    std::process::Command::new(bin)
        .args(args)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

#[derive(Debug, Clone, Default)]
pub struct DeviceSnapshot {
    pub trusted: bool,
    pub connected: bool,
    pub rssi: Option<i16>,
    pub address_type: Option<String>,
}

pub fn device_snapshot_from_objects(
    objs: &bluez::Objects,
    adapter_path: &str,
    address: &str,
) -> Option<DeviceSnapshot> {
    let live = bluez::resolve_device_from_objects(objs, adapter_path, address)?;
    let props = bluez::iface_props(objs, &live.path, bluez::IFACE_DEVICE);
    Some(DeviceSnapshot {
        trusted: props
            .and_then(|p| bluez::prop_bool(p, "Trusted"))
            .unwrap_or(false),
        connected: live.connected,
        rssi: live.rssi,
        address_type: props.and_then(|p| bluez::prop_str(p, "AddressType")),
    })
}

pub fn is_le_address_type(t: Option<&str>) -> bool {
    matches!(
        t.map(|s| s.to_ascii_lowercase()).as_deref(),
        Some("public") | Some("random")
    )
}

pub async fn connect_device_available(conn: &Connection, adapter_path: &str) -> Option<bool> {
    let proxy = zbus::fdo::IntrospectableProxy::builder(conn)
        .destination(bluez::BLUEZ)
        .ok()?
        .path(adapter_path.to_string())
        .ok()?
        .build()
        .await
        .ok()?;
    let xml = tokio::time::timeout(Duration::from_secs(2), proxy.introspect())
        .await
        .ok()?
        .ok()?;
    Some(xml.contains("ConnectDevice"))
}

pub async fn fetch_health_version(port: u16) -> Option<String> {
    let out = tokio::time::timeout(Duration::from_secs(2), async {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .ok()?;
        s.write_all(format!("GET /health HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes())
            .await
            .ok()?;
        let mut buf = String::new();
        s.read_to_string(&mut buf).await.ok()?;
        Some(buf)
    })
    .await
    .ok()??;
    let body = out.split("\r\n\r\n").nth(1)?;
    let v: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    v.get("version")?.as_str().map(String::from)
}

pub async fn collect(
    port: u16,
    queue: &str,
    adapter: Option<&str>,
    state_dir: &Path,
) -> DoctorReport {
    let facts = host::collect_default();
    let adopted_address = crate::adopt::load(state_dir);
    let experimental = facts.experimental;
    let blocked = bluetooth_blocked();
    let autosuspend = autosuspend_from_facts(&facts);

    let mut adapter_powered = None;
    let mut adapter_path = None;
    let mut trusted_le = false;
    let mut connect_device = experimental;
    let mut snap = DeviceSnapshot::default();

    if let Ok(Ok(conn)) = tokio::time::timeout(Duration::from_secs(3), Connection::system()).await {
        if let Ok(objs) = bluez::managed_objects(&conn).await {
            if let Ok(ad) = discovery::choose_adapter(&objs, adapter) {
                adapter_powered = Some(ad.powered);
                adapter_path = Some(ad.path.clone());
                if let Some(avail) = connect_device_available(&conn, &ad.path).await {
                    connect_device = avail;
                }
                if let Some(mac) = adopted_address.as_deref() {
                    if let Some(s) = device_snapshot_from_objects(&objs, &ad.path, mac) {
                        trusted_le = s.trusted && is_le_address_type(s.address_type.as_deref());
                        snap = s;
                    }
                }
            } else if let Some(ad) = discovery::adapters_from(&objs).into_iter().next() {
                adapter_powered = Some(ad.powered);
                adapter_path = Some(ad.path);
            }
        }
    }

    let lp_v = cmd_stdout("lpstat", &["-v", queue]);
    let lp_d = cmd_stdout("lpstat", &["-d"]);
    let queue_uri = parse_lpstat_v(&lp_v, queue);
    let queue_present = queue_uri.is_some();
    let queue_points_at_daemon = queue_uri
        .as_deref()
        .is_some_and(|u| uri_points_at_port(u, port));
    let default_q = parse_lpstat_d(&lp_d);
    let queue_is_default = default_q
        .as_deref()
        .is_some_and(|d| d.eq_ignore_ascii_case(queue));

    let daemon_version = fetch_health_version(port).await;
    let daemon_up = daemon_version.is_some();

    let (rssi, rssi_age_ms) = rssi_for_report(snap.connected, snap.rssi, None);

    DoctorReport {
        adopted: adopted_address.is_some(),
        adopted_address,
        trusted_le,
        connect_transport: connect_transport(connect_device),
        connect_device,
        experimental,
        queue_name: queue.to_string(),
        queue_present,
        queue_uri,
        queue_points_at_daemon,
        queue_is_default,
        port,
        daemon_up,
        daemon_version,
        adapter_powered,
        adapter_blocked: blocked,
        adapter_autosuspend: autosuspend,
        adapter_path,
        rssi,
        rssi_age_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_json_has_distinct_required_fields() {
        let r = DoctorReport {
            adopted: true,
            adopted_address: Some("AA:BB:CC:DD:EE:FF".into()),
            trusted_le: true,
            connect_transport: "le",
            connect_device: true,
            experimental: true,
            queue_name: "CatPrinter".into(),
            queue_present: true,
            queue_uri: Some("ipp://127.0.0.1:8095/ipp/print".into()),
            queue_points_at_daemon: true,
            queue_is_default: false,
            port: 8095,
            daemon_up: true,
            daemon_version: Some("0.2.6".into()),
            adapter_powered: Some(true),
            adapter_blocked: Some(false),
            adapter_autosuspend: Some(false),
            adapter_path: Some("/org/bluez/hci0".into()),
            rssi: None,
            rssi_age_ms: None,
        };
        let v: serde_json::Value = serde_json::from_str(&r.to_json()).unwrap();
        for key in [
            "trusted_le",
            "connect_transport",
            "connect_device",
            "experimental",
            "queue_present",
            "queue_points_at_daemon",
            "port",
            "daemon_up",
            "adapter_powered",
            "adapter_blocked",
            "adapter_autosuspend",
            "rssi",
            "rssi_age_ms",
        ] {
            assert!(v.get(key).is_some(), "missing {key} in {}", r.to_json());
        }
        assert_eq!(v["connect_transport"], "le");
        assert_eq!(v["trusted_le"], true);
        assert_eq!(v["connect_device"], true);
        assert_eq!(v["experimental"], true);
        assert_eq!(v["port"], 8095);
        assert_eq!(v["daemon_up"], true);
        assert!(v["rssi"].is_null());
    }

    #[test]
    fn stale_rssi_is_omitted_live_or_held_is_kept() {
        assert_eq!(rssi_for_report(false, Some(-39), None), (None, None));
        assert_eq!(rssi_for_report(false, None, None), (None, None));
        assert_eq!(rssi_for_report(true, Some(-52), None), (Some(-52), Some(0)));
        let t = Instant::now() - Duration::from_millis(40);
        let (rssi, age) = rssi_for_report(false, Some(-40), Some(t));
        assert_eq!(rssi, Some(-40));
        assert!(age.is_some_and(|a| a >= 40));
    }

    #[test]
    fn connect_transport_follows_connect_device() {
        assert_eq!(connect_transport(true), "le");
        assert_eq!(connect_transport(false), "classic");
    }

    #[test]
    fn lpstat_parsers() {
        assert_eq!(
            parse_lpstat_v(
                "device for CatPrinter: ipp://127.0.0.1:8095/ipp/print\n",
                "CatPrinter"
            )
            .as_deref(),
            Some("ipp://127.0.0.1:8095/ipp/print")
        );
        assert!(parse_lpstat_v(
            "device for Other: ipp://127.0.0.1:631/ipp/print\n",
            "CatPrinter"
        )
        .is_none());
        assert_eq!(
            parse_lpstat_d("system default destination: Office\n").as_deref(),
            Some("Office")
        );
        assert!(uri_points_at_port("ipp://127.0.0.1:8095/ipp/print", 8095));
        assert!(!uri_points_at_port("ipp://127.0.0.1:631/ipp/print", 8095));
    }

    #[test]
    fn rfkill_sysfs_reads_bluetooth_only() {
        let dir = tempfile::tempdir().unwrap();
        let bt = dir.path().join("rfkill0");
        std::fs::create_dir(&bt).unwrap();
        std::fs::write(bt.join("type"), "bluetooth\n").unwrap();
        std::fs::write(bt.join("soft"), "1\n").unwrap();
        std::fs::write(bt.join("hard"), "0\n").unwrap();
        let wlan = dir.path().join("rfkill1");
        std::fs::create_dir(&wlan).unwrap();
        std::fs::write(wlan.join("type"), "wlan\n").unwrap();
        std::fs::write(wlan.join("soft"), "0\n").unwrap();
        std::fs::write(wlan.join("hard"), "0\n").unwrap();
        assert_eq!(bluetooth_blocked_from_sysfs(dir.path()), Some(true));

        std::fs::write(bt.join("soft"), "0\n").unwrap();
        assert_eq!(bluetooth_blocked_from_sysfs(dir.path()), Some(false));
    }
}
