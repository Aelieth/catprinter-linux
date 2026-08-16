//! A connected GATT session: connect with retries, resolve characteristics, detect the model,
//! arm notifications *before* any write, acquire a real ATT MTU, stream bulk data, and always
//! disconnect on the way out (RAII). Ported from catprinter/ble.py `MXW01`.

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::broadcast;
use zbus::zvariant::Value;
use zbus::Connection;

use crate::ble::bluez::{self, Device1Proxy, GattCharacteristic1Proxy};
use crate::ble::discovery::Candidate;
use crate::ble::seqpacket::SeqPacket;
use crate::config::NotifyMode;
use crate::models::{self, Caps, Family};
use crate::printer::PrintError;
use crate::protocol::mxw01::CONNECT_TIMEOUT_S;
use crate::protocol::{CONTROL_UUID, DATA_UUID, NOTIFY_UUID, SERVICE_UUIDS};

const SERVICES_RESOLVED_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_RETRY_PAUSE: Duration = Duration::from_millis(1200);
const POST_DISCONNECT_SLEEP: Duration = Duration::from_millis(800);
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
    /// `attempts`: 3 for a device that is advertising (scan) or Settings-held (Connected), 1 for a
    /// merely-cached entry that may be stale (each timeout costs CONNECT_TIMEOUT_S).
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
        let dev = bluez::device_proxy(conn, &target.path).await?;
        let weak = target.rssi.is_some_and(|r| r < -80);
        connect_with_retries(&dev, was_connected, attempts.max(1), weak).await?;
        wait_services_resolved(conn, &dev).await?;

        // Resolve characteristics from the object tree under this device.
        let objs = bluez::managed_objects(conn).await?;
        let (control_path, notify_path, data_path, has_ae03) = resolve_chars(&objs, &target.path)?;
        let name = target.name.clone().or_else(|| {
            bluez::prop_str(
                bluez::iface_props(&objs, &target.path, bluez::IFACE_DEVICE)
                    .unwrap_or(&HashMap::new()),
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

        // Bulk data channel: MXW01 → AE03, classic → AE01 (it streams everything to control).
        let data_char_path = if detected.family == Family::Mxw01 && has_ae03 {
            data_path.clone().unwrap_or(control_path.clone())
        } else {
            control_path.clone()
        };
        let row_bytes = if detected.family == Family::Mxw01 && detected.caps.grayscale_4bpp {
            192
        } else {
            48
        };
        let data = acquire_data_path(conn, &data_char_path, row_bytes).await?;
        tracing::info!("MTU {} ({} bytes/write)", data.mtu(), data.max_payload());

        Ok(Session {
            conn: conn.clone(),
            device_path: target.path.clone(),
            dev,
            control,
            notify_chr,
            data,
            tx,
            _notify_task: NotifyTask(notify_task),
            family: detected.family,
            caps: detected.caps,
            model_label: detected.label,
            pacing_ms,
            slow,
            closed: false,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.tx.subscribe()
    }

    /// Write a control frame (small) to the control characteristic, write-without-response.
    pub async fn write_ctrl(&self, packet: &[u8]) -> Result<(), PrintError> {
        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opts.insert("type", Value::from("command"));
        bluez::call(
            "writing to the printer",
            bluez::CALL_TIMEOUT,
            self.control.write_value(packet, opts),
        )
        .await
    }

    /// Stream bulk data in whole-row chunks, honouring pacing / slow.
    pub async fn write_bulk(
        &self,
        data: &[u8],
        row_bytes: usize,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(), PrintError> {
        let chunk =
            crate::protocol::mxw01::data_chunk_size(self.data.max_payload(), self.slow, row_bytes);
        for piece in data.chunks(chunk) {
            if cancel.is_cancelled() {
                return Err(PrintError::Cancelled);
            }
            match &self.data {
                DataPath::Acquired(sock) => sock.send(piece).await.map_err(classify_io)?,
                DataPath::WriteValue { chr, .. } => {
                    let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
                    opts.insert("type", Value::from("command"));
                    bluez::call(
                        "sending image data",
                        bluez::CALL_TIMEOUT,
                        chr.write_value(piece, opts),
                    )
                    .await?;
                }
            }
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

    /// Wait for a raw notification matching `pred` (classic ready notification).
    pub async fn wait_raw(
        &self,
        timeout: Duration,
        mut pred: impl FnMut(&[u8]) -> bool,
    ) -> Result<Vec<u8>, PrintError> {
        let mut rx = self.subscribe();
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

    /// Disconnect and release the device. Idempotent; always attempted.
    pub async fn close(mut self) {
        self.closed = true;
        let _ = tokio::time::timeout(bluez::CALL_TIMEOUT, self.notify_chr.stop_notify()).await;
        // Drop the acquired fd before disconnecting so BlueZ releases the channel.
        self.data = DataPath::WriteValue {
            chr: self.control.clone(),
            mtu: 23,
        };
        let _ = tokio::time::timeout(bluez::CALL_TIMEOUT, self.dev.disconnect()).await;
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
        // Best-effort disconnect on cancellation / panic (ble.py:433-451).
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let conn = self.conn.clone();
            let path = self.device_path.clone();
            handle.spawn(async move {
                if let Ok(dev) = bluez::device_proxy(&conn, &path).await {
                    let _ = tokio::time::timeout(bluez::CALL_TIMEOUT, dev.disconnect()).await;
                }
            });
        }
    }
}

async fn connect_with_retries(
    dev: &Device1Proxy<'static>,
    was_connected: bool,
    attempts: u8,
    weak: bool,
) -> Result<(), PrintError> {
    let mut last = String::new();
    for attempt in 1..=attempts {
        match tokio::time::timeout(Duration::from_secs(CONNECT_TIMEOUT_S), dev.connect()).await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(e)) if bluez::is_error_named(&e, "org.bluez.Error.AlreadyConnected") => {
                return Ok(())
            }
            Ok(Err(e)) => {
                last = bluez::err_message(&e);
                tracing::warn!("connect attempt {attempt}/{attempts} failed: {last}");
                if bluez::is_error_named(&e, "org.bluez.Error.NotReady") {
                    return Err(PrintError::AdapterOff);
                }
            }
            Err(_) => {
                last = "timed out".into();
                tracing::warn!("connect attempt {attempt}/{attempts} timed out");
                // Cancel a hung in-flight connect.
                let _ = tokio::time::timeout(bluez::CALL_TIMEOUT, dev.disconnect()).await;
            }
        }
        if attempt < attempts {
            tokio::time::sleep(CONNECT_RETRY_PAUSE).await;
        }
    }
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

async fn wait_services_resolved(
    conn: &Connection,
    dev: &Device1Proxy<'static>,
) -> Result<(), PrintError> {
    let deadline = tokio::time::Instant::now() + SERVICES_RESOLVED_TIMEOUT;
    loop {
        if let Ok(Ok(true)) =
            tokio::time::timeout(bluez::CALL_TIMEOUT, dev.services_resolved()).await
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            // Some stacks never flip the property but still expose the chars; let the caller try.
            let _ = conn;
            return Ok(());
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
                let mut buf = [0u8; 512];
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
            let stream = zbus::MessageStream::for_match_rule(rule, conn, Some(NOTIFY_CHANNEL))
                .await
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

fn classify_io(e: std::io::Error) -> PrintError {
    match e.raw_os_error() {
        Some(libc::ENOTCONN) | Some(libc::EPIPE) | Some(libc::ECONNRESET) => PrintError::LinkLost,
        _ => PrintError::Io(e),
    }
}
