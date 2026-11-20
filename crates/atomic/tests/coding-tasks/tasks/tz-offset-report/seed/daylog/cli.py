"""Command line: report, sites and check."""

import argparse
import sys

from . import __version__
from .buckets import daily_totals
from .events import read_events
from .offsets import format_offset
from .report import render
from .sites import SiteError, adhoc_site, load_sites


def build_parser():
    parser = argparse.ArgumentParser(prog="daylog", description="Daily sales reports by local day.")
    parser.add_argument("--version", action="version", version=f"daylog {__version__}")
    parser.add_argument("--sites", default="sites.json", help="sites file (default: sites.json)")
    commands = parser.add_subparsers(dest="command", required=True)

    report = commands.add_parser("report", help="daily totals for one log")
    where = report.add_mutually_exclusive_group(required=True)
    where.add_argument("--site", help="a site in the sites file")
    where.add_argument("--offset", help="a UTC offset, +HH:MM or -HH:MM")
    report.add_argument("log")

    commands.add_parser("sites", help="list the sites")

    check = commands.add_parser("check", help="report bad lines only")
    check.add_argument("log")
    return parser


def _site(args):
    if args.offset is not None:
        try:
            return adhoc_site(args.offset)
        except ValueError as e:
            raise SiteError(str(e)) from None
    sites = load_sites(args.sites)
    if args.site not in sites:
        raise SiteError(f"no site {args.site} in {args.sites}")
    return sites[args.site]


def _read(path):
    try:
        events, problems = read_events(path)
    except OSError as e:
        raise SiteError(str(e)) from None
    for problem in problems:
        print(problem, file=sys.stderr)
    return events, problems


def main(argv=None):
    args = build_parser().parse_args(argv)
    try:
        if args.command == "sites":
            for key, site in sorted(load_sites(args.sites).items()):
                print(f"{key:<10} UTC{format_offset(site.offset)}  {site.name}")
            return 0
        if args.command == "check":
            _, problems = _read(args.log)
            return 1 if problems else 0
        site = _site(args)
        events, problems = _read(args.log)
        sys.stdout.write(render(site, daily_totals(events, site.offset)))
        return 1 if problems else 0
    except SiteError as e:
        print(f"daylog: {e}", file=sys.stderr)
        return 2
