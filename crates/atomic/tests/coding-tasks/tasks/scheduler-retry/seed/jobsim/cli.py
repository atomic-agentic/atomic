"""The command line: simulate, check and backoff."""

import sys

from . import runner, scenario

USAGE = """usage: python3 -m jobsim simulate SCENARIO
       python3 -m jobsim check SCENARIO...
       python3 -m jobsim backoff SCENARIO

simulate   print the trace of a scenario
check      check scenarios, printing "SCENARIO: ok" for each good one
backoff    print the waits after attempts 1, 2, ... of the scenario's retry policy

Exit codes: 0 when every job succeeded (or every scenario is good), 1 when a job gave up,
2 for bad usage or a scenario that cannot be read."""


def main(argv, out=None, err=None):
    out = out or sys.stdout
    err = err or sys.stderr
    if argv and argv[0] in ("-h", "--help"):
        print(USAGE, file=out)
        return 0
    if len(argv) < 2 or argv[0] not in ("simulate", "check", "backoff"):
        print(USAGE, file=err)
        return 2
    command, paths = argv[0], argv[1:]
    if command != "check" and len(paths) != 1:
        print(f"{command} takes one SCENARIO\n\n{USAGE}", file=err)
        return 2
    loaded = []
    for path in paths:
        try:
            loaded.append((path, scenario.load(path)))
        except ValueError as e:
            print(f"jobsim: {path}: {e}", file=err)
            return 2
    if command == "check":
        for path, _ in loaded:
            print(f"{path}: ok", file=out)
        return 0
    path, chosen = loaded[0]
    if command == "backoff":
        longest = max(job.max_attempts for job in chosen.jobs)
        waits = chosen.retry.schedule(max(longest - 1, 1))
        print(" ".join(f"{w}s" for w in waits), file=out)
        return 0
    trace = runner.Simulator(chosen).run()
    out.write(trace.text())
    return 1 if trace.gave_up else 0
