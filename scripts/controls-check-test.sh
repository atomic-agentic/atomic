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
# built, `kill` kills the script outright, `hang` waits, with the control off,
# until it is stopped and notes the pid to stop in cargo-pid, `orphan` notes its
# pid in orphan-pid and leaves a process behind that keeps whatever descriptors
# cargo handed down, and `spaces` names a failed test as a doctest is named. It
# notes its arguments in cargo-args.
mkdir -p "${WORK}/bin"
cat >"${WORK}/bin/cargo" <<'SH'
#!/usr/bin/env bash
# A control is off while the file it switches differs from the copy's commit.
# CONTROL_FILES, which the test passes down, is every file a row names, so this
# asks about a control's own file wherever that lives -- crates/ or tools/ --
# and takes nothing a case of the test does to a row file, or to scripts/, for a
# control switched off.
off() {
    local f
    # shellcheck disable=SC2086
    for f in ${CONTROL_FILES}; do
        [ -e "${f}" ] || continue
        git diff --quiet -- "${f}" || return 0
    done
    return 1
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
hang)
    # Its own pid before the file a case waits on: `exec` keeps the pid, so a
    # case that stops the run can name the cargo it was waiting on.
    echo "$$" >cargo-pid
    : >cargo-started
    exec sleep 300
    ;;
orphan)
    # A process this run leaves behind, which keeps whatever descriptors cargo
    # handed down; its pid is noted in orphan-pid so that the case asking for
    # it can stop it. It is left once, because the same stand-in is called
    # again after the control goes back. Nothing of its own is on any of its
    # three standard descriptors, so it holds no writer open on anything the
    # run opened, only what it inherited.
    if [ ! -e orphan-pid ]; then
        (exec sleep 300) </dev/null >/dev/null 2>&1 &
        echo "$!" >orphan-pid
    fi
    ;;
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
        # `spaces`: the test is named as a doctest is, with spaces in it.
        if [ "${FAKE_CARGO}" = spaces ]; then
            echo "test crates/atomic-core/src/codex.rs - ${n} (line 1) - compile fail ... FAILED"
        else
            echo "test tests::${n} ... FAILED"
        fi
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
    cp "${ROOT}/scripts/controls-on.py" "${repo}/scripts/"
    # What the tree ignores, so that a run's own state under target/ is judged
    # as it is in the real tree.
    cp "${ROOT}/.gitignore" "${repo}/"
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
# mode that wants a different cp. CONTROL_FILES goes to the stand-in so that it
# knows which files a control may switch off, whichever directory they live in.
run() {
    local mode="$1"
    shift
    out="$(cd "${repo}" && FAKE_CARGO="${mode}" CONTROL_FILES="${CONTROL_FILES[*]}" \
        PATH="${RUN_PATH:+${RUN_PATH}:}${WORK}/bin:${PATH}" \
        scripts/controls-check.sh "$@" 2>&1)"
    code=$?
}

# Whether none of the files the rows name is left modified, which is what the
# script edits. The rows' own files are no control's file, and several cases
# below change or remove them on purpose.
untouched() {
    [ "${#CONTROL_FILES[@]}" -eq 0 ] ||
        [ -z "$(git -C "${repo}" status --porcelain -- "${CONTROL_FILES[@]}")" ]
}

# run_on: ask scripts/controls-on.py about the copy; sets `code` and `out`. It
# reads the tree its own file sits in, so the copy is what it judges.
run_on() {
    out="$(cd "${repo}" && python3 scripts/controls-on.py 2>&1)"
    code=$?
}

# Every control is on in the tree these cases copy. CI asks the same question
# of the tree it checks out; asked again here, because a control committed
# switched off would leave every case below reading a disabled control's files
# as a control on: the stand-in cargo weighs a control against HEAD, and a
# committed one matches HEAD.
if ! on_out="$(python3 "${ROOT}/scripts/controls-on.py" 2>&1)"; then
    echo "FAIL a control is switched off in this tree, so every case below" \
        "would weigh a disabled control against its own commit:" >&2
    printf '%s\n' "${on_out}" >&2
    exit 1
fi

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

# A failed test whose name has spaces in it, as a doctest's does, is seen
# failing.
repo="$(fresh)"
run spaces exec-network-loopback-feature
[ "${code}" -eq 0 ] && grep -q "^ok exec-network-loopback-feature" <<<"${out}" ||
    fail "spaces: exit ${code}: ${out}"
