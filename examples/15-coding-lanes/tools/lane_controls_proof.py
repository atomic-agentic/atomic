#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Read the control rows a coding candidate touched and report each one.

This is the example atom's local fixture command. In a production coding run,
atomic substitutes its pinned host controls driver for this command. The
local fixture receives a checkout, commit, bound base, admitted revision,
reference and selected rows; it answers with one verdict per row. A request
from a run whose command the host substituted also carries ``row_seconds``,
the seconds one row may run: this command starts no subprocess, so it refuses
a budget no driver could have been handed and answers without using it. It is
declared ``effects: { external: 'read-only' }`` and holds to that: it opens
files to read and writes nothing.

What ``intact`` claims, and what it does not: the row is one line of five
fields, it names itself, its switched file is a regular file under the checkout
it was given, its switch text sits in that file exactly once, and its guarding
test is named in a file under ``tests/``. It does not claim the switch was made
and the test failed. The production host driver performs that removal test;
this local command deliberately does not go so far.

Two outcomes, kept apart on purpose:

* **refused** -- exit non-zero with nothing on stdout. The row could not be
  read, so no verdict is invented for it. Anything but "this file is not here"
  is in this class: a permission denied, a link loop, "not a directory", any
  other error, and a name, a row line or a request that does not parse.
* **reported** -- exit zero with a verdict. ``blind`` says the row is here and
  does not hold together, which the reviewers must see, and which still stops
  the workflow because no row was proved.
"""

from __future__ import annotations

import json
import os
import pathlib
import re
import stat
import sys

ROW_DIR = ("scripts", "controls")
TEST_DIR = "tests"
ROW_FIELDS = 5
#: The widest row budget the host's own driver will hand out, so a request this
#: illustration cannot have been given is refused rather than read as harmless.
MAX_ROW_SECONDS = 300
MAX_ROW_BYTES = 16384
MAX_SWITCH_BYTES = 4096
ROW_NAME = re.compile(r"[a-z0-9][a-z0-9-]{0,99}")
COMMIT = re.compile(r"[0-9a-f]{40,64}")
ROW_FIELDS_HERE = ("name", "file", "text to replace", "replacement", "test selector")


class Refused(Exception):
    """The row cannot be read, so it gets no verdict at all."""


def refuse(why):
    raise Refused(why)


def read_row(source, name):
    """The one line of row ``name``, refusing anything that is not that line."""
    if not ROW_NAME.fullmatch(name):
        refuse(f"row name {name!r} is not lowercase letters, digits and dashes")
    path = source.joinpath(*ROW_DIR) / f"{name}.row"
    if path.is_symlink():
        refuse(f"{path}: a row is read as itself, not through a link")
    try:
        mode = path.stat(follow_symlinks=False)
    except FileNotFoundError:
        refuse(f"{path}: the selected row is not in the checkout")
    except OSError as why:
        refuse(f"{path}: cannot be inspected: {why.strerror or why}")
    if not stat.S_ISREG(mode.st_mode):
        refuse(f"{path}: is not a regular file")
    try:
        text = path.read_text(encoding="utf-8", errors="strict")
    except FileNotFoundError:
        refuse(f"{path}: the selected row is not in the checkout")
    except OSError as why:
        refuse(f"{path}: cannot be read: {why.strerror or why}")
    except (UnicodeDecodeError, ValueError):
        refuse(f"{path}: is not UTF-8 text")
    if len(text.encode("utf-8", "replace")) > MAX_ROW_BYTES:
        refuse(f"{path}: is larger than {MAX_ROW_BYTES} bytes")
    lines = [line for line in text.splitlines() if line.strip()]
    if len(lines) != 1:
        refuse(f"{path}: holds {len(lines)} lines, and a row holds exactly one")
    fields = lines[0].split("|")
    if len(fields) != ROW_FIELDS:
        refuse(f"{path}: holds {len(fields)} |-separated fields, expected {ROW_FIELDS}")
    if any(not field.strip() for field in fields):
        refuse(f"{path}: holds an empty field")
    row_name, switched, needle, replacement, selector = (f.strip() for f in fields)
    if row_name != name:
        refuse(f"{path}: names itself {row_name!r}, and the selected row is {name!r}")
    if len(needle) > MAX_SWITCH_BYTES or len(replacement) > MAX_SWITCH_BYTES:
        refuse(f"{path}: a switch text longer than {MAX_SWITCH_BYTES} bytes")
    return switched, needle, selector


def guarded_by(source, selector):
    """The test the row's last field names, and whether it is named under tests/."""
    test = selector.split()[-1]
    if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]{0,199}", test):
        refuse(f"the row's guarding test {test!r} is not a name atomic can look for")
    root = source / TEST_DIR
    if root.is_symlink():
        refuse(f"{root}: a test tree is read as itself, not through a link")
    try:
        entries = sorted(root.iterdir())
    except FileNotFoundError:
        return test, None
    except OSError as why:
        refuse(f"{root}: cannot be listed: {why.strerror or why}")
    found = []
    for entry in entries:
        if not entry.name.endswith(".py") and not entry.name.endswith(".rs"):
            continue
        if entry.is_symlink():
            refuse(f"{entry}: a guarding test is read as itself, not through a link")
        try:
            mode = entry.stat(follow_symlinks=False)
        except FileNotFoundError:
            continue
        except OSError as why:
            refuse(f"{entry}: cannot be inspected: {why.strerror or why}")
        if not stat.S_ISREG(mode.st_mode):
            continue
        try:
            text = entry.read_text(encoding="utf-8", errors="strict")
        except FileNotFoundError:
            continue
        except OSError as why:
            refuse(f"{entry}: cannot be read: {why.strerror or why}")
        except (UnicodeDecodeError, ValueError):
            refuse(f"{entry}: is not UTF-8 text")
        if re.search(rf"\b{re.escape(test)}\b", text):
            found.append(entry.name)
    return test, (",".join(found) if found else None)


