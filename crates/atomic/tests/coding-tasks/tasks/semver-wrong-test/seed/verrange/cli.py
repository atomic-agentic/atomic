"""python3 -m verrange [--all] RANGE VERSION..."""

import argparse
import sys

from verrange.ranges import RangeError
from verrange.resolve import all_satisfying
from verrange.version import VersionError


def main(argv):
    parser = argparse.ArgumentParser(prog="verrange", description="Pick versions that satisfy a range.")
    parser.add_argument("--all", action="store_true", help="print every satisfying version, lowest first")
    parser.add_argument("range", help="a range, such as '^1.2.0 || ~2.0.1'")
    parser.add_argument("versions", nargs="+", help="the versions to choose from")
    args = parser.parse_args(argv)
    try:
        matching = all_satisfying(args.versions, args.range)
    except (RangeError, VersionError) as error:
        print(f"verrange: {error}", file=sys.stderr)
        return 2
    if not matching:
        print(f"no version satisfies {args.range}", file=sys.stderr)
        return 1
    for text in matching if args.all else matching[-1:]:
        print(text)
    return 0
