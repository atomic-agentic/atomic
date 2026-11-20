"""What every stage script shares: its arguments, where it reads and writes, its logging and its exit status."""

import os
import sys

from pipeline import logsetup

USAGE = "usage: python3 {script} DATA_DIR OUT_DIR"


def data_file(data_dir, name):
    return os.path.join(data_dir, name)


def staged_file(out_dir, name):
    return os.path.join(out_dir, "staged", name)


def totals_file(out_dir):
    return os.path.join(out_dir, "totals.csv")


def report_file(out_dir):
    return os.path.join(out_dir, "report.txt")


def run_stage(name, work, argv=None):
    """Run work(data_dir, out_dir, log) as the stage `name`, and return the exit status.

    0 when nothing logged an error, 1 when something did or work raised, 2 for bad arguments.
    """
    argv = sys.argv[1:] if argv is None else argv
    if len(argv) != 2:
        print(USAGE.format(script=f"{name}.py"), file=sys.stderr)
        return 2
    data_dir, out_dir = argv
    counter = logsetup.configure(name, verbose=bool(os.environ.get("PIPELINE_VERBOSE")))
    log = logsetup.stage_logger(name)
    try:
        work(data_dir, out_dir, log)
    except Exception:
        log.exception("%s stopped", name)
    if counter.count:
        log.info("%s finished with %d error(s)", name, counter.count)
        return 1
    log.info("%s finished", name)
    return 0
