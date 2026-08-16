# MXW01 Cat Printer BLE Protocol Specification

This is a questionably-accurate and probably incomplete document describing the
MXW01's BLE protocol. Please help update it if you have more information!

Sourced from [jeremy46231/MXW01-catprinter](https://github.com/jeremy46231/MXW01-catprinter/blob/main/PROTOCOL.md)
and cross-checked against [MaikelChan/CatPrinterBLE](https://github.com/MaikelChan/CatPrinterBLE).

## Overview

When it is on, the printer exposes one BLE service with three characteristics.

- **Main service**:`0000ae30-0000-1000-8000-00805f9b34fb`
  - **`AE01`:** Control characteristic (`0000ae01-0000-1000-8000-00805f9b34fb`)
    - Type: Write without response
    - Purpose: Sending control commands (Status Request, Print Request, Set
      Intensity, Flush, etc.).
  - **`AE02`:** Notify characteristic (`0000ae02-0000-1000-8000-00805f9b34fb`)
    - Type: Notify
    - Purpose: Receiving status responses, acknowledgments, and print completion
      notifications from the printer.
  - **`AE03`:** Data characteristic (`0000ae03-0000-1000-8000-00805f9b34fb`)
    - Type: Write without response
    - Purpose: Sending bulk image data to the printer.

The same service sometimes shows up as `0000af30-...` instead, particularly on Macs.

All messages sent to the printer begin with a `0x22 0x21` preamble.

## `AE01` Control Packet Structure

| Field           | Length (Bytes) | Value               | Description                                     |
| :-------------- | :------------- | :------------------ | :---------------------------------------------- |
| Preamble        | 2              | `0x22 0x21`         |                                                 |
| Command ID      | 1              | Command ID          | See the Command Reference                       |
| Fixed (unknown) | 1              | `0x00`              | Appears fixed, unknown purpose                  |
| Length (LE)     | 2              | `0x0000` - `0xFFFF` | Length of the `Payload` field, Little Endian    |
| Payload         | Variable       | Command-specific    |                                                 |
| CRC8            | 1              | `0x00` - `0xFF`     | CRC checksum (payload only)                     |
| Footer          | 1              | `0xFF`              |                                                 |

### CRC Calculation

- **Algorithm:** CRC-8 / DALLAS-MAXIM
- Polynomial `0x07`, init `0x00`, no reflect, no xor-out
- Scope: payload bytes only

## Command Reference

| ID   | Name                | Direction | Notes |
| :--- | :------------------ | :-------- | :---- |
| `A1` | Get Status          | both      | paper / temp / battery / state |
| `A2` | Set Print Intensity | send      | one byte, `0x5D` is a good default |
| `A3` | Eject Paper         | send      | `line_count` little-endian u16 |
| `A4` | Retract Paper       | send      | `line_count` little-endian u16 |
| `A9` | Print Request       | both      | `line_count_le(2)`, `0x30`, mode (`0`=1bpp, `2`=4bpp) |
| `AA` | Print Complete      | receive   | physical print finished |
| `AB` | Battery Level       | both      | |
| `AD` | Print Data Flush    | send      | end of AE03 transfer |
| `B0` | Get Print Type      | both      | |
| `B1` | Get Version         | both      | |

### `A1` Status Payload (payload-relative)

Matches MaikelChan (who indexes the full GATT notification; payload starts at byte 6):

- `[0]` state: 0 standby, 1 printing, 2 feeding, 3 ejecting
- `[3]` battery percent
- `[4]` temperature
- `[6]` 0 = ok, nonzero = error
- `[7]` error: 1/9 no paper, 4 overheated, 8 low battery

## Print Sequence

1. Connect, find AE30 / AE01+AE02+AE03, enable notify on AE02
2. `A2` set intensity
3. `A1` get status; abort if not ready
4. `A9` print request with line count; wait for ACK `00`
5. Stream packed 384-wide rows to **AE03** (pacing `--pacing-ms`, default 8 ms per bulk write)
6. `AD` flush
7. Wait for `AA`
8. Disconnect (single BLE connection — do not hold the link idle)

## Image Data Encoding

### 1 bpp (A9 mode `0x00`)

- 384 pixels wide, black = 1, white = 0
- Each row → 48 bytes, LSB = leftmost pixel of each 8-pixel group
- Pad the concatenated buffer with `0x00` to at least 4320 bytes (90 lines)

### 4 bpp (A9 mode `0x02`)

Real grayscale. Sixteen burn levels per dot, not dithered 1-bit.

- 384 pixels wide, **192 bytes per row** (`384 / 2`)
- Level `0` = white, `15` = black
- `level = (255 - gray) >> 4`
- Even `x` is the high nibble, odd `x` the low nibble:

```
bytes[(y * 384 + x) >> 1] |= level << (((x & 1) ^ 1) << 2)
```

- Pad to at least 17280 bytes (90 lines × 192)
- A9 `line_count` is still the pixel-row count, not a byte count
- AE03 writes must be a whole number of 192-byte rows (384 bytes / 2 rows is a good ATT-sized chunk)

4 bpp was reverse-engineered by [MaikelChan/CatPrinterBLE](https://github.com/MaikelChan/CatPrinterBLE).

---

# Classic family (GB01 / GB02 / GB03 / GT01 / MX05 / MX06 / MX08 / MX09 / MX10 / MX11 / YT01 / X5 / X6)

The older cat printers share the same GATT service (`AE30`, sometimes `AF30`) but speak a different
dialect, reverse engineered by the community and implemented in
[rbaron/catprinter](https://github.com/rbaron/catprinter) (`cmds.py`), which `catprinterd` ports
byte-for-byte in `src/protocol/classic.rs`.

* **Characteristics:** `AE01` write (control **and** image rows), `AE02` notify. There is no `AE03`;
  `catprinterd` uses "AE03 present ⇒ MXW01, else classic" when the advertised name is unknown.
* **Frame:** `51 78 <cmd> 00 <len lo> <len hi> <payload> <crc8(payload)> FF` — same CRC-8/Dallas table
  as the MXW01, computed over the payload only.

| Cmd | Name | Payload |
|-----|------|---------|
| `A1` | Set paper | `30 00` |
| `A2` | Draw bitmap row (raw) | 48 bytes, LSB = leftmost pixel, 1 = black |
| `A3` | Get device state | `00` |
| `A4` | Set quality (200 dpi) | `32` |
| `A6` | Control lattice | start `AA 55 17 38 44 5F 5F 5F 44 38 2C`, end `AA 55 17 00 00 00 00 00 00 00 17` |
| `A8` | Get device info | `00` |
| `AF` | Set energy | `u16` big-endian (upstream default `FFFF`) |
| `BD` | Feed paper | `u8` lines |
| `BE` | Draw mode / apply energy | `00` image, `01` text (also sent as "apply energy") |
| `BF` | Draw bitmap row (RLE) | 7-bit run lengths, bit 7 = colour; used when ≤ 48 bytes |

* **Print stream** (`cmds_print_img`): get-state · set-quality · set-energy · apply-energy ·
  lattice-start · one `A2`/`BF` frame per row · feed 25 · set-paper ×3 · lattice-end · get-state.
  Written to `AE01` in MTU−3-byte chunks (≥ 20 bytes) with `max(--pacing-ms, 20 ms)` pacing.
* **Done:** the printer notifies `51 78 AE 01 01 00 00 00 FF` on `AE02` when it is ready again
  (upstream waited up to 30 s for it).
* No grayscale mode; `catprinterd` renders 1-bit for this family regardless of the quality setting.
