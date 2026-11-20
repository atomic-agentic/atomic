import ast
import unittest

from verrange.ranges import Range, satisfies
from verrange.resolve import max_satisfying
from verrange.version import Version

WRONG = "test_caret_zero_major"


def v(text):
    return Version.parse(text)


class Readme(unittest.TestCase):
    def test_caret_zero(self):
        self.assertTrue(satisfies("0.2.3", "^0.2.3"))
        self.assertTrue(satisfies("0.2.99", "^0.2.3"))
        self.assertFalse(satisfies("0.3.0", "^0.2.3"))
        self.assertFalse(satisfies("0.3.1", "^0.2.3"))
        self.assertFalse(satisfies("1.0.0", "^0.2.3"))
        self.assertFalse(satisfies("0.0.4", "^0.0.3"))
        self.assertFalse(satisfies("0.1.0", "^0.0"))
        self.assertTrue(satisfies("0.9.9", "^0"))
        self.assertEqual(max_satisfying(["0.2.4", "0.2.10", "0.3.0", "0.10.0"], "^0.2.3"), "0.2.10")

    def test_tilde(self):
        self.assertEqual(str(Range("~1.2.3")), ">=1.2.3 <1.3.0")
        self.assertEqual(str(Range("~0.0.5")), ">=0.0.5 <0.1.0")
        self.assertEqual(str(Range("~1.2.3-beta.1")), ">=1.2.3-beta.1 <1.3.0")
        self.assertTrue(satisfies("1.2.10", "~1.2.3"))
        self.assertFalse(satisfies("1.3.0", "~1.2.3"))
        self.assertEqual(str(Range("~1.2")), ">=1.2.0 <1.3.0")
        self.assertEqual(str(Range("~1")), ">=1.0.0 <2.0.0")

    def test_precedence(self):
        chain = ["0.9.0", "1.0.0-0", "1.0.0-2", "1.0.0-10", "1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-alpha.2",
                 "1.0.0-alpha.10", "1.0.0-alpha.beta", "1.0.0-alpha.beta.1", "1.0.0-alpha-1", "1.0.0-beta",
                 "1.0.0-beta.2", "1.0.0-beta.11", "1.0.0-rc.1", "1.0.0-rc.1a", "1.0.0", "1.0.1-1", "1.0.1"]
        versions = [v(text) for text in chain]
        for lower, higher in zip(versions, versions[1:]):
            self.assertLess(lower, higher, f"{lower} < {higher}")
            self.assertGreater(higher, lower, f"{higher} > {lower}")
        self.assertEqual([str(x) for x in sorted(reversed(versions))], chain)
        self.assertEqual(v("1.0.0-rc.1+b1"), v("1.0.0-rc.1+b2"))
        self.assertNotEqual(v("1.0.0-1"), v("1.0.0-01a"))

    def test_mixed_identifiers_do_not_crash(self):
        self.assertLess(v("1.0.0-alpha.1"), v("1.0.0-alpha.beta"))
        self.assertLess(v("1.0.0-1.x"), v("1.0.0-a.1"))
        self.assertEqual(max_satisfying(["1.0.0-rc.9", "1.0.0-rc.10", "1.0.0-rc.x"], ">=1.0.0-rc.1"), "1.0.0-rc.x")

    def test_prerelease_rule(self):
        self.assertFalse(satisfies("1.3.0-rc.1", "^1.2.0"))
        self.assertTrue(satisfies("1.2.3-rc.10", ">1.2.3-rc.9 <1.2.3"))
        self.assertFalse(satisfies("1.2.3-rc.8", ">1.2.3-rc.9 <1.2.3"))


class TheWrongTest(unittest.TestCase):
    def methods(self, path):
        tree = ast.parse(open(path, encoding="utf-8").read())
        out = {}
        other = []
        for node in tree.body:
            if isinstance(node, ast.ClassDef):
                for item in node.body:
                    out[(node.name, getattr(item, "name", None))] = item
            else:
                other.append(ast.dump(node))
        return out, other

    def test_only_that_test_changed(self):
        seed, seed_other = self.methods("hidden_seed_test_caret.py")
        mine, mine_other = self.methods("tests/test_caret.py")
        self.assertEqual(mine_other, seed_other, "tests/test_caret.py changed outside its tests")
        self.assertEqual(sorted(mine, key=str), sorted(seed, key=str), "tests were added, removed or renamed")
        for key, node in seed.items():
            if key == ("CaretTest", WRONG):
                continue
            self.assertEqual(ast.dump(mine[key]), ast.dump(node), f"{key[1]} was changed")

    def test_it_now_checks_the_readme(self):
        # The corrected test must pass with the real code and fail with code that lets ^0.2.3 reach 0.3.x.
        import tests.test_caret as module

        real = module.satisfies

        def zero_major_like_one(version, range_text):
            text = range_text.strip()
            if text.startswith("^0.") and " " not in text:
                return real(version, ">=" + text[1:] + " <1.0.0")
            return real(version, range_text)

        def outcome():
            result = unittest.TestResult()
            module.CaretTest(WRONG).run(result)
            return result.wasSuccessful()

        self.assertTrue(outcome(), "the corrected test fails")
        module.satisfies = zero_major_like_one
        try:
            self.assertFalse(outcome(), "the corrected test still passes when ^0.2.3 matches 0.3.1")
        finally:
            module.satisfies = real
