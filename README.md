# catprinter-linux — `catprinterd`

**Bluetooth "cat" thermal printers as regular Linux printers.** `catprinterd` is a small Rust daemon
that turns an MXW01 (and the GB0x/GT01/MX0x/YT01/X5/X6 family) into a driverless **IPP Everywhere**
printer on `127.0.0.1:8095`. CUPS treats it like any modern network printer: no PPD to install, no
per-user setup, works for every account on the machine, survives reboots without anyone logging in.

```
File → Print ──► cupsd (pdftopdf → gstoraster → rastertopwg) ──► catprinterd ──► Bluetooth LE ──► 🐈
```

* **Kid contract:** Bluetooth on, printer on, print. No pairing, no MAC addresses, no apps.
* **Hold-and-wait:** if the printer is off, the job waits (10 min by default) and the queue says
  *"Cat printer not found — turn it on and keep it near the computer"*. Switch it on → it prints.
* **Immutable-first:** one self-contained binary (glibc ≥ 2.35; built on ubuntu-22.04), two
  systemd units, one env file. Nothing layered into rpm-ostree; runtime needs only base-image
  packages (`cups`, `cups-filters`, `bluez`, `util-linux`, `policycoreutils`, `curl`; `avahi`
  optional).
* **Model autodetect:** MXW01 (16-level grayscale) or the classic family; new printer → it just works.

## Install (admin, once per machine)

Grab the kit (`make kit` → `dist/catprinter-kit/`, or the release tarball) and run:

```sh
sudo ./install.sh              # install or upgrade
sudo ./install.sh status       # units, health, CUPS queue, journal
sudo ./install.sh update       # swap binary, restart, regenerate the CUPS PPD
sudo ./install.sh uninstall    # remove queue + units (+ --purge for /etc/catprinter)
```

What it does: copies `catprinterd` to `/usr/local/bin`, installs `catprinter.service` (the daemon,
`DynamicUser`, hardened) and `catprinter-queue.service` (a root oneshot that runs
`lpadmin -p CatPrinter -m everywhere …` at every boot, so the queue self-heals), creates
`/etc/catprinter/env`, and prints a status table. Old per-user `mxw01d` units are removed.

