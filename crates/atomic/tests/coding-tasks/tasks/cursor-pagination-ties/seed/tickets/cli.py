"""Command line: count, show and export."""

import argparse
import functools
import sys

from . import __version__
from .api import DEFAULT_PAGE_SIZE, list_tickets
from .client import PagingError, iter_tickets
from .export import FORMATS, write_tickets
from .model import STATUSES, TicketError
from .store import load_store


def build_parser():
    parser = argparse.ArgumentParser(prog="tickets", description="A help-desk ticket store.")
    parser.add_argument("--version", action="version", version=f"tickets {__version__}")
    commands = parser.add_subparsers(dest="command", required=True)

    count = commands.add_parser("count", help="how many tickets, by status")
    count.add_argument("file")

    show = commands.add_parser("show", help="one ticket")
    show.add_argument("file")
    show.add_argument("id", type=int)

    export = commands.add_parser("export", help="every ticket, through the listing API")
    export.add_argument("file")
    export.add_argument("--format", choices=FORMATS, default="csv")
    export.add_argument("--status", choices=STATUSES)
    export.add_argument("--page-size", type=int, default=DEFAULT_PAGE_SIZE)
    return parser


def main(argv=None):
    args = build_parser().parse_args(argv)
    try:
        store = load_store(args.file)
    except (OSError, TicketError) as e:
        print(f"tickets: {e}", file=sys.stderr)
        return 1
    if args.command == "count":
        counts = store.counts()
        print(f"total\t{len(store)}")
        for status in STATUSES:
            print(f"{status}\t{counts.get(status, 0)}")
        return 0
    if args.command == "show":
        try:
            ticket = store.get(args.id)
        except KeyError:
            print(f"tickets: no ticket {args.id}", file=sys.stderr)
            return 1
        for name, value in ticket.to_dict().items():
            print(f"{name}\t{value}")
        return 0
    fetch = functools.partial(list_tickets, store)
    filters = {"status": args.status} if args.status else {}
    try:
        written = write_tickets(iter_tickets(fetch, args.page_size, **filters), sys.stdout, args.format)
    except (ValueError, PagingError) as e:
        print(f"tickets: {e}", file=sys.stderr)
        return 1
    print(f"exported {written} tickets", file=sys.stderr)
    return 0
