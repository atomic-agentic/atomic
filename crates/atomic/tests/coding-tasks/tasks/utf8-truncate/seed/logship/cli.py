"""python3 -m logship [--config FILE] [--output FILE] RECORDS.jsonl"""

import argparse
import sys

from logship.config import ConfigError, load
from logship.formatter import Formatter
from logship.record import RecordError, read_jsonl
from logship.sender import BatchSender


def build_parser():
    parser = argparse.ArgumentParser(prog="logship", description="Format log records and check them for the collector.")
    parser.add_argument("--config", help="a key = value configuration file")
    parser.add_argument("--output", help="where to write the lines (overrides the configuration)")
    parser.add_argument("records", help="a file of JSON lines, one record each")
    return parser


def main(argv):
    args = build_parser().parse_args(argv)
    try:
        config = load(args.config, {"output": args.output})
    except (OSError, ConfigError) as error:
        print(f"logship: {error}", file=sys.stderr)
        return 2
    try:
        with open(args.records, encoding="utf-8") as f:
            records = list(read_jsonl(f))
    except (OSError, RecordError) as error:
        print(f"logship: {error}", file=sys.stderr)
        return 2
    report = BatchSender(config.output, Formatter(config)).send(records)
    for number, reason in report.rejected:
        print(f"rejected line {number}: {reason}", file=sys.stderr)
    print(f"sent {report.sent} lines, collector rejected {len(report.rejected)}")
    return 1 if report.rejected else 0
