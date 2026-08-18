# Cat printer kit (`catprinterd`)

One folder, one command, and the Bluetooth cat printer shows up as a normal printer in every
print dialog on that machine — for every user, whether or not anyone is logged in.

`catprinterd` is a single self-contained binary (glibc ≥ 2.35; built on ubuntu-22.04): an
**IPP Everywhere** (driverless) printer on `127.0.0.1:8095` that CUPS talks to, and a Bluetooth LE
client that talks to the printer.
CUPS does all document conversion (PDF, text, images, n-up, copies…); the daemon receives PWG
raster, trims/dithers it for the 48 mm head, and streams it over Bluetooth. Supported models:
**MXW01** (verified) and the classic family **GB01 / GB02 / GB03 / GT01 / MX05 / MX06 / MX08 /
MX09 / MX10 / MX11 / YT01 / X5 / X6** (ported from the reference implementation, not verified on
hardware here). The model is autodetected when printing — buy a new one, it just works.

**Kid contract:** Bluetooth on, printer on, print. No pairing, no MAC addresses, no per-user setup.

**Immutable-first:** nothing is layered into rpm-ostree. The kit writes only to `/usr/local`
(= `/var/usrlocal`, writable and persistent across upgrades/rebases) and `/etc`, and needs only
base-image packages: `cups`, `cups-filters`, `bluez`, `util-linux` (rfkill), `policycoreutils`
(restorecon), `curl`; `avahi` optional (discovery).

## Install (admin, once per machine)

```sh
sudo ./install.sh              # install or upgrade
sudo ./install.sh status       # units, health, queue, journal (works without sudo too)
sudo ./install.sh status --json  # same facts as JSON; exit 0 only when healthy
sudo ./install.sh update       # same as install (binary swap + restart)
sudo ./install.sh uninstall    # remove queue, units, files; add --purge to drop /etc/catprinter too
```

Options: `--binary PATH` (use another `catprinterd`), `--download [vX.Y.Z|latest]` (fetch the
kit tarball for this architecture from GitHub Releases and verify it against `SHA256SUMS`; default
`latest`, which never resolves to a pre-release), `--yes` (no prompts), `-h` (this usage).

What it does: installs `/usr/local/bin/catprinterd`, `catprinter.service` (the daemon,
`DynamicUser`, hardened) and `catprinter-queue.service` (a root oneshot that runs
`lpadmin -m everywhere` at every boot so the CUPS queue **CatPrinter** always exists and points at
the daemon), keeps a copy of the units + `VERSION`/`INSTALLED` under `/usr/local/share/catprinter`,
creates `/etc/catprinter/env`, waits for the daemon, and prints a status table. It never makes the
cat printer the system default. Re-running is safe. `status` exits 0 only when both units are
active, the queue exists and no kit files shadow an image (see below); `status --json` is the
same facts without grepping prose. `uninstall` reverts BlueZ `Experimental` only if this kit
turned it on, and removes the adopted trusted printer record. It warns when
`CATPRINTERD_ARGS` carries `--port/--queue/--bind` (only `CATPRINTER_PORT/QUEUE` are honoured by
the queue unit and this script). After install: `catprinterd adopt` (or just print once) pins
the printer; `catprinterd doctor --json` is the integrator health check.

## Settings — `/etc/catprinter/env`

systemd `EnvironmentFile` syntax; after editing: `sudo systemctl restart catprinter catprinter-queue`.

| Key | Default | Meaning |
|---|---|---|
| `CATPRINTER_DEVICE` | any known cat printer | pin one printer (MAC or advertised name) |
| `CATPRINTER_MODEL` | `auto` | `mxw01` / `classic` to skip autodetection |
| `CATPRINTER_PORT` | `8095` | loopback IPP port (queue URI follows) |
| `CATPRINTER_PRINTER_WAIT` | `120` | seconds a job waits for the printer to be switched on (inside the ~6 min idle sleep) |

`install.sh` also sets BlueZ `Experimental = true` in `/etc/bluetooth/main.conf` so
`Adapter1.ConnectDevice` exists. MXW01 advertisements look dual-mode; without that
method BlueZ `Device1.Connect` pages Classic and the printer never answers.
| `CATPRINTER_LOG` | `info` | `debug`, `trace`, or a tracing filter |
| `CATPRINTER_QUEUE` | `CatPrinter` | CUPS queue name |
| `CATPRINTER_LOCATION` | Bluetooth, wherever… | printer-location text |
| `CATPRINTER_UUID` | adopted from the queue | fixed printer-uuid (setting it disables the adopter; un-adopted default is v5 of machine-id:port) |
| `CATPRINTER_DNSSD` | `on` | Avahi advertisement on loopback (discovery) |
| `CATPRINTERD_ARGS` | — | extra flags, e.g. `--fake-printer /var/lib/catprinter/fake` — never `--port/--queue/--bind` (use the keys above) |

Rarely needed knobs (`CATPRINTER_ADAPTER`, `_SLOW`, `_PACING_MS`, `_NOTIFY_MODE`, `_BIND`,
`_QUEUE_MAX`, `_MAX_DOCUMENT_MB`, `_MAX_LINES`, `_MAX_LINES_PER_REQUEST`, `_MAX_COPIES`,
`_RESOLUTIONS`, `_DNSSD_NAME`, `_PRINTER_NAME`, `_FAKE_PRINTER`) are documented in the "advanced"
block at the end of `env.example`; each maps to a `catprinterd serve --help` flag.

