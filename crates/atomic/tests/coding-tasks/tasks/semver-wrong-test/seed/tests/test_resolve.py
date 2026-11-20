import unittest

from verrange.resolve import all_satisfying, max_satisfying, min_satisfying
from verrange.version import VersionError

VERSIONS = ["1.0.0", "1.2.0", "1.2.7", "1.10.1", "2.0.0-rc.1", "2.0.0", "2.3.4", "3.0.0-beta.1"]


class ResolveTest(unittest.TestCase):
    def test_max(self):
        self.assertEqual(max_satisfying(VERSIONS, "^1.0.0"), "1.10.1")
        self.assertEqual(max_satisfying(VERSIONS, ">=2.0.0"), "2.3.4")
        self.assertIsNone(max_satisfying(VERSIONS, ">=4.0.0"))

    def test_min(self):
        self.assertEqual(min_satisfying(VERSIONS, "^1.1.0"), "1.2.0")

    def test_all_in_order(self):
        shuffled = ["2.3.4", "1.2.0", "2.0.0", "1.10.1", "1.2.7"]
        self.assertEqual(all_satisfying(shuffled, ">=1.2.0 <2.1.0"), ["1.2.0", "1.2.7", "1.10.1", "2.0.0"])

    def test_keeps_the_strings_given(self):
        self.assertEqual(all_satisfying(["v1.2.0", "1.2.1+b7"], "1.2.x"), ["v1.2.0", "1.2.1+b7"])

    def test_release_candidates(self):
        candidates = ["2.0.0-rc.2", "2.0.0-rc.10", "2.0.0-rc.9", "1.9.0"]
        self.assertEqual(max_satisfying(candidates, ">=2.0.0-rc.1"), "2.0.0-rc.10")

    def test_bad_version(self):
        with self.assertRaises(VersionError):
            max_satisfying(["1.0.0", "one"], "*")
