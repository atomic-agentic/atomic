"""Picking versions from a list: the highest, the lowest, or all that satisfy a range."""

from verrange.ranges import Range
from verrange.version import Version


def all_satisfying(versions, range_text):
    """Every version that satisfies the range, lowest first, as the strings given."""
    rng = Range(range_text)
    parsed = [(Version.parse(text), text) for text in versions]
    return [text for version, text in sorted(parsed, key=lambda pair: pair[0]) if rng.test(version)]


def max_satisfying(versions, range_text):
    matching = all_satisfying(versions, range_text)
    return matching[-1] if matching else None


def min_satisfying(versions, range_text):
    matching = all_satisfying(versions, range_text)
    return matching[0] if matching else None