## In the print dialog

* Printer: **CatPrinter** (never the default — a 48 mm tape must not receive homework by accident).
* **Media**: **Cat tape 48 mm** (default) · Cat tape long · **Document A4** / Document Letter
  (48 mm × A4/Letter aspect — whole homework page shrunk to 384 dots, not a left strip) ·
  Custom (48 mm × up to 5000 mm).
* **Print quality**: **Default** (drawings) · Text (sharp) · Picture (photos / crayon). Style only.
* **Color / tone**: **Black and white** (default) · Grayscale (16-level on the MXW01). Picture +
  Grayscale is the photo path.
* **Paper type**: Paper · Sticker (label only; same burn).
* Anything CUPS can print prints: PDF, text, PNG/JPEG, LibreOffice, browsers, n-up, copies.
  `.webp` is not a CUPS type — convert first.

## Troubleshooting

| Queue / dialog message | What to do |
|---|---|
| Cat printer not found — turn it on and keep it near the computer | Power it on; the job continues by itself (up to `CATPRINTER_PRINTER_WAIT`). |
| Bluetooth is turned off on this computer | Toggle Bluetooth on, or `rfkill unblock bluetooth`. |
| The cat printer is out of paper. | Reload paper; the job retries. |
| The cat printer is too hot / battery is low | Wait / charge; the job retries. |
| Could not connect… Close the phone app / Bluetooth Settings is holding the printer | Only one connection at a time: close the app, or turn the printer off and on. |
| Cat printer not found for N min — job stopped | The wait window expired; turn the printer on and print again. |
| Print would be N m long; limit … | Split the document or pick a shorter page size. |
| Two Cat Printers in the dialog | `sudo ./install.sh update` (re-aligns the DNS-SD uuid), or `CATPRINTER_DNSSD=off`. |
| Nothing prints, queue idle | `sudo ./install.sh status`; `journalctl -u catprinter -n 50`; `catprinterd check` (TemporaryTimeout, combo, bt chip, USB BT power/control, udev). |
| Job stuck | `cancel -a CatPrinter`; `sudo systemctl restart catprinter`. |

## Image-baked install (custom uBlue image)

Put the same files into the image and skip per-machine steps entirely — on the first boot *and* on
every rebase (`make image-files DEST=…` produces this layout from a checkout):

```
/usr/bin/catprinterd
/usr/lib/systemd/system/catprinter.service          (ExecStart=/usr/bin/catprinterd …)
/usr/lib/systemd/system/catprinter-queue.service
/etc/systemd/system/multi-user.target.wants/catprinter.service        -> /usr/lib/systemd/system/…
/etc/systemd/system/multi-user.target.wants/catprinter-queue.service  -> /usr/lib/systemd/system/…
/usr/lib/systemd/system-preset/80-catprinter.preset  (enable both units — first boot only)
/usr/lib/udev/rules.d/61-catprinter-btusb.rules       (USB BT autosuspend off on e0/01/01)
/usr/lib/catprinter/env.example                      (docs; config stays in /etc/catprinter/env)
/usr/lib/catprinter/install.sh                       (status / env / re-enable on the machine)
/usr/lib/catprinter/VERSION                          ("<semver> <git-sha> <build-utc>")
```

Containerfile: `COPY image-root/ /` — the two `etc/…wants/` symlinks are exactly what
`RUN systemctl enable catprinter.service catprinter-queue.service` would create at build time
(ostree carries `/usr/etc` into every deployment's `/etc`). They matter: systemd applies presets
only on a true first boot (`/etc/machine-id` missing/empty), so a machine that *rebases* onto the
image would otherwise never enable the units.

On such a machine `install.sh` (`/usr/lib/catprinter/install.sh` or a kit copy) recognises the
image-baked install and then only manages `/etc/catprinter/env` and the unit state:

* `install`/`update` — create the env file if missing, (re-)enable and restart the units, run the
  checks. `--download`/`--binary` are ignored (the image always wins; ship a new image to update).
* a machine that was kit-installed before it rebased onto the image is **mixed**: the kit's
  `/etc/systemd/system/catprinter*.service` shadow the image's units. `status` says so (exit 1);
  `install` removes the kit files (`/usr/local/bin/catprinterd`, `/usr/local/share/catprinter`,
  the units) and lets the image's units take over. A "kit hot-fix over an image" is impossible by
  design.
* `uninstall` disables the units and removes the queue; image files stay. Re-enable with
  `install.sh install`.

`rpm-ostree status` shows no layered packages either way. `make fleet-test` boots both layouts in
systemd containers (see the repository README).

## Files

`catprinterd` · `install.sh` · `catprinter.service` · `catprinter-queue.service` ·
`80-catprinter.preset` · `61-catprinter-btusb.rules` · `env.example` · `VERSION` · this README.

`VERSION` is one line, three fields: `<semver> <git-sha|nogit> <build-utc>` — the same line
verbatim in the kit, in `/usr/local/share/catprinter/VERSION` (kit install) and in
`/usr/lib/catprinter/VERSION` (image); field 1 is always the semver (`cut -d' ' -f1`). A kit
install also writes `/usr/local/share/catprinter/INSTALLED` = `<install-utc> <source>` (the binary
path, prefixed with `download:<tag>` when fetched). `install.sh status` prints both.
Source, protocol notes and issues: https://github.com/Aelieth/catprinter-linux
