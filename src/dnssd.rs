//! DNS-SD advertisement (`_ipp._tcp`) via Avahi's D-Bus API on the loopback interface — the same
//! trick ipp-usb uses, so `ippfind`, `driverless list`, GNOME/KDE "add printer" and CUPS 3 local
//! discovery see the printer without exposing the port on the LAN. Never fatal: if Avahi is absent
//! we retry every minute.
#![allow(clippy::too_many_arguments)] // Avahi AddService has 9 parameters

use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use zbus::zvariant::OwnedObjectPath;

pub struct DnssdConfig {
    pub name: String,
    pub port: u16,
    pub model_label: String,
    pub location: String,
    pub uuid: tokio::sync::watch::Receiver<String>,
}

const AVAHI_PROTO_INET: i32 = 0;
const SERVER_RUNNING: i32 = 2;
const GROUP_ESTABLISHED: i32 = 2;
const GROUP_COLLISION: i32 = 3;
const GROUP_FAILURE: i32 = 4;
const RETRY: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_secs(30);

#[zbus::proxy(
    interface = "org.freedesktop.Avahi.Server",
    default_service = "org.freedesktop.Avahi",
    default_path = "/"
)]
trait AvahiServer {
    fn get_state(&self) -> zbus::Result<i32>;
    fn get_version_string(&self) -> zbus::Result<String>;
    fn get_network_interface_index_by_name(&self, name: &str) -> zbus::Result<i32>;
    fn entry_group_new(&self) -> zbus::Result<OwnedObjectPath>;
    fn get_alternative_service_name(&self, name: &str) -> zbus::Result<String>;
}

#[zbus::proxy(
    interface = "org.freedesktop.Avahi.EntryGroup",
    default_service = "org.freedesktop.Avahi"
)]
trait AvahiEntryGroup {
    #[allow(clippy::too_many_arguments)]
    fn add_service(
        &self,
        interface: i32,
        protocol: i32,
        flags: u32,
        name: &str,
        r#type: &str,
        domain: &str,
        host: &str,
        port: u16,
        txt: Vec<Vec<u8>>,
    ) -> zbus::Result<()>;
    fn add_service_subtype(
        &self,
        interface: i32,
        protocol: i32,
        flags: u32,
        name: &str,
        r#type: &str,
        domain: &str,
        subtype: &str,
    ) -> zbus::Result<()>;
    fn update_service_txt(
        &self,
        interface: i32,
        protocol: i32,
        flags: u32,
        name: &str,
        r#type: &str,
        domain: &str,
        txt: Vec<Vec<u8>>,
    ) -> zbus::Result<()>;
    fn commit(&self) -> zbus::Result<()>;
    fn reset(&self) -> zbus::Result<()>;
    fn free(&self) -> zbus::Result<()>;
    fn get_state(&self) -> zbus::Result<i32>;
}

/// TXT record set (Bonjour Printing 1.2.1 / PWG 5100.14). Never a `printer-type` key.
pub fn txt_records(cfg: &DnssdConfig, uuid: &str) -> Vec<Vec<u8>> {
    let uuid_bare = uuid.trim_start_matches("urn:uuid:");
    let kv = [
        ("txtvers", "1".to_string()),
        ("qtotal", "1".to_string()),
        ("rp", "ipp/print".to_string()),
        ("ty", format!("Cat Printer {}", cfg.model_label)),
        ("product", format!("({})", cfg.model_label)),
        ("usb_MFG", "CatPrinter".to_string()),
        ("usb_MDL", cfg.model_label.clone()),
        ("usb_CMD", "PWGRaster".to_string()),
        ("pdl", "image/pwg-raster,image/png,image/jpeg".to_string()),
        ("Color", "F".to_string()),
        ("Duplex", "F".to_string()),
        ("Copies", "T".to_string()),
        ("Fax", "F".to_string()),
        ("Scan", "F".to_string()),
        ("kind", "roll,label".to_string()),
        ("note", cfg.location.clone()),
        ("adminurl", format!("http://127.0.0.1:{}/", cfg.port)),
        ("UUID", uuid_bare.to_string()),
        ("priority", "50".to_string()),
    ];
    kv.iter()
        .map(|(k, v)| format!("{k}={v}").into_bytes())
        .collect()
}

