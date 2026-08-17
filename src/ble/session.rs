//! A connected GATT session: connect with retries, resolve characteristics, detect the model,
//! arm notifications *before* any write, acquire a real ATT MTU, stream bulk data, and always
//! disconnect on the way out (RAII). Ported from catprinter/ble.py `MXW01`.

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::broadcast;
use zbus::zvariant::Value;
use zbus::Connection;

use crate::ble::bluez::{self, ConnectFailureKind, Device1Proxy, GattCharacteristic1Proxy};
use crate::ble::discovery::{self, Candidate};
use crate::ble::seqpacket::SeqPacket;
use crate::config::NotifyMode;
use crate::models::{self, Caps, Family};
use crate::printer::PrintError;
use crate::protocol::mxw01::{CONNECT_ATTEMPTS, LIVE_CONNECT_TIMEOUT_S};
use crate::protocol::{CONTROL_UUID, DATA_UUID, NOTIFY_UUID, SERVICE_UUIDS};

const SERVICES_RESOLVED_TIMEOUT: Duration = Duration::from_secs(6);
const CONNECT_RETRY_PAUSE: Duration = Duration::from_millis(600);
const HOST_ABORT_PAUSE: Duration = Duration::from_millis(2000);
const PRUNE_PAUSE: Duration = Duration::from_millis(400);
const ADAPTER_OFF_PAUSE: Duration = Duration::from_millis(3000);
/// stop_notify + Disconnect: 5 s each made close() longer than the print.
const CLOSE_CALL: Duration = Duration::from_secs(2);
const POST_DISCONNECT_SLEEP: Duration = Duration::from_millis(400);
/// Cheap toys drop notifications if the buffer is tiny; 256 is plenty for our command traffic.
const NOTIFY_CHANNEL: usize = 256;

/// How bulk image data leaves the host.
enum DataPath {
    /// AcquireWrite SOCK_SEQPACKET fd (preferred — real MTU).
    Acquired(SeqPacket),
    /// Fallback: WriteValue with {"type": "command"} on the data characteristic.
    WriteValue {
        chr: GattCharacteristic1Proxy<'static>,
        mtu: u16,
    },
}

impl DataPath {
    fn mtu(&self) -> u16 {
        match self {
            DataPath::Acquired(s) => s.mtu,
            DataPath::WriteValue { mtu, .. } => *mtu,
        }
    }
    fn max_payload(&self) -> usize {
        (self.mtu() as usize).saturating_sub(3).max(1)
    }
}

pub struct Session {
    conn: Connection,
    device_path: String,
    dev: Device1Proxy<'static>,
    control: GattCharacteristic1Proxy<'static>,
    notify_chr: GattCharacteristic1Proxy<'static>,
    data: DataPath,
    /// WriteValue `type` option for control frames and the WriteValue data fallback:
    /// `"command"` (write-without-response, MXW01) or `"request"` (classic AE01 that only
    /// advertises `write`).
    write_type: &'static str,
    tx: broadcast::Sender<Vec<u8>>,
    /// Kept alive so the forwarding task keeps running.
    _notify_task: NotifyTask,
    pub family: Family,
    pub caps: Caps,
    pub model_label: String,
    pub pacing_ms: u64,
    pub slow: bool,
    closed: bool,
}

struct NotifyTask(tokio::task::JoinHandle<()>);
impl Drop for NotifyTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Session {
    pub fn mtu(&self) -> u16 {
        self.data.mtu()
    }

