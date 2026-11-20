import contextlib
import io
import unittest

from deployctl.cli import main
from tests.files import CLEAN, TempTree


def run(*argv):
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        code = main(list(argv))
    return code, out.getvalue()


class Check(unittest.TestCase):
    def test_clean(self):
        with TempTree() as tree:
            self.assertEqual(run("check", tree.write("d.conf", CLEAN)), (0, "ok: 5 settings from 1 files\n"))

    def test_quiet(self):
        with TempTree() as tree:
            self.assertEqual(run("check", "-q", tree.write("d.conf", CLEAN)), (0, ""))

    def test_problems(self):
        with TempTree() as tree:
            path = tree.write("d.conf", CLEAN.replace("port = 8000", "port = 0").replace("env = staging", "env = qa"))
            code, out = run("check", path)
        self.assertEqual(code, 1)
        self.assertEqual(out.splitlines(), [
            "d.conf:6: error: env must be one of dev, staging, prod, not 'qa'",
            "d.conf:10: error: port must be a port from 1 to 65535, not '0'",
        ])

    def test_parse_error(self):
        with TempTree() as tree:
            code, out = run("check", tree.write("d.conf", "[service]\noops\n"))
        self.assertEqual((code, out), (1, "d.conf:2: error: not a setting: oops\n"))

    def test_show(self):
        with TempTree() as tree:
            code, out = run("show", tree.write("d.conf", "[b]\ny = ${a.x}!\n[a]\nx = 1\n"))
        self.assertEqual((code, out), (0, "[a]\nx = 1\n\n[b]\ny = 1!\n\n"))


if __name__ == "__main__":
    unittest.main()
