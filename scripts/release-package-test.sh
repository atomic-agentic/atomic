#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Tests for the armed-binary check in scripts/release-package.sh: a binary
# with the crash tests' failpoints, or the test-only compiler dispatch,
# compiled in is refused before anything is packaged, and one without them
# gets past the check. A stand-in for cargo
# stops the script right after it; no toolchain is needed.
#
#   scripts/release-package-test.sh
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)" || exit 1
trap 'rm -rf "${WORK}"' EXIT
failures=0
TRIPLE="x86_64-unknown-linux-gnu"

mkdir -p "${WORK}/bin" "${WORK}/repo/scripts" "${WORK}/repo/target/${TRIPLE}/release"
cp "${ROOT}/scripts/release-package.sh" "${WORK}/repo/scripts/"
printf '#!/bin/sh\necho "the stand-in cargo was reached" >&2\nexit 3\n' >"${WORK}/bin/cargo"
chmod +x "${WORK}/bin/cargo"
BIN="${WORK}/repo/target/${TRIPLE}/release/atomic"

# package <binary content>: sets `code` and `out`.
package() {
    printf '%s' "$1" >"${BIN}"
    chmod +x "${BIN}"
    out="$(PATH="${WORK}/bin:${PATH}" "${WORK}/repo/scripts/release-package.sh" "${TRIPLE}" 2>&1)"
    code=$?
}

package $'\x7fELF...ATOMIC_FAILPOINT_LOG...'
if [ "${code}" -eq 0 ] || ! grep -q "has crash-test failpoints compiled in" <<<"${out}" ||
    grep -q "stand-in cargo was reached" <<<"${out}"; then
    echo "FAIL an armed binary was not refused first: exit ${code}: ${out}" >&2
    failures=$((failures + 1))
fi

package $'\x7fELF...ATOMIC_COMPILER_TEST_DISPATCH...'
if [ "${code}" -eq 0 ] || ! grep -q "has the compiler-in-tests feature compiled in" <<<"${out}" ||
    grep -q "stand-in cargo was reached" <<<"${out}"; then
    echo "FAIL a binary with the test compiler dispatch was not refused first: exit ${code}: ${out}" >&2
    failures=$((failures + 1))
fi

package $'\x7fELF...an ordinary release binary...'
if grep -q "compiled in" <<<"${out}" || ! grep -q "stand-in cargo was reached" <<<"${out}"; then
    echo "FAIL an unarmed binary did not get past the check: exit ${code}: ${out}" >&2
    failures=$((failures + 1))
fi

if [ "${failures}" -ne 0 ]; then
    exit 1
fi
echo "release-package tests passed" >&2
