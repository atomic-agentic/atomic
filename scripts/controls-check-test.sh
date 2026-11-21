#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Tests for scripts/controls-check.sh, in a throwaway copy of the files it
# edits, with a stand-in for cargo. No Rust toolchain is needed.
#
#   scripts/controls-check-test.sh
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)" || exit 1
trap 'chmod -R u+w "${WORK}" 2>/dev/null; rm -rf "${WORK}"' EXIT
failures=0

fail() {
    echo "FAIL $*" >&2
    failures=$((failures + 1))
}

# A stand-in for cargo. Each test a row names is in a test binary of its
# own. FAKE_CARGO says how it behaves: `honest` fails every test while any
# control is switched off, `blind` always passes them, `partial` fails only
# the first a control names, `missing` finds no test for the second, `flood`
# fails tens of thousands of others after the one named, `lose` also deletes
# the saved originals, `unreadable` makes the manifest unreadable, `stale`
# replaces what was saved with a state from before the commit, `rebuild` says
# in rebuild.log how each file it edits stands against the binaries this run
# built, `kill` kills the script outright, and `hang` waits, with the control
# off, until it is stopped. It notes its arguments in cargo-args.
mkdir -p "${WORK}/bin"
cat >"${WORK}/bin/cargo" <<'SH'
#!/usr/bin/env bash
# A control is off while any file the script edits differs from the copy's
# commit.
off() {
    ! git diff --quiet -- crates
}
# What the script keeps for a file, and the file the git.rs control edits.
state_copy() { echo "target/controls-check/$(tr / % <<<"$1")"; }
gitrs=crates/atomic-core/src/git.rs
case "${FAKE_CARGO}" in
lose) rm -f target/controls-check/crates* ;;
unreadable) chmod 000 target/controls-check/manifest ;;
stale)
    # A state from before the commit, recorded as the original: what a run
    # that started from a dirty file would have saved. It restores cleanly
    # against the manifest's own hash while leaving HEAD behind.
    if [ ! -e stale-made ]; then
        : >stale-made
        printf '%s\n' '// left over from an older, uncommitted state' >"$(state_copy "${gitrs}")"
        printf '%s %s\n' "$(git hash-object -- "$(state_copy "${gitrs}")")" "${gitrs}" \
            >target/controls-check/manifest
    fi
    ;;