def read_switched(source, switched):
    """The row's switched file under `source`, refusing any path that leaves it."""
    if switched.startswith("/") or switched.startswith("~"):
        refuse(f"the row's file {switched!r} is not a path inside the checkout")
    candidate = source / switched
    walked = source
    for part in pathlib.PurePosixPath(switched).parts:
        walked = walked / part
        if walked.is_symlink():
            refuse(f"{walked}: a switched file is reached as itself, not through a link")
    try:
        mode = candidate.stat(follow_symlinks=False)
    except FileNotFoundError:
        return None
    except OSError as why:
        refuse(f"{candidate}: cannot be inspected: {why.strerror or why}")
    if not stat.S_ISREG(mode.st_mode):
        refuse(f"{candidate}: is not a regular file")
    resolved = candidate.resolve()
    if os.path.commonpath([str(source.resolve()), str(resolved)]) != str(source.resolve()):
        refuse(f"{candidate}: is outside the checkout it was given")
    try:
        return candidate.read_text(encoding="utf-8", errors="strict")
    except FileNotFoundError:
        return None
    except OSError as why:
        refuse(f"{candidate}: cannot be read: {why.strerror or why}")
    except (UnicodeDecodeError, ValueError):
        refuse(f"{candidate}: is not UTF-8 text")


