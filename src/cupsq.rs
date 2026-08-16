//! Adopt the CUPS queue's printer-uuid so libcups de-duplicates our DNS-SD entry. Stub for now.

use std::sync::{Arc, RwLock};

use tokio_util::sync::CancellationToken;

pub async fn adopt_uuid_task(
    _queue: String,
    _uuid: Arc<RwLock<String>>,
    _tx: tokio::sync::watch::Sender<String>,
    shutdown: CancellationToken,
) {
    shutdown.cancelled().await;
}
