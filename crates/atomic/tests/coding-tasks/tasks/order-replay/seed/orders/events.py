"""One log line: an event the order service accepted."""

import json
from dataclasses import dataclass

from . import timeutil, transitions

FIELDS = ("seq", "ts", "order", "kind")


@dataclass(frozen=True)
class Event:
    seq: int
    ts: str  # as written; see timeutil.parse
    order: str
    kind: str

    @property
    def moment(self):
        return timeutil.parse(self.ts)

    def to_json(self):
        return json.dumps({"seq": self.seq, "ts": self.ts, "order": self.order, "kind": self.kind})


class BadLine(ValueError):
    pass


def parse_line(text, where="line"):
    """An Event from one JSON line, or BadLine saying what is wrong with it."""
    try:
        data = json.loads(text)
    except json.JSONDecodeError as e:
        raise BadLine(f"{where}: not JSON: {e.msg}") from None
    if not isinstance(data, dict):
        raise BadLine(f"{where}: not a JSON object")
    missing = [name for name in FIELDS if name not in data]
    if missing:
        raise BadLine(f"{where}: missing {', '.join(missing)}")
    seq = data["seq"]
    if not isinstance(seq, int) or isinstance(seq, bool) or seq < 0:
        raise BadLine(f"{where}: seq must be a whole number, not {seq!r}")
    try:
        timeutil.parse(data["ts"])
    except ValueError as e:
        raise BadLine(f"{where}: {e}") from None
    order = data["order"]
    if not isinstance(order, str) or not order:
        raise BadLine(f"{where}: order must be a non-empty string")
    kind = data["kind"]
    if not transitions.is_kind(kind):
        raise BadLine(f"{where}: unknown event kind {kind!r}")
    return Event(seq=seq, ts=data["ts"], order=order, kind=kind)
