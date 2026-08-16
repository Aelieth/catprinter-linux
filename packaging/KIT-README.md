# Cat printer kit (`catprinterd`)

One folder, one command, and the Bluetooth cat printer shows up as a normal printer in every
print dialog on that machine — for every user, whether or not anyone is logged in.

`catprinterd` is a single static binary: an **IPP Everywhere** (driverless) printer on
`127.0.0.1:8095` that CUPS talks to, and a Bluetooth LE client that talks to the printer.
CUPS does all document conversion (PDF, text, images, n-up, copies…); the daemon receives PWG
raster, trims/dithers it for the 48 mm head, and streams it over Bluetooth. Supported models:
**MXW01** (verified) and the classic family **GB01 / GB02 / GB03 / GT01 / MX05 / MX06 / MX08 /
MX09 / MX10 / MX11 / YT01 / X5 / X6** (ported from the reference implementation, not verified on
hardware here). The model is autodetected when printing — buy a new one, it just works.

**Kid contract:** Bluetooth on, printer on, print. No pairing, no MAC addresses, no per-user setup.

**Immutable-first:** nothing is layered into rpm-ostree. The kit writes only to `/usr/local`
(= `/var/usrlocal`, writable and persistent across upgrades/rebases) and `/etc`, and needs only
base-image packages (`cups`, `cups-filters`, `bluez`, `util-linux`, `policycoreutils`, `curl`,
optionally `avahi`).

## Install (admin, once per machine)

```sh
sudo ./install.sh              # install or upgrade
sudo ./install.sh status       # units, health, queue, journal (works without sudo too)
sudo ./install.sh update       # same as install (binary swap + restart)
sudo ./install.sh uninstall    # remove queue, units, files; add --purge to drop /etc/catprinter too
```

Options: `--binary PATH` (use another `catprinterd`), `--download [vX.Y.Z]` (fetch the kit from
GitHub Releases and verify `SHA256SUMS`), `--yes` (no prompts).

What it does: installs `/usr/local/bin/catprinterd`, `catprinter.service` (the daemon,
`DynamicUser`, hardened) and `catprinter-queue.service` (a root oneshot that runs
`lpadmin -m everywhere` at every boot so the CUPS queue **CatPrinter** always exists and points at
the daemon), creates `/etc/catprinter/env`, waits for the daemon, and prints a status table. It
never makes the cat printer the system default. Re-running is safe.

## Settings — `/etc/catprinter/env`

systemd `EnvironmentFile` syntax; after editing: `sudo systemctl restart catprinter catprinter-queue`.

| Key | Default | Meaning |
|---|---|---|
| `CATPRINTER_DEVICE` | any known cat printer | pin one printer (MAC or advertised name) |
| `CATPRINTER_MODEL` | `auto` | `mxw01` / `classic` to skip autodetection |
| `CATPRINTER_PORT` | `8095` | loopback IPP port (queue URI follows) |
| `CATPRINTER_PRINTER_WAIT` | `600` | seconds a job waits for the printer to be switched on |
| `CATPRINTER_LOG` | `info` | `debug`, `trace`, or a tracing filter |
| `CATPRINTER_QUEUE` | `CatPrinter` | CUPS queue name |
| `CATPRINTER_LOCATION` | Bluetooth, wherever… | printer-location text |
| `CATPRINTER_UUID` | adopted from the queue | fixed printer-uuid |
| `CATPRINTER_DNSSD` | `on` | Avahi advertisement on loopback (discovery) |
| `CATPRINTERD_ARGS` | — | extra flags, e.g. `--fake-printer /var/lib/catprinter/fake` |

## In the print dialog

* Printer: **CatPrinter** (never the default — a 48 mm tape must not receive homework by accident).
* **Media**: `48x297mm` (cat tape, default) · `48x500mm` (long tape) · `A4` / `Letter` (whole page
  shrunk to the tape) · Custom (48 mm × up to 5000 mm). LibreOffice picks A4/Letter for office
  documents by itself.
* **Print Quality**: `Draft` = sharp text · `Normal` = drawings (dithered) · `High` = photos
  (16-level grayscale on the MXW01).
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
| Nothing prints, queue idle | `sudo ./install.sh status`; `journalctl -u catprinter -n 50`; `catprinterd check`. |
| Job stuck | `cancel -a CatPrinter`; `sudo systemctl restart catprinter`. |

## Image-baked install (custom uBlue image)

Put the same files into the image and skip per-machine steps entirely (`make image-files DEST=…`
produces this layout from a checkout):

```
/usr/bin/catprinterd
/usr/lib/systemd/system/catprinter.service          (ExecStart=/usr/bin/catprinterd …)
/usr/lib/systemd/system/catprinter-queue.service
/usr/lib/systemd/system-preset/80-catprinter.preset  (enable both units)
/usr/lib/catprinter/env.example                      (docs; config stays in /etc/catprinter/env)
```

`install.sh` recognises an image-baked machine and then only manages `/etc/catprinter/env`.
`rpm-ostree status` shows no layered packages either way.

## Files

`catprinterd` · `install.sh` · `catprinter.service` · `catprinter-queue.service` ·
`80-catprinter.preset` · `env.example` · `VERSION` · this README.
Source, protocol notes and issues: https://github.com/Aelieth/catprinter-linux
