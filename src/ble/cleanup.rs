//! Cleanup work that must outlive the future that started it: the best-effort `Disconnect` /
//! `StopDiscovery` calls made from `Drop` impls when a connect or print future is dropped
//! (cancel, shutdown, panic). They are spawned through one process-wide `TaskTracker` so
//! `main` can wait for them before the process exits — otherwise `systemctl restart` in the
//! middle of a connect leaves the LE link held, and a cat printer that is connected stops
//! advertising for everyone until it is power-cycled.

use std::future::Future;
use std::sync::LazyLock;
use std::time::Duration;

use tokio_util::task::TaskTracker;

pub static CLEANUP: LazyLock<TaskTracker> = LazyLock::new(TaskTracker::new);

/// Spawn a cleanup future on the current runtime (silently a no-op outside one, e.g. in tests
/// that drop a session after the runtime is gone).
pub fn spawn<F>(fut: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    if let Ok(h) = tokio::runtime::Handle::try_current() {
        CLEANUP.spawn_on(fut, &h);
    }
}

/// Wait (bounded) for outstanding cleanup tasks. Called once, right before the process exits;
/// tasks spawned after `close()` are still tracked and waited for.
pub async fn drain(max: Duration) {
    CLEANUP.close();
    if CLEANUP.is_empty() {
        return;
    }
    tracing::debug!("waiting for {} cleanup task(s)", CLEANUP.len());
    if tokio::time::timeout(max, CLEANUP.wait()).await.is_err() {
        tracing::warn!("Bluetooth cleanup did not finish within {max:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn drain_waits_for_spawned_cleanup() {
        let done = Arc::new(AtomicBool::new(false));
        let d = done.clone();
        spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            d.store(true, Ordering::SeqCst);
        });
        drain(Duration::from_secs(5)).await;
        assert!(done.load(Ordering::SeqCst));
    }
}