    /// Connect to a discovered candidate and bind everything. `forced` overrides model detection.
    /// `attempts`: 3 when the device is advertising or Settings-held. Silent cache entries are
    /// never Connected (they wait for an advert instead of a 12 s page).
    ///
    /// Every attempt re-resolves a live Device1 path by address. Discovery-time paths are never
    /// reused after TemporaryTimeout may have deleted the object.
    #[allow(clippy::too_many_arguments)]
    pub async fn connect(
        conn: &Connection,
        target: &Candidate,
        was_connected: bool,
        attempts: u8,
        forced: Option<Family>,
        notify_mode: NotifyMode,
        pacing_ms: u64,
        slow: bool,
    ) -> Result<Session, PrintError> {
        let attempts = attempts.max(1);
        let adapter_path = bluez::adapter_from_device_path(&target.path)
            .ok_or_else(|| PrintError::Bus("malformed device path".into()))?
            .to_string();

        let advert_wait =
            crate::ble::host::advert_wait_for(crate::ble::host::collect_default().chip);

        let mut last = String::new();
        let mut last_rssi = target.rssi;
        let mut ready_budget = advert_wait;
        let mut connect_floor = Duration::ZERO;

        for attempt in 1..=attempts {
            // TemporaryTimeout=0 leaves unpaired Device1 objects forever. Connecting
            // while RSSI is unset pages until CONNECT_TIMEOUT_S — the dominant miss
            // on RTL8822CE / MT7925 combo cards. Wait for a live advert or a held ACL.
            // After a failed Connect the kind-specific pause overlaps this wait so
            // host-abort + advert is 2 s, not 2.5 + 5.
            let wait_started = tokio::time::Instant::now();
            let live = match bluez::wait_until_ready_to_connect(
                conn,
                &adapter_path,
                &target.address,
                ready_budget.max(connect_floor),
            )
            .await?
            {
                Some(d) if bluez::connect_readiness(Some(&d)) == bluez::ConnectReadiness::Ready => {
                    d
                }
                maybe => {
                    let (kind, detail, path) = match &maybe {
                        Some(d) => (
                            ConnectFailureKind::NotAdvertising,
                            "Device1 exists but is not advertising",
                            d.path.clone(),
                        ),
                        None => (
                            ConnectFailureKind::Pruned,
                            "device object gone",
                            bluez::device_path_for(&adapter_path, &target.address),
                        ),
                    };
                    last = kind.format_last(detail);
                    log_connect_fail(target, last_rssi, attempt, attempts, &path, kind, detail);
                    match apply_connect_next(
                        conn,
                        &adapter_path,
                        connect_next_action(kind, attempt, attempts),
                    )
                    .await
                    {
                        Ok(Some(pause)) => {
                            ready_budget = retry_ready_budget(pause, advert_wait);
                            connect_floor = pause;
                            continue;
                        }
                        Ok(None) => break,
                        Err(e) => return Err(e),
                    }
                }
            };

            let leftover = leftover_floor(wait_started.elapsed(), connect_floor);
            if !leftover.is_zero() {
                tokio::time::sleep(leftover).await;
            }

            last_rssi = live.rssi.or(target.rssi);
            tracing::info!(
                address = %target.address,
                rssi = ?last_rssi,
                attempt,
                attempts,
                path = %live.path,
                "connect attempt"
            );

            let dev = bluez::device_proxy(conn, &live.path).await?;
            let mut guard = ConnectGuard {
                conn: conn.clone(),
                path: live.path.clone(),
                armed: true,
            };

            match try_one_connect(&dev).await {
                Ok(()) => {
                    let session = bind_session(
                        conn,
                        &live.path,
                        dev,
                        target,
                        forced,
                        notify_mode,
                        pacing_ms,
                        slow,
                    )
                    .await?;
                    guard.armed = false;
                    return Ok(session);
                }
                Err(one) => {
                    last = one.kind.format_last(&one.detail);
                    log_connect_fail(
                        target,
                        last_rssi,
                        attempt,
                        attempts,
                        &live.path,
                        one.kind,
                        &one.detail,
                    );
                    // Do not Disconnect again: host-abort already tore the HCI link
                    // (HCI_ERROR_LOCAL_HOST_TERM); a second Disconnect / StopDiscovery
                    // is what btusb_rtl_reset / btusb_qca_reset treat as cmd-timeout
                    // and USB-resets — adapter goes offline. Timeout already cancelled
                    // once inside try_one_connect.
                    guard.armed = false;
                    drop(dev);
                    drop(guard);
                    match apply_connect_next(
                        conn,
                        &adapter_path,
                        connect_next_action(one.kind, attempt, attempts),
                    )
                    .await
                    {
                        Ok(Some(pause)) => {
                            ready_budget = retry_ready_budget(pause, advert_wait);
                            connect_floor = pause;
                        }
                        Ok(None) => break,
                        Err(e) => return Err(e),
                    }
                }
            }
        }

        let weak = last_rssi.is_some_and(|r| r < -80);
        let mut hint = String::new();
        if weak {
            hint.push_str(" It is far away (weak Bluetooth) — move it next to the computer.");
        }
        if was_connected {
            hint.push_str(
                " Bluetooth Settings may be holding the printer; turn the printer off and on.",
            );
        } else {
            hint.push_str(
                " Close the phone app if it is open — the printer allows only one connection.",
            );
        }
        Err(PrintError::ConnectFailed {
            attempts,
            last,
            hint,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.tx.subscribe()
    }

    /// Write a control frame (small) to the control characteristic.
    pub async fn write_ctrl(&self, packet: &[u8]) -> Result<(), PrintError> {
        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opts.insert("type", Value::from(self.write_type));
        bluez::call(
            "writing to the printer",
            bluez::CALL_TIMEOUT,
            self.control.write_value(packet, opts),
        )
        .await
    }

    /// Write one already-chunked piece of bulk data (no pacing, no re-chunking).
    pub async fn write_data(&self, piece: &[u8]) -> Result<(), PrintError> {
        match &self.data {
            DataPath::Acquired(sock) => loop {
                match sock.send(piece).await {
                    Ok(()) => return Ok(()),
                    // EINTR/EAGAIN: transient, write the same packet again.
                    Err(e) if classify_io(&e) == IoAction::Retry => continue,
                    // Anything else on the acquired socket means the LE link is gone —
                    // retryable, unlike a generic (non-retryable) I/O error.
                    Err(e) => {
                        tracing::debug!("acquired-socket write failed: {e}");
                        return Err(PrintError::LinkLost);
                    }
                }
            },
            DataPath::WriteValue { chr, .. } => {
                let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
                opts.insert("type", Value::from(self.write_type));
                bluez::call(
                    "sending image data",
                    bluez::CALL_TIMEOUT,
                    chr.write_value(piece, opts),
                )
                .await
            }
        }
    }

    /// Stream bulk data in whole-row chunks, honouring pacing / slow. Bytes that reached the
    /// printer are added to `sent` as they go, so on error the caller knows whether the head
    /// already has data (and must not blindly retry the whole job).
    pub async fn write_bulk(
        &self,
        data: &[u8],
        row_bytes: usize,
        sent: &mut usize,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(), PrintError> {
        let chunk =
            crate::protocol::mxw01::data_chunk_size(self.data.max_payload(), self.slow, row_bytes);
        for piece in data.chunks(chunk) {
            if cancel.is_cancelled() {
                return Err(PrintError::Cancelled);
            }
            self.write_data(piece).await?;
            *sent += piece.len();
            if self.pacing_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.pacing_ms)).await;
            }
        }
        Ok(())
    }

