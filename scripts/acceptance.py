#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Run a milestone's acceptance tests as one check.

    scripts/acceptance.py m12                 # run the tests scripts/acceptance/m12.txt lists
    scripts/acceptance.py m12 --log FILE      # judge a log of that run instead of running it
    scripts/acceptance.py m12 --check         # check only that the list states the milestone's clauses

The list names each acceptance clause of the milestone, as a `## ` line
holding the clause exactly as docs/design/milestones.md states it, and under
it the tests that show it, one per line as `<binary> <test path>`: the test
binary as cargo names it (`budgets` for crates/atomic/tests/budgets.rs,
`atomic_core` for atomic-agentic-core's unit tests, by its library name) and
the test's full path inside it. A clause the listed tests do not yet show in
full says so with a `pending: <reason>` line: its tests still run and must
pass, and the verdict names it as pending rather than passed outright.

Every listed test is run in one `cargo test` over the workspace, with CI's
test features (FEATURES, which must match CI's test step), so nothing
rebuilds after that step.

The check fails when any listed test fails, is ignored, or does not run at all
(renamed, removed, or built out by a feature), when the same name runs only in
another binary, when a clause lists no test, when the clauses differ from the
milestone's, or when cargo itself fails. A suite that cannot reach its backend
fails its tests, so the check fails with them; it never passes with a
backend's tests unrun.
"""

import argparse
import os
import re
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FEATURES = "atomic-agentic/postgres-tests,atomic-agentic/sandbox-tests"
MILESTONES = os.path.join(ROOT, "docs", "design", "milestones.md")
# Colour codes, which cargo writes when told to (CI sets CARGO_TERM_COLOR).
ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
# The time GitHub puts before each line of a job's log, as downloaded.
STAMP = re.compile(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z ", re.MULTILINE)
RUNNING = re.compile(r"^\s*Running .*[/\\]deps[/\\]([A-Za-z0-9_]+)-[0-9a-f]+(?:\.exe)?\)\s*$")
RESULT = re.compile(r"^test (\S+) \.\.\. (ok|FAILED|ignored)(?:,.*)?$")
# What cargo prints when it, rather than a listed test, failed.
CARGO_FAILED = re.compile(r"^error(\[E\d+\])?: ")


def plain(text):
    """`text` without colour codes, or the times a downloaded job log puts
    before its lines."""
    return STAMP.sub("", ANSI.sub("", text))


def words(text):
    """`text` with its whitespace made single spaces, for comparing clauses."""
    return " ".join(text.split())


def manifest(path):
    """The clauses in `path`, in order: (clause, tests, pending reason or None)."""
    clauses, problems = [], []
    with open(path, encoding="utf-8") as f:
        for n, line in enumerate(f, 1):
            line = line.strip()
            if not line or (line.startswith("#") and not line.startswith("## ")):
                continue
            if line.startswith("## "):
                clauses.append([line[3:].strip(), [], None])
            elif not clauses:
                problems.append(f"{path}:{n}: a line before any clause")
            elif line.startswith("pending:"):
                reason = line[len("pending:"):].strip()
                if not reason:
                    problems.append(f"{path}:{n}: `pending:` needs a reason")
                clauses[-1][2] = reason
            else:
                parts = line.split()
                if len(parts) != 2:
                    problems.append(f"{path}:{n}: expected `<binary> <test path>`: {line}")
                else:
                    clauses[-1][1].append((parts[0], parts[1]))
    if not clauses:
        problems.append(f"{path}: no clauses")
    return [tuple(c) for c in clauses], problems


def milestone_clauses(milestone, path=MILESTONES):
    """The acceptance clauses `path` states for `milestone`, or None if it
    has no such section."""
    with open(path, encoding="utf-8") as f:
        lines = f.read().splitlines()
    head = re.compile(rf"^##\s+{re.escape(milestone)}\b", re.IGNORECASE)
    start = next((i for i, l in enumerate(lines) if head.match(l)), None)
    if start is None:
        return None
    clauses, inside = [], False
    for line in lines[start + 1:]:
        if line.startswith("#"):
            break
        if line.strip() == "**Acceptance:**":
            inside = True
            continue
        if not inside:
            continue
        if line.startswith("- "):
            clauses.append(line[2:])
        elif line.startswith("  ") and clauses:
            clauses[-1] += " " + line.strip()
        elif line.strip() and clauses:
            break
    return [words(c) for c in clauses]


def results(log):
    """{(binary, test): status} for each test the log shows run."""
    seen, binary = {}, None
    for line in plain(log).splitlines():
        m = RUNNING.match(line)
        if m:
            binary = m.group(1)
            continue
        m = RESULT.match(line)
        if m and binary is not None:
            seen[(binary, m.group(1))] = m.group(2)
    return seen


def cargo_errors(log):
    """The lines in which cargo itself reports failing."""
    return [l for l in plain(log).splitlines() if CARGO_FAILED.match(l)]


def run(clauses):
    names = sorted({test for _, tests, _ in clauses for _, test in tests})
    cmd = [
        "cargo", "test", "--workspace", "--locked", "--no-fail-fast", "--color", "never",
        "--features", FEATURES, "--tests", "--", "--exact", *names,
    ]
    with tempfile.TemporaryFile(mode="w+", encoding="utf-8", errors="replace") as out:
        code = subprocess.call(cmd, cwd=ROOT, stdout=out, stderr=subprocess.STDOUT)
        out.seek(0)
        return code, out.read()


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("milestone")
    parser.add_argument("--log", help="judge this log of the run instead of running it")
    parser.add_argument("--list", help="the list to read instead of scripts/acceptance/<milestone>.txt")
    parser.add_argument("--milestones", default=MILESTONES, help="the milestones document")
    parser.add_argument("--check", action="store_true", help="check the list against the milestone and stop")
    args = parser.parse_args()
    title = args.milestone.upper()
    path = args.list or os.path.join(ROOT, "scripts", "acceptance", f"{args.milestone.lower()}.txt")
    clauses, problems = manifest(path)
    stated = milestone_clauses(title, args.milestones)
    if stated is None:
        problems.append(f"{args.milestones} has no section for {title}")
    elif not problems:
        listed = [words(c) for c, _, _ in clauses]
        problems += [f"{title}'s clause is not listed: {c}" for c in stated if c not in listed]
        problems += [f"listed, but not one of {title}'s clauses: {c}" for c in listed if c not in stated]
    if problems:
        print("\n".join(problems), file=sys.stderr)
        return 1
    if args.check:
        print(f"{path} lists {title}'s {len(clauses)} clauses")
        return 0
    if args.log:
        with open(args.log, encoding="utf-8", errors="replace") as f:
            log = f.read()
        code = 0
    else:
        code, log = run(clauses)
    seen = results(log)
    errors = cargo_errors(log)

    failed = False
    pending = []
    lines = [f"{title} acceptance", ""]
    for clause, tests, reason in clauses:
        bad = []
        if not tests:
            bad.append("no test listed")
        for binary, test in tests:
            status = seen.get((binary, test))
            if status == "ok":
                continue
            if status is None:
                elsewhere = sorted(b for (b, t) in seen if t == test)
                where = f" (ran only in {', '.join(elsewhere)})" if elsewhere else ""
                bad.append(f"{binary} {test}: not run{where}")
            else:
                bad.append(f"{binary} {test}: {status}")
        failed |= bool(bad)
        count = f"{len(tests)} test{'' if len(tests) == 1 else 's'}"
        mark = "FAIL" if bad else ("pending" if reason else "ok")
        lines.append(f"{mark:<7} {clause} ({count})")
        if reason and not bad:
            pending.append(reason)
            lines.append(f"        pending: {reason}")
        lines.extend(f"        {b}" for b in bad)
    if code != 0 or errors:
        failed = True
        lines.append("")
        lines.append("cargo test failed" + (f", exiting {code}" if code else "") + (":" if errors else ""))
        lines.extend(f"  {l}" for l in errors[-5:])
    lines.append("")
    if failed:
        verdict = "FAILED"
    elif pending:
        n = len(pending)
        verdict = f"passed, {n} clause{'' if n == 1 else 's'} pending: " + "; ".join(pending)
    else:
        verdict = "passed"
    lines.append(f"{title} acceptance: {verdict}")
    report = "\n".join(lines)
    if failed:
        # The whole run, folded in CI, for what each failure printed.
        print("::group::cargo test output")
        print(plain(log).rstrip())
        print("::endgroup::")
    print(report)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as f:
            f.write(f"```\n{report}\n```\n")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
