"""Timestamps as the order service writes them: UTC, to the second, like 2026-10-04T09:15:02Z."""

import re
from datetime import datetime, timezone

_TS = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$")


def parse(text):
    """A timezone-aware UTC datetime, or ValueError."""
    if not isinstance(text, str) or not _TS.match(text):
        raise ValueError(f"not a timestamp like 2026-10-04T09:15:02Z: {text!r}")
    return datetime.strptime(text, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)


def format(moment):
    return moment.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def span(moments):
    """(earliest, latest) of some datetimes, or (None, None) for none."""
    moments = list(moments)
    if not moments:
        return None, None
    return min(moments), max(moments)
