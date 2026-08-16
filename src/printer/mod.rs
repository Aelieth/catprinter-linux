//! The thing the job engine prints to: a real Bluetooth cat printer or the fake (files) printer.

pub mod fake;

use std::time::Duration;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::models::Family;
use crate::render::{GrayStrip, RenderOptions};

/// A job after stage-1 rendering: still gray, packed per model at print time.
#[derive(Debug, Clone)]
pub struct PreparedJob {
    pub id: u32,
    pub name: String,
    pub strip: GrayStrip,
    pub opts: RenderOptions,
    pub copies: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Searching,
    Connecting,
    Preparing,
    Printing,
    Finishing,
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub phase: Phase,
    /// 0..=100 within the current job (all copies).
    pub percent: u8,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct PrintReport {
    pub model: String,
    pub family: Option<Family>,
    pub lines: u32,
    pub segments: u32,
    pub copies: u32,
    /// The printer confirmed completion (MXW01 AA / classic ready notify).
    pub complete_confirmed: bool,
    pub mtu: Option<u16>,
    pub battery: Option<u8>,
    pub elapsed: Duration,
}

/// Printer condition reported by a status query (both families map onto this).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    pub ok: bool,
    pub state: String,
    pub battery: Option<u8>,
    pub temperature: Option<u8>,
    /// no-paper | overheated | low-battery | other
    pub error: Option<String>,
    pub message: String,
}

