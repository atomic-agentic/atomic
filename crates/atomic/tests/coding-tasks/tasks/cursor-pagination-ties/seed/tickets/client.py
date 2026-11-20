"""Walking every page, as a remote client does."""


class PagingError(RuntimeError):
    """The server handed back a cursor that does not move forward."""


def iter_tickets(fetch, page_size, **filters):
    """Every ticket, following cursors page by page.

    fetch(page_size=..., cursor=..., **filters) returns a Page; in tests it is list_tickets bound to a
    store, and in production a call over HTTP.
    """
    cursor = None
    seen_cursors = set()
    while True:
        page = fetch(page_size=page_size, cursor=cursor, **filters)
        yield from page.items
        if page.next_cursor is None:
            return
        if page.next_cursor in seen_cursors:
            raise PagingError("the cursor did not advance")
        seen_cursors.add(page.next_cursor)
        cursor = page.next_cursor
