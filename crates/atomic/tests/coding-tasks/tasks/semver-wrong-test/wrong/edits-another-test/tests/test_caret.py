import unittest

from verrange.ranges import Range, satisfies


class CaretTest(unittest.TestCase):
    def test_caret_major(self):
        self.assertTrue(satisfies("1.2.3", "^1.2.3"))
        self.assertTrue(satisfies("1.9.0", "^1.2.3"))
        self.assertFalse(satisfies("2.0.0", "^1.2.3"))
        self.assertFalse(satisfies("1.2.2", "^1.2.3"))

    def test_caret_zero_major(self):
        self.assertTrue(satisfies("0.2.3", "^0.2.3"))
        self.assertTrue(satisfies("0.2.9", "^0.2.3"))
        self.assertFalse(satisfies("0.3.1", "^0.2.3"))

    def test_caret_zero_minor(self):
        self.assertTrue(satisfies("0.0.3", "^0.0.3"))
        self.assertFalse(satisfies("0.0.4", "^0.0.3"))

    def test_caret_partial(self):
        self.assertEqual(str(Range("^1.2")), ">=1.2.0 <2.0.0")
        self.assertEqual(str(Range("^0.2")), ">=0.2.0 <0.3.0")
        self.assertEqual(str(Range("^0.0")), ">=0.0.0 <0.1.0")
        self.assertEqual(str(Range("^0")), ">=0.0.0 <1.0.0")

    def test_caret_prerelease(self):
        self.assertTrue(satisfies("1.2.3-beta.3", "^1.2.3-beta.2"))
        self.assertFalse(satisfies("1.2.4-beta.3", "^1.2.3-beta.2"))
        self.assertTrue(satisfies("1.4.0", "^1.2.3-beta.2"))
