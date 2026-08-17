//! MXW01 wire protocol: frames, CRC8, commands, status, image packing.
//!
//! Byte-exact port of `catprinter/protocol.py` (see also PROTOCOL.md). Frames start with
//! `0x22 0x21`, image bytes go to characteristic AE03, and CRC-8 (Dallas/Maxim) covers the
//! payload only. Pure functions, no I/O.

use std::fmt;
use std::time::Duration;

use crate::protocol::{crc8, PrintMode};

// --- Geometry ------------------------------------------------------------------------------------

/// Dots across the print head.
pub const WIDTH: usize = 384;
/// Bytes per row at 1 bpp.
pub const ROW_BYTES_1BPP: usize = WIDTH / 8; // 48
/// Bytes per row at 4 bpp.
pub const ROW_BYTES_4BPP: usize = WIDTH / 2; // 192
/// The printer wants at least this many rows per print request; shorter buffers are zero-padded.
pub const MIN_LINES: usize = 90;
/// Minimum AE03 payload at 1 bpp (90 rows).
pub const MIN_DATA_BYTES: usize = MIN_LINES * ROW_BYTES_1BPP; // 4320
/// Minimum AE03 payload at 4 bpp (90 rows).
pub const MIN_DATA_BYTES_4BPP: usize = MIN_LINES * ROW_BYTES_4BPP; // 17280

// --- Framing -------------------------------------------------------------------------------------

/// Every MXW01 frame starts with these two bytes.
pub const PREAMBLE: [u8; 2] = [0x22, 0x21];
/// Every MXW01 frame ends with this byte.
pub const FOOTER: u8 = 0xFF;
/// A2 intensity that prints well on plain paper.
pub const DEFAULT_INTENSITY: u8 = 0x5D;

// --- Timing (from catprinter/ble.py) --------------------------------------------------------------

/// Seconds to scan for the printer before giving up.
pub const SCAN_TIMEOUT_S: u64 = 8;
/// Milliseconds between bulk AE03 writes.
pub const PACING_MS: u64 = 8;
/// Seconds to wait for a reply notification to a control command.
pub const NOTIFICATION_TIMEOUT_S: u64 = 7;
/// Base seconds to wait for the AA print-complete notification…
pub const PRINT_COMPLETE_BASE_TIMEOUT_S: u64 = 15;
/// …plus one second per this many lines.
pub const PRINT_COMPLETE_LINES_PER_SEC: u64 = 15;
/// Public ceiling for a Connect / InProgress wait. Not used for a live advert —
/// that path uses [`LIVE_CONNECT_TIMEOUT_S`] so a powered-on printer cannot sit
/// in a 12 s page. Kept at 12: docs and the `CATPRINTER_*` contract.
pub const CONNECT_TIMEOUT_S: u64 = 12;
/// Per-attempt Device1.Connect when the object is advertising or already held.
/// 8 s cancelled hung in-flight connects on weak links; 20 s was too slow for kids.
pub const LIVE_CONNECT_TIMEOUT_S: u64 = 8;
/// BLE connect attempts before giving up.
pub const CONNECT_ATTEMPTS: u8 = 3;

/// How long to wait for the print-complete (AA) notification for a job of `lines` rows.
pub fn print_complete_timeout(lines: u32) -> Duration {
    Duration::from_secs(PRINT_COMPLETE_BASE_TIMEOUT_S)
        + Duration::from_secs_f64(lines as f64 / PRINT_COMPLETE_LINES_PER_SEC as f64)
}

// --- Commands ------------------------------------------------------------------------------------

/// Command identifiers (byte 2 of a frame).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cmd {
    /// A1 — request / receive status (paper, temperature, battery, state).
    GetStatus = 0xA1,
    /// A2 — set print intensity (one byte).
    Intensity = 0xA2,
    /// A3 — eject (feed) paper, `line_count` LE u16.
    Eject = 0xA3,
    /// A4 — retract paper, `line_count` LE u16.
    Retract = 0xA4,
    /// A7 — query count (unknown semantics).
    QueryCount = 0xA7,
    /// A9 — print request: `line_count` LE u16, 0x30, mode.
    Print = 0xA9,
    /// AA — print complete (received).
    PrintComplete = 0xAA,
    /// AB — battery level.
    Battery = 0xAB,
    /// AC — cancel print.
    CancelPrint = 0xAC,
    /// AD — print data flush (end of AE03 transfer).
    Flush = 0xAD,
    /// B0 — get print type.
    GetPrintType = 0xB0,
    /// B1 — get firmware version.
    GetVersion = 0xB1,
}