untouched || fail "spaces: the source is left modified"

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
printf '%s\n' 'git-hardening|crates/atomic-core/src/git.rs|a || b|c|args' \
    >"${repo}/scripts/controls/git-hardening.row"
run honest git-hardening
[ "${code}" -eq 1 ] &&
    grep -q "scripts/controls/git-hardening.row has 6 '|' separators, not the 4 between its five fields" <<<"${out}" ||
    fail "a row whose field holds a '|': exit ${code}: ${out}"
[ ! -e "${repo}/cargo-args" ] || fail "a row whose field holds a '|': cargo ran"

repo="$(fresh)"
printf '%s\n' 'git-hardening|crates/atomic-core/src/git.rs|a|args' \
    >"${repo}/scripts/controls/git-hardening.row"
run honest git-hardening
[ "${code}" -eq 1 ] &&
    grep -q "scripts/controls/git-hardening.row has 3 '|' separators, not the 4 between its five fields" <<<"${out}" ||
    fail "a row missing a field: exit ${code}: ${out}"
[ ! -e "${repo}/cargo-args" ] || fail "a row missing a field: cargo ran"

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

# One run at a time in a tree. Two runs switch the same files, each saving its
# own originals, and the one that finishes last puts back what it found, which
# is the other's switched-off source. While one run holds the tree's lock, the
# next refuses before it edits anything or asks cargo anything. The run that
# holds it is asked to stop by the case, so it ends with its own exit.
repo="$(fresh)"
(cd "${repo}" && FAKE_CARGO=hang PATH="${WORK}/bin:${PATH}" exec scripts/controls-check.sh git-hardening) \
    >/dev/null 2>&1 &
holder=$!
# Not a job of this shell: it is stopped outright below, and a job the shell
# reports as killed reads to a reviewer like a failure of the suite.
disown "${holder}"
for _ in $(seq 1 100); do
    [ -e "${repo}/cargo-started" ] && break
    sleep 0.1
done
[ -e "${repo}/cargo-started" ] || fail "lock held: the first run never reached cargo"
# The kernel holds what the first run took, so the second's refusal is the
# lock and not a message it happens to print.
if flock -n "${repo}/target/controls-check.lock" -c 'exit 0' 2>/dev/null; then
    fail "lock held: target/controls-check.lock is free while the first run is inside cargo, so its refusal was not the lock"
fi
# The row the second run names edits redact.rs, a file the first run never
# touches, so the refusal is for the lock and not for a dirty file. The first
# run has git.rs switched off, which is its own doing and not this run's.
run honest braced-esc
[ "${code}" -eq 1 ] &&
    grep -q "target/controls-check.lock is held by another scripts/controls-check.sh run in this tree" <<<"${out}" ||
    fail "lock held: exit ${code}: ${out}"
grep -q "nothing was edited" <<<"${out}" || fail "lock held: the refusal does not say nothing was edited: ${out}"
[ -z "$(git -C "${repo}" status --porcelain -- crates/atomic-core/src/redact.rs)" ] ||
    fail "lock held: the second run edited a file: $(git -C "${repo}" status --porcelain -- crates/atomic-core/src/redact.rs)"
[ ! -e "${repo}/cargo-args" ] || fail "lock held: the second run asked cargo: $(cat "${repo}/cargo-args")"
# The lock is the kernel's, so a run killed outright releases it and the next
# starts. The kill finds the holder inside cargo, with its control switched off
# and its originals saved, which is the state a lock must outlive.
kill -KILL "${holder}" 2>/dev/null
# What that run was waiting on is this case's to stop, too: a run killed
# outright never gets to stop the cargo it was inside.
kill -KILL "$(cat "${repo}/cargo-pid")" 2>/dev/null
for _ in $(seq 1 100); do
    kill -0 "${holder}" 2>/dev/null || break
    sleep 0.1
done
git -C "${repo}" diff --quiet HEAD -- crates/atomic-core/src/git.rs &&
    fail "killed holder: the first run was not mid-run when it was killed, so the next run proves nothing"
cp "${repo}/target/controls-check/crates%atomic-core%src%git.rs" "${repo}/crates/atomic-core/src/git.rs"
rm -rf "${repo}/target/controls-check"
run honest braced-esc
[ "${code}" -eq 0 ] && grep -q "^ok braced-esc" <<<"${out}" ||
    fail "after a killed holder: exit ${code}: ${out}"
