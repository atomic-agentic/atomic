"""UTC offsets as sites write them: +HH:MM or -HH:MM."""

import re
from datetime import timedelta

_OFFSET = re.compile(r"(?P<sign>[+-])(?P<hours>\d{2}):(?P<minutes>\d{2})")

# The furthest zones in use, west and east of UTC.
MAX_WEST = timedelta(hours=12)
MAX_EAST = timedelta(hours=14)


def parse_offset(text):
    """'+05:30' as timedelta(hours=5, minutes=30). Raises ValueError for anything else."""
    match = _OFFSET.fullmatch(text.strip()) if isinstance(text, str) else None
    if match is None:
        raise ValueError(f"not a UTC offset: {text!r}")
    hours = int(match["hours"])
    minutes = int(match["minutes"])
    if minutes > 59:
        raise ValueError(f"minutes out of range in UTC offset: {text!r}")
    # The sign applies to the whole offset: -03:30 is three and a half hours behind UTC.
    offset = timedelta(hours=hours, minutes=minutes)
    if match["sign"] == "-":
        offset = -offset
    if not -MAX_WEST <= offset <= MAX_EAST:
        raise ValueError(f"UTC offset out of range: {text!r}")
    return offset


def format_offset(delta):
    """timedelta(hours=-3, minutes=-30) as '-03:30'."""
    total = int(delta.total_seconds()) // 60
    sign = "-" if total < 0 else "+"
    hours, minutes = divmod(abs(total), 60)
    return f"{sign}{hours:02d}:{minutes:02d}"