impl Cmd {
    /// Map a raw command byte to a known command.
    pub fn from_u8(b: u8) -> Option<Cmd> {
        Some(match b {
            0xA1 => Cmd::GetStatus,
            0xA2 => Cmd::Intensity,
            0xA3 => Cmd::Eject,
            0xA4 => Cmd::Retract,
            0xA7 => Cmd::QueryCount,
            0xA9 => Cmd::Print,
            0xAA => Cmd::PrintComplete,
            0xAB => Cmd::Battery,
            0xAC => Cmd::CancelPrint,
            0xAD => Cmd::Flush,
            0xB0 => Cmd::GetPrintType,
            0xB1 => Cmd::GetVersion,
            _ => return None,
        })
    }
}

/// Errors from frame building / packing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    /// Payload longer than the u16 length field allows.
    PayloadTooLarge(usize),
    /// A row / buffer does not have the printer's width.
    BadWidth { expected: usize, got: usize },
    /// Buffer length is not a whole number of rows.
    NotWholeRows { len: usize, row_bytes: usize },
    /// Nothing to pack.
    Empty,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolError::PayloadTooLarge(n) => write!(f, "payload too large: {n} bytes"),
            ProtocolError::BadWidth { expected, got } => {
                write!(f, "row length must be {expected}, got {got}")
            }
            ProtocolError::NotWholeRows { len, row_bytes } => {
                write!(
                    f,
                    "image buffer length {len} is not a multiple of {row_bytes}"
                )
            }
            ProtocolError::Empty => write!(f, "image has no rows"),
        }
    }
}

impl std::error::Error for ProtocolError {}

/// Build a complete AE01 control packet: `22 21 cmd 00 len_le(2) payload crc8(payload) FF`.
pub fn make_command(cmd: u8, payload: &[u8]) -> Result<Vec<u8>, ProtocolError> {
    if payload.len() > 0xFFFF {
        return Err(ProtocolError::PayloadTooLarge(payload.len()));
    }
    let len = payload.len() as u16;
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&PREAMBLE);
    out.push(cmd);
    out.push(0x00);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(payload);
    out.push(crc8(payload));
    out.push(FOOTER);
    Ok(out)
}

/// A parsed frame (notification or command).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Command byte.
    pub cmd: u8,
    /// Payload bytes (without CRC / footer).
    pub payload: Vec<u8>,
}

/// Parse an MXW01 frame; `None` if the preamble is wrong or the payload is truncated.
/// Trailing bytes beyond the declared payload (CRC, footer) are ignored, as in the Python port.
pub fn parse_notification(data: &[u8]) -> Option<Frame> {
    if data.len() < 6 || data[0] != PREAMBLE[0] || data[1] != PREAMBLE[1] {
        return None;
    }
    let cmd = data[2];
    let payload_len = u16::from_le_bytes([data[4], data[5]]) as usize;
    let end = 6 + payload_len;
    if data.len() < end {
        return None;
    }
    Some(Frame {
        cmd,
        payload: data[6..end].to_vec(),
    })
}

fn cmd(c: Cmd, payload: &[u8]) -> Vec<u8> {
    // Payloads built here are tiny; the u16 limit cannot be exceeded.
    make_command(c as u8, payload).expect("small payload")
}

/// A1 status request.
pub fn cmd_get_status() -> Vec<u8> {
    cmd(Cmd::GetStatus, &[0x00])
}

/// A2 set intensity (0..=255; the Python port clamps, a `u8` cannot overflow).
pub fn cmd_set_intensity(intensity: u8) -> Vec<u8> {
    cmd(Cmd::Intensity, &[intensity])
}

/// Wire byte for a print mode (A9 payload byte 3).
pub fn mode_byte(mode: PrintMode) -> u8 {
    match mode {
        PrintMode::Mono => 0x00,
        PrintMode::Gray4 => 0x02,
    }
}

/// A9 print request: `line_count` (pixel rows, LE u16), 0x30, mode.
pub fn cmd_print_request(lines: u16, mode: PrintMode) -> Vec<u8> {
    let mut payload = Vec::with_capacity(4);
    payload.extend_from_slice(&lines.to_le_bytes());
    payload.push(0x30);
    payload.push(mode_byte(mode));
    cmd(Cmd::Print, &payload)
}

/// AD flush — end of the AE03 transfer.
pub fn cmd_flush() -> Vec<u8> {
    cmd(Cmd::Flush, &[0x00])
}

/// A3 eject (feed) `lines` rows of paper.
pub fn cmd_eject(lines: u16) -> Vec<u8> {
    cmd(Cmd::Eject, &lines.to_le_bytes())
}

/// A4 retract `lines` rows of paper.
pub fn cmd_retract(lines: u16) -> Vec<u8> {
    cmd(Cmd::Retract, &lines.to_le_bytes())
}

