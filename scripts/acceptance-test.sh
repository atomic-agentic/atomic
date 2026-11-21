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

# A pending clause whose test failed fails, pending or not.
sed 's/^test eval::tests::scorer \.\.\. ok$/test eval::tests::scorer ... FAILED/' "${WORK}/ok.log" >"${WORK}/pending-failed.log"
check 1 "atomic_core eval::tests::scorer: FAILED" --log "${WORK}/pending-failed.log"

# A test seen to fail stays failed, whatever a later line says of it.
{
    log FAILED
    echo "     Running unittests src/lib.rs (target/debug/deps/atomic_core-0123456789abcdef)"
    echo "test budget::tests::unpriced ... ok"
} >"${WORK}/twice.log"
check 1 "budget::tests::unpriced: FAILED" --log "${WORK}/twice.log"

# A downloaded log may start with a byte-order mark.
{ printf '\xef\xbb\xbf'; cat "${WORK}/ok.log"; } >"${WORK}/bom.log"
check 0 "M0 acceptance: passed, 1 clause pending" --log "${WORK}/bom.log"

# A clause of several sub-clauses lists each, in its words, with a test of its
# own.
printf '## M2: Sub-clauses\n\n**Acceptance:**\n\n- A cap parks the run; a child cannot spend twice.\n' >"${WORK}/sub.md"
subs() { printf '## A cap parks the run; a child cannot spend twice.\n%s' "$1" >"${WORK}/sub.txt"; }
subs_check() {
    local code="$1" text="$2" out
    out="$(python3 "${CHECK}" m2 --milestones "${WORK}/sub.md" --list "${WORK}/sub.txt" --log "${WORK}/ok.log" 2>&1)"
    if [ $? -ne "${code}" ] || ! grep -qF -- "${text}" <<<"${out}"; then
        fail "sub-clauses: expected exit ${code} and '${text}': ${out}"
    fi
}
subs $'### A cap parks the run\nbudgets a_cap_parks\n### a child cannot spend twice\natomic_core budget::tests::unpriced\n'
subs_check 0 "M2 acceptance: passed"
subs $'### A cap parks the run\nbudgets a_cap_parks\n'
subs_check 1 "not listed: a child cannot spend twice"
subs $'### A cap parks the run\nbudgets a_cap_parks\n### a child cannot spend twice\n'
subs_check 1 "no test listed for: a child cannot spend twice"
subs $'### A cap parks the run\nbudgets a_cap_parks\n### a child cannot spend at all\natomic_core budget::tests::unpriced\n'
subs_check 1 "not a sub-clause of"
subs $'budgets a_cap_parks\n### A cap parks the run\nbudgets a_cap_parks\n### a child cannot spend twice\natomic_core budget::tests::unpriced\n'
subs_check 1 "lists tests outside its sub-clauses"

# A list that says a thing twice is refused.
{ cat "${WORK}/list.txt"; printf 'pending: again\n'; } >"${WORK}/two-pending.txt"
list_check two-pending.txt "has a \`pending:\` line already"
{ cat "${WORK}/list.txt"; printf '## A cap parks the run.\nbudgets a_cap_parks\n'; } >"${WORK}/twice.txt"
list_check twice.txt "the clause is listed twice"

# Anything in the milestone's Acceptance block that is not a clause, or a
# clause's two-space continuation, is refused rather than skipped.
for odd in '* Another clause.' '1. Another clause.' ' - Another clause.' $'-\tAnother clause.' \
    'An unindented continuation.' '  - A nested bullet.'; do
    printf '## M0: A milestone\n\n**Acceptance:**\n\n- A cap parks the run.\n%s\n' "${odd}" >"${WORK}/odd.md"
    out="$(python3 "${CHECK}" m0 --milestones "${WORK}/odd.md" --list "${WORK}/list.txt" --check 2>&1)"
    if [ $? -ne 1 ] || ! grep -qF "not a clause or its continuation" <<<"${out}"; then
        fail "a line '${odd}' in the Acceptance block was not refused: ${out}"
    fi
