import os
import sys
import unittest


class VersionAndWhichTests(unittest.TestCase):
    def test_version_without_numpy(self):
        # Health checks import catprinter without pulling render/Pillow.
        banned = {m for m in sys.modules if m == "numpy" or m.startswith("PIL")}
        from catprinter import NOT_FOUND, __version__, which

        self.assertEqual(__version__, "0.1.0")
        self.assertIn("Bluetooth", NOT_FOUND)
        self.assertTrue(which("pdftoppm") or which("gs") or which("python3"))
        after = {m for m in sys.modules if m == "numpy" or m.startswith("PIL")}
        self.assertEqual(after, banned)


if __name__ == "__main__":
    unittest.main()
