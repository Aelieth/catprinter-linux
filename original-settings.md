# Original print settings (Python mxw01d era)

This is the dialog and conversion contract we built and printed with
before the Rust IPP Everywhere rewrite. The render *math* mostly
survived in `src/render/mod.rs` (`Preset` / `Tone`). The **kid-facing
controls** collapsed into Draft / Normal / High and glued High to
grayscale. That is what felt like it “sucks.”

Recorded so it is not forgotten.

---

## Two independent axes (not one “quality” knob)

Style answers *what the page is*. Tone answers *how the head burns it*.
They cross. That was the whole point.

### Print style — `CatQuality`

| Dialog | CLI | Dither (1 bpp) | Intensity | Trim white | Unsharp after 384-px scale | Grayscale curve (k / midtone) | 16-level FS when tone=gray |
|---|---|---|---|---|---|---|---|
| **Default** | `-q default` | Floyd–Steinberg | `0x5D` | yes | yes | 1.1 / 1.12 | yes |
| **Picture** | `-q picture` | Floyd–Steinberg | `0x78` (hotter) | yes | yes | 1.6 / 1.22 | yes |
| **Text** | `-q text` | threshold (`gray < 180`) | `0x68` | yes | no | 2.4 / 1.35 | no (straight quantize) |
| **Document** | `-q document` | threshold | `0x68` | **no** — whole sheet | yes | 2.4 / 1.35 | no |

Aliases: `auto`/`normal` → default; `photo`/`graphics`/`high` → picture; `draft` → text; `doc` → document.

**Document** is the homework sentence: “Tell it to print Document.”
LibreOffice lays out to the printer’s paper size. On tape-only paper it
clips a 48 mm sliver from the middle. Document must make the app emit a
full A4 (or Letter) page, then we scale that *entire* page to 384 px
(about 4.4× shrink). Titles stay at the top, last line at the bottom.

PPD: `CatQuality Document` ran `setpagedevice` A4, and UIConstraints
forbade combining Document with Roll48 / Roll48Long.

**In the remade driver (no vendor PPD):** pick paper size **Document A4**
or **Document Letter**. Sheet media selects the Document preset (threshold,
no trim, `0x68`) so homework type stays solid.

### Tone — `CatTone` (orthogonal)

| Dialog | CLI | Mode | Packing |
|---|---|---|---|
| **Black and white** | `--tone blackwhite` | A9 `0x00` 1 bpp | 48 bytes/row, LSB-left |
| **Grayscale** | `--tone grayscale` | A9 `0x02` 4 bpp | 192 bytes/row, 16 levels, even x = high nibble |

Default tone: **BlackWhite** (do not surprise existing 1-bit prints).

Picture + Grayscale was the quality photo path (hackoclock used all 16
levels). Text/Document + Grayscale kept glyphs solid.

**In the remade driver:** IPP `print-color-mode` `bi-level` = Black and
white, `monochrome` = Grayscale. Print quality must **not** force tone.

### Paper type — `MediaType` (label only)

| Dialog | Meaning |
|---|---|
| **Paper** | Thermal roll (`stationery`) |
| **Sticker** | Same coating, glue on the back (`labels`) |

**Does not change intensity, dither, or BLE.** Same 48 mm head. Kids
should not see “Plain paper” like a laser.

### Page sizes

| PPD / PWG name | Kid label | Points | mm | Role |
|---|---|---|---|---|
| Roll48 / `custom_cat-tape_48x297mm` | Cat tape 48 mm | 136 × 842 | 48 × 297 | **Default.** Paint / doodles. |
| Roll48Long / `custom_cat-tape-long_48x500mm` | Cat tape long | 136 × 1417 | 48 × 500 | Long receipt. |
| DocA4 / `iso_a4_210x297mm` | Document A4 | 595 × 842 | 210 × 297 | Homework miniature. |
| DocLetter / `na_letter_8.5x11in` | Document Letter | 612 × 792 | 8.5 × 11 in | Same, US. |

Default page size stays tape so GTK preview is a skinny strip, not A4.
203 dpi. Apps that still ship A4 PDFs (Firefox) keep working:
Default/Picture/Text trim near-white then scale to 384 when the *job*
is on tape media.

---

## Conversion (quality, not `convert("L")`)

1. Flatten alpha onto **white** (Pillow RGBA-on-black was a black slab).
2. Rec. 709 luma (`0.2126 R + 0.7152 G + 0.0722 B`).
3. Trim (unless Document / sheet).
4. LANCZOS to width 384; cap height.
5. Mild unsharp (Default / Picture / Document).
6. Then either 1 bpp dither or thermal S-curve + 16-level serpentine FS.
7. Rotate 180° by default (tape comes out right-side up).

4 bpp pack (MaikelChan): `level = (255-gray) >> 4`, 0=white, 15=black.

Thermal preview: 384-px PNG + 48 mm PDF at 203 dpi. GTK live preview
only shows layout at the advertised page size — it cannot show our dither.

---

## Kid sentences

- Drawings / paint: **Cat tape 48 mm**, Default or Picture.
- Photos / crayon: **Picture** + **Grayscale**.
- Stick figures / type on tape: **Text** + **Black and white**.
- LibreOffice homework: paper size **Document A4** (or Letter).
- Sticker vs paper: pick the label if you want; the burn is the same.
- Never make Cat Printer the system default printer.

---

## What went wrong in the Everywhere rewrite

CUPS’s `ipp` backend only forwards standard attributes. The daemon mapped:

| print-quality | Result |
|---|---|
| 3 Draft | Text + BlackWhite |
| 4 Normal | Default + BlackWhite |
| 5 High | Picture **and** Grayscale |

That destroyed the orthogonal tone axis and hid Document as a named
choice. `scripts/fixture.ppd` still has the old vendor options for
fixture generation only.

The Rust `Preset` and `Tone` enums already encode the table above
(intensity, trim, unsharp, curve, gray_dither). Restore the **dialog
contract** on top of those enums using standard IPP:

- print-quality 3/4/5 → Text / Default / Picture (names, not Draft/Normal/High)
- print-color-mode bi-level / monochrome → BlackWhite / Grayscale
- A4/Letter media → Document preset + sheet layout
- media-type stationery / labels → Paper / Sticker (ignored for burn)