/// AB battery level request.
pub fn cmd_get_battery() -> Vec<u8> {
    cmd(Cmd::Battery, &[0x00])
}

/// B1 firmware version request.
pub fn cmd_get_version() -> Vec<u8> {
    cmd(Cmd::GetVersion, &[0x00])
}

// --- Status --------------------------------------------------------------------------------------

/// Printer state byte of an A1 status payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// 0x00
    Standby,
    /// 0x01
    Printing,
    /// 0x02
    Feeding,
    /// 0x03
    Ejecting,
    /// Anything else.
    Unknown(u8),
}

impl State {
    /// Decode the state byte.
    pub fn from_u8(b: u8) -> State {
        match b {
            0x00 => State::Standby,
            0x01 => State::Printing,
            0x02 => State::Feeding,
            0x03 => State::Ejecting,
            other => State::Unknown(other),
        }
    }

    /// Human-readable name (Python `PRINTER_STATES` / `0x%02X` fallback).
    pub fn name(&self) -> String {
        match self {
            State::Standby => "standby".into(),
            State::Printing => "printing".into(),
            State::Feeding => "feeding".into(),
            State::Ejecting => "ejecting".into(),
            State::Unknown(b) => format!("0x{b:02X}"),
        }
    }
}

/// Error reported by an A1 status payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusError {
    /// Codes 0x01 and 0x09.
    NoPaper,
    /// Code 0x04.
    Overheated,
    /// Code 0x08.
    LowBattery,
    /// Any other non-zero error code.
    Other(u8),
    /// The payload was too short to carry a status.
    ShortPayload,
}

impl StatusError {
    /// Python `ERROR_CODES` text ("no paper", "overheated", "low battery", "error 0xNN").
    pub fn name(&self) -> String {
        match self {
            StatusError::NoPaper => "no paper".into(),
            StatusError::Overheated => "overheated".into(),
            StatusError::LowBattery => "low battery".into(),
            StatusError::Other(c) => format!("error 0x{c:02X}"),
            StatusError::ShortPayload => "short status payload".into(),
        }
    }
}

/// Decoded A1 status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrinterStatus {
    /// `payload[6] == 0`.
    pub ok: bool,
    /// Decoded state byte.
    pub state: State,
    /// Raw state byte (`-1` in Python for a short payload; here 0xFF is not used — see `error`).
    pub state_code: u8,
    /// Battery percent (`payload[3]`).
    pub battery: Option<u8>,
    /// Temperature (`payload[4]`).
    pub temperature: Option<u8>,
    /// Error, if `!ok` (or `ShortPayload`).
    pub error: Option<StatusError>,
    /// The raw payload.
    pub raw: Vec<u8>,
}

impl PrinterStatus {
    /// Kid-facing message (exact strings from the Python port).
    pub fn kid_message(&self) -> String {
        if self.ok {
            let batt = match self.battery {
                Some(b) => format!(", battery {b}%"),
                None => String::new(),
            };
            return format!("Printer ready ({}{})", self.state.name(), batt);
        }
        match self.error {
            Some(StatusError::NoPaper) => "The cat printer is out of paper.".into(),
            Some(StatusError::Overheated) => "The cat printer is too hot. Give it a minute.".into(),
            Some(StatusError::LowBattery) => "The cat printer battery is low. Charge it.".into(),
            _ => "The cat printer is not ready. Turn it on and check the paper.".into(),
        }
    }

    /// Stable machine key for the error: `no-paper` | `overheated` | `low-battery` | `other`; `None` if ok.
    pub fn error_key(&self) -> Option<&'static str> {
        if self.ok {
            return None;
        }
        Some(match self.error {
            Some(StatusError::NoPaper) => "no-paper",
            Some(StatusError::Overheated) => "overheated",
            Some(StatusError::LowBattery) => "low-battery",
            _ => "other",
        })
    }
}

/// Parse an A1 status payload (payload-relative indices, matching MaikelChan/CatPrinterBLE which
/// indexes the full GATT notification with the payload starting at byte 6):
///
/// * `[0]` state (0 standby / 1 printing / 2 feeding / 3 ejecting)
/// * `[3]` battery percent
/// * `[4]` temperature
/// * `[6]` 0 = ok, nonzero = error
/// * `[7]` error code (1/9 no paper, 4 overheat, 8 low battery)
pub fn parse_status(payload: &[u8]) -> PrinterStatus {
    if payload.len() < 7 {
        return PrinterStatus {
            ok: false,
            state: State::Unknown(0xFF),
            state_code: 0xFF,
            battery: None,
            temperature: None,
            error: Some(StatusError::ShortPayload),
            raw: payload.to_vec(),
        };
    }
    let state_code = payload[0];
    let battery = payload.get(3).copied();
    let temperature = payload.get(4).copied();
    let ok = payload[6] == 0;
    let error = if !ok && payload.len() > 7 {
        Some(match payload[7] {
            0x01 | 0x09 => StatusError::NoPaper,
            0x04 => StatusError::Overheated,
            0x08 => StatusError::LowBattery,
            other => StatusError::Other(other),
        })
    } else {
        None
    };
    PrinterStatus {
        ok,
        state: State::from_u8(state_code),
        state_code,
        battery,
        temperature,
        error,
        raw: payload.to_vec(),
    }
}

