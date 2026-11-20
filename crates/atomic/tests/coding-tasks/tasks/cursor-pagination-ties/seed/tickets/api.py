"""The listing API, as the HTTP layer calls it: one page of tickets and a cursor to the next."""

from dataclasses import dataclass, field

from .cursor import CursorError, decode_cursor, encode_cursor

DEFAULT_PAGE_SIZE = 25
MAX_PAGE_SIZE = 100


@dataclass
class Page:
    items: list = field(default_factory=list)
    next_cursor: object = None  # str, or None on the last page


def _check_page_size(page_size):
    if not isinstance(page_size, int) or isinstance(page_size, bool) or not 1 <= page_size <= MAX_PAGE_SIZE:
        raise ValueError(f"page_size must be between 1 and {MAX_PAGE_SIZE}")


def _position(ticket):
    """Where a page that ends with this ticket ends."""
    return ticket.updated_at


def _after(cursor):
    if cursor is None:
        return None
    position = decode_cursor(cursor)
    if not isinstance(position, str):
        raise CursorError("invalid cursor")
    return position


def list_tickets(store, page_size=DEFAULT_PAGE_SIZE, cursor=None, status=None):
    """One page of tickets in listing order. See README.md."""
    _check_page_size(page_size)
    # One more than a page, to know whether another page follows without a second query.
    rows = store.query(after=_after(cursor), limit=page_size + 1, status=status)
    items = rows[:page_size]
    next_cursor = encode_cursor(_position(items[-1])) if len(rows) > page_size else None
    return Page(items=items, next_cursor=next_cursor)
