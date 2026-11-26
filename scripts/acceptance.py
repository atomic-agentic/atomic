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

A clause made of several sub-clauses, separated by semicolons, can list each
sub-clause as a `### ` line, in the clause's own words, with the tests that
show that sub-clause under it: then every sub-clause must be listed, and each
needs a test of its own. ("Part" here means only a milestone's `a` and `b`
sections.) A clause the listed tests do not yet show in full says so with one
`pending: <reason>` line: its tests still run and must pass, and the verdict
names it as pending rather than passed outright.

In the milestones document, an Acceptance block starts at a line that begins
`**Acceptance` and ends `:**`, with or without a title after the word, and
holds a bullet per clause: a milestone's clauses are those of all its blocks,
in order. A line that only looks like that marker is refused with its line
number, not skipped, so a block in such a shape cannot go unread: outside a
block, a line is refused as one when, past a wrapper of markdown punctuation
(`-`, `_`, `*`, `~`, `>`, `#` and spaces) that may carry one bullet's mark
(`- `, `* `, `+ `, or a number, then `.` or `)`, then a space), its first word
is a spelling of `accept` and the line ends in a colon with only that
punctuation after it. A split milestone's parts are sections under it. An "a" part states
its own acceptance; a "b" part finishes its parent's work, so its clauses are
its parent's, with any of its own blocks after them.

Every listed test is run in one `cargo test` over the workspace, with CI's
test features by default (FEATURES, shared with scripts/test-shards.py).
Milestones with a feature-gated acceptance test can supply `--features`; CI
must name those features in its acceptance step.

The check fails when any listed test fails, is ignored, or does not run at all
(renamed, removed, or built out by a feature), when the same name runs only in
another binary, when a clause or sub-clause lists no test, when the clauses
differ from the milestone's, or when cargo itself fails. A suite that cannot
reach its backend fails its tests, so the check fails with them; it never
passes with a backend's tests unrun.
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
# An Acceptance block's first line: `**Acceptance:**`, or the same with a
# title, as `**Acceptance, the managed Deno:**`.
ACCEPTANCE = re.compile(r"^\*\*Acceptance\b.*:\*\*$")
# A line that means to be that marker and is not, as one whose colon sits
# outside the bold. Refused wherever it sits, open block or not: the clauses
# under it would be skipped in silence.
NEAR_MARKER = re.compile(r"^\*\*Acceptance\b", re.IGNORECASE)
# A bullet: `- `, `* ` or `+ `, or a number, at the start of the line or
# indented. Only a `- ` at the start of a line is read as a clause.
BULLET = re.compile(r"^\s*(?:[-*+][ \t]|\d+[.)]\s)")
# The same bullet, optional and unanchored, for a pattern that matches a whole
# line: `- `, `* `, `+ `, or a number, then `.` or `)`, then a space.
BULLET_MARK = r"(?:[-*+][ \t]|\d+[.)]\s)?"
# Any line shaped like that marker: `Acceptance:`, `__Acceptance:__`,
# `**Acceptence:**`, `- **Acceptance, files and data:**`, and the same marker
# led by a bullet. Before the word it takes markdown punctuation (`-`, `_`,
# `*`, `~`, `>`, `#` and spaces), which may carry BULLET_MARK once; after the
# colon, that punctuation only. So a line whose first word is not a spelling
# of `accept` stays prose, as `1. Cap the run.`, and a wrapper sitting
# anywhere else in the line does too, as in `The **Acceptance:** list`.
# Outside a block such a line is refused too, since only a line beginning
# `**Acceptance` opens one.
MARKERISH = re.compile(
    rf"^[-_*~>#\s]*{BULLET_MARK}[-_*~>#\s]*accept\w*\b.*:[-_*~>#\s]*$",
    re.IGNORECASE,
)
# A heading's first word, which holds its id when the heading states one.
HEADING_ID = re.compile(r"^#+\s+(\S+)")
# A milestone id: `M11`, or `M11b` for one of its parts.
ID = re.compile(r"[a-z][a-z0-9]*\d+[a-z]?")
# A part id and its parent: `M11b` is part `b` of `M11`.
PART = re.compile(r"^(.*\d)([a-z])$")


