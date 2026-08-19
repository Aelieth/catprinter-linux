# catprinterd — your tiny Bluetooth cat printer finally feels at home on Linux 🐱🖨️

<img src="media/catprinter-with-nyancat.jpg" alt="Catprinter with Nyan Cat" width="400">

*Meet the star: a pocket-sized Bluetooth thermal cat that prints other cats (Nyan!) and whatever else a kid (or tired adult) desires — once Linux is taught how to talk to it properly.*

You bought the cutest, cheapest little thermal printer on the internet. It has a face. It has ears. It costs less than lunch. And then you tried to use it on Linux.

Bluetooth Low Energy + BlueZ + combo wireless cards that secretly dislike cats + an official phone app that hogs the single connection = a special kind of chaos. An entire program had to be written so this catty device would behave like a normal printer.

**catprinterd** is the better way: a small, hardened Rust daemon that turns your cat printer into a real system **IPP Everywhere** printer. Unlike the phone app, it doesn’t hog the only Bluetooth connection, works for every user on the machine, survives reboots, and appears in the normal File → Print dialog. No pairing. No phone required after the one-time setup.

Supported models (auto-detected): **MXW01** (with lovely 16-level grayscale) and the classic family **GB0x / GT01 / MX0x / YT01 / X5 / X6**.


```
File → Print  →  CUPS  →  catprinterd  →  Bluetooth  →  🐈 *purrs and prints*
```

### The Sacred Kid Contract 🤝

1. Bluetooth is on  
2. The cat printer is on  
3. Hit **Print**

No MAC addresses. No pairing. No phone apps.

**Hold-and-wait magic:** If the printer is off or napping, the job waits patiently in our queue (2 minutes by default) and the status says *“Cat printer not found — turn it on and keep it near the computer.”* Switch it on → it prints.  

The little cats fall asleep after about 5–6 minutes of boredom. If yours is already awake, the print usually starts in just a few seconds. A longer wait only blocks the queue while the cat is still sleeping. (The phone app can’t do this.)

### Quick Install — Make the cat official

First, give the cat its treats:

| Distro              | Packages |
|---------------------|----------|
| Fedora / RHEL / openSUSE | `sudo dnf install cups cups-filters bluez avahi util-linux policycoreutils curl` |
| Debian / Ubuntu     | `sudo apt install cups cups-filters cups-ipp-utils bluez avahi-daemon avahi-utils rfkill curl` |
| Arch                | `sudo pacman -S cups cups-filters bluez bluez-utils avahi util-linux curl` |

Prebuilt kits are available for **x86_64** and **aarch64** (Raspberry Pi friendly). Or build with `cargo build --release`.

Then:

```sh
sudo ./install.sh          # install or upgrade
sudo ./install.sh status   # how is the cat feeling?
```

The printer appears as **CatPrinter** in every print dialog.

> ⚠️ **Please do not make CatPrinter the system default.**  
> Homework (or any long document) on 48 mm thermal tape is its own special kind of chaos. Use a normal printer for schoolwork.  
>  
> The **Cat Minidoc** (Document) option is perfect for mini flyers, cute notes, and stylized little pages — it shrinks a full page into a readable strip the cat can handle.

### What the cat loves to print 🎨

| What you want            | Paper                | Style     | Tone          |
|--------------------------|----------------------|-----------|---------------|
| Doodles / stickers / notes | Cat Tape short      | Default   | Black & white |
| Photos / crayon art      | Cat Tape short       | Picture   | Grayscale     |
| Mini flyers / stylized pages | Cat Minidoc A4 or Letter | Text or Default | Black & white |

## Customized Cat-tastic options

| Setting | Choices | What happens |
|---|---|---|
| **Paper** | **Cat Tape short** (default) · Cat Tape long · **Cat Minidoc A4** · Cat Minidoc Letter · custom 48×(25–5000) mm | Tape: trim white, fill the 384-dot head. Minidoc: the app emits a full A4/Letter page; we shrink that *entire* page to 384 dots (~4.4×), leftover **left/right** white only (title at the top, last line at the bottom). |
| **Print style** | **Default** (drawings) · Text (sharp glyphs) · Picture (hotter dither) | Dither and heat. Works on tape *and* Minidoc. Not Draft/Normal/High. |
| **Tone** | **Black and white** (default, fast 1-bit) · Grayscale | 16-level burn only for **Picture + Grayscale** (the photo path; slower on purpose). |
| **Paper type** | **Paper** · Sticker | A label for kids. Same heat. |
| Copies, n-up, landscape | as usual | CUPS already knows how. |

**A little about your cat printer**  
It prints on 48 mm thermal paper or stickers with a 384-dot head. The MXW01 can do beautiful 16-level grayscale (especially with Picture + Grayscale). You get three thoughtful styles (Default for drawings, Text for sharp letters, Picture for hotter, richer dithering) that the phone app doesn’t match as nicely. Paper type (Paper or Sticker) is just a friendly label for kids — the heat is the same. Cat Minidoc shrinks a whole A4 or Letter page (~4.4×) into a neat, readable strip with the title at the top and white margins on the sides — ideal for flyers and creative notes.

### If the cat is grumpy 😿

Start with:

```sh
sudo ./install.sh status
```

Common messages you might see:

| Message | What to do |
|---------|------------|
| Cat printer not found — turn it on and keep it near the computer | Power the printer on (and close the phone app — it only allows one connection). The job will continue by itself. |
| Could not connect to the cat printer… | Close the phone app or turn the printer off and on. Keep it next to the computer. |
| Bluetooth is turned off on this computer | Turn Bluetooth on. The cat can’t hear you otherwise. |
| The cat printer is out of paper | Load a new roll and close the lid. Hungry cats need paper! |
| The cat printer is too hot / battery is low | Give it a minute to cool down, or plug it in to charge. |

For anything else, the status command and the rest of the repository have more help.

---

Made so kids (and tired adults) can simply print — better than the phone app ever did.  
Cat printer is ready. Linux is ready. 🐾
