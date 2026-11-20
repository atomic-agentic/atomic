"""The command line: replay, summary and states."""

import sys

from . import logfile, replay, report

USAGE = """usage: python3 -m orders replay LOG...
       python3 -m orders summary LOG...
       python3 -m orders states

Exit codes: 0 when every event is a valid transition, 1 when one is not, 2 for bad usage or a log
that cannot be read."""


def main(argv, out=None, err=None):
    out = out or sys.stdout
    err = err or sys.stderr
    if argv and argv[0] in ("-h", "--help"):
        print(USAGE, file=out)
        return 0
    if not argv or argv[0] not in ("replay", "summary", "states"):
        print(USAGE, file=err)
        return 2
    command, paths = argv[0], argv[1:]
    if command == "states":
        if paths:
            print(USAGE, file=err)
            return 2
        out.write(report.states_text())
        return 0
    if not paths:
        print(f"{command} needs at least one LOG\n\n{USAGE}", file=err)
        return 2
    try:
        events = logfile.load(paths)
    except ValueError as e:
        print(f"orders: {e}", file=err)
        return 2
    result = replay.replay(events)
    if command == "replay":
        out.write(report.replay_text(result))
    else:
        out.write(report.summary_text(result))
    return 0 if result.ok else 1
