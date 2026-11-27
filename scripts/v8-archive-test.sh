#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Tests for scripts/v8-archive.sh, against copies of an already verified
# mirror (run scripts/v8-archive.sh first; nothing is downloaded).
#
#   scripts/v8-archive-test.sh [MIRROR]   # default target/v8-mirror
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SOURCE="${1:-${ROOT}/target/v8-mirror}"
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT
failures=0

check() {
    local want="$1" name="$2" script="$3" mirror="$4"
    shift 4
    local got=pass
    "${script}" --mirror "${mirror}" "$@" >"${WORK}/out" 2>&1 || got=fail
    if grep -q "downloading" "${WORK}/out"; then
        echo "FAIL ${name}: tried to download"
        failures=$((failures + 1))
    elif [ "${got}" = "${want}" ]; then
        echo "ok   ${name}"
    else
        echo "FAIL ${name}: expected ${want}, got ${got}"
        sed 's/^/    /' "${WORK}/out"
        failures=$((failures + 1))
    fi
}

copy() {
    rm -rf "${WORK}/mirror"
    cp -R "${SOURCE}" "${WORK}/mirror"
}

# The version Cargo.lock builds; the mirror may also hold others.
version="v$(awk '
    /^\[\[/ { in_package = ($0 == "[[package]]"); name = "" }
    in_package && /^name = / { gsub(/"/, "", $3); name = $3 }
    in_package && /^version = / && name == "v8" { gsub(/"/, "", $3); print $3 }
' "${ROOT}/Cargo.lock" | sort -u)"
if [ ! -d "${SOURCE}/${version}" ]; then
    echo "scripts/v8-archive-test.sh: ${SOURCE} has no ${version}; run scripts/v8-archive.sh first" >&2
    exit 1
fi
archive="$(ls "${SOURCE}/${version}" | grep -E '^(lib)?rusty_v8_' | head -n1)"
binding="$(ls "${SOURCE}/${version}" | grep '^src_binding' | head -n1)"
target="${archive#lib}"
target="${target#rusty_v8_release_}"
target="${target%.a.gz}"
target="${target%.lib.gz}"

copy
check pass "the pinned archive and bindings verify offline" "${ROOT}/scripts/v8-archive.sh" "${WORK}/mirror" "${target}"

copy
printf 'tampered' >>"${WORK}/mirror/${version}/${archive}"
check fail "a tampered archive fails" "${ROOT}/scripts/v8-archive.sh" "${WORK}/mirror" "${target}"

copy
printf '// tampered\n' >>"${WORK}/mirror/${version}/${binding}"
check fail "tampered bindings fail" "${ROOT}/scripts/v8-archive.sh" "${WORK}/mirror" "${target}"

copy
check fail "a target deps/v8.toml does not pin fails" "${ROOT}/scripts/v8-archive.sh" "${WORK}/mirror" "riscv64gc-unknown-linux-gnu"

# Control: with the hash comparison removed, the tampered archive is accepted.
copy
printf 'tampered' >>"${WORK}/mirror/${version}/${archive}"
mkdir -p "${WORK}/scripts"
sed 's/if \[ "${got}" != "${want}" \]; then/if false; then/' "${ROOT}/scripts/v8-archive.sh" >"${WORK}/scripts/v8-archive.sh"
chmod +x "${WORK}/scripts/v8-archive.sh"
ln -s "${ROOT}/Cargo.lock" "${WORK}/Cargo.lock"
mkdir -p "${WORK}/deps" && ln -s "${ROOT}/deps/v8.toml" "${WORK}/deps/v8.toml"
check pass "control: without the hash check a tampered archive passes" "${WORK}/scripts/v8-archive.sh" "${WORK}/mirror" "${target}"

[ "${failures}" -eq 0 ] && echo "v8-archive: all cases pass"
exit "$([ "${failures}" -eq 0 ] && echo 0 || echo 1)"
