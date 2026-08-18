### Because your $1 AliExpress cat printer deserves better than a mean phone app and BlueZ tantrums

![It's Hack o' Clock!](media/hackoclock.jpg)

*Meet the star: a pocket-sized Bluetooth thermal cat that prints “It’s Hack o’ Clock” and whatever else a kid (or tired adult) desires — once Linux is taught how to talk to it properly.*

You bought the cutest, cheapest little thermal printer on the internet. It has a face. It has ears. It costs less than lunch. And then you tried to use it on Linux.

Bluetooth Low Energy + BlueZ + combo wireless cards that secretly dislike cats + an official phone app that hogs the single connection = a special kind of chaos. An entire program had to be written so this $12 device would behave like a normal printer.

**catprinterd** is that program: a small, hardened Rust daemon that turns an MXW01 (and the GB0x / GT01 / MX0x / YT01 / X5 / X6 family) into a driverless **IPP Everywhere** printer on `127.0.0.1:8095`. CUPS treats it like any modern network printer. No PPD to install, no per-user setup, works for every account on the machine, and survives reboots without anyone logging in.

```
File → Print ──► cupsd (pdftopdf → gstoraster → rastertopwg) ──► catprinterd ──► Bluetooth LE ──► 🐈 *purrs and prints*
```

### The Sacred Kid Contract 🤝

1. Bluetooth is on  
2. The cat printer is on  
3. Hit Print  

No pairing. No MAC addresses. No phone apps. That is the entire contract.

- **Hold-and-wait magic:** If the printer is off, the job waits (2 minutes by default) and the queue says *“Cat printer not found — turn it on and keep it near the computer.”* Switch it on → it prints. If the cat is already awake, the print should start in a few seconds — they nap after ~5–6 minutes of boredom, so a 10-minute wait only blocks the queue.  
- **Immutable-first:** One self-contained binary (glibc ≥ 2.35; built on ubuntu-22.04), two systemd units, one env file. Nothing layered into rpm-ostree; runtime needs only base-image packages (`cups`, `cups-filters`, `bluez`, `util-linux`, `policycoreutils`, `curl`; `avahi` optional).  
- **Model autodetect:** MXW01 (16-level grayscale) or the classic family; a new printer simply works.

## Install (admin, once per machine) — Make the cat official

Grab the kit (`make kit` → `dist/catprinter-kit/`, or the release tarball) and run:

```sh
sudo ./install.sh              # install or upgrade
sudo ./install.sh status       # units, health, CUPS queue, journal — how is the cat feeling?
sudo ./install.sh update       # swap binary, restart, regenerate the CUPS PPD
sudo ./install.sh uninstall    # remove queue + units (+ --purge for /etc/catprinter)
```

What it does: copies `catprinterd` to `/usr/local/bin`, installs `catprinter.service` (the daemon, `DynamicUser`, hardened, `Type=notify`) and `catprinter-queue.service` (a root oneshot that runs `lpadmin -p CatPrinter -m everywhere …` at every boot so the queue self-heals), creates `/etc/catprinter/env`, installs a udev rule so combo Wi-Fi/Bluetooth cards don’t nap mid-Connect (`61-catprinter-btusb.rules`), turns on BlueZ `Experimental = true` so we can force an **LE** connect (MXW01 ads look dual-mode; Classic `Connect` never talks to the printer), and prints a status table. Old per-user `mxw01d` units are removed.

**Image-baked (custom uBlue / ostree image):** `make image-files DEST=<rootfs>` drops the same files into `/usr/bin`, `/usr/lib/systemd/system`, the `etc/systemd/system/multi-user.target.wants/` symlinks (so they are enabled at build time), a system-preset, `/usr/lib/udev/rules.d/61-catprinter-btusb.rules`, and `/usr/lib/catprinter/{VERSION,install.sh,env.example}`. Containerfile: `COPY image-root/ /`. Zero per-machine steps on first boot and on every rebase. On such a machine `install.sh` only manages `/etc/catprinter/env` and unit state; the image always wins. See [packaging/KIT-README.md](packaging/KIT-README.md).

