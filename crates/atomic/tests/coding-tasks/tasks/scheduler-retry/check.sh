#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Two ordering bugs in different modules of a job-scheduler simulation: retry timing and tie-breaking.
source "$(dirname "$0")/../../lib/check-lib.sh"
unchanged README.md
unchanged examples
py_test discover -s tests -t . || fail "the visible tests fail"
hidden_tests
