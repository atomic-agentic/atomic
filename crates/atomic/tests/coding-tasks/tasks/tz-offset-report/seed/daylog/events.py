"""Reading event logs: one JSON object a line."""

import json
from dataclasses import dataclass
from decimal import Decimal, InvalidOperation

from .clock import parse_utc

KINDS = ("sale", "refund")


@dataclass(frozen=True)
class Event:
    at: object  # aware datetime, UTC
    kind: str
    amount: Decimal


def parse_amount(text):
    if not isinstance(text, str):
        raise ValueError(f"amount must be a string, not {type(text).__name__}")
    try:
        value = Decimal(text)
    except InvalidOperation:
        raise ValueError(f"not an amount: {text!r}") from None
    if not value.is_finite() or value < 0 or value.as_tuple().exponent < -2:
        raise ValueError(f"not an amount: {text!r}")
    return value.quantize(Decimal("0.01"))


def parse_event(line):
    """One log line as an Event. Raises ValueError with a short message."""
    try:
        record = json.loads(line)
    except json.JSONDecodeError as e:
        raise ValueError(f"not JSON: {e.msg}") from None
    if not isinstance(record, dict):
        raise ValueError("not a JSON object")
    missing = [key for key in ("ts", "kind", "amount") if key not in record]
    if missing:
        raise ValueError(f"no {missing[0]}")
    if record["kind"] not in KINDS:
        raise ValueError(f"unknown kind {record['kind']!r}")
    return Event(at=parse_utc(record["ts"]), kind=record["kind"], amount=parse_amount(record["amount"]))


def read_events(path):
    """(events, problems), where each problem is 'PATH:N: message'."""
    events, problems = [], []
    with open(path, encoding="utf-8") as f:
        for number, line in enumerate(f, start=1):
            if not line.strip():
                continue
            try:
                events.append(parse_event(line))
            except ValueError as e:
                problems.append(f"{path}:{number}: {e}")
    return events, problems
