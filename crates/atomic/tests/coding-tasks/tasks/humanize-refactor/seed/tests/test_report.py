import unittest

from duscan.report import by_extension, extension_of, largest, summary
from duscan.scanner import entries_from_spec

SPEC = {
    "README.md": 2_100,
    "src/app.py": 15_000,
    "src/util.py": 4_000,
    "media/intro.mov": 3_000_000,
    "media/logo.png": 48_000,
    "Makefile": 900,
}


class Report(unittest.TestCase):
    def test_extension_of(self):
        self.assertEqual(extension_of("a/b.PY"), ".py")
        self.assertEqual(extension_of("Makefile"), "")
        self.assertEqual(extension_of(".bashrc"), "")
        self.assertEqual(extension_of("x/archive.tar.gz"), ".gz")

    def test_by_extension(self):
        rows = by_extension(entries_from_spec(SPEC))
        self.assertEqual(rows[0], (".mov", 1, 3_000_000))
        self.assertEqual([r[0] for r in rows], [".mov", ".png", ".py", ".md", ""])

    def test_largest(self):
        self.assertEqual([e.path for e in largest(entries_from_spec(SPEC), 2)], ["media/intro.mov", "media/logo.png"])

    def test_summary_head(self):
        text = summary(entries_from_spec(SPEC), top=2)
        self.assertTrue(text.startswith("files: 6  directories: 2  total: 2.9 MiB\n"), text)
        self.assertIn("     2.9 MiB  media/intro.mov\n", text)
        self.assertNotIn("scanned at", text)

    def test_summary_timing(self):
        text = summary(entries_from_spec(SPEC), seconds=2.0)
        self.assertTrue(text.endswith("scanned at 1.53 MB/s\n"), text)


if __name__ == "__main__":
    unittest.main()
