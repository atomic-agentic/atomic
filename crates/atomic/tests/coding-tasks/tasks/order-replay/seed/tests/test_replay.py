import io
import os
import tempfile
import unittest

from orders import cli, report
from orders.events import Event
from orders.replay import replay


def ev(seq, minute, order, kind):
    return Event(seq=seq, ts=f"2026-10-04T10:{minute:02d}:00Z", order=order, kind=kind)


class ReplayTest(unittest.TestCase):
    def test_states_and_problems(self):
        result = replay([
            ev(1, 0, "B-2", "create"),
            ev(2, 1, "B-1", "create"),
            ev(3, 2, "B-1", "pay"),
            ev(4, 3, "B-2", "ship"),
            ev(5, 4, "B-1", "ship"),
        ])
        self.assertEqual(result.states, {"B-1": "shipped", "B-2": "pending"})
        self.assertEqual(report.replay_text(result),
                         "B-1 shipped\nB-2 pending\ninvalid: seq 4 order B-2 ship from pending\n"
                         "2 orders, 5 events, 1 invalid\n")
        self.assertFalse(result.ok)

    def test_no_create(self):
        result = replay([ev(1, 0, "B-3", "pay")])
        self.assertEqual(result.states, {"B-3": "none"})
        self.assertEqual(report.replay_text(result),
                         "B-3 none\ninvalid: seq 1 order B-3 pay from none\n1 order, 1 event, 1 invalid\n")

    def test_summary(self):
        result = replay([ev(1, 0, "B-1", "create"), ev(2, 5, "B-2", "create"), ev(3, 9, "B-2", "cancel")])
        self.assertEqual(report.summary_text(result),
                         "pending 1\ncancelled 1\n2 orders, from 2026-10-04T10:00:00Z to 2026-10-04T10:09:00Z\n")


class CliTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()

    def tearDown(self):
        self.dir.cleanup()

    def write(self, name, events):
        path = os.path.join(self.dir.name, name)
        with open(path, "w", encoding="utf-8") as f:
            f.write("".join(event.to_json() + "\n" for event in events))
        return path

    def run_cli(self, *argv):
        out, err = io.StringIO(), io.StringIO()
        return cli.main(list(argv), out=out, err=err), out.getvalue(), err.getvalue()

    def test_exit_codes(self):
        good = self.write("good.jsonl", [ev(1, 0, "C-1", "create"), ev(2, 1, "C-1", "pay")])
        bad = self.write("bad.jsonl", [ev(3, 2, "C-1", "deliver")])
        self.assertEqual(self.run_cli("replay", good)[0], 0)
        self.assertEqual(self.run_cli("replay", good, bad)[0], 1)
        self.assertEqual(self.run_cli("summary", good, bad)[0], 1)
        self.assertEqual(self.run_cli("replay")[0], 2)
        self.assertEqual(self.run_cli("replay", os.path.join(self.dir.name, "missing.jsonl"))[0], 2)

    def test_exact_duplicates_are_dropped(self):
        path = self.write("dup.jsonl", [ev(1, 0, "C-2", "create"), ev(1, 0, "C-2", "create")])
        code, out, _ = self.run_cli("replay", path)
        self.assertEqual((code, out), (0, "C-2 pending\n1 order, 1 event, 0 invalid\n"))

    def test_states_table(self):
        code, out, _ = self.run_cli("states")
        self.assertEqual(code, 0)
        self.assertIn("return_pending  refund   refunded", out)


if __name__ == "__main__":
    unittest.main()
