# SPDX-License-Identifier: Apache-2.0
"""Checks the proposed `decisions` block of a manifest, for the example checker.

Not an implementation of atomic. A decision records a choice, the conditions it rests
on and a date by which to look again. `atomic stale` is planned to evaluate those
conditions against stored eval results. This module checks only what can be checked
offline: the block's shape, that each condition names an atom with eval cases, and
which decisions are past their review date on a date given by the caller.

Nothing here reads the clock. The date is always an argument, as it is for the
proposed `atomic stale --as-of <date>`, so the same inputs give the same answer.
"""

from __future__ import annotations

import datetime
import re

KEYS = {"chose", "rests_on", "evidence_max_age", "review_by"}
REQUIRED = {"chose", "rests_on", "review_by"}
NAME = re.compile(r"[a-z][a-z0-9-]*")
MAX_AGE = re.compile(r"[1-9]\d*[dw]")
EVIDENCE = re.compile(r"\bevals\[\s*'([^']+)'\s*\]\.(\w+)")
EVIDENCE_FIELDS = {"pass_rate"}


def _fail(where, message):
    raise ValueError(f"{where}: {message}")


def check(block, atoms_with_cases, where):
    """Validates a `decisions` mapping. `atoms_with_cases` is the set of atom names that have eval cases."""
    if not isinstance(block, dict) or not block:
        _fail(where, "decisions must be a non-empty mapping of name to decision")
    for name, decision in block.items():
        here = f"{where}: decision {name}"
        if not isinstance(name, str) or not NAME.fullmatch(name):
            _fail(where, f"invalid decision name {name!r}")
        if not isinstance(decision, dict):
            _fail(here, "must be a mapping")
        unknown = decision.keys() - KEYS
        if unknown:
            _fail(here, f"unknown keys {sorted(unknown)}")
        missing = REQUIRED - decision.keys()
        if missing:
            _fail(here, f"missing {sorted(missing)}")
        if not isinstance(decision["chose"], str) or not decision["chose"].strip():
            _fail(here, "chose must say what was chosen")
        conditions = decision["rests_on"]
        if not isinstance(conditions, list) or not conditions or not all(isinstance(c, str) for c in conditions):
            _fail(here, "rests_on must be a non-empty list of CEL strings")
        for condition in conditions:
            evidence = EVIDENCE.findall(condition)
            if not evidence:
                _fail(here, f"{condition!r} names no evidence; the catalog supports evals['<atom>'].pass_rate")
            for atom, field in evidence:
                if atom not in atoms_with_cases:
                    _fail(here, f"{condition!r} rests on evals of {atom!r}, which has no eval cases in this project")
                if field not in EVIDENCE_FIELDS:
                    _fail(here, f"{condition!r} reads evals[...].{field}; the catalog supports {sorted(EVIDENCE_FIELDS)}")
        # A bare YAML date loads as a date. A quoted one is a string and is rejected,
        # so a review date cannot silently be an arbitrary label.
        if type(decision["review_by"]) is not datetime.date:
            _fail(here, "review_by must be a date, written YYYY-MM-DD without quotes")
        if "evidence_max_age" in decision and not (
            isinstance(decision["evidence_max_age"], str) and MAX_AGE.fullmatch(decision["evidence_max_age"])
        ):
            _fail(here, "evidence_max_age must be a number of days or weeks, such as 30d or 6w")


def past_review(block, as_of):
    """Names of decisions whose review date is before `as_of`. Due on the date itself is not past."""
    if type(as_of) is not datetime.date:
        raise TypeError("as_of must be a date supplied by the caller")
    return sorted(name for name, decision in block.items() if decision["review_by"] < as_of)