untouched || fail "after a killed holder: the source is left modified"
# The lock file is not run state, so it does not trip the rule that refuses a
# tree holding a run that did not finish.
[ -e "${repo}/target/controls-check.lock" ] || fail "lock file: the lock is gone from target/"
[ -z "$(git -C "${repo}" status --porcelain -- target)" ] ||
    fail "lock file: the lock is not ignored: $(git -C "${repo}" status --porcelain -- target)"

# A test that leaves a process behind must not leave the tree locked. Cargo and
# everything it starts are given the lock's descriptor closed, so the run's exit
# ends the lock; a descriptor handed down would keep it held until that process
# ended on its own, which the next run reads as one still going.
repo="$(fresh)"
run orphan braced-esc
[ "${code}" -eq 0 ] && grep -q "^ok braced-esc" <<<"${out}" || fail "orphan: exit ${code}: ${out}"
untouched || fail "orphan: the source is left modified"
orphan_pid="$(cat "${repo}/orphan-pid")"
[ -n "${orphan_pid}" ] || fail "orphan: the stand-in noted no process left behind"
run honest git-dir
[ "${code}" -eq 0 ] && grep -q "^ok git-dir" <<<"${out}" ||
    fail "orphan left behind: exit ${code}: ${out}"
# The process this case asked for is this case's to stop. What is asserted is
# that the lock is free again, not that the process is gone: killed, it may
# wait to be reaped by whoever adopted it, and a dead process holds no lock.
kill -KILL "${orphan_pid}" 2>/dev/null
free=1
for _ in $(seq 1 100); do
    if flock -n "${repo}/target/controls-check.lock" -c 'exit 0' 2>/dev/null; then
        free=0
        break
    fi
    sleep 0.1
done
[ "${free}" -eq 0 ] ||
    fail "orphan left behind: pid ${orphan_pid} holds the lock even after it was killed"

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
#   flock -n 9                                      the lock held
#   9>&-                              in run_cargo  the orphan left behind, and
#                                                   the killed holder's cargo
#   command -v flock >/dev/null 2>&1 || die         refuses a run it cannot keep
#                                                   apart from another; not
#                                                   tested, because faking that
#                                                   would hide git and cp too

# scripts/controls-on.py: every control is on in the tree. CI runs it on the
# commit it checks out, where controls-check.sh cannot run (a rebuild per
# control), so a control committed switched off is caught here and not there.
# Each case builds a fixture the way a checkout is: the file as committed, and
# the row that says what switching the control off writes.

# A tree with every control on passes, saying how many it read: a run that
# found no control and said nothing would be the quiet way to lose them all.
repo="$(fresh)"
run_on
[ "${code}" -eq 0 ] &&
    grep -q "^ok: ${#CONTROL_ROWS[@]} controls are on in this tree$" <<<"${out}" ||
    fail "controls on: exit ${code}: ${out}"

# The row the cases about one control write, and the file it names. Its switch
# and its replacement are unlike each other, and neither is a substring of the
# other.
gh_row='git-hardening|crates/atomic-core/src/git.rs|        for setting in HARDENING {|        for setting in HARDENING.iter().take(0) {|guard'
gh_file=crates/atomic-core/src/git.rs

# Commit what the copy holds, so the next case starts from a checked-in tree.
commit() {
    git -C "${repo}" add -A &&
        git -C "${repo}" -c user.name=test -c user.email=test@example.invalid \
            -c commit.gpgSign=false commit -qm committed
}

# switch_off ROW FILE: write the control off in FILE the way the row says, the
# one place it claims. It fails the case when that place is not there, so a
# fixture never asserts against a file it never changed.
switch_off() {
    local row="$1" file="$2" old new
    IFS='|' read -r _ _ old new _ <<<"${row}"
    python3 - "${file}" "${old}" "${new}" <<'PY' || fail "switch_off: ${file} is not switched off"
import sys

path, old, new = sys.argv[1:]
text = open(path, newline="").read()
if text.count(old) != 1:
    sys.exit(f"{old!r} is not the one place it claims in {path}")
open(path, "w", newline="").write(text.replace(old, new, 1))
PY
}

