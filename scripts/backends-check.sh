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
# then show nothing. The sandbox half uses CI's test step's cargo flags, so
# nothing rebuilds; the Postgres half rebuilds atomic-agentic-durable alone.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT}"
FEATURES="atomic-agentic/postgres-tests,atomic-agentic/sandbox-tests"
LOG="$(mktemp)"
FAILING="$(mktemp)"
trap 'rm -f "${LOG}" "${FAILING}"' EXIT
AUDIT="${ROOT}/scripts/backends-check-audit.py"
status=0

# check <what> <message> <cargo test arguments...>: the tests the arguments
# select must all fail, each with <message> in its own output, and every test
# binary must report its result (see backends-check-audit.py).
check() {
    local what="$1" message="$2"
    shift 2
    cargo test --locked --no-fail-fast "$@" >"${LOG}" 2>&1
    if out="$(python3 "${AUDIT}" all-fail "${LOG}" "${message}")"; then
        echo "ok ${what}:" >&2
        echo "${out}" >&2
    else
        echo "FAIL ${what}:" >&2
        echo "${out}" >&2
        grep -m3 -E "^error" "${LOG}" >&2
        status=1
    fi
}

case "${1:-}" in
postgres)
    # The Postgres tests are found by what they do, not by their names: run
    # every atomic-agentic-durable test (the package whose tests reach the
    # database, as crates/atomic-durable/tests/test_database.rs checks) with
    # no database. The tests that fail are the Postgres tests: each must fail
    # naming the database, and they must be exactly the Postgres variants,
    # whose names say postgres; none of those may pass or be ignored.
    ATOMIC_TEST_DATABASE_URL="" cargo test --locked --no-fail-fast -p atomic-agentic-durable \
        --features postgres-tests >"${LOG}" 2>&1
    if out="$(python3 "${AUDIT}" discover "${LOG}" "ATOMIC_TEST_DATABASE_URL is not set" "${FAILING}")"; then
        echo "ok Postgres tests with no database configured:" >&2
        echo "${out}" >&2
        mapfile -t failing <"${FAILING}"
        # Port 9 (discard) on loopback: nothing listens there.
        ATOMIC_TEST_DATABASE_URL="postgres://atomic:atomic@127.0.0.1:9/atomic" \
            check "the same tests with the database unreachable" \
            "test database postgres://***@127.0.0.1:9/atomic (ATOMIC_TEST_DATABASE_URL) cannot be reached" \
            -p atomic-agentic-durable --features postgres-tests -- --exact "${failing[@]}"
    else
        echo "FAIL Postgres tests with no database configured:" >&2
        echo "${out}" >&2
        grep -m3 -E "^error" "${LOG}" >&2
        status=1
    fi
    ;;
sandbox)
    if bwrap --unshare-user --unshare-pid --ro-bind / / --proc /proc --dev /dev true 2>/dev/null; then
        echo "scripts/backends-check.sh: the sandbox can start here, so its absence cannot be checked" >&2
        exit 2
    fi
    # Every test file behind sandbox-tests, found by its cfg.
    suites=()
    found=()
    for file in crates/*/tests/*.rs; do
        if grep -q 'feature = "sandbox-tests"' "${file}"; then
            name="$(basename "${file}" .rs)"
            suites+=(--test "${name}")
            found+=("${name}")
        fi
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
