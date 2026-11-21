#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Split the workspace's tests into the shards CI runs in parallel.

    scripts/test-shards.py check-table      # every test target is in exactly one shard (cargo metadata)
    scripts/test-shards.py run <shard>      # check the table, then run that shard's tests
    scripts/test-shards.py check            # every test cargo runs is in exactly one shard (--list)

scripts/test-shards.txt says which shard runs which test target. Each shard
runs with CI's test features (FEATURES, shared with scripts/acceptance.py) over
the whole workspace, so every shard resolves the same features and reuses the
same build cache.

`check-table` compares the table with the targets `cargo metadata` reports and
needs no build: it fails on a test target in no shard or in two, on a line
naming no target, and on a kind of target the table cannot place. `run` does
the same before it runs anything, so a new test file left out of the table
fails every shard rather than running nowhere.

`check` lists the tests (`cargo test -- --list`) of the unsplit run, all of
CI's tests in one `cargo test --workspace`, and of each shard, and fails
unless each test of the unsplit run is listed by exactly one shard and no
shard lists a test the unsplit run does not. It builds every test target.
"""

import argparse
import json
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "scripts"))
from acceptance import FEATURES, plain  # noqa: E402

TABLE = os.path.join(ROOT, "scripts", "test-shards.txt")
# A test binary, by the name and hash cargo gives its file, so that two
# targets of one name in different packages stay apart.
RUNNING = re.compile(r"^\s*Running .*[/\\]deps[/\\]([A-Za-z0-9_]+-[0-9a-f]+)(?:\.exe)?\)\s*$")
# Doctests carry no hash, only their library's name, which units_needed()
# requires to be unique.
DOCTESTS = re.compile(r"^\s*Doc-tests (\S+)\s*$")
LISTED = re.compile(r"^(.+): (test|bench)$")
# The units a table line can name, besides `test <name>`.
GROUPS = {"lib", "bins", "doc", "examples"}
LIB_KINDS = {"lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"}


def read_table(path):
    """[(shard, [unit, ...])] in order, and what is wrong with the file."""
    shards, problems = [], []
    with open(path, encoding="utf-8") as f:
        for n, raw in enumerate(f, 1):
            line = raw.split("#", 1)[0].strip()
            where = f"{path}:{n}"
            if not line:
                continue
            m = re.fullmatch(r"\[([A-Za-z0-9_-]+)\]", line)
            if m:
                if any(name == m.group(1) for name, _ in shards):
                    problems.append(f"{where}: shard [{m.group(1)}] is defined twice")
                shards.append((m.group(1), []))
                continue
            if not shards:
                problems.append(f"{where}: a unit before any [shard]: {line}")
                continue
            words = line.split()
            if len(words) == 1 and words[0] in GROUPS:
                shards[-1][1].append(words[0])
            elif len(words) == 2 and words[0] == "test":
                shards[-1][1].append(f"test {words[1]}")
            else:
                problems.append(f"{where}: expected lib, bins, doc, examples or `test <name>`: {line}")
    if not shards:
        problems.append(f"{path}: no shards")
    for name, units in shards:
        if not units:
            problems.append(f"{path}: shard [{name}] runs nothing")
    return shards, problems


def metadata():
    """The workspace's packages, as `cargo metadata` reports them."""
    out = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"],
        cwd=ROOT, stdout=subprocess.PIPE, check=True,
    ).stdout
    return json.loads(out)["packages"]


def units_needed(packages):
    """{unit: where it comes from} for every unit a plain `cargo test
    --workspace` runs or builds, and what the table cannot express."""
    needed, problems, tests, libs = {}, [], {}, {}
    for pkg in packages:
        for target in pkg["targets"]:
            kinds = set(target["kind"])
            name = target["name"]
            where = f"{pkg['name']} {'/'.join(sorted(kinds))} {name}"
            if kinds & LIB_KINDS:
                libs.setdefault(name.replace("-", "_"), []).append(pkg["name"])
                if target.get("test", True):
                    needed.setdefault("lib", where)
                if target.get("doctest", True):
                    needed.setdefault("doc", where)
            elif "bin" in kinds:
                if target.get("test", True):
                    needed.setdefault("bins", where)
            elif "test" in kinds:
                tests.setdefault(name, []).append(pkg["name"])
                needed[f"test {name}"] = where
            elif "example" in kinds:
                if target.get("test", False):
                    problems.append(f"{where}: an example with `test = true`, which no unit runs")
                needed.setdefault("examples", where)
            elif "bench" in kinds:
                if target.get("test", False):
                    problems.append(f"{where}: a bench with `test = true`, which no unit runs")
            elif "custom-build" not in kinds:
                problems.append(f"{where}: a kind of target the shards cannot place")
    for name, pkgs in sorted(tests.items()):
        if len(pkgs) > 1:
            problems.append(f"test target {name} is in {', '.join(pkgs)}; `--test {name}` cannot tell them apart")
    for name, pkgs in sorted(libs.items()):
        if len(pkgs) > 1:
            problems.append(f"library {name} is in {', '.join(pkgs)}; their doctests cannot be told apart")
    return needed, problems


