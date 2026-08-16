//! Finding the printer: adapter selection, candidate matching (registry names / AE30 service /
//! `--device` hint), cache-first picking (Connected, then strongest RSSI), and the LE scan.
//! Ported from catprinter/ble.py `_props_look_like_mxw01`, `_pick_known_props`, `_scan_and_connect`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use zbus::zvariant::Value;
use zbus::Connection;

use crate::ble::bluez::{self, Objects, Props};
use crate::printer::PrintError;
use crate::protocol::mxw01::SCAN_TIMEOUT_S;
use crate::protocol::SERVICE_UUIDS;

/// `--device`: a MAC address / BlueZ device id, or an advertised name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceHint {
    Address(String),
    Name(String),
}

impl DeviceHint {
    pub fn parse(s: &str) -> DeviceHint {
        let t = s.trim();
        let mac = t.len() == 17
            && t.split(':').count() == 6
            && t.split(':')
                .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()));
        let underscored = t.len() == 17
            && t.split('_').count() == 6
            && t.split('_')
                .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()));
        if mac || underscored {
            DeviceHint::Address(t.replace('_', ":").to_ascii_uppercase())
        } else {
            DeviceHint::Name(t.to_string())
        }
    }
}

/// A BlueZ Device1 object that might be a cat printer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: String,
    pub address: String,
    pub name: Option<String>,
    pub rssi: Option<i16>,
    pub connected: bool,
    pub uuids: Vec<String>,
}

impl Candidate {
    pub fn label(&self) -> String {
        match &self.name {
            Some(n) => format!("{n} ({})", self.address),
            None => self.address.clone(),
        }
    }
}

/// Does this look like a cat printer? Hint wins (address or exact name); else a registry name;
/// else the AE30/AF30 service in the advertised UUIDs.
///
/// Registry names shorter than 4 characters (X5, X6) are too generic to trust on their own —
/// plenty of random gadgets advertise two-letter names — so those must ALSO advertise the
/// AE30/AF30 service (every real cat printer does). Longer names match by name alone.
pub fn looks_like_cat_printer(
    name: Option<&str>,
    address: &str,
    uuids: &[String],
    hint: Option<&DeviceHint>,
) -> bool {
    let has_service = uuids
        .iter()
        .any(|u| SERVICE_UUIDS.iter().any(|s| s.eq_ignore_ascii_case(u)));
    match hint {
        Some(DeviceHint::Address(a)) => address.eq_ignore_ascii_case(a),
        Some(DeviceHint::Name(n)) => name.is_some_and(|x| x.eq_ignore_ascii_case(n.trim())),
        None => {
            if let Some(n) = name {
                if crate::models::lookup(n).is_some() && (n.trim().len() >= 4 || has_service) {
                    return true;
                }
            }
            has_service
        }
    }
}

/// Prefer an already-connected device (Settings "Connect" makes it stop advertising), then the
/// strongest signal; devices without RSSI (stale cache) come last.
pub fn pick(cands: &[Candidate]) -> Option<&Candidate> {
    cands
        .iter()
        .max_by_key(|c| (c.connected, c.rssi.unwrap_or(-999)))
}

/// Fold one scan poll into the best candidate seen so far during the settle window. A newer
/// observation of an equal-or-stronger candidate wins (RSSI refreshes between polls).
pub fn pick_after_settle(best: Option<Candidate>, live: &[Candidate]) -> Option<Candidate> {
    let cur = pick(live).cloned();
    match (best, cur) {
        (None, c) => c,
        (b, None) => b,
        (Some(b), Some(c)) => {
            let key = |x: &Candidate| (x.connected, x.rssi.unwrap_or(-999));
            Some(if key(&c) >= key(&b) { c } else { b })
        }
    }
}

/// BlueZ sets `Alias` to the dashed MAC when a device never advertised a name; treating that as
/// a name makes labels like "48-0F-57-17-06-9D (48:0F:57:17:06:9D)" and defeats name detection.
fn name_is_dashed_mac(name: &str, address: &str) -> bool {
    !address.is_empty() && name.eq_ignore_ascii_case(&address.replace(':', "-"))
}