def id_of(line):
    """The id a heading states, lower-cased, or None as it states none."""
    first = HEADING_ID.match(line)
    stated = first.group(1).rstrip(":").lower() if first else None
    return stated if stated and ID.fullmatch(stated) else None


def plain(text):
    """`text` without colour codes, a byte-order mark, or the times a
    downloaded job log puts before its lines."""
    return STAMP.sub("", ANSI.sub("", text.lstrip("\ufeff")))


def words(text):
    """`text` with its whitespace made single spaces, for comparing clauses."""
    return " ".join(text.split())


class Clause:
    """One clause of the list: its text, its tests, its sub-clauses (each a text
    and its tests, held in `parts`), and why it is pending, if it is."""

    def __init__(self, text):
        self.text = text
        self.tests = []
        self.parts = []
        self.pending = None

    def all_tests(self):
        return self.tests + [t for _, tests in self.parts for t in tests]


def segments(clause):
    """A clause's semicolon-separated sub-clauses, as a `### ` line states each."""
    return [words(s).rstrip(".") for s in clause.split(";")]


def manifest(path):
    """The clauses in `path`, in order, and what is wrong with the list."""
    clauses, problems = [], []
    with open(path, encoding="utf-8") as f:
        for n, line in enumerate(f, 1):
            line = line.strip()
            where = f"{path}:{n}"
            # A comment is one `#`; a clause `##`, and a sub-clause `###`.
            if not line or line == "#" or line.startswith("# "):
                continue
            if line.startswith("## "):
                text = line[3:].strip()
                if any(words(c.text) == words(text) for c in clauses):
                    problems.append(f"{where}: the clause is listed twice: {text}")
                clauses.append(Clause(text))
            elif line.startswith("### "):
                if not clauses:
                    problems.append(f"{where}: a sub-clause before any clause")
                    continue
                clauses[-1].parts.append((line[4:].strip(), []))
            elif HEADING.match(line):
                problems.append(f"{where}: only `## ` clauses and `### ` sub-clauses: {line}")
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
        problems += [
            f"{path}: sub-clause of `{c.text}` not listed: {s}" for s in stated if s not in listed
        ]
        problems += [
            f"{path}: not a sub-clause of `{c.text}`: {p}" for p in listed if p not in stated
        ]
        if c.tests:
            problems.append(f"{path}: `{c.text}` lists tests outside its sub-clauses")
    return clauses, problems


def is_part(stated, parent):
    """`stated` and `parent`, both lower-cased ids: `stated` names one of
    `parent`'s parts."""
    return stated.startswith(parent) and len(stated) == len(parent) + 1 and stated[-1].isalpha()