# reflow ROW FILE: put each statement the row's replacement wrote onto a line of
# its own, at the `; ` between them and at the replacement's own indentation,
# which is what rustfmt does to a line holding several of them. The statements
# and their order are what is left; only the layout changes. It fails the case
# when the replacement is not the one place it claims, holds no `; ` to reflow
# at, or would not leave the row's switch text as a line of the reflowed text --
# a fixture that changed nothing, or changed the wrong thing, must not be left to
# assert against a tree it never made.
reflow() {
    local row="$1" file="$2"
    python3 - "${file}" "$(cut -d'|' -f3 <<<"${row}")" "$(cut -d'|' -f4 <<<"${row}")" <<'PY' || fail "reflow: ${file} is not reflowed"
import sys

path, switch, replacement = sys.argv[1:]
text = open(path, newline="").read()
if text.count(replacement) != 1:
    sys.exit(f"{replacement!r} is not the one place it claims in {path}")
indent = replacement[: len(replacement) - len(replacement.lstrip())]
reflowed = replacement.replace("; ", ";\n" + indent)
if reflowed == replacement:
    sys.exit(f"{replacement!r} holds no '; ' to reflow at, so it tests nothing")
if switch not in reflowed.split("\n"):
    sys.exit(f"{switch!r} is not one whole line of the reflowed {reflowed!r}, so the "
             "count would not stay blind to what this case is about")
open(path, "w", newline="").write(text.replace(replacement, reflowed, 1))
PY
}

# A control committed switched off: the commit holds what the row's replacement
# writes where the control was, which is what a `git commit -a` taken while a
# controls-check run was editing leaves behind. The row is named.
repo="$(fresh)"
printf '%s\n' "${gh_row}" >"${repo}/scripts/controls/git-hardening.row"
switch_off "${gh_row}" "${repo}/${gh_file}"
commit
run_on
[ "${code}" -eq 1 ] &&
    grep -q "^FAIL git-hardening: its switch text is not in ${gh_file}, so the control is not the one this row switches off" <<<"${out}" ||
    fail "control committed off: exit ${code}: ${out}"
grep -q "1 row(s) name a control that is not on in this tree" <<<"${out}" ||
    fail "control committed off: it does not count what it refused: ${out}"

# A row whose replacement is a word its file says elsewhere, which is what most
# rows are: switching that off takes the switch text with it, so the count is
# what reports it. Such a row passes with the control on and is named when the
# commit holds it off.
repo="$(fresh)"
atr_row="$(cat "${repo}/scripts/controls/aws-token-relative.row")"
run_on
[ "${code}" -eq 0 ] || fail "a common replacement, on: exit ${code}: ${out}"
switch_off "${atr_row}" "${repo}/crates/atomic-core/src/codex.rs"
commit
run_on
[ "${code}" -eq 1 ] &&
    grep -q "^FAIL aws-token-relative: its switch text is not in crates/atomic-core/src/codex.rs" <<<"${out}" ||
    fail "a common replacement, off: exit ${code}: ${out}"

# A row whose replacement holds its own switch text: switching that off leaves
# the switch where the count looks for it, so the count cannot tell the tree
# off from on and the replacement is what can. The row is allowed with the
# control on, and named once the commit holds it off.
repo="$(fresh)"
be_row='braced-esc|crates/atomic-core/src/redact.rs|    .chain(BRACED_ESC)|    .chain(BRACED_ESC) /*CONTROL_OFF*/|guard'
printf '%s\n' "${be_row}" >"${repo}/scripts/controls/braced-esc.row"
commit
run_on
[ "${code}" -eq 0 ] || fail "a replacement holding its switch, on: exit ${code}: ${out}"
switch_off "${be_row}" "${repo}/crates/atomic-core/src/redact.rs"
commit
run_on
[ "${code}" -eq 1 ] && grep -q "^FAIL braced-esc: " <<<"${out}" ||
    fail "a replacement holding its switch, off: exit ${code}: ${out}"
grep -q "so this tree holds the control switched off" <<<"${out}" ||
    fail "a replacement holding its switch, off: it does not say the control is off: ${out}"

# The same rule when rustfmt has had its turn with the line. Most rows whose
# replacement holds its own switch text write several statements on one line, and
# rustfmt gives each a line of its own: the exact replacement text is then gone
# from a tree that still holds the switch text exactly once, so the count is
# silent and only what is left with the whitespace out of both says the
# statements are in the file. This takes a real row of that shape, switches it
# off, and reflows what was written at the `; ` between its statements the way
# rustfmt would. One of the lines it makes is the switch text itself, byte for
# byte, which is why the count stays silent and what is refused is the reflow.
repo="$(fresh)"
gf_row="$(cat "${repo}/scripts/controls/git-filters.row")"
switch_off "${gf_row}" "${repo}/${gh_file}"
reflow "${gf_row}" "${repo}/${gh_file}"
commit
run_on
[ "${code}" -eq 1 ] &&
    grep -q "^FAIL git-filters: what switching it off writes is in ${gh_file}" <<<"${out}" ||
    fail "a replacement reflowed onto lines: exit ${code}: ${out}"
