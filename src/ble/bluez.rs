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
