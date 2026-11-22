#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Checks a built atomic binary's own Astro compiler entry, the one `main`
# dispatches (test binaries serve it before `main`, so no cargo test reaches
# this one): it compiles a one-line atom with an empty environment and no Deno,
# and the binary holds no test-only dispatch.
#
#   scripts/compiler-entry-check.sh target/debug/atomic
set -euo pipefail

BIN="${1:?usage: scripts/compiler-entry-check.sh <atomic binary>}"
if grep -qa "ATOMIC_COMPILER_TEST_DISPATCH" "${BIN}"; then
    echo "scripts/compiler-entry-check.sh: ${BIN} has the compiler-in-tests feature compiled in" >&2
    exit 1
fi
request='{"protocol":1,"action":"load","file":"/p/a.atom.astro","root":"/p","sources":{"/p/a.atom.astro":"---\n---\nHello from the embedded compiler"},"sourcePaths":{"/p/a.atom.astro":"/p/a.atom.astro"},"absent":[]}'
answer="$(printf '%s\n' "${request}" | env -i "${BIN}" __atomic-compiler)"
python3 -I -c '
import json, sys
answer = json.loads(sys.argv[1])
sources = answer.get("ok", {}).get("sources", {})
if sources.get("/p/a.atom.astro") != "---\n---\nHello from the embedded compiler":
    sys.exit(f"scripts/compiler-entry-check.sh: the compiler did not load the atom: {answer}")
' "${answer}"
echo "the compiler entry of ${BIN} loads an atom" >&2
