#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Check that every registry crate atomic shares with Codex resolves to a
# version Codex's own Cargo.lock has at the pinned revision, unless
# deps/lock-exceptions.toml lists the difference with a reason, and that
# Cargo.lock holds at most one built V8 version (deps/v8.toml).
#
#   scripts/lock-check.sh                 # both checks
#   scripts/lock-check.sh v8 [LOCKFILE]   # only the V8 check, of LOCKFILE
#   scripts/lock-check.sh v8-built        # each V8 version deps/v8.toml lists
#                                         # as unbuilt stays out of the build
#                                         # graph (needs cargo: scripts/dev.sh)
#
# Codex's lockfile is read from CODEX_SRC (a clone of the Codex repository deps/codex.toml names) when set,
# otherwise downloaded at the pinned revision.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# The tests (scripts/lock-check-test.sh) point this at their own list.
V8_TOML="${LOCK_CHECK_V8_TOML:-${ROOT}/deps/v8.toml}"

# "crate version" for each [[unbuilt]] entry in deps/v8.toml.
v8_unbuilt() {
    awk '
        /^\[\[unbuilt\]\]/ { crate = "" }
        /^crate = / { gsub(/"/, "", $3); crate = $3 }
        /^version = / && crate != "" { gsub(/"/, "", $3); print crate, $3 }
    ' "${V8_TOML}" | sort -u
}

# Fails when LOCKFILE holds two V8 builds that are not listed as unbuilt. A
# build is a V8 binding crate (v8, or deno_v8's alternative v8x) at one
# version; entries under [[patch.unused]] are not in the graph.
v8_check() {
    local lockfile="$1" builds unbuilt built status=0
    builds="$(awk '
        /^\[\[/ { in_package = ($0 == "[[package]]"); name = "" }
        in_package && /^name = / { gsub(/"/, "", $3); name = $3 }
        in_package && /^version = / && (name == "v8" || name == "v8x") { gsub(/"/, "", $3); print name, $3 }
    ' "${lockfile}" | sort -u)"
    unbuilt="$(v8_unbuilt)"
    built="$(comm -23 <(printf '%s\n' "${builds}" | sed '/^$/d') <(printf '%s\n' "${unbuilt}" | sed '/^$/d') \
        | sort -u)"
    if [ "$(printf '%s\n' "${built}" | sed '/^$/d' | wc -l)" -gt 1 ]; then
        echo "scripts/lock-check.sh: ${lockfile} holds more than one V8 build, and two V8 builds cannot be linked into one binary:" >&2
        printf '%s\n' "${builds}" | sed 's/^/    /' >&2
        echo "  converge on one version, or list a locked-but-unbuilt version in deps/v8.toml (see deps/README.md)" >&2
        status=1
    fi
    while read -r name version; do
        [ -n "${name}" ] || continue
        if ! printf '%s\n' "${builds}" | grep -qxF "${name} ${version}"; then
            echo "scripts/lock-check.sh: deps/v8.toml lists ${name} ${version}, which ${lockfile} no longer holds; remove it" >&2
            status=1
        fi
    done <<<"${unbuilt}"
    [ "${status}" -eq 0 ] && echo "V8: $(printf '%s\n' "${built}" | sed '/^$/d' | wc -l) built version(s) in ${lockfile}, $(printf '%s\n' "${unbuilt}" | sed '/^$/d' | wc -l) locked but unbuilt"
    return "${status}"
}

# Proves each version deps/v8.toml lists as unbuilt is outside the graph cargo
# builds, for every workspace feature and target.
v8_built() {
    local status=0 output
    while read -r name version; do
        [ -n "${name}" ] || continue
        if output="$(cargo tree --manifest-path "${ROOT}/Cargo.toml" --locked --workspace --all-features --target all \
            -e normal,build,dev -i "${name}@${version}" 2>&1)"; then
            echo "scripts/lock-check.sh: deps/v8.toml lists ${name} ${version} as unbuilt, but the build graph includes it:" >&2
            printf '%s\n' "${output}" | sed 's/^/    /' >&2
            status=1
        elif ! grep -q "did not match any packages" <<<"${output}"; then
            echo "scripts/lock-check.sh: cargo tree failed for ${name} ${version}:" >&2
            printf '%s\n' "${output}" | sed 's/^/    /' >&2
            status=1
        fi
    done <<<"$(v8_unbuilt)"
    [ "${status}" -eq 0 ] && echo "V8: every version deps/v8.toml lists as unbuilt stays out of the build graph"
    return "${status}"
}

case "${1:-}" in
    v8) v8_check "${2:-${ROOT}/Cargo.lock}"; exit ;;
    v8-built) v8_built; exit ;;
    '') ;;
    *) echo "usage: scripts/lock-check.sh [v8 [LOCKFILE] | v8-built]" >&2; exit 2 ;;
esac
REV="$(sed -n 's/^rev = "\(.*\)"$/\1/p' "${ROOT}/deps/codex.toml" | head -n1)"
REPOSITORY="$(sed -n 's/^repository = "\(.*\)"$/\1/p' "${ROOT}/deps/codex.toml" | head -n1)"
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT

if [ -n "${CODEX_SRC:-}" ]; then
    git -C "${CODEX_SRC}" show "${REV}:codex-rs/Cargo.lock" >"${WORK}/codex.lock"
else
    curl -fsSL --retry 3 -o "${WORK}/codex.lock" "https://raw.githubusercontent.com/${REPOSITORY#https://github.com/}/${REV}/codex-rs/Cargo.lock"
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
v8_check "${ROOT}/Cargo.lock" || status=1
exit "${status}"