def own(lines, milestone, start, level, path):
    """The clauses in the Acceptance blocks of the section `path` states for
    `milestone` from line `start`, and what in them could not be read as one.

    The section's own text ends where one of its parts' sections starts. Under
    any other deeper heading a bullet is refused, because which block it
    belongs to would be a guess; a block marker ends that, as it starts a block
    of its own. A line that only looks like a block marker is refused too: open
    block or not, one that begins `**Acceptance`, and outside a block any line
    shaped like the marker at all. What sits under such a line is read as
    nothing, and a "b" part, which is asked for its parent's clauses as well as
    its own, is never then asked for a clause it states itself."""
    clauses, problems = [], []
    inside, deeper = False, None
    parent = PART.match(milestone)
    # The ids whose part sections come under this one: its own, and, as this
    # milestone is itself a part, its parent's, whose other parts sit beside it.
    wholes = [milestone] + ([parent.group(1)] if parent else [])
    for n, line in enumerate(lines[start + 1:], start + 2):
        heading = HEADING.match(line)
        if heading:
            if len(heading.group(1)) <= level:
                break
            stated = id_of(line)
            if stated and any(is_part(stated, whole) for whole in wholes):
                break
            inside, deeper = False, line.strip()
            continue
        text = line.strip()
        if ACCEPTANCE.match(text):
            inside, deeper = True, None
            continue
        if not text:
            continue
        if NEAR_MARKER.match(text) or not inside and MARKERISH.match(text):
            problems.append(
                f"{path}:{n}: not an Acceptance block marker, so everything "
                "under it would be skipped: a block starts at a line "
                'that begins `**Acceptance` and ends `:**`, as `**Acceptance:**` '
                "or `**Acceptance, beyond the full M4 test:**`: " + text
            )
        elif BULLET.match(line):
            if deeper:
                problems.append(
                    f"{path}:{n}: not a clause: a bullet under the deeper heading "
                    f"`{deeper}`, which states no Acceptance block: {text}"
                )
            elif not inside:
                # The section's own prose, as the list under `**Delivers:**`,
                # which states what the milestone does rather than what shows
                # it. A block marker, or one that only looks like one, is what
                # takes a bullet out of here.
                continue
            elif not line.startswith("- "):
                problems.append(f"{path}:{n}: not a clause or its continuation: {text}")
            else:
                clauses.append(line[2:])
        elif not inside:
            continue
        elif line.startswith("  ") and clauses and not line.lstrip().startswith(("-", "*")):
            clauses[-1] += " " + line.strip()
        else:
            problems.append(f"{path}:{n}: not a clause or its continuation: {text}")
    return [words(c) for c in clauses], problems


def milestone_clauses(milestone, path=MILESTONES):
    """The acceptance clauses `path` states for `milestone`, and what in its
    Acceptance blocks could not be read as one. A "b" part finishes its
    parent's work, so it states its parent's clauses as well as its own."""
    with open(path, encoding="utf-8") as f:
        lines = f.read().splitlines()
    lower = milestone.lower()

    def heading_at(stated):
        """Where `path` states the heading `stated`, and at what level."""
        name = re.compile(rf"^(#+)\s+{re.escape(stated)}\b", re.IGNORECASE)
        start = next((i for i, l in enumerate(lines) if name.match(l)), None)
        return (start, len(name.match(lines[start]).group(1))) if start is not None else (None, None)

    start, level = heading_at(lower)
    if start is None:
        return None, [f"{path} has no section for {milestone}"]
    clauses, problems = own(lines, lower, start, level, path)
    part = PART.match(lower)
    if part and part.group(2) == "b":
        parent, at = heading_at(part.group(1))
        if parent is None:
            problems.append(f"{path}: {milestone} has no {part.group(1).upper()} to finish")
        else:
            stated, unread = own(lines, part.group(1), parent, at, path)
            problems += unread
            clauses = [c for c in stated if c not in clauses] + clauses
    if not clauses:
        problems.append(f"{path}: {milestone} states no Acceptance clause")
    return clauses, problems


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


def run(clauses, features=FEATURES):
    names = sorted({test for c in clauses for _, test in c.all_tests()})
    cmd = [
        "cargo", "test", "--workspace", "--locked", "--no-fail-fast", "--color", "never",
        "--features", features, "--tests", "--", "--exact", *names,
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
    parser.add_argument("--features", default=FEATURES, help="Cargo features for this milestone's tests")
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
        code, log = run(clauses, args.features)
        errors = cargo_errors(log) if code != 0 else []
    seen = results(log)

    failed = False
    pending = []
    lines = [f"{title} acceptance", ""]
    for clause in clauses:
        bad = []
        if not clause.all_tests():
            bad.append("no test listed")
        for sub, tests in clause.parts:
            if not tests:
                bad.append(f"no test listed for: {sub}")
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
