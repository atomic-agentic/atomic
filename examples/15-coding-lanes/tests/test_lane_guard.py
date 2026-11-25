#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""The guarding test for the row ``scripts/controls/lane-guard-bites.row``.

Stdlib only, so it runs wherever ``python3`` does: no provider, no network, no
built binary. It is the test the row names, and it fails when the row's switch
text is replaced in ``tools/lane_guard.py``, which is what makes that row worth
running:

    python3 tests/test_lane_guard.py
"""

from __future__ import annotations

import importlib.util
import pathlib
import sys

# Importing the guard would otherwise leave a __pycache__ directory in the
# source tree the catalog ships.
sys.dont_write_bytecode = True


def load_guard():
    path = pathlib.Path(__file__).resolve().parent.parent / "tools" / "lane_guard.py"
    spec = importlib.util.spec_from_file_location("lane_guard", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def lane_guard_refuses_unverified():
    """An author-claimed finding with no host record is never confirmed."""
    guard = load_guard()
    result = guard.decide({"claim": "the timeout is too short", "verification": None})
    assert result["confirmed"] is False, f"an unverified finding was confirmed: {result}"
    assert result["reason"] == "unverified finding", result


def lane_guard_confirms_a_host_record():
    """The counterexample the guard must still let through."""
    guard = load_guard()
    result = guard.decide(
        {"claim": "the timeout is too short", "verification": {"verified_by": "host"}}
    )
    assert result["confirmed"] is True, f"a host-recorded finding was refused: {result}"


def lane_guard_refuses_an_absent_record():
    """A missing record is refused, not read as an unverified finding."""
    guard = load_guard()
    try:
        result = guard.decide({"claim": "the timeout is too short"})
    except guard.Refused:
        return
    raise AssertionError(f"an absent verification record was decided: {result}")


def main():
    checks = [
        lane_guard_refuses_unverified,
        lane_guard_confirms_a_host_record,
        lane_guard_refuses_an_absent_record,
    ]
    failed = []
    for check in checks:
        try:
            check()
        except AssertionError as exc:
            failed.append(f"{check.__name__}: {exc}")
            print(f"FAIL {check.__name__}: {exc}", file=sys.stderr)
        else:
            print(f"PASS {check.__name__}")
    if failed:
        return 1
    print(f"PASS {len(checks)} lane guard checks")
    return 0


if __name__ == "__main__":
    sys.exit(main())
