//! Classic cat-printer family (GB01/GB02/GB03/GT01/MX05/MX06/MX08/MX09/MX10/MX11/YT01/X5/X6).
//!
//! Byte-exact port of upstream rbaron/catprinter `catprinter/cmds.py` (+ constants from its
//! `ble.py`). Frames start with `0x51 0x78`; everything — commands *and* image rows — is written to
//! characteristic AE01, notifications arrive on AE02. The checksum is the same CRC-8/Dallas-Maxim
//! table as the MXW01, computed over the payload only.
//!
//! Frame layout: `51 78 cmd 00 len_le(2) payload crc8(payload) FF`.

use crate::protocol::crc8;

// --- Constants -----------------------------------------------------------------------------------

/// Every classic frame starts with these two bytes.
pub const PREAMBLE: [u8; 2] = [0x51, 0x78];
/// Every classic frame ends with this byte.
pub const FOOTER: u8 = 0xFF;
/// The printer sends this on AE02 when it has finished printing and is ready again
/// (note byte 3 is `0x01` here, unlike the `0x00` we put in frames we build).
pub const READY_NOTIFICATION: [u8; 9] = [0x51, 0x78, 0xAE, 0x01, 0x01, 0x00, 0x00, 0x00, 0xFF];
/// Dots across the print head.
pub const WIDTH: usize = 384;
/// Bytes per raw (uncompressed) row.
pub const ROW_BYTES: usize = WIDTH / 8; // 48
/// Milliseconds to wait after each BLE chunk (upstream `WAIT_AFTER_EACH_CHUNK_S = 0.02`).
pub const WAIT_AFTER_CHUNK_MS: u64 = 20;
/// Seconds to wait for the ready notification after the stream (upstream `WAIT_FOR_PRINTER_DONE_TIMEOUT`).
pub const WAIT_FOR_READY_TIMEOUT_S: u64 = 30;
/// Default burn energy (upstream `cmds_print_img(img, energy=0xffff)`).
pub const DEFAULT_ENERGY: u16 = 0xFFFF;
/// Rows fed after the image (upstream `cmd_feed_paper(25)`).
pub const DEFAULT_FEED_AFTER: u8 = 25;

/// Command identifiers (byte 2 of a frame).
pub mod cmd {
    /// A1 — set paper (payload `30 00`).
    pub const SET_PAPER: u8 = 0xA1;
    /// A2 — raw bitmap row (48 bytes, LSB-first).
    pub const DRAW_BITMAP: u8 = 0xA2;
    /// A3 — get device state.
    pub const GET_DEV_STATE: u8 = 0xA3;
    /// A4 — set quality (`0x32` = 200 dpi).
    pub const SET_QUALITY: u8 = 0xA4;
    /// A6 — control lattice (start / end).
    pub const LATTICE: u8 = 0xA6;
    /// A8 — get device info.
    pub const GET_DEV_INFO: u8 = 0xA8;
    /// AE — notification: printer ready (see [`super::READY_NOTIFICATION`]).
    pub const READY: u8 = 0xAE;
    /// AF — set energy (u16 big-endian).
    pub const SET_ENERGY: u8 = 0xAF;
    /// BD — feed paper (u8 lines).
    pub const FEED_PAPER: u8 = 0xBD;
    /// BE — draw mode (0 image / 1 text) — also used as "apply energy".
    pub const DRAW_MODE: u8 = 0xBE;
    /// BF — run-length-encoded bitmap row.
    pub const DRAW_BITMAP_RLE: u8 = 0xBF;
}

// --- Framing -------------------------------------------------------------------------------------

/// Build a classic frame: `51 78 cmd 00 len_le(2) payload crc8(payload) FF`.
/// Payloads are ≤ 48 bytes in practice; anything up to `u16::MAX` is encodable.
pub fn make_frame(cmd: u8, payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() <= 0xFFFF, "classic payload too large");
    let len = payload.len() as u16;
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&PREAMBLE);
    out.push(cmd);
    out.push(0x00);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(payload);
    out.push(crc8(payload));
    out.push(FOOTER);
    out
}

/// Parse a classic frame → `(cmd, payload)`; `None` if the preamble is wrong or the payload is
/// truncated. Trailing bytes (CRC, footer) are not validated, mirroring the MXW01 parser.
pub fn parse_frame(data: &[u8]) -> Option<(u8, Vec<u8>)> {
    if data.len() < 6 || data[0] != PREAMBLE[0] || data[1] != PREAMBLE[1] {
        return None;
    }
    let cmd = data[2];
    let payload_len = u16::from_le_bytes([data[4], data[5]]) as usize;
    let end = 6 + payload_len;
    if data.len() < end {
        return None;
    }
    Some((cmd, data[6..end].to_vec()))
}