#[derive(Debug, Error)]
pub enum PrintError {
    #[error("Bluetooth is not available on this computer (bluetoothd is not running)")]
    NoBluetoothd,
    #[error("Bluetooth is turned off on this computer")]
    AdapterOff,
    #[error("Cat printer not found — turn it on and keep it near the computer")]
    NotFound,
    #[error("Could not connect to the cat printer ({last}).{hint}")]
    ConnectFailed {
        attempts: u8,
        last: String,
        hint: String,
    },
    #[error("Bluetooth link too small for image data (MTU {mtu}) — the Bluetooth stack negotiated a tiny packet size; turn the printer off and on so it reconnects")]
    MtuTooSmall { mtu: u16 },
    #[error("Connected, but this is not a cat printer we know: {0}")]
    NotCatPrinter(String),
    #[error("{}", .0.message)]
    Condition(Condition),
    #[error("Printer rejected the print job ({0})")]
    Rejected(String),
    #[error("Printer did not answer ({0})")]
    NoAnswer(&'static str),
    #[error("Bluetooth link to the printer was lost")]
    LinkLost,
    /// The link dropped (or the printer went silent) after image data had already been written.
    /// NOT retryable: a blind retry reprints the part that already came out of the head.
    #[error("The print stopped partway (Bluetooth dropped). Move the printer next to the computer and print again.")]
    Interrupted { lines_sent: u32 },
    #[error("Timed out while {0}")]
    Timeout(&'static str),
    #[error("cancelled")]
    Cancelled,
    #[error("Bluetooth error: {0}")]
    Bus(String),
    #[error("render error: {0}")]
    Render(#[from] crate::render::RenderError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl PrintError {
    /// Worth retrying inside the printer-wait window?
    pub fn retryable(&self) -> bool {
        match self {
            PrintError::NoBluetoothd
            | PrintError::AdapterOff
            | PrintError::NotFound
            | PrintError::ConnectFailed { .. }
            | PrintError::MtuTooSmall { .. }
            | PrintError::NoAnswer(_)
            | PrintError::LinkLost
            | PrintError::Timeout(_)
            // A reject is almost always "busy" (head still running, phone app poking it);
            // that is transient, and the printer-wait deadline bounds how long we keep trying.
            | PrintError::Rejected(_)
            | PrintError::Bus(_) => true,
            // Every reported condition (no paper, overheating, even "other") can clear on its
            // own — someone feeds paper, the head cools down — so keep retrying until the
            // printer-wait deadline.
            PrintError::Condition(_) => true,
            PrintError::NotCatPrinter(_)
            | PrintError::Interrupted { .. }
            | PrintError::Cancelled
            | PrintError::Render(_)
            | PrintError::Io(_) => false,
        }
    }

    /// Kid-facing one-liner for the queue.
    pub fn kid_message(&self) -> String {
        match self {
            // The raw reject code means nothing to a kid; busy is the overwhelmingly common cause.
            PrintError::Rejected(_) => {
                "The printer is busy — if it stays stuck, turn it off and on.".into()
            }
            _ => self.to_string(),
        }
    }

    /// IPP printer-state-reasons keywords while this error is being retried / after giving up.
    pub fn printer_reasons(&self) -> &'static [&'static str] {
        match self {
            PrintError::NotFound | PrintError::ConnectFailed { .. } => &["connecting-to-device"],
            PrintError::NoBluetoothd | PrintError::AdapterOff => {
                &["connecting-to-device", "other-error"]
            }
            PrintError::Condition(c) => match c.error.as_deref() {
                Some("no-paper") => &["media-empty-error", "media-needed"],
                Some("overheated") => &["fuser-over-temp-warning"],
                Some("low-battery") => &["other-warning"],
                _ => &["other-error"],
            },
            PrintError::Render(_) => &[],
            PrintError::Cancelled => &[],
            // Interrupted and the rest surface as a generic error.
            _ => &["other-error"],
        }
    }
}

/// Dispatch over the two printer kinds without dyn/async-trait.
pub enum Printer {
    Ble(crate::ble::BlePrinter),
    Fake(fake::FakePrinter),
}

impl Printer {
    pub async fn print(
        &mut self,
        job: &PreparedJob,
        cancel: &CancellationToken,
        progress: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<PrintReport, PrintError> {
        match self {
            Printer::Ble(p) => p.print(job, cancel, progress).await,
            Printer::Fake(p) => p.print(job, cancel, progress).await,
        }
    }

    pub async fn status(&mut self, cancel: &CancellationToken) -> Result<Condition, PrintError> {
        match self {
            Printer::Ble(p) => p.status(cancel).await,
            Printer::Fake(p) => p.status(cancel).await,
        }
    }

    /// Identify-Printer: feed a little paper (MXW01 eject / classic feed) or write a marker file.
    pub async fn identify(&mut self, cancel: &CancellationToken) -> Result<(), PrintError> {
        match self {
            Printer::Ble(p) => p.identify(cancel).await,
            Printer::Fake(p) => p.identify(cancel).await,
        }
    }

    pub fn is_fake(&self) -> bool {
        matches!(self, Printer::Fake(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cond(error: Option<&str>) -> Condition {
        Condition {
            ok: false,
            state: "error".into(),
            battery: None,
            temperature: None,
            error: error.map(String::from),
            message: "test".into(),
        }
    }

    #[test]
    fn retryable_matrix() {
        // Transient — keep trying inside the printer-wait window.
        assert!(PrintError::NoBluetoothd.retryable());
        assert!(PrintError::AdapterOff.retryable());
        assert!(PrintError::NotFound.retryable());
        assert!(PrintError::ConnectFailed {
            attempts: 3,
            last: "x".into(),
            hint: String::new()
        }
        .retryable());
        assert!(PrintError::MtuTooSmall { mtu: 23 }.retryable());
        assert!(PrintError::NoAnswer("status").retryable());
        assert!(PrintError::LinkLost.retryable());
        assert!(PrintError::Timeout("connecting").retryable());
        assert!(PrintError::Bus("boom".into()).retryable());
        // Busy/reject is transient (D4).
        assert!(PrintError::Rejected("01".into()).retryable());
        // Every condition retries, including "other" (D4).
        assert!(PrintError::Condition(cond(Some("no-paper"))).retryable());
        assert!(PrintError::Condition(cond(Some("overheated"))).retryable());
        assert!(PrintError::Condition(cond(Some("other"))).retryable());
        assert!(PrintError::Condition(cond(None)).retryable());
        // Terminal — retrying would reprint or can never work.
        assert!(!PrintError::Interrupted { lines_sent: 42 }.retryable());
        assert!(!PrintError::NotCatPrinter("phone".into()).retryable());
        assert!(!PrintError::Cancelled.retryable());
        assert!(!PrintError::Io(std::io::Error::other("x")).retryable());
    }

    #[test]
    fn interrupted_kid_message_and_reasons() {
        let e = PrintError::Interrupted { lines_sent: 7 };
        assert_eq!(
            e.kid_message(),
            "The print stopped partway (Bluetooth dropped). Move the printer next to the computer and print again."
        );
        assert_eq!(e.printer_reasons(), &["other-error"]);
    }

    #[test]
    fn rejected_kid_message_is_busy() {
        let e = PrintError::Rejected("0102".into());
        assert_eq!(
            e.kid_message(),
            "The printer is busy — if it stays stuck, turn it off and on."
        );
        // The technical display keeps the reject code for logs.
        assert!(e.to_string().contains("0102"));
    }

    #[test]
    fn mtu_message_does_not_blame_distance() {
        let m = PrintError::MtuTooSmall { mtu: 23 }.to_string();
        assert!(!m.to_lowercase().contains("closer"), "{m}");
        assert!(m.contains("23"), "{m}");
    }
}
