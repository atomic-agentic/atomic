import json
import os
import random
import subprocess
import sys
import tempfile
import unittest

EXPECTED = """A-1001 delivered
A-1002 cancelled
A-1003 refunded
A-1004 shipped
A-1005 pending
A-1006 refunded
A-1007 delivered
invalid: seq 4111 order A-1004 ship from pending
7 orders, 25 events, 1 invalid
"""

EXPECTED_SUMMARY = """pending 1
shipped 1
delivered 2
cancelled 1
refunded 2
7 orders, from 2026-10-04T08:00:11Z to 2026-10-04T17:42:09Z
"""


def line(seq, ts, order, kind):
    return json.dumps({"seq": seq, "ts": f"2026-10-05T{ts}Z", "order": order, "kind": kind}) + "\n"


class Hidden(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        with open("examples/orders.jsonl", encoding="utf-8") as f:
            self.example = f.readlines()

    def tearDown(self):
        self.dir.cleanup()

    def write(self, name, lines):
        path = os.path.join(self.dir.name, name)
        with open(path, "w", encoding="utf-8") as f:
            f.writelines(lines)
        return path

    def orders(self, *argv):
        result = subprocess.run([sys.executable, "-B", "-m", "orders", *argv], capture_output=True, text=True,
                                timeout=30)
        return result.returncode, result.stdout, result.stderr

    def test_example(self):
        self.assertEqual(self.orders("replay", "examples/orders.jsonl")[:2], (1, EXPECTED))
        self.assertEqual(self.orders("summary", "examples/orders.jsonl")[:2], (1, EXPECTED_SUMMARY))

    def test_any_line_order(self):
        rng = random.Random(4107)
        for n in range(4):
            lines = list(self.example)
            rng.shuffle(lines)
            path = self.write(f"shuffled-{n}.jsonl", lines)
            self.assertEqual(self.orders("replay", path)[:2], (1, EXPECTED), n)

    def test_files_in_either_order(self):
        half = len(self.example) // 2
        a = self.write("a.jsonl", self.example[:half])
        b = self.write("b.jsonl", self.example[half:])
        self.assertEqual(self.orders("replay", b, a)[:2], (1, EXPECTED))
        self.assertEqual(self.orders("replay", a, b)[:2], (1, EXPECTED))

    def test_skewed_clocks_and_shared_seconds(self):
        path = self.write("skew.jsonl", [
            line(12, "09:00:09", "D-3", "pay"),
            line(10, "09:00:05", "D-3", "create"),
            line(13, "09:00:04", "D-3", "ship"),
            line(14, "09:00:04", "D-3", "deliver"),
            line(20, "09:10:00", "D-4", "create"),
            line(21, "09:10:00", "D-4", "pay"),
            line(22, "09:10:00", "D-4", "cancel"),
            line(30, "09:20:00", "D-5", "create"),
            line(31, "09:20:00", "D-5", "pay"),
            line(32, "09:19:58", "D-5", "refund"),
        ])
        self.assertEqual(self.orders("replay", path)[:2],
                         (0, "D-3 delivered\nD-4 cancelled\nD-5 refunded\n3 orders, 10 events, 0 invalid\n"))

    def test_real_problems_still_reported(self):
        path = self.write("real.jsonl", [
            line(5, "09:00:03", "E-1", "cancel"),
            line(3, "09:00:01", "E-1", "create"),
            line(6, "09:00:01", "E-1", "pay"),
            line(4, "09:00:02", "E-2", "pay"),
            line(7, "09:00:09", "E-2", "create"),
            line(8, "09:00:09", "E-2", "pay"),
            line(9, "09:00:09", "E-2", "deliver"),
        ])
        self.assertEqual(self.orders("replay", path)[:2], (1, (
            "E-1 cancelled\nE-2 paid\n"
            "invalid: seq 4 order E-2 pay from none\n"
            "invalid: seq 6 order E-1 pay from cancelled\n"
            "invalid: seq 9 order E-2 deliver from paid\n"
            "2 orders, 7 events, 3 invalid\n")))

    def test_duplicates(self):
        dup = self.write("dup.jsonl", [line(2, "09:00:01", "F-1", "pay"), line(1, "09:00:01", "F-1", "create"),
                                       line(2, "09:00:01", "F-1", "pay")])
        self.assertEqual(self.orders("replay", dup)[:2], (0, "F-1 paid\n1 order, 2 events, 0 invalid\n"))
        clash = self.write("clash.jsonl", [line(1, "09:00:01", "F-2", "create"), line(1, "09:00:01", "F-2", "pay")])
        code, out, err = self.orders("replay", clash)
        self.assertEqual(code, 2)
        self.assertIn("conflicting events for seq 1", err)


if __name__ == "__main__":
    unittest.main()
