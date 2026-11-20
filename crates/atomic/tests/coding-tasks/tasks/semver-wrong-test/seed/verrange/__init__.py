"""verrange: semantic versions and version ranges."""

from verrange.ranges import Range, satisfies
from verrange.resolve import all_satisfying, max_satisfying, min_satisfying
from verrange.version import Version

__all__ = ["Range", "Version", "all_satisfying", "max_satisfying", "min_satisfying", "satisfies"]
