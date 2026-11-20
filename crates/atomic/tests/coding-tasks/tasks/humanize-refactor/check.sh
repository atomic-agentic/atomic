#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Refactor three near-duplicate functions onto one helper, with every quirk of the old output kept.
source "$(dirname "$0")/../../lib/check-lib.sh"
py_test discover -s tests -t . || fail "the visible tests fail"
hidden_tests