def check_table(shards, packages):
    """What is wrong with `shards` against the workspace's targets."""
    needed, problems = units_needed(packages)
    seen = {}
    for name, units in shards:
        for unit in units:
            if unit in seen:
                problems.append(f"{unit} is in shard [{seen[unit]}] and again in [{name}]")
                continue
            seen[unit] = name
            if unit not in needed:
                problems.append(f"shard [{name}] lists {unit}, which the workspace does not have")
    for unit, where in sorted(needed.items()):
        if unit not in seen:
            problems.append(f"{unit} ({where}) is in no shard: add it to one in scripts/test-shards.txt")
    return problems


def commands(units):
    """The cargo commands that run `units`, in order."""
    base = ["--workspace", "--locked", "--features", FEATURES]
    selectors = []
    for unit in units:
        if unit == "lib":
            selectors.append("--lib")
        elif unit == "bins":
            selectors.append("--bins")
        elif unit.startswith("test "):
            selectors += ["--test", unit[5:]]
    cmds = []
    if selectors:
        cmds.append(["cargo", "test", *base, "--no-fail-fast", *selectors])
    if "doc" in units:
        cmds.append(["cargo", "test", *base, "--no-fail-fast", "--doc"])
    if "examples" in units:
        cmds.append(["cargo", "build", *base, "--examples"])
    return cmds


def listed(cmd):
    """{(binary, test)} that `cmd -- --list` lists, and what went wrong."""
    run = subprocess.run(
        [*cmd, "--color", "never", "--", "--list"],
        cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        encoding="utf-8", errors="replace",
    )
    tests, binary, problems = set(), None, []
    for line in plain(run.stdout).splitlines():
        m = RUNNING.match(line)
        if m:
            binary = m.group(1)
            continue
        m = DOCTESTS.match(line)
        if m:
            binary = f"doctests of {m.group(1)}"
            continue
        m = LISTED.match(line)
        if m and binary is not None:
            tests.add((binary, m.group(1)))
    if run.returncode != 0:
        tail = [l for l in plain(run.stdout).splitlines() if l.strip()][-5:]
        problems.append(f"`{' '.join(cmd)} -- --list` exited {run.returncode}:")
        problems += [f"    {l}" for l in tail]
    return tests, problems


def by_binary(tests, limit=5):
    """Lines naming `tests`, grouped by binary, a few names each."""
    groups = {}
    for binary, test in sorted(tests):
        groups.setdefault(binary, []).append(test)
    lines = []
    for binary, names in groups.items():
        shown = ", ".join(names[:limit]) + (", ..." if len(names) > limit else "")
        lines.append(f"    {binary}: {len(names)} test{'' if len(names) == 1 else 's'} ({shown})")
    return lines


def check_lists(shards):
    problems = []
    unsplit, bad = listed(["cargo", "test", "--workspace", "--locked", "--features", FEATURES])
    problems += bad
    owner = {}
    for name, units in shards:
        found = set()
        for cmd in commands(units):
            if cmd[1] != "test":
                continue
            tests, bad = listed(cmd)
            problems += bad
            found |= tests
        twice = {t for t in found if t in owner}
        if twice:
            problems.append(f"shard [{name}] also runs tests another shard runs:")
            problems += by_binary(twice)
        extra = found - unsplit
        if extra:
            problems.append(f"shard [{name}] runs tests the unsplit run does not:")
            problems += by_binary(extra)
        for t in found:
            owner.setdefault(t, name)
        binaries = {b for b, _ in found}
        print(f"[{name}]: {len(found)} tests in {len(binaries)} binaries")
    missing = unsplit - set(owner)
    if missing:
        problems.append("tests the unsplit run lists that no shard runs:")
        problems += by_binary(missing)
    print(f"unsplit: {len(unsplit)} tests in {len({b for b, _ in unsplit})} binaries")
    if not unsplit:
        problems.append("the unsplit run listed no tests")
    return problems


def report(problems, what):
    if problems:
        print("\n".join(problems), file=sys.stderr)
        print(f"scripts/test-shards.py: {what}: FAILED", file=sys.stderr)
        return 1
    print(f"scripts/test-shards.py: {what}: ok")
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("action", choices=["check-table", "run", "check"])
    parser.add_argument("shard", nargs="?", help="the shard to run (run only)")
    parser.add_argument("--table", default=TABLE, help="the table to read instead of scripts/test-shards.txt")
    args = parser.parse_args()
    if (args.action == "run") != (args.shard is not None):
        parser.error("run takes a shard; the checks take none")

    shards, problems = read_table(args.table)
    if not problems:
        problems = check_table(shards, metadata())
    if problems or args.action == "check-table":
        return report(problems, "the shard table")

    if args.action == "check":
        return report(check_lists(shards), "every test in exactly one shard")

    units = dict(shards).get(args.shard)
    if units is None:
        return report([f"no shard [{args.shard}] in {args.table}"], "run")
    status = 0
    # Every command runs, so one failure does not hide another's.
    for cmd in commands(units):
        print("+ " + " ".join(cmd), flush=True)
        code = subprocess.call(cmd, cwd=ROOT)
        if code != 0:
            print(f"scripts/test-shards.py: `{' '.join(cmd[:2])}` exited {code}", file=sys.stderr)
            status = 1
    return status


if __name__ == "__main__":
    sys.exit(main())
