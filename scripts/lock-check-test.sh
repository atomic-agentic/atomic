#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Tests for the V8 rule in scripts/lock-check.sh, against small lockfiles.
#
#   scripts/lock-check-test.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT
failures=0

package() {
    printf '\n[[package]]\nname = "%s"\nversion = "%s"\nsource = "%s"\n\n' "$1" "$2" "${3:-registry+https://github.com/rust-lang/crates.io-index}"
}
unused() {
    printf '\n[[patch.unused]]\nname = "%s"\nversion = "%s"\nsource = "git+https://example.invalid/rusty_v8.git"\n\n' "$1" "$2"
}
# expect pass|fail NAME LOCKFILE-CONTENT
expect() {
    local want="$1" name="$2"
    printf '%s' "$3" >"${WORK}/Cargo.lock"
    local got=pass
    "${ROOT}/scripts/lock-check.sh" v8 "${WORK}/Cargo.lock" >"${WORK}/out" 2>&1 || got=fail
    if [ "${got}" = "${want}" ]; then
        echo "ok   ${name}"
    else
        echo "FAIL ${name}: expected ${want}, got ${got}"
        sed 's/^/    /' "${WORK}/out"
        failures=$((failures + 1))
    fi
}

expect pass "no V8 at all" "$(package serde 1.0.0)"
expect fail "one V8 version" "$(package v8 150.4.0)"
expect fail "two V8 lines (150 and 152)" "$(package v8 150.4.0)$(package v8 152.2.0 git+https://example.invalid/rusty_v8.git)"
expect fail "two versions of one V8 line" "$(package v8 150.4.0)$(package v8 150.5.0)"
expect fail "v8 and deno_v8's alternative v8x" "$(package v8 150.4.0)$(package v8x 149.4.0-rc.4)"
expect pass "a patch Cargo.lock records as unused is not built" "$(unused v8 152.2.0)"
expect pass "the repository's own Cargo.lock" "$(cat "${ROOT}/Cargo.lock")"

[ "${failures}" -eq 0 ] && echo "lock-check V8 rule: all cases pass"
exit "$([ "${failures}" -eq 0 ] && echo 0 || echo 1)"
