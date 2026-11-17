#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Check that every crate deps/forks.toml lists resolves in Cargo.lock from the
# fork's repository at its pinned revision.
#
#   scripts/forks-check.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FORKS="${ROOT}/deps/forks.toml"
LOCK="${ROOT}/Cargo.lock"

# The source each package resolves from, as "name source" lines.
sources="$(awk '
    /^\[\[package\]\]/ { name = ""; source = "" }
    /^name = / { gsub(/"/, "", $3); name = $3 }
    /^source = / { gsub(/"/, "", $3); source = $3; if (name != "") print name, source }
' "${LOCK}")"

status=0
checked=0
# Each fork's repository, revision and crates, one fork per line.
while IFS='|' read -r repo rev crates; do
    for crate in ${crates}; do
        want="git+${repo}?rev=${rev}#${rev}"
        got="$(printf '%s\n' "${sources}" | awk -v n="${crate}" '$1 == n { print $2 }')"
        if [ -z "${got}" ]; then
            echo "scripts/forks-check.sh: ${crate} is not in Cargo.lock" >&2
            status=1
        elif [ "$(printf '%s\n' "${got}" | sort -u)" != "${want}" ]; then
            echo "scripts/forks-check.sh: ${crate} resolves from ${got}, but deps/forks.toml pins ${want}" >&2
            status=1
        fi
        checked=$((checked + 1))
    done
done < <(awk '
    /^\[\[fork\]\]/ { if (repo != "") print repo "|" rev "|" crates; repo = ""; rev = ""; crates = "" }
    /^repository = / { gsub(/"/, "", $3); repo = $3 }
    /^rev = / { gsub(/"/, "", $3); rev = $3 }
    /^crates = / { line = $0; sub(/^crates = \[/, "", line); sub(/\].*$/, "", line); gsub(/[",]/, " ", line); crates = line }
    END { if (repo != "") print repo "|" rev "|" crates }
' "${FORKS}")

[ "${status}" -eq 0 ] && echo "forks: ${checked} crates resolve from their pinned fork revisions"
exit "${status}"
