#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Check that every registry crate atomic shares with Codex resolves to a
# version Codex's own Cargo.lock has at the pinned revision, unless
# deps/lock-exceptions.toml lists the difference with a reason.
#
#   scripts/lock-check.sh
#
# Codex's lockfile is read from CODEX_SRC (a clone of openai/codex) when set,
# otherwise downloaded at the pinned revision.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REV="$(sed -n 's/^rev = "\(.*\)"$/\1/p' "${ROOT}/deps/codex.toml" | head -n1)"
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT

if [ -n "${CODEX_SRC:-}" ]; then
    git -C "${CODEX_SRC}" show "${REV}:codex-rs/Cargo.lock" >"${WORK}/codex.lock"
else
    curl -fsSL --retry 3 -o "${WORK}/codex.lock" "https://raw.githubusercontent.com/openai/codex/${REV}/codex-rs/Cargo.lock"
fi

# "name version" for every package from the crates.io registry.
registry() {
    awk '
        /^\[\[package\]\]/ { name = ""; version = "" }
        /^name = / { gsub(/"/, "", $3); name = $3 }
        /^version = / { gsub(/"/, "", $3); version = $3 }
        /^source = "registry\+/ { print name, version }
    ' "$1" | sort -u
}
registry "${WORK}/codex.lock" >"${WORK}/codex.txt"
registry "${ROOT}/Cargo.lock" >"${WORK}/atomic.txt"
awk '
    /^crate = / { gsub(/"/, "", $3); crate = $3 }
    /^version = / { gsub(/"/, "", $3); print crate, $3 }
' "${ROOT}/deps/lock-exceptions.toml" | sort -u >"${WORK}/allowed.txt"

# Crates both lockfiles have, then atomic's versions of them that Codex lacks.
cut -d' ' -f1 "${WORK}/codex.txt" | sort -u >"${WORK}/codex-names.txt"
awk 'NR == FNR { shared[$1] = 1; next } ($1 in shared)' "${WORK}/codex-names.txt" "${WORK}/atomic.txt" \
    | comm -23 - "${WORK}/codex.txt" >"${WORK}/differs.txt"

status=0
while read -r name version; do
    if ! grep -qxF "${name} ${version}" "${WORK}/allowed.txt"; then
        echo "scripts/lock-check.sh: ${name} ${version} is not in Codex's lockfile at ${REV}; pin it to Codex's version or list it in deps/lock-exceptions.toml" >&2
        status=1
    fi
done <"${WORK}/differs.txt"
while read -r name version; do
    if ! grep -qxF "${name} ${version}" "${WORK}/differs.txt"; then
        echo "scripts/lock-check.sh: deps/lock-exceptions.toml lists ${name} ${version}, which no longer differs; remove it" >&2
        status=1
    fi
done <"${WORK}/allowed.txt"

[ "${status}" -eq 0 ] && echo "lockfile: $(wc -l <"${WORK}/differs.txt") listed differences from Codex ${REV}, none unlisted"
exit "${status}"
