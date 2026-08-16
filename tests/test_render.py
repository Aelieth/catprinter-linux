import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

import numpy as np
from PIL import Image

from catprinter import protocol as proto
from catprinter.img import (
    image_to_luma,
    quantize_16,
    quantize_16_serpentine_fs,
    thermal_curve,
)
from catprinter.render import (
    binarize,
    fit_width,
    load_gray,
    normalize_quality,
    normalize_tone,
    preview_dir,
    preview_image,
    render_job,
    render_to_buffer,
    trim_whitespace,
    write_preview,
)


class QualityTests(unittest.TestCase):
    def test_aliases(self):
        self.assertEqual(normalize_quality("Picture"), "picture")
        self.assertEqual(normalize_quality("photo"), "picture")
        self.assertEqual(normalize_quality("TEXT"), "text")
        self.assertEqual(normalize_quality("Document"), "document")
        self.assertEqual(normalize_quality("doc"), "document")
        self.assertEqual(normalize_quality("auto"), "default")
        with self.assertRaises(RuntimeError):
            normalize_quality("neon")

    def test_tone_aliases(self):
        self.assertEqual(normalize_tone("Grayscale"), "grayscale")
        self.assertEqual(normalize_tone("BlackWhite"), "blackwhite")
        self.assertEqual(normalize_tone("grey"), "grayscale")
        self.assertEqual(normalize_tone("mono"), "blackwhite")
        self.assertEqual(normalize_tone(None), "blackwhite")
        with self.assertRaises(RuntimeError):
            normalize_tone("sepia")


class TrimScaleTests(unittest.TestCase):
    def test_trim_doodle_on_a4(self):
        # Fake A4 @ ~100 dpi: big white page, small black mark near the middle.
        page = np.full((1100, 800), 255, dtype=np.uint8)
        page[400:520, 300:500] = 0
        cropped = trim_whitespace(page, threshold=245, pad=8)
        self.assertLess(cropped.shape[0], 200)
        self.assertLess(cropped.shape[1], 250)
        self.assertTrue((cropped < 10).any())

    def test_trim_all_white(self):
        page = np.full((200, 200), 255, dtype=np.uint8)
        cropped = trim_whitespace(page)
        self.assertEqual(cropped.shape[0], 16)

    def test_fit_width_shrinks_and_grows(self):
        fat = np.zeros((100, 800), dtype=np.uint8)
        skinny = fit_width(fat, 384)
        self.assertEqual(skinny.shape[1], 384)
        self.assertGreater(skinny.shape[0], 1)

        tiny = np.zeros((20, 40), dtype=np.uint8)
        grown = fit_width(tiny, 384)
        self.assertEqual(grown.shape, (192, 384))

    def test_height_cap(self):
        tall = np.zeros((20000, 384), dtype=np.uint8)
        capped = fit_width(tall, 384, max_height=100)
        self.assertEqual(capped.shape, (100, 384))

    def test_text_threshold_keeps_ink_solid(self):
        gray = np.full((32, 384), 255, dtype=np.uint8)
        gray[8:24, 40:80] = 20
        bits = binarize(gray, "threshold")
        self.assertTrue(bits[12, 50])
        self.assertFalse(bits[0, 0])