done
# check <exit code> <text> <milestones file> <list file> <milestone>: the same
# for a milestones document of its own, as the shapes below write them.
list_check_against() {
    local code="$1" text="$2" milestones="$3" list="$4" id="$5" out got
    out="$(python3 "${CHECK}" "${id}" --milestones "${milestones}" --list "${list}" --check 2>&1)"
    got=$?
    if [ "${got}" -ne "${code}" ] || ! grep -qF -- "${text}" <<<"${out}"; then
        fail "${id}: expected exit ${code} and '${text}': ${out}"
    fi
}
# clauses_of <milestone> <document>: the clauses the reader states for the
# milestone in that document, one per line. A list that cannot be checked stops
# before the clauses are compared, so what was read of the document shows only
# by asking the reader itself.
clauses_of() {
    python3 -c "
import sys
sys.path.insert(0, '${ROOT}/scripts')
import acceptance
stated, _ = acceptance.milestone_clauses(sys.argv[1].upper(), sys.argv[2])
print('\n'.join(stated or []))
" "${1}" "${2}"
}

# Every Acceptance block of a milestone reads, a titled one among them: its
# clauses are those of all its blocks, in order.
printf '## M0: A milestone\n\n**Acceptance, extra:**\n\n- Another.\n\n**Acceptance:**\n\n- A cap parks the run.\n- Cases in JSONL run the same, written over two lines.\n- A scorer fails CI.\n' >"${WORK}/titled.md"
list_check_against 1 "M0's clause is not listed: Another." "${WORK}/titled.md" "${WORK}/list.txt" m0
{ cat "${WORK}/list.txt"; printf '## Another.\nbudgets a_cap_parks\n'; } >"${WORK}/titled-list.txt"
list_check_against 0 "lists M0's 4 clauses" "${WORK}/titled.md" "${WORK}/titled-list.txt" m0

# A bullet under a deeper heading inside the section is refused, not skipped:
# which block it belongs to would be a guess. Every shape of bullet, as inside
# a block.
for shape in '- Another clause.' '* Another clause.' '1. Another clause.' '  - Another clause.'; do
    printf '## M0: A milestone\n\n**Acceptance:**\n\n- A cap parks the run.\n\n#### Under a deeper heading\n\n%s\n' "${shape}" >"${WORK}/deeper.md"
    list_check_against 1 "a bullet under the deeper heading" "${WORK}/deeper.md" "${WORK}/list.txt" m0
done
# A block marker after a deeper heading starts a block of its own, so what is
# under it reads.
printf '## M0: A milestone\n\n#### Under a deeper heading\n\n**Acceptance, extra:**\n\n- A cap parks the run.\n- Cases in JSONL run the same, written over two lines.\n- A scorer fails CI.\n' >"${WORK}/after-deeper.md"
list_check_against 0 "lists M0's 3 clauses" "${WORK}/after-deeper.md" "${WORK}/list.txt" m0
# A bullet outside every block is the section's own prose, as the list under
# `**Delivers:**`, and is read as nothing.
printf '## M0: A milestone\n\n**Delivers:**\n\n- A cap parks the run, in prose.\n\n**Acceptance:**\n\n- A cap parks the run.\n- Cases in JSONL run the same, written over two lines.\n- A scorer fails CI.\n' >"${WORK}/prose.md"
list_check_against 0 "lists M0's 3 clauses" "${WORK}/prose.md" "${WORK}/list.txt" m0

# A split milestone states its own clauses, its "a" part its own, and its "b"
# part its parent's, since it finishes that work, with any of its own after
# them. Nothing of a part reads as its parent's, or the other way round.
cat >"${WORK}/split.md" <<'EOF'
## M4: Split

**Acceptance:**

- A cap parks the run.
- A scorer fails CI.

### M4a

**Acceptance:**

- A child cannot spend twice.
- A wait outlives a restart.

### M4b

**v1.0.** Finishes M4 and must pass its full acceptance.

**Acceptance, beyond the full M4 test:**

