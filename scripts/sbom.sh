#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Write the CycloneDX SBOM for the atomic binary and check that it lists every
# fork in deps/forks.toml at its pinned revision.
#
#   scripts/sbom.sh            # writes target/sbom/atomic-agentic.cdx.json
#
# Needs cargo-cyclonedx (in the development image).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${ROOT}/target/sbom"
mkdir -p "${OUT}"

(cd "${ROOT}/crates/atomic" && cargo cyclonedx --format json --spec-version 1.5 >/dev/null 2>&1)
mv "${ROOT}/crates/atomic/atomic-agentic.cdx.json" "${OUT}/atomic-agentic.cdx.json"
SBOM="${OUT}/atomic-agentic.cdx.json"

status=0
checked=0
while IFS='|' read -r repo rev crates; do
    # cargo-cyclonedx records a git source as vcs_url=git+<repo>@<rev>, with
    # the + percent-encoded.
    want="vcs_url=git%2B${repo}%40${rev}"
    for crate in ${crates}; do
        if ! grep -q "\"pkg:cargo/${crate}@[^\"]*${want}\"" "${SBOM}"; then
            echo "scripts/sbom.sh: the SBOM does not list ${crate} from ${repo} at ${rev}" >&2
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
' "${ROOT}/deps/forks.toml")

[ "${status}" -eq 0 ] && echo "SBOM ${SBOM}: lists all ${checked} forked crates at their pinned revisions"
exit "${status}"
