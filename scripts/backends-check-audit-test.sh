#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Tests for scripts/backends-check-audit.py, on cargo test output written
# here, plain and coloured as CI's CARGO_TERM_COLOR=always colours it. No Rust
# toolchain is needed.
#
#   scripts/backends-check-audit-test.sh
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
AUDIT="${ROOT}/scripts/backends-check-audit.py"
WORK="$(mktemp -d)" || exit 1
trap 'rm -rf "${WORK}"' EXIT
failures=0
MSG="ATOMIC_TEST_DATABASE_URL is not set"

# A binary's section: binary NAME, then OUTCOME:TEST pairs; failing tests
# say MSG unless their outcome is FAILED-silent.
binary() {
    local name="$1"
    shift
    local p=0 f=0 i=0 t outcome
    printf '     Running %s (target/debug/deps/%s-0123)\n\nrunning %d tests\n' "${name}" "${name}" "$#"
    for t in "$@"; do
        outcome="${t%%:*}"
        case "${outcome}" in
        ok) p=$((p + 1)); printf 'test %s ... ok\n' "${t#*:}" ;;
        ignored) i=$((i + 1)); printf 'test %s ... ignored\n' "${t#*:}" ;;
        child) i=$((i + 1)); printf 'test %s ... ignored, run by the suite in a child process\n' "${t#*:}" ;;
        FAILED*) f=$((f + 1)); printf 'test %s ... FAILED\n' "${t#*:}" ;;
        esac
    done
    if [ "${f}" -gt 0 ]; then
        printf '\nfailures:\n\n'
        for t in "$@"; do
            case "${t%%:*}" in
            FAILED) printf -- '---- %s stdout ----\nthread panicked at x.rs:1:1:\n%s\n\n' "${t#*:}" "${MSG}" ;;
            FAILED-silent) printf -- '---- %s stdout ----\nthread panicked at x.rs:1:1:\nsomething else\n\n' "${t#*:}" ;;
            esac
        done
        printf 'failures:\n'
        for t in "$@"; do
            case "${t%%:*}" in FAILED*) printf '    %s\n' "${t#*:}" ;; esac
        done
        printf '\ntest result: FAILED. %d passed; %d failed; %d ignored; 0 measured; 0 filtered out\n\n' "${p}" "${f}" "${i}"
    else
        printf '\ntest result: ok. %d passed; %d failed; %d ignored; 0 measured; 0 filtered out\n\n' "${p}" "${f}" "${i}"
    fi
}

# Colour each line's start as cargo does.
colour() { sed -e 's/^\(\s*\)\(Running\|Doc-tests\)/\1\x1b[1m\x1b[32m\2\x1b[0m/' -e 's/^test result:/\x1b[32mtest result:\x1b[0m/'; }

# expect <0|1> <description> <mode> <log> [want text]
expect() {
    local want="$1" what="$2" mode="$3" log="$4" text="${5:-}"
    local out code
    out="$(python3 "${AUDIT}" "${mode}" "${log}" "${MSG}" "${WORK}/failing" 2>&1)"
    code=$?
    if [ "${code}" -ne "${want}" ]; then
        echo "FAIL ${what}: exit ${code}, wanted ${want}: ${out}" >&2
        failures=$((failures + 1))
    elif [ -n "${text}" ] && ! grep -qF "${text}" <<<"${out}"; then
        echo "FAIL ${what}: no '${text}' in: ${out}" >&2
        failures=$((failures + 1))
    fi
}

{ binary a FAILED:x_on_postgres FAILED:y_on_postgres; binary b FAILED:z_on_postgres; } >"${WORK}/fail.log"
expect 0 "every test failed, saying why" all-fail "${WORK}/fail.log"
colour <"${WORK}/fail.log" >"${WORK}/fail-colour.log"
grep -q $'\x1b' "${WORK}/fail-colour.log" || { echo "FAIL the coloured log has no escapes" >&2; failures=$((failures + 1)); }
expect 0 "the same, coloured" all-fail "${WORK}/fail-colour.log"

binary a FAILED:x_on_postgres ignored:y_on_postgres >"${WORK}/ignored.log"
expect 1 "an ignored test is no failure" all-fail "${WORK}/ignored.log" "y_on_postgres (a-0123: ignored)"
colour <"${WORK}/ignored.log" >"${WORK}/ignored-colour.log"
expect 1 "the same, coloured" all-fail "${WORK}/ignored-colour.log" "y_on_postgres (a-0123: ignored)"

# A child-process entry point, ignored by design, is not one of the tests.
binary a FAILED:x_on_postgres child:x_child >"${WORK}/child.log"
expect 0 "a child-process entry point is no test" all-fail "${WORK}/child.log"
# A Postgres variant ignored as a child is still a Postgres variant.
binary a FAILED:x_on_postgres child:y_on_postgres >"${WORK}/child-postgres.log"
expect 1 "a Postgres variant is never let off as a child" all-fail "${WORK}/child-postgres.log" "y_on_postgres (a-0123: child)"

binary a FAILED:x_on_postgres ok:y_on_postgres >"${WORK}/passed.log"
expect 1 "a passing test is no failure" all-fail "${WORK}/passed.log" "y_on_postgres (a-0123: ok)"

binary a FAILED:x_on_postgres FAILED-silent:y_on_postgres >"${WORK}/silent.log"
expect 1 "a failure must say why" all-fail "${WORK}/silent.log" "y_on_postgres (a-0123)"

# Two binaries with a test of one name: two tests, one of which passed.
{ binary a FAILED:same_on_postgres; binary b ok:same_on_postgres; } >"${WORK}/same.log"
expect 1 "tests are told apart by binary" all-fail "${WORK}/same.log" "same_on_postgres (b-0123: ok)"

# A binary whose own count disagrees with what it listed.
binary a FAILED:x_on_postgres | sed 's/1 failed; 0 ignored/1 failed; 1 ignored/' >"${WORK}/count.log"
expect 1 "a binary's counts must match its tests" all-fail "${WORK}/count.log" "reported 0 passed, 1 failed and 1 ignored"

# A binary that ran and never reported.
{ binary a FAILED:x_on_postgres; printf '     Running b (target/debug/deps/b-0123)\n\nrunning 1 tests\n'; } >"${WORK}/early.log"
expect 1 "a binary that ended early" all-fail "${WORK}/early.log" "reported no result"

# discover: the Postgres variants fail, the rest pass.
binary a FAILED:x_on_postgres ok:x_on_sqlite >"${WORK}/discover.log"
colour <"${WORK}/discover.log" >"${WORK}/discover-colour.log"
expect 0 "discover finds the Postgres variants" discover "${WORK}/discover-colour.log"
grep -qx "x_on_postgres" "${WORK}/failing" || { echo "FAIL discover did not write the failing test" >&2; failures=$((failures + 1)); }
binary a FAILED:x_on_postgres ignored:y_on_postgres ok:x_on_sqlite >"${WORK}/discover-ignored.log"
expect 1 "discover refuses an ignored Postgres variant" discover "${WORK}/discover-ignored.log" "y_on_postgres (a-0123)"

if [ "${failures}" -gt 0 ]; then
    echo "${failures} backends-check-audit tests failed" >&2
    exit 1
fi
echo "backends-check-audit tests passed" >&2
