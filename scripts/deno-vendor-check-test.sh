#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Tests for scripts/deno-vendor-check.sh, against copies of third_party/deno.
#
#   scripts/deno-vendor-check-test.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT
failures=0

check() {
    local want="$1" name="$2" script="$3"
    local got=pass
    "${script}" --vendor "${WORK}/vendor" >"${WORK}/out" 2>&1 || got=fail
    if [ "${got}" = "${want}" ]; then
        echo "ok   ${name}"
    else
        echo "FAIL ${name}: expected ${want}, got ${got}"
        sed 's/^/    /' "${WORK}/out"
        failures=$((failures + 1))
    fi
}

copy() {
    rm -rf "${WORK}/vendor"
    cp -R "${ROOT}/third_party/deno" "${WORK}/vendor"
}

copy
check pass "the vendored tree is the pinned releases with the carried change" "${ROOT}/scripts/deno-vendor-check.sh"

copy
printf '\n// tampered\n' >>"${WORK}/vendor/deno_core/runtime/setup.rs"
check fail "an edited vendored source fails" "${ROOT}/scripts/deno-vendor-check.sh"

copy
printf 'globalThis.extra = 1;\n' >"${WORK}/vendor/deno_v8/extra.js"
check fail "a file added to a vendored crate fails" "${ROOT}/scripts/deno-vendor-check.sh"

copy
rm "${WORK}/vendor/deno_core/01_core.js"
check fail "a vendored file removed fails" "${ROOT}/scripts/deno-vendor-check.sh"

copy
sed -i.bak 's/`7ad7d5294f640fd99db08c455e2453c9860b1fa728b76fbdacf0eed3e308d48a`/`0000000000000000000000000000000000000000000000000000000000000000`/' "${WORK}/vendor/README.md"
check fail "a .crate file that is not the pinned one fails" "${ROOT}/scripts/deno-vendor-check.sh"

# Control: with the comparison removed, the edited source passes.
copy
printf '\n// tampered\n' >>"${WORK}/vendor/deno_core/runtime/setup.rs"
mkdir -p "${WORK}/scripts"
sed 's/if ! diff -r /if false && ! diff -r /' "${ROOT}/scripts/deno-vendor-check.sh" >"${WORK}/scripts/deno-vendor-check.sh"
chmod +x "${WORK}/scripts/deno-vendor-check.sh"
check pass "control: without the comparison an edited source passes" "${WORK}/scripts/deno-vendor-check.sh"

[ "${failures}" -eq 0 ] && echo "deno-vendor-check: all cases pass"
exit "$([ "${failures}" -eq 0 ] && echo 0 || echo 1)"
