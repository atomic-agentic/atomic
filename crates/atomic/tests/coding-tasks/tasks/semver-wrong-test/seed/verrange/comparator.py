"""One comparator: an operator and a version."""

import operator
from dataclasses import dataclass

from verrange.version import Version

OPERATORS = {
    "<": operator.lt,
    "<=": operator.le,
    ">": operator.gt,
    ">=": operator.ge,
    "=": operator.eq,
}


@dataclass(frozen=True)
class Comparator:
    op: str
    version: Version

    def __post_init__(self):
        if self.op not in OPERATORS:
            raise ValueError(f"unknown operator {self.op!r}")

    def test(self, version):
        return OPERATORS[self.op](version, self.version)

    def __str__(self):
        return f"{self.op}{self.version}"


ANY = Comparator(">=", Version(0, 0, 0))
