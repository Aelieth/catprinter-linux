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
    ConnectFailed { attempts: u8, last: String, hint: String },
    #[error("Bluetooth link too small for image data (MTU {mtu}); move the printer closer and try again")]
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
            | PrintError::Bus(_) => true,
            PrintError::Condition(c) => c.error.as_deref() != Some("other"),
            PrintError::NotCatPrinter(_)
            | PrintError::Rejected(_)
            | PrintError::Cancelled
            | PrintError::Render(_)
            | PrintError::Io(_) => false,
        }
    }

    /// Kid-facing one-liner for the queue.
    pub fn kid_message(&self) -> String {
        self.to_string()
    }

    /// IPP printer-state-reasons keywords while this error is being retried / after giving up.
    pub fn printer_reasons(&self) -> &'static [&'static str] {
        match self {
            PrintError::NotFound | PrintError::ConnectFailed { .. } => &["connecting-to-device"],
            PrintError::NoBluetoothd | PrintError::AdapterOff => &["connecting-to-device", "other-error"],
            PrintError::Condition(c) => match c.error.as_deref() {
                Some("no-paper") => &["media-empty-error", "media-needed"],
                Some("overheated") => &["fuser-over-temp-warning"],
                Some("low-battery") => &["other-warning"],
                _ => &["other-error"],
            },
            PrintError::Render(_) => &[],
            PrintError::Cancelled => &[],
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
