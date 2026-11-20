"""Semantic versions: parsing and precedence."""

import functools
import re

_IDENTIFIERS = r"[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*"
_VERSION = re.compile(
    r"^v?(?P<major>0|[1-9]\d*)\.(?P<minor>0|[1-9]\d*)\.(?P<patch>0|[1-9]\d*)"
    rf"(?:-(?P<pre>{_IDENTIFIERS}))?(?:\+(?P<build>{_IDENTIFIERS}))?$"
)


class VersionError(ValueError):
    pass


@functools.total_ordering
class Version:
    __slots__ = ("major", "minor", "patch", "prerelease", "build")

    def __init__(self, major, minor, patch, prerelease=(), build=()):
        for part in (major, minor, patch):
            if isinstance(part, bool) or not isinstance(part, int) or part < 0:
                raise VersionError(f"version parts must be non-negative integers, not {part!r}")
        self.major = major
        self.minor = minor
        self.patch = patch
        self.prerelease = tuple(prerelease)
        self.build = tuple(build)

    @classmethod
    def parse(cls, text):
        if isinstance(text, Version):
            return text
        match = _VERSION.match(text.strip()) if isinstance(text, str) else None
        if match is None:
            raise VersionError(f"not a version: {text!r}")
        pre = match.group("pre")
        build = match.group("build")
        return cls(
            int(match.group("major")),
            int(match.group("minor")),
            int(match.group("patch")),
            tuple(pre.split(".")) if pre else (),
            tuple(build.split(".")) if build else (),
        )

    @property
    def core(self):
        return (self.major, self.minor, self.patch)

    @property
    def is_prerelease(self):
        return bool(self.prerelease)

    def _key(self):
        # A release sorts after every pre-release of the same core. Digits-only identifiers compare as
        # numbers and below any other identifier, so each becomes (0, number) or (1, text).
        identifiers = tuple((0, int(part)) if part.isdigit() else (1, part) for part in self.prerelease)
        return (self.core, 0 if self.prerelease else 1, identifiers)

    def __eq__(self, other):
        if not isinstance(other, Version):
            return NotImplemented
        return self._key() == other._key()

    def __lt__(self, other):
        if not isinstance(other, Version):
            return NotImplemented
        return self._key() < other._key()

    def __hash__(self):
        return hash(self._key())

    def __str__(self):
        text = f"{self.major}.{self.minor}.{self.patch}"
        if self.prerelease:
            text += "-" + ".".join(self.prerelease)
        if self.build:
            text += "+" + ".".join(self.build)
        return text

    def __repr__(self):
        return f"Version({str(self)!r})"
