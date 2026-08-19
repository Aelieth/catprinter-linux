//! `catprinterd adopt`: discover (or pin) the printer, force an LE-only BlueZ object, Trust it,
//! persist the MAC. `--status` / `--forget` do not need the printer on.

use std::collections::HashMap;
use std::path::Path;

use zbus::zvariant::Value;
use zbus::Connection;

use crate::adopt::{self, AdoptOutcome};
use crate::ble::{bluez, discovery};
use crate::config::AdoptArgs;
use crate::printer::PrintError;
use crate::protocol::mxw01::LIVE_CONNECT_TIMEOUT_S;

pub async fn run(args: AdoptArgs) -> i32 {
    let explicit = args.state_dir.as_deref();
    if args.status {
        let o = adopt::status_any(explicit);
        println!("{}", o.message());
        return o.exit_code();
    }
    if args.forget {
        let o = forget(explicit, args.device.as_deref()).await;
        println!("{}", o.message());
        return o.exit_code();
    }
    match adopt_now(explicit, args.device.as_deref(), args.adapter.as_deref()).await {
        Ok(o) => {
            if matches!(o, AdoptOutcome::PrinterOff | AdoptOutcome::NeedExperimental) {
                eprintln!("{}", o.message());
            } else {
                println!("{}", o.message());
            }
            o.exit_code()
        }
        Err(e) => {
            eprintln!("{e}");
            2
        }
    }
}

async fn forget(explicit: Option<&Path>, device: Option<&str>) -> AdoptOutcome {
    let mac = device
        .and_then(adopt::normalize_mac)
        .or_else(|| adopt::load_any(explicit));
    let _ = adopt::clear_all(explicit);
    if let Some(ref mac) = mac {
        let _ = remove_trusted_record(None, mac).await;
    }
    AdoptOutcome::Forgotten { address: mac }
}

/// Remove the adopted Device1 so uninstall does not leave a trusted ghost.
pub async fn remove_trusted_record(adapter: Option<&str>, mac: &str) -> Result<(), PrintError> {
    let conn = bluez::system_bus().await?;
    let objs = bluez::managed_objects(&conn).await?;
    let ad = match discovery::choose_adapter(&objs, adapter) {
        Ok(a) => a,
        Err(_) => {
            let all = discovery::adapters_from(&objs);
            match all.into_iter().next() {
                Some(a) => a,
                None => return Ok(()),
            }
        }
    };
    let Some(live) = bluez::resolve_device_from_objects(&objs, &ad.path, mac) else {
        return Ok(());
    };
    let proxy = bluez::adapter_proxy(&conn, &ad.path).await?;
    if let Ok(p) = zbus::zvariant::ObjectPath::try_from(live.path.as_str()) {
        let _ = tokio::time::timeout(bluez::CALL_TIMEOUT, proxy.remove_device(&p)).await;
    }
    Ok(())
}

async fn device_is_trusted(adapter: Option<&str>, mac: &str) -> bool {
    let Ok(conn) = bluez::system_bus().await else {
        return false;
    };
    let Ok(objs) = bluez::managed_objects(&conn).await else {
        return false;
    };
    let Ok(ad) = discovery::choose_adapter(&objs, adapter) else {
        return false;
    };
    let Some(live) = bluez::resolve_device_from_objects(&objs, &ad.path, mac) else {
        return false;
    };
    let Ok(dev) = bluez::device_proxy(&conn, &live.path).await else {
        return false;
    };
    matches!(
        tokio::time::timeout(bluez::CALL_TIMEOUT, dev.trusted()).await,
        Ok(Ok(true))
    )
}

async fn adopt_now(
    explicit: Option<&Path>,
    device: Option<&str>,
    adapter: Option<&str>,
) -> Result<AdoptOutcome, PrintError> {
    let already = adopt::load_any(explicit);
    let hint_mac = device.and_then(adopt::normalize_mac);

    if let (Some(mac), Some(want)) = (already.as_deref(), hint_mac.as_deref()) {
        if mac.eq_ignore_ascii_case(want) && device_is_trusted(adapter, mac).await {
            return Ok(AdoptOutcome::Adopted {
                address: mac.to_string(),
                already: true,
            });
        }
    }

    let conn = bluez::system_bus().await?;
    let objs = bluez::managed_objects(&conn).await?;
    let adapter_info = discovery::choose_adapter(&objs, adapter)?;

    let hint = match (hint_mac.as_deref(), device) {
        (Some(m), _) => Some(discovery::DeviceHint::Address(m.to_string())),
        (None, Some(name)) => Some(discovery::DeviceHint::parse(name)),
        _ => None,
    };

    struct ScanGuard {
        conn: Connection,
        path: String,
    }
    impl Drop for ScanGuard {
        fn drop(&mut self) {
            let conn = self.conn.clone();
            let path = self.path.clone();
            crate::ble::cleanup::spawn(async move { discovery::stop_scan(&conn, &path).await });
        }
    }
    let _scan = ScanGuard {
        conn: conn.clone(),
        path: adapter_info.path.clone(),
    };

    let cand = match discovery::scan(
        &conn,
        &adapter_info,
        hint.as_ref(),
        &[],
        discovery::default_scan_timeout(),
    )
    .await
    {
        Ok(c) => c,
        Err(e) => match outcome_from_connect(&e) {
            Some(o) => return Ok(o),
            None => return Err(e),
        },
    };

    match force_le_trusted(&conn, &adapter_info.path, &cand.address, &cand.path).await {
        Ok(()) => {
            let mac = adopt::persist_visible(explicit, &cand.address).map_err(PrintError::Io)?;
            let already = already
                .as_deref()
                .is_some_and(|a| a.eq_ignore_ascii_case(&mac));
            discovery::stop_scan(&conn, &adapter_info.path).await;
            Ok(AdoptOutcome::Adopted {
                address: mac,
                already,
            })
        }
        Err(e) => match outcome_from_connect(&e) {
            Some(o) => Ok(o),
            None => Err(e),
        },
    }
}

