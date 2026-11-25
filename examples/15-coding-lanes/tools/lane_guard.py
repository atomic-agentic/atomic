#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Decide whether one review finding may be reported as confirmed.

This is the file the example's control row switches. Its guard is the line the
row names, and the control's test is ``lane_guard_refuses_unverified`` under
``tests/``: with the guard replaced by the row's replacement the test fails,
which is what makes the row worth running.

Reads one finding on stdin as JSON and prints the decision. It reads nothing
but what it is given and writes nothing.
"""

from __future__ import annotations

import json
import sys

MAX_FINDING_BYTES = 4096


class Refused(Exception):
    """The finding cannot be decided, so nothing is reported as confirmed."""


def _text(value, field):
    # A missing or wrongly typed field is refused rather than read as empty:
    # an absent `verification` must not look like an unverified finding that
    # happens to pass.
    if not isinstance(value, str) or not value or len(value) > MAX_FINDING_BYTES:
        raise Refused(f"{field} must be text of 1..{MAX_FINDING_BYTES} bytes")
    return value


def decide(finding):
    """Whether `finding` may be reported confirmed, refusing what cannot be read."""
    if not isinstance(finding, dict):
        raise Refused("a finding is a JSON object")
    unknown = set(finding) - {"claim", "verification"}
    if unknown:
        raise Refused(f"unknown finding field {sorted(unknown)[0]!r}")
    claim = _text(finding.get("claim"), "claim")
    if "verification" not in finding:
        raise Refused("verification is missing; an absent record is not an unverified finding")
    verification = finding["verification"]
    if verification is not None:
        if not isinstance(verification, dict):
            raise Refused("verification must be an object or null")
        if verification.get("verified_by") != "host":
            raise Refused("verification was not made by the host")
    findings = [claim]
    # The control row switches this line.
    if findings and not verified(verification):
        return {"confirmed": False, "claim": claim, "reason": "unverified finding"}
    return {"confirmed": True, "claim": claim, "reason": ""}


def verified(verification):
    """Whether the host recorded this record; null is the author's own claim."""
    return verification is not None


def main():
    try:
        raw = sys.stdin.read()
        if len(raw.encode("utf-8", "surrogateescape")) > MAX_FINDING_BYTES * 4:
            raise Refused("input is too large")
        finding = json.loads(raw)
        result = decide(finding)
    except RecursionError:
        raise Refused("input nests too deeply")
    except Refused as why:
        print(f"lane_guard: {why}", file=sys.stderr)
        return 1
    except (json.JSONDecodeError, UnicodeDecodeError):
        print("lane_guard: stdin is not one JSON document", file=sys.stderr)
        return 1
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
