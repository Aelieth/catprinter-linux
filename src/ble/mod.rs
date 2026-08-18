//! Bluetooth transport over BlueZ's D-Bus API (zbus).
//!
//! Per print job: find the printer (cache-first, then LE scan), connect, detect the model, pack the
//! strip for that model, drive the family sequence for each copy, then always disconnect.

pub mod bluez;
pub mod cleanup;
pub mod discovery;
pub mod host;
pub mod seqpacket;
pub mod session;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::protocol::mxw01::CONNECT_ATTEMPTS;
use zbus::Connection;

use crate::ble::discovery::{Candidate, DeviceHint};
use crate::ble::session::Session;
use crate::config::NotifyMode;
use crate::models::Family;
use crate::printer::{Condition, Phase, PreparedJob, PrintError, PrintReport, Progress};
use crate::render;

/// How long a mis-picked device (connected fine, but no cat-printer GATT) stays excluded from
/// autodetection. Long enough that a neighbour's gadget cannot eat every retry of a job, short
/// enough that a device that really becomes a printer (re-pairing, firmware) comes back.
const AVOID_TTL: Duration = Duration::from_secs(600);

/// Extra scan+connect rounds one `open()` may spend skipping mis-picked devices before giving
/// the attempt back to the engine as (retryable) NotFound.
const MISPICK_EXTRA_TRIES: usize = 2;

/// Addresses that recently turned out not to be cat printers. Process-wide rather than a
/// `BlePrinter` field because the daemon drives exactly one printer per process and the
/// `BlePrinter` construction sites are frozen; the TTL keeps entries from going stale.
static AVOID: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(Mutex::default);

/// Non-expired avoided addresses (pruning expired ones as a side effect).
fn avoided_addresses() -> Vec<String> {
    let mut m = AVOID.lock().unwrap_or_else(|e| e.into_inner());
    m.retain(|_, t| t.elapsed() < AVOID_TTL);
    m.keys().cloned().collect()
}

fn note_not_cat_printer(address: &str) {
    let mut m = AVOID.lock().unwrap_or_else(|e| e.into_inner());
    m.retain(|_, t| t.elapsed() < AVOID_TTL);
    m.insert(address.to_ascii_uppercase(), Instant::now());
}

/// Stops a scan `open()` started if its future is dropped (cancel / shutdown) before the normal
/// stop runs, via the cleanup tracker so main's drain() waits for it.
struct ScanGuard {
    conn: Connection,
    path: String,
    armed: bool,
}

impl Drop for ScanGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let conn = self.conn.clone();
        let path = self.path.clone();
        crate::ble::cleanup::spawn(async move { discovery::stop_scan(&conn, &path).await });
    }
}

/// Real printer: discovery → connect → detect model → pack → drive → disconnect, per job.
pub struct BlePrinter {
    pub device_hint: Option<String>,
    pub adapter: Option<String>,
    pub forced_family: Option<Family>,
    pub slow: bool,
    pub pacing_ms: u64,
    pub notify_mode: NotifyMode,
    /// When set, the first successful live connect records the MAC here (auto-adopt).
    pub state_dir: Option<PathBuf>,
}

impl BlePrinter {
    fn hint(&self) -> Option<DeviceHint> {
        self.device_hint.as_deref().map(DeviceHint::parse)
    }

    fn record_adopted(&self, address: &str) {
        let Some(dir) = &self.state_dir else {
            return;
        };
        match crate::adopt::persist_if_empty(dir, address) {
            Ok(Some(mac)) => tracing::info!(%mac, "recorded adopted printer"),
            Ok(None) => {}
            Err(e) => tracing::debug!("could not record adopted printer: {e}"),
        }
    }

    async fn connect_to(
        &self,
        conn: &Connection,
        target: &Candidate,
        was_connected: bool,
        attempts: u8,
    ) -> Result<Session, PrintError> {
        Session::connect(
            conn,
            target,
            was_connected,
            attempts,
            self.forced_family,
            self.notify_mode,
            self.pacing_ms,
            self.slow,
        )
        .await
    }

