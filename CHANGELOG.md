# Changelog

## 0.3.0 — 2026-08-18

Retracts the mistaken `v3.0.0` tag / GitHub Release (semver `3.0.0` is
greater than `0.3.0`; `install.sh --download latest` and version-sorted
clients would stay wrong). This is the same dialog-settings restore that
was briefly published as 3.0.0, plus a small-footprint audit.

Print dialog matches the original kid settings ([original-settings.md](original-settings.md)).
Style and tone are independent again: Picture no longer forces grayscale.
Document A4/Letter selects the no-trim homework miniature. Paper / Sticker
is a label only. Regenerating the CUPS queue (`install.sh update`) is
required so the driverless PPD picks up the new names.

Breaking: IPP `print-quality` 5 is Picture (1-bit unless color-mode is
monochrome). `print-color-mode-default` is `bi-level` (Black and white).
CLI `-q high` is an alias for Picture, not grayscale; use `--tone grayscale`.

Audit: drop the unused `nix` crate, unused `uuid` v4, unused tokio `fs` /
`process`, and unused tokio-util default features. No new runtime package
and no extra ostree layer. Kit and image-baked layouts are unchanged.

## 0.2.6 — 2026-08-17

Setup and lifecycle surface so an integrator does not reimplement BlueZ
dance in bash.

- `catprinterd adopt` / `adopt --device` / `adopt --status`: discover (or
  pin) the printer, force an LE-only BlueZ object via `ConnectDevice`,
  set `Trusted`, persist the MAC under `$STATE_DIRECTORY` (DynamicUser
  cannot write `/etc`). First successful live connect records the MAC
  automatically. Printer off → non-zero, "switch it on and re-run".
- `catprinterd doctor` / `doctor --json`: adopted/trusted-LE, LE vs
  Classic, `ConnectDevice`/`Experimental`, queue/port, daemon-up,
  adapter power/block/autosuspend. Stale BlueZ RSSI is omitted.
- `install.sh uninstall` reverts only kit-set `Experimental` and the
  adopted trusted record. `install.sh status --json` is
  machine-readable; exit 0 only when healthy. No new kit files.
- Refused jobs (oversize / over-`max_pages` / unwritable
  `--fake-printer` dest) log at **error** and finish aborted, not
  completed-successfully.

## 0.2.5 — 2026-08-17

Hardware report on 0.2.4 (MT7925, BlueZ 5.87, MXW01): `Device1.Connect()`
was paging **BR/EDR (Classic)** because the printer's ads look dual-mode
(`0x0A` flags, public address). Every failure cut off at 8 s; every
success was 11–16 s. `SetDiscoveryFilter(Transport=le)` does not choose
the Connect transport.

- LE-only connect via `Adapter1.ConnectDevice` with Address +
  `AddressType=public`. If the method is missing (BlueZ `Experimental`
  off), the job fails with a clear `NeedExperimental` message — not an
  unexplained bus error. `AlreadyExists` removes the scan object and
  retries ConnectDevice; `Device1.Connect` runs only on that LE object.
  A Settings-held link (`Connected=true`) is reused after a failed
  ConnectDevice. Install-time `main.conf` sets `Experimental = true`
  (the daemon still does not write it at runtime).
- Mark the Device1 **Trusted** once so a discovery-stop sweep cannot
  prune it.
- Live connect wait is **30 s**; default `CATPRINTER_PRINTER_WAIT` is
  **120 s** (inside the printer's ~6 min idle sleep).
- Discovery is stopped when `open()` returns — idle `Discovering=true`
  kept the printer awake.
- The "close the phone app" hint is only for a peer refuse. Same-host
  clients share BlueZ's link.
- `catprinter.service` is `Type=notify`; READY is signaled after the
  IPP listener binds.

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