// --- Image packing -------------------------------------------------------------------------------

/// Bytes per row for a print mode (48 for 1 bpp, 192 for 4 bpp).
pub fn bytes_per_row(mode: PrintMode) -> usize {
    match mode {
        PrintMode::Mono => ROW_BYTES_1BPP,
        PrintMode::Gray4 => ROW_BYTES_4BPP,
    }
}

/// Minimum AE03 payload for a print mode (90 rows).
pub fn min_data_bytes(mode: PrintMode) -> usize {
    match mode {
        PrintMode::Mono => MIN_DATA_BYTES,
        PrintMode::Gray4 => MIN_DATA_BYTES_4BPP,
    }
}

/// AE03 write size: whole rows only, as large as the ATT MTU allows.
/// `slow` (or an MTU below one row) forces one row per write.
pub fn data_chunk_size(max_att_bytes: usize, slow: bool, row_bytes: usize) -> usize {
    assert!(row_bytes > 0, "row_bytes must be positive");
    if slow || max_att_bytes < row_bytes {
        return row_bytes;
    }
    (max_att_bytes / row_bytes) * row_bytes
}

/// Gray value (0 black … 255 white) → 4-bit burn level (0 white … 15 black).
pub fn gray_to_level(gray: u8) -> u8 {
    (255 - gray) >> 4
}

/// Pack a `height × width` boolean bitmap (`true` = black, row-major) into 1 bpp rows:
/// LSB = leftmost pixel of each 8-pixel group. `width` must be 384. Zero-pads to `MIN_DATA_BYTES`
/// when `pad`.
pub fn pack_1bpp(rows: &[bool], width: usize, pad: bool) -> Result<Vec<u8>, ProtocolError> {
    if width != WIDTH {
        return Err(ProtocolError::BadWidth {
            expected: WIDTH,
            got: width,
        });
    }
    if rows.is_empty() {
        return Err(ProtocolError::Empty);
    }
    if rows.len() % width != 0 {
        return Err(ProtocolError::NotWholeRows {
            len: rows.len(),
            row_bytes: width,
        });
    }
    let height = rows.len() / width;
    let mut out =
        Vec::with_capacity((height * ROW_BYTES_1BPP).max(if pad { MIN_DATA_BYTES } else { 0 }));
    for row in rows.chunks_exact(width) {
        for group in row.chunks_exact(8) {
            let mut value = 0u8;
            for (bit, &px) in group.iter().enumerate() {
                if px {
                    value |= 1 << bit;
                }
            }
            out.push(value);
        }
    }
    if pad && out.len() < MIN_DATA_BYTES {
        out.resize(MIN_DATA_BYTES, 0x00);
    }
    Ok(out)
}

/// Pack a `height × width` gray buffer (0 black … 255 white) into 1 bpp rows using `is_black`.
pub fn pack_1bpp_from_gray(
    gray: &[u8],
    width: usize,
    height: usize,
    pad: bool,
    is_black: impl Fn(u8) -> bool,
) -> Result<Vec<u8>, ProtocolError> {
    if gray.len() != width * height {
        return Err(ProtocolError::NotWholeRows {
            len: gray.len(),
            row_bytes: width,
        });
    }
    let bits: Vec<bool> = gray.iter().map(|&g| is_black(g)).collect();
    pack_1bpp(&bits, width, pad)
}