    /// Find and connect to the printer (cache first, then scan). A device that connects but has
    /// no cat-printer GATT is remembered and skipped instead of failing the job outright —
    /// unless the user pinned it with `--device`.
    async fn open(
        &self,
        conn: &Connection,
        progress: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<Session, PrintError> {
        let hint = self.hint();
        let objs = bluez::managed_objects(conn).await?;
        let adapter = match discovery::choose_adapter(&objs, self.adapter.as_deref()) {
            Ok(a) => a,
            Err(PrintError::AdapterOff) => {
                // Combo USB reset / rfkill leftover: Adapter1 exists but Powered=false,
                // or the object is mid-re-enumerate. Try Set Powered, then re-choose.
                let path = discovery::adapters_from(&objs)
                    .into_iter()
                    .next()
                    .map(|a| a.path)
                    .unwrap_or_else(|| "/org/bluez/hci0".into());
                if bluez::recover_adapter(conn, &path).await {
                    let objs = bluez::managed_objects(conn).await?;
                    discovery::choose_adapter(&objs, self.adapter.as_deref())?
                } else {
                    return Err(PrintError::AdapterOff);
                }
            }
            Err(e) => return Err(e),
        };

        // Always stop discovery when this open() ends. ensure_le_discovery on
        // retry otherwise leaves Discovering=true with an empty queue.
        let mut scan_guard = ScanGuard {
            conn: conn.clone(),
            path: adapter.path.clone(),
            armed: true,
        };

        progress(Progress {
            phase: Phase::Searching,
            percent: 0,
            message: "Looking for the cat printer".into(),
        });

        // An explicit --device is never avoided; autodetection skips known non-printers.
        let avoid = if hint.is_some() {
            Vec::new()
        } else {
            avoided_addresses()
        };

        // Cache-first — but only when the device is actually present (Settings-held Connected,
        // or advertising with an RSSI). A merely-known cache entry costs a doomed 12 s connect
        // per round whenever the printer is simply off.
        let cached = discovery::candidates_from(&objs, &adapter.path, hint.as_ref(), &avoid);
        if let Some(best) = discovery::pick(&cached).filter(|c| c.connected || c.rssi.is_some()) {
            let was_connected = best.connected;
            tracing::info!(
                "found {} in the Bluetooth cache ({})",
                best.label(),
                if was_connected {
                    "connected"
                } else {
                    "advertising"
                }
            );
            progress(Progress {
                phase: Phase::Connecting,
                percent: 0,
                message: "Connecting".into(),
            });
            // Live cache (RSSI or Connected): full retries. We no longer Connect
            // on a silent cache, so a failed advertising connect is not "maybe
            // stale — scan 8 s"; scanning the same printer just burns the kid clock.
            match self
                .connect_to(conn, best, was_connected, CONNECT_ATTEMPTS)
                .await
            {
                Ok(s) => {
                    self.record_adopted(&best.address);
                    scan_guard.armed = false;
                    discovery::stop_scan(conn, &adapter.path).await;
                    return Ok(s);
                }
                Err(PrintError::NotCatPrinter(why)) if hint.is_none() => {
                    tracing::warn!(
                        "{} connected but is not a cat printer ({why}); avoiding it for a while",
                        best.label()
                    );
                    note_not_cat_printer(&best.address);
                }
                Err(e) => {
                    scan_guard.armed = false;
                    discovery::stop_scan(conn, &adapter.path).await;
                    return Err(e);
                }
            }
        }

        // Scan. Left running while connecting on purpose (stopping first makes BlueZ
        // page-timeout on these toys); always stopped on the way out, drop included.
        progress(Progress {
            phase: Phase::Searching,
            percent: 0,
            message: "Scanning for the cat printer".into(),
        });
        let mut result: Result<Session, PrintError> = Err(PrintError::NotFound);
        for _ in 0..=MISPICK_EXTRA_TRIES {
            let avoid = if hint.is_some() {
                Vec::new()
            } else {
                avoided_addresses()
            };
            let target: Candidate = match discovery::scan(
                conn,
                &adapter,
                hint.as_ref(),
                &avoid,
                discovery::default_scan_timeout(),
            )
            .await
            {
                Ok(t) => t,
                Err(e) => {
                    result = Err(e);
                    break;
                }
            };
            progress(Progress {
                phase: Phase::Connecting,
                percent: 0,
                message: "Connecting".into(),
            });
            match self
                .connect_to(conn, &target, false, CONNECT_ATTEMPTS)
                .await
            {
                Err(PrintError::NotCatPrinter(why)) if hint.is_none() => {
                    tracing::warn!(
                        "{} connected but is not a cat printer ({why}); trying the next device",
                        target.label()
                    );
                    note_not_cat_printer(&target.address);
                    // Nothing else in range → NotFound, which the engine retries.
                    result = Err(PrintError::NotFound);
                }
                Ok(s) => {
                    self.record_adopted(&target.address);
                    result = Ok(s);
                    break;
                }
                r => {
                    result = r;
                    break;
                }
            }
        }
        scan_guard.armed = false;
        discovery::stop_scan(conn, &adapter.path).await;
        result
    }

    pub async fn print(
        &mut self,
        job: &PreparedJob,
        cancel: &CancellationToken,
        progress: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<PrintReport, PrintError> {
        let started = Instant::now();
        let conn = bluez::system_bus().await?;
        // Observe cancellation during scan/connect too: the dropped open() future releases its
        // half-open link and scan through the Drop guards (routed via cleanup::drain).
        let session = tokio::select! {
            s = self.open(&conn, progress) => s?,
            _ = cancel.cancelled() => return Err(PrintError::Cancelled),
        };
        // The drivers poll `cancel` between writes; the select is the net under a stuck bus call.
        let result = tokio::select! {
            r = self.drive(&session, job, cancel, progress) => r,
            _ = cancel.cancelled() => Err(PrintError::Cancelled),
        };
        // ALWAYS release the link — success, error and cancel alike (bounded; see Session::close).
        session.close().await;
        let (confirmed, battery, model, family, mtu, lines, segments) = result?;
        Ok(PrintReport {
            model,
            family: Some(family),
            lines,
            segments,
            copies: job.copies.max(1),
            complete_confirmed: confirmed,
            mtu: Some(mtu),
            battery,
            elapsed: started.elapsed(),
        })
    }

    #[allow(clippy::type_complexity)]
    async fn drive(
        &self,
        session: &Session,
        job: &PreparedJob,
        cancel: &CancellationToken,
        progress: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<(bool, Option<u8>, String, Family, u16, u32, u32), PrintError> {
        progress(Progress {
            phase: Phase::Preparing,
            percent: 0,
            message: "Preparing image".into(),
        });
        // caps.grayscale_4bpp is already downgraded by the session when the negotiated MTU
        // cannot carry 192-byte 4bpp rows.
        let mode = render::mode_for(job.opts.tone, session.caps.grayscale_4bpp);
        // Only the MXW01 protocol has the ≥90-line minimum; padding a classic print would
        // burn up to 89 blank rows before the trailing feed.
        let packed = render::pack_padded(
            &job.strip,
            &job.opts,
            mode,
            session.caps.width_px,
            session.family == Family::Mxw01,
        )?;
        let intensity = job.opts.intensity();
        let copies = job.copies.max(1);
        let mut confirmed = true;
        let mut battery = None;
        for c in 0..copies {
            if cancel.is_cancelled() {
                return Err(PrintError::Cancelled);
            }
            if c > 0 && session.family == Family::Mxw01 {
                // Let the head finish the previous copy before the next A9.
                if let Err(e) = crate::models::mxw01::wait_standby(session).await {
                    return Err(crate::models::mxw01::interrupted(e, packed.lines * c, true));
                }
            }
            if copies > 1 {
                progress(Progress {
                    phase: Phase::Printing,
                    percent: (c * 100 / copies) as u8,
                    message: format!("Copy {}/{}", c + 1, copies),
                });
            }
            let (ok, batt) = match session.family {
                Family::Mxw01 => {
                    crate::models::mxw01::print(session, &packed, intensity, cancel, progress)
                        .await?
                }
                Family::Classic => {
                    crate::models::classic::print(
                        session,
                        &packed,
                        job.opts.intensity_override,
                        cancel,
                        progress,
                    )
                    .await?
                }
            };
            confirmed &= ok;
            battery = batt.or(battery);
        }
        Ok((
            confirmed,
            battery,
            session.model_label.clone(),
            session.family,
            session.mtu(),
            packed.lines,
            packed.segments.len() as u32,
        ))
    }

    pub async fn status(&mut self, cancel: &CancellationToken) -> Result<Condition, PrintError> {
        let conn = bluez::system_bus().await?;
        let mut noop = |_: Progress| {};
        let session = tokio::select! {
            s = self.open(&conn, &mut noop) => s?,
            _ = cancel.cancelled() => return Err(PrintError::Cancelled),
        };
        let query = async {
            match session.family {
                Family::Mxw01 => crate::models::mxw01::status(&session).await,
                Family::Classic => crate::models::classic::status(&session).await,
            }
        };
        let result = tokio::select! {
            r = query => r,
            _ = cancel.cancelled() => Err(PrintError::Cancelled),
        };
        session.close().await;
        result
    }

    pub async fn identify(&mut self, cancel: &CancellationToken) -> Result<(), PrintError> {
        let conn = bluez::system_bus().await?;
        let mut noop = |_: Progress| {};
        let session = tokio::select! {
            s = self.open(&conn, &mut noop) => s?,
            _ = cancel.cancelled() => return Err(PrintError::Cancelled),
        };
        let action = async {
            match session.family {
                Family::Mxw01 => crate::models::mxw01::identify(&session).await,
                Family::Classic => crate::models::classic::identify(&session).await,
            }
        };
        let result = tokio::select! {
            r = action => r,
            _ = cancel.cancelled() => Err(PrintError::Cancelled),
        };
        session.close().await;
        result
    }
}

/// Periodically report adapter state into /health (bluetoothd reachable, adapter, powered).
pub async fn adapter_probe_task(
    adapter: Option<String>,
    extra: Arc<RwLock<serde_json::Value>>,
    shutdown: CancellationToken,
) {
    let mut iv = tokio::time::interval(Duration::from_secs(30));
    loop {
        tokio::select! {
            _ = iv.tick() => {},
            _ = shutdown.cancelled() => break,
        }
        let bt = probe(adapter.as_deref()).await;
        if let Ok(mut v) = extra.write() {
            if let serde_json::Value::Object(map) = &mut *v {
                map.insert("bluetooth".into(), bt);
            } else {
                *v = serde_json::json!({ "bluetooth": bt });
            }
        }
    }
}

async fn probe(want: Option<&str>) -> serde_json::Value {
    let conn = match tokio::time::timeout(Duration::from_secs(2), Connection::system()).await {
        Ok(Ok(c)) => c,
        _ => return serde_json::json!({ "bluetoothd": false }),
    };
    let objs = match bluez::managed_objects(&conn).await {
        Ok(o) => o,
        Err(_) => {
            return serde_json::json!({ "bluetoothd": true, "adapter": serde_json::Value::Null })
        }
    };
    match discovery::choose_adapter(&objs, want) {
        Ok(a) => {
            serde_json::json!({ "bluetoothd": true, "adapter": a.path, "powered": a.powered, "address": a.address, "name": a.name })
        }
        Err(PrintError::AdapterOff) => {
            let any = discovery::adapters_from(&objs).into_iter().next();
            serde_json::json!({ "bluetoothd": true, "adapter": any.as_ref().map(|a| a.path.clone()), "powered": false })
        }
        Err(_) => serde_json::json!({ "bluetoothd": true, "adapter": serde_json::Value::Null }),
    }
}
