#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# A cache one module away from the symptom: mixed-profile batches priced right, each profile still loaded once.
source "$(dirname "$0")/../../lib/check-lib.sh"
unchanged tests
py_test discover -s tests -t . || fail "the visible tests fail"
hidden_tests