/// Pack a `height × width` array of levels (0 = white … 15 = black) into 4 bpp rows:
/// even `x` is the high nibble, odd `x` the low nibble (MaikelChan's MXW01 grayscale encoding).
/// Levels are clamped to 15. `width` must be 384. Zero-pads to `MIN_DATA_BYTES_4BPP` when `pad`.
pub fn pack_4bpp(levels: &[u8], width: usize, pad: bool) -> Result<Vec<u8>, ProtocolError> {
    if width != WIDTH {
        return Err(ProtocolError::BadWidth {
            expected: WIDTH,
            got: width,
        });
    }
    if levels.is_empty() {
        return Err(ProtocolError::Empty);
    }
    if levels.len() % width != 0 {
        return Err(ProtocolError::NotWholeRows {
            len: levels.len(),
            row_bytes: width,
        });
    }
    let height = levels.len() / width;
    let mut out = Vec::with_capacity((height * ROW_BYTES_4BPP).max(if pad {
        MIN_DATA_BYTES_4BPP
    } else {
        0
    }));
    for row in levels.chunks_exact(width) {
        for pair in row.chunks_exact(2) {
            let hi = pair[0].min(15);
            let lo = pair[1].min(15);
            out.push((hi << 4) | lo);
        }
    }
    if pad && out.len() < MIN_DATA_BYTES_4BPP {
        out.resize(MIN_DATA_BYTES_4BPP, 0x00);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- crc8 (uses the shared table) ---

    #[test]
    fn connect_attempt_constants_are_the_public_contract() {
        assert_eq!(CONNECT_TIMEOUT_S, 12);
        assert_eq!(CONNECT_ATTEMPTS, 3);
        const { assert!(LIVE_CONNECT_TIMEOUT_S < CONNECT_TIMEOUT_S) };
        const { assert!(LIVE_CONNECT_TIMEOUT_S >= 6) };
    }

    #[test]
    fn crc8_empty_and_zero() {
        assert_eq!(crc8(&[]), 0x00);
        assert_eq!(crc8(&[0x00]), 0x00);
    }

    #[test]
    fn crc8_known_payload() {
        // CRC-8/MAXIM-DOW, poly 0x07, init 0: table[0x5D] == 0x94
        assert_eq!(crc8(&[0x5D]), 0x94);
    }

    // --- frames ---

    #[test]
    fn make_command_status_layout() {
        let pkt = cmd_get_status();
        assert_eq!(pkt[0], 0x22);
        assert_eq!(pkt[1], 0x21);
        assert_eq!(pkt[2], Cmd::GetStatus as u8);
        assert_eq!(pkt[3], 0x00);
        assert_eq!(u16::from_le_bytes([pkt[4], pkt[5]]), 1);
        assert_eq!(pkt[6], 0x00);
        assert_eq!(pkt[7], crc8(&[0x00]));
        assert_eq!(pkt[8], 0xFF);
        assert_eq!(pkt.len(), 9);
    }

    #[test]
    fn make_command_rejects_huge_payload() {
        let big = vec![0u8; 0x1_0000];
        assert_eq!(
            make_command(0xA1, &big),
            Err(ProtocolError::PayloadTooLarge(0x1_0000))
        );
        assert!(make_command(0xA1, &big[..0xFFFF]).is_ok());
    }

    #[test]
    fn print_request_line_count() {
        let pkt = cmd_print_request(384, PrintMode::Mono);
        let payload = &pkt[6..pkt.len() - 2];
        assert_eq!(u16::from_le_bytes([payload[0], payload[1]]), 384);
        assert_eq!(payload[2], 0x30);
        assert_eq!(payload[3], 0x00);
        assert_eq!(pkt[pkt.len() - 2], crc8(payload));
        assert_eq!(pkt[pkt.len() - 1], 0xFF);
    }

    #[test]
    fn print_request_grayscale_mode() {
        let pkt = cmd_print_request(100, PrintMode::Gray4);
        let payload = &pkt[6..pkt.len() - 2];
        assert_eq!(u16::from_le_bytes([payload[0], payload[1]]), 100);
        assert_eq!(payload[3], 0x02);
        assert_eq!(mode_byte(PrintMode::Mono), 0x00);
        assert_eq!(mode_byte(PrintMode::Gray4), 0x02);
    }

    #[test]
    fn set_intensity_byte() {
        assert_eq!(cmd_set_intensity(0xFF)[6], 0xFF);
        assert_eq!(cmd_set_intensity(0x00)[6], 0x00);
        assert_eq!(cmd_set_intensity(DEFAULT_INTENSITY)[6], 0x5D);
        assert_eq!(cmd_set_intensity(0x5D)[7], 0x94);
    }

    #[test]
    fn eject_retract_flush_battery_version() {
        let e = cmd_eject(0x1234);
        assert_eq!(e[2], 0xA3);
        assert_eq!(&e[6..8], &[0x34, 0x12]);
        let r = cmd_retract(40);
        assert_eq!(r[2], 0xA4);
        assert_eq!(&r[6..8], &[40, 0]);
        assert_eq!(cmd_flush()[2], 0xAD);
        assert_eq!(cmd_flush()[6], 0x00);
        assert_eq!(cmd_get_battery()[2], 0xAB);
        assert_eq!(cmd_get_version()[2], 0xB1);
    }

    #[test]
    fn parse_notification_roundtrip() {
        let pkt = cmd_get_status();
        let parsed = parse_notification(&pkt).expect("frame");
        assert_eq!(parsed.cmd, Cmd::GetStatus as u8);
        assert_eq!(parsed.payload, vec![0x00]);
        assert_eq!(Cmd::from_u8(parsed.cmd), Some(Cmd::GetStatus));
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_notification(b"hello").is_none());
        // A classic-family frame (51 78) is not an MXW01 frame.
        assert!(
            parse_notification(&[0x51, 0x78, 0xA1, 0x00, 0x01, 0x00, 0x00, 0x00, 0xFF]).is_none()
        );
        // Truncated payload.
        assert!(parse_notification(&[0x22, 0x21, 0xA1, 0x00, 0x05, 0x00, 0x00]).is_none());
        // Too short overall.
        assert!(parse_notification(&[0x22, 0x21, 0xA1]).is_none());
    }

    #[test]
    fn cmd_from_u8_covers_all_ids() {
        for (b, c) in [
            (0xA1, Cmd::GetStatus),
            (0xA2, Cmd::Intensity),
            (0xA3, Cmd::Eject),
            (0xA4, Cmd::Retract),
            (0xA7, Cmd::QueryCount),
            (0xA9, Cmd::Print),
            (0xAA, Cmd::PrintComplete),
            (0xAB, Cmd::Battery),
            (0xAC, Cmd::CancelPrint),
            (0xAD, Cmd::Flush),
            (0xB0, Cmd::GetPrintType),
            (0xB1, Cmd::GetVersion),
        ] {
            assert_eq!(Cmd::from_u8(b), Some(c));
            assert_eq!(c as u8, b);
        }
        assert_eq!(Cmd::from_u8(0x00), None);
    }

    // --- status ---

    fn payload(state: u8, battery: u8, temp: u8, ok: u8, error: u8, length: usize) -> Vec<u8> {
        let mut data = vec![0u8; length];
        data[0] = state;
        if length > 3 {
            data[3] = battery;
        }
        if length > 4 {
            data[4] = temp;
        }
        if length > 6 {
            data[6] = ok;
        }
        if length > 7 {
            data[7] = error;
        }
        data
    }

    #[test]
    fn status_ok_standby() {
        let status = parse_status(&payload(0, 80, 25, 0, 0, 8));
        assert!(status.ok);
        assert_eq!(status.state, State::Standby);
        assert_eq!(status.state.name(), "standby");
        assert_eq!(status.battery, Some(80));
        assert_eq!(status.temperature, Some(25));
        assert_eq!(status.error, None);
        assert!(status.kid_message().to_lowercase().contains("ready"));
        assert_eq!(status.kid_message(), "Printer ready (standby, battery 80%)");
        assert_eq!(status.error_key(), None);
    }

    #[test]
    fn status_no_paper() {
        let status = parse_status(&payload(0, 80, 25, 1, 0x01, 8));
        assert!(!status.ok);
        assert_eq!(status.error, Some(StatusError::NoPaper));
        assert_eq!(status.error.unwrap().name(), "no paper");
        assert!(status.kid_message().to_lowercase().contains("paper"));
        assert_eq!(status.kid_message(), "The cat printer is out of paper.");
        assert_eq!(status.error_key(), Some("no-paper"));
        // 0x09 is also "no paper"
        let status = parse_status(&payload(0, 80, 25, 1, 0x09, 8));
        assert_eq!(status.error, Some(StatusError::NoPaper));
    }

    #[test]
    fn status_overheat() {
        let status = parse_status(&payload(0, 80, 25, 1, 0x04, 8));
        assert_eq!(status.error, Some(StatusError::Overheated));
        assert_eq!(status.error.unwrap().name(), "overheated");
        assert_eq!(
            status.kid_message(),
            "The cat printer is too hot. Give it a minute."
        );
        assert_eq!(status.error_key(), Some("overheated"));
    }

    #[test]
    fn status_low_battery() {
        let status = parse_status(&payload(0, 5, 25, 1, 0x08, 8));
        assert_eq!(status.error, Some(StatusError::LowBattery));
        assert_eq!(status.error.unwrap().name(), "low battery");
        assert_eq!(
            status.kid_message(),
            "The cat printer battery is low. Charge it."
        );
        assert_eq!(status.error_key(), Some("low-battery"));
    }

    #[test]
    fn status_unknown_error_and_short_payload() {
        let status = parse_status(&payload(1, 80, 25, 1, 0x42, 8));
        assert_eq!(status.error, Some(StatusError::Other(0x42)));
        assert_eq!(status.error.unwrap().name(), "error 0x42");
        assert_eq!(status.state, State::Printing);
        assert_eq!(
            status.kid_message(),
            "The cat printer is not ready. Turn it on and check the paper."
        );
        assert_eq!(status.error_key(), Some("other"));
        // Error flag set but no error byte at all (length 7): still !ok, error None → generic text.
        let status = parse_status(&payload(0, 80, 25, 1, 0, 7));
        assert!(!status.ok);
        assert_eq!(status.error, None);
        assert_eq!(status.error_key(), Some("other"));
        // Short payload.
        let status = parse_status(&[0x00, 0x00]);
        assert!(!status.ok);
        assert_eq!(status.error, Some(StatusError::ShortPayload));
        assert_eq!(status.battery, None);
        assert_eq!(status.raw, vec![0x00, 0x00]);
    }

    #[test]
    fn state_names() {
        assert_eq!(State::from_u8(2), State::Feeding);
        assert_eq!(State::from_u8(3).name(), "ejecting");
        assert_eq!(State::from_u8(0x7A), State::Unknown(0x7A));
        assert_eq!(State::from_u8(0x7A).name(), "0x7A");
    }

    // --- 1 bpp packing ---

    fn row_of(v: bool) -> Vec<bool> {
        vec![v; WIDTH]
    }

    #[test]
    fn pack_1bpp_row_all_white() {
        let packed = pack_1bpp(&row_of(false), WIDTH, false).unwrap();
        assert_eq!(packed.len(), ROW_BYTES_1BPP);
        assert_eq!(packed, vec![0x00; ROW_BYTES_1BPP]);
    }

    #[test]
    fn pack_1bpp_row_all_black() {
        let packed = pack_1bpp(&row_of(true), WIDTH, false).unwrap();
        assert_eq!(packed, vec![0xFF; ROW_BYTES_1BPP]);
    }

    #[test]
    fn pack_1bpp_lsb_is_leftmost_pixel() {
        let mut row = row_of(false);
        row[0] = true; // leftmost pixel of the page
        let packed = pack_1bpp(&row, WIDTH, false).unwrap();
        assert_eq!(packed[0], 0x01);
        assert_eq!(packed[1..].iter().map(|&b| b as u32).sum::<u32>(), 0);
    }

    #[test]
    fn pack_1bpp_wrong_width() {
        assert_eq!(
            pack_1bpp(&[true; 10], 10, false),
            Err(ProtocolError::BadWidth {
                expected: 384,
                got: 10
            })
        );
        // Right width but not whole rows.
        let mut bits = row_of(false);
        bits.push(true);
        assert!(matches!(
            pack_1bpp(&bits, WIDTH, false),
            Err(ProtocolError::NotWholeRows { .. })
        ));
        assert_eq!(pack_1bpp(&[], WIDTH, false), Err(ProtocolError::Empty));
    }

    #[test]
    fn pack_1bpp_padding_to_minimum() {
        // 10 white rows = 480 bytes, must pad to 4320
        let rows = vec![false; WIDTH * 10];
        let buf = pack_1bpp(&rows, WIDTH, true).unwrap();
        assert_eq!(buf.len(), MIN_DATA_BYTES);
        assert_eq!(buf, vec![0x00; MIN_DATA_BYTES]);
    }

    #[test]
    fn pack_1bpp_no_pad_when_tall_enough() {
        let rows = vec![false; WIDTH * 100];
        let buf = pack_1bpp(&rows, WIDTH, true).unwrap();
        assert_eq!(buf.len(), 100 * ROW_BYTES_1BPP);
    }

    #[test]
    fn pack_1bpp_matches_bit_loop() {
        let mut row = row_of(false);
        row[0] = true;
        row[7] = true;
        row[8] = true;
        let packed = pack_1bpp(&row, WIDTH, false).unwrap();
        assert_eq!(packed[0], 0x81); // bits 0 and 7
        assert_eq!(packed[1], 0x01);
    }

    #[test]
    fn pack_1bpp_from_gray_thresholds() {
        let mut gray = vec![255u8; WIDTH * 2];
        gray[0] = 0; // black at (0,0)
        gray[WIDTH + 383] = 10; // black at (383,1)
        let packed = pack_1bpp_from_gray(&gray, WIDTH, 2, false, |g| g < 128).unwrap();
        assert_eq!(packed.len(), 2 * ROW_BYTES_1BPP);
        assert_eq!(packed[0], 0x01);
        assert_eq!(packed[ROW_BYTES_1BPP + 47], 0x80);
        assert!(matches!(
            pack_1bpp_from_gray(&gray, WIDTH, 3, false, |g| g < 128),
            Err(ProtocolError::NotWholeRows { .. })
        ));
    }

    // --- chunk sizes ---

    #[test]
    fn chunk_size_aligns_to_rows() {
        assert_eq!(data_chunk_size(20, false, ROW_BYTES_1BPP), 48);
        assert_eq!(data_chunk_size(200, true, ROW_BYTES_1BPP), 48);
        assert_eq!(data_chunk_size(509, false, ROW_BYTES_1BPP), 480);
        assert_eq!(data_chunk_size(48, false, ROW_BYTES_1BPP), 48);
    }

    #[test]
    fn chunk_size_4bpp_aligns_to_192() {
        assert_eq!(data_chunk_size(509, false, ROW_BYTES_4BPP), 384);
        assert_eq!(data_chunk_size(200, true, ROW_BYTES_4BPP), 192);
        assert_eq!(bytes_per_row(PrintMode::Gray4), 192);
        assert_eq!(bytes_per_row(PrintMode::Mono), 48);
        assert_eq!(min_data_bytes(PrintMode::Mono), 4320);
        assert_eq!(min_data_bytes(PrintMode::Gray4), 17280);
    }

    #[test]
    fn print_complete_timeout_scales_with_lines() {
        assert_eq!(print_complete_timeout(0), Duration::from_secs(15));
        assert_eq!(print_complete_timeout(150), Duration::from_secs(25));
        assert_eq!(
            print_complete_timeout(4000),
            Duration::from_secs(15) + Duration::from_secs_f64(4000.0 / 15.0)
        );
    }

    // --- 4 bpp packing ---

    #[test]
    fn pack_4bpp_row_all_white() {
        let packed = pack_4bpp(&vec![0u8; WIDTH], WIDTH, false).unwrap();
        assert_eq!(packed.len(), ROW_BYTES_4BPP);
        assert_eq!(packed, vec![0x00; ROW_BYTES_4BPP]);
    }

    #[test]
    fn pack_4bpp_row_all_black() {
        let packed = pack_4bpp(&vec![15u8; WIDTH], WIDTH, false).unwrap();
        assert_eq!(packed, vec![0xFF; ROW_BYTES_4BPP]);
    }

    #[test]
    fn pack_4bpp_even_x_is_high_nibble() {
        let mut row = vec![0u8; WIDTH];
        row[0] = 0xA; // even x → high nibble
        row[1] = 0x3; // odd x → low nibble
        let packed = pack_4bpp(&row, WIDTH, false).unwrap();
        assert_eq!(packed[0], 0xA3);
        assert_eq!(packed[1..].iter().map(|&b| b as u32).sum::<u32>(), 0);
    }

    #[test]
    fn pack_4bpp_maikelchan_formula() {
        // bytes[(y*w+x)>>1] |= level << (((x&1)^1)<<2)
        let mut row = vec![0u8; WIDTH];
        row[0] = 15;
        row[2] = 1;
        let packed = pack_4bpp(&row, WIDTH, false).unwrap();
        let mut expect = vec![0u8; ROW_BYTES_4BPP];
        for (x, &level) in row.iter().enumerate() {
            expect[x >> 1] |= level << (((x & 1) ^ 1) << 2);
        }
        assert_eq!(packed, expect);
    }

    #[test]
    fn pack_4bpp_padding_to_minimum() {
        let rows = vec![0u8; WIDTH * 10];
        let buf = pack_4bpp(&rows, WIDTH, true).unwrap();
        assert_eq!(buf.len(), MIN_DATA_BYTES_4BPP);
        let rows = vec![0u8; WIDTH * 100];
        let buf = pack_4bpp(&rows, WIDTH, true).unwrap();
        assert_eq!(buf.len(), 100 * ROW_BYTES_4BPP);
    }

    #[test]
    fn pack_4bpp_wrong_width_and_clamp() {
        assert_eq!(
            pack_4bpp(&[1, 2, 3], 3, false),
            Err(ProtocolError::BadWidth {
                expected: 384,
                got: 3
            })
        );
        assert_eq!(pack_4bpp(&[], WIDTH, false), Err(ProtocolError::Empty));
        let mut row = vec![0u8; WIDTH];
        row[0] = 200; // clamps to 15
        let packed = pack_4bpp(&row, WIDTH, false).unwrap();
        assert_eq!(packed[0], 0xF0);
    }

    #[test]
    fn gray_to_level_endpoints() {
        assert_eq!(gray_to_level(255), 0);
        assert_eq!(gray_to_level(0), 15);
        assert_eq!(gray_to_level(128), 7);
        assert_eq!(gray_to_level(240), 0);
        assert_eq!(gray_to_level(239), 1);
    }
}
