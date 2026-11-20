"""python3 -m deployctl check FILE | show FILE"""

import argparse
import os
import sys

from deployctl import interpolate, rules
from deployctl.loader import LoadError, load


def display(path, root):
    """A path as problems show it: relative to the checked file's directory when it is inside it."""
    relative = os.path.relpath(path, root)
    return path if relative.startswith("..") else relative


def problems_for(config):
    """(path, line, message) for every problem in a loaded config, in file and line order."""
    values, broken = interpolate.expand(config)
    found = [(v.path, v.line, message) for v, message in broken]
    for section, key, message in rules.run(values):
        where = config.value(section, key) if key else None
        if where is not None:
            found.append((where.path, where.line, message))
        else:
            found.append((config.files[0], 0, message))
    order = {path: i for i, path in enumerate(config.files)}
    return sorted(set(found), key=lambda p: (order.get(p[0], len(order)), p[1], p[2]))


def cmd_check(args):
    try:
        config = load(args.file)
    except LoadError as error:
        root = os.path.dirname(os.path.abspath(args.file))
        print(f"{display(error.path, root)}:{error.line}: error: {error.message}")
        return 1
    found = problems_for(config)
    for path, line, message in found:
        print(f"{display(path, config.root)}:{line}: error: {message}")
    if found:
        return 1
    if not args.quiet:
        count = sum(len(v) for v in config.sections.values())
        print(f"ok: {count} settings from {len(config.files)} files")
    return 0


def cmd_show(args):
    try:
        config = load(args.file)
    except LoadError as error:
        print(f"deployctl: {error}", file=sys.stderr)
        return 1
    values, _ = interpolate.expand(config)
    for section in sorted(values):
        print(f"[{section}]")
        for key in sorted(values[section]):
            print(f"{key} = {values[section][key]}")
        print()
    return 0


def build_parser():
    parser = argparse.ArgumentParser(prog="deployctl", description="Check deploy files.")
    commands = parser.add_subparsers(dest="command", required=True, metavar="COMMAND")
    check = commands.add_parser("check", help="check a deploy file and its includes")
    check.add_argument("file")
    check.add_argument("-q", "--quiet", action="store_true", help="print nothing when the file is clean")
    show = commands.add_parser("show", help="print every setting, expanded")
    show.add_argument("file")
    return parser


def main(argv=None):
    args = build_parser().parse_args(argv)
    return {"check": cmd_check, "show": cmd_show}[args.command](args)
