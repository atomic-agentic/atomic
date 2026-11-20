"""A ticket, and checking one read from a file."""

import re
from dataclasses import asdict, dataclass

STATUSES = ("open", "pending", "closed")
_STAMP = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z")


class TicketError(ValueError):
    """A ticket record that is wrong."""


@dataclass(frozen=True)
class Ticket:
    id: int
    title: str
    status: str
    updated_at: str  # UTC, YYYY-MM-DDTHH:MM:SSZ, so that text order is time order

    def to_dict(self):
        return asdict(self)


def ticket_from_dict(record):
    if not isinstance(record, dict):
        raise TicketError("a ticket must be an object")
    try:
        tid, title, status, updated_at = (record[k] for k in ("id", "title", "status", "updated_at"))
    except KeyError as e:
        raise TicketError(f"a ticket has no {e.args[0]}") from None
    if not isinstance(tid, int) or isinstance(tid, bool) or tid <= 0:
        raise TicketError(f"not a ticket id: {tid!r}")
    if not isinstance(title, str) or not title.strip():
        raise TicketError(f"ticket {tid}: empty title")
    if status not in STATUSES:
        raise TicketError(f"ticket {tid}: unknown status {status!r}")
    if not isinstance(updated_at, str) or not _STAMP.fullmatch(updated_at):
        raise TicketError(f"ticket {tid}: not a UTC timestamp: {updated_at!r}")
    return Ticket(id=tid, title=title.strip(), status=status, updated_at=updated_at)
