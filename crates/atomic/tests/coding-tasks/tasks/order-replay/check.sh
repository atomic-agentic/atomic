#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# A bogus invalid transition shown by replay, caused by how the log loader orders events.
set -euo pipefail
source "$(dirname "$0")/../../lib/check-lib.sh"
unchanged examples
py_test discover -s tests -t . || fail "the visible tests fail"
hidden_tests
