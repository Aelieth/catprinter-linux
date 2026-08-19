# Changelog

## 0.4.0 — 2026-08-19

Cross-distro and cross-desktop: the kit now installs and runs on any systemd + CUPS distro
(Fedora/RHEL/openSUSE, Debian/Ubuntu, Arch), not just Immutable Fedora. `src/` was already
portable; the Fedora coupling lived in packaging, docs and tests.

- **`install.sh` preflight no longer hard-fails off Fedora** — and could already mis-fire *on*
  Fedora. It located CUPS filters via `cups-config` (usually absent at runtime) with a
  `/usr/lib/cups` fallback that is wrong on 64-bit Fedora/RHEL/openSUSE (`/usr/lib64/cups`), then
  required standalone filter binaries (`pdftopdf`/`rastertopwg`/`gstoraster`) that cups-filters
  2.x / CUPS 3.x reorganize or drop. Now: a robust serverbin locator (lib/lib64/libexec), the
  filter check is a warning (`catprinter-queue.service` is the authoritative gate), and cupsd
  start tries `cups.service` then `cups.socket`.
- **Distro-aware errors** — `install.sh` reads `/etc/os-release` and prints the exact
  `dnf`/`apt`/`pacman`/`zypper install …` command for a missing dependency. It still never
  installs packages itself (immutable-first; on ostree a runtime install would not apply anyway).
- Prebuilt kits for **x86_64** and **aarch64** (both glibc ≥ 2.35; aarch64 kit is built and
  tested on `ubuntu-22.04-arm`). Other arches get a source-build hint.
- `catprinter.service` documents that `RestartSteps`/`RestartMaxDelaySec` need systemd v254+ and
  degrade gracefully below it; `make check`'s unit verify tolerates that warning (Debian 12 /
  Ubuntu 22.04 ship systemd 252 / 249).
- **Fleet test on Fedora + Debian + Arch** — real systemd-in-podman boot + a print through CUPS,
  via per-distro provisioners (`tests/fleet/provision/*.sh`) and `FLEET_DISTRO`; `make
  fleet-test-all` runs the set. CI matrix gates on Fedora + Debian, Arch informational.
- Docs: kid-facing README (quick install, what to print, Cat-tastic options) plus KIT-README
  per-distro treats; honest arch/glibc floor.
- Print dialog: a pinned classic printer (`CATPRINTER_MODEL=classic`) advertises **Cat Printer
  Classic** instead of misreporting **MXW01**; `mxw01`/`auto` unchanged.
- Tidy: `[lints.clippy] all = deny` so a plain local `cargo clippy` matches CI's `-D warnings`
  (clippy-only, so a distro packager's `cargo build` is unaffected); drop a redundant `serde_json`
  dev-dep and a redundant `#[allow]`; the version test asserts semver shape instead of a
  hardcoded string; shared `bluez::created_le_path` helper; `main.rs` gains a module doc.

## 0.3.2 — 2026-08-18

Document is a **full A4 / Letter page**, then the daemon shrinks that entire
raster to 384 dots (~4.4×). 0.3.1 advertised Document at 48 mm × aspect so
CUPS named the size `48x68mm` (no Document/A4 in the dialog) and apps laid
out a 48 mm column instead of a homework page.

- **Cat Tape short** (`custom_cat-tape_48x297mm`, default) and **Cat Tape long**
  — 48 mm roll; trim and fill the head.
- **Cat Minidoc A4** / **Cat Minidoc Letter** — true `iso_a4_210x297mm`
  (210×297 mm) and `na_letter_8.5x11in` (8.5×11 in) so LibreOffice/GTK emit a
  full page. `Layout::Sheet` scales the whole raster to 384 px (title stays at
  the top, last line at the bottom). Style (Text / Default / Picture) still
  applies — Minidoc is paper, not a hidden Document preset.
- printer-strings: `PageSize.A4` / `PageSize.Letter` → Cat Minidoc A4 / Letter;
  `PageSize.48x297mm` / `48x500mm` → Cat Tape short / Cat Tape long.

Print style and speed: CUPS `black_1` FastGray made Text/Default/Picture look
identical (already 1-bit), and ColorModel **Gray** default sent every job as
4 bpp grayscale (slow). Raster type is **`sgray_8` only** so we dither; 4 bpp
is **Picture + Grayscale** only (the photo path). Default is 1 bpp Black and
white. Jobs log quality, color-mode, preset, tone, and layout.

`ensure-queue` now installs a **cups-filters `driverless` PPD** (GTK reads
`Choice/Human name`, not `printer-strings-uri`). CUPS `-m everywhere` left
Draft/Normal/High, Stationery/Labels, and `48x297mm`. Duplicate
`Cat_Printer` queues that point at the same loopback URI are removed.
`print-content-optimize` is advertised as **`auto` only** (IPP Everywhere still
wants the attributes) and stripped from the PPD so GTK does not show a second
**Print Optimization** menu with Text / Photo / Graphics next to **Print style**.

Minidoc/Sheet trims **left/right** white (not top/bottom) so Gwenview/KDE's
~0.17 in dialog margins do not shrink type; title stays at the top, last line
at the bottom. PPD `ImageableArea` / `HWMargins` left and right are forced to 0
on every page size.

`install.sh update` (or equivalent queue refresh) is required so the
driverless PPD is regenerated from the new sizes.

## 0.3.1 — 2026-08-18

Printer-properties: Document A4 / Document Letter are advertised at **48 mm
tape width × A4/Letter aspect** so CUPS rasterises the whole homework page to
384 dots (0.3.0 used true 210 mm / 8.5 in width and printed a left strip).
Document is selected by media-size-name (`iso_a4` / `na_letter`), not by
width — otherwise 48 mm homework would be trimmed as tape.
`pwg-raster-document-type-supported` is `black_1` and `sgray_8` so
**Black and white** and **Grayscale** both appear (default Black and white).
Quality strings also map cupsPrintQuality Draft/Normal/High → Text/Default/Picture.

`install.sh update` (or equivalent queue refresh) is required so the driverless
PPD is regenerated.

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
