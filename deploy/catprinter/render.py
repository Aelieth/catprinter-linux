"""Turn any print job (PDF / image) into a 384-px MXW01 bitmap.

Uses host tools that Aurora / Bazzite / Kinoite already ship: pdftoppm
(poppler-utils) or Ghostscript. No extra pip PDF library.
"""

from __future__ import annotations

import os
import subprocess
import tempfile
from dataclasses import dataclass
from pathlib import Path

import numpy as np
from PIL import Image

# Real A4 @ 300 dpi is ~9 MP. Refuse only the truly absurd.
Image.MAX_IMAGE_PIXELS = 40_000_000

from catprinter import logger, which
from catprinter import protocol as proto
from catprinter.img import (
    atkinson_dither,
    halftone_dither,
    image_to_luma,
    quantize_16,
    quantize_16_serpentine_fs,
    thermal_curve,
    unsharp_gray,
)

PRINT_WIDTH = proto.PRINTER_WIDTH_PIXELS
PDF_DPI = 300
TRIM_WHITE = 245
TRIM_PAD = 8
MAX_HEIGHT = 4000
MAX_RASTER_WIDTH = 2600  # a hair over A4 @ 300 dpi

# Dialog names the kids will see. Values are what we actually do.
# Style (this table) and tone (TONE_PRESETS) are independent axes.
QUALITY_PRESETS = {
    "default": {
        "dither": "floyd-steinberg",
        "intensity": 0x5D,
        "trim": True,
        "unsharp": True,
        "curve_contrast": 1.1,
        "curve_midtone": 1.12,
        "gray_dither": True,
    },
    "picture": {
        "dither": "floyd-steinberg",
        "intensity": 0x78,
        "trim": True,
        "unsharp": True,
        "curve_contrast": 1.6,
        "curve_midtone": 1.22,
        "gray_dither": True,
    },
    "text": {
        "dither": "threshold",
        "intensity": 0x68,
        "trim": True,
        "unsharp": False,
        "curve_contrast": 2.4,
        "curve_midtone": 1.35,
        "gray_dither": False,
    },
    # Full A4/Letter page, no content-trim — miniature of the sheet.
    "document": {
        "dither": "threshold",
        "intensity": 0x68,
        "trim": False,
        "unsharp": True,
        "curve_contrast": 2.4,
        "curve_midtone": 1.35,
        "gray_dither": False,
    },
}

TONE_PRESETS = {
    "blackwhite": {"mode": proto.PrintModes.MONOCHROME},
    "grayscale": {"mode": proto.PrintModes.GRAYSCALE},
}

_QUALITY_ALIASES = {
    "auto": "default",
    "normal": "default",
    "photo": "picture",
    "graphics": "picture",
    "high": "picture",
    "draft": "text",
    "doc": "document",
}


_TONE_ALIASES = {
    "bw": "blackwhite",
    "blackandwhite": "blackwhite",
    "black-and-white": "blackwhite",
    "mono": "blackwhite",
    "monochrome": "blackwhite",
    "bilevel": "blackwhite",
    "bi-level": "blackwhite",
    "gray": "grayscale",
    "grey": "grayscale",
    "greyscale": "grayscale",
}


def normalize_quality(name: str | None) -> str:
    if not name:
        return "default"
    key = name.strip().lower()
    key = _QUALITY_ALIASES.get(key, key)
    if key not in QUALITY_PRESETS:
        raise RuntimeError(
            f"Unknown print style {name!r}. Use default, picture, text, or document."
        )
    return key


def normalize_tone(name: str | None) -> str:
    if not name:
        return "blackwhite"
    key = name.strip().lower().replace("_", "").replace(" ", "")
    key = _TONE_ALIASES.get(key, key)
    # BlackWhite / black-white both collapse to blackwhite
    key = key.replace("-", "")
    key = _TONE_ALIASES.get(key, key)
    if key not in TONE_PRESETS:
        raise RuntimeError(f"Unknown tone {name!r}. Use blackwhite or grayscale.")
    return key