**Image-baked (custom uBlue image):** `make image-files DEST=<rootfs>` drops the same files into
`/usr/bin`, `/usr/lib/systemd/system`, the `etc/systemd/system/multi-user.target.wants/` symlinks
(= `systemctl enable` at build time — presets alone only fire on a true first boot, not on a
rebase), a `system-preset`, and `/usr/lib/catprinter/{VERSION,install.sh,env.example}`.
Containerfile: `COPY image-root/ /`. Zero per-machine steps, on first boot and on every rebase.
On such a machine `install.sh` only manages `/etc/catprinter/env` and the unit state:
`--download/--binary` are ignored (the image always wins — ship a new image to update), a
kit-installed machine that rebased onto the image is migrated (`install.sh install` removes the
kit files that shadow the image's units), and after `uninstall` the units come back with
`install.sh install`. `VERSION` files are one line, `<semver> <git-sha> <build-utc>`; field 1 is
the semver. See [packaging/KIT-README.md](packaging/KIT-README.md).

Config knobs live in `/etc/catprinter/env` (see `packaging/env.example`): `CATPRINTER_DEVICE`
(pin one printer), `CATPRINTER_MODEL` (`auto|mxw01|classic`), `CATPRINTER_PRINTER_WAIT`,
`CATPRINTER_PORT`, `CATPRINTER_DNSSD`, `CATPRINTERD_ARGS` (e.g. `--fake-printer DIR` for testing).

## In the print dialog

| Setting | Choices | What happens |
|---|---|---|
| Media / paper size | **48x297mm** (tape, default), 48x500mm, A4, Letter, Custom 48×(25–5000) mm | Tape sizes: white margins are trimmed and the content fills the 384-dot head. A4/Letter: the whole page is shrunk to the tape width (miniature). |
| Print quality | **Normal** (drawings), Draft (sharp text), High (photos → 16-level grayscale on the MXW01) | selects dithering / grayscale / burn intensity |
| Copies, n-up, landscape | as usual | CUPS handles them |

Kids never need to touch these; the defaults print drawings and text nicely.
Never make CatPrinter the *default* printer (homework on 48 mm tape); `install.sh` warns if it is.

## Troubleshooting (what the queue says → what to do)

| Queue message (`lpstat -p CatPrinter -l`, GNOME/KDE printer applet) | Do this |
|---|---|
| Cat printer not found — turn it on and keep it near the computer | Power the printer on (and close the phone app; it allows one connection). The job continues by itself. |
| Bluetooth is turned off on this computer | Turn Bluetooth on (`rfkill unblock bluetooth`). |
| The cat printer is out of paper. | Load a roll, close the lid. |
| The cat printer is too hot / battery is low | Wait a minute / charge it. |
| Cat printer not found for 10 min — job N stopped | The job gave up; turn the printer on and print again. |
| Print would be … long; limit … | Pick a shorter page size or split the document. |
| queue missing / daemon down | `sudo ./install.sh status`, `journalctl -u catprinter -u catprinter-queue`, `sudo ./install.sh update` |
| two "Cat Printer" entries in the dialog | the daemon adopts the CUPS queue's uuid within a minute; if it persists, `sudo systemctl restart catprinter`. |

`catprinterd check` (Bluetooth adapter / bluetoothd / port) and `catprinterd status` (connects to the
printer, reports model, battery, paper) are handy on the console.

## Developing

```sh
make check                      # fmt, clippy -D warnings, tests, shell lint, unit verify
make fleet-test                 # boots kit + image files in systemd containers (podman; ~3 min warm)
cargo run -- serve --port 8096 --fake-printer /tmp/fake --dnssd off     # no hardware needed
ipptool -V 2.0 -tI -f tests/fixtures/text-roll48.pwg -d filetype=image/pwg-raster \
        ipp://127.0.0.1:8096/ipp/print /usr/share/cups/ipptool/ipp-everywhere.test
driverless ipp://127.0.0.1:8096/ipp/print      # the PPD CUPS would generate
lpadmin -p CatTest -E -v ipp://127.0.0.1:8096/ipp/print -m everywhere && lp -d CatTest file.pdf
cargo run -- print media/hackoclock.jpg -q high    # straight to the printer over BLE
cargo run -- print file.png --preview-only out.png # render only
```

* `--fake-printer DIR` writes `job-N-*.png` (what the head would burn) + `job-N.json` per job and
  is scripted by `DIR/state` (`ok|off|no-paper|overheated|low-battery|slow|flaky:N`).
* Raster fixtures come from CUPS itself: `scripts/make-fixtures.sh` (uses `cupsfilter`).
* Layout: `src/protocol` (wire formats), `src/models` (registry + drivers), `src/ble` (BlueZ over
  D-Bus), `src/raster` (PWG decode), `src/render` (trim/fit/dither/pack), `src/ipp` + `src/http`
  (IPP Everywhere), `src/engine` (queue/worker), `src/dnssd` (Avahi), `src/cupsq` (uuid adoption).
* Protocol notes: [PROTOCOL.md](PROTOCOL.md). The Python driver this was ported from (and its
  hardware-proven BLE quirks) is preserved at git tag `python-final`; source comments cite it as
  `catprinter/*.py`.
* `make fleet-test` (`scripts/fleet-test.sh`, also a CI job) builds two Fedora systemd
  containers from `dist/` and boots them with podman: an image-baked machine in the *rebase*
  case (no first boot ⇒ no presets; the shipped wants symlinks must enable the units; a print
  goes through real CUPS into `--fake-printer`), and a plain machine that takes the kit path and
  then "rebases" onto the image (kit → image migration, uninstall/`--purge` idempotence).
  `KEEP=1` keeps the containers, `FLEET_FIRST_BOOT=1` adds the preset/first-boot variant, logs
  land in `tests/out/fleet/`. From a distrobox: `PODMAN="distrobox-host-exec podman"` (rootless
  works; the machines come up `degraded` because of `/sys/kernel/*` mounts, which is accepted).
* Hardware notes (MXW01, this project): connects reliably at MTU 512; at weak signal (≈ −80 dBm)
  BlueZ often aborts the first connect (`le-connection-abort-by-local`) — the daemon retries
  (3 attempts per round, rounds until `CATPRINTER_PRINTER_WAIT`). Strips longer than 4000 lines
  (multi-request segments) and the "Bluetooth Settings holds the link" path are implemented but
  were not exercised on hardware. Machines with more than one Bluetooth adapter: the daemon uses
  the first powered adapter; pin one with `CATPRINTER_ADAPTER=hci1` in `/etc/catprinter/env`, and
  pin the printer itself per machine with `CATPRINTER_DEVICE=<MAC or name>` when several cat
  printers are in range. `catprinterd status` exit codes: 0 = printer answered and is ready,
  2 = it answered but is not ready (no paper, too hot, low battery) or the connection failed
  (Bluetooth off, link held by another app — `catprinterd check` tells which), 3 = no cat
  printer found (turn it on, come closer).

### Supported models

| Family | Names | Verified here |
|---|---|---|
| MXW01 | `MXW01` — 384 px, 1-bit + 4-bit grayscale | yes (hardware) |
| classic (upstream rbaron/catprinter set) | GB01 GB02 GB03 GT01 MX05 MX06 MX08 MX09 MX10 MX11 YT01 X5 X6 — 384 px, 1-bit | protocol ported byte-for-byte from the reference implementation; **not hardware-tested by this project** — reports welcome |

Unknown names that advertise the AE30 service are driven by GATT shape (AE03 present ⇒ MXW01
protocol, else classic); force with `CATPRINTER_MODEL=`.

## Credits

Built on the reverse engineering of the cat-printer community: [rbaron/catprinter](https://github.com/rbaron/catprinter)
(classic family), [jeremy46231/MXW01-catprinter](https://github.com/jeremy46231/MXW01-catprinter) and
[MaikelChan/CatPrinterBLE](https://github.com/MaikelChan/CatPrinterBLE) (MXW01, 4-bit grayscale). MIT licensed.