grep -q "1 row(s) name a control that is not on in this tree" <<<"${out}" ||
    fail "a replacement reflowed onto lines: it does not count what it refused: ${out}"

# A switch that is no longer there: the code moved, so the row points at
# nothing and the control it names is unchecked.
repo="$(fresh)"
printf '%s\n' "${gh_row}" >"${repo}/scripts/controls/git-hardening.row"
sed -i 's|        for setting in HARDENING {|        for setting in HARDENING_MOVED {|' \
    "${repo}/${gh_file}"
commit
run_on
[ "${code}" -eq 1 ] &&
    grep -q "^FAIL git-hardening: its switch text is not in ${gh_file}" <<<"${out}" ||
    fail "the switch moved: exit ${code}: ${out}"

# A switch that meets the file twice: which site the row switches is not fixed,
# so the count is what is refused, not the absence.
repo="$(fresh)"
printf '%s\n' "${gh_row}" >"${repo}/scripts/controls/git-hardening.row"
printf '%s\n' '        for setting in HARDENING {' >>"${repo}/${gh_file}"
commit
run_on
[ "${code}" -eq 1 ] &&
    grep -q "^FAIL git-hardening: its switch text appears 2 times in ${gh_file}, not once" <<<"${out}" ||
    fail "the switch meets twice: exit ${code}: ${out}"

# Everything it cannot read is a refusal, in the same shape and with the same
# reasons as the rows rules in controls-check.sh, so that a control lost to a
# malformed file or a lost file is never reported as one that is on.
repo="$(fresh)"
rm -rf "${repo}/scripts/controls"
run_on
[ "${code}" -eq 1 ] &&
    grep -q "FAIL: ${repo}/scripts/controls is not there, so there is no control to check" <<<"${out}" ||
    fail "controls-on the rows gone: exit ${code}: ${out}"

repo="$(fresh)"
rm -rf "${repo}/scripts/controls"
: >"${repo}/scripts/controls"
run_on
[ "${code}" -eq 1 ] &&
    grep -q "FAIL: ${repo}/scripts/controls is not a directory, so there is no control to check" <<<"${out}" ||
    fail "controls-on the rows path is a file: exit ${code}: ${out}"

