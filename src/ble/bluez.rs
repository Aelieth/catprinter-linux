//! BlueZ D-Bus API surface (zbus proxies), managed-object helpers, error mapping.
//!
//! Every call goes through `call()` which applies a timeout — zbus itself never times out.
//! Proxies are built with `CacheProperties::No` so property reads are always live.

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use zbus::proxy::CacheProperties;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::Connection;

use crate::printer::PrintError;

pub const BLUEZ: &str = "org.bluez";
pub const IFACE_ADAPTER: &str = "org.bluez.Adapter1";
pub const IFACE_DEVICE: &str = "org.bluez.Device1";
pub const IFACE_SERVICE: &str = "org.bluez.GattService1";
pub const IFACE_CHAR: &str = "org.bluez.GattCharacteristic1";

/// Default per-call timeout for cheap D-Bus calls.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(5);

#[zbus::proxy(interface = "org.bluez.Adapter1", default_service = "org.bluez")]
pub trait Adapter1 {
    fn start_discovery(&self) -> zbus::Result<()>;
    fn stop_discovery(&self) -> zbus::Result<()>;
    fn set_discovery_filter(&self, filter: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
    #[zbus(property)]
    fn powered(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_powered(&self, value: bool) -> zbus::Result<()>;
    #[zbus(property)]
    fn discovering(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn address(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn alias(&self) -> zbus::Result<String>;
}

#[zbus::proxy(interface = "org.bluez.Device1", default_service = "org.bluez")]
pub trait Device1 {
    fn connect(&self) -> zbus::Result<()>;
    fn disconnect(&self) -> zbus::Result<()>;
    #[zbus(property)]
    fn connected(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn services_resolved(&self) -> zbus::Result<bool>;
    #[zbus(property, name = "RSSI")]
    fn rssi(&self) -> zbus::Result<i16>;
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn alias(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn address(&self) -> zbus::Result<String>;
    #[zbus(property, name = "UUIDs")]
    fn uuids(&self) -> zbus::Result<Vec<String>>;
    #[zbus(property)]
    fn adapter(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn paired(&self) -> zbus::Result<bool>;
}

#[zbus::proxy(interface = "org.bluez.GattService1", default_service = "org.bluez")]
pub trait GattService1 {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn device(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn primary(&self) -> zbus::Result<bool>;
}

#[zbus::proxy(
    interface = "org.bluez.GattCharacteristic1",
    default_service = "org.bluez"
)]
pub trait GattCharacteristic1 {
    fn write_value(&self, value: &[u8], options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
    fn start_notify(&self) -> zbus::Result<()>;
    fn stop_notify(&self) -> zbus::Result<()>;
    fn acquire_write(
        &self,
        options: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<(zbus::zvariant::OwnedFd, u16)>;
    fn acquire_notify(
        &self,
        options: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<(zbus::zvariant::OwnedFd, u16)>;
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn flags(&self) -> zbus::Result<Vec<String>>;
    #[zbus(property, name = "MTU")]
    fn mtu(&self) -> zbus::Result<u16>;
    #[zbus(property)]
    fn service(&self) -> zbus::Result<OwnedObjectPath>;
}

/// Connect to the system bus (short timeout; failure = no D-Bus / no BlueZ).
pub async fn system_bus() -> Result<Connection, PrintError> {
    match tokio::time::timeout(Duration::from_secs(3), Connection::system()).await {
        Ok(Ok(c)) => Ok(c),
        Ok(Err(e)) => {
            tracing::debug!("system bus: {e}");
            Err(PrintError::NoBluetoothd)
        }
        Err(_) => Err(PrintError::Timeout("connecting to the system bus")),
    }
}

/// Run a D-Bus call with a timeout, mapping errors.
pub async fn call<T, F>(what: &'static str, timeout: Duration, fut: F) -> Result<T, PrintError>
where
    F: Future<Output = zbus::Result<T>>,
{
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(map_err(e, what)),
        Err(_) => Err(PrintError::Timeout(what)),
    }
}

/// The D-Bus error name of a zbus error, if it is a method error.
pub fn err_name(e: &zbus::Error) -> Option<String> {
    match e {
        zbus::Error::MethodError(name, _, _) => Some(name.as_str().to_string()),
        zbus::Error::FDO(fdo) => Some(match &**fdo {
            zbus::fdo::Error::ServiceUnknown(_) => {
                "org.freedesktop.DBus.Error.ServiceUnknown".into()
            }
            zbus::fdo::Error::NameHasNoOwner(_) => {
                "org.freedesktop.DBus.Error.NameHasNoOwner".into()
            }
            zbus::fdo::Error::UnknownObject(_) => "org.freedesktop.DBus.Error.UnknownObject".into(),
            zbus::fdo::Error::UnknownMethod(_) => "org.freedesktop.DBus.Error.UnknownMethod".into(),
            zbus::fdo::Error::UnknownInterface(_) => {
                "org.freedesktop.DBus.Error.UnknownInterface".into()
            }
            other => format!("{other:?}"),
        }),
        _ => None,
    }
}

/// The human message carried by a D-Bus method error.
pub fn err_message(e: &zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(_, Some(msg), _) => msg.clone(),
        zbus::Error::MethodError(name, None, _) => name.as_str().to_string(),
        other => other.to_string(),
    }
}

/// Is this a "device object vanished / not connected" kind of error?
pub fn is_gone(e: &zbus::Error) -> bool {
    match err_name(e).as_deref() {
        Some("org.freedesktop.DBus.Error.UnknownObject") | Some("org.bluez.Error.NotConnected") => {
            true
        }
        Some("org.bluez.Error.Failed") => {
            let m = err_message(e).to_ascii_lowercase();
            m.contains("not connected") || m.contains("disconnected")
        }
        _ => false,
    }
}

pub fn is_error_named(e: &zbus::Error, name: &str) -> bool {
    err_name(e).as_deref() == Some(name)
}

/// Map a zbus error to a PrintError. Callers special-case InProgress / AlreadyConnected first.
pub fn map_err(e: zbus::Error, what: &'static str) -> PrintError {
    let name = err_name(&e).unwrap_or_default();
    let msg = err_message(&e);
    match name.as_str() {
        "org.freedesktop.DBus.Error.ServiceUnknown"
        | "org.freedesktop.DBus.Error.NameHasNoOwner" => PrintError::NoBluetoothd,
        "org.bluez.Error.NotReady" => PrintError::AdapterOff,
        "org.freedesktop.DBus.Error.UnknownObject" | "org.bluez.Error.NotConnected" => {
            PrintError::LinkLost
        }
        "org.bluez.Error.Failed"
        | "org.bluez.Error.Timeout"
        | "org.freedesktop.DBus.Error.NoReply"
            if what == "connect" =>
        {
            PrintError::ConnectFailed {
                attempts: 1,
                last: msg,
                hint: String::new(),
            }
        }
        // bluez answers writes on a dropped link with Failed("Not connected") — that is a lost
        // link (retryable), not a generic bus error.
        _ if is_gone(&e) => PrintError::LinkLost,
        _ => match e {
            zbus::Error::InputOutput(_)
            | zbus::Error::Connection(_, _)
            | zbus::Error::Handshake(_)
            | zbus::Error::Address(_) => PrintError::NoBluetoothd,
            _ => PrintError::Bus(format!("{what}: {msg}")),
        },
    }
}

/// `org.freedesktop.DBus.ObjectManager` on org.bluez.
pub async fn object_manager(
    conn: &Connection,
) -> Result<zbus::fdo::ObjectManagerProxy<'static>, PrintError> {
    zbus::fdo::ObjectManagerProxy::builder(conn)
        .destination(BLUEZ)
        .map_err(|e| PrintError::Bus(e.to_string()))?
        .path("/")
        .map_err(|e| PrintError::Bus(e.to_string()))?
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(|e| PrintError::Bus(e.to_string()))
}

pub type Objects = zbus::fdo::ManagedObjects;

/// All BlueZ objects.
pub async fn managed_objects(conn: &Connection) -> Result<Objects, PrintError> {
    let om = object_manager(conn).await?;
    match tokio::time::timeout(CALL_TIMEOUT, om.get_managed_objects()).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(map_fdo(e)),
        Err(_) => Err(PrintError::Timeout("listing Bluetooth objects")),
    }
}

fn map_fdo(e: zbus::fdo::Error) -> PrintError {
    match e {
        zbus::fdo::Error::ServiceUnknown(_) | zbus::fdo::Error::NameHasNoOwner(_) => {
            PrintError::NoBluetoothd
        }
        other => PrintError::Bus(other.to_string()),
    }
}

pub async fn adapter_proxy(
    conn: &Connection,
    path: &str,
) -> Result<Adapter1Proxy<'static>, PrintError> {
    Adapter1Proxy::builder(conn)
        .path(path.to_string())
        .map_err(|e| PrintError::Bus(e.to_string()))?
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(|e| PrintError::Bus(e.to_string()))
}

pub async fn device_proxy(
    conn: &Connection,
    path: &str,
) -> Result<Device1Proxy<'static>, PrintError> {
    Device1Proxy::builder(conn)
        .path(path.to_string())
        .map_err(|e| PrintError::Bus(e.to_string()))?
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(|e| PrintError::Bus(e.to_string()))
}

pub async fn char_proxy(
    conn: &Connection,
    path: &str,
) -> Result<GattCharacteristic1Proxy<'static>, PrintError> {
    GattCharacteristic1Proxy::builder(conn)
        .path(path.to_string())
        .map_err(|e| PrintError::Bus(e.to_string()))?
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(|e| PrintError::Bus(e.to_string()))
}

// ---- property extraction from ManagedObjects dictionaries ---------------------------------------

pub type Props = HashMap<String, OwnedValue>;

pub fn prop_str(p: &Props, key: &str) -> Option<String> {
    let v = p.get(key)?;
    match &**v {
        Value::Str(s) => Some(s.to_string()),
        Value::ObjectPath(o) => Some(o.to_string()),
        _ => None,
    }
}

pub fn prop_bool(p: &Props, key: &str) -> Option<bool> {
    match &**p.get(key)? {
        Value::Bool(b) => Some(*b),
        _ => None,
    }
}

pub fn prop_i16(p: &Props, key: &str) -> Option<i16> {
    match &**p.get(key)? {
        Value::I16(v) => Some(*v),
        Value::I32(v) => i16::try_from(*v).ok(),
        _ => None,
    }
}

pub fn prop_u16(p: &Props, key: &str) -> Option<u16> {
    match &**p.get(key)? {
        Value::U16(v) => Some(*v),
        Value::U32(v) => u16::try_from(*v).ok(),
        _ => None,
    }
}

pub fn prop_strs(p: &Props, key: &str) -> Vec<String> {
    match p.get(key).map(|v| &**v) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| match x {
                Value::Str(s) => Some(s.to_string()),
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}

/// Bytes of an `ay` value (notification payloads in PropertiesChanged).
pub fn value_bytes(v: &Value<'_>) -> Option<Vec<u8>> {
    match v {
        Value::Array(a) => {
            let mut out = Vec::with_capacity(a.len());
            for x in a.iter() {
                match x {
                    Value::U8(b) => out.push(*b),
                    _ => return None,
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// Interface properties of an object, if the object implements the interface.
pub fn iface_props<'a>(objs: &'a Objects, path: &str, iface: &str) -> Option<&'a Props> {
    objs.iter()
        .find(|(p, _)| p.as_str() == path)
        .and_then(|(_, ifs)| {
            ifs.iter()
                .find(|(i, _)| i.as_str() == iface)
                .map(|(_, p)| p)
        })
}

/// Iterate objects implementing `iface`: (path, props).
pub fn objects_with<'a>(objs: &'a Objects, iface: &str) -> Vec<(String, &'a Props)> {
    let mut v: Vec<(String, &Props)> = objs
        .iter()
        .filter_map(|(p, ifs)| {
            ifs.iter()
                .find(|(i, _)| i.as_str() == iface)
                .map(|(_, props)| (p.as_str().to_string(), props))
        })
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

// ---- live Device1 resolution + connect-error classification ------------------------------------

/// Hex digits of a BD_ADDR, uppercased, no separators. `"aa:bb:…"` / `"AA_BB_…"` → `"AABB…"`.
pub fn compact_bdaddr(address: &str) -> String {
    address
        .bytes()
        .filter(u8::is_ascii_hexdigit)
        .map(|b| b.to_ascii_uppercase() as char)
        .collect()
}

/// Conventional BlueZ device path: `/org/bluez/hci0` + `AA:BB:…` → `/org/bluez/hci0/dev_AA_BB_…`.
pub fn device_path_for(adapter_path: &str, address: &str) -> String {
    let compact = compact_bdaddr(address);
    let mut hex = compact.as_str();
    let mut parts = Vec::with_capacity(6);
    while hex.len() >= 2 {
        parts.push(&hex[..2]);
        hex = &hex[2..];
    }
    format!("{adapter_path}/dev_{}", parts.join("_"))
}

/// `/org/bluez/hci0/dev_XX_XX_…` → `/org/bluez/hci0`.
pub fn adapter_from_device_path(device_path: &str) -> Option<&str> {
    device_path
        .rsplit_once("/dev_")
        .map(|(adapter, _)| adapter)
        .filter(|adapter| !adapter.is_empty())
}

/// A Device1 object that exists in the current managed-objects snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveDevice {
    pub path: String,
    pub rssi: Option<i16>,
    pub connected: bool,
}

/// Whether we may issue Device1.Connect *now*.
///
/// A cache hit with no RSSI (TemporaryTimeout=0 keeps unpaired objects forever)
/// is not connectable: BlueZ will page-timeout for the full CONNECT_TIMEOUT_S
/// while the printer is not advertising. Combo cards (RTL8822CE / MT7925)
/// make that timeout the dominant failure after the kit sets TemporaryTimeout=0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectReadiness {
    Ready,
    Silent,
    Missing,
}

pub fn connect_readiness(dev: Option<&LiveDevice>) -> ConnectReadiness {
    match dev {
        None => ConnectReadiness::Missing,
        Some(d) if d.connected || d.rssi.is_some() => ConnectReadiness::Ready,
        Some(_) => ConnectReadiness::Silent,
    }
}

/// Look up a live Device1 for `address` under `adapter_path`.
///
/// Walks Address properties first; if none match, falls back to the conventional
/// `…/dev_XX_XX_…` path when that object still exists. `None` means BlueZ has
/// already pruned the temporary object (TemporaryTimeout).
pub fn resolve_device_from_objects(
    objs: &Objects,
    adapter_path: &str,
    address: &str,
) -> Option<LiveDevice> {
    let prefix = format!("{adapter_path}/");
    let want = compact_bdaddr(address);
    if want.is_empty() {
        return None;
    }
    for (path, props) in objects_with(objs, IFACE_DEVICE) {
        if !path.starts_with(&prefix) {
            continue;
        }
        let Some(addr) = prop_str(props, "Address") else {
            continue;
        };
        if compact_bdaddr(&addr) == want {
            return Some(LiveDevice {
                path,
                rssi: prop_i16(props, "RSSI"),
                connected: prop_bool(props, "Connected").unwrap_or(false),
            });
        }
    }
    let conventional = device_path_for(adapter_path, address);
    if objs.keys().any(|p| p.as_str() == conventional) {
        let props = iface_props(objs, &conventional, IFACE_DEVICE);
        return Some(LiveDevice {
            path: conventional,
            rssi: props.and_then(|p| prop_i16(p, "RSSI")),
            connected: props
                .and_then(|p| prop_bool(p, "Connected"))
                .unwrap_or(false),
        });
    }
    None
}

/// Live Adapter1 state from a managed-objects snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterHealth {
    Powered,
    Unpowered,
    Missing,
}

pub fn adapter_health_from_objects(objs: &Objects, adapter_path: &str) -> AdapterHealth {
    match iface_props(objs, adapter_path, IFACE_ADAPTER) {
        None => AdapterHealth::Missing,
        Some(p) if prop_bool(p, "Powered").unwrap_or(false) => AdapterHealth::Powered,
        Some(_) => AdapterHealth::Unpowered,
    }
}

/// Wait for the adapter object to exist and be Powered. Unpowered → `Set Powered true`
/// (D-Bus, not a config write). Missing → USB reset / firmware reload in progress.
/// Returns whether the adapter is usable when the wait ends.
pub async fn recover_adapter(conn: &Connection, adapter_path: &str) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    let mut asked_power = false;
    loop {
        match managed_objects(conn).await {
            Ok(objs) => match adapter_health_from_objects(&objs, adapter_path) {
                AdapterHealth::Powered => return true,
                AdapterHealth::Unpowered if !asked_power => {
                    asked_power = true;
                    if let Ok(ad) = adapter_proxy(conn, adapter_path).await {
                        match tokio::time::timeout(CALL_TIMEOUT, ad.set_powered(true)).await {
                            Ok(Ok(())) => tracing::info!(
                                path = %adapter_path,
                                "adapter was off; Set Powered true"
                            ),
                            Ok(Err(e)) => tracing::debug!(
                                "Set Powered on {adapter_path}: {}",
                                err_message(&e)
                            ),
                            Err(_) => tracing::debug!("Set Powered on {adapter_path} timed out"),
                        }
                    }
                }
                AdapterHealth::Missing => {
                    tracing::warn!(
                        path = %adapter_path,
                        "adapter object gone (USB reset / firmware reload?); waiting"
                    );
                }
                AdapterHealth::Unpowered => {}
            },
            Err(e) => tracing::debug!("recover_adapter list objects: {e}"),
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Fetch managed objects and resolve. `None` is a prune, not a bus error.
pub async fn resolve_device_path(
    conn: &Connection,
    adapter_path: &str,
    address: &str,
) -> Result<Option<LiveDevice>, PrintError> {
    let objs = managed_objects(conn).await?;
    Ok(resolve_device_from_objects(&objs, adapter_path, address))
}

/// Poll until the device is advertising (RSSI) or already Connected, or `budget` elapses.
pub async fn wait_until_ready_to_connect(
    conn: &Connection,
    adapter_path: &str,
    address: &str,
    budget: Duration,
) -> Result<Option<LiveDevice>, PrintError> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let live = resolve_device_path(conn, adapter_path, address).await?;
        if connect_readiness(live.as_ref()) == ConnectReadiness::Ready {
            return Ok(live);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(live);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Why a Connect attempt failed. Stable `as_str` prefixes go into journal / `ConnectFailed.last`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectFailureKind {
    /// TemporaryTimeout / UnknownObject / Device1 gone.
    Pruned,
    /// Host aborted the LE link (`le-connection-abort-by-local`, ECONNABORTED).
    HostAbort,
    /// Wall-clock or BlueZ timeout.
    Timeout,
    /// Adapter powered off / NotReady.
    AdapterOff,
    /// Device1 exists (often TemporaryTimeout=0) but is not advertising and not held.
    NotAdvertising,
    Other,
}

impl ConnectFailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pruned => "pruned",
            Self::HostAbort => "host-abort",
            Self::Timeout => "timeout",
            Self::AdapterOff => "adapter-off",
            Self::NotAdvertising => "not-advertising",
            Self::Other => "other",
        }
    }

    /// `"{kind}: {detail}"` (or just the kind when detail is empty).
    pub fn format_last(self, detail: &str) -> String {
        let d = detail.trim();
        if d.is_empty() {
            self.as_str().to_string()
        } else {
            format!("{}: {d}", self.as_str())
        }
    }
}

/// Classify a connect-time zbus error via its D-Bus name and message.
pub fn classify_connect_error(e: &zbus::Error) -> ConnectFailureKind {
    if is_gone(e) {
        return ConnectFailureKind::Pruned;
    }
    classify_connect_name_message(err_name(e).as_deref(), &err_message(e))
}

/// Classify from representative BlueZ error names + messages. Wall-clock timeouts
/// never produce a zbus error — callers map those to [`ConnectFailureKind::Timeout`]
/// directly.
pub fn classify_connect_name_message(name: Option<&str>, message: &str) -> ConnectFailureKind {
    let name = name.unwrap_or("");
    let msg = message.to_ascii_lowercase();

    if matches!(
        name,
        "org.freedesktop.DBus.Error.UnknownObject" | "org.bluez.Error.NotConnected"
    ) || msg.contains("doesn't exist")
        || msg.contains("unknown object")
        || (name == "org.bluez.Error.Failed"
            && (msg.contains("not connected") || msg.contains("disconnected")))
    {
        return ConnectFailureKind::Pruned;
    }

    // BlueZ device.c: EHOSTUNREACH = adapter not powered, ECONNABORTED = adapter
    // powered down. Distinct from HCI_ERROR_LOCAL_HOST_TERM (host-abort).
    if name == "org.bluez.Error.NotReady"
        || msg.contains("adapter not powered")
        || msg.contains("ehostunreach")
        || msg.contains("resource not ready")
        || msg.contains("no such device")
        || msg.contains("hci down")
        || (msg.contains("econnaborted") && !msg.contains("le-connection-abort-by-local"))
    {
        return ConnectFailureKind::AdapterOff;
    }

    if msg.contains("le-connection-abort-by-local") || msg.contains("connection aborted") {
        return ConnectFailureKind::HostAbort;
    }

    if name == "org.bluez.Error.Timeout"
        || name == "org.freedesktop.DBus.Error.NoReply"
        || ((name == "org.bluez.Error.Failed" || name.is_empty())
            && (msg.contains("timed out") || msg.contains("timeout")))
    {
        return ConnectFailureKind::Timeout;
    }

    ConnectFailureKind::Other
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

    fn ov(v: Value<'static>) -> OwnedValue {
        OwnedValue::try_from(v).unwrap()
    }

    fn insert_iface(
        objs: &mut Objects,
        path: &str,
        iface: &str,
        props: Vec<(&str, Value<'static>)>,
    ) {
        let mut map = HashMap::new();
        for (k, v) in props {
            map.insert(k.to_string(), ov(v));
        }
        let mut ifaces = objs
            .remove(&OwnedObjectPath::try_from(path).unwrap())
            .unwrap_or_default();
        ifaces.insert(
            zbus::names::OwnedInterfaceName::try_from(iface).unwrap(),
            map,
        );
        objs.insert(OwnedObjectPath::try_from(path).unwrap(), ifaces);
    }

    fn insert_empty(objs: &mut Objects, path: &str) {
        objs.insert(OwnedObjectPath::try_from(path).unwrap(), HashMap::new());
    }

    #[test]
    fn compact_and_conventional_path() {
        assert_eq!(compact_bdaddr("aa:bb:cc:dd:ee:ff"), "AABBCCDDEEFF");
        assert_eq!(compact_bdaddr("AA_BB_CC_DD_EE_FF"), "AABBCCDDEEFF");
        assert_eq!(
            device_path_for("/org/bluez/hci0", "48:0f:57:17:06:9d"),
            "/org/bluez/hci0/dev_48_0F_57_17_06_9D"
        );
        assert_eq!(
            device_path_for("/org/bluez/hci1", "48_0F_57_17_06_9D"),
            "/org/bluez/hci1/dev_48_0F_57_17_06_9D"
        );
        assert_eq!(
            adapter_from_device_path("/org/bluez/hci0/dev_48_0F_57_17_06_9D"),
            Some("/org/bluez/hci0")
        );
        assert_eq!(adapter_from_device_path("/org/bluez/hci0"), None);
        assert_eq!(adapter_from_device_path("/dev_AA"), None);
    }

    #[test]
    fn resolve_live_path_from_address() {
        let mut objs: Objects = Default::default();
        insert_iface(
            &mut objs,
            "/org/bluez/hci0/dev_48_0F_57_17_06_9D",
            IFACE_DEVICE,
            vec![
                ("Address", Value::from("48:0F:57:17:06:9D")),
                ("RSSI", Value::from(-52i16)),
            ],
        );
        let live = resolve_device_from_objects(&objs, "/org/bluez/hci0", "48:0f:57:17:06:9d")
            .expect("live");
        assert_eq!(live.path, "/org/bluez/hci0/dev_48_0F_57_17_06_9D");
        assert_eq!(live.rssi, Some(-52));
    }

    #[test]
    fn resolve_prefers_address_match_over_conventional_name() {
        let mut objs: Objects = Default::default();
        // Same BD_ADDR living under a non-conventional path.
        insert_iface(
            &mut objs,
            "/org/bluez/hci0/dev_TMP_1",
            IFACE_DEVICE,
            vec![
                ("Address", Value::from("AA:BB:CC:DD:EE:FF")),
                ("RSSI", Value::from(-40i16)),
            ],
        );
        insert_empty(&mut objs, "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF");
        let live =
            resolve_device_from_objects(&objs, "/org/bluez/hci0", "AA:BB:CC:DD:EE:FF").unwrap();
        assert_eq!(live.path, "/org/bluez/hci0/dev_TMP_1");
        assert_eq!(live.rssi, Some(-40));
    }

    #[test]
    fn resolve_conventional_fallback_when_object_exists() {
        let mut objs: Objects = Default::default();
        // Object exists at the conventional path but has no Address property — walk misses it.
        insert_iface(
            &mut objs,
            "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF",
            IFACE_DEVICE,
            vec![("RSSI", Value::from(-61i16))],
        );
        let live =
            resolve_device_from_objects(&objs, "/org/bluez/hci0", "aa:bb:cc:dd:ee:ff").unwrap();
        assert_eq!(live.path, "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF");
        assert_eq!(live.rssi, Some(-61));
    }

    #[test]
    fn connect_readiness_requires_rssi_or_held() {
        assert_eq!(connect_readiness(None), ConnectReadiness::Missing);
        let silent = LiveDevice {
            path: "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF".into(),
            rssi: None,
            connected: false,
        };
        assert_eq!(connect_readiness(Some(&silent)), ConnectReadiness::Silent);
        let adv = LiveDevice {
            rssi: Some(-52),
            ..silent.clone()
        };
        assert_eq!(connect_readiness(Some(&adv)), ConnectReadiness::Ready);
        let held = LiveDevice {
            connected: true,
            rssi: None,
            ..silent
        };
        assert_eq!(connect_readiness(Some(&held)), ConnectReadiness::Ready);
    }

    #[test]
    fn resolve_silent_cache_is_not_ready() {
        let mut objs: Objects = Default::default();
        insert_iface(
            &mut objs,
            "/org/bluez/hci0/dev_48_0F_57_17_06_9D",
            IFACE_DEVICE,
            vec![
                ("Address", Value::from("48:0F:57:17:06:9D")),
                ("Connected", Value::from(false)),
            ],
        );
        let live =
            resolve_device_from_objects(&objs, "/org/bluez/hci0", "48:0F:57:17:06:9D").unwrap();
        assert!(live.rssi.is_none());
        assert!(!live.connected);
        assert_eq!(connect_readiness(Some(&live)), ConnectReadiness::Silent);
    }

    #[test]
    fn resolve_none_when_pruned() {
        let objs: Objects = Default::default();
        assert!(
            resolve_device_from_objects(&objs, "/org/bluez/hci0", "AA:BB:CC:DD:EE:FF").is_none()
        );
    }

    #[test]
    fn resolve_does_not_cross_adapters() {
        let mut objs: Objects = Default::default();
        insert_iface(
            &mut objs,
            "/org/bluez/hci1/dev_AA_BB_CC_DD_EE_FF",
            IFACE_DEVICE,
            vec![("Address", Value::from("AA:BB:CC:DD:EE:FF"))],
        );
        assert!(
            resolve_device_from_objects(&objs, "/org/bluez/hci0", "AA:BB:CC:DD:EE:FF").is_none()
        );
    }

    fn method_err(name: &str, msg: &str) -> zbus::Error {
        let call = zbus::Message::method_call("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF", "Connect")
            .expect("builder")
            .build(&())
            .expect("message");
        let ename = zbus::names::ErrorName::try_from(name).expect("error name");
        zbus::Error::MethodError(ename.into(), Some(msg.to_string()), call)
    }

    #[test]
    fn classify_prune_host_abort_timeout_adapter_off_other() {
        let unknown = zbus::Error::FDO(Box::new(zbus::fdo::Error::UnknownObject(
            "Method \"Connect\" with signature \"\" on interface \"org.bluez.Device1\" doesn't exist"
                .into(),
        )));
        assert_eq!(classify_connect_error(&unknown), ConnectFailureKind::Pruned);
        assert_eq!(
            classify_connect_name_message(
                Some("org.freedesktop.DBus.Error.UnknownObject"),
                "Method \"Connect\" … doesn't exist"
            ),
            ConnectFailureKind::Pruned
        );
        assert_eq!(
            classify_connect_name_message(
                Some("org.freedesktop.DBus.Error.UnknownMethod"),
                "Method \"Connect\" with signature \"\" on interface \"org.bluez.Device1\" doesn't exist"
            ),
            ConnectFailureKind::Pruned
        );

        let abort = method_err(
            "org.bluez.Error.Failed",
            "br-connection-canceled, le-connection-abort-by-local",
        );
        assert_eq!(
            classify_connect_error(&abort),
            ConnectFailureKind::HostAbort
        );
        assert_eq!(
            classify_connect_name_message(Some("org.bluez.Error.Failed"), "connection aborted"),
            ConnectFailureKind::HostAbort
        );
        assert_eq!(
            classify_connect_name_message(None, "ECONNABORTED"),
            ConnectFailureKind::AdapterOff
        );
        assert_eq!(
            classify_connect_name_message(
                Some("org.bluez.Error.Failed"),
                "br-connection-canceled, Adapter not powered"
            ),
            ConnectFailureKind::AdapterOff
        );

        let timed = method_err("org.bluez.Error.Failed", "Operation timed out");
        assert_eq!(classify_connect_error(&timed), ConnectFailureKind::Timeout);
        assert_eq!(
            classify_connect_name_message(Some("org.bluez.Error.Failed"), "timed out"),
            ConnectFailureKind::Timeout
        );
        assert_eq!(
            classify_connect_name_message(Some("org.bluez.Error.Timeout"), "gave up"),
            ConnectFailureKind::Timeout
        );

        let off = method_err("org.bluez.Error.NotReady", "Resource Not Ready");
        assert_eq!(classify_connect_error(&off), ConnectFailureKind::AdapterOff);
        assert_eq!(
            classify_connect_name_message(Some("org.bluez.Error.NotReady"), ""),
            ConnectFailureKind::AdapterOff
        );

        let other = method_err(
            "org.bluez.Error.Failed",
            "br-connection-profile-unavailable",
        );
        assert_eq!(classify_connect_error(&other), ConnectFailureKind::Other);

        assert_eq!(
            ConnectFailureKind::Pruned.format_last("device object gone"),
            "pruned: device object gone"
        );
        assert_eq!(
            ConnectFailureKind::HostAbort.format_last("le-connection-abort-by-local"),
            "host-abort: le-connection-abort-by-local"
        );
        assert_eq!(
            ConnectFailureKind::Timeout.format_last("timed out"),
            "timeout: timed out"
        );
        assert_eq!(
            ConnectFailureKind::NotAdvertising.format_last("Device1 exists but is not advertising"),
            "not-advertising: Device1 exists but is not advertising"
        );
    }

    #[test]
    fn adapter_health_powered_unpowered_missing() {
        let mut objs: Objects = Default::default();
        assert_eq!(
            adapter_health_from_objects(&objs, "/org/bluez/hci0"),
            AdapterHealth::Missing
        );
        insert_iface(
            &mut objs,
            "/org/bluez/hci0",
            IFACE_ADAPTER,
            vec![("Powered", Value::from(false))],
        );
        assert_eq!(
            adapter_health_from_objects(&objs, "/org/bluez/hci0"),
            AdapterHealth::Unpowered
        );
        insert_iface(
            &mut objs,
            "/org/bluez/hci0",
            IFACE_ADAPTER,
            vec![("Powered", Value::from(true))],
        );
        assert_eq!(
            adapter_health_from_objects(&objs, "/org/bluez/hci0"),
            AdapterHealth::Powered
        );
    }
}