fn candidate_from_props(path: &str, p: &Props) -> Candidate {
    let address = bluez::prop_str(p, "Address").unwrap_or_default();
    let name = bluez::prop_str(p, "Name")
        .or_else(|| bluez::prop_str(p, "Alias"))
        .filter(|s| !s.is_empty())
        .filter(|s| !name_is_dashed_mac(s, &address));
    Candidate {
        path: path.to_string(),
        address,
        name,
        rssi: bluez::prop_i16(p, "RSSI"),
        connected: bluez::prop_bool(p, "Connected").unwrap_or(false),
        uuids: bluez::prop_strs(p, "UUIDs"),
    }
}

/// Matching Device1 objects under `adapter_path`, minus the addresses in `avoid` (devices that
/// connected fine but turned out not to be cat printers).
pub fn candidates_from(
    objs: &Objects,
    adapter_path: &str,
    hint: Option<&DeviceHint>,
    avoid: &[String],
) -> Vec<Candidate> {
    let prefix = format!("{adapter_path}/");
    bluez::objects_with(objs, bluez::IFACE_DEVICE)
        .into_iter()
        .filter(|(path, _)| path.starts_with(&prefix))
        .map(|(path, p)| candidate_from_props(&path, p))
        .filter(|c| looks_like_cat_printer(c.name.as_deref(), &c.address, &c.uuids, hint))
        .filter(|c| !avoid.iter().any(|a| a.eq_ignore_ascii_case(&c.address)))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterInfo {
    pub path: String,
    pub address: String,
    pub name: String,
    pub powered: bool,
}

/// All adapters BlueZ knows about.
pub fn adapters_from(objs: &Objects) -> Vec<AdapterInfo> {
    bluez::objects_with(objs, bluez::IFACE_ADAPTER)
        .into_iter()
        .map(|(path, p)| AdapterInfo {
            path,
            address: bluez::prop_str(p, "Address").unwrap_or_default(),
            name: bluez::prop_str(p, "Alias")
                .or_else(|| bluez::prop_str(p, "Name"))
                .unwrap_or_default(),
            powered: bluez::prop_bool(p, "Powered").unwrap_or(false),
        })
        .collect()
}

/// `--adapter hciN` → that adapter (must be powered); else the first powered adapter.
pub fn choose_adapter(objs: &Objects, want: Option<&str>) -> Result<AdapterInfo, PrintError> {
    let all = adapters_from(objs);
    if let Some(w) = want {
        let path = if w.starts_with('/') {
            w.to_string()
        } else {
            format!("/org/bluez/{w}")
        };
        let a = all
            .into_iter()
            .find(|a| a.path == path)
            .ok_or_else(|| PrintError::Bus(format!("Bluetooth adapter {w} not found")))?;
        if !a.powered {
            return Err(PrintError::AdapterOff);
        }
        return Ok(a);
    }
    if all.is_empty() {
        return Err(PrintError::AdapterOff);
    }
    all.into_iter()
        .find(|a| a.powered)
        .ok_or(PrintError::AdapterOff)
}

/// LE scan for a matching device: SetDiscoveryFilter(Transport=le) + StartDiscovery, poll the
/// object tree, return the best match. Discovery is left RUNNING on purpose (connect while
/// scanning; stopping first makes BlueZ page-timeout on these toys) — the caller stops it later.
pub async fn scan(
    conn: &Connection,
    adapter: &AdapterInfo,
    hint: Option<&DeviceHint>,
    avoid: &[String],
    timeout: Duration,
) -> Result<Candidate, PrintError> {
    let ad = bluez::adapter_proxy(conn, &adapter.path).await?;
    let mut filter: HashMap<&str, Value<'_>> = HashMap::new();
    filter.insert("Transport", Value::from("le"));
    filter.insert("DuplicateData", Value::from(true));
    match tokio::time::timeout(bluez::CALL_TIMEOUT, ad.set_discovery_filter(filter)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::debug!(
            "set_discovery_filter failed (scanning unfiltered): {}",
            bluez::err_message(&e)
        ),
        Err(_) => tracing::debug!("set_discovery_filter timed out (scanning unfiltered)"),
    }
    match tokio::time::timeout(bluez::CALL_TIMEOUT, ad.start_discovery()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) if bluez::is_error_named(&e, "org.bluez.Error.InProgress") => {}
        Ok(Err(e)) => return Err(bluez::map_err(e, "starting the Bluetooth scan")),
        Err(_) => return Err(PrintError::Timeout("starting the Bluetooth scan")),
    }
    let deadline = Instant::now() + timeout;
    let mut best: Option<Candidate> = None;
    let mut settle_until: Option<Instant> = None;
    // Poll the object tree; RSSI/name arrive within a few hundred ms of the first advertisement.
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let objs = bluez::managed_objects(conn).await?;
        let cands = candidates_from(&objs, &adapter.path, hint, avoid);
        // During a scan only trust devices that are actually advertising now (have RSSI) or connected.
        let live: Vec<Candidate> = cands
            .into_iter()
            .filter(|c| c.rssi.is_some() || c.connected)
            .collect();
        best = pick_after_settle(best, &live);
        if best.is_some() {
            // An explicit --device target is unambiguous: take it right away. Autodetection
            // keeps polling ~1 s more so a stronger (closer) printer seen a beat later wins.
            if hint.is_some() {
                break;
            }
            match settle_until {
                None => settle_until = Some(Instant::now() + Duration::from_millis(1000)),
                Some(t) if Instant::now() >= t => break,
                Some(_) => {}
            }
        }
    }
    best.ok_or(PrintError::NotFound)
}

