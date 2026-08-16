//! Bluetooth transport over BlueZ's D-Bus API (zbus). Filled in the BLE milestone.
//! Modules: bluez (proxies/events), discovery (find the printer), session (connected GATT link),
//! seqpacket (AcquireWrite/AcquireNotify fds).

use tokio_util::sync::CancellationToken;

use crate::printer::{Condition, PreparedJob, PrintError, PrintReport, Progress};

/// Real printer: discovery → connect → detect model → pack → drive → disconnect, per job.
pub struct BlePrinter {
    pub device_hint: Option<String>,
    pub adapter: Option<String>,
    pub forced_family: Option<crate::models::Family>,
    pub slow: bool,
    pub pacing_ms: u64,
}

impl BlePrinter {
    pub async fn print(
        &mut self,
        _job: &PreparedJob,
        _cancel: &CancellationToken,
        _progress: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<PrintReport, PrintError> {
        Err(PrintError::Bus("BLE transport not implemented yet".into()))
    }
    pub async fn status(&mut self, _cancel: &CancellationToken) -> Result<Condition, PrintError> {
        Err(PrintError::Bus("BLE transport not implemented yet".into()))
    }
    pub async fn identify(&mut self, _cancel: &CancellationToken) -> Result<(), PrintError> {
        Err(PrintError::Bus("BLE transport not implemented yet".into()))
    }
}

/// Periodically report adapter state into /health (stub until the BLE milestone).
pub async fn adapter_probe_task(
    _adapter: Option<String>,
    _extra: std::sync::Arc<std::sync::RwLock<serde_json::Value>>,
    shutdown: CancellationToken,
) {
    shutdown.cancelled().await;
}
