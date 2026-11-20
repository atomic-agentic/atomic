import json
import os
import subprocess
import sys
import tempfile
import unittest

EXPECTED = [
    "us WIDGET-S x3: net 28.65 USD, tax 2.08 USD, total 30.73 USD",
    "eu WIDGET-S x3: net 26.36 EUR, tax 5.54 EUR, total 31.90 EUR",
    "jp LAMP x2: net 5,678 JPY, tax 568 JPY, total 6,246 JPY",
    "eu HUB-4 x10: net 362.11 EUR, tax 76.04 EUR, total 438.15 EUR (bulk -4%)",
    "us HUB-4 x12: net 467.40 USD, tax 33.89 USD, total 501.29 USD (bulk -5%)",
    "jp BRACKET x25: net 14,347 JPY, tax 1,435 JPY, total 15,782 JPY (bulk -5%)",
    "eu CABLE-2M x1: net 5.75 EUR, tax 1.21 EUR, total 6.96 EUR",
    "us LAMP x1: net 18.75 USD, tax 1.36 USD, total 20.11 USD",
    "jp WIDGET-L x19: net 69,326 JPY, tax 6,933 JPY, total 76,259 JPY",
]


def run(*args, env_extra=None):
    env = dict(os.environ)
    env.pop("PRICER_CONFIG_DIR", None)
    for key in list(env):
        if key.startswith("PRICER_"):
            del env[key]
    env.update(env_extra or {})
    return subprocess.run([sys.executable, "-B", *args], capture_output=True, text=True, timeout=30, env=env)


class Hidden(unittest.TestCase):
    def test_mixed_batch(self):
        result = run("-m", "pricer", "batch", "hidden_jobs.txt")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), EXPECTED)

    def test_batch_matches_single_quotes(self):
        batch = run("-m", "pricer", "batch", "data/jobs-example.txt").stdout.splitlines()
        singles = []
        with open("data/jobs-example.txt") as f:
            for line in f:
                if line.strip() and not line.startswith("#"):
                    profile, sku, quantity = line.split()
                    singles.append(run("-m", "pricer", "quote", "--profile", profile, sku, quantity).stdout.strip())
        self.assertEqual(batch, singles)

    def test_profiles(self):
        result = run("-m", "pricer", "profiles")
        self.assertEqual(result.stdout.splitlines(),
                         ["eu\tEurozone\tEUR", "jp\tJapan\tJPY", "us\tUnited States\tUSD"])

    def test_environment_override_per_profile(self):
        result = run("-m", "pricer", "batch", "hidden_jobs.txt", env_extra={"PRICER_EU_TAX_RATE": "19"})
        lines = result.stdout.splitlines()
        self.assertEqual(lines[0], EXPECTED[0])
        self.assertEqual(lines[1], "eu WIDGET-S x3: net 26.36 EUR, tax 5.01 EUR, total 31.37 EUR")
        self.assertEqual(lines[2], EXPECTED[2])
        self.assertEqual(lines[4], EXPECTED[4])

    def test_each_profile_loaded_once(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "counts.json")
            result = run("hidden_count.py", "hidden_jobs.txt", out)
            self.assertEqual(result.returncode, 0, result.stderr)
            with open(out) as f:
                data = json.load(f)
        self.assertEqual(data["stdout"].splitlines(), EXPECTED)
        counts = data["counts"]
        for profile in ("us.ini", "eu.ini", "jp.ini"):
            self.assertEqual(counts.get(profile), 1, f"{profile} opened {counts.get(profile, 0)} times: {counts}")
        self.assertLessEqual(counts.get("defaults.ini", 0), 3, counts)
        self.assertGreaterEqual(counts.get("defaults.ini", 0), 1, counts)


if __name__ == "__main__":
    unittest.main()
