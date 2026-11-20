import unittest

from verrange.ranges import Range, RangeError, satisfies


class ComparatorTest(unittest.TestCase):
    def test_exact(self):
        self.assertTrue(satisfies("1.2.3", "1.2.3"))
        self.assertTrue(satisfies("1.2.3", "=1.2.3"))
        self.assertFalse(satisfies("1.2.4", "1.2.3"))

    def test_operators(self):
        self.assertTrue(satisfies("1.2.3", ">1.2.2"))
        self.assertFalse(satisfies("1.2.2", ">1.2.2"))
        self.assertTrue(satisfies("1.2.2", ">=1.2.2"))
        self.assertTrue(satisfies("1.2.1", "<1.2.2"))
        self.assertTrue(satisfies("1.2.2", "<= 1.2.2"))

    def test_and_or(self):
        self.assertTrue(satisfies("1.5.0", ">=1.2.0 <2.0.0"))
        self.assertFalse(satisfies("2.0.0", ">=1.2.0 <2.0.0"))
        self.assertTrue(satisfies("3.1.0", "<1.0.0 || >=3.0.0"))
        self.assertFalse(satisfies("2.0.0", "<1.0.0 || >=3.0.0"))

    def test_any(self):
        for text in ("*", "x", "", "  "):
            self.assertTrue(satisfies("4.5.6", text), text)

    def test_errors(self):
        for text in (">=1.2", "1.x.3", "~>1.2.3", "^1.2-rc", "banana"):
            with self.assertRaises(RangeError, msg=text):
                Range(text)


class XRangeTest(unittest.TestCase):
    def test_major(self):
        for text in ("1.x", "1.*", "1", "1.X"):
            self.assertTrue(satisfies("1.0.0", text), text)
            self.assertTrue(satisfies("1.99.4", text), text)
            self.assertFalse(satisfies("2.0.0", text), text)
            self.assertFalse(satisfies("0.9.9", text), text)

    def test_minor(self):
        for text in ("1.2.x", "1.2"):
            self.assertTrue(satisfies("1.2.0", text), text)
            self.assertTrue(satisfies("1.2.77", text), text)
            self.assertFalse(satisfies("1.3.0", text), text)


class TildeTest(unittest.TestCase):
    def test_full(self):
        self.assertTrue(satisfies("1.2.3", "~1.2.3"))
        self.assertTrue(satisfies("1.2.9", "~1.2.3"))
        self.assertFalse(satisfies("1.3.0", "~1.2.3"))
        self.assertFalse(satisfies("1.2.2", "~1.2.3"))

    def test_partial(self):
        self.assertTrue(satisfies("1.2.0", "~1.2"))
        self.assertFalse(satisfies("1.3.0", "~1.2"))
        self.assertTrue(satisfies("1.9.0", "~1"))
        self.assertFalse(satisfies("2.0.0", "~1"))


class PrereleaseTest(unittest.TestCase):
    def test_excluded_by_default(self):
        self.assertFalse(satisfies("1.3.0-rc.1", "^1.2.0"))
        self.assertFalse(satisfies("2.0.0-rc.1", "<2.0.0"))

    def test_same_core_allowed(self):
        self.assertTrue(satisfies("1.2.4-beta.1", ">=1.2.4-alpha <1.3.0"))
        self.assertFalse(satisfies("1.2.5-beta.1", ">=1.2.4-alpha <1.3.0"))

    def test_only_within_one_set(self):
        self.assertFalse(satisfies("1.2.4-beta.1", ">=1.2.0 || <1.3.0"))
        self.assertTrue(satisfies("1.2.4-beta.1", ">=1.2.0 || >=1.2.4-alpha <1.3.0"))
