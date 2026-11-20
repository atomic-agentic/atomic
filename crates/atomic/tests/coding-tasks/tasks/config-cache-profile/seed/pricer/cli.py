"""Command line: quote, batch and profiles."""

import argparse
import os
import sys

from . import __version__
from .catalog import CatalogError, load_catalog
from .quote import Job, JobError, format_quote, parse_job, price_job
from .settings import SettingsError, load_settings, profile_names


def build_parser():
    parser = argparse.ArgumentParser(prog="pricer", description="Regional quotes from catalog prices.")
    parser.add_argument("--version", action="version", version=f"pricer {__version__}")
    parser.add_argument("--config", default=os.environ.get("PRICER_CONFIG_DIR", "config"),
                        help="configuration directory (default: $PRICER_CONFIG_DIR, else config)")
    parser.add_argument("--catalog", default=os.path.join("data", "catalog.csv"), help="catalog CSV file")
    commands = parser.add_subparsers(dest="command", required=True)

    quote = commands.add_parser("quote", help="price one job")
    quote.add_argument("--profile", required=True)
    quote.add_argument("sku")
    quote.add_argument("quantity", type=int)

    batch = commands.add_parser("batch", help="price every job in a file")
    batch.add_argument("jobs", help="file of PROFILE SKU QUANTITY lines")

    commands.add_parser("profiles", help="list the profiles")
    return parser


def _quote(args, catalog):
    if args.quantity <= 0:
        print("pricer: quantity must be positive", file=sys.stderr)
        return 2
    job = Job(profile=args.profile.lower(), sku=args.sku.upper(), quantity=args.quantity)
    print(format_quote(price_job(job, catalog, args.config)))
    return 0


def _batch(args, catalog):
    try:
        with open(args.jobs, encoding="utf-8") as f:
            lines = f.readlines()
    except OSError as e:
        print(f"pricer: {e}", file=sys.stderr)
        return 2
    status = 0
    for number, line in enumerate(lines, start=1):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        try:
            print(format_quote(price_job(parse_job(line), catalog, args.config)))
        except (JobError, CatalogError, SettingsError) as e:
            print(f"line {number}: {e}", file=sys.stderr)
            status = 1
    return status


def _profiles(args):
    for name in profile_names(args.config):
        rules = load_settings(args.config, profile=name)
        print(f"{name}\t{rules.label}\t{rules.currency}")
    return 0


def main(argv=None):
    args = build_parser().parse_args(argv)
    try:
        if args.command == "profiles":
            return _profiles(args)
        catalog = load_catalog(args.catalog)
        if args.command == "quote":
            return _quote(args, catalog)
        return _batch(args, catalog)
    except (CatalogError, SettingsError) as e:
        print(f"pricer: {e}", file=sys.stderr)
        return 1
