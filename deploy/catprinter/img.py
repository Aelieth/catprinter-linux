"""Load and dither images for the 384-px MXW01."""

from __future__ import annotations

from math import ceil

import numpy as np
from PIL import Image, ImageDraw, ImageFilter

from catprinter import logger

# Rec. 709 luma. Pillow convert("L") is Rec. 601 and composites RGBA onto black.
_LUMA_R = 0.2126
_LUMA_G = 0.7152
_LUMA_B = 0.0722


def flatten_to_rgb(im: Image.Image) -> Image.Image:
    """Composite any alpha onto white, then return RGB."""
    if im.mode in {"RGBA", "LA"} or "transparency" in im.info:
        rgba = im.convert("RGBA")
        bg = Image.new("RGBA", rgba.size, (255, 255, 255, 255))
        bg.alpha_composite(rgba)
        return bg.convert("RGB")
    if im.mode != "RGB":
        return im.convert("RGB")
    return im


def image_to_luma(im: Image.Image) -> np.ndarray:
    """Rec. 709 grayscale, uint8, 0=black 255=white. Alpha becomes white paper."""
    rgb = np.asarray(flatten_to_rgb(im), dtype=np.float64)
    y = _LUMA_R * rgb[:, :, 0] + _LUMA_G * rgb[:, :, 1] + _LUMA_B * rgb[:, :, 2]
    return np.clip(np.rint(y), 0, 255).astype(np.uint8)


def unsharp_gray(
    gray: np.ndarray,
    radius: float = 1.0,
    percent: int = 55,
    threshold: int = 2,
) -> np.ndarray:
    """Mild unsharp after LANCZOS so crayon edges survive the 384-px shrink."""
    im = Image.fromarray(gray, mode="L")
    im = im.filter(ImageFilter.UnsharpMask(radius=radius, percent=percent, threshold=threshold))
    return np.array(im, dtype=np.uint8)


def thermal_curve(
    gray: np.ndarray,
    contrast: float = 1.1,
    midtone: float = 1.12,
) -> np.ndarray:
    """Park more of the image in the MXW01's printable mid-grays.

    Input/output are 0=black, 255=white. `midtone` > 1 darkens the middle so
    light crayon actually burns. `contrast` is the S-curve k (0 = skip).
    """
    x = np.clip(gray.astype(np.float64) / 255.0, 0.0, 1.0)
    if midtone > 0 and midtone != 1.0:
        x = np.power(x, midtone)
    if contrast and contrast > 0:
        denom = np.tanh(contrast * 0.5)
        if denom != 0:
            x = 0.5 * (1.0 + np.tanh(contrast * (x - 0.5)) / denom)
    return np.clip(np.rint(x * 255.0), 0, 255).astype(np.uint8)


def quantize_16(gray: np.ndarray) -> np.ndarray:
    """Straight 16-bin quantize matching the MXW01 packer: 0=white .. 15=black."""
    return ((255 - gray.astype(np.uint16)) >> 4).astype(np.uint8)


def quantize_16_serpentine_fs(gray: np.ndarray) -> np.ndarray:
    """16-level serpentine Floyd–Steinberg onto the MXW01 4 bpp bins.

    Reconstruction uses each bin's center so a smooth ramp uses many levels
    instead of banding. Alternate rows run right-to-left to cut FS worms.
    """
    work = gray.astype(np.float64)
    height, width = work.shape
    out = np.empty((height, width), dtype=np.uint8)
    for y in range(height):
        if y % 2 == 0:
            xs = range(width)
            step = 1
        else:
            xs = range(width - 1, -1, -1)
            step = -1
        for x in xs:
            val = work[y, x]
            if val < 0:
                val = 0.0
            elif val > 255:
                val = 255.0
            level = int(255 - val) >> 4
            if level < 0:
                level = 0
            elif level > 15:
                level = 15
            out[y, x] = level
            recon = 247.0 - 16.0 * level
            err = val - recon
            nx = x + step
            if 0 <= nx < width:
                work[y, nx] += err * (7.0 / 16.0)
            ny = y + 1
            if ny < height:
                px = x - step
                if 0 <= px < width:
                    work[ny, px] += err * (3.0 / 16.0)
                work[ny, x] += err * (5.0 / 16.0)
                if 0 <= nx < width:
                    work[ny, nx] += err * (1.0 / 16.0)
    return out