- Restarting twice costs nothing.
EOF
printf '## A cap parks the run.\nbudgets a_cap_parks\n## A scorer fails CI.\nbudgets a_cap_parks\n' >"${WORK}/m4.txt"
printf '## A child cannot spend twice.\nbudgets a_cap_parks\n## A wait outlives a restart.\nbudgets a_cap_parks\n' >"${WORK}/m4a.txt"
cat "${WORK}/m4.txt" >"${WORK}/m4b.txt"
printf '## Restarting twice costs nothing.\nbudgets a_cap_parks\n' >>"${WORK}/m4b.txt"
list_check_against 0 "lists M4's 2 clauses" "${WORK}/split.md" "${WORK}/m4.txt" m4
list_check_against 0 "lists M4A's 2 clauses" "${WORK}/split.md" "${WORK}/m4a.txt" m4a
list_check_against 0 "lists M4B's 3 clauses" "${WORK}/split.md" "${WORK}/m4b.txt" m4b
# A "b" part is asked for its parent's clauses as well as its own.
printf '## Restarting twice costs nothing.\nbudgets a_cap_parks\n' >"${WORK}/m4b-less.txt"
list_check_against 1 "M4B's clause is not listed: A cap parks the run." "${WORK}/split.md" "${WORK}/m4b-less.txt" m4b
# A line that only looks like an Acceptance block marker is refused with its
# line number, open block or not: everything under it would be read as nothing,
# and a "b" part, which is asked for its parent's clauses as well as its own,
# would be asked for none of the clauses it states itself.
for marker in '**Acceptance, beyond the full M4 test**:' '**Acceptance, beyond the full M4 test**' \
    '**acceptance, files and data:**' '**Acceptance'; do
    printf '## M4: Split\n\n**Acceptance:**\n\n- A cap parks the run.\n- A scorer fails CI.\n\n### M4b\n\n**v1.0.** Finishes M4.\n\n%s\n\n- Restarting twice costs nothing.\n' \
        "${marker}" >"${WORK}/near.md"
    list_check_against 1 "not an Acceptance block marker" "${WORK}/near.md" "${WORK}/m4b.txt" m4b
done
# The same for a line shaped like the marker in any other shape: no wrapper,
# the word misspelt or plural, a wrapper that is not bold, a bullet before it.
# Only a line beginning `**Acceptance` opens a block, so what sits under any of
# these is read as nothing, and outside a block such a line was skipped in
# silence.
for marker in 'Acceptance:' 'Acceptance, files and data:' 'acceptance:' \
    '__Acceptance:__' '**Acceptence:**' '**Acceptances:**' '- **Acceptance:**' \
    '- **Acceptance, beyond the full M4 test:**' '+ **Acceptance, files and data:**' \
    '1. **Acceptance, files and data:**' '2) **Acceptance:**'; do
    printf '## M4: Split\n\n**Acceptance:**\n\n- A cap parks the run.\n- A scorer fails CI.\n\n### M4b\n\n**v1.0.** Finishes M4.\n\n%s\n\n- Restarting twice costs nothing.\n' \
            "${marker}" >"${WORK}/shaped.md"
    out="$(python3 "${CHECK}" m4b --milestones "${WORK}/shaped.md" --list "${WORK}/m4b.txt" --check 2>&1)"
    if [ $? -ne 1 ] || ! grep -qF "not an Acceptance block marker" <<<"${out}"; then
        fail "a line shaped like a marker, '${marker}', was not refused: ${out}"
    fi
done
# A line shaped like the marker inside an open block is refused too, with its
# line number. A marker ends a block and this line opens none, so the clause
# under it joins the block above: the document says two blocks, the check would
# read one, which is why the line is refused and not skipped.
printf '## M0: A milestone\n\n**Acceptance:**\n\n- A cap parks the run.\n\n**Acceptence:**\n\n- A scorer fails CI.\n' >"${WORK}/inside.md"
out="$(python3 "${CHECK}" m0 --milestones "${WORK}/inside.md" --list "${WORK}/list.txt" --check 2>&1)"
if [ $? -ne 1 ] || ! grep -qF "inside.md:7: not an Acceptance block marker" <<<"${out}"; then
    fail "a mangled marker inside an open block was not refused with its line: ${out}"
fi
grep -qF "not a clause or its continuation" <<<"${out}" &&
    fail "a mangled marker inside an open block was refused as a clause: ${out}"
grep -qF "block that started at line 3" <<<"${out}" ||
    fail "a mangled marker inside an open block does not say which block it sits in: ${out}"
