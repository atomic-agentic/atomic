#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# A suite that cannot reach its backend fails; it never skips. This runs each
# backend-dependent suite with its backend missing and checks that every one of
# its tests fails, with a message naming what is missing.
#
#   scripts/backends-check.sh postgres   # ATOMIC_TEST_DATABASE_URL unset, then unreachable
#   scripts/backends-check.sh sandbox    # run where Codex's Linux sandbox cannot start
#
# Run it as the tests run: inside scripts/dev.sh, or on a CI runner. The
# sandbox check needs an environment without the sandbox; under scripts/dev.sh
# that is the default (ATOMIC_DEV_SANDBOX unset), and CI turns user namespaces
# off for the step. It refuses to run where the sandbox works, since it could
# then show nothing. The cargo flags match CI's test step, so nothing rebuilds.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT}"
FEATURES="atomic-agentic/postgres-tests,atomic-agentic/sandbox-tests"
LOG="$(mktemp)"
trap 'rm -f "${LOG}"' EXIT
status=0

# check <what> <message> <cargo test arguments...>: the tests the arguments
# select must all fail, and each failure must carry <message>. With
# `expected` set, exactly that many must fail.
expected=""
check() {
    local what="$1" message="$2"
    shift 2
    cargo test --locked --no-fail-fast "$@" >"${LOG}" 2>&1
    local code=$?
    local passed failed named
    passed="$(awk '/^test result:/ { p += $4 } END { print p + 0 }' "${LOG}")"
    failed="$(awk '/^test result:/ { f += $6 } END { print f + 0 }' "${LOG}")"
    # Each failing test prints its panic once: "thread '<test>' ... panicked at".
    named="$(grep -A3 "panicked at" "${LOG}" | grep -c -F "${message}")"
    if [ "${code}" -eq 0 ] || [ "${passed}" -ne 0 ] || [ "${failed}" -eq 0 ] ||
        [ "${named}" -lt "${failed}" ] ||
        { [ -n "${expected}" ] && [ "${failed}" -ne "${expected}" ]; }; then
        echo "FAIL ${what}: exit ${code}, ${passed} passed, ${failed} failed${expected:+ of ${expected}}, ${named} naming '${message}'" >&2
        grep -E "^test .* (ok|FAILED)$|panicked at" "${LOG}" | head -40 >&2
        grep -m3 -E "^error" "${LOG}" >&2
        status=1
    else
        echo "ok ${what}: all ${failed} tests failed, naming '${message}'" >&2
    fi
}

case "${1:-}" in
postgres)
    # Every Postgres variant has postgres in its name; the filter selects
    # them across the workspace.
    ATOMIC_TEST_DATABASE_URL="" check "Postgres suites with no database configured" \
        "ATOMIC_TEST_DATABASE_URL is not set" \
        --workspace --features "${FEATURES}" -- postgres
    # Port 9 (discard) on loopback: nothing listens there.
    ATOMIC_TEST_DATABASE_URL="postgres://atomic:atomic@127.0.0.1:9/atomic" \
        check "Postgres suites with the database unreachable" \
        "test database postgres://***@127.0.0.1:9/atomic (ATOMIC_TEST_DATABASE_URL) cannot be reached" \
        --workspace --features "${FEATURES}" -- postgres
    ;;
sandbox)
    if bwrap --unshare-user --unshare-pid --ro-bind / / --proc /proc --dev /dev true 2>/dev/null; then
        echo "scripts/backends-check.sh: the sandbox can start here, so its absence cannot be checked" >&2
        exit 2
    fi
    # Every test file behind sandbox-tests, found by its cfg: whole files,
    # and in a file whose other tests need no sandbox, each test marked
    # `// needs_sandbox` above its attributes.
    suites=()
    found=()
    for file in crates/*/tests/*.rs; do
        grep -q 'feature = "sandbox-tests"' "${file}" || continue
        name="$(basename "${file}" .rs)"
        if grep -q '^#!\[cfg(all(target_os = "linux", feature = "sandbox-tests"))\]' "${file}"; then
            suites+=(--test "${name}")
            found+=("${name}")
            continue
        fi
        mapfile -t tests < <(awk '
            /^[[:space:]]*\/\/ needs_sandbox/ { marked = 1; next }
            marked && /^[[:space:]]*(pub(\([a-z]+\))? )?(async )?fn [a-z0-9_]+\(/ {
                sub(/^.*fn /, ""); sub(/\(.*/, ""); print; marked = 0
            }
        ' "${file}")
        if [ "${#tests[@]}" -eq 0 ]; then
            echo "scripts/backends-check.sh: ${file} names sandbox-tests but marks no test needs_sandbox" >&2
            status=1
            continue
        fi
        # Each marked test, and no other, fails.
        expected="${#tests[@]}"
        check "the ${#tests[@]} sandbox tests of ${name} with no sandbox" \
            "the sandbox tests need Codex's Linux sandbox" \
            --workspace --features "${FEATURES}" --test "${name}" -- --exact "${tests[@]}"
        expected=""
    done
    [ "${#suites[@]}" -gt 0 ] || { echo "scripts/backends-check.sh: no sandbox suite found" >&2; exit 1; }
    check "sandbox suites (${found[*]}) with no sandbox" \
        "the sandbox tests need Codex's Linux sandbox" \
        --workspace --features "${FEATURES}" "${suites[@]}"
    ;;
*)
    echo "usage: scripts/backends-check.sh postgres|sandbox" >&2
    exit 2
    ;;
esac
exit "${status}"