def proof(request):
    """One verdict per selected row, refusing what gives no basis for one."""
    if not isinstance(request, dict):
        refuse("the request is a JSON object")
    missing = {"source", "commit", "bound", "rows"} - set(request)
    if missing:
        refuse(f"the request has no {sorted(missing)[0]}")
    extra = set(request) - {"source", "commit", "bound", "rows", "row_seconds"}
    if extra:
        refuse(f"the request has an unknown field {sorted(extra)[0]!r}")
    source_text = request["source"]
    if not isinstance(source_text, str) or not source_text:
        refuse("the request's source is not a path")
    source = pathlib.Path(source_text)
    if not source.is_absolute():
        source = pathlib.Path.cwd() / source
    try:
        source = source.resolve()
    except OSError as why:
        refuse(f"the request's source cannot be resolved: {why.strerror or why}")
    if source.is_symlink():
        refuse(f"{source}: a checkout is read as itself, not through a link")
    if not source.is_dir():
        refuse(f"{source}: is not a directory")
    commit = request["commit"]
    if not isinstance(commit, str) or not COMMIT.fullmatch(commit):
        refuse("the request's commit is not 40 to 64 hexadecimal characters")
    bound = request["bound"]
    if not isinstance(bound, dict) or set(bound) != {"reference", "base", "admitted"}:
        refuse("the request's bound is a reference, a base and an admitted revision")
    reference, base, admitted = bound["reference"], bound["base"], bound["admitted"]
    if not isinstance(reference, str) or not reference:
        refuse("the request's bound reference is not text")
    if not isinstance(base, str) or not COMMIT.fullmatch(base):
        refuse("the request's bound base is not 40 to 64 hexadecimal characters")
    if not isinstance(admitted, str) or not COMMIT.fullmatch(admitted):
        refuse("the request's bound admitted revision is not 40 to 64 hexadecimal characters")
    rows = request["rows"]
    if not isinstance(rows, list) or not 1 <= len(rows) <= 16:
        refuse("the request names 1 to 16 rows")
    if "row_seconds" in request:
        # A budget the host's own driver could never have handed out is a
        # request this step was not meant to answer, so it is refused here
        # rather than carried along and quietly ignored.
        seconds = request["row_seconds"]
        whole = isinstance(seconds, int) and not isinstance(seconds, bool)
        if not whole or not 1 <= seconds <= MAX_ROW_SECONDS:
            refuse(
                f"the request's row_seconds is not a whole number of seconds from 1 to {MAX_ROW_SECONDS}"
            )
    results = []
    for name in rows:
        if not isinstance(name, str):
            refuse("a selected row is not text")
        switched, needle, selector = read_row(source, name)
        text = read_switched(source, switched)
        test, where = guarded_by(source, selector)
        if text is None:
            results.append(
                {
                    "name": name,
                    "verdict": "blind",
                    "detail": f"blind {name}: its switched file {switched} is not in the checkout, so nothing switches",
                }
            )
            continue
        count = text.count(needle)
        if count != 1:
            results.append(
                {
                    "name": name,
                    "verdict": "blind",
                    "detail": f"blind {name}: its switch text appears {count} times in {switched}, and the row needs it exactly once",
                }
            )
        elif where is None:
            results.append(
                {
                    "name": name,
                    "verdict": "blind",
                    "detail": f"blind {name}: no file under {TEST_DIR}/ names the guarding test {test}",
                }
            )
        else:
            results.append(
                {
                    "name": name,
                    "verdict": "intact",
                    "detail": f"intact {name}: one switch text in {switched}, guarding test {test} in {where}; the switch itself was not made here",
                }
            )
    return {
        "ok": all(row["verdict"] == "intact" for row in results),
        # Echoed as it was given: an absolute path resolved here would make the
        # answer depend on where the checkout happens to sit on this machine.
        "source": source_text,
        "commit": commit,
        "bound": {"reference": reference, "base": base, "admitted": admitted},
        "rows": results,
    }


def main():
    try:
        raw = sys.stdin.read()
        if len(raw.encode("utf-8", "surrogateescape")) > 1 << 20:
            refuse("the request is too large")
        answer = proof(json.loads(raw))
    except RecursionError:
        refuse("the request nests too deeply")
    except Refused as why:
        print(f"lane_controls_proof: {why}", file=sys.stderr)
        return 1
    except json.JSONDecodeError:
        print("lane_controls_proof: stdin is not one JSON document", file=sys.stderr)
        return 1
    print(json.dumps(answer, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
