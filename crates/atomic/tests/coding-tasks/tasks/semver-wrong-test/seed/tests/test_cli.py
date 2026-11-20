import os
import subprocess
import sys
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def run(*args):
    return subprocess.run([sys.executable, "-B", "-m", "verrange", *args], cwd=ROOT,
                          capture_output=True, text=True, timeout=30)


class CliTest(unittest.TestCase):
    def test_highest(self):
        result = run("^1.2.0 || ~2.0.1", "1.1.9", "1.4.0", "2.0.7", "2.1.0")
        self.assertEqual((result.returncode, result.stdout), (0, "2.0.7\n"))

    def test_all(self):
        result = run("--all", "1.x", "1.4.0", "2.0.0", "1.0.1")
        self.assertEqual((result.returncode, result.stdout), (0, "1.0.1\n1.4.0\n"))

    def test_none(self):
        result = run(">=3.0.0", "1.0.0")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stderr.strip(), "no version satisfies >=3.0.0")

    def test_bad_range(self):
        result = run(">=1.2", "1.2.0")
        self.assertEqual(result.returncode, 2)
