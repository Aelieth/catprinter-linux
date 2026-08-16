//! DNS-SD advertisement via Avahi's D-Bus API on the loopback interface (the ipp-usb pattern).
//! Filled in fully in M2; this stub keeps the wiring compiling.

use std::sync::{Arc, RwLock};

use tokio_util::sync::CancellationToken;

pub struct DnssdConfig {
    pub name: String,
    pub port: u16,
    pub model_label: String,
    pub location: String,
    pub uuid: tokio::sync::watch::Receiver<String>,
}

pub async fn run(
    _cfg: DnssdConfig,
    status: Arc<RwLock<serde_json::Value>>,
    shutdown: CancellationToken,
) {
    if let Ok(mut s) = status.write() {
        *s = serde_json::json!({"enabled": true, "registered": false, "note": "not implemented yet"});
    }
    shutdown.cancelled().await;
}