def floyd_steinberg_dither(img: np.ndarray) -> np.ndarray:
    """Floyd-Steinberg dithering, in place. 8-bit grayscale in, 0/255 out."""
    h, w = img.shape

    def adjust_pixel(y, x, delta):
        if y < 0 or y >= h or x < 0 or x >= w:
            return
        img[y][x] = min(255, max(0, img[y][x] + delta))

    for y in range(h):
        for x in range(w):
            new_val = 255 if img[y][x] > 127 else 0
            err = int(img[y][x]) - new_val
            img[y][x] = new_val
            adjust_pixel(y, x + 1, err * 7 / 16)
            adjust_pixel(y + 1, x - 1, err * 3 / 16)
            adjust_pixel(y + 1, x, err * 5 / 16)
            adjust_pixel(y + 1, x + 1, err * 1 / 16)
    return img


def atkinson_dither(img: np.ndarray) -> np.ndarray:
    """Atkinson dithering, in place. 8-bit grayscale in, 0/255 out."""
    h, w = img.shape

    def adjust_pixel(y, x, delta):
        if y < 0 or y >= h or x < 0 or x >= w:
            return
        img[y][x] = min(255, max(0, img[y][x] + delta))

    for y in range(h):
        for x in range(w):
            new_val = 255 if img[y][x] > 127 else 0
            err = int(img[y][x]) - new_val
            img[y][x] = new_val
            adjust_pixel(y, x + 1, err * 1 / 8)
            adjust_pixel(y, x + 2, err * 1 / 8)
            adjust_pixel(y + 1, x - 1, err * 1 / 8)
            adjust_pixel(y + 1, x, err * 1 / 8)
            adjust_pixel(y + 1, x + 1, err * 1 / 8)
            adjust_pixel(y + 2, x, err * 1 / 8)
    return img


def _filled_circle(side: int, radius: int) -> np.ndarray:
    img = Image.new("L", (side, side), 255)
    if radius > 0:
        draw = ImageDraw.Draw(img)
        cx = cy = side / 2
        draw.ellipse((cx - radius, cy - radius, cx + radius, cy + radius), fill=0)
    return np.array(img, dtype=np.uint8)


def halftone_dither(img: np.ndarray) -> np.ndarray:
    """Halftone dithering using filled circles of varying radius."""
    side = 4
    jump = 4
    alpha = 3
    height, width = img.shape
    canvas = np.zeros((side * ceil(height / jump), side * ceil(width / jump)), np.uint8)
    y_output = 0
    for y in range(0, height, jump):
        x_output = 0
        for x in range(0, width, jump):
            block = img[y : y + jump, x : x + jump]
            intensity = 1 - (block.mean() / 255)
            radius = int(alpha * intensity * side / 2)
            canvas[y_output : y_output + side, x_output : x_output + side] = _filled_circle(
                side, radius
            )
            x_output += side
        y_output += side
    return canvas


def read_img(filename, print_width, img_binarization_algo) -> np.ndarray:
    """Load, resize to print_width, dither. Returns bool array, True=black."""
    try:
        im = Image.open(filename)
        luma = image_to_luma(im)
    except Exception as exc:  # noqa: BLE001 — surface as a print-time error
        raise RuntimeError(f"Could not read image file: {filename}") from exc

    height, width = luma.shape
    new_height = max(1, int(height * (print_width / width)))
    resized_im = Image.fromarray(luma, mode="L").resize(
        (print_width, new_height), Image.Resampling.LANCZOS
    )
    resized = np.array(resized_im, dtype=np.uint8)

    if img_binarization_algo == "atkinson":
        logger.info("⏳ Applying Atkinson dithering to image...")
        dithered = atkinson_dither(resized.copy())
        logger.info("✅ Done.")
        bin_img_bool = dithered > 127
    elif img_binarization_algo == "floyd-steinberg":
        # PIL's C implementation — same algorithm, not 384×H Python loops.
        bw = resized_im.convert("1", dither=Image.Dither.FLOYDSTEINBERG)
        # PIL mode "1": 0 = black. Encoder wants True = black.
        return np.array(bw, dtype=np.uint8) == 0
    elif img_binarization_algo == "halftone":
        logger.info("⏳ Applying halftone dithering to image...")
        dithered = halftone_dither(resized.copy())
        logger.info("✅ Done.")
        bin_img_bool = dithered > 127
    elif img_binarization_algo == "mean-threshold":
        bin_img_bool = resized > resized.mean()
    elif img_binarization_algo == "none":
        if width == print_width:
            bin_img_bool = luma > 127
        else:
            raise RuntimeError(
                f"Wrong width of {width} px. "
                f"An image with a width of {print_width} px "
                f'is required for "none" binarization'
            )
    else:
        raise RuntimeError(
            f"unknown image binarization algorithm: {img_binarization_algo}"
        )

    # Invert: True must mean "burn this pixel" (black on thermal paper).
    return ~bin_img_bool