# Both of its bullets are what the reader states of such a document, which is the
# reading the refusal above stops: a milestone cannot be checked for fewer clauses
# than it states.
now_reads="$(clauses_of m0 "${WORK}/inside.md")"
[ "${now_reads}" = $'A cap parks the run.\nA scorer fails CI.' ] ||
    fail "a mangled marker inside an open block changed the clauses read: ${now_reads}"
# Hard-wrapped prose whose last line ends in a colon is shaped like the marker,
# so it is still refused. It is prose, so the refusal says what to do about it.
printf '## M0: A milestone\n\n**Delivers:** a runner that\naccepts one of three decisions:\n\n**Acceptance:**\n\n- A cap parks the run.\n- Cases in JSONL run the same, written over two lines.\n- A scorer fails CI.\n' >"${WORK}/wrapped.md"
list_check_against 1 "so rewrap it or reword it so it does not end in one" "${WORK}/wrapped.md" "${WORK}/list.txt" m0
# A fenced code block is example text, which states no heading, no marker and no
# clause: what it holds is this check's business no more than a milestone's prose
# is. A fence left open at the section's end is refused, since what it held and
# the sections after it went unread, and its own closer cannot be found.
printf '## M0: A milestone\n\n**Delivers:** a thing, as\n\n```markdown\n- Accepts one of three decisions:\n**Acceptance, files and data:**\n```\n\nand as YAML:\n\n```yaml\nacceptance_criteria:\n  - a cap parks the run\n```\n\n**Acceptance:**\n\n- A cap parks the run.\n- Cases in JSONL run the same, written over two lines.\n- A scorer fails CI.\n' >"${WORK}/fenced.md"
check 0 "M0 acceptance: passed, 1 clause pending" --milestones "${WORK}/fenced.md" --log "${WORK}/ok.log"
printf '## M0: A milestone\n\n**Acceptance:**\n\n- A cap parks the run.\n\n```yaml\nacceptance_criteria:\n' >"${WORK}/open-fence.md"
list_check_against 1 "ends inside a fenced code block" "${WORK}/open-fence.md" "${WORK}/list.txt" m0
# A heading of the milestone's own level that is about the acceptance ends the
# section where it sits, before any marker-shaped line is consulted, so what sits
# under it leaves the milestone with the heading. It is refused as the marker it
# was meant to be, and the milestone it truncates is named. A mangled spelling of
# the word is refused with it: a word whose letters hold the accept word is that
# word to this check.
for heading in '## Acceptance criteria' '## Aceptance criteria'; do
    printf '## M0: A milestone\n\n**Delivers:** a thing.\n%s\n\n- A cap parks the run.\n- A scorer fails CI.\n' \
            "${heading}" >"${WORK}/marker-heading.md"
    out="$(python3 "${CHECK}" m0 --milestones "${WORK}/marker-heading.md" --list "${WORK}/list.txt" --check 2>&1)"
    if [ $? -ne 1 ] || ! grep -qF "not an Acceptance block marker" <<<"${out}"; then
        fail "a heading '${heading}' that ends a milestone was not refused: ${out}"
    fi
    grep -qF "M0 states no Acceptance clause" <<<"${out}" ||
        fail "a milestone truncated by a heading '${heading}' was not named: ${out}"
done
# The same at the level a milestone under a phase heading has of its own.
printf '## Phase\n\n### M3a: Small\n\n**Delivers:** a thing.\n### Acceptance:\n\n- A cap parks the run.\n' >"${WORK}/deep-heading.md"
printf '# Nothing is listed.\n' >"${WORK}/none.txt"
list_check_against 1 "M3A would be skipped" "${WORK}/deep-heading.md" "${WORK}/none.txt" m3a
# A heading that states a milestone id ends a section as it should, though its
# title is about the acceptance: it starts the next milestone's section.
printf '## Phase\n\n### M3a: Small\n\n**Acceptance:**\n\n- A cap parks the run.\n\n### M3b: Acceptance under load\n\n**Acceptance:**\n\n- A scorer fails CI.\n' >"${WORK}/named-heading.md"
printf '## A cap parks the run.\nbudgets a_cap_parks\n' >"${WORK}/m3a.txt"
list_check_against 0 "lists M3A's 1 clauses" "${WORK}/named-heading.md" "${WORK}/m3a.txt" m3a
# The same line before a block that does read: open block or not, it is the
# line that is refused, and named with its number.
printf '## M0: A milestone\n\n**Acceptance, files and data**:\n\n- A thing that is not a clause.\n\n**Acceptance:**\n\n- A cap parks the run.\n- Cases in JSONL run the same, written over two lines.\n- A scorer fails CI.\n' >"${WORK}/near-open.md"
out="$(python3 "${CHECK}" m0 --milestones "${WORK}/near-open.md" --list "${WORK}/list.txt" --check 2>&1)"
if [ $? -ne 1 ] || ! grep -qF "near-open.md:3: not an Acceptance block marker" <<<"${out}"; then
    fail "a near-miss marker before an open block was not refused with its line: ${out}"
