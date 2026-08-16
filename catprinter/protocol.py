"""MXW01 BLE protocol: frames, CRC8, commands, status, image packing.

The MXW01 is a different animal from the GT01/GB0x printers. Frames start
with 0x22 0x21, image bytes go to characteristic AE03, and CRC-8 is Dallas
Maxim over the payload only. See PROTOCOL.md.
"""

from __future__ import annotations

from dataclasses import dataclass

# --- BLE UUIDs --------------------------------------------------------------

MAIN_SERVICE_UUID = "0000ae30-0000-1000-8000-00805f9b34fb"
MAIN_SERVICE_UUID_ALT = "0000af30-0000-1000-8000-00805f9b34fb"
CONTROL_WRITE_UUID = "0000ae01-0000-1000-8000-00805f9b34fb"
NOTIFY_UUID = "0000ae02-0000-1000-8000-00805f9b34fb"
DATA_WRITE_UUID = "0000ae03-0000-1000-8000-00805f9b34fb"

SERVICE_UUIDS = (MAIN_SERVICE_UUID, MAIN_SERVICE_UUID_ALT)

# --- Printer geometry -------------------------------------------------------

PRINTER_WIDTH_PIXELS = 384
PRINTER_WIDTH_BYTES = PRINTER_WIDTH_PIXELS // 8  # 48  (1 bpp)
PRINTER_WIDTH_NIBBLES = PRINTER_WIDTH_PIXELS // 2  # 192 (4 bpp)
MIN_DATA_BYTES = 90 * PRINTER_WIDTH_BYTES  # 4320
MIN_DATA_BYTES_4BPP = 90 * PRINTER_WIDTH_NIBBLES  # 17280
PRINT_WIDTH = PRINTER_WIDTH_PIXELS  # alias used by the CLI / image pipeline

PREAMBLE = (0x22, 0x21)
FOOTER = 0xFF
DEFAULT_INTENSITY = 0x5D

# --- Command IDs ------------------------------------------------------------


class CommandIDs:
    GET_STATUS = 0xA1
    PRINT_INTENSITY = 0xA2
    EJECT_PAPER = 0xA3
    RETRACT_PAPER = 0xA4
    QUERY_COUNT = 0xA7
    PRINT = 0xA9
    PRINT_COMPLETE = 0xAA
    BATTERY_LEVEL = 0xAB
    CANCEL_PRINT = 0xAC
    PRINT_DATA_FLUSH = 0xAD
    GET_PRINT_TYPE = 0xB0
    GET_VERSION = 0xB1


class PrintModes:
    MONOCHROME = 0x00  # 1 bit per pixel
    GRAYSCALE = 0x02  # 4 bits per pixel


PRINTER_STATES = {
    0x00: "standby",
    0x01: "printing",
    0x02: "feeding",
    0x03: "ejecting",
}

ERROR_CODES = {
    0x01: "no paper",
    0x04: "overheated",
    0x08: "low battery",
    0x09: "no paper",
}

