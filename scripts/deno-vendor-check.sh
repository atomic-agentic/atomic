#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Check that the vendored deno_core and deno_v8 are exactly the crates.io
# releases third_party/deno/README.md pins, with third_party/deno/pr-36665.diff
# applied and the LICENSE.md atomic adds: rebuild that tree from the .crate
# files, whose SHA-256 must match the README, and compare it file by file.
#
#   scripts/deno-vendor-check.sh [--vendor DIR]   # DIR defaults to third_party/deno
#
# A .crate file is read from cargo's registry cache when it is there and
# downloaded from crates.io otherwise; either way its hash is checked first.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VENDOR="${ROOT}/third_party/deno"
while [ $# -gt 0 ]; do
    case "$1" in
        --vendor) VENDOR="$2"; shift 2 ;;
        *) echo "usage: scripts/deno-vendor-check.sh [--vendor DIR]" >&2; exit 2 ;;
    esac
done
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT
mkdir -p "${WORK}/tree"

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

# "name version sha256" for each row of the README's crate table.
pins="$(sed -n 's/^| `\([a-z_0-9]*\)` | \([0-9.]*\) | `\([0-9a-f]\{64\}\)` |$/\1 \2 \3/p' "${VENDOR}/README.md")"
if [ "$(printf '%s\n' "${pins}" | sed '/^$/d' | wc -l)" -ne 2 ]; then
    echo "scripts/deno-vendor-check.sh: ${VENDOR}/README.md does not pin deno_core and deno_v8" >&2
    exit 1
fi

while read -r name version want; do
    crate="${WORK}/${name}-${version}.crate"
    cached="$(ls "${CARGO_HOME:-${HOME}/.cargo}"/registry/cache/*/"${name}-${version}.crate" 2>/dev/null | head -n1 || true)"
    if [ -n "${cached}" ]; then
        cp "${cached}" "${crate}"
    else
        curl -fsSL --retry 3 -o "${crate}" "https://static.crates.io/crates/${name}/${name}-${version}.crate"
    fi
    got="$(sha256 "${crate}")"
    if [ "${got}" != "${want}" ]; then
        echo "scripts/deno-vendor-check.sh: ${name}-${version}.crate has SHA-256 ${got}, not the pinned ${want}" >&2
        exit 1
    fi
    tar -xzf "${crate}" -C "${WORK}/tree"
    mv "${WORK}/tree/${name}-${version}" "${WORK}/tree/${name}"
    cp "${VENDOR}/LICENSE.md" "${WORK}/tree/${name}/LICENSE.md"
done <<<"${pins}"

(cd "${WORK}/tree" && patch -s -p1 --no-backup-if-mismatch) <"${VENDOR}/pr-36665.diff"

status=0
while read -r name _ _; do
    if ! diff -r "${WORK}/tree/${name}" "${VENDOR}/${name}" >"${WORK}/${name}.diff" 2>&1; then
        echo "scripts/deno-vendor-check.sh: ${VENDOR}/${name} differs from the crates.io release with pr-36665.diff applied:" >&2
        head -n 40 "${WORK}/${name}.diff" | sed 's/^/    /' >&2
        status=1
    fi
done <<<"${pins}"
[ "${status}" -eq 0 ] && echo "deno vendor: deno_core and deno_v8 are the pinned crates.io releases with pr-36665.diff applied"
exit "${status}"
