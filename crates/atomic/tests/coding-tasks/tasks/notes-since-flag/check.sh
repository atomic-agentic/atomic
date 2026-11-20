#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# A feature in four places: --since in the CLI, the query, the README and tests that catch a since that is ignored.
set -euo pipefail
source "$(dirname "$0")/../../lib/check-lib.sh"
hidden_tests
py_test discover -s tests -t . || fail "the visible tests fail"
grep -q since tests/test_query.py || fail "tests/test_query.py has no test for since"
# The agent's query tests must fail against a filter_notes that ignores since.
mutant="$(mktemp -d "${TMPDIR:-/tmp}/atomic-task-mutant.XXXXXX")"
trap 'rm -rf "${G}" "${mutant}"' EXIT
cp -rf "${G}/." "${mutant}/"
cat >>"${mutant}/notes/query.py" <<'MUTANT'


import inspect as _hidden_inspect

_hidden_filter_notes = filter_notes


def filter_notes(*args, **kwargs):
    bound = _hidden_inspect.signature(_hidden_filter_notes).bind(*args, **kwargs)
    bound.arguments.pop("since", None)
    return _hidden_filter_notes(*bound.args, **bound.kwargs)
MUTANT
if (cd "${mutant}" && py_test tests.test_query 2>/dev/null); then
    fail "the tests in tests/test_query.py pass against a filter_notes that ignores since"
fi
