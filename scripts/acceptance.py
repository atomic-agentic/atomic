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
the test's full path inside it.

A clause made of several parts, separated by semicolons, can list each part
as a `### ` line, in the clause's own words, with the tests that show that
part under it: then every part must be listed, and each needs a test of its
own. A clause the listed tests do not yet show in full says so with one
`pending: <reason>` line: its tests still run and must pass, and the verdict
names it as pending rather than passed outright.

Every listed test is run in one `cargo test` over the workspace, with CI's
test features (FEATURES, which must match CI's test step), so nothing
rebuilds after that step.

The check fails when any listed test fails, is ignored, or does not run at all
(renamed, removed, or built out by a feature), when the same name runs only in
another binary, when a clause or part lists no test, when the clauses differ
from the milestone's, or when cargo itself fails. A suite that cannot reach its
backend fails its tests, so the check fails with them; it never passes with a
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
HEADING = re.compile(r"^(#+)\s")


def plain(text):
    """`text` without colour codes, a byte-order mark, or the times a
    downloaded job log puts before its lines."""
    return STAMP.sub("", ANSI.sub("", text.lstrip("\ufeff")))


def words(text):
    """`text` with its whitespace made single spaces, for comparing clauses."""
    return " ".join(text.split())


class Clause:
    """One clause of the list: its text, its tests, its parts (each a text
    and its tests), and why it is pending, if it is."""

    def __init__(self, text):
        self.text = text
        self.tests = []
        self.parts = []
        self.pending = None

    def all_tests(self):
        return self.tests + [t for _, tests in self.parts for t in tests]


def segments(clause):
    """A clause's semicolon-separated parts, as a `### ` line states each."""
    return [words(s).rstrip(".") for s in clause.split(";")]


def manifest(path):
    """The clauses in `path`, in order, and what is wrong with the list."""
    clauses, problems = [], []
    with open(path, encoding="utf-8") as f:
        for n, line in enumerate(f, 1):
            line = line.strip()
            where = f"{path}:{n}"
            # A comment is one `#`; a clause `##`, and a part `###`.
            if not line or line == "#" or line.startswith("# "):
                continue
            if line.startswith("## "):
                text = line[3:].strip()
                if any(words(c.text) == words(text) for c in clauses):
                    problems.append(f"{where}: the clause is listed twice: {text}")
                clauses.append(Clause(text))
            elif line.startswith("### "):
                if not clauses:
                    problems.append(f"{where}: a part before any clause")
                    continue
                clauses[-1].parts.append((line[4:].strip(), []))
            elif HEADING.match(line):
                problems.append(f"{where}: only `## ` clauses and `### ` parts: {line}")
            elif not clauses:
                problems.append(f"{where}: a line before any clause")
            elif line.startswith("pending:"):
                reason = line[len("pending:"):].strip()
                if not reason:
                    problems.append(f"{where}: `pending:` needs a reason")
                if clauses[-1].pending is not None:
                    problems.append(f"{where}: the clause has a `pending:` line already")
                clauses[-1].pending = reason
            else:
                parts = line.split()
                if len(parts) != 2:
                    problems.append(f"{where}: expected `<binary> <test path>`: {line}")
                elif clauses[-1].parts:
                    clauses[-1].parts[-1][1].append((parts[0], parts[1]))
                else:
                    clauses[-1].tests.append((parts[0], parts[1]))
    if not clauses:
        problems.append(f"{path}: no clauses")
    for c in clauses:
        if not c.parts:
            continue
        stated = segments(c.text)
        listed = [words(p).rstrip(".") for p, _ in c.parts]
        problems += [f"{path}: part of `{c.text}` not listed: {s}" for s in stated if s not in listed]
        problems += [f"{path}: not a part of `{c.text}`: {p}" for p in listed if p not in stated]
        if c.tests:
            problems.append(f"{path}: `{c.text}` lists tests outside its parts")
    return clauses, problems


def milestone_clauses(milestone, path=MILESTONES):
    """The acceptance clauses `path` states for `milestone`, and what in its
    Acceptance block could not be read as one."""
    with open(path, encoding="utf-8") as f:
        lines = f.read().splitlines()
    name = re.compile(rf"^(#+)\s+{re.escape(milestone)}\b", re.IGNORECASE)
    start = next((i for i, l in enumerate(lines) if name.match(l)), None)
    if start is None:
        return None, [f"{path} has no section for {milestone}"]
    level = len(name.match(lines[start]).group(1))
    clauses, problems, inside, blocks = [], [], False, 0
    for n, line in enumerate(lines[start + 1:], start + 2):
        heading = HEADING.match(line)
        if heading and len(heading.group(1)) <= level:
            break
        if line.strip() == "**Acceptance:**":
            blocks += 1
            inside = True
            continue
        if not inside or not line.strip():
            continue
        if heading:
            inside = False
        elif line.startswith("- "):
            clauses.append(line[2:])
        elif line.startswith("  ") and clauses and not line.lstrip().startswith(("-", "*")):
            clauses[-1] += " " + line.strip()
        else:
            problems.append(f"{path}:{n}: not a clause or its continuation: {line.strip()}")
    if blocks != 1:
        problems.append(f"{path}: {milestone} has {blocks} Acceptance blocks, not one")
    return [words(c) for c in clauses], problems


def results(log):
    """{(binary, test): status} for each test the log shows run. A test
    seen to fail or be ignored keeps that status, whatever a later line
    says."""
    seen, binary = {}, None
    for line in plain(log).splitlines():
        m = RUNNING.match(line)
        if m:
            binary = m.group(1)
            continue
        m = RESULT.match(line)
        if m and binary is not None:
            key = (binary, m.group(1))
            if seen.get(key, "ok") == "ok":
                seen[key] = m.group(2)
    return seen


def cargo_errors(log):
    """The lines in which cargo itself reports failing."""
    return [l for l in plain(log).splitlines() if CARGO_FAILED.match(l)]


def run(clauses):
    names = sorted({test for c in clauses for _, test in c.all_tests()})
    cmd = [
        "cargo", "test", "--workspace", "--locked", "--no-fail-fast", "--color", "never",
        "--features", FEATURES, "--tests", "--", "--exact", *names,
    ]
    with tempfile.TemporaryFile(mode="w+", encoding="utf-8", errors="replace") as out:
        code = subprocess.call(cmd, cwd=ROOT, stdout=out, stderr=subprocess.STDOUT)
        out.seek(0)
        return code, out.read()


def summarize(report):
    """Add `report` to the CI job's summary, when there is one."""
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as f:
            f.write(f"```\n{report}\n```\n")


def judge(binary, test, seen):
    """What is wrong with `test` in `binary`, as the run shows it, if anything."""
    status = seen.get((binary, test))
    if status == "ok":
        return None
    if status is None:
        elsewhere = sorted(b for (b, t) in seen if t == test)
        where = f" (ran only in {', '.join(elsewhere)})" if elsewhere else ""
        return f"{binary} {test}: not run{where}"
    return f"{binary} {test}: {status}"


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
    stated, unread = milestone_clauses(title, args.milestones)
    problems += unread
    if stated is not None and not problems:
        listed = [words(c.text) for c in clauses]
        problems += [f"{title}'s clause is not listed: {c}" for c in stated if c not in listed]
        problems += [f"listed, but not one of {title}'s clauses: {c}" for c in listed if c not in stated]
    if problems:
        report = "\n".join(problems + ["", f"{title} acceptance: FAILED: the list cannot be checked"])
        print(report, file=sys.stderr)
        summarize(report)
        return 1
    if args.check:
        print(f"{path} lists {title}'s {len(clauses)} clauses")
        return 0
    if args.log:
        with open(args.log, encoding="utf-8", errors="replace") as f:
            log = f.read()
        code = 0
        # Only a log says whether cargo failed: a real run's exit code does.
        errors = cargo_errors(log)
    else:
        code, log = run(clauses)
        errors = cargo_errors(log) if code != 0 else []
    seen = results(log)

    failed = False
    pending = []
    lines = [f"{title} acceptance", ""]
    for clause in clauses:
        bad = []
        if not clause.all_tests():
            bad.append("no test listed")
        for part, tests in clause.parts:
            if not tests:
                bad.append(f"no test listed for: {part}")
        bad += [b for b in (judge(binary, test, seen) for binary, test in clause.all_tests()) if b]
        failed |= bool(bad)
        n = len(clause.all_tests())
        count = f"{n} test{'' if n == 1 else 's'}"
        mark = "FAIL" if bad else ("pending" if clause.pending else "ok")
        lines.append(f"{mark:<7} {clause.text} ({count})")
        if clause.pending and not bad:
            pending.append(clause.pending)
            lines.append(f"        pending: {clause.pending}")
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
    summarize(report)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
