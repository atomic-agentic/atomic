#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Tests for scripts/acceptance.py, against hand-written logs and a stand-in
# for cargo. No Rust toolchain is needed.
#
#   scripts/acceptance-test.sh
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)" || exit 1
trap 'rm -rf "${WORK}"' EXIT
CHECK="${ROOT}/scripts/acceptance.py"
failures=0

fail() {
    echo "FAIL $*" >&2
    failures=$((failures + 1))
}

cat >"${WORK}/list.txt" <<'EOF'
# A comment line.
## A cap parks the run.
budgets a_cap_parks
atomic_core budget::tests::unpriced

## Cases in JSONL run the same.
atomic_core eval::tests::jsonl
EOF

# A log of the run: the binaries cargo ran, each with its results.
log() {
    cat <<EOF
     Running unittests src/lib.rs (target/debug/deps/atomic_core-0123456789abcdef)
test budget::tests::unpriced ... ${1:-ok}
test eval::tests::jsonl ... ${2:-ok}
test result: ok. 2 passed; 0 failed
     Running tests/budgets.rs (/work/target/debug/deps/budgets-fedcba9876543210)
test ${3:-a_cap_parks} ... ok
test result: ok. 1 passed; 0 failed
EOF
}

# expect <exit code> <text the report must hold> <acceptance.py arguments...>
expect() {
    local code="$1" text="$2"
    shift 2
    local out got
    out="$(python3 "${CHECK}" m0 --list "${WORK}/list.txt" "$@" 2>&1)"
    got=$?
    if [ "${got}" -ne "${code}" ]; then
        fail "acceptance.py $* exited ${got}, not ${code}: ${out}"
    elif ! grep -qF -- "${text}" <<<"${out}"; then
        fail "acceptance.py $* does not say '${text}': ${out}"
    fi
}

log >"${WORK}/ok.log"
expect 0 "M0 acceptance: passed" --log "${WORK}/ok.log"

log FAILED >"${WORK}/failed.log"
expect 1 "atomic_core budget::tests::unpriced: FAILED" --log "${WORK}/failed.log"

log ok ignored >"${WORK}/ignored.log"
expect 1 "eval::tests::jsonl: ignored" --log "${WORK}/ignored.log"

# Renamed: the listed test no longer runs.
log ok ok a_cap_parks_renamed >"${WORK}/renamed.log"
expect 1 "budgets a_cap_parks: not run" --log "${WORK}/renamed.log"

# The same name in another binary does not stand in for it.
{
    log ok ok a_cap_parks_renamed
    echo "     Running tests/pipeline.rs (target/debug/deps/pipeline-00112233445566aa)"
    echo "test a_cap_parks ... ok"
} >"${WORK}/elsewhere.log"
expect 1 "not run (ran only in pipeline)" --log "${WORK}/elsewhere.log"

# A clause with no test fails.
printf '## A clause.\nbudgets a_cap_parks\n## A clause with none.\n' >"${WORK}/empty.txt"
out="$(python3 "${CHECK}" m0 --list "${WORK}/empty.txt" --log "${WORK}/ok.log" 2>&1)"
if [ $? -ne 1 ] || ! grep -qF "no test listed" <<<"${out}"; then
    fail "a clause with no test passed: ${out}"
fi

# A malformed line fails before anything runs.
printf '## A clause.\nbudgets\n' >"${WORK}/malformed.txt"
out="$(python3 "${CHECK}" m0 --list "${WORK}/malformed.txt" --log "${WORK}/ok.log" 2>&1)"
if [ $? -ne 1 ] || ! grep -qF "expected \`<binary> <test path>\`" <<<"${out}"; then
    fail "a malformed line passed: ${out}"
fi

# Run through a stand-in cargo: it is asked for every listed test, exactly,
# and its failure fails the check even when every listed test passed.
mkdir -p "${WORK}/bin"
cat >"${WORK}/bin/cargo" <<SH
#!/usr/bin/env bash
echo "\$*" >"${WORK}/args"
cat "${WORK}/ok.log"
exit "\${FAKE_CARGO_EXIT:-0}"
SH
chmod +x "${WORK}/bin/cargo"
PATH="${WORK}/bin:${PATH}" expect 0 "M0 acceptance: passed"
want="test --workspace --locked --no-fail-fast --features atomic-agentic/postgres-tests,atomic-agentic/sandbox-tests --tests -- --exact a_cap_parks budget::tests::unpriced eval::tests::jsonl"
if [ "$(cat "${WORK}/args" 2>/dev/null)" != "${want}" ]; then
    fail "cargo was run as '$(cat "${WORK}/args" 2>/dev/null)', not '${want}'"
fi
echo "error: could not compile \`atomic-agentic\`" >>"${WORK}/ok.log"
PATH="${WORK}/bin:${PATH}" FAKE_CARGO_EXIT=101 expect 1 "cargo test exited 101"

if [ "${failures}" -ne 0 ]; then
    echo "${failures} acceptance.py test(s) failed" >&2
    exit 1
fi
echo "acceptance.py: all tests passed"
