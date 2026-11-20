#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Found by running the pipeline: a stage's errors and its exit status are lost in two places.
source "$(dirname "$0")/../../lib/check-lib.sh"
unchanged data
py_test discover -s tests -t . || fail "the visible tests fail"
hidden_tests