fn set_status(status: &Arc<RwLock<serde_json::Value>>, v: serde_json::Value) {
    if let Ok(mut s) = status.write() {
        *s = v;
    }
}

pub async fn run(
    mut cfg: DnssdConfig,
    status: Arc<RwLock<serde_json::Value>>,
    shutdown: CancellationToken,
) {
    let mut logged_absent = false;
    loop {
        if shutdown.is_cancelled() {
            return;
        }
        match register_and_watch(&mut cfg, &status, &shutdown, &mut logged_absent).await {
            Ok(()) => return, // shutdown
            Err(e) => {
                if !logged_absent {
                    tracing::info!(
                        "dnssd: not advertising ({e}); retrying every {}s",
                        RETRY.as_secs()
                    );
                    logged_absent = true;
                }
                set_status(
                    &status,
                    serde_json::json!({"enabled": true, "registered": false, "error": e.to_string()}),
                );
                tokio::select! {
                    _ = tokio::time::sleep(RETRY) => {},
                    _ = shutdown.cancelled() => return,
                }
            }
        }
    }
}

async fn register_and_watch(
    cfg: &mut DnssdConfig,
    status: &Arc<RwLock<serde_json::Value>>,
    shutdown: &CancellationToken,
    logged_absent: &mut bool,
) -> anyhow::Result<()> {
    let conn = tokio::time::timeout(Duration::from_secs(5), zbus::Connection::system()).await??;
    let server = AvahiServerProxy::new(&conn).await?;
    // At boot Avahi may still be registering its host name; follow it up for a while.
    let mut state = tokio::time::timeout(Duration::from_secs(5), server.get_state()).await??;
    let mut waited = 0u32;
    while state != SERVER_RUNNING && waited < 60 {
        if shutdown.is_cancelled() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        waited += 2;
        state = tokio::time::timeout(Duration::from_secs(5), server.get_state()).await??;
    }
    if state != SERVER_RUNNING {
        anyhow::bail!("avahi server state {state}");
    }
    let ifindex = tokio::time::timeout(
        Duration::from_secs(5),
        server.get_network_interface_index_by_name("lo"),
    )
    .await?
    .unwrap_or(1);
    let group_path =
        tokio::time::timeout(Duration::from_secs(5), server.entry_group_new()).await??;
    let group = AvahiEntryGroupProxy::builder(&conn)
        .path(group_path.clone())?
        .build()
        .await?;
    let mut name = cfg.name.clone();
    let mut uuid = cfg.uuid.borrow().clone();

    // Register (with collision handling).
    let mut attempts = 0;
    loop {
        attempts += 1;
        let txt = txt_records(cfg, &uuid);
        let add = async {
            group
                .add_service(
                    ifindex,
                    AVAHI_PROTO_INET,
                    0,
                    &name,
                    "_ipp._tcp",
                    "",
                    "",
                    cfg.port,
                    txt,
                )
                .await?;
            let _ = group
                .add_service_subtype(
                    ifindex,
                    AVAHI_PROTO_INET,
                    0,
                    &name,
                    "_ipp._tcp",
                    "",
                    "_print._sub._ipp._tcp",
                )
                .await;
            let _ = group
                .add_service_subtype(
                    ifindex,
                    AVAHI_PROTO_INET,
                    0,
                    &name,
                    "_ipp._tcp",
                    "",
                    "_universal._sub._ipp._tcp",
                )
                .await;
            group.commit().await
        };
        match tokio::time::timeout(Duration::from_secs(10), add).await? {
            Ok(()) => {}
            Err(e) => {
                let s = e.to_string();
                if s.contains("Collision") || s.contains("collision") {
                    if attempts > 5 {
                        anyhow::bail!("name collision persists");
                    }
                    let alt = server
                        .get_alternative_service_name(&name)
                        .await
                        .unwrap_or_else(|_| format!("{name} ({attempts})"));
                    tracing::info!("dnssd: name '{name}' in use, trying '{alt}'");
                    name = alt;
                    let _ = group.reset().await;
                    continue;
                }
                return Err(e.into());
            }
        }
        // wait for established (up to ~5 s)
        let mut ok = false;
        for _ in 0..25 {
            match tokio::time::timeout(Duration::from_secs(2), group.get_state()).await? {
                Ok(GROUP_ESTABLISHED) => {
                    ok = true;
                    break;
                }
                Ok(GROUP_COLLISION) => break,
                Ok(GROUP_FAILURE) => anyhow::bail!("avahi entry group failed"),
                _ => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        }
        if ok {
            break;
        }
        // collision reported asynchronously
        if attempts > 5 {
            anyhow::bail!("name collision persists");
        }
        let alt = server
            .get_alternative_service_name(&name)
            .await
            .unwrap_or_else(|_| format!("{name} ({attempts})"));
        tracing::info!("dnssd: name '{name}' collided, trying '{alt}'");
        name = alt;
        let _ = group.reset().await;
    }
    tracing::info!(
        "dnssd: advertising '{name}' (_ipp._tcp on lo, port {}) via Avahi {}",
        cfg.port,
        server.get_version_string().await.unwrap_or_default()
    );
    *logged_absent = false;
    set_status(
        status,
        serde_json::json!({"enabled": true, "registered": true, "name": name, "interface": "lo", "uuid": uuid}),
    );

    // Watch: uuid changes → update TXT; periodic health poll; shutdown → Free.
    let mut poll = tokio::time::interval(POLL);
    poll.tick().await;
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                let _ = tokio::time::timeout(Duration::from_secs(3), group.free()).await;
                set_status(status, serde_json::json!({"enabled": true, "registered": false, "note": "shutdown"}));
                return Ok(());
            }
            changed = cfg.uuid.changed() => {
                if changed.is_err() { continue; }
                let new = cfg.uuid.borrow().clone();
                if new != uuid {
                    uuid = new;
                    let txt = txt_records(cfg, &uuid);
                    match tokio::time::timeout(Duration::from_secs(5), group.update_service_txt(ifindex, AVAHI_PROTO_INET, 0, &name, "_ipp._tcp", "", txt)).await {
                        Ok(Ok(())) => {
                            tracing::info!("dnssd: TXT UUID updated to {uuid}");
                            set_status(status, serde_json::json!({"enabled": true, "registered": true, "name": name, "interface": "lo", "uuid": uuid}));
                        }
                        other => anyhow::bail!("update TXT failed: {other:?}"),
                    }
                }
            }
            _ = poll.tick() => {
                match tokio::time::timeout(Duration::from_secs(5), group.get_state()).await {
                    Ok(Ok(GROUP_ESTABLISHED)) => {}
                    other => anyhow::bail!("entry group lost ({other:?}) — re-registering"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn txt_has_required_keys_and_no_printer_type() {
        let (_tx, rx) = tokio::sync::watch::channel("urn:uuid:abc".to_string());
        let cfg = DnssdConfig {
            name: "Cat Printer".into(),
            port: 8095,
            model_label: "MXW01".into(),
            location: "here".into(),
            uuid: rx,
        };
        let txt: Vec<String> = txt_records(&cfg, "urn:uuid:abc")
            .into_iter()
            .map(|b| String::from_utf8(b).unwrap())
            .collect();
        for k in [
            "txtvers=1",
            "rp=ipp/print",
            "pdl=image/pwg-raster,image/png,image/jpeg",
            "UUID=abc",
            "kind=roll,label",
            "adminurl=http://127.0.0.1:8095/",
        ] {
            assert!(txt.iter().any(|t| t == k), "missing {k}");
        }
        assert!(!txt.iter().any(|t| t.starts_with("printer-type")));
    }
}