class RenderJobTests(unittest.TestCase):
    def _doodle_png(self, path):
        im = Image.new("RGB", (800, 1100), "white")
        # A "drawing": thick black stroke + a gray blob (photo-ish).
        for x in range(200, 600):
            for t in range(-3, 4):
                y = 400 + int(40 * np.sin(x / 30)) + t
                if 0 <= y < 1100:
                    im.putpixel((x, y), (0, 0, 0))
        for y in range(700, 820):
            for x in range(300, 460):
                im.putpixel((x, y), (90, 90, 90))
        im.save(path)

    def test_png_default_is_384_and_has_ink(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "doodle.png")
            self._doodle_png(path)
            job = render_job(path=path, quality="default")
            self.assertEqual(job.bitmap.shape[1], proto.PRINTER_WIDTH_PIXELS)
            self.assertLess(job.bitmap.shape[0], 600)
            self.assertGreater(job.bitmap.shape[0], 20)
            self.assertTrue(job.bitmap.any())
            self.assertEqual(job.dither, "floyd-steinberg")

    def test_document_keeps_full_page_default_trims(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "letter.png")
            # Fake A4-ish sheet: a header, a footer, and nothing in the middle.
            im = Image.new("RGB", (800, 1100), "white")
            for y in range(20, 40):
                for x in range(100, 700):
                    im.putpixel((x, y), (0, 0, 0))
            for y in range(1060, 1080):
                for x in range(100, 700):
                    im.putpixel((x, y), (0, 0, 0))
            im.save(path)
            full = render_job(path=path, quality="document")
            self.assertEqual(full.dither, "threshold")
            self.assertEqual(full.bitmap.shape, (528, 384))  # 1100 * 384/800
            self.assertTrue(full.bitmap[:30].any())
            self.assertTrue(full.bitmap[-30:].any())

            # A small mark in the middle of a big page: Default crops, Document does not.
            doodle = os.path.join(tmp, "doodle-on-page.png")
            page = Image.new("RGB", (800, 1100), "white")
            for y in range(520, 580):
                for x in range(360, 440):
                    page.putpixel((x, y), (0, 0, 0))
            page.save(doodle)
            trimmed = render_job(path=doodle, quality="default")
            whole = render_job(path=doodle, quality="document")
            self.assertLess(trimmed.bitmap.shape[0], 400)
            self.assertEqual(whole.bitmap.shape[0], 528)
            self.assertGreater(whole.bitmap.shape[0], trimmed.bitmap.shape[0])

    def test_text_preset_uses_threshold(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "doodle.png")
            self._doodle_png(path)
            job = render_job(path=path, quality="text")
            self.assertEqual(job.dither, "threshold")
            self.assertEqual(job.bitmap.shape[1], 384)

    @unittest.skipUnless(
        shutil.which("magick") or os.path.isfile("/usr/bin/magick"),
        "ImageMagick missing",
    )
    def test_pdf_a4_trims_like_a_doodle(self):
        magick = shutil.which("magick") or "/usr/bin/magick"
        with tempfile.TemporaryDirectory() as tmp:
            png = os.path.join(tmp, "doodle.png")
            pdf = os.path.join(tmp, "doodle-on-a4.pdf")
            self._doodle_png(png)
            # Center the doodle on a white A4 canvas, then wrap as PDF.
            subprocess.run(
                [
                    magick,
                    "-page",
                    "a4",
                    png,
                    "-background",
                    "white",
                    "-gravity",
                    "center",
                    "-extent",
                    "595x842",
                    pdf,
                ],
                check=True,
                capture_output=True,
            )
            job = render_job(path=pdf, quality="default")
            self.assertEqual(job.bitmap.shape[1], 384)
            self.assertLess(job.bitmap.shape[0], 2000)
            self.assertTrue(job.bitmap.any())