# CRC-8 / Dallas-Maxim, poly 0x07, init 0x00, no reflect, no xor-out.
# fmt: off
_CRC8_TABLE = [
    0x00, 0x07, 0x0E, 0x09, 0x1C, 0x1B, 0x12, 0x15, 0x38, 0x3F, 0x36, 0x31, 0x24, 0x23, 0x2A, 0x2D,
    0x70, 0x77, 0x7E, 0x79, 0x6C, 0x6B, 0x62, 0x65, 0x48, 0x4F, 0x46, 0x41, 0x54, 0x53, 0x5A, 0x5D,
    0xE0, 0xE7, 0xEE, 0xE9, 0xFC, 0xFB, 0xF2, 0xF5, 0xD8, 0xDF, 0xD6, 0xD1, 0xC4, 0xC3, 0xCA, 0xCD,
    0x90, 0x97, 0x9E, 0x99, 0x8C, 0x8B, 0x82, 0x85, 0xA8, 0xAF, 0xA6, 0xA1, 0xB4, 0xB3, 0xBA, 0xBD,
    0xC7, 0xC0, 0xC9, 0xCE, 0xDB, 0xDC, 0xD5, 0xD2, 0xFF, 0xF8, 0xF1, 0xF6, 0xE3, 0xE4, 0xED, 0xEA,
    0xB7, 0xB0, 0xB9, 0xBE, 0xAB, 0xAC, 0xA5, 0xA2, 0x8F, 0x88, 0x81, 0x86, 0x93, 0x94, 0x9D, 0x9A,
    0x27, 0x20, 0x29, 0x2E, 0x3B, 0x3C, 0x35, 0x32, 0x1F, 0x18, 0x11, 0x16, 0x03, 0x04, 0x0D, 0x0A,
    0x57, 0x50, 0x59, 0x5E, 0x4B, 0x4C, 0x45, 0x42, 0x6F, 0x68, 0x61, 0x66, 0x73, 0x74, 0x7D, 0x7A,
    0x89, 0x8E, 0x87, 0x80, 0x95, 0x92, 0x9B, 0x9C, 0xB1, 0xB6, 0xBF, 0xB8, 0xAD, 0xAA, 0xA3, 0xA4,
    0xF9, 0xFE, 0xF7, 0xF0, 0xE5, 0xE2, 0xEB, 0xEC, 0xC1, 0xC6, 0xCF, 0xC8, 0xDD, 0xDA, 0xD3, 0xD4,
    0x69, 0x6E, 0x67, 0x60, 0x75, 0x72, 0x7B, 0x7C, 0x51, 0x56, 0x5F, 0x58, 0x4D, 0x4A, 0x43, 0x44,
    0x19, 0x1E, 0x17, 0x10, 0x05, 0x02, 0x0B, 0x0C, 0x21, 0x26, 0x2F, 0x28, 0x3D, 0x3A, 0x33, 0x34,
    0x4E, 0x49, 0x40, 0x47, 0x52, 0x55, 0x5C, 0x5B, 0x76, 0x71, 0x78, 0x7F, 0x6A, 0x6D, 0x64, 0x63,
    0x3E, 0x39, 0x30, 0x37, 0x22, 0x25, 0x2C, 0x2B, 0x06, 0x01, 0x08, 0x0F, 0x1A, 0x1D, 0x14, 0x13,
    0xAE, 0xA9, 0xA0, 0xA7, 0xB2, 0xB5, 0xBC, 0xBB, 0x96, 0x91, 0x98, 0x9F, 0x8A, 0x8D, 0x84, 0x83,
    0xDE, 0xD9, 0xD0, 0xD7, 0xC2, 0xC5, 0xCC, 0xCB, 0xE6, 0xE1, 0xE8, 0xEF, 0xFA, 0xFD, 0xF4, 0xF3,
]
# fmt: on


def crc8(data: bytes) -> int:
    """CRC-8 Dallas/Maxim over `data` only (not the header)."""
    crc = 0
    for byte in data:
        crc = _CRC8_TABLE[crc ^ byte]
    return crc


def make_command(command_id: int, payload: bytes) -> bytes:
    """Build a complete AE01 control packet."""
    if len(payload) > 0xFFFF:
        raise ValueError(f"payload too large: {len(payload)} bytes")
    header = bytes(
        [
            PREAMBLE[0],
            PREAMBLE[1],
            command_id & 0xFF,
            0x00,
            len(payload) & 0xFF,
            (len(payload) >> 8) & 0xFF,
        ]
    )
    return header + payload + bytes([crc8(payload), FOOTER])


def parse_notification(data: bytes) -> tuple[int, bytes] | None:
    """Return (command_id, payload) or None if this is not an MXW01 frame."""
    if len(data) < 6 or data[0] != PREAMBLE[0] or data[1] != PREAMBLE[1]:
        return None
    cmd_id = data[2]
    payload_len = int.from_bytes(data[4:6], "little")
    end = 6 + payload_len
    if len(data) < end:
        return None
    return cmd_id, bytes(data[6:end])


