"""Text helpers shared by the formatter and the collector check."""

import re

_CONTROL = re.compile(r"[\x00-\x1f\x7f]+")
_WHITESPACE = re.compile(r"\s+")


def clean(text):
    """Turns each run of control characters into one space, and strips the ends."""
    return _CONTROL.sub(" ", text).strip(" ")


def token(name):
    """A host or app name as one token: whitespace becomes '-', and empty becomes '-'."""
    name = _WHITESPACE.sub("-", name.strip())
    return name or "-"


def utf8_len(text):
    """The length of text in bytes, encoded as UTF-8."""
    return len(text.encode("utf-8"))


def truncate_bytes(text, limit):
    """Text cut to at most limit bytes of UTF-8."""
    if limit <= 0:
        return ""
    encoded = text.encode("utf-8")
    if len(encoded) <= limit:
        return text
    return encoded[:limit].decode("utf-8", errors="replace")
