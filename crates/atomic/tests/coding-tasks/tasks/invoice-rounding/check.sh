#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Money rounding across modules: half-up cents, and taxes and totals from the rounded lines, as README says.
set -euo pipefail
source "$(dirname "$0")/../../lib/check-lib.sh"
unchanged tests
unchanged README.md
py_test discover -s tests -t . || fail "the visible tests fail"
hidden_tests