fi
printf '## M0: A milestone\n\nAcceptance, files and data:\n\n- A thing that is not a clause.\n\n**Acceptance:**\n\n- A cap parks the run.\n- Cases in JSONL run the same, written over two lines.\n- A scorer fails CI.\n' >"${WORK}/shaped-open.md"
out="$(python3 "${CHECK}" m0 --milestones "${WORK}/shaped-open.md" --list "${WORK}/list.txt" --check 2>&1)"
if [ $? -ne 1 ] || ! grep -qF "shaped-open.md:3: not an Acceptance block marker" <<<"${out}"; then
    fail "a marker-shaped line outside a block was not refused with its line: ${out}"
fi
grep -qF "M0's clause is not listed" <<<"${out}" &&
    fail "a block after a marker-shaped line stopped reading: ${out}"
# A milestone that states no clause says so, rather than passing with nothing
# asked of it: a "b" part inherits its parent's, so this is where the check that
# nothing was read has its say.
printf '## M0: A milestone\n\n**Delivers:** a thing.\n' >"${WORK}/no-clause.md"
printf '# Nothing is listed yet.\n' >"${WORK}/no-list.txt"
list_check_against 1 "M0 states no Acceptance clause" "${WORK}/no-clause.md" "${WORK}/no-list.txt" m0
printf '## M4: Split\n\n**Delivers:** a thing.\n\n### M4b\n\n**v1.0.** Finishes M4.\n' >"${WORK}/no-clause-split.md"
printf '## A cap parks the run.\nbudgets a_cap_parks\n## A scorer fails CI.\nbudgets a_cap_parks\n' >"${WORK}/m4b-none.txt"
list_check_against 1 "M4B states no Acceptance clause" "${WORK}/no-clause-split.md" "${WORK}/m4b-none.txt" m4b
# A milestone under a `###` heading is found, and ends at the next heading
# of its level.
printf '## Phase\n\n### M3a: Small\n\n**Acceptance:**\n\n- A cap parks the run.\n\n### M3b: Next\n\n**Acceptance:**\n\n- Other.\n' >"${WORK}/nested.md"
printf '## A cap parks the run.\nbudgets a_cap_parks\n' >"${WORK}/nested.txt"
out="$(python3 "${CHECK}" m3a --milestones "${WORK}/nested.md" --list "${WORK}/nested.txt" --check 2>&1)" ||
    fail "a ### milestone was not read: ${out}"

# A list that cannot be checked still says so in the job summary.
GITHUB_STEP_SUMMARY="${WORK}/bad-summary.md" list_check twice.txt "listed twice"
grep -qF "the list cannot be checked" "${WORK}/bad-summary.md" 2>/dev/null ||
    fail "a list that cannot be checked wrote no job summary"