class LumaAndToneTests(unittest.TestCase):
    def test_rec709_weights(self):
        red = Image.new("RGB", (8, 8), (255, 0, 0))
        green = Image.new("RGB", (8, 8), (0, 255, 0))
        blue = Image.new("RGB", (8, 8), (0, 0, 255))
        yr = int(image_to_luma(red)[0, 0])
        yg = int(image_to_luma(green)[0, 0])
        yb = int(image_to_luma(blue)[0, 0])
        self.assertEqual(yr, 54)   # round(0.2126 * 255)
        self.assertEqual(yg, 182)  # round(0.7152 * 255)
        self.assertEqual(yb, 18)   # round(0.0722 * 255)
        self.assertGreater(yg, yr)
        self.assertGreater(yr, yb)

    def test_rgba_flattens_onto_white(self):
        im = Image.new("RGBA", (16, 16), (0, 0, 0, 0))
        gray = image_to_luma(im)
        self.assertTrue((gray == 255).all())

    def test_load_gray_transparent_png(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "ghost.png")
            Image.new("RGBA", (40, 40), (0, 0, 0, 0)).save(path)
            gray = load_gray(path=path)
            self.assertTrue((gray > 250).all())

    def test_smooth_ramp_uses_many_levels(self):
        ramp = np.tile(np.linspace(0, 255, 384, dtype=np.uint8), (32, 1))
        levels = quantize_16_serpentine_fs(ramp)
        used = set(int(v) for v in np.unique(levels))
        self.assertGreaterEqual(len(used), 12)
        self.assertIn(0, used)
        self.assertIn(15, used)

    def test_straight_quantize_is_maikel_bins(self):
        gray = np.array([[0, 15, 16, 255]], dtype=np.uint8)
        levels = quantize_16(gray)
        self.assertEqual(list(levels[0]), [15, 15, 14, 0])

    def test_thermal_curve_darkens_midtones(self):
        mid = np.full((4, 4), 160, dtype=np.uint8)
        out = thermal_curve(mid, contrast=1.6, midtone=1.22)
        self.assertLess(int(out.mean()), 160)

    def test_picture_grayscale_is_4bpp(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "doodle.png")
            Image.new("RGB", (200, 80), (120, 130, 140)).save(path)
            # A mark so trim keeps something.
            im = Image.open(path)
            for x in range(20, 80):
                im.putpixel((x, 40), (10, 10, 10))
            im.save(path)
            job = render_job(path=path, quality="picture", tone="grayscale")
            self.assertEqual(job.tone, "grayscale")
            self.assertEqual(job.mode, proto.PrintModes.GRAYSCALE)
            self.assertEqual(job.levels.shape[1], 384)
            self.assertEqual(job.levels.dtype, np.uint8)
            self.assertLessEqual(int(job.levels.max()), 15)
            buf, rendered = render_to_buffer(
                path=path, quality="picture", tone="grayscale", rotate_180=False
            )
            self.assertEqual(rendered.mode, proto.PrintModes.GRAYSCALE)
            self.assertEqual(len(buf) % proto.PRINTER_WIDTH_NIBBLES, 0)
            self.assertGreaterEqual(len(buf), proto.MIN_DATA_BYTES_4BPP)

    def test_text_grayscale_keeps_solid_ink(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "glyph.png")
            im = Image.new("RGB", (384, 64), "white")
            for y in range(16, 48):
                for x in range(40, 80):
                    im.putpixel((x, y), (0, 0, 0))
            im.save(path)
            job = render_job(path=path, quality="text", tone="grayscale", trim=False)
            self.assertEqual(job.dither, "16-level")
            # Interior of the block should be near-black (high level).
            self.assertGreaterEqual(int(job.levels[32, 60]), 12)
            self.assertLessEqual(int(job.levels[2, 2]), 1)

    def test_preview_png_is_384_and_pdf_is_tape_width(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "doodle.png")
            Image.new("RGB", (200, 80), (80, 90, 100)).save(path)
            im = Image.open(path)
            for x in range(10, 60):
                im.putpixel((x, 40), (0, 0, 0))
            im.save(path)
            job = render_job(path=path, quality="picture", tone="grayscale")
            preview = preview_image(job, rotate_180=False)
            self.assertEqual(preview.size[0], 384)
            self.assertEqual(preview.mode, "L")
            self.assertGreater(len(set(np.array(preview).ravel().tolist())), 2)
            png = os.path.join(tmp, "catprinter-preview.png")
            pdf = os.path.join(tmp, "catprinter-preview.pdf")
            write_preview(job, rotate_180=False, png_path=png, pdf_path=pdf)
            self.assertTrue(os.path.isfile(png))
            self.assertTrue(os.path.isfile(pdf))
            with Image.open(png) as saved:
                self.assertEqual(saved.size[0], 384)
            raw = Path(pdf).read_bytes()
            self.assertTrue(raw.startswith(b"%PDF"))
            # 384 px @ 203 dpi ≈ 48 mm ≈ 136 pt.
            self.assertIn(b"MediaBox [ 0 0 136.", raw)

    def test_preview_defaults_to_xdg_cache(self):
        with tempfile.TemporaryDirectory() as tmp:
            cache = os.path.join(tmp, "cache")
            os.environ["XDG_CACHE_HOME"] = cache
            try:
                out = preview_dir()
                self.assertEqual(out, Path(cache) / "catprinter")
                path = os.path.join(tmp, "doodle.png")
                Image.new("RGB", (80, 40), "black").save(path)
                job = render_job(path=path, quality="default")
                png, pdf = write_preview(job, rotate_180=False)
                self.assertTrue(png.startswith(str(out)))
                self.assertTrue(os.path.isfile(png))
                self.assertTrue(os.path.isfile(pdf))
            finally:
                os.environ.pop("XDG_CACHE_HOME", None)

    def test_default_tone_is_still_1bpp(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "doodle.png")
            Image.new("RGB", (80, 40), "black").save(path)
            job = render_job(path=path, quality="default")
            self.assertEqual(job.tone, "blackwhite")
            self.assertEqual(job.mode, proto.PrintModes.MONOCHROME)
            self.assertTrue(set(np.unique(job.levels)).issubset({0, 15}))
