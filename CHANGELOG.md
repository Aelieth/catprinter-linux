# Changelog

## 0.2.4 — 2026-08-17

TemporaryTimeout=0 left unpaired Device1 objects with no RSSI; Connect
page-timed-out for 12 s on combo cards. OEM remaps classified as generic,
Broadcom was missing, and the live path was too slow for kids.

- Device1.Connect only when the object is advertising (RSSI) or already
  Connected. Silent / missing objects wait for a fresh advertisement
  (2 s on Realtek / MediaTek / QCA / Broadcom, 800 ms otherwise) and
  classify as `not-advertising` / `pruned` instead of burning
  `CONNECT_TIMEOUT_S`. The wait overlaps the host-abort pause.
- Live Connect is 8 s (`CONNECT_TIMEOUT_S` stays 12). Scan settle is
  350 ms. Close is 2 s + 2 s + 400 ms. A live cache hit no longer falls
  through to an 8 s rescan. Worst-case BLE setup stays under a minute;
  a typical short print should land around 20 s plus head time.
- Install-time udev `61-catprinter-btusb.rules`: `power/control=on` on
  Wireless Controller *interfaces* (`e0/01/01`), Broadcom vendor-specific
  `ff/01/01`, and their parent (combo IAD cards are `bDeviceClass=ef`).
  Daemon still never writes udev, sysfs, `main.conf`, or rfkill.
- `catprinterd check` prints `udev present|absent`.
- Host chip classification follows `btusb` quirk tables across OEM remaps
  (Foxconn 0489, Azurewave 13d3, Lite-On 04ca, ASUS 0b05, Toshiba 0930)
  and adds Broadcom (`0a5c` / Apple / Dell). Shared OEM VIDs map by PID,
  never VID-wide.

## 0.2.3 — 2026-08-17

Production: combo-card USB firmware (btusb Realtek/QCA/MediaTek) was going
offline under 0.2.2 retries. Kernel `btusb_rtl_reset` / `btusb_qca_reset` /
`btusb_reset` USB-reset the controller on command timeout; BlueZ
`device_request_disconnect` already has a 2 s timer. Stopping discovery and
Disconnecting again after `le-connection-abort-by-local` raced that reset.

- Never `StopDiscovery` between Connect attempts (`ensure_le_discovery` only
  starts if Discovering is already false).
- Do not Disconnect again after a failed Connect (host-abort already tore the
  HCI link; timeout cancels once).
- Treat `ECONNABORTED` / “adapter not powered” as adapter-off, not host-abort.
- Adapter-off is no longer immediately terminal: wait, `Set Powered true`,
  wait for USB re-enumeration, then retry inside the 3-attempt budget.
- `open()` recovers a Powered=false adapter before giving up.
- `catprinterd check` prints `bt chip` from btusb id tables (mediatek /
  realtek / qca / intel).

## 0.2.2 — 2026-08-16

BlueZ connect no longer trusts a discovery-time Device1 path. Unpaired printers
are temporary objects; the default `TemporaryTimeout` (30 s) deletes them, which
used to surface as “Device1 doesn’t exist” or an opaque timeout — especially on
combo Wi-Fi/Bluetooth cards.

- Re-resolve a live Device1 by BD_ADDR under the chosen adapter immediately
  before every Connect. Conventional `…/dev_XX_XX_…` is only used when that
  object still exists; a missing object is a **prune**, not a 12 s timeout.
- GATT bind uses the live path. The Device1 proxy is dropped between attempts.
- Classify connect failures: `pruned`, `host-abort`
  (`le-connection-abort-by-local` / ECONNABORTED), `timeout`, `adapter-off`,
  `other`. Journal and `ConnectFailed.last` use those prefixes.
- Kind-specific retry: host-abort waits longer; prune waits shorter and
  re-resolves; both refresh LE discovery so BlueZ can recreate the temporary
  object. AdapterOff is terminal. Scan stays running during Connect.
- Each attempt logs address, live RSSI, `n/N`, path, and kind.
- `catprinterd check` prints read-only host facts that never change READY:
  `TemporaryTimeout` (BlueZ default 30 when the key is absent/commented),
  `combo` (composite USB `ef` parent + Wireless Controller *interfaces*
  `e0/01/01` — never device-class `e0` alone), and `power/control` on those
  interfaces. Facts still print when bluetoothd is missing.

Connect timeout (12 s) and attempt count (3), public CLI flags, and
`CATPRINTER_*` env vars are unchanged. The daemon does not write `main.conf`,
udev, or rfkill.

## 0.2.1 — 2026-08-16

Post-release audit: fleet-test CI fixes, worker supervision, capped
decompression, streaming Send-Document, cooperative BLE link release, safer
device pick, faithful classic transport, panic-safe IPP.

## 0.2.0 — 2026-08-15

First `catprinterd` kit: IPP Everywhere on loopback, MXW01 + classic family,
hold-and-wait queue, image-baked and kit install paths.