/// Map a connect/scan failure onto the operator-facing adopt outcome.
pub fn outcome_from_connect(e: &PrintError) -> Option<AdoptOutcome> {
    match e {
        PrintError::NotFound => Some(AdoptOutcome::PrinterOff),
        PrintError::NeedExperimental => Some(AdoptOutcome::NeedExperimental),
        _ => None,
    }
}

/// ConnectDevice → Trusted → Disconnect. Same outcome table as the live print path.
async fn force_le_trusted(
    conn: &Connection,
    adapter_path: &str,
    address: &str,
    existing_path: &str,
) -> Result<(), PrintError> {
    let ad = bluez::adapter_proxy(conn, adapter_path).await?;
    for attempt in 0..2 {
        let pairs = bluez::le_connect_device_pairs(address);
        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opts.insert(pairs[0].0, Value::from(pairs[0].1.as_str()));
        opts.insert(pairs[1].0, Value::from(pairs[1].1.as_str()));
        let call = tokio::time::timeout(
            std::time::Duration::from_secs(LIVE_CONNECT_TIMEOUT_S),
            ad.connect_device(opts),
        )
        .await;
        let (ok, name, message, timed_out, created) = match &call {
            Ok(Ok(p)) => (
                true,
                None,
                String::new(),
                false,
                Some(p.as_str().to_string()),
            ),
            Ok(Err(e)) => (
                false,
                bluez::err_name(e),
                bluez::err_message(e),
                false,
                None,
            ),
            Err(_) => (false, None, String::new(), true, None),
        };
        match bluez::connect_device_outcome(ok, name.as_deref(), &message, timed_out) {
            bluez::ConnectDeviceOutcome::CreatedLe => {
                let path = bluez::created_le_path(created);
                return trust_and_release(conn, &path).await;
            }
            bluez::ConnectDeviceOutcome::NeedExperimental => {
                return Err(PrintError::NeedExperimental);
            }
            bluez::ConnectDeviceOutcome::Recreate if attempt == 0 => {
                if let Ok(p) = zbus::zvariant::ObjectPath::try_from(existing_path) {
                    let _ = tokio::time::timeout(bluez::CALL_TIMEOUT, ad.remove_device(&p)).await;
                }
                continue;
            }
            bluez::ConnectDeviceOutcome::ReuseIfConnected
            | bluez::ConnectDeviceOutcome::WaitInProgress => {
                if let Ok(dev) = bluez::device_proxy(conn, existing_path).await {
                    if matches!(
                        tokio::time::timeout(bluez::CALL_TIMEOUT, dev.connected()).await,
                        Ok(Ok(true))
                    ) {
                        return trust_and_release(conn, existing_path).await;
                    }
                }
                return Err(PrintError::NotFound);
            }
            _ => return Err(PrintError::NotFound),
        }
    }
    Err(PrintError::NotFound)
}

async fn trust_and_release(conn: &Connection, path: &str) -> Result<(), PrintError> {
    let dev = bluez::device_proxy(conn, path).await?;
    let now = match tokio::time::timeout(bluez::CALL_TIMEOUT, dev.trusted()).await {
        Ok(Ok(v)) => Some(v),
        _ => None,
    };
    if bluez::should_set_trusted(now) {
        match tokio::time::timeout(bluez::CALL_TIMEOUT, dev.set_trusted(true)).await {
            Ok(Ok(())) => tracing::info!("marked printer Trusted so BlueZ will not prune it"),
            Ok(Err(e)) => tracing::debug!("set Trusted: {}", bluez::err_message(&e)),
            Err(_) => tracing::debug!("set Trusted timed out"),
        }
    }
    let _ = tokio::time::timeout(bluez::CALL_TIMEOUT, dev.disconnect()).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printer_off_is_the_shipped_outcome() {
        let o = outcome_from_connect(&PrintError::NotFound).unwrap();
        assert_eq!(o, AdoptOutcome::PrinterOff);
        assert_ne!(o.exit_code(), 0);
        assert!(o.message().contains("switch it on"));
        assert_eq!(o.message(), crate::adopt::printer_off_message());
        assert_eq!(
            outcome_from_connect(&PrintError::NeedExperimental),
            Some(AdoptOutcome::NeedExperimental)
        );
    }

    #[test]
    fn status_uses_persist_store() {
        let dir = tempfile::tempdir().unwrap();
        let empty = adopt::status_of(dir.path());
        assert_eq!(empty.message(), "not adopted");
        assert_eq!(empty.exit_code(), 1);
        adopt::persist(dir.path(), "AA:BB:CC:DD:EE:FF").unwrap();
        let yes = adopt::status_of(dir.path());
        assert_eq!(yes.message(), "adopted AA:BB:CC:DD:EE:FF");
        assert_eq!(yes.exit_code(), 0);
    }
}
