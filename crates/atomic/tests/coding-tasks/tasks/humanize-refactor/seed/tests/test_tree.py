import os
import tempfile
import unittest

from duscan.scanner import directory_sizes, entries_from_spec, scan
from duscan.tree import biggest_directories, render_tree

SPEC = {"a/one.txt": 1000, "a/b/two.bin": 3000, "c/three.log": 24, "top.txt": 7}


class Tree(unittest.TestCase):
    def test_directory_sizes(self):
        sizes = directory_sizes(entries_from_spec(SPEC))
        self.assertEqual(sizes, {"": 4031, "a": 4000, "a/b": 3000, "c": 24})

    def test_render(self):
        text = render_tree(entries_from_spec(SPEC))
        self.assertEqual(text.splitlines()[0], "    3.9 KiB  .")
        self.assertIn("    2.9 KiB      b/", text)
        self.assertIn("       24 B      three.log", text)

    def test_depth_and_dirs_only(self):
        text = render_tree(entries_from_spec(SPEC), max_depth=1, files=False)
        self.assertEqual([line.split()[-1] for line in text.splitlines()], [".", "a/", "c/"])

    def test_biggest(self):
        self.assertEqual(biggest_directories(entries_from_spec(SPEC), 2), [("a", 4000), ("a/b", 3000)])


class Scan(unittest.TestCase):
    def test_scan_matches_spec(self):
        with tempfile.TemporaryDirectory() as root:
            for path, size in SPEC.items():
                os.makedirs(os.path.join(root, os.path.dirname(path)), exist_ok=True)
                with open(os.path.join(root, path), "wb") as f:
                    f.write(b"x" * size)
            os.makedirs(os.path.join(root, ".cache"))
            with open(os.path.join(root, ".cache", "blob"), "wb") as f:
                f.write(b"y" * 10)
            self.assertEqual(scan(root, include_hidden=False), entries_from_spec(SPEC))
            self.assertEqual(len(scan(root)), len(entries_from_spec(SPEC)) + 2)


if __name__ == "__main__":
    unittest.main()
