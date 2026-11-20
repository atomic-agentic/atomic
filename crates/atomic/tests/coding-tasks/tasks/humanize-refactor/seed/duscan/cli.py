"""python3 -m duscan PATH: where the bytes are."""

import argparse
import sys
import time

from duscan import __version__
from duscan.humanize import format_bytes
from duscan.report import summary
from duscan.scanner import scan
from duscan.tree import biggest_directories, render_tree


def non_negative(text):
    try:
        value = int(text)
    except ValueError:
        raise argparse.ArgumentTypeError(f"not a number: {text!r}") from None
    if value < 0:
        raise argparse.ArgumentTypeError(f"must be 0 or more: {value}")
    return value


def build_parser():
    parser = argparse.ArgumentParser(prog="duscan", description="Report where the bytes are under a directory.")
    parser.add_argument("--version", action="version", version=f"duscan {__version__}")
    parser.add_argument("path", help="the directory to scan")
    parser.add_argument("--top", type=non_negative, default=5, metavar="N", help="list the N largest files (default 5)")
    parser.add_argument("--tree", action="store_true", help="print the tree instead of the summary")
    parser.add_argument("--depth", type=non_negative, metavar="N", help="with --tree, go N levels down at most")
    parser.add_argument("--dirs-only", action="store_true", help="with --tree, leave files out")
    parser.add_argument("--precision", type=non_negative, default=1, metavar="P", help="decimals in the tree (default 1)")
    parser.add_argument("--no-hidden", action="store_true", help="skip names that start with a dot")
    parser.add_argument("--timing", action="store_true", help="end the summary with the scan's throughput")
    return parser


def main(argv=None, clock=time.perf_counter):
    args = build_parser().parse_args(argv)
    started = clock()
    try:
        entries = scan(args.path, include_hidden=not args.no_hidden)
    except OSError as error:
        print(f"duscan: {error}", file=sys.stderr)
        return 1
    seconds = clock() - started
    if args.tree:
        sys.stdout.write(render_tree(entries, max_depth=args.depth, precision=args.precision, files=not args.dirs_only))
        return 0
    sys.stdout.write(summary(entries, top=args.top, seconds=seconds if args.timing else None))
    dirs = biggest_directories(entries)
    if dirs:
        print()
        print("biggest directories:")
        for path, size in dirs:
            print(f"  {format_bytes(size):>10}  {path}/")
    return 0
