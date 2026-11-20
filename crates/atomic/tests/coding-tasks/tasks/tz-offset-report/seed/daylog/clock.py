"""Instants in UTC, and the local day they fall on at a site."""

from datetime import datetime, timedelta, timezone

UTC_SUFFIXES = ("Z", "+00:00")


def parse_utc(text):
    """'2026-03-02T03:10:00Z' as an aware datetime in UTC. Raises ValueError otherwise."""
    if not isinstance(text, str) or not text.endswith(UTC_SUFFIXES):
        raise ValueError(f"not a UTC timestamp: {text!r}")
    body = text[:-1] if text.endswith("Z") else text[: -len("+00:00")]
    try:
        moment = datetime.strptime(body, "%Y-%m-%dT%H:%M:%S")
    except ValueError:
        raise ValueError(f"not a UTC timestamp: {text!r}") from None
    return moment.replace(tzinfo=timezone.utc)


def to_local(moment, offset):
    """The wall-clock time at a site with this offset, as an aware datetime."""
    if moment.tzinfo is None:
        raise ValueError("a naive datetime has no instant")
    if not isinstance(offset, timedelta):
        raise TypeError("offset must be a timedelta")
    return moment.astimezone(timezone(offset))


def local_day(moment, offset):
    """The calendar day at the site when this instant happened."""
    return to_local(moment, offset).date()
