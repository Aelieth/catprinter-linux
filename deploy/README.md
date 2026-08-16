# Cat printer deploy payload

This folder is everything File → Print needs. Put it **anywhere**. The
daemon and CLI find `catprinter/` next to themselves. systemd cannot do
that on its own — `write-user-unit.sh` writes a user unit aimed at *this*
copy.

## Contents

| File | Role |
|---|---|
| `catprinter/` | BLE, render, IPP |
| `mxw01d` | Localhost IPP daemon (the driver) |
| `run-mxw01d` | Wrapper: this tree’s venv (or `CATPRINTER_PYTHON`) + `mxw01d` |
| `print.py` | Optional CLI (`--status`, `--preview-only`) |
| `catprinter.ppd` | Tape sizes, Document / Picture / Text, tone |
| `requirements.txt` | `bleak`, `numpy`, `Pillow` |
| `catprinter-mxw01d.service.in` | Unit template (`@ROOT@`) |
| `write-user-unit.sh` | Renders the unit (`--root`, `--dest`, `--wanted-by`, `--stdout`) |

Host already has: `python3`, `cupsd`, BlueZ, `pdftoppm` or Ghostscript.
Do not rpm-ostree layer. Do not install a CUPS backend into `/usr`.

## What your script must do

`$DEST` is whatever directory you choose.

1. Copy this tree to `$DEST`.
2. `python3 -m venv "$DEST/venv" && "$DEST/venv/bin/pip" install -r "$DEST/requirements.txt"`  
   (or set `CATPRINTER_PYTHON` to another interpreter that has the deps).
3. Hand install: `"$DEST/write-user-unit.sh"` then  
   `systemctl --user daemon-reload && systemctl --user enable --now catprinter-mxw01d`  
   Fleet: `write-user-unit.sh --stdout --root "$DEST" --wanted-by graphical-session.target`  
   Health without the printer: `"$DEST/run-mxw01d" --check`
4. ```
   lpadmin -p CatPrinter -E \
     -v ipp://127.0.0.1:8095/ipp/print \
     -P "$DEST/catprinter.ppd" \
     -D "Cat Printer"
   ```
   (often needs sudo; CUPS writes `/etc/cups/ppd/`.)
5. Do not `cupsctl --share-printers`. Do not bake a MAC. Do not Pair.

After a driver change, run `./sync-from-src.sh` from the repo’s `deploy/`
folder, ship the folder again, restart `mxw01d`. After a PPD change, run
`lpadmin -P` again.

Kid contract: Bluetooth on, printer on, print.