Config knobs live in `/etc/catprinter/env` (see `packaging/env.example`): `CATPRINTER_DEVICE` (pin one printer), `CATPRINTER_MODEL` (`auto|mxw01|classic`), `CATPRINTER_PRINTER_WAIT`, `CATPRINTER_PORT`, `CATPRINTER_DNSSD`, `CATPRINTERD_ARGS` (e.g. `--fake-printer DIR` for testing).

## In the print dialog — What will the cat eat today?

| Setting | Choices | What happens |
|---|---|---|
| Media / paper size | **48x297mm** (tape, default), 48x500mm, A4, Letter, Custom 48×(25–5000) mm | Tape sizes: white margins are trimmed and the content fills the 384-dot head. A4/Letter: the whole page is shrunk to the tape width (miniature). |
| Print quality | **Normal** (drawings), Draft (sharp text), High (photos → 16-level grayscale on the MXW01) | selects dithering / grayscale / burn intensity |
| Copies, n-up, landscape | as usual | CUPS handles them |

Kids never need to touch these; the defaults print drawings and text nicely.  
Never make CatPrinter the *default* printer (homework on 48 mm tape is its own special chaos); `install.sh` warns if it is.

## Troubleshooting — When the cat is grumpy 😿

| Queue message (`lpstat -p CatPrinter -l`, GNOME/KDE printer applet) | What to do (cat-whisperer edition) |
|---|---|
| Cat printer not found — turn it on and keep it near the computer | Power the printer on (and close the phone app; it allows only one connection). The job continues by itself. |
| Could not connect to the cat printer… | Close the phone app; if Bluetooth Settings is holding it, turn the printer off and on. Keep it next to the computer. The job keeps trying. |
| Bluetooth is turned off on this computer | Turn Bluetooth on (`rfkill unblock bluetooth`). The cat cannot hear you otherwise. |
| The cat printer is out of paper. | Load a roll, close the lid. Hungry cats need paper. |
| The cat printer is too hot / battery is low | Wait a minute / charge it. Even cats need rest. |
| Cat printer not found for 2 min — job N stopped | The job gave up; turn the printer on and print again. |
| Print would be … long; limit … | Pick a shorter page size or split the document. The tape has limits. |
| The print stopped partway (Bluetooth dropped)… | Move the printer next to the computer and print again (a retry would reprint the bit that already came out). |
| queue missing / daemon down | `sudo ./install.sh status`, `journalctl -u catprinter -u catprinter-queue`, `sudo ./install.sh update` |
| two “Cat Printer” entries in the dialog | the daemon adopts the CUPS queue’s uuid within a minute; if it persists, `sudo systemctl restart catprinter`. |

`catprinterd check` (Bluetooth adapter / bluetoothd / port, plus read-only host facts: TemporaryTimeout, combo, `bt chip`, USB BT `power/control`, `udev`, `Experimental`) and `catprinterd status` (connects to the printer, reports model, battery, paper) are handy on the console. `catprinterd adopt` remembers the printer so the next print does not have to rediscover it (`adopt --status` asks; first successful print does this by itself). `catprinterd doctor` / `doctor --json` answers the questions an installer used to grep BlueZ for: trusted LE? would a connect go LE or Classic? is `ConnectDevice` there? is the queue pointed at us? `bt chip` names the USB Bluetooth family (realtek / mediatek / qca / intel / broadcom) even when the laptop badge is Foxconn or Azurewave. `udev present` means the combo-card autosuspend rule is installed. `Experimental true` means BlueZ will expose `ConnectDevice` so we can connect **LE**, not Classic.

## Developing — For the grown-up cats who like to tinker

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

* `--fake-printer DIR` writes `job-N-*.png` (what the head would burn) + `job-N.json` per job and is scripted by `DIR/state` (`ok|off|no-paper|overheated|low-battery|slow|flaky:N`).  
* Raster fixtures come from CUPS itself: `scripts/make-fixtures.sh` (uses `cupsfilter`).  
* Layout: `src/protocol` (wire formats), `src/models` (registry + drivers), `src/ble` (BlueZ over D-Bus), `src/raster` (PWG decode), `src/render` (trim/fit/dither/pack), `src/ipp` + `src/http` (IPP Everywhere), `src/engine` (queue/worker), `src/dnssd` (Avahi), `src/cupsq` (uuid adoption).

---

Made so kids can simply print.  
Cat printer is ready. Linux is ready.  

Meow.