# The repository's own lists state their milestones' clauses.
for list in "${ROOT}"/scripts/acceptance/*.txt; do
    milestone="$(basename "${list}" .txt)"
    out="$(python3 "${CHECK}" "${milestone}" --check 2>&1)" || fail "${list}: ${out}"
done

# And every milestone heading in the repository's own milestones document reads:
# an Acceptance block in a shape the check cannot read fails here, whether or
# not that milestone has a list yet. An empty list is enough to ask, so the
# whole of what the check says of a milestone whose blocks read is that list's
# own problem: `no clauses`, and nothing else. sweep_ok takes only that answer,
# so everything else fails the sweep - a block it could not read, a milestone
# that states no clause at all, or the check itself failing.
MILESTONES="${ROOT}/docs/design/milestones.md"
printf '# Nothing is listed.\n' >"${WORK}/sweep.txt"
# sweep_ok <milestone> <document> <title> <what else the check is asked to say,
# one problem per argument>: leaves what the check said in `said`, and says
# whether those were the only problems, in that order. Each argument is matched
# against the start of a problem line, so a message may say more than the test
# asks. The list it asks with holds no clauses, so what the check always has to
# say is that the list states none; of a milestone whose blocks all read that is
# all it says. A block it could not read, a milestone that states no clause at
# all, or the check itself failing says more, and that is what the arguments
# name - and a run that crashed says something the shape below cannot match.
sweep_ok() {
    local id="$1" doc="$2" title="$3" got code arg verdict
    local -a lines
    shift 3
    got="$(python3 "${CHECK}" "${id}" --milestones "${doc}" --list "${WORK}/sweep.txt" --check 2>&1)"
    code=$?
    said="${got}"
    [ "${code}" -eq 1 ] || return 1
    verdict="${title} acceptance: FAILED: the list cannot be checked"
    [[ "${got}" == *$'\n\n'"${verdict}" ]] || return 1
    mapfile -t lines <<<"${got%$'\n\n'"${verdict}"}"
    [ "${#lines[@]}" -eq $((1 + $#)) ] || return 1
    [ "${lines[0]}" = "${WORK}/sweep.txt: no clauses" ] || return 1
    local n=0
    for arg in "$@"; do
        [[ "${lines[$((1 + n))]}" == "${arg}"* ]] || return 1
        n=$((n + 1))
    done
}
swept=0
# The ids the sweep asked about, for the count of clauses below.
swept_ids=()
while read -r milestone; do
    sweep_ok "${milestone}" "${MILESTONES}" "$(tr '[:lower:]' '[:upper:]' <<<"${milestone}")" ||
        fail "${milestone}: the sweep asked with an empty list and heard: ${said}"
    swept=$((swept + 1))
    swept_ids+=("${milestone}")
done < <(grep -oE '^#+[[:space:]]+M[0-9]+[a-z]?' "${MILESTONES}" | grep -oE 'M[0-9]+[a-z]?')
[ "${swept}" -gt 50 ] || fail "the sweep read only ${swept} milestone headings in ${MILESTONES}"
# How many clauses the document states, over every milestone heading. The sweep
# above only asks that nothing went unread; a shape that stops the reader early
# loses clauses too, and a document whose blocks all read says so by how many it
# states. The count is a floor, which a document with more milestones clears.
stated_total=0
for milestone in "${swept_ids[@]}"; do
    n="$(clauses_of "${milestone}" "${MILESTONES}" | grep -c .)"
    stated_total=$((stated_total + n))
done
[ "${stated_total}" -ge 425 ] ||
    fail "the reader states ${stated_total} clauses over ${MILESTONES}, fewer than the 425 it states today"
# Only that answer passes, so anything else the check has to say of a milestone
# fails the sweep: a marker it cannot read, a milestone that states no clause, a
# document it cannot open. A document of the sweep's own, written here, so that no
# retitle of the real one can leave this asking about nothing.
named_doc="${WORK}/named-doc.md"
printf '## M4: Split\n\n**Acceptance:**\n\n- A cap parks the run.\n\n### M4b\n\n**v1.0.** Finishes M4.\n' >"${named_doc}"
# marker_line <milestone> <title> <document> <line number>: the check's answer for
# that milestone in that document, when all that is wrong is the one line.
marker_line() {
    sweep_ok "${1}" "${3}" "${2}" \
        "${3}:${4}: not an Acceptance block marker" \
        "${3}: ${2} states no Acceptance clause"
}
# So a marker the check cannot read is named even in a document no list asks
# about, by its line. A "b" part is not also named as stating no clause, since it
# inherits its parent's; the refusal is what the sweep cannot take.
printf '\n**Acceptance, beyond the full M4 test**:\n\n- Restarting twice costs nothing.\n' >>"${named_doc}"
sweep_ok m4b "${named_doc}" M4B "${named_doc}:11: not an Acceptance block marker" ||
    fail "a marker the check cannot read is not named as the sweep hears it: ${said}"
# A document the check cannot open says so in a traceback that names neither a
# line nor a milestone, so it is never the one answer the sweep takes.
sweep_ok m12 M12 "${WORK}/no-such-doc.md" &&
    fail "the sweep passed a check that failed outright: ${said}"
grep -qF "Traceback" <<<"${said}" ||
    fail "a document the check cannot open is not asked about as one: ${said}"
# A milestone that states no clause at all is named, so a document whose
# milestone states none is never the answer the sweep takes either.
printf '## M0: A milestone\n\n**Delivers:** a thing.\n' >"${WORK}/no-clause-doc.md"
sweep_ok m0 "${WORK}/no-clause-doc.md" M0 \
    "${WORK}/no-clause-doc.md: M0 states no Acceptance clause" ||
    fail "a milestone stating no clause is not named: ${said}"
# Which is what a milestone whose only marker is mangled comes to: its clauses sit
# under a line that opens no block, so none of them is read, and the check says
# both the line and the milestone. The wrapper may be none, bold, or a bullet -
# `+` and a number among them - and the word misspelt or plural.
for marker in 'Acceptance:' 'acceptance:' '__Acceptance:__' '**Acceptence:**' \
    '**Acceptances:**' '- **Acceptance:**' '+ **Acceptance:**' '1. **Acceptance:**'; do
    printf '## M0: A milestone\n\n**Delivers:** a thing.\n\n%s\n\n- A cap parks the run.\n- A scorer fails CI.\n' \
            "${marker}" >"${WORK}/mangled-doc.md"
    marker_line m0 M0 "${WORK}/mangled-doc.md" 5 ||
        fail "a milestone whose only marker was '${marker}' is not named as the sweep hears it: ${said}"
done
# A heading of the milestone's own level is the same shape, read as the next
# section's start rather than as a marker, and named as the line that ended the
# milestone it truncated.
printf '## M0: A milestone\n\n**Delivers:** a thing.\n\n## Acceptance criteria\n\n- A cap parks the run.\n' >"${WORK}/heading-doc.md"
sweep_ok m0 "${WORK}/heading-doc.md" M0 \
    "${WORK}/heading-doc.md:5: not an Acceptance block marker" \
    "${WORK}/heading-doc.md: M0 states no Acceptance clause" ||
    fail "a heading that truncates a milestone is not named as the sweep hears it: ${said}"
# A fence left open at the end says so too, though it names no line: what it held
# and every section after it went unread.
printf '## M0: A milestone\n\n**Delivers:** a thing.\n\n```yaml\nacceptance_criteria:\n' >"${WORK}/open-fence-doc.md"
sweep_ok m0 "${WORK}/open-fence-doc.md" M0 \
    "${WORK}/open-fence-doc.md: M0's section ends inside a fenced code block" \
    "${WORK}/open-fence-doc.md: M0 states no Acceptance clause" ||
    fail "a fence left open is not named as the sweep hears it: ${said}"

# The features it runs with are those CI builds the tests with, so nothing
# rebuilds.
ci="$(grep -o 'cargo test --workspace --locked --no-fail-fast --features [^ ]*' "${ROOT}/.github/workflows/ci.yml" | head -1 | awk '{print $NF}')"
ours="$(python3 -c "import sys; sys.path.insert(0, '${ROOT}/scripts'); import acceptance; print(acceptance.FEATURES)")"
[ -n "${ci}" ] && [ "${ci}" = "${ours}" ] || fail "acceptance.py's features (${ours}) are not CI's test build's (${ci})"

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
# In a run, cargo's exit says whether it failed: a line a test printed that
# reads like cargo's error does not fail a run that passed.
echo "error: a test's own output" >>"${WORK}/ok.log"
PATH="${WORK}/bin:${PATH}" check 0 "M0 acceptance: passed"

if [ "${failures}" -ne 0 ]; then
    echo "${failures} acceptance.py test(s) failed" >&2
    exit 1
fi
echo "acceptance.py: all tests passed"