    /// Send `packet`, then wait for the first notification whose command byte is `expect`.
    /// Arms the receiver BEFORE writing.
    pub async fn request(
        &self,
        packet: &[u8],
        expect: u8,
        timeout: Duration,
    ) -> Result<Vec<u8>, PrintError> {
        let mut rx = self.subscribe();
        self.write_ctrl(packet).await?;
        let wait = async {
            loop {
                match rx.recv().await {
                    Ok(raw) => {
                        if let Some(frame) = crate::protocol::mxw01::parse_notification(&raw) {
                            if frame.cmd == expect {
                                return Ok(frame.payload);
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return Err(PrintError::LinkLost),
                }
            }
        };
        match tokio::time::timeout(timeout, wait).await {
            Ok(r) => r,
            Err(_) => Err(PrintError::NoAnswer(match expect {
                0xA1 => "status",
                0xA9 => "print request",
                0xAA => "print complete",
                _ => "printer reply",
            })),
        }
    }

    /// Wait on an already-armed receiver for a raw notification matching `pred` (classic ready
    /// notification). The receiver must be subscribed BEFORE the write it answers, so a ping
    /// that arrives while the tail of the stream is still going out is not missed.
    pub async fn wait_raw_on(
        rx: &mut broadcast::Receiver<Vec<u8>>,
        timeout: Duration,
        mut pred: impl FnMut(&[u8]) -> bool,
    ) -> Result<Vec<u8>, PrintError> {
        let wait = async {
            loop {
                match rx.recv().await {
                    Ok(raw) => {
                        if pred(&raw) {
                            return Ok(raw);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return Err(PrintError::LinkLost),
                }
            }
        };
        tokio::time::timeout(timeout, wait)
            .await
            .map_err(|_| PrintError::NoAnswer("printer reply"))?
    }

    pub fn row_bytes(&self) -> usize {
        if self.family == Family::Mxw01 && self.caps.grayscale_4bpp {
            192
        } else {
            48
        }
    }

    /// Disconnect and release the device. Idempotent; always attempted. Bounded: two
    /// CLOSE_CALLs plus POST_DISCONNECT_SLEEP (≈ 2 + 2 + 0.4 s worst case).
    pub async fn close(mut self) {
        let _ = tokio::time::timeout(CLOSE_CALL, self.notify_chr.stop_notify()).await;
        // Drop the acquired fd before disconnecting so BlueZ releases the channel.
        self.data = DataPath::WriteValue {
            chr: self.control.clone(),
            mtu: 23,
        };
        let _ = tokio::time::timeout(CLOSE_CALL, self.dev.disconnect()).await;
        // Only mark closed once the Disconnect call has actually returned: if this future is
        // dropped mid-close, the Drop fallback below still releases the link.
        self.closed = true;
        tracing::info!("disconnected");
        // Cheap LE toys keep advertising off until the link is fully gone.
        tokio::time::sleep(POST_DISCONNECT_SLEEP).await;
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        // Best-effort disconnect on cancellation / panic (ble.py:433-451), routed through the
        // cleanup tracker so main's drain() waits for it before the process exits.
        let conn = self.conn.clone();
        let path = self.device_path.clone();
        crate::ble::cleanup::spawn(async move {
            if let Ok(dev) = bluez::device_proxy(&conn, &path).await {
                let _ = tokio::time::timeout(bluez::CALL_TIMEOUT, dev.disconnect()).await;
            }
        });
    }
}

/// Disconnects the device if the connect phase is abandoned (cancellation, panic, error) before a
/// `Session` exists to own the link. Disarmed once the Session takes over.
struct ConnectGuard {
    conn: Connection,
    path: String,
    armed: bool,
}

impl Drop for ConnectGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Routed through the cleanup tracker so main's drain() waits for it (D1).
        let conn = self.conn.clone();
        let path = self.path.clone();
        crate::ble::cleanup::spawn(async move {
            if let Ok(dev) = bluez::device_proxy(&conn, &path).await {
                let _ = tokio::time::timeout(bluez::CALL_TIMEOUT, dev.disconnect()).await;
            }
        });
    }
}

/// Overlap the kind-specific pause with the advert wait so host-abort + RSSI
/// is one window, not pause-then-wait.
pub(crate) fn retry_ready_budget(pause: Duration, advert: Duration) -> Duration {
    pause.max(advert)
}

/// If `wait_until_ready_to_connect` returned early, still honour `floor`
/// (BlueZ's ~2 s disconnect timer after host-abort).
pub(crate) fn leftover_floor(elapsed: Duration, floor: Duration) -> Duration {
    floor.saturating_sub(elapsed)
}

/// Worst-case wall time for one `Session::connect` + GATT bind + `close` when the
/// printer is advertising or Settings-held. Excludes thermal-head AA wait.
/// Must stay slightly over a minute at most so kids are not parked in BLE retries.
pub fn live_job_setup_ceiling() -> Duration {
    let combo = crate::ble::host::advert_wait_for(crate::ble::host::ChipFamily::Realtek);
    let retry = retry_ready_budget(HOST_ABORT_PAUSE, combo);
    Duration::from_secs(LIVE_CONNECT_TIMEOUT_S) * u32::from(CONNECT_ATTEMPTS)
        + retry * u32::from(CONNECT_ATTEMPTS.saturating_sub(1))
        + SERVICES_RESOLVED_TIMEOUT
        + CLOSE_CALL
        + CLOSE_CALL
        + POST_DISCONNECT_SLEEP
}

/// Inter-attempt pause: host-abort (autosuspend/coex) waits longer; prune is short + re-resolve.
/// Adapter-off waits longest so USB reset / firmware reload can finish.
pub(crate) fn pause_after_failure(kind: ConnectFailureKind) -> Duration {
    match kind {
        ConnectFailureKind::AdapterOff => ADAPTER_OFF_PAUSE,
        ConnectFailureKind::HostAbort => HOST_ABORT_PAUSE,
        ConnectFailureKind::Pruned | ConnectFailureKind::NotAdvertising => PRUNE_PAUSE,
        _ => CONNECT_RETRY_PAUSE,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConnectNext {
    Retry {
        pause: Duration,
        /// StartDiscovery if it died — never StopDiscovery between attempts.
        ensure_discovery: bool,
        /// Set Powered / wait for USB re-enumeration.
        recover_adapter: bool,
    },
    GiveUp,
    FailAdapterOff,
}

/// What to do after a classified connect failure.
///
/// AdapterOff used to be immediately terminal. Combo-card USB reset (btusb_reset
/// after a hung Connect) drops Adapter1.Powered for a few seconds; giving up
/// then leaves the job with "Bluetooth is turned off" while the radio is only
/// reloading firmware. Recover and retry inside the attempt budget; only the
/// last attempt stays terminal.
pub(crate) fn connect_next_action(
    kind: ConnectFailureKind,
    attempt: u8,
    attempts: u8,
) -> ConnectNext {
    if kind == ConnectFailureKind::AdapterOff && attempt >= attempts {
        return ConnectNext::FailAdapterOff;
    }
    if attempt >= attempts {
        return ConnectNext::GiveUp;
    }
    ConnectNext::Retry {
        pause: pause_after_failure(kind),
        ensure_discovery: matches!(
            kind,
            ConnectFailureKind::Pruned
                | ConnectFailureKind::HostAbort
                | ConnectFailureKind::NotAdvertising
        ),
        recover_adapter: kind == ConnectFailureKind::AdapterOff,
    }
}

/// `Ok(Some(pause))` = try again after overlapping that pause with the advert
/// wait; `Ok(None)` = last attempt used up; `Err` = terminal.
async fn apply_connect_next(
    conn: &Connection,
    adapter_path: &str,
    next: ConnectNext,
) -> Result<Option<Duration>, PrintError> {
    match next {
        ConnectNext::FailAdapterOff => Err(PrintError::AdapterOff),
        ConnectNext::GiveUp => Ok(None),
        ConnectNext::Retry {
            pause,
            ensure_discovery,
            recover_adapter,
        } => {
            if recover_adapter {
                let _ = bluez::recover_adapter(conn, adapter_path).await;
            }
            if ensure_discovery {
                discovery::ensure_le_discovery(conn, adapter_path).await;
            }
            Ok(Some(pause))
        }
    }
}

struct OneConnect {
    kind: ConnectFailureKind,
    detail: String,
}

fn log_connect_fail(
    target: &Candidate,
    rssi: Option<i16>,
    attempt: u8,
    attempts: u8,
    path: &str,
    kind: ConnectFailureKind,
    detail: &str,
) {
    // A single-attempt probe of a merely-cached device is routine (printer off).
    if attempts == 1 {
        tracing::info!(
            address = %target.address,
            rssi = ?rssi,
            attempt,
            attempts,
            path = %path,
            kind = kind.as_str(),
            "connect attempt failed: {detail}"
        );
    } else {
        tracing::warn!(
            address = %target.address,
            rssi = ?rssi,
            attempt,
            attempts,
            path = %path,
            kind = kind.as_str(),
            "connect attempt failed: {detail}"
        );
    }
}

async fn try_one_connect(dev: &Device1Proxy<'static>) -> Result<(), OneConnect> {
    match tokio::time::timeout(Duration::from_secs(LIVE_CONNECT_TIMEOUT_S), dev.connect()).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) if bluez::is_error_named(&e, "org.bluez.Error.AlreadyConnected") => Ok(()),
        Ok(Err(e)) if bluez::is_error_named(&e, "org.bluez.Error.InProgress") => {
            wait_connected(dev, Duration::from_secs(LIVE_CONNECT_TIMEOUT_S)).await
        }
        Ok(Err(e)) => Err(OneConnect {
            kind: bluez::classify_connect_error(&e),
            detail: bluez::err_message(&e),
        }),
        Err(_) => {
            // Cancel a hung in-flight connect.
            let _ = tokio::time::timeout(CLOSE_CALL, dev.disconnect()).await;
            Err(OneConnect {
                kind: ConnectFailureKind::Timeout,
                detail: "timed out".into(),
            })
        }
    }
}

async fn wait_connected(dev: &Device1Proxy<'static>, budget: Duration) -> Result<(), OneConnect> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Ok(Ok(true)) = tokio::time::timeout(bluez::CALL_TIMEOUT, dev.connected()).await {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = tokio::time::timeout(bluez::CALL_TIMEOUT, dev.disconnect()).await;
            return Err(OneConnect {
                kind: ConnectFailureKind::Timeout,
                detail: "timed out".into(),
            });
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// GATT bind on a *live* Device1 path after Connect succeeded. Never uses the discovery-time path.
#[allow(clippy::too_many_arguments)]
async fn bind_session(
    conn: &Connection,
    path: &str,
    dev: Device1Proxy<'static>,
    target: &Candidate,
    forced: Option<Family>,
    notify_mode: NotifyMode,
    pacing_ms: u64,
    slow: bool,
) -> Result<Session, PrintError> {
    wait_services_resolved(&dev).await?;

    let objs = bluez::managed_objects(conn).await?;
    let (control_path, notify_path, data_path, has_ae03) = resolve_chars(&objs, path)?;
    let name = target.name.clone().or_else(|| {
        bluez::prop_str(
            bluez::iface_props(&objs, path, bluez::IFACE_DEVICE).unwrap_or(&HashMap::new()),
            "Name",
        )
    });
    let detected = models::detect(name.as_deref(), has_ae03, forced);
    tracing::info!(
        "connected to {} — driving as {} ({})",
        target.label(),
        detected.family.name(),
        detected.label
    );

    let control = bluez::char_proxy(conn, &control_path).await?;
    let notify_chr = bluez::char_proxy(conn, &notify_path).await?;

    // Arm notifications BEFORE any write (fixes the arm-after-write race in the Python code).
    let (tx, notify_task) =
        start_notifications(conn, &notify_chr, &notify_path, notify_mode).await?;

    // Bulk data channel: MXW01 → AE03 via AcquireWrite, classic → AE01 via WriteValue only
    // (AcquireWrite on AE01 makes BlueZ refuse the WriteValue control frames that
    // identify/status need with NotPermitted).
    let mut write_type: &'static str = "command";
    let data = if detected.family == Family::Classic {
        // Upstream ble.py writes plain GATT chunks of `mtu - 3` to AE01 (ble.py:100,110-112).
        let mtu = tokio::time::timeout(bluez::CALL_TIMEOUT, control.mtu())
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or(23)
            .max(23);
        let flags = tokio::time::timeout(bluez::CALL_TIMEOUT, control.flags())
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or_default();
        if flags.iter().any(|f| f == "write") {
            write_type = "request";
        }
        DataPath::WriteValue {
            chr: control.clone(),
            mtu,
        }
    } else {
        let data_char_path = if has_ae03 {
            data_path.clone().unwrap_or(control_path.clone())
        } else {
            control_path.clone()
        };
        // 48 (one 1bpp row) is the floor for every model; 4bpp is downgraded below when the
        // negotiated MTU cannot carry a 192-byte row.
        acquire_data_path(conn, &data_char_path, 48).await?
    };
    let mut caps = detected.caps;
    if caps.grayscale_4bpp && data.max_payload() < 192 {
        tracing::info!(
            "MTU {} cannot carry 192-byte 4bpp rows; falling back to 1-bit",
            data.mtu()
        );
        caps.grayscale_4bpp = false;
    }
    tracing::info!("MTU {} ({} bytes/write)", data.mtu(), data.max_payload());

    Ok(Session {
        conn: conn.clone(),
        device_path: path.to_string(),
        dev,
        control,
        notify_chr,
        data,
        write_type,
        tx,
        _notify_task: NotifyTask(notify_task),
        family: detected.family,
        caps,
        model_label: detected.label,
        pacing_ms,
        slow,
        closed: false,
    })
}

async fn wait_services_resolved(dev: &Device1Proxy<'static>) -> Result<(), PrintError> {
    let deadline = tokio::time::Instant::now() + SERVICES_RESOLVED_TIMEOUT;
    loop {
        if let Ok(Ok(true)) =
            tokio::time::timeout(bluez::CALL_TIMEOUT, dev.services_resolved()).await
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            // Timing out here used to fall through to resolve_chars, which then reported the
            // (non-retryable) NotCatPrinter on an empty tree. Discovery not finishing is a
            // link/timing problem — report it as such so the engine retries.
            return Err(PrintError::Timeout("resolving printer services"));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Find AE01/AE02/AE03 under the device (whichever service is AE30/AF30). Returns
/// (control, notify, data?, has_ae03).
fn resolve_chars(
    objs: &bluez::Objects,
    device_path: &str,
) -> Result<(String, String, Option<String>, bool), PrintError> {
    // Services of this device that are AE30/AF30.
    let service_paths: Vec<String> = bluez::objects_with(objs, bluez::IFACE_SERVICE)
        .into_iter()
        .filter(|(path, props)| {
            path.starts_with(&format!("{device_path}/"))
                && bluez::prop_str(props, "UUID")
                    .is_some_and(|u| SERVICE_UUIDS.iter().any(|s| s.eq_ignore_ascii_case(&u)))
        })
        .map(|(p, _)| p)
        .collect();
    if service_paths.is_empty() {
        return Err(PrintError::NotCatPrinter("missing the AE30 service".into()));
    }
    let mut control = None;
    let mut notify = None;
    let mut data = None;
    for (path, props) in bluez::objects_with(objs, bluez::IFACE_CHAR) {
        if !service_paths
            .iter()
            .any(|s| path.starts_with(&format!("{s}/")))
        {
            continue;
        }
        match bluez::prop_str(&props.clone(), "UUID").map(|u| u.to_ascii_lowercase()) {
            Some(u) if u == CONTROL_UUID => control = Some(path),
            Some(u) if u == NOTIFY_UUID => notify = Some(path),
            Some(u) if u == DATA_UUID => data = Some(path),
            _ => {}
        }
    }
    let control = control.ok_or_else(|| PrintError::NotCatPrinter("missing AE01".into()))?;
    let notify = notify.ok_or_else(|| PrintError::NotCatPrinter("missing AE02".into()))?;
    let has_ae03 = data.is_some();
    Ok((control, notify, data, has_ae03))
}

async fn start_notifications(
    conn: &Connection,
    notify_chr: &GattCharacteristic1Proxy<'static>,
    notify_path: &str,
    mode: NotifyMode,
) -> Result<(broadcast::Sender<Vec<u8>>, tokio::task::JoinHandle<()>), PrintError> {
    let (tx, _rx) = broadcast::channel(NOTIFY_CHANNEL);
    let task = match mode {
        NotifyMode::Acquire => {
            let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
            opts.insert("type", Value::from("notify"));
            let (fd, mtu) = bluez::call(
                "subscribing to notifications",
                bluez::CALL_TIMEOUT,
                notify_chr.acquire_notify(opts),
            )
            .await?;
            let sock = SeqPacket::new(fd.into(), mtu)
                .map_err(|e| PrintError::Bus(format!("notify fd: {e}")))?;
            let tx2 = tx.clone();
            tokio::spawn(async move {
                // Max ATT notification at MTU 517 is 514 bytes; a 512-byte buffer would
                // truncate the top of a full-size notification.
                let mut buf = [0u8; 517];
                loop {
                    match sock.recv(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let _ = tx2.send(buf[..n].to_vec());
                        }
                    }
                }
            })
        }
        NotifyMode::Props => {
            // A single MessageStream on PropertiesChanged for this characteristic (armed before StartNotify).
            let rule = zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface("org.freedesktop.DBus.Properties")
                .map_err(|e| PrintError::Bus(e.to_string()))?
                .member("PropertiesChanged")
                .map_err(|e| PrintError::Bus(e.to_string()))?
                .path(notify_path.to_string())
                .map_err(|e| PrintError::Bus(e.to_string()))?
                .build();
            // AddMatch is a bus round-trip like any other — time-bound it (the one call that
            // previously had no timeout).
            let stream = tokio::time::timeout(
                bluez::CALL_TIMEOUT,
                zbus::MessageStream::for_match_rule(rule, conn, Some(NOTIFY_CHANNEL)),
            )
            .await
            .map_err(|_| PrintError::Timeout("subscribing to notifications"))?
            .map_err(|e| PrintError::Bus(e.to_string()))?;
            let tx2 = tx.clone();
            tokio::spawn(async move {
                use futures_util::StreamExt;
                let mut stream = stream;
                while let Some(Ok(msg)) = stream.next().await {
                    if let Some(bytes) = notify_payload(&msg) {
                        let _ = tx2.send(bytes);
                    }
                }
            })
        }
    };
    // NOW enable notifications (in props mode; harmless in acquire mode where AcquireNotify already did).
    if matches!(mode, NotifyMode::Props) {
        bluez::call(
            "enabling notifications",
            bluez::CALL_TIMEOUT,
            notify_chr.start_notify(),
        )
        .await?;
    }
    Ok((tx, task))
}

/// Extract the `Value` byte array from a `PropertiesChanged(interface, changed, invalidated)` on a
/// GattCharacteristic1.
fn notify_payload(msg: &zbus::message::Message) -> Option<Vec<u8>> {
    let body = msg.body();
    let (iface, changed, _invalidated): (
        String,
        HashMap<String, zbus::zvariant::OwnedValue>,
        Vec<String>,
    ) = body.deserialize().ok()?;
    if iface != bluez::IFACE_CHAR {
        return None;
    }
    let v = changed.get("Value")?;
    bluez::value_bytes(v)
}

async fn acquire_data_path(
    conn: &Connection,
    char_path: &str,
    row_bytes: usize,
) -> Result<DataPath, PrintError> {
    let chr = bluez::char_proxy(conn, char_path).await?;
    for attempt in 0..2 {
        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opts.insert("type", Value::from("command"));
        match tokio::time::timeout(bluez::CALL_TIMEOUT, chr.acquire_write(opts)).await {
            Ok(Ok((fd, mtu))) => {
                if (mtu as usize).saturating_sub(3) >= row_bytes {
                    match SeqPacket::new(fd.into(), mtu) {
                        Ok(sock) => return Ok(DataPath::Acquired(sock)),
                        Err(e) => tracing::debug!("wrap acquired fd: {e}"),
                    }
                } else {
                    tracing::debug!(
                        "AcquireWrite MTU {mtu} too small for {row_bytes}-byte rows; retrying"
                    );
                    drop(fd);
                }
            }
            Ok(Err(e)) => {
                tracing::debug!("AcquireWrite failed: {}", bluez::err_message(&e));
                break; // not supported → fall back to WriteValue
            }
            Err(_) => tracing::debug!("AcquireWrite timed out"),
        }
        if attempt == 0 {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    // Fallback: WriteValue with the MTU property (BlueZ often leaves it at 23).
    let mtu = tokio::time::timeout(bluez::CALL_TIMEOUT, chr.mtu())
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or(23)
        .max(23);
    if (mtu as usize).saturating_sub(3) < row_bytes {
        return Err(PrintError::MtuTooSmall { mtu });
    }
    Ok(DataPath::WriteValue { chr, mtu })
}

/// What to do with an I/O error from the acquired SOCK_SEQPACKET channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IoAction {
    /// Transient (EINTR/EAGAIN): write the same packet again.
    Retry,
    /// Everything else on this socket means the LE link is gone (ENOTCONN, EPIPE, EIO, …) —
    /// surface the retryable LinkLost, never the non-retryable generic Io error.
    LinkLost,
}

fn classify_io(e: &std::io::Error) -> IoAction {
    match e.raw_os_error() {
        Some(libc::EINTR) | Some(libc::EAGAIN) => IoAction::Retry,
        _ => IoAction::LinkLost,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_io_maps_errnos() {
        let e = |n: i32| std::io::Error::from_raw_os_error(n);
        assert_eq!(classify_io(&e(libc::EINTR)), IoAction::Retry);
        assert_eq!(classify_io(&e(libc::EAGAIN)), IoAction::Retry);
        assert_eq!(classify_io(&e(libc::ENOTCONN)), IoAction::LinkLost);
        assert_eq!(classify_io(&e(libc::EPIPE)), IoAction::LinkLost);
        assert_eq!(classify_io(&e(libc::ECONNRESET)), IoAction::LinkLost);
        assert_eq!(classify_io(&e(libc::EIO)), IoAction::LinkLost);
        // No errno at all (e.g. our own WriteZero) still counts as a lost link.
        assert_eq!(
            classify_io(&std::io::Error::new(std::io::ErrorKind::WriteZero, "x")),
            IoAction::LinkLost
        );
    }

    #[test]
    fn host_abort_pauses_longer_than_timeout_and_prune() {
        let abort = pause_after_failure(ConnectFailureKind::HostAbort);
        let timeout = pause_after_failure(ConnectFailureKind::Timeout);
        let prune = pause_after_failure(ConnectFailureKind::Pruned);
        let other = pause_after_failure(ConnectFailureKind::Other);
        assert!(
            abort > timeout,
            "{abort:?} should exceed timeout {timeout:?}"
        );
        assert!(abort > prune, "{abort:?} should exceed prune {prune:?}");
        assert_eq!(timeout, CONNECT_RETRY_PAUSE);
        assert_eq!(other, CONNECT_RETRY_PAUSE);
        assert_eq!(prune, PRUNE_PAUSE);
        assert_eq!(abort, HOST_ABORT_PAUSE);
        assert!(
            pause_after_failure(ConnectFailureKind::AdapterOff) > abort,
            "USB-reset recovery must wait longer than a host-abort"
        );
    }

    #[test]
    fn connect_next_action_by_kind() {
        assert_eq!(
            connect_next_action(ConnectFailureKind::AdapterOff, 3, 3),
            ConnectNext::FailAdapterOff
        );
        assert_eq!(
            connect_next_action(ConnectFailureKind::AdapterOff, 1, 3),
            ConnectNext::Retry {
                pause: ADAPTER_OFF_PAUSE,
                ensure_discovery: false,
                recover_adapter: true,
            }
        );
        assert_eq!(
            connect_next_action(ConnectFailureKind::Timeout, 3, 3),
            ConnectNext::GiveUp
        );
        assert_eq!(
            connect_next_action(ConnectFailureKind::Timeout, 1, 3),
            ConnectNext::Retry {
                pause: CONNECT_RETRY_PAUSE,
                ensure_discovery: false,
                recover_adapter: false,
            }
        );
        assert_eq!(
            connect_next_action(ConnectFailureKind::Pruned, 1, 3),
            ConnectNext::Retry {
                pause: PRUNE_PAUSE,
                ensure_discovery: true,
                recover_adapter: false,
            }
        );
        assert_eq!(
            connect_next_action(ConnectFailureKind::HostAbort, 2, 3),
            ConnectNext::Retry {
                pause: HOST_ABORT_PAUSE,
                ensure_discovery: true,
                recover_adapter: false,
            }
        );
        assert_eq!(
            connect_next_action(ConnectFailureKind::Other, 2, 3),
            ConnectNext::Retry {
                pause: CONNECT_RETRY_PAUSE,
                ensure_discovery: false,
                recover_adapter: false,
            }
        );
        assert_eq!(
            connect_next_action(ConnectFailureKind::NotAdvertising, 1, 3),
            ConnectNext::Retry {
                pause: PRUNE_PAUSE,
                ensure_discovery: true,
                recover_adapter: false,
            }
        );
        assert_eq!(
            connect_next_action(ConnectFailureKind::NotAdvertising, 3, 3),
            ConnectNext::GiveUp
        );
        assert_eq!(
            pause_after_failure(ConnectFailureKind::NotAdvertising),
            PRUNE_PAUSE
        );
    }

    #[test]
    fn leftover_floor_honours_host_abort_even_if_rssi_is_already_set() {
        assert_eq!(
            leftover_floor(Duration::from_millis(0), HOST_ABORT_PAUSE),
            HOST_ABORT_PAUSE
        );
        assert_eq!(
            leftover_floor(HOST_ABORT_PAUSE, HOST_ABORT_PAUSE),
            Duration::ZERO
        );
        assert_eq!(
            leftover_floor(
                HOST_ABORT_PAUSE + Duration::from_millis(50),
                HOST_ABORT_PAUSE
            ),
            Duration::ZERO
        );
        let advert = crate::ble::host::advert_wait_for(crate::ble::host::ChipFamily::Realtek);
        assert_eq!(
            retry_ready_budget(HOST_ABORT_PAUSE, advert),
            HOST_ABORT_PAUSE.max(advert)
        );
        assert!(
            retry_ready_budget(HOST_ABORT_PAUSE, advert) < Duration::from_secs(5),
            "pause+advert must overlap, not stack to 7+ s"
        );
    }

    #[test]
    fn live_setup_ceiling_stays_under_a_minute() {
        let ceiling = live_job_setup_ceiling();
        assert!(
            ceiling <= Duration::from_secs(60),
            "live printer setup {ceiling:?} must not exceed a minute (plus head time)"
        );
        assert!(
            Duration::from_secs(LIVE_CONNECT_TIMEOUT_S)
                < Duration::from_secs(crate::protocol::mxw01::CONNECT_TIMEOUT_S)
        );
        // 3 × 8 s live Connect is the bulk; stacked 12 s pages + 5 s adverts would miss this.
        assert!(
            Duration::from_secs(LIVE_CONNECT_TIMEOUT_S) * u32::from(CONNECT_ATTEMPTS)
                + HOST_ABORT_PAUSE * 2
                < Duration::from_secs(35)
        );
        let src = include_str!("session.rs");
        assert!(
            src.contains("LIVE_CONNECT_TIMEOUT_S"),
            "try_one_connect must use the live timeout, not only mention it"
        );
        assert!(
            src.contains("leftover_floor"),
            "connect loop must honour leftover_floor"
        );
        let open = include_str!("mod.rs");
        let start = open.find("async fn open(").expect("open");
        let end = open.find("pub async fn print(").expect("print");
        let block = &open[start..end];
        assert!(
            !block.contains("cached printer did not answer"),
            "live cache must not fall through to an 8 s scan after ConnectFailed"
        );
        assert!(
            block.contains("CONNECT_ATTEMPTS"),
            "advertising cache uses the full attempt budget"
        );
    }

    #[test]
    fn retry_refresh_path_does_not_stop_discovery() {
        let src = include_str!("discovery.rs");
        let start = src
            .find("pub async fn ensure_le_discovery")
            .expect("ensure_le_discovery");
        let end = src.find("pub async fn scan").expect("scan");
        let block = &src[start..end];
        assert!(
            !block.contains("stop_scan"),
            "ensure/refresh must not StopDiscovery (combo USB reset): {block}"
        );
        assert!(block.contains("start_le_discovery") || block.contains("ensure_le_discovery"));
    }
}
