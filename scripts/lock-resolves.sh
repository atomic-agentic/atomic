#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Resolve atomic's dependency graph from scratch, ignoring the committed
# Cargo.lock, and check that it still lands on the libsqlite3-sys Codex locks
# at the pinned revision. Codex links SQLite through that crate and the durare
# fork does too, and Cargo allows one crate to link a native library, so the
# graph resolves only if both accept the same version.
#
#   scripts/lock-resolves.sh
#
# It works in a copy of the committed tree and changes nothing here. It needs
# cargo and the network (crates.io and the pinned git sources). Codex's
# lockfile is read from CODEX_SRC (a clone of openai/codex) when set,
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

# The versions of a package a lockfile holds, one per line.
versions() {
    awk -v want="$2" '
        /^\[\[package\]\]/ { name = "" }
        /^name = / { gsub(/"/, "", $3); name = $3 }
        /^version = / && name == want { gsub(/"/, "", $3); print $3 }
    ' "$1" | sort -u
}

expected="$(versions "${WORK}/codex.lock" libsqlite3-sys)"
[ -n "${expected}" ] || { echo "scripts/lock-resolves.sh: Codex ${REV:0:9} locks no libsqlite3-sys" >&2; exit 1; }

mkdir "${WORK}/tree"
git -C "${ROOT}" archive HEAD | tar -x -C "${WORK}/tree"
rm -f "${WORK}/tree/Cargo.lock"
(cd "${WORK}/tree" && cargo generate-lockfile)
resolved="$(versions "${WORK}/tree/Cargo.lock" libsqlite3-sys)"

if [ "${resolved}" != "${expected}" ]; then
    echo "scripts/lock-resolves.sh: a fresh resolution gives libsqlite3-sys ${resolved//$'\n'/, }, Codex ${REV:0:9} locks ${expected}" >&2
    exit 1
fi
echo "lock-resolves: a fresh resolution gives libsqlite3-sys ${resolved}, as Codex ${REV:0:9} locks"
