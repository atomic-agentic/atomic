import os
import unittest

from deployctl.loader import LoadError, load
from tests.files import TempTree


class Includes(unittest.TestCase):
    def setUp(self):
        self.tree = TempTree().__enter__()
        self.cwd = os.getcwd()
        os.chdir(self.tree.root)

    def tearDown(self):
        os.chdir(self.cwd)
        self.tree.__exit__(None, None, None)

    def test_include_merges_where_it_appears(self):
        self.tree.write("base.conf", "[a]\nx = base\ny = base\n")
        top = self.tree.write("top.conf", "[a]\nx = top\ninclude = base.conf\nz = top\n")
        config = load(top)
        self.assertEqual(config.get("a", "x"), "base")
        self.assertEqual(config.get("a", "z"), "top")
        self.assertEqual(config.value("a", "y").path, os.path.join(self.tree.root, "base.conf"))
        self.assertEqual(config.files, [top, os.path.join(self.tree.root, "base.conf")])

    def test_later_setting_wins(self):
        self.tree.write("base.conf", "[a]\nx = base\n")
        top = self.tree.write("top.conf", "include = base.conf\n[a]\nx = top\n")
        self.assertEqual(load(top).get("a", "x"), "top")

    def test_nested(self):
        self.tree.write("one.conf", "include = two.conf\n[a]\none = 1\n")
        self.tree.write("two.conf", "[a]\ntwo = 2\n")
        config = load(self.tree.write("top.conf", "include = one.conf\n"))
        self.assertEqual((config.get("a", "one"), config.get("a", "two")), ("1", "2"))

    def test_missing(self):
        top = self.tree.write("top.conf", "[a]\nx = 1\ninclude = gone.conf\n")
        with self.assertRaises(LoadError) as caught:
            load(top)
        self.assertEqual(caught.exception.line, 3)
        self.assertIn("include not found", caught.exception.message)

    def test_cycle(self):
        self.tree.write("a.conf", "include = b.conf\n")
        self.tree.write("b.conf", "include = a.conf\n")
        with self.assertRaises(LoadError) as caught:
            load(os.path.join(self.tree.root, "a.conf"))
        self.assertEqual(caught.exception.message, f"include cycle through {os.path.join(self.tree.root, 'a.conf')}")

    def test_same_file_twice_is_not_a_cycle(self):
        self.tree.write("common.conf", "[a]\nn = 1\n")
        config = load(self.tree.write("top.conf", "include = common.conf\ninclude = common.conf\n"))
        self.assertEqual(config.get("a", "n"), "1")


if __name__ == "__main__":
    unittest.main()
