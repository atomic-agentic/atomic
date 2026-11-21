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
# then show nothing. The sandbox half and the Postgres half's workspace pass
# use CI's test build's cargo flags, so nothing rebuilds for them; the
# Postgres half's pass over atomic-agentic-durable alone rebuilds that
# package with its own features.
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
# binary must report its result (see backends-check-audit.py). With
# `expected` set, exactly that many must fail.
expected=""
check() {
    local what="$1" message="$2"
    shift 2
    cargo test --color never --locked --no-fail-fast "$@" >"${LOG}" 2>&1
    local failed=""
    if out="$(python3 "${AUDIT}" all-fail "${LOG}" "${message}")" &&
        failed="$(sed -n 's/^  \([0-9][0-9]*\) tests failed.*/\1/p' <<<"${out}")" &&
        { [ -z "${expected}" ] || [ "${failed}" = "${expected}" ]; }; then
        echo "ok ${what}:" >&2
        echo "${out}" >&2
    else
        echo "FAIL ${what}:${expected:+ ${failed:-no} tests failed of the ${expected} marked}" >&2
        echo "${out}" >&2
        grep -m3 -E "^error" "${LOG}" >&2
        status=1
    fi
}

case "${1:-}" in
postgres)
    # In atomic-agentic-durable, the Postgres tests are found by what they
    # do, not by their names: run every test of that package with no
    # database. The tests that fail are the Postgres tests: each must fail
    # naming the database, and they must be exactly the Postgres variants,
    # whose names say postgres; none of those may pass or be ignored.
    ATOMIC_TEST_DATABASE_URL="" cargo test --color never --locked --no-fail-fast -p atomic-agentic-durable \
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
    # The command-line tests in crates/atomic/tests/run_cli.rs, and one in
    # crates/atomic/tests/failpoints.rs, also use the database. That
    # package's tests are too many and too slow to run wholesale without it,
    # so its Postgres tests are selected by name, as their names say
    # postgres; test_database.rs keeps the users of the database to these
    # places.
    ATOMIC_TEST_DATABASE_URL="" check "Postgres tests across the workspace, by name, with no database configured" \
        "ATOMIC_TEST_DATABASE_URL is not set" \
        --workspace --features "${FEATURES}" -- postgres
    ATOMIC_TEST_DATABASE_URL="postgres://atomic:atomic@127.0.0.1:9/atomic" \
        check "Postgres tests across the workspace, by name, with the database unreachable" \
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
