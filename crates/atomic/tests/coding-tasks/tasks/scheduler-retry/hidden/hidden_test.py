import json
import os
import subprocess
import sys
import tempfile
import unittest


def job(name, at, duration=1, priority=0, outcomes=(), max_attempts=None):
    data = {"name": name, "at": at, "duration": duration, "priority": priority, "outcomes": list(outcomes)}
    if max_attempts is not None:
        data["max_attempts"] = max_attempts
    return data


class Hidden(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()

    def tearDown(self):
        self.dir.cleanup()

    def simulate(self, path):
        result = subprocess.run([sys.executable, "-B", "-m", "jobsim", "simulate", path], capture_output=True,
                                text=True, timeout=30)
        return result.returncode, result.stdout

    def scenario(self, name, retry, jobs):
        path = os.path.join(self.dir.name, name + ".json")
        with open(path, "w", encoding="utf-8") as f:
            json.dump({"retry": retry, "jobs": jobs}, f)
        return self.simulate(path)

    def test_examples(self):
        self.assertEqual(self.simulate("examples/scenario-retry.json"), (0, (
            "t=0 run sync-ledger attempt 1 -> fail\n"
            "t=4 run build-index attempt 1 -> ok\n"
            "t=9 run sync-ledger attempt 2 -> fail\n"
            "t=13 run mail-digest attempt 1 -> fail\n"
            "t=15 run purge-tmp attempt 1 -> ok\n"
            "t=20 run mail-digest attempt 2 -> ok\n"
            "t=23 run sync-ledger attempt 3 -> ok\n"
            "done at t=27: 4 ok, 0 gave up, 7 runs\n")))
        self.assertEqual(self.simulate("examples/scenario-ties.json"), (0, (
            "t=0 run warm-cache attempt 1 -> ok\n"
            "t=2 run rotate-logs attempt 1 -> fail\n"
            "t=4 run backup-db attempt 1 -> ok\n"
            "t=6 run reindex attempt 1 -> ok\n"
            "t=8 run send-alerts attempt 1 -> ok\n"
            "t=9 run rotate-logs attempt 2 -> ok\n"
            "done at t=11: 5 ok, 0 gave up, 6 runs\n")))

    def test_long_attempts_give_up_and_cap(self):
        got = self.scenario("long", {"base": 10, "factor": 3, "max_delay": 50}, [
            job("etl-orders", 0, duration=25, outcomes=["fail", "fail", "fail"]),
            job("etl-users", 1, duration=2, priority=5),
            job("report", 30, duration=5, outcomes=["fail"]),
        ])
        self.assertEqual(got, (1, (
            "t=0 run etl-orders attempt 1 -> fail\n"
            "t=25 run etl-users attempt 1 -> ok\n"
            "t=30 run report attempt 1 -> fail\n"
            "t=35 run etl-orders attempt 2 -> fail\n"
            "t=60 run report attempt 2 -> ok\n"
            "t=90 run etl-orders attempt 3 -> fail\n"
            "t=115 give up etl-orders after 3 attempts\n"
            "done at t=115: 2 ok, 1 gave up, 6 runs\n")))

    def test_same_moment_same_priority_in_listed_order(self):
        jobs = [job(name, 3, priority=1) for name in ("e5", "d4", "c3", "b2", "a1")] + [job("z0", 3, priority=2)]
        self.assertEqual(self.scenario("ties", {}, jobs), (0, (
            "t=3 run z0 attempt 1 -> ok\n"
            "t=4 run e5 attempt 1 -> ok\n"
            "t=5 run d4 attempt 1 -> ok\n"
            "t=6 run c3 attempt 1 -> ok\n"
            "t=7 run b2 attempt 1 -> ok\n"
            "t=8 run a1 attempt 1 -> ok\n"
            "done at t=9: 6 ok, 0 gave up, 6 runs\n")))

    def test_retry_due_with_a_fresh_job(self):
        got = self.scenario("fresh", {"base": 3, "factor": 1}, [
            job("alpha", 0, duration=2, outcomes=["fail"]),
            job("omega", 5),
            job("beta", 1, duration=2, outcomes=["fail", "fail"]),
            job("aardvark", 13),
        ])
        self.assertEqual(got, (0, (
            "t=0 run alpha attempt 1 -> fail\n"
            "t=2 run beta attempt 1 -> fail\n"
            "t=5 run omega attempt 1 -> ok\n"
            "t=6 run alpha attempt 2 -> ok\n"
            "t=8 run beta attempt 2 -> fail\n"
            "t=13 run aardvark attempt 1 -> ok\n"
            "t=14 run beta attempt 3 -> ok\n"
            "done at t=16: 4 ok, 0 gave up, 7 runs\n")))

    def test_two_retries_due_together(self):
        got = self.scenario("retries", {"base": 2, "factor": 2}, [
            job("yak", 0, outcomes=["fail", "fail"]),
            job("ant", 4, duration=2, outcomes=["fail"]),
        ])
        self.assertEqual(got, (0, (
            "t=0 run yak attempt 1 -> fail\n"
            "t=3 run yak attempt 2 -> fail\n"
            "t=4 run ant attempt 1 -> fail\n"
            "t=8 run yak attempt 3 -> ok\n"
            "t=9 run ant attempt 2 -> ok\n"
            "done at t=11: 2 ok, 0 gave up, 5 runs\n")))

    def test_earliest_due_before_priority(self):
        got = self.scenario("due", {}, [
            job("low-early", 0, duration=5),
            job("low-mid", 2),
            job("high-late", 3, priority=9),
            job("high-same", 2, priority=3),
        ])
        self.assertEqual(got, (0, (
            "t=0 run low-early attempt 1 -> ok\n"
            "t=5 run high-same attempt 1 -> ok\n"
            "t=6 run low-mid attempt 1 -> ok\n"
            "t=7 run high-late attempt 1 -> ok\n"
            "done at t=8: 4 ok, 0 gave up, 4 runs\n")))


if __name__ == "__main__":
    unittest.main()
