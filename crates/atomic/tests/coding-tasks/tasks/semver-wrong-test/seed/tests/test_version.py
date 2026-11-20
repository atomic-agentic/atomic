import unittest

from verrange.version import Version, VersionError


class ParseTest(unittest.TestCase):
    def test_parts(self):
        version = Version.parse("1.4.0-rc.2+build.77")
        self.assertEqual(version.core, (1, 4, 0))
        self.assertEqual(version.prerelease, ("rc", "2"))
        self.assertEqual(version.build, ("build", "77"))
        self.assertEqual(str(version), "1.4.0-rc.2+build.77")

    def test_leading_v(self):
        self.assertEqual(Version.parse("v2.0.1"), Version(2, 0, 1))

    def test_invalid(self):
        for text in ("1.2", "1.2.3.4", "01.2.3", "1.2.3-", "1.2.3-rc..1", "a.b.c", ""):
            with self.assertRaises(VersionError, msg=text):
                Version.parse(text)


class PrecedenceTest(unittest.TestCase):
    def test_core(self):
        self.assertLess(Version.parse("1.2.3"), Version.parse("1.10.0"))
        self.assertLess(Version.parse("1.9.9"), Version.parse("2.0.0"))
        self.assertLess(Version.parse("0.0.9"), Version.parse("0.0.10"))

    def test_prerelease_below_release(self):
        self.assertLess(Version.parse("1.0.0-rc.1"), Version.parse("1.0.0"))
        self.assertLess(Version.parse("1.0.0"), Version.parse("1.0.1-alpha"))

    def test_build_ignored(self):
        self.assertEqual(Version.parse("1.0.0+a"), Version.parse("1.0.0+b"))
        self.assertEqual(len({Version.parse("1.0.0+a"), Version.parse("1.0.0")}), 1)

    def test_prerelease_chain(self):
        chain = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
        ]
        versions = [Version.parse(text) for text in chain]
        for lower, higher in zip(versions, versions[1:]):
            self.assertLess(lower, higher)
        self.assertEqual(sorted(reversed(versions)), versions)

    def test_numeric_identifiers(self):
        self.assertLess(Version.parse("2.0.0-rc.9"), Version.parse("2.0.0-rc.10"))
