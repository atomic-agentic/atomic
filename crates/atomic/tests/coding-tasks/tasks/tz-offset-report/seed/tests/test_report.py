import subprocess
import sys
import unittest
from decimal import Decimal

from daylog.events import parse_event
from daylog.sites import load_sites


def run(*args):
    return subprocess.run([sys.executable, "-B", "-m", "daylog", *args], capture_output=True, text=True, timeout=30)


class EventTest(unittest.TestCase):
    def test_parse(self):
        event = parse_event('{"ts": "2026-03-01T03:00:00Z", "kind": "refund", "amount": "1.5"}')
        self.assertEqual((event.kind, event.amount), ("refund", Decimal("1.50")))
        for line in ('{"ts": "2026-03-01T03:00:00Z", "kind": "sale"}', '{"ts": "x", "kind": "sale", "amount": "1"}',
                     '{"ts": "2026-03-01T03:00:00Z", "kind": "gift", "amount": "1"}', "[]", "{",
                     '{"ts": "2026-03-01T03:00:00Z", "kind": "sale", "amount": "1.005"}'):
            with self.assertRaises(ValueError, msg=line):
                parse_event(line)


class ReportTest(unittest.TestCase):
    def test_sites(self):
        sites = load_sites("sites.json")
        self.assertEqual(sorted(sites), ["delta", "depot", "harbour", "ridge"])
        self.assertEqual(sites["delta"].name, "Delta Market")

    def test_delta(self):
        result = run("report", "--site", "delta", "data/events-delta.jsonl")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, (
            "Delta Market (UTC+05:30): 5 events\n"
            "day         events      sales    refunds        net\n"
            "2026-03-01       2     165.50       0.00     165.50\n"
            "2026-03-02       2      30.00      15.00      15.00\n"
            "2026-03-03       1       8.80       0.00       8.80\n"
        ))

    def test_whole_hours_west(self):
        result = run("report", "--offset=-05:00", "data/events-delta.jsonl")
        self.assertEqual(result.stdout.splitlines()[2:], [
            "2026-02-28       1     120.00       0.00     120.00",
            "2026-03-01       2      75.50       0.00      75.50",
            "2026-03-02       2       8.80      15.00      -6.20",
        ])

    def test_bad_line(self):
        result = run("check", "sites.json")
        self.assertEqual(result.returncode, 1)
        self.assertIn("sites.json:1: ", result.stderr)

    def test_bad_offset(self):
        result = run("report", "--offset", "+5:30", "data/events-delta.jsonl")
        self.assertEqual(result.returncode, 2)
        self.assertIn("not a UTC offset", result.stderr)
