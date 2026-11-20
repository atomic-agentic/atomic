import os
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest

from deployctl.loader import LoadError, load

ROOT = os.path.dirname(os.path.abspath(__file__))


def write(root, relative, text):
    path = os.path.join(root, relative)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        f.write(textwrap.dedent(text))
    return path


class Tree(unittest.TestCase):
    """A synthetic deploy tree, and decoys of the same names in every directory a run starts from."""

    def setUp(self):
        self.base = os.path.realpath(tempfile.mkdtemp())
        self.tree = os.path.join(self.base, "repo")
        self.elsewhere = os.path.join(self.base, "elsewhere")
        os.makedirs(self.elsewhere)
        self.old_cwd = os.getcwd()
        t = self.tree
        self.top = write(t, "conf/top.conf", """\
            include = common.conf
            [service]
            name = alpha-web
            include = services/web.conf
            image = r.example.internal/alpha/web:3.1.0
            [deploy]
            env = staging
            replicas = 2
            """)
        write(t, "conf/common.conf", "[service]\nteam = alpha\n")
        write(t, "conf/services/web.conf", """\
            include = limits.conf
            [http]
            port = 8080
            include = ../shared/net.conf
            """)
        write(t, "conf/services/limits.conf", "[limits]\ncpu = 2\n")
        write(t, "conf/shared/net.conf", "[net]\nmtu = 1400\ninclude = deeper/dns.conf\n")
        write(t, "conf/shared/deeper/dns.conf", "[net]\ndns = 10.0.0.53\n")
        for directory in (t, os.path.join(t, "conf"), os.path.join(t, "conf/services"), self.elsewhere):
            for name in ("common.conf", "limits.conf", "services/web.conf", "deeper/dns.conf", "../shared/net.conf"):
                path = os.path.normpath(os.path.join(directory, name))
                if not os.path.exists(path) and path.startswith(self.base):
                    write(self.base, os.path.relpath(path, self.base), "[decoy]\nwrong = yes\n")
        self.starts = [self.elsewhere, t, os.path.join(t, "conf"), os.path.join(t, "conf/services"),
                       os.path.join(t, "conf/shared")]

    def tearDown(self):
        os.chdir(self.old_cwd)
        shutil.rmtree(self.base)

    def load_from(self, cwd, path):
        os.chdir(cwd)
        try:
            return load(path)
        finally:
            os.chdir(self.old_cwd)

    def expected_files(self):
        t = self.tree
        return [os.path.join(t, p) for p in ("conf/top.conf", "conf/common.conf", "conf/services/web.conf",
                                             "conf/services/limits.conf", "conf/shared/net.conf",
                                             "conf/shared/deeper/dns.conf")]

    def test_nested_includes_from_every_directory(self):
        for cwd in self.starts:
            for path in (self.top, os.path.relpath(self.top, cwd)):
                config = self.load_from(cwd, path)
                where = f"from {cwd} with {path}"
                self.assertEqual(config.files, self.expected_files(), where)
                self.assertNotIn("decoy", config.sections, where)
                self.assertEqual(config.get("service", "team"), "alpha", where)
                self.assertEqual(config.get("limits", "cpu"), "2", where)
                self.assertEqual(config.get("net", "dns"), "10.0.0.53", where)
                self.assertEqual(config.get("service", "image"), "r.example.internal/alpha/web:3.1.0", where)

    def test_absolute_include(self):
        absolute = write(self.base, "abs/extra.conf", "[extra]\nk = v\n")
        top = write(self.tree, "conf/abs.conf", f"include = {absolute}\n")
        config = self.load_from(self.elsewhere, top)
        self.assertEqual(config.get("extra", "k"), "v")

    def test_missing_include_names_the_path_it_looked_for(self):
        write(self.tree, "conf/services/web.conf", "[http]\nport = 1\ninclude = nothere.conf\n")
        for cwd in self.starts:
            with self.assertRaises(LoadError) as caught:
                self.load_from(cwd, self.top)
            error = caught.exception
            self.assertEqual(error.message, f"include not found: {os.path.join(self.tree, 'conf/services/nothere.conf')}")
            self.assertEqual((error.path, error.line), (os.path.join(self.tree, "conf/services/web.conf"), 3))

    def test_cycles_still_found(self):
        a = write(self.tree, "conf/a.conf", "[x]\nk = 1\ninclude = sub/b.conf\n")
        write(self.tree, "conf/sub/b.conf", "include = ../c.conf\n")
        write(self.tree, "conf/c.conf", "include = ./sub/../a.conf\n")
        for cwd in (self.elsewhere, os.path.join(self.tree, "conf")):
            with self.assertRaises(LoadError) as caught:
                self.load_from(cwd, a)
            self.assertEqual(caught.exception.message, f"include cycle through {a}")
            self.assertEqual(caught.exception.path, os.path.join(self.tree, "conf/c.conf"))
        selfish = write(self.tree, "conf/self.conf", "include = self.conf\n")
        with self.assertRaises(LoadError) as caught:
            self.load_from(self.elsewhere, selfish)
        self.assertEqual(caught.exception.message, f"include cycle through {selfish}")

    def cli(self, cwd, *argv):
        env = dict(os.environ, PYTHONPATH=ROOT)
        done = subprocess.run([sys.executable, "-B", "-m", "deployctl", *argv], cwd=cwd, env=env,
                              capture_output=True, text=True, timeout=20)
        return done.returncode, done.stdout

    def test_cli_from_elsewhere(self):
        for cwd in (self.elsewhere, self.tree):
            self.assertEqual(self.cli(cwd, "check", self.top), (0, "ok: 9 settings from 6 files\n"))
        write(self.tree, "conf/services/web.conf", "[http]\nport = 1\ninclude = nothere.conf\n")
        code, out = self.cli(self.elsewhere, "check", self.top)
        self.assertEqual(code, 1)
        self.assertEqual(out, "services/web.conf:3: error: include not found: "
                              f"{os.path.join(self.tree, 'conf/services/nothere.conf')}\n")

    def test_project_configs_from_elsewhere(self):
        for name in ("prod.conf", "staging.conf"):
            for cwd in (self.elsewhere, ROOT):
                code, out = self.cli(cwd, "check", os.path.join(ROOT, "conf", name))
                self.assertEqual(code, 0, f"{name} from {cwd}: {out}")
                self.assertTrue(out.startswith("ok: "), out)
        code, out = self.cli(ROOT, "show", "conf/staging.conf")
        self.assertEqual(code, 0, out)
        self.assertIn("[http]\nadmin_port = 9090\nport = 8081\nupstream = http://storefront-web-staging:8081\n", out)


if __name__ == "__main__":
    unittest.main()
