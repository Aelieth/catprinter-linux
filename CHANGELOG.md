# Changelog

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
