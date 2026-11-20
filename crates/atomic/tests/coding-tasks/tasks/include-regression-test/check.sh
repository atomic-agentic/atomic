#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Fix a bug that shows only from another directory, and add a regression test that fails on the old code.
set -euo pipefail
source "$(dirname "$0")/../../lib/check-lib.sh"
unchanged conf
hidden_tests
[ -f tests/test_includes.py ] || fail "there is no tests/test_includes.py"
py_test discover -s tests -t . || fail "the visible tests fail"
# The agent's tests in tests/test_includes.py must fail when the package's seed files are put back.
mutant="$(mktemp -d "${TMPDIR:-/tmp}/atomic-task-mutant.XXXXXX")"
trap 'rm -rf "${G}" "${mutant}"' EXIT
cp -rf "${G}/." "${mutant}/"
cp -rf "${SEED}/deployctl/." "${mutant}/deployctl/"
if (cd "${mutant}" && py_test tests.test_includes 2>/dev/null); then
    fail "the tests in tests/test_includes.py pass against the old code"
fi