pub async fn stop_scan(conn: &Connection, adapter_path: &str) {
    if let Ok(ad) = bluez::adapter_proxy(conn, adapter_path).await {
        let _ = tokio::time::timeout(bluez::CALL_TIMEOUT, ad.stop_discovery()).await;
    }
}

pub fn default_scan_timeout() -> Duration {
    Duration::from_secs(SCAN_TIMEOUT_S)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(
        name: Option<&str>,
        addr: &str,
        rssi: Option<i16>,
        connected: bool,
        uuids: &[&str],
    ) -> Candidate {
        Candidate {
            path: format!("/org/bluez/hci0/dev_{}", addr.replace(':', "_")),
            address: addr.into(),
            name: name.map(String::from),
            rssi,
            connected,
            uuids: uuids.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn hint_parsing() {
        assert_eq!(
            DeviceHint::parse("48:0f:57:17:06:9d"),
            DeviceHint::Address("48:0F:57:17:06:9D".into())
        );
        assert_eq!(
            DeviceHint::parse("48_0F_57_17_06_9D"),
            DeviceHint::Address("48:0F:57:17:06:9D".into())
        );
        assert_eq!(DeviceHint::parse("MXW01"), DeviceHint::Name("MXW01".into()));
        assert_eq!(DeviceHint::parse(" gb03 "), DeviceHint::Name("gb03".into()));
    }

    #[test]
    fn matching_rules() {
        // registry name
        assert!(looks_like_cat_printer(
            Some("MXW01"),
            "AA:BB:CC:DD:EE:FF",
            &[],
            None
        ));
        assert!(looks_like_cat_printer(
            Some("gt01"),
            "AA:BB:CC:DD:EE:FF",
            &[],
            None
        ));
        // service uuid, unknown name
        assert!(looks_like_cat_printer(
            Some("Cat-9000"),
            "AA:BB:CC:DD:EE:FF",
            &["0000ae30-0000-1000-8000-00805f9b34fb".into()],
            None
        ));
        assert!(looks_like_cat_printer(
            None,
            "AA:BB:CC:DD:EE:FF",
            &["0000AF30-0000-1000-8000-00805F9B34FB".into()],
            None
        ));
        // neither
        assert!(!looks_like_cat_printer(
            Some("Phone"),
            "AA:BB:CC:DD:EE:FF",
            &["0000180f-0000-1000-8000-00805f9b34fb".into()],
            None
        ));
        // hints
        let addr = DeviceHint::Address("AA:BB:CC:DD:EE:FF".into());
        assert!(looks_like_cat_printer(
            Some("Phone"),
            "aa:bb:cc:dd:ee:ff",
            &[],
            Some(&addr)
        ));
        assert!(!looks_like_cat_printer(
            Some("MXW01"),
            "11:22:33:44:55:66",
            &[],
            Some(&addr)
        ));
        let name = DeviceHint::Name("MyCat".into());
        assert!(looks_like_cat_printer(
            Some("mycat"),
            "11:22:33:44:55:66",
            &[],
            Some(&name)
        ));
        assert!(!looks_like_cat_printer(
            Some("MXW01"),
            "11:22:33:44:55:66",
            &[],
            Some(&name)
        ));
    }

    #[test]
    fn picking_prefers_connected_then_rssi() {
        let a = c(Some("MXW01"), "AA:00:00:00:00:01", Some(-70), false, &[]);
        let b = c(Some("MXW01"), "AA:00:00:00:00:02", Some(-50), false, &[]);
        let d = c(Some("MXW01"), "AA:00:00:00:00:03", None, true, &[]);
        let e = c(Some("MXW01"), "AA:00:00:00:00:04", None, false, &[]);
        assert_eq!(pick(&[a.clone(), b.clone()]).unwrap().address, b.address);
        assert_eq!(
            pick(&[a.clone(), b.clone(), d.clone()]).unwrap().address,
            d.address
        );
        assert_eq!(pick(&[e.clone(), a.clone()]).unwrap().address, a.address);
        assert!(pick(&[]).is_none());
    }

    #[test]
    fn candidates_from_objects_and_adapters() {
        use zbus::zvariant::{OwnedObjectPath, OwnedValue};
        let mut objs: Objects = Default::default();
        let mk = |pairs: Vec<(&str, OwnedValue)>| -> HashMap<String, OwnedValue> {
            pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
        };
        let ov = |v: Value<'static>| OwnedValue::try_from(v).unwrap();
        let mut ifs = HashMap::new();
        ifs.insert(
            zbus::names::OwnedInterfaceName::try_from("org.bluez.Adapter1").unwrap(),
            mk(vec![
                ("Powered", ov(Value::from(true))),
                ("Address", ov(Value::from("00:11:22:33:44:55"))),
                ("Alias", ov(Value::from("alien"))),
            ]),
        );
        objs.insert(OwnedObjectPath::try_from("/org/bluez/hci0").unwrap(), ifs);
        let mut ifs = HashMap::new();
        ifs.insert(
            zbus::names::OwnedInterfaceName::try_from("org.bluez.Adapter1").unwrap(),
            mk(vec![
                ("Powered", ov(Value::from(false))),
                ("Address", ov(Value::from("00:11:22:33:44:66"))),
            ]),
        );
        objs.insert(OwnedObjectPath::try_from("/org/bluez/hci1").unwrap(), ifs);
        let mut ifs = HashMap::new();
        ifs.insert(
            zbus::names::OwnedInterfaceName::try_from("org.bluez.Device1").unwrap(),
            mk(vec![
                ("Name", ov(Value::from("MXW01"))),
                ("Address", ov(Value::from("48:0F:57:17:06:9D"))),
                ("RSSI", ov(Value::from(-60i16))),
                ("Connected", ov(Value::from(false))),
                (
                    "UUIDs",
                    ov(Value::from(vec!["0000ae30-0000-1000-8000-00805f9b34fb"])),
                ),
            ]),
        );
        objs.insert(
            OwnedObjectPath::try_from("/org/bluez/hci0/dev_48_0F_57_17_06_9D").unwrap(),
            ifs,
        );
        let mut ifs = HashMap::new();
        ifs.insert(
            zbus::names::OwnedInterfaceName::try_from("org.bluez.Device1").unwrap(),
            mk(vec![
                ("Name", ov(Value::from("Phone"))),
                ("Address", ov(Value::from("11:11:11:11:11:11"))),
            ]),
        );
        objs.insert(
            OwnedObjectPath::try_from("/org/bluez/hci0/dev_11_11_11_11_11_11").unwrap(),
            ifs,
        );

        let ad = choose_adapter(&objs, None).unwrap();
        assert_eq!(ad.path, "/org/bluez/hci0");
        assert!(matches!(
            choose_adapter(&objs, Some("hci1")),
            Err(PrintError::AdapterOff)
        ));
        assert!(choose_adapter(&objs, Some("hci9")).is_err());
        let cands = candidates_from(&objs, "/org/bluez/hci0", None, &[]);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].name.as_deref(), Some("MXW01"));
        assert_eq!(cands[0].rssi, Some(-60));
        let hinted = candidates_from(
            &objs,
            "/org/bluez/hci0",
            Some(&DeviceHint::Address("48:0f:57:17:06:9d".into())),
            &[],
        );
        assert_eq!(hinted.len(), 1);
        let none = candidates_from(&objs, "/org/bluez/hci1", None, &[]);
        assert!(none.is_empty());
        // An avoided address (connected fine but not a cat printer) is excluded, case-insensitively.
        let avoided = candidates_from(
            &objs,
            "/org/bluez/hci0",
            None,
            &["48:0f:57:17:06:9d".to_string()],
        );
        assert!(avoided.is_empty());
    }

    #[test]
    fn short_registry_names_need_the_service_uuid() {
        let ae30 = ["0000ae30-0000-1000-8000-00805f9b34fb".to_string()];
        // X5/X6 are two characters — any gadget could advertise that; require AE30/AF30 too.
        assert!(!looks_like_cat_printer(
            Some("X6"),
            "AA:BB:CC:DD:EE:FF",
            &[],
            None
        ));
        assert!(!looks_like_cat_printer(
            Some("X5"),
            "AA:BB:CC:DD:EE:FF",
            &["0000180f-0000-1000-8000-00805f9b34fb".into()],
            None
        ));
        assert!(looks_like_cat_printer(
            Some("X6"),
            "AA:BB:CC:DD:EE:FF",
            &ae30,
            None
        ));
        // Names of 4+ characters keep matching by name alone.
        assert!(looks_like_cat_printer(
            Some("MX05"),
            "AA:BB:CC:DD:EE:FF",
            &[],
            None
        ));
        assert!(looks_like_cat_printer(
            Some("GB01"),
            "AA:BB:CC:DD:EE:FF",
            &[],
            None
        ));
        // An explicit hint still matches unconditionally.
        assert!(looks_like_cat_printer(
            Some("X6"),
            "AA:BB:CC:DD:EE:FF",
            &[],
            Some(&DeviceHint::Name("X6".into()))
        ));
    }

    #[test]
    fn pick_after_settle_picks_the_stronger_rssi() {
        let weak = c(Some("MXW01"), "AA:00:00:00:00:01", Some(-82), false, &[]);
        let strong = c(Some("MXW01"), "AA:00:00:00:00:02", Some(-48), false, &[]);
        // First poll sees only the weak one; the strong one shows up a beat later and wins.
        let best = pick_after_settle(None, std::slice::from_ref(&weak));
        assert_eq!(best.as_ref().unwrap().address, weak.address);
        let best = pick_after_settle(best, &[weak.clone(), strong.clone()]);
        assert_eq!(best.as_ref().unwrap().address, strong.address);
        // A later, emptier poll does not lose the best seen so far.
        let best = pick_after_settle(best, &[]);
        assert_eq!(best.as_ref().unwrap().address, strong.address);
        // A refreshed observation of the same device replaces the stale one.
        let refreshed = c(Some("MXW01"), "AA:00:00:00:00:02", Some(-40), false, &[]);
        let best = pick_after_settle(best, std::slice::from_ref(&refreshed));
        assert_eq!(best.as_ref().unwrap().rssi, Some(-40));
        // A connected device beats any RSSI.
        let held = c(Some("MXW01"), "AA:00:00:00:00:03", None, true, &[]);
        let best = pick_after_settle(best, std::slice::from_ref(&held));
        assert_eq!(best.unwrap().address, held.address);
    }

    #[test]
    fn dashed_mac_alias_is_not_a_name() {
        assert!(name_is_dashed_mac("48-0F-57-17-06-9D", "48:0F:57:17:06:9D"));
        assert!(name_is_dashed_mac("48-0f-57-17-06-9d", "48:0F:57:17:06:9D"));
        assert!(!name_is_dashed_mac("MXW01", "48:0F:57:17:06:9D"));
        assert!(!name_is_dashed_mac("", ""));
    }
}
