import ast
import os
import random
import shutil
import subprocess
import sys
import tempfile
import unittest
from decimal import Decimal

import hidden_original as original
from duscan import humanize
from duscan.report import summary
from duscan.scanner import entries_from_spec
from duscan.tree import render_tree

ROOT = os.path.dirname(os.path.abspath(__file__))
FUNCTIONS = ("format_bytes", "format_si", "format_rate")


def values():
    out = [0, 0.0, -0.0, 1, -1, 0.4, 0.5, 0.05, 0.004, 1e-9, -1e-9, 512, 999, 1000, 1001, 1023, 1024, 1025,
           999.4, 999.5, 999.94, 999.95, 999.96, 999.99, 1000.0, 1000.4, 1000.5, 1023.4, 1023.94, 1023.95, 1023.96,
           1048570, 1048575, 1048576, 999_999, 1_000_000, 999_950, 2 ** 70, 10 ** 21, 10 ** 30, 1.5e18,
           float("nan"), float("inf"), float("-inf")]
    for base in (1000, 1024):
        for power in range(0, 8):
            edge = base ** power
            out += [edge - 1, edge, edge + 1]
            for factor in (0.5, 0.999, 0.9995, 0.99995, 0.999995, 1.0, 1.00001, 1.0005, 1.5):
                out.append(edge * factor)
    rng = random.Random(20261108)
    for _ in range(1500):
        out.append(rng.randrange(0, 10 ** rng.randrange(1, 20)))
        out.append(rng.lognormvariate(8, 6))
    return out + [-v for v in out]


def outcome(function, *args):
    try:
        return ("ok", function(*args))
    except Exception as error:
        return (type(error).__name__, str(error))


class SameOutput(unittest.TestCase):
    def test_every_value_and_precision(self):
        for name in FUNCTIONS:
            old, new = getattr(original, name), getattr(humanize, name)
            for value in values():
                self.assertEqual(outcome(new, value), outcome(old, value), f"{name}({value!r})")
                for precision in (0, 1, 2, 3):
                    self.assertEqual(outcome(new, value, precision), outcome(old, value, precision),
                                     f"{name}({value!r}, {precision})")

    def test_keyword_precision(self):
        for name in FUNCTIONS:
            for value in (1536, 2_500_000, 0.25):
                self.assertEqual(getattr(humanize, name)(value, precision=3), getattr(original, name)(value, precision=3))

    def test_errors(self):
        for name in FUNCTIONS:
            old, new = getattr(original, name), getattr(humanize, name)
            for bad in (None, "12", b"1", True, False, [1], complex(1, 1), Decimal("1.5")):
                expected = outcome(old, bad)
                self.assertNotEqual(expected[0], "ok")
                self.assertEqual(outcome(new, bad), expected, f"{name}({bad!r})")

    def test_share_unchanged(self):
        for part, whole in ((1, 8), (0, 0), (3, 7), (5, 5)):
            self.assertEqual(humanize.format_share(part, whole), original.format_share(part, whole))


class Structure(unittest.TestCase):
    def setUp(self):
        with open(humanize.__file__, encoding="utf-8") as f:
            self.tree = ast.parse(f.read())
        self.functions = {n.name: n for n in self.tree.body if isinstance(n, ast.FunctionDef)}

    def test_helper(self):
        self.assertIn("_scale", self.functions)
        names = [a.arg for a in self.functions["_scale"].args.args][:4]
        self.assertEqual(names, ["value", "base", "units", "precision"])

    def test_each_function_calls_it_and_has_no_loop(self):
        for name in FUNCTIONS:
            self.assertIn(name, self.functions)
            body = self.functions[name]
            calls = [n.func.id for n in ast.walk(body) if isinstance(n, ast.Call) and isinstance(n.func, ast.Name)]
            self.assertIn("_scale", calls, name)
            loops = [n for n in ast.walk(body) if isinstance(n, (ast.For, ast.While, ast.comprehension))]
            self.assertEqual(loops, [], f"{name} still has a loop of its own")


SPEC = {"docs/guide.md": 10239, "docs/img/a.png": 1048570, "docs/img/b.png": 1023, "src/main.c": 999999,
        "src/lib/x.c": 1000, "src/lib/y.h": 1024, "data/big.bin": 5 * 1024 ** 4 - 3, "data/tiny": 1,
        "data/empty.txt": 0, "LICENSE": 1023, "notes.txt": 999, "a.tar.gz": 1536, "b.json": 300, "c.yaml": 77,
        "d.toml": 12, "e.ini": 9, "f.cfg": 5, "g.lock": 2048}