def detect_kind(data: bytes | None = None, path: str | os.PathLike | None = None) -> str:
    """Return 'pdf', 'png', 'jpeg', or 'image'."""
    head = b""
    if data:
        head = data[:8]
    elif path:
        with open(path, "rb") as fh:
            head = fh.read(8)
        suffix = Path(path).suffix.lower()
        if suffix == ".pdf":
            return "pdf"
        if suffix in {".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".tif", ".tiff"}:
            return "image"
    if head.startswith(b"%PDF"):
        return "pdf"
    return "image"


def trim_whitespace(gray: np.ndarray, threshold: int = TRIM_WHITE, pad: int = TRIM_PAD) -> np.ndarray:
    """Crop to the ink bounding box. All-white input stays a tiny white strip."""
    if gray.ndim != 2:
        raise ValueError("trim_whitespace expects a 2D grayscale array")
    ink = gray < threshold
    if not ink.any():
        return np.full((16, gray.shape[1]), 255, dtype=np.uint8)
    rows = np.any(ink, axis=1)
    cols = np.any(ink, axis=0)
    r0, r1 = np.flatnonzero(rows)[[0, -1]]
    c0, c1 = np.flatnonzero(cols)[[0, -1]]
    r0 = max(0, int(r0) - pad)
    c0 = max(0, int(c0) - pad)
    r1 = min(gray.shape[0] - 1, int(r1) + pad)
    c1 = min(gray.shape[1] - 1, int(c1) + pad)
    return gray[r0 : r1 + 1, c0 : c1 + 1]


def fit_width(gray: np.ndarray, width: int = PRINT_WIDTH, max_height: int = MAX_HEIGHT) -> np.ndarray:
    """Scale so the head width is exact. LANCZOS both up and down."""
    h, w = gray.shape
    if w < 1 or h < 1:
        raise ValueError("empty image")
    new_h = max(1, int(round(h * (width / w))))
    if new_h > max_height:
        logger.warning("Truncating print from %s to %s lines", new_h, max_height)
        # Scale as if we will crop the bottom after fitting width.
        new_h = max_height
        # Recompute width-preserving scale then crop — keep width == 384.
        scale = width / w
        scaled_h = max(1, int(round(h * scale)))
        im = Image.fromarray(gray, mode="L").resize((width, scaled_h), Image.Resampling.LANCZOS)
        return np.array(im, dtype=np.uint8)[:max_height]
    if w == width and h == new_h:
        return gray
    im = Image.fromarray(gray, mode="L").resize((width, new_h), Image.Resampling.LANCZOS)
    return np.array(im, dtype=np.uint8)


def binarize(gray: np.ndarray, dither: str) -> np.ndarray:
    """Grayscale uint8 → bool array, True = black (burn)."""
    if dither == "floyd-steinberg":
        bw = Image.fromarray(gray, mode="L").convert("1", dither=Image.Dither.FLOYDSTEINBERG)
        return np.array(bw, dtype=np.uint8) == 0
    if dither == "atkinson":
        return atkinson_dither(gray.copy()) <= 127
    if dither == "halftone":
        dithered = halftone_dither(gray.copy())
        # Halftone can change geometry; fit again.
        if dithered.shape[1] != PRINT_WIDTH:
            dithered = fit_width(dithered, PRINT_WIDTH)
        return dithered <= 127
    if dither == "mean-threshold":
        return gray < gray.mean()
    if dither == "threshold":
        return gray < 180
    if dither == "none":
        return gray < 128
    raise RuntimeError(f"unknown dither {dither!r}")


def _rasterize_pdf(path: str, dpi: int = PDF_DPI) -> list[np.ndarray]:
    pdftoppm = which("pdftoppm")
    gs = which("gs")
    with tempfile.TemporaryDirectory(prefix="catprinter-") as tmp:
        prefix = os.path.join(tmp, "page")
        if pdftoppm:
            cmd = [pdftoppm, "-r", str(dpi), "-gray", path, prefix]
            logger.info("⏳ Rasterizing PDF with pdftoppm (%s dpi)...", dpi)
        elif gs:
            cmd = [
                gs,
                "-dSAFER",
                "-dBATCH",
                "-dNOPAUSE",
                "-dQUIET",
                "-sDEVICE=pnggray",
                f"-r{dpi}",
                f"-sOutputFile={prefix}-%d.png",
                path,
            ]
            logger.info("⏳ Rasterizing PDF with Ghostscript (%s dpi)...", dpi)
        else:
            raise RuntimeError(
                "Cannot rasterize PDF: install poppler-utils (pdftoppm) or ghostscript. "
                "Both ship on Aurora / Bazzite."
            )
        try:
            subprocess.run(cmd, check=True, capture_output=True)
        except subprocess.CalledProcessError as exc:
            err = (exc.stderr or exc.stdout or b"").decode("utf-8", "replace")
            raise RuntimeError(f"PDF rasterize failed: {err.strip() or exc}") from exc

        pages: list[np.ndarray] = []
        files = sorted(
            p
            for p in Path(tmp).iterdir()
            if p.is_file() and p.suffix.lower() in {".pgm", ".png", ".ppm"}
        )
        if not files:
            raise RuntimeError("PDF rasterize produced no pages")
        for page in files:
            im = Image.open(page).convert("L")
            arr = np.array(im, dtype=np.uint8)
            if arr.shape[1] > MAX_RASTER_WIDTH:
                arr = fit_width(arr, MAX_RASTER_WIDTH, max_height=MAX_HEIGHT * 3)
            pages.append(arr)
        return pages


def load_gray(
    path: str | os.PathLike | None = None,
    data: bytes | None = None,
    filename_hint: str = "",
) -> np.ndarray:
    """Load a PDF or image as one grayscale page stack (pages concatenated)."""
    kind = detect_kind(data=data, path=path or filename_hint or None)
    tmp_path = None
    try:
        if data is not None and kind == "pdf":
            fd, tmp_path = tempfile.mkstemp(prefix="catprinter-", suffix=".pdf")
            os.close(fd)
            Path(tmp_path).write_bytes(data)
            path = tmp_path
        if kind == "pdf":
            if not path:
                raise RuntimeError("PDF job has no file")
            pages = _rasterize_pdf(str(path))
            if len(pages) == 1:
                return pages[0]
            width = max(p.shape[1] for p in pages)
            padded = []
            for page in pages:
                if page.shape[1] < width:
                    pad = np.full((page.shape[0], width - page.shape[1]), 255, dtype=np.uint8)
                    page = np.concatenate([page, pad], axis=1)
                padded.append(page)
            return np.concatenate(padded, axis=0)

        if data is not None:
            from io import BytesIO

            im = Image.open(BytesIO(data))
        elif path:
            im = Image.open(path)
        else:
            raise RuntimeError("No document to print")
        return image_to_luma(im)
    except RuntimeError:
        raise
    except Exception as exc:  # noqa: BLE001
        raise RuntimeError(f"Could not read document: {exc}") from exc
    finally:
        if tmp_path:
            Path(tmp_path).unlink(missing_ok=True)


@dataclass(frozen=True)
class RenderedJob:
    bitmap: np.ndarray  # bool, True=black, width 384 (preview / 1 bpp)
    levels: np.ndarray  # uint8, 0=white .. 15=black (4 bpp; 0/15 for 1 bpp)
    intensity: int
    quality: str
    dither: str
    tone: str
    mode: int


def render_job(
    path: str | os.PathLike | None = None,
    data: bytes | None = None,
    filename_hint: str = "",
    quality: str = "default",
    tone: str = "blackwhite",
    dither: str | None = None,
    trim: bool | None = None,
    intensity: int | None = None,
) -> RenderedJob:
    quality = normalize_quality(quality)
    tone = normalize_tone(tone)
    preset = QUALITY_PRESETS[quality]
    mode = TONE_PRESETS[tone]["mode"]
    dither = dither or preset["dither"]
    do_trim = preset["trim"] if trim is None else trim
    burn = preset["intensity"] if intensity is None else intensity

    gray = load_gray(path=path, data=data, filename_hint=filename_hint)
    if do_trim:
        gray = trim_whitespace(gray)
    gray = fit_width(gray, PRINT_WIDTH, MAX_HEIGHT)
    if preset.get("unsharp"):
        gray = unsharp_gray(gray)

    if tone == "grayscale":
        curved = thermal_curve(
            gray,
            contrast=float(preset["curve_contrast"]),
            midtone=float(preset["curve_midtone"]),
        )
        if preset.get("gray_dither"):
            levels = quantize_16_serpentine_fs(curved)
            dither = "16-level-floyd-steinberg"
        else:
            levels = quantize_16(curved)
            dither = "16-level"
        bitmap = levels >= 8
    else:
        bitmap = binarize(gray, dither)
        if bitmap.shape[1] != PRINT_WIDTH:
            as_gray = np.where(bitmap, 0, 255).astype(np.uint8)
            bitmap = fit_width(as_gray, PRINT_WIDTH) < 128
        levels = bitmap.astype(np.uint8) * 15

    logger.info(
        "✅ Rendered %s/%s (%s, %s): %s",
        quality,
        tone,
        dither,
        "trimmed" if do_trim else "full page",
        bitmap.shape,
    )
    return RenderedJob(
        bitmap=bitmap,
        levels=levels,
        intensity=burn,
        quality=quality,
        dither=dither,
        tone=tone,
        mode=mode,
    )


def render_to_buffer(
    path: str | os.PathLike | None = None,
    data: bytes | None = None,
    filename_hint: str = "",
    quality: str = "default",
    tone: str = "blackwhite",
    dither: str | None = None,
    trim: bool | None = None,
    intensity: int | None = None,
    rotate_180: bool = True,
) -> tuple[bytes, RenderedJob]:
    job = render_job(
        path=path,
        data=data,
        filename_hint=filename_hint,
        quality=quality,
        tone=tone,
        dither=dither,
        trim=trim,
        intensity=intensity,
    )
    bitmap = job.bitmap
    levels = job.levels
    if rotate_180:
        bitmap = np.rot90(bitmap, k=2)
        levels = np.rot90(levels, k=2)
    if job.mode == proto.PrintModes.GRAYSCALE:
        return proto.prepare_image_data_4bpp(levels), job
    return proto.prepare_image_data(bitmap), job


PREVIEW_DPI = 203
PREVIEW_STEM = "catprinter-preview"


def preview_dir() -> Path:
    """Somewhere the invoking user can always write.

    NOT the current directory. The deployed tree is root-owned and the user
    unit sets WorkingDirectory to it, so a relative filename means EACCES in a
    directory nobody should be writing to anyway.
    """
    base = os.environ.get("XDG_CACHE_HOME") or os.path.expanduser("~/.cache")
    target = Path(base) / "catprinter"
    try:
        target.mkdir(parents=True, exist_ok=True)
        probe = target / ".writable"
        probe.touch()
        probe.unlink()
        return target
    except OSError:
        return Path(tempfile.gettempdir())


def preview_image(job: RenderedJob, rotate_180: bool = True) -> Image.Image:
    """White paper, burned ink. Same pixels the head will print."""
    if job.tone == "grayscale":
        arr = ((15 - job.levels.astype(np.uint16)) * 17).astype(np.uint8)
    else:
        arr = np.where(job.bitmap, 0, 255).astype(np.uint8)
    if rotate_180:
        arr = np.rot90(arr, k=2)
    return Image.fromarray(arr, mode="L")


def write_preview(
    job: RenderedJob,
    rotate_180: bool = True,
    png_path: str | None = None,
    pdf_path: str | None = None,
) -> tuple[str, str]:
    """Write a 384-px PNG and a 48 mm × content-height PDF at 203 dpi."""
    out = preview_dir()
    png_path = str(png_path or out / f"{PREVIEW_STEM}.png")
    pdf_path = str(pdf_path or out / f"{PREVIEW_STEM}.pdf")
    im = preview_image(job, rotate_180=rotate_180)
    im.save(png_path)
    im.save(pdf_path, "PDF", resolution=float(PREVIEW_DPI))
    logger.info("ℹ️  Preview written to %s and %s", png_path, pdf_path)
    return png_path, pdf_path
