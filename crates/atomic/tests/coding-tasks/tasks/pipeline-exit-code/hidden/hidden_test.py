import os
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = os.path.dirname(os.path.abspath(__file__))
STAGES = ("extract", "validate", "transform", "load")


def data(name):
    for base in ("data", "hidden_data"):
        path = os.path.join(ROOT, base, name)
        if os.path.isdir(path):
            return path
    raise AssertionError(f"no data set {name}")


class Pipeline(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="atomic-task-out.")
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.out = os.path.join(self.tmp, "out")

    def run_pipeline(self, name):
        return subprocess.run(["sh", os.path.join(ROOT, "run.sh"), data(name), self.out], cwd=self.tmp,
                              capture_output=True, text=True, timeout=60)

    def stage(self, stage, name):
        return subprocess.run([sys.executable, "-B", os.path.join(ROOT, f"{stage}.py"), data(name), self.out],
                              cwd=self.tmp, capture_output=True, text=True, timeout=30)

    def logs(self):
        return sorted(f[:-4] for f in os.listdir(self.out) if f.endswith(".log")) if os.path.isdir(self.out) else []

    def log(self, stage):
        with open(os.path.join(self.out, f"{stage}.log"), encoding="utf-8") as f:
            return f.read()

    def assert_failed_at(self, result, stage):
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn(f"pipeline failed: {stage}", result.stderr)
        self.assertNotIn("pipeline ok", result.stdout)
        ran = list(STAGES[:STAGES.index(stage) + 1])
        self.assertEqual(self.logs(), sorted(ran), "later stages must not run")
        self.assertIn("ERROR", self.log(stage))

    def test_good_data(self):
        result = self.run_pipeline("good")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("pipeline ok", result.stdout)
        self.assertNotIn("pipeline failed", result.stderr)
        self.assertEqual(self.logs(), sorted(STAGES))
        for stage in STAGES:
            self.assertNotIn("ERROR", self.log(stage))
        with open(os.path.join(self.out, "report.txt"), encoding="utf-8") as f:
            self.assertIn("Grand total: 1001.68", f.read())

    def test_bad_data_stops_at_validate(self):
        result = self.run_pipeline("bad")
        self.assert_failed_at(result, "validate")
        self.assertIn("order 2002: unknown customer C9", self.log("validate"))
        self.assertIn("order 2001: duplicate id", self.log("validate"))
        self.assertFalse(os.path.exists(os.path.join(self.out, "totals.csv")))

    def test_an_unknown_rate_stops_at_transform(self):
        self.assert_failed_at(self.run_pipeline("xfail"), "transform")
        self.assertIn("no rate for NOK", self.log("transform"))

    def test_a_ragged_row_stops_at_extract(self):
        self.assert_failed_at(self.run_pipeline("ragged"), "extract")

    def test_a_missing_file_stops_at_extract(self):
        self.assert_failed_at(self.run_pipeline("missing"), "extract")

    def test_old_logs_are_removed(self):
        self.assertEqual(self.run_pipeline("good").returncode, 0)
        self.assert_failed_at(self.run_pipeline("bad"), "validate")

    def test_stages_alone(self):
        self.assertEqual(self.stage("extract", "bad").returncode, 0)
        result = self.stage("validate", "bad")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("ERROR pipeline.validate: order 2003: bad date", result.stderr)
        self.assertEqual(self.stage("extract", "ragged").returncode, 1)
        self.assertEqual(self.stage("extract", "xfail").returncode, 0)
        self.assertEqual(self.stage("validate", "xfail").returncode, 0)
        self.assertEqual(self.stage("transform", "xfail").returncode, 1)
        self.assertEqual(self.stage("load", "xfail").returncode, 0)
        shutil.rmtree(self.out)
        self.assertEqual(self.stage("load", "good").returncode, 1)
        self.assertEqual(self.stage("transform", "good").returncode, 1, "a crash is an error")
        for stage in STAGES:
            self.assertEqual(self.stage(stage, "good").returncode, 0, stage)

    def test_usage(self):
        result = subprocess.run(["sh", os.path.join(ROOT, "run.sh")], cwd=self.tmp, capture_output=True,
                                text=True, timeout=30)
        self.assertEqual(result.returncode, 2)


if __name__ == "__main__":
    unittest.main()
