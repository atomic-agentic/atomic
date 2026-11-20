"""Version ranges: comparator sets joined by ||, and the shorthand forms that expand to them."""

import re

from verrange.comparator import ANY, Comparator
from verrange.version import Version

_SPACE_AFTER_OPERATOR = re.compile(r"(<=|>=|<|>|=)\s+")
_TOKEN = re.compile(
    r"^(?P<op><=|>=|<|>|=|~|\^)?v?"
    r"(?P<major>\d+|[xX*])(?:\.(?P<minor>\d+|[xX*]))?(?:\.(?P<patch>\d+|[xX*]))?"
    r"(?:-(?P<pre>[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$"
)


class RangeError(ValueError):
    pass


def _part(text):
    if text is None or text in ("x", "X", "*"):
        return None
    return int(text)


def _caret(major, minor, patch, pre):
    lower = Version(major, minor or 0, patch or 0, pre)
    if major > 0 or minor is None:
        upper = Version(major + 1, 0, 0)
    elif minor > 0 or patch is None:
        upper = Version(0, minor + 1, 0)
    else:
        upper = Version(0, 0, patch + 1)
    return [Comparator(">=", lower), Comparator("<", upper)]


def _tilde(major, minor, patch, pre):
    lower = Version(major, minor or 0, patch or 0, pre)
    if minor is None:
        upper = Version(major + 1, 0, 0)
    elif patch is None:
        upper = Version(major, minor + 1, 0)
    else:
        upper = Version(major, minor, patch + 1)
    return [Comparator(">=", lower), Comparator("<", upper)]


def _x_range(major, minor, patch, pre):
    if major is None:
        return [ANY]
    if minor is None:
        return [Comparator(">=", Version(major, 0, 0)), Comparator("<", Version(major + 1, 0, 0))]
    if patch is None:
        return [Comparator(">=", Version(major, minor, 0)), Comparator("<", Version(major, minor + 1, 0))]
    return [Comparator("=", Version(major, minor, patch, pre))]


def expand(token):
    """The comparators one token of a set stands for."""
    if token in ("*", "x", "X"):
        return [ANY]
    match = _TOKEN.match(token)
    if match is None:
        raise RangeError(f"not a comparator: {token!r}")
    op = match.group("op") or ""
    major, minor, patch = (_part(match.group(name)) for name in ("major", "minor", "patch"))
    if major is None and (minor is not None or patch is not None):
        raise RangeError(f"not a comparator: {token!r}")
    if minor is None and patch is not None:
        raise RangeError(f"not a comparator: {token!r}")
    pre = tuple(match.group("pre").split(".")) if match.group("pre") else ()
    if pre and patch is None:
        raise RangeError(f"a pre-release needs a full version: {token!r}")
    if op == "^":
        if major is None:
            return [ANY]
        return _caret(major, minor, patch, pre)
    if op == "~":
        if major is None:
            return [ANY]
        return _tilde(major, minor, patch, pre)
    if op in ("", "="):
        return _x_range(major, minor, patch, pre)
    if patch is None:
        raise RangeError(f"{op} needs a full version: {token!r}")
    return [Comparator(op, Version(major, minor, patch, pre))]


class Range:
    def __init__(self, text):
        self.text = text
        self.sets = []
        for part in text.split("||"):
            part = _SPACE_AFTER_OPERATOR.sub(r"\1", part.strip())
            comparators = []
            for token in part.split():
                comparators.extend(expand(token))
            self.sets.append(comparators or [ANY])

    def test(self, version):
        version = Version.parse(version)
        return any(_set_allows(comparators, version) for comparators in self.sets)

    def __str__(self):
        return " || ".join(" ".join(str(c) for c in comparators) for comparators in self.sets)


def _set_allows(comparators, version):
    if not all(c.test(version) for c in comparators):
        return False
    if not version.is_prerelease:
        return True
    return any(c.version.is_prerelease and c.version.core == version.core for c in comparators)


def satisfies(version, range_text):
    """True if the version satisfies the range. Both may be strings."""
    version = Version.parse(version)
    rng = range_text if isinstance(range_text, Range) else Range(range_text)
    return rng.test(version)
