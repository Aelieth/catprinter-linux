import os
import unittest

from catprinter import protocol as proto
from catprinter.img import read_img

MEDIA = os.path.join(os.path.dirname(__file__), "..", "media", "hackoclock.jpg")


class ImagePipelineTests(unittest.TestCase):
    def test_floyd_steinberg_resizes_and_packs(self):
        img = read_img(MEDIA, proto.PRINTER_WIDTH_PIXELS, "floyd-steinberg")
        self.assertEqual(img.shape[1], proto.PRINTER_WIDTH_PIXELS)
        self.assertGreater(img.shape[0], 1)
        self.assertEqual(img.dtype, bool)
        buf = proto.prepare_image_data(img)
        self.assertEqual(len(buf) % proto.PRINTER_WIDTH_BYTES, 0)
        self.assertGreaterEqual(len(buf), proto.MIN_DATA_BYTES)

    def test_missing_file(self):
        with self.assertRaises(RuntimeError):
            read_img("/no/such/cat.png", proto.PRINTER_WIDTH_PIXELS, "floyd-steinberg")