def cmd_get_status() -> bytes:
    return make_command(CommandIDs.GET_STATUS, b"\x00")


def cmd_set_intensity(intensity: int) -> bytes:
    intensity = max(0, min(255, intensity))
    return make_command(CommandIDs.PRINT_INTENSITY, bytes([intensity]))


def cmd_print_request(line_count: int, mode: int = PrintModes.MONOCHROME) -> bytes:
    payload = line_count.to_bytes(2, "little") + bytes([0x30, mode & 0xFF])
    return make_command(CommandIDs.PRINT, payload)


def cmd_flush() -> bytes:
    return make_command(CommandIDs.PRINT_DATA_FLUSH, b"\x00")


def cmd_eject(line_count: int) -> bytes:
    return make_command(CommandIDs.EJECT_PAPER, line_count.to_bytes(2, "little"))


def cmd_retract(line_count: int) -> bytes:
    return make_command(CommandIDs.RETRACT_PAPER, line_count.to_bytes(2, "little"))


def cmd_get_battery() -> bytes:
    return make_command(CommandIDs.BATTERY_LEVEL, b"\x00")


def cmd_get_version() -> bytes:
    return make_command(CommandIDs.GET_VERSION, b"\x00")


@dataclass(frozen=True)
class PrinterStatus:
    ok: bool
    state: str
    state_code: int
    battery: int | None
    temperature: int | None
    error: str | None
    raw: bytes

    def kid_message(self) -> str:
        if self.ok:
            batt = f", battery {self.battery}%" if self.battery is not None else ""
            return f"Printer ready ({self.state}{batt})"
        if self.error == "no paper":
            return "The cat printer is out of paper."
        if self.error == "overheated":
            return "The cat printer is too hot. Give it a minute."
        if self.error == "low battery":
            return "The cat printer battery is low. Charge it."
        return "The cat printer is not ready. Turn it on and check the paper."


def parse_status(payload: bytes) -> PrinterStatus:
    """Parse an A1 status payload.

    Indices are payload-relative and match MaikelChan/CatPrinterBLE, which
    indexes the full GATT notification (payload starts at byte 6):

      payload[0]  state (0 standby / 1 printing / 2 feeding / 3 ejecting)
      payload[3]  battery percent
      payload[4]  temperature
      payload[6]  0 = ok, nonzero = error
      payload[7]  error code (1/9 no paper, 4 overheat, 8 low battery)
    """
    if len(payload) < 7:
        return PrinterStatus(
            ok=False,
            state="unknown",
            state_code=-1,
            battery=None,
            temperature=None,
            error="short status payload",
            raw=payload,
        )
    state_code = payload[0]
    battery = payload[3] if len(payload) > 3 else None
    temperature = payload[4] if len(payload) > 4 else None
    ok = payload[6] == 0
    error = None
    if not ok and len(payload) > 7:
        error = ERROR_CODES.get(payload[7], f"error 0x{payload[7]:02X}")
    return PrinterStatus(
        ok=ok,
        state=PRINTER_STATES.get(state_code, f"0x{state_code:02X}"),
        state_code=state_code,
        battery=battery,
        temperature=temperature,
        error=error,
        raw=payload,
    )


def encode_1bpp_row(row) -> bytes:
    """Pack 384 boolean pixels (True=black) into 48 bytes, LSB = leftmost pixel."""
    if len(row) != PRINTER_WIDTH_PIXELS:
        raise ValueError(
            f"row length must be {PRINTER_WIDTH_PIXELS}, got {len(row)}"
        )
    return prepare_image_data([row], pad=False)


def bytes_per_row(mode: int = PrintModes.MONOCHROME) -> int:
    if mode == PrintModes.GRAYSCALE:
        return PRINTER_WIDTH_NIBBLES
    return PRINTER_WIDTH_BYTES


def min_data_bytes(mode: int = PrintModes.MONOCHROME) -> int:
    if mode == PrintModes.GRAYSCALE:
        return MIN_DATA_BYTES_4BPP
    return MIN_DATA_BYTES


