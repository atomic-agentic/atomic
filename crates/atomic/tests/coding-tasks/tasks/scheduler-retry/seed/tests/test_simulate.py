import io
import json
import os
import tempfile
import unittest

from jobsim import cli, scenario
from jobsim.runner import simulate


class ScenarioTest(unittest.TestCase):
    def test_defaults(self):
        loaded = scenario.from_dict({"jobs": [{"name": "a", "at": 0}]})
        job = loaded.jobs[0]
        self.assertEqual((job.priority, job.duration, job.outcomes, job.max_attempts), (0, 1, (), 3))
        self.assertEqual((loaded.retry.base, loaded.retry.factor, loaded.retry.max_delay), (5, 2, 300))

    def test_bad(self):
        cases = [
            ({"jobs": []}, "non-empty list"),
            ({"jobs": [{"name": "a"}]}, "at must be"),
            ({"jobs": [{"name": "a b", "at": 0}]}, "without spaces"),
            ({"jobs": [{"name": "a", "at": 0}, {"name": "a", "at": 1}]}, "unique: a"),
            ({"jobs": [{"name": "a", "at": 0, "outcomes": ["maybe"]}]}, "outcomes"),
            ({"jobs": [{"name": "a", "at": 0, "max_attempts": 0}]}, "max_attempts"),
            ({"retry": {"jitter": 1}, "jobs": [{"name": "a", "at": 0}]}, "unknown jitter"),
        ]
        for data, message in cases:
            with self.assertRaisesRegex(ValueError, message):
                scenario.from_dict(data)


class SimulateTest(unittest.TestCase):
    def test_in_order(self):
        loaded = scenario.from_dict({"jobs": [
            {"name": "first", "at": 0, "duration": 3},
            {"name": "second", "at": 1, "duration": 1},
            {"name": "third", "at": 10, "priority": 4},
        ]})
        self.assertEqual(simulate(loaded), (
            "t=0 run first attempt 1 -> ok\n"
            "t=3 run second attempt 1 -> ok\n"
            "t=10 run third attempt 1 -> ok\n"
            "done at t=11: 3 ok, 0 gave up, 3 runs\n"))

    def test_instant_job_gives_up(self):
        loaded = scenario.from_dict({"retry": {"base": 2, "factor": 3}, "jobs": [
            {"name": "flaky", "at": 1, "duration": 0, "outcomes": ["fail", "fail", "fail"]},
        ]})
        self.assertEqual(simulate(loaded), (
            "t=1 run flaky attempt 1 -> fail\n"
            "t=3 run flaky attempt 2 -> fail\n"
            "t=9 run flaky attempt 3 -> fail\n"
            "t=9 give up flaky after 3 attempts\n"
            "done at t=9: 0 ok, 1 gave up, 3 runs\n"))


class CliTest(unittest.TestCase):
    def run_cli(self, *argv):
        out, err = io.StringIO(), io.StringIO()
        return cli.main(list(argv), out=out, err=err), out.getvalue(), err.getvalue()

    def test_exit_codes(self):
        with tempfile.TemporaryDirectory() as tmp:
            good = os.path.join(tmp, "good.json")
            with open(good, "w", encoding="utf-8") as f:
                json.dump({"jobs": [{"name": "a", "at": 0}]}, f)
            bad = os.path.join(tmp, "bad.json")
            with open(bad, "w", encoding="utf-8") as f:
                json.dump({"jobs": [{"name": "a", "at": 0, "outcomes": ["fail"], "max_attempts": 1}]}, f)
            self.assertEqual(self.run_cli("simulate", good)[0], 0)
            self.assertEqual(self.run_cli("simulate", bad)[0], 1)
            self.assertEqual(self.run_cli("check", good, bad)[:2], (0, f"{good}: ok\n{bad}: ok\n"))
            self.assertEqual(self.run_cli("simulate")[0], 2)
            self.assertEqual(self.run_cli("simulate", os.path.join(tmp, "none.json"))[0], 2)

    def test_backoff(self):
        self.assertEqual(self.run_cli("backoff", "examples/scenario-retry.json")[:2], (0, "5s 10s\n"))


if __name__ == "__main__":
    unittest.main()
