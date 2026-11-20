#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Tests for scripts/acceptance.py, against hand-written logs and a stand-in
# for cargo. No Rust toolchain is needed.
#
#   scripts/acceptance-test.sh
set -uo pipefail

# The cases below write job summaries of their own, never CI's.
unset GITHUB_STEP_SUMMARY
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)" || exit 1
trap 'rm -rf "${WORK}"' EXIT
CHECK="${ROOT}/scripts/acceptance.py"
failures=0

fail() {
    echo "FAIL $*" >&2
    failures=$((failures + 1))
}

# A milestone with three clauses, and a list of them.
cat >"${WORK}/milestones.md" <<'EOF'
# Milestones

## M0: A milestone

**Acceptance:**

- A cap parks the run.
- Cases in JSONL run the same, written
  over two lines.
- A scorer fails CI.

## M1: The next one

**Acceptance:**

- Something else.
EOF
cat >"${WORK}/list.txt" <<'EOF'
# A comment line.
## A cap parks the run.
budgets a_cap_parks
atomic_core budget::tests::unpriced

## Cases in JSONL run the same, written over two lines.
atomic_core eval::tests::jsonl

## A scorer fails CI.
pending: no judge yet
atomic_core eval::tests::scorer
EOF

# A log of the run: the binaries cargo ran, each with its results.
log() {
    cat <<EOF
     Running unittests src/lib.rs (target/debug/deps/atomic_core-0123456789abcdef)
test budget::tests::unpriced ... ${1:-ok}
test eval::tests::jsonl ... ${2:-ok}
test eval::tests::scorer ... ok
test result: ok. 3 passed; 0 failed
     Running tests/budgets.rs (/work/target/debug/deps/budgets-fedcba9876543210)
test ${3:-a_cap_parks} ... ok
test result: ok. 1 passed; 0 failed
EOF
}

# check <exit code> <text the output must hold> <acceptance.py arguments...>
check() {
    local code="$1" text="$2"
    shift 2
    local out got
    out="$(python3 "${CHECK}" m0 --milestones "${WORK}/milestones.md" --list "${WORK}/list.txt" "$@" 2>&1)"
    got=$?
    if [ "${got}" -ne "${code}" ]; then
        fail "acceptance.py $* exited ${got}, not ${code}: ${out}"
    elif ! grep -qF -- "${text}" <<<"${out}"; then
        fail "acceptance.py $* does not say '${text}': ${out}"
    fi
}

log >"${WORK}/ok.log"
# A pending clause whose tests pass is named in the verdict, not passed
# outright.
check 0 "M0 acceptance: passed, 1 clause pending: no judge yet" --log "${WORK}/ok.log"
check 0 "pending A scorer fails CI. (1 test)" --log "${WORK}/ok.log"

# A coloured log, as cargo writes one under CARGO_TERM_COLOR=always, reads
# the same.
sed -e 's/^\( *\)Running/\x1b[1m\x1b[92m\1Running\x1b[0m/' \
    -e 's/ \.\.\. ok$/ ... \x1b[32mok\x1b[0m/' "${WORK}/ok.log" >"${WORK}/colour.log"
grep -q $'\x1b\\[92m' "${WORK}/colour.log" || fail "the coloured log has no colour"
check 0 "M0 acceptance: passed, 1 clause pending" --log "${WORK}/colour.log"
# So does a job's log as GitHub serves it, each line after its time.
sed 's/^/2026-11-20T10:00:00.1234567Z /' "${WORK}/colour.log" >"${WORK}/job.log"
check 0 "M0 acceptance: passed, 1 clause pending" --log "${WORK}/job.log"

log FAILED >"${WORK}/failed.log"
check 1 "atomic_core budget::tests::unpriced: FAILED" --log "${WORK}/failed.log"
# A failure shows what cargo printed, folded.
check 1 "::group::cargo test output" --log "${WORK}/failed.log"

log ok ignored >"${WORK}/ignored.log"
check 1 "eval::tests::jsonl: ignored" --log "${WORK}/ignored.log"

# Renamed: the listed test no longer runs.
log ok ok a_cap_parks_renamed >"${WORK}/renamed.log"
check 1 "budgets a_cap_parks: not run" --log "${WORK}/renamed.log"

# The same name in another binary does not stand in for it.
{
    log ok ok a_cap_parks_renamed
    echo "     Running tests/pipeline.rs (target/debug/deps/pipeline-00112233445566aa)"
    echo "test a_cap_parks ... ok"
} >"${WORK}/elsewhere.log"
check 1 "not run (ran only in pipeline)" --log "${WORK}/elsewhere.log"

