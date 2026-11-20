#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Run a milestone's acceptance tests as one check.

    scripts/acceptance.py m12                 # run the tests scripts/acceptance/m12.txt lists
    scripts/acceptance.py m12 --log FILE      # judge a log of that run instead of running it

The list names each acceptance clause of the milestone, as a `## ` line, and
under it the tests that show it, one per line as `<binary> <test path>`: the
test binary as cargo names it (`budgets` for crates/atomic/tests/budgets.rs,
`atomic_core` for atomic-agentic-core's unit tests, by its library name) and
the test's full path inside it. Every listed test is run in one `cargo test` over the workspace,
with CI's test features, so nothing rebuilds after CI's test step.

The check fails when any listed test fails, is ignored, or does not run at all
(renamed, removed, or built out by a feature), when the same name runs only in
another binary, when a clause lists no test, or when cargo itself fails. A
suite that cannot reach its backend fails its tests, so the check fails with
them; it never passes with a backend's tests unrun.
"""

import argparse
import os
import re
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FEATURES = "atomic-agentic/postgres-tests,atomic-agentic/sandbox-tests"
RUNNING = re.compile(r"^\s+Running .*[/\\]deps[/\\]([A-Za-z0-9_]+)-[0-9a-f]+(?:\.exe)?\)\s*$")
RESULT = re.compile(r"^test (\S+) \.\.\. (ok|FAILED|ignored)(?:,.*)?$")


def manifest(path):
    """The clauses in `path`, in order, each with its (binary, test) pairs."""
    clauses, problems = [], []
    with open(path, encoding="utf-8") as f:
        for n, line in enumerate(f, 1):
            line = line.strip()
            if not line or line.startswith("#") and not line.startswith("## "):
                continue
            if line.startswith("## "):
                clauses.append((line[3:].strip(), []))
            elif not clauses:
                problems.append(f"{path}:{n}: a test before any clause")
            else:
                parts = line.split()
                if len(parts) != 2:
                    problems.append(f"{path}:{n}: expected `<binary> <test path>`: {line}")
                else:
                    clauses[-1][1].append((parts[0], parts[1]))
    if not clauses:
        problems.append(f"{path}: no clauses")
    return clauses, problems


def results(log):
    """{(binary, test): status} for each test the log shows run."""
    seen, binary = {}, None
    for line in log.splitlines():
        m = RUNNING.match(line)
        if m:
            binary = m.group(1)
            continue
        m = RESULT.match(line)
        if m and binary is not None:
            seen[(binary, m.group(1))] = m.group(2)
    return seen


def run(clauses):
    names = sorted({test for _, tests in clauses for _, test in tests})
    cmd = [
        "cargo", "test", "--workspace", "--locked", "--no-fail-fast",
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
    args = parser.parse_args()
    path = args.list or os.path.join(ROOT, "scripts", "acceptance", f"{args.milestone.lower()}.txt")
    clauses, problems = manifest(path)
    if problems:
        print("\n".join(problems), file=sys.stderr)
        return 1
    if args.log:
        code = 0
        with open(args.log, encoding="utf-8", errors="replace") as f:
            log = f.read()
    else:
        code, log = run(clauses)
    seen = results(log)

    failed = False
    title = args.milestone.upper()
    lines = [f"{title} acceptance", ""]
    for clause, tests in clauses:
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
        lines.append(f"{'FAIL' if bad else 'ok  '} {clause} ({len(tests)} test{'' if len(tests) == 1 else 's'})")
        lines.extend(f"       {b}" for b in bad)
    if code != 0:
        failed = True
        tail = [l for l in log.splitlines() if l.startswith("error")][-5:]
        lines.append("")
        lines.append(f"cargo test exited {code}" + (":" if tail else ""))
        lines.extend(f"  {l}" for l in tail)
    lines.append("")
    lines.append(f"{title} acceptance: {'FAILED' if failed else 'passed'}")
    report = "\n".join(lines)
    print(report)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as f:
            f.write(f"```\n{report}\n```\n")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
