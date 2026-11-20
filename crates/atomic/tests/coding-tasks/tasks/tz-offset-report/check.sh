#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# A wrong local day in the report, caused in the offset parser; every offset README.md allows or forbids.
set -euo pipefail
source "$(dirname "$0")/../../lib/check-lib.sh"
unchanged tests
py_test discover -s tests -t . || fail "the visible tests fail"
hidden_tests
