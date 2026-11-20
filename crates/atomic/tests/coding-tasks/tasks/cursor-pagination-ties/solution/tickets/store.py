"""The ticket store: every ticket in memory, kept in listing order."""

import bisect
import json

from .model import TicketError, ticket_from_dict


def listing_key(ticket):
    """The order tickets are listed in: oldest update first, then by id."""
    return (ticket.updated_at, ticket.id)


class TicketStore:
    def __init__(self, tickets=()):
        self._by_id = {}
        for ticket in tickets:
            if ticket.id in self._by_id:
                raise TicketError(f"duplicate ticket id {ticket.id}")
            self._by_id[ticket.id] = ticket
        self._ordered = sorted(self._by_id.values(), key=listing_key)
        # Kept beside _ordered so that a page can start with a binary search, not a scan.
        self._keys = [listing_key(t) for t in self._ordered]

    def __len__(self):
        return len(self._ordered)

    def get(self, ticket_id):
        try:
            return self._by_id[ticket_id]
        except KeyError:
            raise KeyError(f"no ticket {ticket_id}") from None

    def counts(self):
        out = {}
        for ticket in self._ordered:
            out[ticket.status] = out.get(ticket.status, 0) + 1
        return out

    def query(self, after=None, limit=None, status=None):
        """Tickets in listing order that come after the position `after`, at most `limit` of them.

        `after` is where the previous page ended, as a listing key (updated_at, id), or None to start at
        the beginning. Many tickets can share an updated_at, so the id is needed to know which of them
        the page ended with.
        """
        start = 0 if after is None else bisect.bisect_right(self._keys, tuple(after))
        out = []
        for ticket in self._ordered[start:]:
            if status is not None and ticket.status != status:
                continue
            out.append(ticket)
            if limit is not None and len(out) >= limit:
                break
        return out


def load_store(path):
    with open(path, encoding="utf-8") as f:
        try:
            records = json.load(f)
        except json.JSONDecodeError as e:
            raise TicketError(f"{path}: not JSON: {e.msg}") from None
    if not isinstance(records, list):
        raise TicketError(f"{path}: expected a list of tickets")
    return TicketStore(ticket_from_dict(r) for r in records)