def data_chunk_size(
    max_att_bytes: int,
    slow: bool = False,
    row_bytes: int = PRINTER_WIDTH_BYTES,
) -> int:
    """AE03 write size: whole rows only, as large as the ATT MTU allows."""
    if row_bytes < 1:
        raise ValueError("row_bytes must be positive")
    if slow or max_att_bytes < row_bytes:
        return row_bytes
    return (max_att_bytes // row_bytes) * row_bytes


def prepare_image_data(rows, pad: bool = True) -> bytes:
    """Encode a 2D True=black array. Pad to MIN_DATA_BYTES unless pad=False."""
    try:
        import numpy as np
    except ImportError:
        np = None

    if np is not None:
        arr = np.asarray(rows, dtype=bool)
        if arr.ndim == 1:
            arr = arr.reshape(1, -1)
        if arr.size == 0:
            raise ValueError("image has no rows")
        height, width = arr.shape
        if width != PRINTER_WIDTH_PIXELS:
            raise ValueError(
                f"row length must be {PRINTER_WIDTH_PIXELS}, got {width}"
            )
        # packbits is MSB-first; reverse each 8-pixel group so LSB is leftmost.
        grouped = arr.astype(np.uint8).reshape(height, PRINTER_WIDTH_BYTES, 8)
        packed = np.packbits(grouped[:, :, ::-1], axis=2).reshape(height, PRINTER_WIDTH_BYTES)
        buf = packed.tobytes()
    else:
        height = len(rows)
        if height == 0:
            raise ValueError("image has no rows")
        out = bytearray()
        for y in range(height):
            row = rows[y]
            if len(row) != PRINTER_WIDTH_PIXELS:
                raise ValueError(
                    f"row length must be {PRINTER_WIDTH_PIXELS}, got {len(row)}"
                )
            for byte_idx in range(PRINTER_WIDTH_BYTES):
                value = 0
                base = byte_idx * 8
                for bit in range(8):
                    if row[base + bit]:
                        value |= 1 << bit
                out.append(value)
        buf = bytes(out)

    if pad and len(buf) < MIN_DATA_BYTES:
        buf += b"\x00" * (MIN_DATA_BYTES - len(buf))
    return buf


def prepare_image_data_4bpp(levels, pad: bool = True) -> bytes:
    """Pack a 384-wide array of levels (0=white .. 15=black) into 4 bpp rows.

    Even x is the high nibble, odd x the low nibble — MaikelChan's MXW01
    grayscale encoding. Pad to MIN_DATA_BYTES_4BPP unless pad=False.
    """
    try:
        import numpy as np
    except ImportError:
        np = None

    if np is not None:
        arr = np.asarray(levels, dtype=np.uint8)
        if arr.ndim == 1:
            arr = arr.reshape(1, -1)
        if arr.size == 0:
            raise ValueError("image has no rows")
        height, width = arr.shape
        if width != PRINTER_WIDTH_PIXELS:
            raise ValueError(
                f"row length must be {PRINTER_WIDTH_PIXELS}, got {width}"
            )
        arr = np.clip(arr, 0, 15)
        packed = ((arr[:, 0::2] & 0x0F) << 4) | (arr[:, 1::2] & 0x0F)
        buf = packed.astype(np.uint8).tobytes()
    else:
        height = len(levels)
        if height == 0:
            raise ValueError("image has no rows")
        out = bytearray()
        for y in range(height):
            row = levels[y]
            if len(row) != PRINTER_WIDTH_PIXELS:
                raise ValueError(
                    f"row length must be {PRINTER_WIDTH_PIXELS}, got {len(row)}"
                )
            for x in range(0, PRINTER_WIDTH_PIXELS, 2):
                hi = int(row[x]) & 0x0F
                lo = int(row[x + 1]) & 0x0F
                out.append((hi << 4) | lo)
        buf = bytes(out)

    if pad and len(buf) < MIN_DATA_BYTES_4BPP:
        buf += b"\x00" * (MIN_DATA_BYTES_4BPP - len(buf))
    return buf
