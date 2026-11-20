#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Pagination that loses tickets only when equal timestamps straddle a page boundary: cursor and store both.
set -euo pipefail
source "$(dirname "$0")/../../lib/check-lib.sh"
unchanged tests
py_test discover -s tests -t . || fail "the visible tests fail"
hidden_tests
