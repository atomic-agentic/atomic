import json
import os
import subprocess
import sys
import tempfile
import unittest
from datetime import date, timedelta

from daylog.clock import local_day, parse_utc
from daylog.offsets import format_offset, parse_offset
from daylog.sites import SiteError, load_sites


def run(*args):
    return subprocess.run([sys.executable, "-B", "-m", "daylog", *args], capture_output=True, text=True, timeout=30)


def minutes(n):
    return timedelta(minutes=n)


class Offsets(unittest.TestCase):
    def test_valid(self):
        cases = {
            "-03:30": -210, "-09:30": -570, "-00:30": -30, "+05:45": 345, "+00:00": 0, "-00:00": 0,
            "-12:00": -720, "+14:00": 840, "+05:30": 330, "-05:00": -300, "-02:15": -135, "+12:45": 765,
        }
        for text, expected in cases.items():
            self.assertEqual(parse_offset(text), minutes(expected), text)

    def test_round_trip(self):
        for text in ("-03:30", "-09:30", "-00:30", "+05:45", "+00:00", "-12:00", "+14:00", "-11:59", "+13:59"):
            self.assertEqual(format_offset(parse_offset(text)), text)

    def test_invalid(self):
        for text in ("+5:30", "05:30", "+0530", "+24:00", "+14:30", "+14:01", "-12:45", "-12:01", "-13:00",
                     "+15:00", "+03:60", "-03:5", "", "UTC", "Z", None, "+05:30:00"):
            with self.assertRaises(ValueError, msg=repr(text)):
                parse_offset(text)


class Days(unittest.TestCase):
    def test_near_local_midnight(self):
        cases = [
            ("2026-03-02T03:10:00Z", "-03:30", date(2026, 3, 1)),
            ("2026-03-02T03:29:59Z", "-03:30", date(2026, 3, 1)),
            ("2026-03-02T03:30:00Z", "-03:30", date(2026, 3, 2)),
            ("2026-03-02T09:20:00Z", "-09:30", date(2026, 3, 1)),
            ("2026-03-02T09:30:00Z", "-09:30", date(2026, 3, 2)),
            ("2026-03-02T00:10:00Z", "-00:30", date(2026, 3, 1)),
            ("2026-03-02T00:30:00Z", "-00:30", date(2026, 3, 2)),
            ("2026-03-01T18:14:59Z", "+05:45", date(2026, 3, 1)),
            ("2026-03-01T18:15:00Z", "+05:45", date(2026, 3, 2)),
            ("2026-03-01T10:00:00Z", "+14:00", date(2026, 3, 2)),
            ("2026-03-02T11:59:00Z", "-12:00", date(2026, 3, 1)),
        ]
        for ts, offset, day in cases:
            self.assertEqual(local_day(parse_utc(ts), parse_offset(offset)), day, (ts, offset))


class Reports(unittest.TestCase):
    def test_harbour(self):
        result = run("report", "--site", "harbour", "data/events-harbour.jsonl")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, (
            "Harbour Street (UTC-03:30): 10 events\n"
            "day         events      sales    refunds        net\n"
            "2026-03-01       5      43.75       2.25      41.50\n"
            "2026-03-02       4      30.85       5.00      25.85\n"
            "2026-03-03       1      15.00       0.00      15.00\n"
        ))

    def test_west_half_hours(self):
        result = run("report", "--offset=-09:30", "hidden_events.jsonl")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines()[2:], [
            "2026-10-31       7      26.50       0.40      26.10",
            "2026-11-01       3      14.30       0.60      13.70",
        ])
        result = run("report", "--offset=-00:30", "hidden_events.jsonl")
        self.assertEqual(result.stdout.splitlines()[2:], [
            "2026-10-31       1      10.00       0.00      10.00",
            "2026-11-01       9      30.80       1.00      29.80",
        ])

    def test_sites_listing(self):
        result = run("sites")
        self.assertIn("harbour    UTC-03:30  Harbour Street", result.stdout.splitlines())

    def test_out_of_range_rejected(self):
        self.assertEqual(run("report", "--offset=+14:30", "hidden_events.jsonl").returncode, 2)
        self.assertEqual(run("report", "--offset=-12:45", "hidden_events.jsonl").returncode, 2)
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "sites.json")
            with open(path, "w") as f:
                json.dump({"far": {"offset": "-13:00"}}, f)
            with self.assertRaises(SiteError):
                load_sites(path)


if __name__ == "__main__":
    unittest.main()
