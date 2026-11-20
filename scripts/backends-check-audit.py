#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
#
# Read the output of `cargo test` run without a backend, for
# scripts/backends-check.sh, and say whether it shows what the check needs.
#
#   backends-check-audit.py all-fail LOG MESSAGE
#       every test that was selected failed, each with MESSAGE in its own
#       output: none passed and none was ignored
#   backends-check-audit.py discover LOG MESSAGE FAILING
#       the tests that failed, each with MESSAGE in its own output, are
#       exactly the Postgres variants (their names say postgres): none of
#       those passed or was ignored. Their names are written to FAILING.
#
# A child-process entry point, ignored with a reason that says it runs "in
# a child process", is not a test of the suite and counts as neither.
#
# Either way, every test binary must have reported its result (one that
# aborted or was killed has not) and exited normally or with test failures,
# and each binary's own count of passed, failed and ignored tests must match
# the tests it listed. Tests are told apart by binary and name, so two
# binaries' tests of one name are two tests. Colour escapes, as cargo writes
# them under CARGO_TERM_COLOR=always, are ignored. Prints what it found;
# exits 1 if the run does not show it.
import re
import sys

ESCAPE = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
STARTS = re.compile(r"^\s+(Running|Doc-tests) (.+)$")
OUTCOME = re.compile(r"^test (.+) \.\.\. (ok|FAILED|ignored)\b(.*)$")
# What a child-process entry point's #[ignore] reason says: such a test is
# not one of the suite's tests but a way for one to start a process, and it
# is ignored by design.
CHILD = "in a child process"
RESULT = re.compile(
    r"^test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured;"
)
COUNTED = {"ok": 0, "FAILED": 1, "ignored": 2, "child": 2}


def short(started):
    """A test binary's name: the file in its `(target/.../<file>)`, which
    tells two `unittests src/lib.rs` apart, or a doc test's crate."""
    m = re.search(r"\(([^()]*)\)\s*$", started)
    return m.group(1).rsplit("/", 1)[-1] if m else started.strip()


def audit(mode, log, message):
    """The problems the log shows, and (failed, passed) keys."""
    lines = [ESCAPE.sub("", l) for l in log.splitlines()]
    problems = []
    binary = None
    started = []
    # (binary, test) -> outcome, in the order the binaries ran.
    outcome = {}
    # Each failing test's own output, between its header and the next one.
    blocks = {}
    block = None
    reported = {}
    for l in lines:
        m = STARTS.match(l)
        if m:
            binary = short(m.group(2))
            started.append(binary)
            block = None
            continue
        m = OUTCOME.match(l)
        if m:
            seen = m.group(2)
            if seen == "ignored" and CHILD in m.group(3):
                seen = "child"
            outcome[(binary, m.group(1))] = seen
            continue
        m = re.match(r"^---- (.+) stdout ----$", l)
        if m:
            block = (binary, m.group(1))
            blocks[block] = []
            continue
        m = RESULT.match(l)
        if m:
            reported[binary] = tuple(int(n) for n in m.groups()[:3])
            block = None
            continue
        if l == "failures:":
            block = None
        elif block is not None:
            blocks[block].append(l)
        if "process didn't exit successfully" in l and "(exit status: 101)" not in l:
            problems.append(f"a test binary did not end normally: {l.strip()}")

    if not started:
        problems.append("no test binary ran (did it build?)")
    for b in started:
        if b not in reported:
            problems.append(f"{b} ran and reported no result: it ended early")
            continue
        listed = [0, 0, 0]
        for (ob, _), o in outcome.items():
            if ob == b:
                listed[COUNTED[o]] += 1
        if tuple(listed) != reported[b]:
            problems.append(
                f"{b} reported {reported[b][0]} passed, {reported[b][1]} failed and "
                f"{reported[b][2]} ignored, and listed {listed[0]}, {listed[1]} and {listed[2]}"
            )

    def shown(keys):
        return ", ".join(f"{t} ({b})" for b, t in sorted(keys))

    failed = sorted(k for k, o in outcome.items() if o == "FAILED")
    passed = sorted(k for k, o in outcome.items() if o == "ok")
    unnamed = [k for k in failed if message not in "\n".join(blocks.get(k, []))]
    if unnamed:
        problems.append(f"these failed without saying '{message}': " + shown(unnamed))
    if not failed:
        problems.append("no test failed")

    if mode == "all-fail":
        others = [k for k, o in outcome.items() if o not in ("FAILED", "child")]
        if others:
            problems.append(
                "these did not fail without the backend: "
                + ", ".join(f"{t} ({b}: {outcome[(b, t)]})" for b, t in sorted(others))
            )
    elif mode == "discover":
        misnamed = [k for k in failed if "postgres" not in k[1]]
        if misnamed:
            problems.append(
                "these failed without the database, and their names do not say postgres: "
                + shown(misnamed)
            )
        unrun = [k for k, o in outcome.items() if "postgres" in k[1] and o != "FAILED"]
        if unrun:
            problems.append(
                "these say postgres, and passed or were ignored without the database: "
                + shown(unrun)
            )
    else:
        raise SystemExit(f"unknown mode {mode}")
    return problems, failed, passed


def main():
    mode, log_path, message = sys.argv[1:4]
    log = open(log_path, encoding="utf-8", errors="replace").read()
    problems, failed, passed = audit(mode, log, message)
    if mode == "discover":
        with open(sys.argv[4], "w") as out:
            out.write("".join(t + "\n" for _, t in failed))
    for p in problems:
        print(f"  {p}")
    if problems:
        sys.exit(1)
    print(f"  {len(failed)} tests failed, each saying '{message}'; {len(passed)} passed")


if __name__ == "__main__":
    main()