/// Is this notification the "done printing, ready again" ping?
pub fn is_ready_notification(data: &[u8]) -> bool {
    data == READY_NOTIFICATION
}

// --- Commands (exact bytes of upstream cmds.py) ----------------------------------------------------

/// `CMD_GET_DEV_STATE` — A3, payload `00`.
pub fn cmd_get_dev_state() -> Vec<u8> {
    make_frame(cmd::GET_DEV_STATE, &[0x00])
}

/// `CMD_SET_QUALITY_200_DPI` — A4, payload `32`.
pub fn cmd_set_quality_200dpi() -> Vec<u8> {
    make_frame(cmd::SET_QUALITY, &[0x32])
}

/// `CMD_GET_DEV_INFO` — A8, payload `00`.
pub fn cmd_get_dev_info() -> Vec<u8> {
    make_frame(cmd::GET_DEV_INFO, &[0x00])
}

/// `CMD_LATTICE_START` — A6, 11 bytes `AA 55 17 38 44 5F 5F 5F 44 38 2C`.
pub fn cmd_lattice_start() -> Vec<u8> {
    make_frame(
        cmd::LATTICE,
        &[
            0xAA, 0x55, 0x17, 0x38, 0x44, 0x5F, 0x5F, 0x5F, 0x44, 0x38, 0x2C,
        ],
    )
}

/// `CMD_LATTICE_END` — A6, 11 bytes `AA 55 17 00 00 00 00 00 00 00 17`.
pub fn cmd_lattice_end() -> Vec<u8> {
    make_frame(
        cmd::LATTICE,
        &[
            0xAA, 0x55, 0x17, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x17,
        ],
    )
}

/// `CMD_SET_PAPER` — A1, payload `30 00`.
pub fn cmd_set_paper() -> Vec<u8> {
    make_frame(cmd::SET_PAPER, &[0x30, 0x00])
}

/// `CMD_PRINT_IMG` — BE, payload `00` (image draw mode).
pub fn cmd_print_img_mode() -> Vec<u8> {
    make_frame(cmd::DRAW_MODE, &[0x00])
}

/// `CMD_PRINT_TEXT` — BE, payload `01` (text draw mode).
pub fn cmd_print_text_mode() -> Vec<u8> {
    make_frame(cmd::DRAW_MODE, &[0x01])
}

/// `cmd_feed_paper(n)` — BD, feed `n` lines.
pub fn cmd_feed_paper(lines: u8) -> Vec<u8> {
    make_frame(cmd::FEED_PAPER, &[lines])
}

/// `cmd_set_energy(v)` — AF, u16 big-endian energy.
pub fn cmd_set_energy(energy: u16) -> Vec<u8> {
    make_frame(cmd::SET_ENERGY, &energy.to_be_bytes())
}

/// `cmd_apply_energy()` — BE, payload `01` (same bytes as the text draw mode).
pub fn cmd_apply_energy() -> Vec<u8> {
    make_frame(cmd::DRAW_MODE, &[0x01])
}

// --- Row encoding ---------------------------------------------------------------------------------

/// Upstream `encode_run_length_repetition`: a run of `n` pixels of `val` as 7-bit counts with the
/// pixel value in the top bit; runs longer than 127 are split.
pub fn encode_run_length_repetition(mut n: usize, val: bool) -> Vec<u8> {
    let v = (val as u8) << 7;
    let mut res = Vec::new();
    while n > 0x7F {
        res.push(0x7F | v);
        n -= 0x7F;
    }
    if n > 0 {
        res.push(v | n as u8);
    }
    res
}

/// Upstream `run_length_encode`: RLE a whole row (`true` = black).
pub fn run_length_encode(row: &[bool]) -> Vec<u8> {
    let mut res = Vec::new();
    let mut count = 0usize;
    let mut last: Option<bool> = None;
    for &px in row {
        if Some(px) == last {
            count += 1;
        } else {
            if let Some(l) = last {
                res.extend(encode_run_length_repetition(count, l));
            }
            count = 1;
            last = Some(px);
        }
    }
    if let (Some(l), true) = (last, count > 0) {
        res.extend(encode_run_length_repetition(count, l));
    }
    res
}

