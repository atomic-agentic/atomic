#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Fix the code to the tests, except one test that contradicts the README: that test is corrected instead.
source "$(dirname "$0")/../../lib/check-lib.sh"
unchanged README.md
unchanged tests/__init__.py
unchanged tests/test_version.py
unchanged tests/test_ranges.py
unchanged tests/test_resolve.py
unchanged tests/test_cli.py
py_test discover -s tests -t . || fail "the visible tests fail"
hidden_tests