repo="$(fresh)"
rm -f "${repo}"/scripts/controls/*.row
run_on
[ "${code}" -eq 1 ] && grep -q "FAIL: ${repo}/scripts/controls holds no control" <<<"${out}" ||
    fail "controls-on no row files: exit ${code}: ${out}"

repo="$(fresh)"
printf '%s\n' 'git-hardening|crates/atomic-core/src/git.rs|a|b|args' \
    >"${repo}/scripts/controls/git-hardening.row.txt"
run_on
[ "${code}" -eq 1 ] &&
    grep -q "scripts/controls/git-hardening.row.txt is not named <name>.row, so its control would never be read" <<<"${out}" ||
    fail "controls-on a row file named otherwise: exit ${code}: ${out}"

repo="$(fresh)"
printf '%s\n%s\n' \
    'git-hardening|crates/atomic-core/src/git.rs|a|b|args' \
    'git-filters|crates/atomic-core/src/git.rs|c|d|args' \
    >"${repo}/scripts/controls/git-hardening.row"
run_on
[ "${code}" -eq 1 ] &&
    grep -q "scripts/controls/git-hardening.row holds 2 rows, not the one it must" <<<"${out}" ||
    fail "controls-on two rows in a file: exit ${code}: ${out}"

repo="$(fresh)"
printf '%s\n' 'git-hardening|crates/atomic-core/src/git.rs|a || b|c|args' \
    >"${repo}/scripts/controls/git-hardening.row"
run_on
[ "${code}" -eq 1 ] &&
    grep -q "scripts/controls/git-hardening.row has 6 '|' separators, not the 4 between its five fields" <<<"${out}" ||
    fail "controls-on a field holding a '|': exit ${code}: ${out}"

repo="$(fresh)"
printf '%s\n' 'git-hardening|crates/atomic-core/src/git.rs|a|args' \
    >"${repo}/scripts/controls/git-hardening.row"
run_on
[ "${code}" -eq 1 ] &&
    grep -q "scripts/controls/git-hardening.row has 3 '|' separators, not the 4 between its five fields" <<<"${out}" ||
    fail "controls-on a row missing a field: exit ${code}: ${out}"

repo="$(fresh)"
printf '%s\n' '|crates/atomic-core/src/git.rs|a|b|args' \
    >"${repo}/scripts/controls/git-hardening.row"
run_on
[ "${code}" -eq 1 ] && grep -q "scripts/controls/git-hardening.row names no control" <<<"${out}" ||
    fail "controls-on a row naming no control: exit ${code}: ${out}"

repo="$(fresh)"
printf '%s\n' 'another-name|crates/atomic-core/src/git.rs|a|b|args' \
    >"${repo}/scripts/controls/git-hardening.row"
run_on
[ "${code}" -eq 1 ] &&
    grep -q "scripts/controls/git-hardening.row names itself 'another-name', which is not its file name" <<<"${out}" ||
    fail "controls-on a row named otherwise: exit ${code}: ${out}"

repo="$(fresh)"
printf '%s\n' 'git-hardening|crates/atomic-core/src/git.rs|a|b|args' \
    >"${repo}/scripts/controls/zsecond.row"
run_on
[ "${code}" -eq 1 ] &&
    grep -q "FAIL zsecond.row: scripts/controls/zsecond.row names the control 'git-hardening', which another row file already names" <<<"${out}" ||
    fail "controls-on two rows naming one control: exit ${code}: ${out}"

repo="$(fresh)"
mkdir -p "${repo}/scripts/controls/sub.row"
run_on
[ "${code}" -eq 1 ] && grep -q "scripts/controls/sub.row is not a regular file" <<<"${out}" ||
    fail "controls-on a directory in the rows: exit ${code}: ${out}"

repo="$(fresh)"
ln -s now-where-it-points.row "${repo}/scripts/controls/dangling.row"
run_on
[ "${code}" -eq 1 ] && grep -q "scripts/controls/dangling.row is not a regular file" <<<"${out}" ||
    fail "controls-on a row that is a link to nothing: exit ${code}: ${out}"

repo="$(fresh)"
printf 'x\xffy\n' >"${repo}/scripts/controls/git-hardening.row"
run_on
[ "${code}" -eq 1 ] && grep -q "scripts/controls/git-hardening.row is not text" <<<"${out}" ||
    fail "controls-on a row that is not text: exit ${code}: ${out}"

repo="$(fresh)"
printf '%s\n' 'under-a-file|crates/atomic-core/src/git.rs/under-a-file.rs|a|b|args' \
    >"${repo}/scripts/controls/under-a-file.row"
run_on
[ "${code}" -eq 1 ] &&
    grep -q "^FAIL under-a-file: scripts/controls/under-a-file.row names crates/atomic-core/src/git.rs/under-a-file.rs, which is not a file and so holds no control" <<<"${out}" ||
    fail "controls-on a row naming a path under a file: exit ${code}: ${out}"

repo="$(fresh)"
printf '%s\n' 'a-directory|crates/atomic-core/src|a|b|args' \
    >"${repo}/scripts/controls/a-directory.row"
run_on
[ "${code}" -eq 1 ] &&
    grep -q "^FAIL a-directory: scripts/controls/a-directory.row names crates/atomic-core/src, which is a directory and so holds no control" <<<"${out}" ||
    fail "controls-on a row naming a directory: exit ${code}: ${out}"

repo="$(fresh)"
rm -f "${repo}/${gh_file}"
run_on
[ "${code}" -eq 1 ] &&
    grep -q "^FAIL git-hardening: scripts/controls/git-hardening.row names ${gh_file}, which is not there" <<<"${out}" &&
    grep -q "^FAIL git-dir: scripts/controls/git-dir.row names ${gh_file}, which is not there" <<<"${out}" ||
    fail "controls-on the file a row names is gone: exit ${code}: ${out}"

# A source that cannot be read is refused as unreadable, never as a control
# that is merely absent, which is the difference between catching a control
# switched off and saying nothing about one. A link to itself is what says so
# here, because a mode that refuses reads is one the user running the check can
# be above.
repo="$(fresh)"
rm -f "${repo}/${gh_file}"
ln -s git.rs "${repo}/${gh_file}"
run_on
[ "${code}" -eq 1 ] &&
    grep -q "^FAIL git-hardening: scripts/controls/git-hardening.row names ${gh_file}, which cannot be read" <<<"${out}" ||
    fail "controls-on the file a row names cannot be read: exit ${code}: ${out}"
! grep -q "which is not there" <<<"${out}" ||
    fail "controls-on the file a row names cannot be read: it is reported as absent: ${out}"

repo="$(fresh)"
printf '\xff\xfe\n' >"${repo}/${gh_file}"
run_on
[ "${code}" -eq 1 ] &&
    grep -q "^FAIL git-hardening: scripts/controls/git-hardening.row names ${gh_file}, which is not text" <<<"${out}" ||
    fail "controls-on the file a row names is not text: exit ${code}: ${out}"

# Two controls switched off in one commit are both named, so a message that
# stops at the first would leave a reviewer to find the second by chance.
repo="$(fresh)"
switch_off "$(cat "${repo}/scripts/controls/git-hardening.row")" "${repo}/${gh_file}"
switch_off "$(cat "${repo}/scripts/controls/braced-esc.row")" \
    "${repo}/crates/atomic-core/src/redact.rs"
commit
run_on
[ "${code}" -eq 1 ] &&
    grep -q "^FAIL git-hardening: " <<<"${out}" &&
    grep -q "^FAIL braced-esc: " <<<"${out}" &&
    grep -q "2 row(s) name a control that is not on in this tree" <<<"${out}" ||
    fail "controls-on two controls off: exit ${code}: ${out}"

# A row that switches nothing switches no control.
repo="$(fresh)"
printf '%s\n' 'git-hardening|crates/atomic-core/src/git.rs||b|args' \
    >"${repo}/scripts/controls/git-hardening.row"
run_on
[ "${code}" -eq 1 ] &&
    grep -q "scripts/controls/git-hardening.row switches nothing, so nothing in ${gh_file} is its control" <<<"${out}" ||
    fail "controls-on a row switching nothing: exit ${code}: ${out}"

# What each case above is the control of, in scripts/controls-on.py. The last
# two are this script's own: every case above asks the copy rather than this
# tree, and a control committed off in this tree stops the run before any case.
# Two handlers are not reached on their own, and both are of a shape another
# case does reach: what cannot be listed (which needs a directory its reader can
# be above), and a row file that cannot be opened (which is how the file a row
# names is tested).
#
#   return None, ... is not there                   the rows directory is gone
#   return None, ... is not a directory             the rows path is a file
#   if not entries                                  no row files
#   if not entry.is_file()                          a directory in the rows, and
#                                                   a link to nothing
#   if not entry.name.endswith(".row")              a row file named otherwise
#   if name in by_name                              two rows naming one control
#   if not name                                     a row naming no control
#   name != entry.name[ :-len(".row")]              a row naming itself otherwise
#   len(lines) != 2 or lines[1] != ""               two rows in one file
#   len(fields) != 5                                a field holding a '|', and a
#                                                   row missing a field
#   except UnicodeDecodeError, on the row           a row that is not text
#   except FileNotFoundError                        the file a row names is gone
#   except NotADirectoryError                       a row naming a path under a
#                                                   file
#   except IsADirectoryError                        a row naming a directory
#   except OSError                                  the file a row names is a
#                                                   link to itself
#   except UnicodeDecodeError, on the source        the file a row names is not
#                                                   text
#   if not switch                                   a row switching nothing
#   if switch in replacement and (                  a replacement holding its own
#       replacement in source or                    switch text, committed off, and
#       "".join(replacement.split())                the same control with its line
#           in "".join(source.split()))             reflowed as rustfmt would
#   if found == 0                                   the switch moved, a control
#                                                   committed off, that control
#                                                   with a replacement its file
#                                                   says elsewhere, and two
#                                                   controls off at once
#   if found > 1                                    the switch meets twice
#   checked += 1, and the ok line                   the count of what was read
#   run_on's cd "${repo}"                           the cases ask the copy
#   if ! on_out="$(python3 ... controls-on.py ...)"  a control committed off in
#                                                   this tree

if [ "${failures}" -ne 0 ]; then
    echo "${failures} controls-check test(s) failed" >&2
    exit 1
fi
echo "controls-check tests passed" >&2
