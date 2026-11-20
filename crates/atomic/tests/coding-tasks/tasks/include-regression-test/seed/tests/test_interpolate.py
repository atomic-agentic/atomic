import unittest

from deployctl.interpolate import expand
from deployctl.loader import load
from tests.files import TempTree


class Interpolate(unittest.TestCase):
    def expand_text(self, text):
        with TempTree() as tree:
            return expand(load(tree.write("d.conf", text)))

    def test_same_and_other_section(self):
        values, problems = self.expand_text("[a]\nx = 1\ny = ${x}-${b.z}\n[b]\nz = two\n")
        self.assertEqual(problems, [])
        self.assertEqual(values["a"]["y"], "1-two")

    def test_chains_and_dollar(self):
        values, _ = self.expand_text("[a]\nx = ${y}\ny = ${z}$$\nz = end\n")
        self.assertEqual(values["a"]["x"], "end$")

    def test_undefined(self):
        values, problems = self.expand_text("[a]\nx = ${nope}\n")
        self.assertEqual([m for _, m in problems], ["undefined variable ${nope}"])
        self.assertEqual(values["a"]["x"], "${nope}")

    def test_cycle(self):
        _, problems = self.expand_text("[a]\nx = ${y}\ny = ${x}\n")
        self.assertEqual(len(problems), 2)
        self.assertTrue(all(m.startswith("variable cycle: ") for _, m in problems))


if __name__ == "__main__":
    unittest.main()