rebuild)
    # The first run is the build with the control off, so note when its
    # binaries are made. The second runs the tests with the control back on,
    # after the script put the file back: say how that file's time stands
    # against those binaries, which is how cargo weighs them.
    if [ ! -e artefacts ]; then
        : >artefacts
    else
        # Only the files the script saved a copy of are ones it edited.
        for f in crates/*/src/*.rs crates/*/src/*/*.rs; do
            [ -e "$f" ] || continue
            [ -e "$(state_copy "$f")" ] || continue
            if [ "$f" -nt artefacts ]; then
                echo "newer $f"
            else
                echo "not-newer $f"
            fi
        done >>rebuild.log
    fi
    ;;
kill) kill -KILL "${PPID}"; exit 0 ;;
hang) : >cargo-started; exec sleep 300 ;;
esac
# The tests the arguments name, after `--` or as positional filters, each
# in a test binary of its own, run in order as cargo runs them: without
# --no-fail-fast, cargo stops at the first binary that fails. A flag's value
# is no test, and neither is a flag of the test binary's after `--`.
echo "$*" >>cargo-args
[ "${1:-}" = test ] && shift
names=()
after=0
skip=0
no_fail_fast=0
for a in "$@"; do
    if [ "${skip}" -eq 1 ]; then skip=0; continue; fi
    if [ "${after}" -eq 1 ]; then
        case "${a}" in -*) ;; *) names+=("${a}") ;; esac
        continue
    fi
    case "${a}" in
    --) after=1 skip=0 ;;
    --no-fail-fast) no_fail_fast=1 ;;
    -p | --package | --features | --test | --bin | --example) skip=1 ;;
    -*) ;;
    *) names+=("${a}") ;;
    esac
done
[ "${#names[@]}" -gt 0 ] || names=(guard)
status=0
i=0
for n in "${names[@]}"; do
    i=$((i + 1))
    # `missing`: a second named test matches none, so nothing of it is said.
    if [ "${FAKE_CARGO}" = missing ] && [ "${i}" -gt 1 ]; then
        echo "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out"
        continue
    fi
    fails=0
    if [ "${FAKE_CARGO}" != blind ] && off; then
        fails=1
        # `partial`: only the first named test fails.
        [ "${FAKE_CARGO}" = partial ] && [ "${i}" -gt 1 ] && fails=0
    fi
    if [ "${fails}" -eq 1 ]; then
        echo "test tests::${n} ... FAILED"
        # `flood`: and so many other tests failed after it that what is left
        # to read is far past what a pipe holds, which a check that stops at
        # the first match pays for with a broken pipe.
        if [ "${FAKE_CARGO}" = flood ]; then
            awk 'BEGIN {
                for (i = 1; i <= 40000; i++) print "test tests::another_" i " ... FAILED"
            }'
        fi
        echo "test result: FAILED. 0 passed; 1 failed"
        status=101
        [ "${no_fail_fast}" -eq 1 ] || exit 101
    else
        echo "test tests::${n} ... ok"
        echo "test result: ok. 1 passed; 0 failed"
    fi
done
exit "${status}"
SH
chmod +x "${WORK}/bin/cargo"

# A cp that keeps the timestamp of what it copies, as `cp -p` or an alias
# for it does. A plain cp gives the file a new mtime, so cargo rebuilds; only
# with this cp on the PATH does the rebuild case depend on the script's own
# touch, which is what it tests.
mkdir -p "${WORK}/bin-oldcp"
cp_bin="$(command -v cp)"
cat >"${WORK}/bin-oldcp/cp" <<SH
#!/usr/bin/env bash
exec "${cp_bin}" -p "\$@"
SH
chmod +x "${WORK}/bin-oldcp/cp"

# The controls the script reads, one per file under scripts/controls/, and the
# files they edit.
mapfile -d '' -t CONTROL_ROW_FILES < <(
    find "${ROOT}/scripts/controls" -mindepth 1 -maxdepth 1 -type f -name '*.row' -print0 |
        LC_ALL=C sort -z
)
[ "${#CONTROL_ROW_FILES[@]}" -gt 0 ] || {
    echo "no controls found under scripts/controls" >&2
    exit 1
}
CONTROL_ROWS=()
for control_row_file in "${CONTROL_ROW_FILES[@]}"; do
    mapfile -t control_row_lines <"${control_row_file}"
    [ "${#control_row_lines[@]}" -eq 1 ] || {
        echo "${control_row_file} does not hold exactly one row" >&2
        exit 1
    }
    CONTROL_ROWS+=("${control_row_lines[0]}")
done
mapfile -t CONTROL_FILES < <(printf '%s\n' "${CONTROL_ROWS[@]}" | cut -d'|' -f2 | sort -u)

# A fresh copy of the repository's part the script touches, committed.
fresh() {
    local repo="${WORK}/repo"
    chmod -R u+w "${repo}" 2>/dev/null
    rm -rf "${repo}"
    mkdir -p "${repo}/scripts/controls"
    cp "${ROOT}/scripts/controls-check.sh" "${repo}/scripts/"
    cp "${ROOT}"/scripts/controls/*.row "${repo}/scripts/controls/"
    # Every file a control names.
    local file
    for file in "${CONTROL_FILES[@]}"; do
        mkdir -p "${repo}/$(dirname "${file}")"
        cp "${ROOT}/${file}" "${repo}/${file}"
    done
    git -C "${repo}" init -q
    git -C "${repo}" -c user.name=test -c user.email=test@example.invalid add -A
    git -C "${repo}" -c user.name=test -c user.email=test@example.invalid -c commit.gpgSign=false \
        commit -qm base
    echo "${repo}"
}

# run MODE ARGS...: run the script in the copy; sets `code` and `out`.
# RUN_PATH, when set, is a directory put before the stand-in cargo's, for a
# mode that wants a different cp.
run() {
    local mode="$1"
    shift
    out="$(cd "${repo}" && FAKE_CARGO="${mode}" \
        PATH="${RUN_PATH:+${RUN_PATH}:}${WORK}/bin:${PATH}" \
        scripts/controls-check.sh "$@" 2>&1)"
    code=$?
}

untouched() {
    [ -z "$(git -C "${repo}" status --porcelain -- crates)" ]
}

# Every control fails its test when off and passes when on; nothing is left.
repo="$(fresh)"
run honest
[ "${code}" -eq 0 ] || fail "honest: exit ${code}: ${out}"
[ "$(grep -c '^ok ' <<<"${out}")" -eq "${#CONTROL_ROWS[@]}" ] ||
    fail "honest: not all ${#CONTROL_ROWS[@]} controls ok: ${out}"
untouched || fail "honest: the source is left modified"
[ -z "$(git -C "${repo}" status --porcelain -- scripts)" ] ||
    fail "honest: a file under scripts/ is left changed: $(git -C "${repo}" status --porcelain -- scripts)"
[ ! -e "${repo}/target/controls-check" ] || fail "honest: the saved copies are left behind"

# A test that passes with its control off fails the check.
repo="$(fresh)"
run blind git-dir
[ "${code}" -ne 0 ] && grep -q "FAIL git-dir: its test passed with the control off" <<<"${out}" ||
    fail "blind: exit ${code}: ${out}"
untouched || fail "blind: the source is left modified"

# A control whose tests do not all fail with it off fails the check.
repo="$(fresh)"
run partial sandbox-writes
[ "${code}" -ne 0 ] && grep -q "FAIL sandbox-writes: its test passed with the control off" <<<"${out}" ||
    fail "partial: exit ${code}: ${out}"
untouched || fail "partial: the source is left modified"

# A control one of whose tests matches nothing, so it is never seen
# failing, fails the check, though nothing passed.
repo="$(fresh)"
run missing sandbox-writes
[ "${code}" -ne 0 ] && grep -q "FAIL sandbox-writes: with the control off, these tests were not seen failing: a_project_under_the_system_temporary_directory_stays_out_of_reach" <<<"${out}" ||
    fail "missing: exit ${code}: ${out}"
untouched || fail "missing: the source is left modified"

# A row's arguments reach cargo as written: no variable in them is expanded,
# and none is left to be.
repo="$(fresh)"
run honest sandbox-writes
grep -q -- "--features atomic-agentic/postgres-tests,atomic-agentic/sandbox-tests" "${repo}/cargo-args" ||
    fail "features: cargo was not given the row's features: $(cat "${repo}/cargo-args")"
! grep -q '\${' "${repo}/cargo-args" ||
    fail "features: a variable reached cargo unexpanded: $(cat "${repo}/cargo-args")"
! grep -l '\${' "${ROOT}"/scripts/controls/*.row >/dev/null 2>&1 ||
    fail "features: a row holds a variable: $(grep -l '\${' "${ROOT}"/scripts/controls/*.row)"

# A row whose only argument after `--` is a flag of the test binary's, not a
# test: the flag is not taken for a test that should have been seen failing.
repo="$(fresh)"
sed -i 's/|-p atomic-agentic-core .*$/|-p atomic-agentic-core --lib -- --exact/' \
    "${repo}/scripts/controls/git-dir.row"
grep -q '^git-dir|.*-- --exact$' "${repo}/scripts/controls/git-dir.row" ||
    fail "flag after --: the row was not changed: $(cat "${repo}/scripts/controls/git-dir.row")"
run honest git-dir
[ "${code}" -eq 0 ] && grep -q "^ok git-dir" <<<"${out}" ||
    fail "flag after --: exit ${code}: ${out}"
untouched || fail "flag after --: the source is left modified"

# Tens of thousands of other tests failed as well, after the one a row names:
# that one is still seen failing, where a check that stopped at the first
# match would kill whatever piped to it and read that as a test never seen.
repo="$(fresh)"
run flood git-dir
[ "${code}" -eq 0 ] && grep -q "^ok git-dir" <<<"${out}" ||
    fail "flood: exit ${code}: ${out}"
untouched || fail "flood: the source is left modified"

# Every test binary runs, so a test after a failing one is seen too.
repo="$(fresh)"
run honest sandbox-writes
grep -q -- "--no-fail-fast" "${repo}/cargo-args" || fail "no-fail-fast: cargo ran as $(cat "${repo}/cargo-args")"

# An unknown name is refused, and nothing is edited.
repo="$(fresh)"
run honest no-such-control
[ "${code}" -eq 2 ] && grep -q "no control is named 'no-such-control'" <<<"${out}" ||
    fail "unknown name: exit ${code}: ${out}"
untouched || fail "unknown name: the source is edited"

# A control is whatever a file under scripts/controls/ says it is, to the end
# of the run: a file added there is a control the script selects, switches off
# and requires its test to fail. A run that asked cargo what to check, instead
# of reading the rows, would never switch this one off and would pass.
repo="$(fresh)"
printf '%s\n' \
    'no-such-control|crates/atomic-core/src/git.rs|        for setting in HARDENING {|        for setting in HARDENING.iter().take(0) {|guard' \
    >"${repo}/scripts/controls/no-such-control.row"
run blind no-such-control
[ "${code}" -ne 0 ] && grep -q "FAIL no-such-control: its test passed with the control off" <<<"${out}" ||
    fail "a row added is a control run: exit ${code}: ${out}"
untouched || fail "a row added is a control run: the source is left modified"

# Each rule that reads the rows names the file it refuses and stops before
# anything is edited: a control the script cannot read is one it would
# otherwise never check and still report.
repo="$(fresh)"
printf '%s\n' 'another-name|crates/atomic-core/src/git.rs|a|b|args' \
    >"${repo}/scripts/controls/git-hardening.row"
run honest git-hardening
[ "${code}" -eq 1 ] &&
    grep -q "scripts/controls/git-hardening.row names itself 'another-name', which is not its file name" <<<"${out}" ||
    fail "a row named otherwise: exit ${code}: ${out}"
[ ! -e "${repo}/cargo-args" ] || fail "a row named otherwise: cargo ran"

repo="$(fresh)"
printf '%s\n%s\n' \
    'git-hardening|crates/atomic-core/src/git.rs|a|b|args' \
    'git-filters|crates/atomic-core/src/git.rs|c|d|args' \
    >"${repo}/scripts/controls/git-hardening.row"
run honest git-hardening
[ "${code}" -eq 1 ] &&
    grep -q "scripts/controls/git-hardening.row holds 2 rows, not the one it must" <<<"${out}" ||
    fail "a row file with two rows: exit ${code}: ${out}"
[ ! -e "${repo}/cargo-args" ] || fail "a row file with two rows: cargo ran"

repo="$(fresh)"
: >"${repo}/scripts/controls/git-hardening.row"
run honest git-hardening
[ "${code}" -eq 1 ] &&
    grep -q "scripts/controls/git-hardening.row holds 0 rows, not the one it must" <<<"${out}" ||
    fail "an empty row file: exit ${code}: ${out}"
[ ! -e "${repo}/cargo-args" ] || fail "an empty row file: cargo ran"

repo="$(fresh)"
printf '%s\n' '|crates/atomic-core/src/git.rs|a|b|args' \
    >"${repo}/scripts/controls/git-hardening.row"
run honest git-hardening
[ "${code}" -eq 1 ] && grep -q "scripts/controls/git-hardening.row names no control" <<<"${out}" ||
    fail "a row naming no control: exit ${code}: ${out}"
[ ! -e "${repo}/cargo-args" ] || fail "a row naming no control: cargo ran"

repo="$(fresh)"
printf '%s\n' 'git-hardening|crates/atomic-core/src/git.rs|a|b|args' \
    >"${repo}/scripts/controls/zsecond.row"
run honest git-hardening
[ "${code}" -eq 1 ] &&
    grep -q "scripts/controls/zsecond.row names the control 'git-hardening', which another row file already names" <<<"${out}" ||
    fail "two rows naming one control: exit ${code}: ${out}"
[ ! -e "${repo}/cargo-args" ] || fail "two rows naming one control: cargo ran"

repo="$(fresh)"
printf '%s\n' 'git-hardening|crates/atomic-core/src/git.rs|a|b|args' \
    >"${repo}/scripts/controls/git-hardening.row.txt"
run honest git-hardening
[ "${code}" -eq 1 ] &&
    grep -q "scripts/controls/git-hardening.row.txt is not named <name>.row, so its control would never be read" <<<"${out}" ||
    fail "a row file named otherwise: exit ${code}: ${out}"
[ ! -e "${repo}/cargo-args" ] || fail "a row file named otherwise: cargo ran"

repo="$(fresh)"
mkdir -p "${repo}/scripts/controls/sub.row"
run honest git-hardening
[ "${code}" -eq 1 ] && grep -q "scripts/controls/sub.row is not a regular file" <<<"${out}" ||
    fail "a directory in the rows: exit ${code}: ${out}"
[ ! -e "${repo}/cargo-args" ] || fail "a directory in the rows: cargo ran"

# Rows that are gone are no control to check. A run that found nothing and
# said nothing would be the quiet way to lose every control at once.
repo="$(fresh)"
rm -rf "${repo}/scripts/controls"
run honest
[ "${code}" -eq 1 ] &&
    grep -q "scripts/controls is not a directory, so there is no control to check" <<<"${out}" ||
    fail "the rows gone: exit ${code}: ${out}"
[ ! -e "${repo}/cargo-args" ] || fail "the rows gone: cargo ran"
untouched || fail "the rows gone: the source is edited"

repo="$(fresh)"
rm -f "${repo}"/scripts/controls/*.row
run honest
[ "${code}" -eq 1 ] && grep -q "scripts/controls holds no control" <<<"${out}" ||
    fail "no row files: exit ${code}: ${out}"
[ ! -e "${repo}/cargo-args" ] || fail "no row files: cargo ran"

# One row file gone is one control gone: every other control still runs, and
# the name that lost its file is unknown.
repo="$(fresh)"
rm -f "${repo}/scripts/controls/git-hardening.row"
run honest
[ "${code}" -eq 0 ] || fail "one row file gone: exit ${code}: ${out}"
[ "$(grep -c '^ok ' <<<"${out}")" -eq "$((${#CONTROL_ROWS[@]} - 1))" ] ||
    fail "one row file gone: not every other control ok: ${out}"
run honest git-hardening
[ "${code}" -eq 2 ] && grep -q "no control is named 'git-hardening'" <<<"${out}" ||
    fail "one row file gone, by name: exit ${code}: ${out}"

# When the originals cannot be saved, nothing is edited.
repo="$(fresh)"
: >"${repo}/target"
run honest git-hardening
[ "${code}" -ne 0 ] && grep -q "nothing was edited" <<<"${out}" ||
    fail "no backup: exit ${code}: ${out}"
untouched || fail "no backup: the source is edited"
rm -f "${repo}/target"

# When an original is lost mid-run, the script says the file is left
# modified, fails, and keeps what it saved.
repo="$(fresh)"
run lose git-hardening
[ "${code}" -ne 0 ] && grep -q "crates/atomic-core/src/git.rs IS LEFT MODIFIED" <<<"${out}" ||
    fail "lost original: exit ${code}: ${out}"
[ -e "${repo}/target/controls-check/manifest" ] || fail "lost original: the manifest is gone"

# An unreadable manifest is a failure, not a clean restore.
repo="$(fresh)"
run unreadable git-hardening
[ "${code}" -ne 0 ] && grep -q "manifest cannot be read" <<<"${out}" ||
    fail "unreadable manifest: exit ${code}: ${out}"
chmod 600 "${repo}/target/controls-check/manifest"

# A file that already differs from HEAD is refused before anything is
# edited: the run would save that state, put it back at the end and read as
# a clean restore.
repo="$(fresh)"
printf '\n// a change that was never committed\n' >>"${repo}/crates/atomic-core/src/git.rs"
run honest git-hardening
[ "${code}" -ne 0 ] &&
    grep -q "differ from HEAD: crates/atomic-core/src/git.rs" <<<"${out}" ||
    fail "dirty at the start: exit ${code}: ${out}"
grep -q "nothing was edited" <<<"${out}" || fail "dirty at the start: ${out}"
grep -q "never committed" <<<"$(git -C "${repo}" diff -- crates/atomic-core/src/git.rs)" ||
    fail "dirty at the start: the run edited the file's own change"
[ ! -e "${repo}/target/controls-check" ] || fail "dirty at the start: state was left behind"
[ ! -e "${repo}/cargo-args" ] || fail "dirty at the start: cargo ran"
# A whole-suite run names every file it refuses, not just the first.
printf '\n// a change that was never committed\n' >>"${repo}/crates/atomic-core/src/codex.rs"
run honest
# Both, whichever order the rows list them in.
dirty="$(grep -o 'differ from HEAD:[^(]*' <<<"${out}")"
grep -q "src/git\.rs" <<<"${dirty}" && grep -q "src/codex\.rs" <<<"${dirty}" ||
    fail "dirty at the start, all controls: exit ${code}: ${out}"

# A file git does not track, or one it is told to assume unchanged, cannot
# be shown to match HEAD, so it is refused the same way.
repo="$(fresh)"
git -C "${repo}" update-index --assume-unchanged crates/atomic-core/src/git.rs
printf '\n// a change git is told not to look at\n' >>"${repo}/crates/atomic-core/src/git.rs"
run honest git-hardening
[ "${code}" -ne 0 ] && grep -q "nothing was edited" <<<"${out}" ||
    fail "assume-unchanged at the start: exit ${code}: ${out}"
[ ! -e "${repo}/cargo-args" ] || fail "assume-unchanged at the start: cargo ran"
repo="$(fresh)"
git -C "${repo}" rm -q --cached crates/atomic-core/src/git.rs
git -C "${repo}" -c user.name=test -c user.email=test@example.invalid -c commit.gpgSign=false \
    commit -qm "stop tracking git.rs"
run honest git-hardening
[ "${code}" -ne 0 ] && grep -q "nothing was edited" <<<"${out}" ||
    fail "untracked at the start: exit ${code}: ${out}"
[ ! -e "${repo}/cargo-args" ] || fail "untracked at the start: cargo ran"

# A restore that leaves a file matching the copy the run made, but not HEAD,
# fails and keeps its state: the manifest's own hash cannot see that what was
# saved was never the commit.
repo="$(fresh)"
run stale git-hardening
[ "${code}" -ne 0 ] &&
    grep -q "crates/atomic-core/src/git.rs IS LEFT DIFFERENT FROM HEAD" <<<"${out}" ||
    fail "stale restore: exit ${code}: ${out}"
[ -e "${repo}/target/controls-check/manifest" ] || fail "stale restore: the state is gone"
git -C "${repo}" diff --quiet HEAD -- crates/atomic-core/src/git.rs &&
    fail "stale restore: nothing was left differing from HEAD, so the case tests nothing"

# A file put back is newer than the binaries built from it while its control
# was off, so cargo rebuilds it instead of running what is already there. This
# runs with a cp that keeps the timestamp of what it copies, as `cp -p` and
# some cp implementations do, and a source older than those binaries: that is
# how a restore can leave a file that cargo will not look at again.
repo="$(fresh)"
touch -d '2000-01-01 00:00:00 UTC' "${repo}/crates/atomic-core/src/git.rs"
RUN_PATH="${WORK}/bin-oldcp"
run rebuild git-hardening
RUN_PATH=
[ "${code}" -eq 0 ] || fail "rebuild: exit ${code}: ${out}"
untouched || fail "rebuild: the source is left modified"
[ -e "${repo}/rebuild.log" ] || fail "rebuild: cargo never saw the file put back"
newer="$(grep -c '^newer crates/atomic-core/src/git.rs$' "${repo}/rebuild.log")"
older="$(grep -c '^not-newer ' "${repo}/rebuild.log")"
[ "${newer}" -eq 1 ] && [ "${older}" -eq 0 ] ||
    fail "rebuild: $(cat "${repo}/rebuild.log")"
# And the same when the run ends and puts every file back, so that nothing
# later reuses a binary built while a control was off.
[ "${repo}/crates/atomic-core/src/git.rs" -nt "${repo}/artefacts" ] ||
    fail "rebuild: the file left at the end is not newer than this run's build"

# A TERM while cargo runs with a control off, as a time limit sends, stops
# the script promptly and puts the source back.
repo="$(fresh)"
(cd "${repo}" && FAKE_CARGO=hang PATH="${WORK}/bin:${PATH}" exec scripts/controls-check.sh git-hardening) \
    >"${WORK}/term.out" 2>&1 &
pid=$!
for _ in $(seq 1 100); do
    [ -e "${repo}/cargo-started" ] && break
    sleep 0.1
done
untouched && fail "TERM: the control was never switched off"
kill -TERM "${pid}"
for _ in $(seq 1 100); do
    kill -0 "${pid}" 2>/dev/null || break
    sleep 0.1
done
if kill -0 "${pid}" 2>/dev/null; then
    fail "TERM: the script was still running 10 s after it"
    kill -KILL "${pid}"
fi
wait "${pid}"
code=$?
[ "${code}" -ne 0 ] || fail "TERM: exit 0: $(cat "${WORK}/term.out")"
untouched || fail "TERM: the control is left switched off"
[ ! -e "${repo}/target/controls-check" ] || fail "TERM: the saved copies are left behind"

# A run killed outright leaves its originals, and the next run refuses to
# start until they are dealt with.
repo="$(fresh)"
run kill git-hardening
untouched && fail "killed: the stand-in did not kill the script mid-run"
grep -q "git.rs" "${repo}/target/controls-check/manifest" || fail "killed: no manifest left"
run honest git-hardening
[ "${code}" -ne 0 ] && grep -q "left from a run that did not finish" <<<"${out}" ||
    fail "after a kill: exit ${code}: ${out}"
cp "${repo}/target/controls-check/crates%atomic-core%src%git.rs" "${repo}/crates/atomic-core/src/git.rs"
untouched || fail "after a kill: the saved original does not restore the file"

# What each case above is the control of, in scripts/controls-check.sh. A
# control is tested by removing it: neutralise one line at a time and its case
# fails.
#
#   dirty+="${file} "                               dirty at the start
#   *) return 1 ;;                  in head_diff    untracked, assume-unchanged
#   head_diff "${file}" || code=$?  in restore      stale restore
#   && touch -- "$1"                in put_back     rebuild, in rebuild.log
#   touch -- "${file}"              in restore      rebuild, against artefacts
#   [ -d "${ROWS}" ] || die                         the rows gone
#   [ "${#entries[@]}" -gt 0 ] || die               no row files
#   [ -f "${entry}" ] || die                        a directory in the rows
#   *.row) ;;                                       a row file with another
#                                                   extension
#   [ "${#lines[@]}" -eq 1 ] || die                 two rows, and an empty file
#   [ -n "${name}" ] || die                         a row naming no control
#   case " ${names[*]-} " in                        two rows naming one control
#   [ "${name}" = "${base%.row}" ] || die           a row naming itself
#                                                   otherwise
#   selected+=("${row}")                            a row added, one row file
#                                                   gone, and the honest count
#   [ "${#selected[@]}" -eq 0 ] || ... exit 2       unknown names, the name of
#                                                   a control whose file is
#                                                   gone

if [ "${failures}" -ne 0 ]; then
    echo "${failures} controls-check test(s) failed" >&2
    exit 1
fi
echo "controls-check tests passed" >&2
