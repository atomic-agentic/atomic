#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Queries must be analyzed as documents are: folding, word splitting and phrase positions.
set -euo pipefail
source "$(dirname "$0")/../../lib/check-lib.sh"
unchanged docs
py_test discover -s tests -t . || fail "the visible tests fail"
hidden_tests
