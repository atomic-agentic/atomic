#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Cut lines to a byte limit at code-point boundaries, counting the prefix in bytes too.
source "$(dirname "$0")/../../lib/check-lib.sh"
py_test discover -s tests -t . || fail "the visible tests fail"
hidden_tests
