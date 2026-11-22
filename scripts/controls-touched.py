#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""The controls a change touches, which CI's Controls workflow runs.

    scripts/controls-touched.py BASE HEAD                       # the rows to run, one per line
    scripts/controls-touched.py BASE HEAD --shards N --shard I  # those in shard I of N
    scripts/controls-touched.py BASE HEAD --report              # touched rows not run, and deleted rows

A change touches a row (scripts/controls/<name>.row) when it adds or changes
the row's file, or changes the file the row switches off: a code change can
move the switch's text or stop the guarding test from biting. The rows are
read from HEAD's tree, not the working tree, and the changes are git's diff
from BASE to HEAD.

It fails, printing why, rather than leave out a row it cannot read: a row file
that is not one row of five fields named after its file, or a file under
scripts/controls/ that is not a regular `*.row` file. It also fails on a row
file the change deletes while the row's switch still applies, its text still
in a file that is still there, and no other row switches that text in that
file: deleting the row would only stop its control being checked. A deleted
row whose file is gone, or whose text is gone from it, or whose switch another
row now holds (as a rename leaves it), is listed by --report.

scripts/controls-local.txt lists the rows a CI runner cannot run, each with
its reason. A touched row listed there is left out of the rows to run and
listed by --report instead; a line there naming no row fails.

The shards are deterministic: the rows to run are sorted by their cargo
arguments before `--`, so that rows built the same way share a shard and its
build, then by name, and cut into N runs of nearly equal length.
"""

import argparse
import os
import posixpath
import subprocess
import sys

ROWS = "scripts/controls"
LOCAL = "scripts/controls-local.txt"


class Refused(Exception):
    pass


def git(*args):
    done = subprocess.run(["git", *args], capture_output=True)
    if done.returncode != 0:
        raise Refused(f"git {' '.join(args)} failed: {done.stderr.decode(errors='replace').strip()}")
    return done.stdout


def exists(commit, path):
    done = subprocess.run(["git", "cat-file", "-e", f"{commit}:{path}"], capture_output=True)
    return done.returncode == 0


def text_of(commit, path):
    try:
        return git("cat-file", "blob", f"{commit}:{path}").decode("utf-8")
    except UnicodeDecodeError:
        raise Refused(f"{path} is not UTF-8 at {commit}")


def parse(path, text):
    """One row: its name, the file it switches, and its five fields."""
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    if len(lines) != 1:
        raise Refused(f"{path} holds {len(lines)} rows, not the one it must")
    fields = lines[0].split("|")
    if len(fields) != 5:
        raise Refused(f"{path} has {len(fields) - 1} '|' separators, not the 4 between its five fields")
    name, file, switch, _, args = fields
    if name != posixpath.basename(path)[: -len(".row")]:
        raise Refused(f"{path} names itself '{name}', which is not its file name")
    if not file or not switch:
        raise Refused(f"{path} names no file or no text to switch")
    return {"name": name, "file": posixpath.normpath(file), "switch": switch, "args": args}


def rows_at(commit):
    """Every row in the commit's tree, by name."""
    rows = {}
    for entry in filter(None, git("ls-tree", "-z", f"{commit}:{ROWS}/").split(b"\0")):
        meta, _, base = entry.decode("utf-8", errors="replace").partition("\t")
        path = f"{ROWS}/{base}"
        if not base.endswith(".row"):
            raise Refused(f"{path} is not named <name>.row, so its control would never be read")
        if meta.split(" ")[0] not in ("100644", "100755"):
            raise Refused(f"{path} is not a regular file")
        row = parse(path, text_of(commit, path))
        rows[row["name"]] = row
    if not rows:
        raise Refused(f"{ROWS} holds no control at {commit}")
    return rows


def local_rows(commit, rows):
    """The rows a CI runner cannot run, by name, with each reason."""
    if not exists(commit, LOCAL):
        return {}
    local = {}
    for n, line in enumerate(text_of(commit, LOCAL).splitlines(), 1):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        name, colon, reason = line.partition(":")
        name, reason = name.strip(), reason.strip()
        if not colon or not name or not reason:
            raise Refused(f"{LOCAL}:{n}: '{line}' is not '<row>: <why a CI runner cannot run it>'")
        if name not in rows:
            raise Refused(f"{LOCAL}:{n}: names '{name}', which is not a row")
        if name in local:
            raise Refused(f"{LOCAL}:{n}: names '{name}' a second time")
        local[name] = reason
    return local


def deleted(base, head, changed, rows):
    """Each row file the change deletes, with why that is allowed."""
    held = {(row["file"], row["switch"]): row["name"] for row in rows.values()}
    report = []
    for path in sorted(changed):
        if posixpath.dirname(path) != ROWS or not path.endswith(".row"):
            continue
        if exists(head, path) or not exists(base, path):
            continue
        gone = parse(path, text_of(base, path))
        key = (gone["file"], gone["switch"])
        if not exists(head, gone["file"]):
            why = f"{gone['file']} is gone"
        elif key in held:
            why = f"{held[key]} switches the same text in {gone['file']}"
        elif gone["switch"] not in text_of(head, gone["file"]):
            why = f"its text is gone from {gone['file']}"
        else:
            raise Refused(
                f"{path} is deleted, but its switch still applies to {gone['file']} and no other row "
                "holds it; a control whose code remains keeps its row"
            )
        report.append((gone["name"], why))
    return report


def touched(base, head):
    """The rows to run, the touched rows CI does not run, and the deleted rows."""
    changed = {
        posixpath.normpath(path.decode("utf-8", errors="surrogateescape"))
        for path in git("diff", "--no-renames", "--name-only", "-z", base, head).split(b"\0")
        if path
    }
    rows = rows_at(head)
    gone = deleted(base, head, changed, rows)
    local = local_rows(head, rows)
    names = sorted(
        name for name, row in rows.items() if f"{ROWS}/{name}.row" in changed or row["file"] in changed
    )
    run = [rows[n] for n in names if n not in local]
    return run, [(n, local[n]) for n in names if n in local], gone


def build_key(row):
    args = row["args"].split()
    return " ".join(args[: args.index("--")] if "--" in args else args)


def shard(rows, shards, index):
    ordered = sorted(rows, key=lambda row: (build_key(row), row["name"]))
    n = len(ordered)
    return ordered[(index - 1) * n // shards : index * n // shards]


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("base")
    parser.add_argument("head")
    parser.add_argument("--shards", type=int, help="cut the rows to run into this many shards")
    parser.add_argument("--shard", type=int, help="print only this shard, from 1")
    parser.add_argument("--report", action="store_true", help="list touched rows not run, and deleted rows")
    args = parser.parse_args()
    if (args.shards is None) != (args.shard is None):
        parser.error("--shards and --shard go together")
    if args.shards is not None and not (args.shards >= 1 and 1 <= args.shard <= args.shards):
        parser.error("--shards must be at least 1, and --shard between 1 and --shards")
    if args.report and args.shards is not None:
        parser.error("--report takes no shard")
    try:
        run, not_run, gone = touched(args.base, args.head)
    except Refused as refused:
        print(f"scripts/controls-touched.py: {refused}", file=sys.stderr)
        return 1
    if args.report:
        for name, reason in not_run:
            print(f"not run in CI {name}: {reason}")
        for name, why in gone:
            print(f"deleted {name}: {why}")
        return 0
    if args.shards is not None:
        run = shard(run, args.shards, args.shard)
    for row in run:
        print(row["name"])
    return 0


if __name__ == "__main__":
    sys.exit(main())
