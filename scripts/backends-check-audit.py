#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
#
# Read the output of `cargo test` run without a backend, for
# scripts/backends-check.sh, and say whether it shows what the check needs.
#
#   backends-check-audit.py all-fail LOG MESSAGE
#       every test that ran failed, each with MESSAGE in its own output
#   backends-check-audit.py discover LOG MESSAGE FAILING
#       the tests that failed, each with MESSAGE in its own output, are
#       exactly the Postgres variants (their names say postgres): none of
#       those passed or was ignored. Their names are written to FAILING.
#
# Either way, every test binary must have reported its result (one that
# aborted or was killed has not) and exited normally or with test failures.
# Prints what it found; exits 1 if the run does not show it.
import re
import sys

mode, log_path, message = sys.argv[1:4]
log = open(log_path, encoding="utf-8", errors="replace").read()
lines = log.splitlines()
problems = []

binaries = sum(1 for l in lines if re.match(r"\s+(Running|Doc-tests) ", l))
results = sum(1 for l in lines if l.startswith("test result:"))
if binaries == 0:
    problems.append("no test binary ran (did it build?)")
if results != binaries:
    problems.append(f"{binaries} test binaries ran and {results} reported a result: one ended early")
for l in lines:
    if "process didn't exit successfully" in l and "(exit status: 101)" not in l:
        problems.append(f"a test binary did not end normally: {l.strip()}")

outcome = {}
for l in lines:
    m = re.match(r"^test (.+) \.\.\. (ok|FAILED|ignored)\b", l)
    if m:
        outcome[m.group(1)] = m.group(2)
failed = sorted(n for n, o in outcome.items() if o == "FAILED")
passed = sorted(n for n, o in outcome.items() if o == "ok")

# Each failing test's own output, between its header and the next one.
blocks = {}
name = None
for l in lines:
    m = re.match(r"^---- (.+) stdout ----$", l)
    if m:
        name = m.group(1)
        blocks[name] = []
    elif l == "failures:" or l.startswith("test result:"):
        name = None
    elif name is not None:
        blocks[name].append(l)
unnamed = [n for n in failed if message not in "\n".join(blocks.get(n, []))]
if unnamed:
    problems.append(f"these failed without saying '{message}': " + ", ".join(unnamed))

if mode == "all-fail":
    if not failed:
        problems.append("no test failed")
    if passed:
        problems.append("these passed without the backend: " + ", ".join(passed))
elif mode == "discover":
    if not failed:
        problems.append("no test failed")
    misnamed = [n for n in failed if "postgres" not in n]
    if misnamed:
        problems.append(
            "these failed without the database, and their names do not say postgres: "
            + ", ".join(misnamed)
        )
    unrun = [n for n, o in outcome.items() if "postgres" in n and o != "FAILED"]
    if unrun:
        problems.append(
            "these say postgres, and passed or were ignored without the database: "
            + ", ".join(sorted(unrun))
        )
    with open(sys.argv[4], "w") as out:
        out.write("".join(n + "\n" for n in failed))
else:
    sys.exit(f"unknown mode {mode}")

for p in problems:
    print(f"  {p}")
if problems:
    sys.exit(1)
print(f"  {len(failed)} tests failed, each saying '{message}'; {len(passed)} passed")
