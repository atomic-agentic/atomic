import unittest

from tickets.api import list_tickets
from tickets.model import Ticket
from tickets.store import TicketStore


def ticket(tid, minute, status="open"):
    return Ticket(id=tid, title=f"Ticket {tid}", status=status, updated_at=f"2026-05-04T09:{minute:02d}:00Z")


# Ten tickets, each updated in its own minute, ids out of order.
STORE = TicketStore([ticket(7, 5), ticket(3, 1), ticket(9, 9, "closed"), ticket(1, 3), ticket(4, 2, "pending"),
                     ticket(8, 8), ticket(2, 0, "closed"), ticket(6, 6), ticket(5, 4), ticket(10, 7, "pending")])
ORDER = [2, 3, 4, 1, 5, 7, 6, 10, 8, 9]


def walk(store, page_size, **filters):
    pages, cursor = [], None
    while True:
        page = list_tickets(store, page_size=page_size, cursor=cursor, **filters)
        pages.append([t.id for t in page.items])
        if page.next_cursor is None:
            return pages
        cursor = page.next_cursor


class ApiTest(unittest.TestCase):
    def test_one_page(self):
        page = list_tickets(STORE)
        self.assertEqual([t.id for t in page.items], ORDER)
        self.assertIsNone(page.next_cursor)

    def test_pages(self):
        self.assertEqual(walk(STORE, 4), [[2, 3, 4, 1], [5, 7, 6, 10], [8, 9]])
        self.assertEqual(walk(STORE, 5), [[2, 3, 4, 1, 5], [7, 6, 10, 8, 9]])
        self.assertEqual(walk(STORE, 1), [[i] for i in ORDER])

    def test_status(self):
        self.assertEqual(walk(STORE, 2, status="open"), [[3, 1], [5, 7], [6, 8]])
        self.assertEqual(walk(STORE, 2, status="closed"), [[2, 9]])

    def test_empty(self):
        page = list_tickets(TicketStore([]), page_size=3)
        self.assertEqual((page.items, page.next_cursor), ([], None))

    def test_page_size(self):
        for size in (0, -1, 101, "10", 2.5, True):
            with self.assertRaises(ValueError, msg=repr(size)):
                list_tickets(STORE, page_size=size)
        self.assertEqual(len(list_tickets(STORE, page_size=100).items), 10)

    def test_bad_cursor(self):
        with self.assertRaises(ValueError):
            list_tickets(STORE, cursor="garbage")
