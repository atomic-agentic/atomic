import io
import json
import subprocess
import sys
import unittest

from tickets.export import write_tickets
from tickets.model import Ticket, TicketError, ticket_from_dict
from tickets.store import load_store


def run(*args):
    return subprocess.run([sys.executable, "-B", "-m", "tickets", *args], capture_output=True, text=True, timeout=30)


class ExportTest(unittest.TestCase):
    def test_csv(self):
        out = io.StringIO()
        rows = [Ticket(1, 'Say "hi", then go', "open", "2026-05-04T09:00:00Z")]
        self.assertEqual(write_tickets(rows, out), 1)
        self.assertEqual(out.getvalue(), 'id,updated_at,status,title\n1,2026-05-04T09:00:00Z,open,"Say ""hi"", then go"\n')

    def test_jsonl(self):
        out = io.StringIO()
        write_tickets([Ticket(1, "A", "open", "2026-05-04T09:00:00Z")], out, "jsonl")
        self.assertEqual(json.loads(out.getvalue()), {"id": 1, "updated_at": "2026-05-04T09:00:00Z", "status": "open", "title": "A"})

    def test_fixture_in_one_page(self):
        result = run("export", "--format", "jsonl", "--page-size", "100", "data/tickets.json")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(result.stdout.splitlines()), len(load_store("data/tickets.json")))
        self.assertIn("exported 37 tickets", result.stderr)

    def test_count(self):
        result = run("count", "data/tickets.json")
        self.assertEqual(result.stdout.splitlines()[0], "total\t37")

    def test_bad_record(self):
        for record in ({"id": 0, "title": "x", "status": "open", "updated_at": "2026-05-04T09:00:00Z"},
                       {"id": 1, "title": "x", "status": "done", "updated_at": "2026-05-04T09:00:00Z"},
                       {"id": 1, "title": "x", "status": "open", "updated_at": "2026-05-04 09:00:00"},
                       {"id": 1, "status": "open", "updated_at": "2026-05-04T09:00:00Z"}):
            with self.assertRaises(TicketError):
                ticket_from_dict(record)