/// Upstream `byte_encode`: pack a row 8 pixels per byte, LSB = leftmost pixel of each group.
/// A trailing partial group is packed into its own byte.
pub fn byte_encode(row: &[bool]) -> Vec<u8> {
    row.chunks(8)
        .map(|group| {
            group.iter().enumerate().fold(
                0u8,
                |acc, (bit, &px)| if px { acc | (1 << bit) } else { acc },
            )
        })
        .collect()
}

/// Upstream `cmd_print_row`: BF with RLE data when that fits in `WIDTH / 8` (48) bytes, else A2 with
/// the raw 48-byte bitmap.
pub fn cmd_print_row(row: &[bool]) -> Vec<u8> {
    let encoded = run_length_encode(row);
    if encoded.len() > WIDTH / 8 {
        make_frame(cmd::DRAW_BITMAP, &byte_encode(row))
    } else {
        make_frame(cmd::DRAW_BITMAP_RLE, &encoded)
    }
}

/// Upstream `cmds_print_img`: the complete AE01 byte stream for one image.
///
/// `rows` yields one row of `WIDTH` booleans (`true` = black) per item. Order:
/// get_dev_state, set_quality_200dpi, set_energy, apply_energy, lattice_start, rows…,
/// feed_paper(`feed_after`), set_paper ×3, lattice_end, get_dev_state.
pub fn print_stream<I, R>(rows: I, energy: u16, feed_after: u8) -> Vec<u8>
where
    I: IntoIterator<Item = R>,
    R: AsRef<[bool]>,
{
    let mut data = Vec::new();
    data.extend(cmd_get_dev_state());
    data.extend(cmd_set_quality_200dpi());
    data.extend(cmd_set_energy(energy));
    data.extend(cmd_apply_energy());
    data.extend(cmd_lattice_start());
    for row in rows {
        data.extend(cmd_print_row(row.as_ref()));
    }
    data.extend(cmd_feed_paper(feed_after));
    data.extend(cmd_set_paper());
    data.extend(cmd_set_paper());
    data.extend(cmd_set_paper());
    data.extend(cmd_lattice_end());
    data.extend(cmd_get_dev_state());
    data
}

