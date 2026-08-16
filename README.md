# catprinter-linux

Linux driver for the **MXW01** Bluetooth cat printer. File → Print is the goal.

This is not the GT01/GB01 protocol. Those printers speak `0x51 0x78` on one
characteristic. The MXW01 speaks `0x22 0x21` and needs **three** BLE
characteristics (`AE01` control, `AE02` notify, `AE03` image data). See
[PROTOCOL.md](PROTOCOL.md).

## Kid contract

1. Turn Bluetooth on.
2. Turn the little printer on (paper loaded).
3. Print.

No pairing. Do not click Pair in Bluetooth Settings, and do not leave the
printer Connected there between jobs — that parks its only radio slot.
Power the printer on when you print. The driver scans (or borrows a
Settings link if you already clicked Connect), prints, and disconnects.
Bluetooth is left alone until the next job.

No MAC addresses. Walk the printer to the other computer and do the same
thing — install the driver once on each machine.

## Install

```bash
# On the host (Fedora Silverblue / Workstation):
#   Bluetooth is BlueZ. Turn it on in Settings.
# Inside a toolbox/distrobox this repo still works — it talks to the host bus.

python3 -m venv venv
source venv/bin/activate
pip install -r requirements.txt
```

Add your user to the `bluetooth` group if scans fail as a normal user (this is a
home-directory usermod, not an rpm-ostree layer):

```bash
sudo usermod -aG bluetooth "$USER"
# log out and back in
```

On Immutable Fedora, BlueZ is the **host** daemon. A toolbox/distrobox has no
system bus; `print.py` will use `/run/host/run/dbus/system_bus_socket` by
itself. Do not bake a MAC address into your deploy script — the CLI
auto-discovers `MXW01`.

## Usage

```bash
source venv/bin/activate

# Print an image (auto-discovers any MXW01 in range)
./print.py photo.png

# Darker / lighter (0-255, default 0x5D)
./print.py -i 0x80 photo.png

# Preview the tape (PNG + 48 mm PDF) and ask before printing
./print.py --show-preview photo.png

# Write the preview and stop (no Bluetooth)
./print.py --preview-only -q picture --tone grayscale photo.png

# Battery / paper / temperature
./print.py --status

# Feed or retract paper
./print.py --eject 40
./print.py --retract 20

# If a print comes out striped, force one BLE write per row
./print.py --slow photo.png

# Print style: default, picture, text, document (whole A4/Letter page)
./print.py -q picture vacation.jpg
./print.py -q text notes.pdf
./print.py -q document homework.pdf

# Real 16-level grayscale (best photos). Independent of style.
./print.py -q picture --tone grayscale vacation.jpg
```

Two independent controls (same names as the CUPS dropdowns):

**Print style** — what the page is.

| Style | What it does |
|---|---|
| `default` | Mixed drawings. Trim white, then fill the tape. Normal heat (`0x5D`). |
| `picture` | Photos and crayon. Hotter head (`0x78`), stronger curve. |
| `text` | Homework doodles and terminal dumps. Sharp, no speckle. |
| `document` | Whole A4 / Letter page, no trim, shrink to 48 mm. Tell the kids this one. |

**Tone** — how the head burns it.

| Tone | What it does |
|---|---|
| `blackwhite` | 1 bit per dot. Floyd–Steinberg (default/picture) or a hard threshold (text). |
| `grayscale` | 16 real burn levels (4 bpp). Best photos. Picture + Grayscale is the quality path. |

White margins are cropped, then the result is scaled to 384 px wide (the print head). A doodle on an A4 page becomes a short strip, not a white banner. Color is Rec. 709 luma; transparent pixels become white paper.

By default the image is rotated 180° so text comes out right-side up. Pass
`--top-first` to skip that.

If nothing is found:

```
Turn the cat printer on and make sure Bluetooth is on.
```

## Hardware test

```bash
bluetoothctl power on
./print.py --status
./print.py media/hackoclock.jpg
```

## CUPS (File → Print)

`mxw01d` is a localhost IPP printer. It must run as the logged-in user (Bluetooth).
CUPS stays the host daemon. Nothing is written into `/usr`.

Aurora / Bazzite already ship `pdftoppm`, Ghostscript, and CUPS — we use those
to rasterize PDFs. No extra PDF library.

```bash
# In the user session (or via contrib/catprinter-mxw01d.service)
./venv/bin/python mxw01d

# Once, as a user who can run lpadmin (often needs sudo on Fedora):
sudo lpadmin -p CatPrinter -E \
  -v ipp://127.0.0.1:8095/ipp/print \
  -P "$PWD/data/catprinter.ppd" \
  -D "Cat Printer"
```

In the print dialog: printer **Cat Printer**, size **Cat tape 48 mm** for
drawings, style **Default** / **Picture** / **Text** / **Document**, tone
**Black and white** / **Grayscale**, type **Paper** / **Sticker** (label only).

Tell the kids: homework from LibreOffice → **Document**. That selects a full
A4 page and we shrink the whole sheet onto the tape. Drawings stay on Cat
tape 48 mm (Default / Picture). If a program still previews a sliver, pick
paper size **Document A4** (or **Document Letter**) once.

The GTK/Qt preview follows the page size. For what the head will actually
burn, use `./print.py --preview-only` and open `catprinter-preview.pdf`.

After updating the PPD, run `lpadmin` again so CUPS picks up Document.

Do not enable CUPS sharing. Each machine talks BLE itself.

## What works / what's next

- [x] MXW01 BLE protocol (print, status, eject)
- [x] Auto-discover by name / service, strongest RSSI if two are on
- [x] Disconnect when the job ends (so the other room can grab it)
- [x] CUPS / File → Print via localhost IPP (`mxw01d`)
- [x] Orthogonal Default/Picture/Text × BlackWhite/Grayscale, real 4 bpp grayscale
- [ ] Your deploy script on the kids' machines

## Credits

- Protocol: [jeremy46231/MXW01-catprinter](https://github.com/jeremy46231/MXW01-catprinter),
  [dave9123/MXW01-catprinter](https://github.com/dave9123/MXW01-catprinter),
  [MaikelChan/CatPrinterBLE](https://github.com/MaikelChan/CatPrinterBLE)
- Original GT01 Python client + dithering: [rbaron/catprinter](https://github.com/rbaron/catprinter)
- Linux BlueZ MTU workaround from this fork's upstream