# A log in which cargo failed fails, though every listed test passed.
{ log; printf '\x1b[1m\x1b[91merror\x1b[0m: could not compile `atomic-agentic`\n'; } >"${WORK}/broken.log"
check 1 "error: could not compile" --log "${WORK}/broken.log"

# The list must state the milestone's clauses: one dropped, one not the
# milestone's, and one with no test each fail before anything runs.
list_check() {
    local name="$1" text="$2" milestones="${3:-${WORK}/milestones.md}"
    out="$(python3 "${CHECK}" m0 --milestones "${milestones}" --list "${WORK}/${name}" --log "${WORK}/ok.log" 2>&1)"
    if [ $? -ne 1 ] || ! grep -qF -- "${text}" <<<"${out}"; then
        fail "${name} passed, or does not say '${text}': ${out}"
    fi
}
grep -v "^## A scorer fails CI.$" "${WORK}/list.txt" | grep -v "^pending:" >"${WORK}/dropped.txt"
list_check dropped.txt "M0's clause is not listed: A scorer fails CI."
sed 's/^## A cap parks the run\.$/## A cap parks the workflow./' "${WORK}/list.txt" >"${WORK}/reworded.txt"
list_check reworded.txt "listed, but not one of M0's clauses: A cap parks the workflow."
{ cat "${WORK}/list.txt"; printf '## Something else.\n'; } >"${WORK}/extra.txt"
sed 's/^- A scorer fails CI\.$/- A scorer fails CI.\n- Something else./' "${WORK}/milestones.md" >"${WORK}/four.md"
list_check extra.txt "no test listed" "${WORK}/four.md"
printf '## A cap parks the run.\nbudgets\n' >"${WORK}/malformed.txt"
list_check malformed.txt "expected \`<binary> <test path>\`"
printf '## A cap parks the run.\npending:\nbudgets a_cap_parks\n' >"${WORK}/no-reason.txt"
list_check no-reason.txt "\`pending:\` needs a reason"

# The repository's own lists state their milestones' clauses.
for list in "${ROOT}"/scripts/acceptance/*.txt; do
    milestone="$(basename "${list}" .txt)"
    out="$(python3 "${CHECK}" "${milestone}" --check 2>&1)" || fail "${list}: ${out}"
done

# The features it runs with are CI's test step's, so nothing rebuilds.
ci="$(grep -o 'cargo test --workspace --locked --no-fail-fast --features [^ ]*' "${ROOT}/.github/workflows/ci.yml" | head -1 | awk '{print $NF}')"
ours="$(python3 -c "import sys; sys.path.insert(0, '${ROOT}/scripts'); import acceptance; print(acceptance.FEATURES)")"
[ -n "${ci}" ] && [ "${ci}" = "${ours}" ] || fail "acceptance.py's features (${ours}) are not CI's test step's (${ci})"

# A job summary goes where GITHUB_STEP_SUMMARY says, and holds the report.
GITHUB_STEP_SUMMARY="${WORK}/summary.md" check 0 "passed" --log "${WORK}/ok.log"
grep -qF "M0 acceptance: passed, 1 clause pending: no judge yet" "${WORK}/summary.md" 2>/dev/null ||
    fail "the job summary does not hold the report: $(cat "${WORK}/summary.md" 2>/dev/null)"

# Run through a stand-in cargo: it is asked for every listed test, exactly,
# without colour, and its failure fails the check even when every listed
# test passed.
mkdir -p "${WORK}/bin"
cat >"${WORK}/bin/cargo" <<SH
#!/usr/bin/env bash
echo "\$*" >"${WORK}/args"
cat "${WORK}/ok.log"
exit "\${FAKE_CARGO_EXIT:-0}"
SH
chmod +x "${WORK}/bin/cargo"
PATH="${WORK}/bin:${PATH}" check 0 "M0 acceptance: passed"
want="test --workspace --locked --no-fail-fast --color never --features ${ours} --tests -- --exact a_cap_parks budget::tests::unpriced eval::tests::jsonl eval::tests::scorer"
if [ "$(cat "${WORK}/args" 2>/dev/null)" != "${want}" ]; then
    fail "cargo was run as '$(cat "${WORK}/args" 2>/dev/null)', not '${want}'"
fi
PATH="${WORK}/bin:${PATH}" FAKE_CARGO_EXIT=101 check 1 "cargo test failed, exiting 101"

if [ "${failures}" -ne 0 ]; then
    echo "${failures} acceptance.py test(s) failed" >&2
    exit 1
fi
echo "acceptance.py: all tests passed"