/// Convenience over a flat `height × width` bitmap (row-major, `true` = black).
pub fn print_stream_from_bitmap(
    bits: &[bool],
    width: usize,
    energy: u16,
    feed_after: u8,
) -> Vec<u8> {
    let rows = if width == 0 {
        Vec::new()
    } else {
        bits.chunks_exact(width).collect::<Vec<_>>()
    };
    print_stream(rows, energy, feed_after)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // Reference hex produced by running upstream cmds.py (git show 20fe5b7:catprinter/cmds.py).
    const GET_DEV_STATE: &str = "5178a30001000000ff";
    const SET_QUALITY_200_DPI: &str = "5178a4000100329eff";
    const GET_DEV_INFO: &str = "5178a80001000000ff";
    const LATTICE_START: &str = "5178a6000b00aa551738445f5f5f44382ca1ff";
    const LATTICE_END: &str = "5178a6000b00aa5517000000000000001711ff";
    const SET_PAPER: &str = "5178a10002003000f9ff";
    const PRINT_IMG: &str = "5178be0001000000ff";
    const PRINT_TEXT: &str = "5178be0001000107ff";
    const FEED_25: &str = "5178bd000100194fff";
    const FEED_40: &str = "5178bd00010028d8ff";
    const ENERGY_FFFF: &str = "5178af000200ffff24ff";
    const ENERGY_1234: &str = "5178af0002001234f1ff";
    const APPLY_ENERGY: &str = "5178be0001000107ff";
    const ROW_ALL_BLACK: &str = "5178bf000400ffffff83adff";
    const ROW_ALT: &str = "5178a2003000555555555555555555555555555555555555555555555555555555555555555555555555555555555555555555555555a5ff";
    const ROW_ALL_WHITE: &str = "5178bf0004007f7f7f03a8ff";
    const ROW_0_7_8: &str = "5178bf0006008106827f7f79baff";
    const ROW_200B_184W: &str = "5178bf000400ffc97f39a8ff";
    const STREAM_2ROWS: &str = "5178a30001000000ff5178a4000100329eff5178af000200ffff24ff5178be0001000107ff5178a6000b00aa551738445f5f5f44382ca1ff5178bf000400ffffff83adff5178bf0004007f7f7f03a8ff5178bd000100194fff5178a10002003000f9ff5178a10002003000f9ff5178a10002003000f9ff5178a6000b00aa5517000000000000001711ff5178a30001000000ff";
    const STREAM_2ROWS_ENERGY_1234: &str = "5178a30001000000ff5178a4000100329eff5178af0002001234f1ff5178be0001000107ff5178a6000b00aa551738445f5f5f44382ca1ff5178bf000400ffffff83adff5178bf0004007f7f7f03a8ff5178bd000100194fff5178a10002003000f9ff5178a10002003000f9ff5178a10002003000f9ff5178a6000b00aa5517000000000000001711ff5178a30001000000ff";

    #[test]
    fn fixed_commands_match_upstream() {
        assert_eq!(hex(&cmd_get_dev_state()), GET_DEV_STATE);
        assert_eq!(hex(&cmd_set_quality_200dpi()), SET_QUALITY_200_DPI);
        assert_eq!(hex(&cmd_get_dev_info()), GET_DEV_INFO);
        assert_eq!(hex(&cmd_lattice_start()), LATTICE_START);
        assert_eq!(hex(&cmd_lattice_end()), LATTICE_END);
        assert_eq!(hex(&cmd_set_paper()), SET_PAPER);
        assert_eq!(hex(&cmd_print_img_mode()), PRINT_IMG);
        assert_eq!(hex(&cmd_print_text_mode()), PRINT_TEXT);
        assert_eq!(hex(&cmd_apply_energy()), APPLY_ENERGY);
    }

    #[test]
    fn parameterised_commands_match_upstream() {
        assert_eq!(hex(&cmd_feed_paper(25)), FEED_25);
        assert_eq!(hex(&cmd_feed_paper(40)), FEED_40);
        assert_eq!(hex(&cmd_set_energy(0xFFFF)), ENERGY_FFFF);
        assert_eq!(hex(&cmd_set_energy(0x1234)), ENERGY_1234);
        assert_eq!(hex(&cmd_set_energy(DEFAULT_ENERGY)), ENERGY_FFFF);
    }

    #[test]
    fn frame_layout() {
        let f = make_frame(0xA3, &[0x00]);
        assert_eq!(f[0], 0x51);
        assert_eq!(f[1], 0x78);
        assert_eq!(f[2], 0xA3);
        assert_eq!(f[3], 0x00);
        assert_eq!(u16::from_le_bytes([f[4], f[5]]), 1);
        assert_eq!(f[6], 0x00);
        assert_eq!(f[7], crc8(&[0x00]));
        assert_eq!(f[8], 0xFF);
        // Length is little-endian u16: a 300-byte payload → 2C 01.
        let f = make_frame(0xA2, &[0u8; 300]);
        assert_eq!(&f[4..6], &[0x2C, 0x01]);
        assert_eq!(f.len(), 300 + 8);
    }

    #[test]
    fn parse_frame_roundtrip_and_rejects() {
        let (c, p) = parse_frame(&cmd_set_energy(0x1234)).unwrap();
        assert_eq!(c, cmd::SET_ENERGY);
        assert_eq!(p, vec![0x12, 0x34]);
        let (c, p) = parse_frame(&READY_NOTIFICATION).unwrap();
        assert_eq!(c, cmd::READY);
        // byte 3 is 0x01 in this notification; the declared 1-byte payload is 0x00
        assert_eq!(p, vec![0x00]);
        // MXW01 frame is not a classic frame.
        assert!(parse_frame(&[0x22, 0x21, 0xA1, 0x00, 0x01, 0x00, 0x00, 0x00, 0xFF]).is_none());
        assert!(parse_frame(&[0x51, 0x78, 0xA1, 0x00, 0x05, 0x00, 0x00]).is_none());
        assert!(parse_frame(b"hi").is_none());
    }

    #[test]
    fn ready_notification() {
        assert!(is_ready_notification(&READY_NOTIFICATION));
        assert_eq!(hex(&READY_NOTIFICATION), "5178ae0101000000ff");
        assert!(!is_ready_notification(&cmd_get_dev_state()));
        assert!(!is_ready_notification(&READY_NOTIFICATION[..8]));
    }

    #[test]
    fn rle_matches_upstream() {
        assert_eq!(run_length_encode(&[true; 384]), vec![255, 255, 255, 131]);
        let mut row = vec![true; 200];
        row.extend(vec![false; 184]);
        assert_eq!(run_length_encode(&row), vec![255, 201, 127, 57]);
        assert_eq!(run_length_encode(&[]), Vec::<u8>::new());
        assert_eq!(
            run_length_encode(&[false; 384]),
            vec![0x7F, 0x7F, 0x7F, 0x03]
        );
        assert_eq!(encode_run_length_repetition(0, true), Vec::<u8>::new());
        assert_eq!(encode_run_length_repetition(127, false), vec![0x7F]);
        assert_eq!(encode_run_length_repetition(128, true), vec![0xFF, 0x81]);
    }

    #[test]
    fn byte_encode_lsb_first() {
        let mut row = vec![false; 384];
        row[0] = true;
        row[7] = true;
        row[8] = true;
        let b = byte_encode(&row);
        assert_eq!(b.len(), 48);
        assert_eq!(b[0], 0x81);
        assert_eq!(b[1], 0x01);
        assert_eq!(b[2..].iter().map(|&x| x as u32).sum::<u32>(), 0);
        let alt: Vec<bool> = (0..384).map(|i| i % 2 == 0).collect();
        assert_eq!(byte_encode(&alt), vec![0x55; 48]);
        // Partial trailing group gets its own byte.
        assert_eq!(byte_encode(&[true, false, true]), vec![0x05]);
    }

    #[test]
    fn print_row_matches_upstream() {
        assert_eq!(hex(&cmd_print_row(&[true; 384])), ROW_ALL_BLACK);
        assert_eq!(hex(&cmd_print_row(&[false; 384])), ROW_ALL_WHITE);
        let alt: Vec<bool> = (0..384).map(|i| i % 2 == 0).collect();
        assert_eq!(hex(&cmd_print_row(&alt)), ROW_ALT);
        let mut row = vec![false; 384];
        row[0] = true;
        row[7] = true;
        row[8] = true;
        assert_eq!(hex(&cmd_print_row(&row)), ROW_0_7_8);
        let mut row = vec![true; 200];
        row.extend(vec![false; 184]);
        assert_eq!(hex(&cmd_print_row(&row)), ROW_200B_184W);
    }

    #[test]
    fn print_row_picks_raw_when_rle_is_longer_than_48() {
        // 96 alternating runs of 4 → 96 RLE bytes > 48 → raw A2 with 48 bytes.
        let row: Vec<bool> = (0..384).map(|i| (i / 4) % 2 == 0).collect();
        let f = cmd_print_row(&row);
        assert_eq!(f[2], cmd::DRAW_BITMAP);
        assert_eq!(u16::from_le_bytes([f[4], f[5]]), 48);
        assert_eq!(f.len(), 48 + 8);
        // 12 runs of 32 → 12 RLE bytes ≤ 48 → BF.
        let row: Vec<bool> = (0..384).map(|i| (i / 32) % 2 == 0).collect();
        let f = cmd_print_row(&row);
        assert_eq!(f[2], cmd::DRAW_BITMAP_RLE);
        assert_eq!(u16::from_le_bytes([f[4], f[5]]), 12);
    }

    #[test]
    fn print_stream_matches_upstream() {
        let rows = [vec![true; 384], vec![false; 384]];
        assert_eq!(
            hex(&print_stream(
                rows.iter(),
                DEFAULT_ENERGY,
                DEFAULT_FEED_AFTER
            )),
            STREAM_2ROWS
        );
        assert_eq!(
            hex(&print_stream(rows.iter(), 0x1234, DEFAULT_FEED_AFTER)),
            STREAM_2ROWS_ENERGY_1234
        );
        let mut bits = vec![true; 384];
        bits.extend(vec![false; 384]);
        assert_eq!(
            hex(&print_stream_from_bitmap(
                &bits,
                384,
                DEFAULT_ENERGY,
                DEFAULT_FEED_AFTER
            )),
            STREAM_2ROWS
        );
        assert_eq!(
            print_stream_from_bitmap(&bits, 384, DEFAULT_ENERGY, DEFAULT_FEED_AFTER),
            unhex(STREAM_2ROWS)
        );
    }

    #[test]
    fn print_stream_structure() {
        // Empty image: still the full envelope, no row frames.
        let s = print_stream(Vec::<Vec<bool>>::new(), DEFAULT_ENERGY, 25);
        let expect = [
            hex(&cmd_get_dev_state()),
            hex(&cmd_set_quality_200dpi()),
            hex(&cmd_set_energy(0xFFFF)),
            hex(&cmd_apply_energy()),
            hex(&cmd_lattice_start()),
            hex(&cmd_feed_paper(25)),
            hex(&cmd_set_paper()),
            hex(&cmd_set_paper()),
            hex(&cmd_set_paper()),
            hex(&cmd_lattice_end()),
            hex(&cmd_get_dev_state()),
        ]
        .concat();
        assert_eq!(hex(&s), expect);
    }
}
