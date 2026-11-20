import json
import random
import subprocess
import sys
import unittest

from tickets.api import list_tickets
from tickets.model import Ticket
from tickets.store import TicketStore, load_store

STATUSES = ("open", "pending", "closed")


def make_store(rng, n, stamps):
    ids = rng.sample(range(1, 10 * n + 10), n)
    pool = [f"2026-05-04T09:{m:02d}:00Z" for m in range(stamps)]
    return [Ticket(id=i, title=f"T{i}", status=rng.choice(STATUSES), updated_at=rng.choice(pool)) for i in ids]


def walk(store, page_size, status=None, limit_pages=None):
    """Every page, failing rather than looping if the cursors never end."""
    pages, cursor = [], None
    limit_pages = limit_pages or len(store) + 3
    while True:
        page = list_tickets(store, page_size=page_size, cursor=cursor, status=status)
        pages.append(page)
        if page.next_cursor is None:
            return pages
        if len(pages) > limit_pages:
            raise AssertionError(f"more than {limit_pages} pages at page size {page_size}: the cursors do not end")
        cursor = page.next_cursor


class Hidden(unittest.TestCase):
    def check(self, tickets, page_size, status=None):
        store = TicketStore(tickets)
        expected = [t.id for t in sorted(tickets, key=lambda t: (t.updated_at, t.id))
                    if status is None or t.status == status]
        pages = walk(store, page_size, status)
        got = [t.id for p in pages for t in p.items]
        where = f"page size {page_size}, status {status}, {len(tickets)} tickets"
        self.assertEqual(got, expected, where)
        for page in pages[:-1]:
            self.assertEqual(len(page.items), page_size, where)
            self.assertIsInstance(page.next_cursor, str, where)
        self.assertIsNone(pages[-1].next_cursor, where)
        if expected:
            self.assertGreater(len(pages[-1].items), 0, where)
            self.assertEqual(len(pages), -(-len(expected) // page_size), where)
        else:
            self.assertEqual(len(pages), 1, where)

    def test_heavy_ties_every_page_size(self):
        rng = random.Random(20261108)
        for n, stamps in ((1, 1), (5, 1), (6, 2), (23, 3), (40, 4), (40, 40), (64, 5)):
            tickets = make_store(rng, n, stamps)
            for page_size in list(range(1, n + 2)) + [100]:
                self.check(tickets, page_size)

    def test_heavy_ties_with_status(self):
        rng = random.Random(77)
        tickets = make_store(rng, 45, 3)
        for status in STATUSES:
            for page_size in range(1, 20):
                self.check(tickets, page_size, status)

    def test_fixture(self):
        self.assertEqual(len(load_store("data/tickets.json")), 37)
        with open("data/tickets.json") as f:
            records = json.load(f)
        tickets = [Ticket(**r) for r in records]
        for page_size in range(1, 40):
            self.check(tickets, page_size)
            self.check(tickets, page_size, "open")

    def test_export_command(self):
        with open("data/tickets.json") as f:
            records = json.load(f)
        expected = [r["id"] for r in sorted(records, key=lambda r: (r["updated_at"], r["id"]))]
        for page_size in (1, 2, 3, 5, 10, 11, 12, 13, 25, 36, 37, 38):
            result = subprocess.run(
                [sys.executable, "-B", "-m", "tickets", "export", "--format", "jsonl", "--page-size", str(page_size),
                 "data/tickets.json"], capture_output=True, text=True, timeout=30)
            self.assertEqual(result.returncode, 0, result.stderr)
            got = [json.loads(line)["id"] for line in result.stdout.splitlines()]
            self.assertEqual(got, expected, f"page size {page_size}")
            self.assertIn("exported 37 tickets", result.stderr)
        result = subprocess.run(
            [sys.executable, "-B", "-m", "tickets", "export", "--page-size", "10", "data/tickets.json"],
            capture_output=True, text=True, timeout=30)
        self.assertEqual(len(result.stdout.splitlines()), 38)

    def test_validation(self):
        store = TicketStore([Ticket(id=1, title="A", status="open", updated_at="2026-05-04T09:00:00Z")])
        for size in (0, -1, 101, None, "5"):
            with self.assertRaises(ValueError, msg=repr(size)):
                list_tickets(store, page_size=size)
        for cursor in ("", "garbage", "eyJ2IjoxfQ"):
            with self.assertRaises(ValueError, msg=cursor):
                list_tickets(store, cursor=cursor)


if __name__ == "__main__":
    unittest.main()