class Reports(unittest.TestCase):
    """The rest of the tool prints what it printed before."""

    def test_summary(self):
        e = entries_from_spec(SPEC)
        self.assertEqual(summary(e, top=4, seconds=7.0), 'files: 18  directories: 5  total: 5.0 TiB\n\nlargest 4:\n     5.0 TiB  data/big.bin\n  1024.0 KiB  docs/img/a.png\n   976.6 KiB  src/main.c\n    10.0 KiB  docs/guide.md\n\nby extension:\n  .bin          1     5.0 TiB  100.0%\n  .png          2     1.0 MiB    0.0%\n  .c            2   977.5 KiB    0.0%\n  .md           1    10.0 KiB    0.0%\n  .lock         1     2.0 KiB    0.0%\n  .gz           1     1.5 KiB    0.0%\n  (none)        2     1.0 KiB    0.0%\n  .h            1     1.0 KiB    0.0%\n  (other)       7     1.4 KiB    0.0%\n\nscanned at 785.37 GB/s\n')
        self.assertEqual(summary(e, top=3, seconds=1048.57, extensions=3), 'files: 18  directories: 5  total: 5.0 TiB\n\nlargest 3:\n     5.0 TiB  data/big.bin\n  1024.0 KiB  docs/img/a.png\n   976.6 KiB  src/main.c\n\nby extension:\n  .bin          1     5.0 TiB  100.0%\n  .png          2     1.0 MiB    0.0%\n  .c            2   977.5 KiB    0.0%\n  (other)      13    16.9 KiB    0.0%\n\nscanned at 5.24 GB/s\n')

    def test_tree(self):
        e = entries_from_spec(SPEC)
        self.assertEqual(render_tree(e), '    5.0 TiB  .\n     1023 B  LICENSE\n    1.5 KiB  a.tar.gz\n      300 B  b.json\n       77 B  c.yaml\n       12 B  d.toml\n    5.0 TiB  data/\n    5.0 TiB      big.bin\n        0 B      empty.txt\n        1 B      tiny\n    1.0 MiB  docs/\n   10.0 KiB      guide.md\n    1.0 MiB      img/\n 1024.0 KiB          a.png\n     1023 B          b.png\n        9 B  e.ini\n        5 B  f.cfg\n    2.0 KiB  g.lock\n      999 B  notes.txt\n  978.5 KiB  src/\n    2.0 KiB      lib/\n     1000 B          x.c\n    1.0 KiB          y.h\n  976.6 KiB      main.c\n')
        self.assertEqual(render_tree(e, max_depth=2, precision=0), '      5 TiB  .\n     1023 B  LICENSE\n      2 KiB  a.tar.gz\n      300 B  b.json\n       77 B  c.yaml\n       12 B  d.toml\n      5 TiB  data/\n      5 TiB      big.bin\n        0 B      empty.txt\n        1 B      tiny\n      1 MiB  docs/\n     10 KiB      guide.md\n      1 MiB      img/\n        9 B  e.ini\n        5 B  f.cfg\n      2 KiB  g.lock\n      999 B  notes.txt\n    979 KiB  src/\n      2 KiB      lib/\n    977 KiB      main.c\n')
        self.assertEqual(render_tree(e, precision=3, files=False), '  5.000 TiB  .\n  5.000 TiB  data/\n  1.011 MiB  docs/\n  1.001 MiB      img/\n978.538 KiB  src/\n  1.977 KiB      lib/\n')

    def test_cli(self):
        root = tempfile.mkdtemp()
        try:
            spec = {"app/main.py": 4097, "app/util/io.py": 1023, "app/util/fmt.py": 2048, "assets/hero.jpg": 1048576,
                    "assets/icon.svg": 999, "README": 1000, ".hidden/x": 5}
            for path, size in spec.items():
                os.makedirs(os.path.join(root, os.path.dirname(path)), exist_ok=True)
                with open(os.path.join(root, path), "wb") as f:
                    f.write(b"z" * size)
            runs = {
                ("--no-hidden", "--top", "3"): 'files: 6  directories: 3  total: 1.0 MiB\n\nlargest 3:\n     1.0 MiB  assets/hero.jpg\n     4.0 KiB  app/main.py\n     2.0 KiB  app/util/fmt.py\n\nby extension:\n  .jpg          1     1.0 MiB   99.1%\n  .py           3     7.0 KiB    0.7%\n  (none)        1      1000 B    0.1%\n  .svg          1       999 B    0.1%\n\nbiggest directories:\n     1.0 MiB  assets/\n     7.0 KiB  app/\n     3.0 KiB  app/util/\n',
                ("--tree", "--precision", "2"): '   1.01 MiB  .\n        5 B  .hidden/\n        5 B      x\n     1000 B  README\n   7.00 KiB  app/\n   4.00 KiB      main.py\n   3.00 KiB      util/\n   2.00 KiB          fmt.py\n     1023 B          io.py\n   1.00 MiB  assets/\n   1.00 MiB      hero.jpg\n      999 B      icon.svg\n',
            }
            for args, expected in runs.items():
                done = subprocess.run([sys.executable, "-B", "-m", "duscan", root, *args], cwd=ROOT,
                                      capture_output=True, text=True, timeout=20)
                self.assertEqual((done.returncode, done.stdout), (0, expected), done.stderr)
        finally:
            shutil.rmtree(root)


if __name__ == "__main__":
    unittest.main()
