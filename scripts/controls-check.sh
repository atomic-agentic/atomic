#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Controls are tested by removing them. For each v0.1 control, this switches
# the control off in the source, runs the test that guards it and requires that
# test to fail, then puts the source back and requires it to pass again.
#
# Every control is one file under scripts/controls/, named <name>.row and
# holding its one row, so that two branches adding a control add two files and
# never edit the same line. Add a control by adding a file there; the rows are
# read in file-name order.
#
#   scripts/controls-check.sh            # every control
#   scripts/controls-check.sh git-dir    # one, by name
#
# Run it inside scripts/dev.sh with the sandbox available, since some of the
# controls are the sandbox's:
#
#   ATOMIC_DEV_SANDBOX=on scripts/dev.sh scripts/controls-check.sh
#
# It hashes files with git, so in a linked worktree the main checkout's .git
# must be mounted too (ATOMIC_DEV_MOUNTS=<main checkout>/.git); without it
# the script stops before editing anything.
#
# A control whose code has moved, so that its switch no longer applies, fails
# the check: update the switch in its file under scripts/controls/ with the
# code. Each control costs a rebuild of the crates it is in.
#
# The source is edited in place, so the script guards it. It refuses to start
# when another run holds the tree's lock in target/controls-check.lock, because
# two runs switch the same files under each other; it releases that lock when it
# exits, however it exits. It refuses to start when a file a control names
# differs from HEAD, because it would restore that dirty state and read as
# clean. Before any edit it copies every file it will
# touch to target/controls-check/ and checks each copy against the file's git
# hash-object; if that fails, nothing is edited. On exit, however it ends, it
# copies the originals back, checks each file's hash again and asks git that the
# file no longer differs from HEAD, and fails loudly, keeping the copies, if
# either fails. Each file put back is touched, so cargo rebuilds it rather than
# reusing an artefact built with the control off. A run killed outright leaves
# target/controls-check/ behind, and the next run refuses to start until someone
# has looked. scripts/controls-check-test.sh tests this.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT}"
STATE="${ROOT}/target/controls-check"
LOG="${STATE}/cargo.log"

# Each control's row lives in its own file: scripts/controls/<name>.row holds
# one line, name | file | text to replace | replacement | cargo test arguments.
# A control guarded by several tests names them all after `--`; each must fail
# with the control off. A row's arguments are taken as written, with no
# expansion, so a row that needs the sandbox and Postgres suites names their
# features itself. A row switches off the first place its text is found, so
# where one control has two sites the text of each is unlike the other's
# (stop-own-tool and stop-own-tool-wait), and there is a row for each.
ROWS="${ROOT}/scripts/controls"

die() {
    echo "scripts/controls-check.sh: $*" >&2
    exit 1
}

# Fail before any edit, leaving nothing behind. Only a run that holds the lock
# may call this: it removes the state directory, which is what a running peer
# is keeping its originals in.
die_unedited() {
    rm -rf "${STATE}" 2>/dev/null
    die "$*; nothing was edited"
}

# One run at a time in a tree. Two runs switch the same files, each saving its
# own originals: the one that finishes last puts back what it found, which is
# the other's switched-off source, and each reads a failing test as its own
# control caught. So the first to start holds a lock for the whole run and the
# rest refuse. The lock is the kernel's, on a descriptor this process owns, so
# a run that is killed outright releases it; it lives beside
# ${STATE} rather than inside it, because ${STATE} is deleted on the way out
# and deleting a lock's file lets a second holder in beside the first.
LOCK="${ROOT}/target/controls-check.lock"
command -v flock >/dev/null 2>&1 ||
    die "flock is not available, so this run cannot be kept apart from another; nothing was edited"
mkdir -p "${ROOT}/target" || die "cannot create ${ROOT}/target for the run's lock; nothing was edited"
if ! exec 9>>"${LOCK}"; then
    die "cannot open ${LOCK} for the run's lock; nothing was edited"
fi
# The refusal must not remove anything: a run that holds the lock is keeping
# what it has saved in ${STATE}, which die_unedited would delete. And there is
# nothing here for anyone to clean up: the kernel frees the lock when the run
# that took it ends, however it ends, and removing the file would free nothing
# while letting a third run lock a fresh file beside the run still holding the
# old one.
flock -n 9 ||
    die "${LOCK} is held by another scripts/controls-check.sh run in this tree; it frees itself when that run ends, so wait for it and do not remove ${LOCK}; nothing was edited"

# No git is handed the lock's descriptor, as no cargo is. A git whose worktree
# has core.fsmonitor set starts a fsmonitor daemon out of `git diff` or
# `git ls-files`, which outlives the command that started it; given the
# descriptor it would keep the tree locked after this run had exited, which the
# next run reads as one still going. Nothing else the run starts can do that:
# cp, touch, tr, find, sort, grep, sed, awk, rm, mkdir, python3 and flock all
# answer and go, and none of them leaves a process behind the run.
git() {
    command git "$@" 9>&-
}

# Where a file's original is kept.
copy_of() {
    echo "${STATE}/$(tr / % <<<"$1")"
}

# Put a file's saved original back, and make its mtime now. A plain cp already
# does that, so cargo rebuilds what it built with the control off; the touch
# keeps that true for a cp that preserves timestamps (cp -p, or an alias).
put_back() {
    cp -- "$(copy_of "$1")" "$1" && touch -- "$1"
}

# git's answer for how a file stands against HEAD: 0 when it matches the
# commit, 1 when it differs, and anything else when git could not be asked --
# in a linked worktree whose main checkout's .git is not mounted, for instance.
# A file git does not track, or one marked assume-unchanged or skip-worktree,
# counts as differing: git diff would say nothing about it either way.
head_diff() {
    local code=0 listed
    listed="$(git ls-files -v -- "$1")" || return 2
    case "${listed}" in
        "H "*) ;;
        *) return 1 ;;
    esac
    git diff --quiet HEAD -- "$1" || code=$?
    return "${code}"
}

# Read every control from its own file under ${ROWS}. Each rule here fails
# closed: a row the script cannot read is a control it would otherwise never
# check and still report as passing, so every refusal names the file and stops
# the run before anything is edited.
rows=()
[ -d "${ROWS}" ] || die "${ROWS} is not a directory, so there is no control to check"
entries=()
while IFS= read -r -d '' entry; do
    entries+=("${entry}")
done < <(find "${ROWS}" -mindepth 1 -maxdepth 1 -print0 | LC_ALL=C sort -z)
[ "${#entries[@]}" -gt 0 ] || die "${ROWS} holds no control"
names=()
for entry in "${entries[@]}"; do
    base="${entry##*/}"
    [ -f "${entry}" ] || die "${entry} is not a regular file"
    case "${base}" in
    *.row) ;;
    *) die "${entry} is not named <name>.row, so its control would never be read" ;;
    esac
    lines=()
    mapfile -t lines <"${entry}" || die "cannot read ${entry}"
    [ "${#lines[@]}" -eq 1 ] ||
        die "${entry} holds ${#lines[@]} rows, not the one it must"
    row="${lines[0]}"
    # Five fields, so four separators: a `|` inside a field would move what
    # follows it into the wrong one.
    bars="${row//[^|]/}"
    [ "${#bars}" -eq 4 ] ||
        die "${entry} has ${#bars} '|' separators, not the 4 between its five fields; a field may not hold a '|'"
    name="${row%%|*}"
    [ -n "${name}" ] || die "${entry} names no control"
    # Before the name's own rule: two files naming one control are refused for
    # the name, which is what makes the second one wrong.
    case " ${names[*]-} " in
    *" ${name} "*)
        die "${entry} names the control '${name}', which another row file already names"
        ;;
    esac
    [ "${name}" = "${base%.row}" ] ||
        die "${entry} names itself '${name}', which is not its file name"
    names+=("${name}")
    rows+=("${row}")
done

# The controls to check: all of them, or the one named.
only="${1:-}"
selected=()
for row in "${rows[@]}"; do
    name="${row%%|*}"
    if [ -z "${only}" ] || [ "${only}" = "${name}" ]; then
        selected+=("${row}")
    fi
done
if [ "${#selected[@]}" -eq 0 ]; then
    echo "scripts/controls-check.sh: no control is named '${only}'. The controls:" >&2
    for row in "${rows[@]}"; do
        echo "  ${row%%|*}" >&2
    done
    exit 2
fi

if [ -e "${STATE}" ]; then
    die "${STATE} is left from a run that did not finish. Its manifest names each file that run edited, with the file's original hash, and the directory holds the originals: compare them with the files (git diff), put them back, then remove ${STATE}."
fi

# Refuse to start on a file that already differs from HEAD. The run saves what
# it finds and puts that back, so a file dirty before the start would end the
# run dirty again and read as a clean restore; and a switch applied to someone
# else's uncommitted change is not this control. HEAD is the baseline for the
# whole run, so ask for it before anything is edited.
dirty=" "
seen=" "
for row in "${selected[@]}"; do
    IFS='|' read -r _ file _ <<<"${row}"
    case "${seen}" in *" ${file} "*) continue ;; esac
    code=0
    head_diff "${file}" || code=$?
    if [ "${code}" -eq 1 ]; then
        dirty+="${file} "
    elif [ "${code}" -ne 0 ]; then
        die_unedited "cannot compare ${file} with HEAD"
    fi
    seen+="${file} "
done
[ "${dirty}" = " " ] ||
    die_unedited "these files differ from HEAD:${dirty% *} (a file git does not track as committed counts). A run edits them and puts back what it found, so it would restore that state and read as a clean restore; commit them or put them back first"

# Save every file the selected controls edit, and check each copy, before
# editing anything.
mkdir -p "${STATE}" || die_unedited "cannot create ${STATE}"
: >"${STATE}/manifest" || die_unedited "cannot write ${STATE}/manifest"
recorded=" "
for row in "${selected[@]}"; do
    IFS='|' read -r _ file _ <<<"${row}"
    case "${recorded}" in *" ${file} "*) continue ;; esac
    hash="$(git hash-object -- "${file}")" || die_unedited "cannot hash ${file}"
    copy="$(copy_of "${file}")"
    cp -- "${file}" "${copy}" || die_unedited "cannot save ${file}"
    [ "$(git hash-object -- "${copy}")" = "${hash}" ] ||
        die_unedited "the copy of ${file} does not match it"
    echo "${hash} ${file}" >>"${STATE}/manifest" || die_unedited "cannot record ${file}"
    recorded+="${file} "
done

# Put every saved file back and check it. A file that cannot be put back is
# named, and the originals stay. The saved copies and the manifest are this
# script's own state, so neither is asked what the source should be: after the
# hash, git says whether the file matches HEAD, and a restore that leaves it
# differing is a failure even when it matches the copy. Each file is touched on
# the way back, so the next cargo run rebuilds it.
restore() {
    local bad=0 hash file code
    if [ ! -r "${STATE}/manifest" ]; then
        echo "scripts/controls-check.sh: ${STATE}/manifest cannot be read, so no file can be checked. Compare the files with git and ${STATE} by hand." >&2
        return 1
    fi
    while read -r hash file; do
        cp -- "$(copy_of "${file}")" "${file}" 2>/dev/null
        if [ "$(git hash-object -- "${file}" 2>/dev/null)" != "${hash}" ]; then
            echo "scripts/controls-check.sh: ${file} IS LEFT MODIFIED: it does not match its original, ${hash}. Put it back by hand; ${STATE} keeps what was saved." >&2
            bad=1
            continue
        fi
        code=0
        head_diff "${file}" || code=$?
        if [ "${code}" -ne 0 ]; then
            if [ "${code}" -eq 1 ]; then
                echo "scripts/controls-check.sh: ${file} IS LEFT DIFFERENT FROM HEAD: it matches the copy made at the start, which was not HEAD. Put it back from HEAD; ${STATE} keeps what was saved." >&2
            else
                echo "scripts/controls-check.sh: cannot compare ${file} with HEAD, so it cannot be shown to be back as committed." >&2
            fi
            bad=1
            continue
        fi
        touch -- "${file}" 2>/dev/null
    done <"${STATE}/manifest"
    if [ "${bad}" -eq 0 ]; then
        rm -rf "${STATE}"
    fi
    return "${bad}"
}
on_exit() {
    local code=$?
    # Nothing interrupts putting the files back.
    trap '' INT TERM HUP
    restore || code=1
    exit "${code}"
}
trap on_exit EXIT

# cargo runs in the background so that a signal reaches the script while it
# waits (bash would hold a signal until a foreground command returned, and a
# time limit's later SIGKILL would then leave a control switched off). The
# signal stops cargo, and the exit puts the files back.
cargo_pid=""
on_signal() {
    trap '' INT TERM HUP
    [ -n "${cargo_pid}" ] && kill -TERM "${cargo_pid}" 2>/dev/null
    echo "scripts/controls-check.sh: interrupted; putting the files back" >&2
    exit 130
}
trap on_signal INT TERM HUP
run_cargo() {
    # shellcheck disable=SC2086
    # Every test binary runs, however an earlier one ends: a row may name
    # tests in several, and each must be seen to fail.
    #
    # The lock's descriptor is closed for cargo and everything it starts: a
    # test that leaves a process behind would otherwise keep holding the tree
    # after this run has exited.
    cargo test --locked --no-fail-fast ${1} 9>&- >"${LOG}" 2>&1 &
    cargo_pid=$!
    wait "${cargo_pid}"
    local code=$?
    cargo_pid=""
    return "${code}"
}
# The tests a row's cargo arguments name: those after `--`, or else its
# positional filters.
named_tests() {
    local after=0 skip=0 a
    # shellcheck disable=SC2086
    for a in ${1}; do
        if [ "${skip}" -eq 1 ]; then
            skip=0
            continue
        fi
        # After `--`, the test binary's own flags, such as `--nocapture`, are
        # not tests.
        if [ "${after}" -eq 1 ]; then
            case "${a}" in
            -*) ;;
            *) echo "${a}" ;;
            esac
            continue
        fi
        case "${a}" in
        --) after=1 ;;
        -p | --package | --features | --test | --bin | --example) skip=1 ;;
        -*) ;;
        *) echo "${a}" ;;
        esac
    done
}

# Keep a failing control's cargo output, which the exit would delete.
keep_log() {
    cp -- "${LOG}" "${ROOT}/target/controls-check-$1.log" 2>/dev/null &&
        echo "  cargo's output: target/controls-check-$1.log" >&2
}

# Switch a control off: replace the first `old` in `file` with `new`.
switch_off() {
    python3 - "$1" "$2" "$3" <<'PY'
import sys
path, old, new = sys.argv[1:]
text = open(path).read()
open(path, "w").write(text.replace(old, new, 1))
PY
}

status=0
for row in "${selected[@]}"; do
    IFS='|' read -r name file text replacement args <<<"${row}"
    if ! grep -qF -- "${text}" "${file}"; then
        echo "FAIL ${name}: its switch no longer applies to ${file}; update ${ROWS}/${name}.row" >&2
        status=1
        continue
    fi
    switch_off "${file}" "${text}" "${replacement}" || die "cannot switch ${name} off"
    run_cargo "${args}"
    removed=$?
    put_back "${file}" || die "cannot put ${file} back"
    if grep -q "^error\(\[E[0-9]*\]\)\?: could not compile\|^error: could not compile" "${LOG}"; then
        echo "FAIL ${name}: with the control off the code does not build" >&2
        grep -m5 -E "^error" "${LOG}" >&2
        keep_log "${name}"
        status=1
        continue
    fi
    if ! grep -q "^test result:" "${LOG}"; then
        echo "FAIL ${name}: no test ran" >&2
        grep -m5 -E "^error" "${LOG}" >&2
        keep_log "${name}"
        status=1
        continue
    fi
    # Every test the row names must fail with the control off.
    passed="$(awk '/^test result:/ { p += $4 } END { print p + 0 }' "${LOG}")"
    if [ "${removed}" -eq 0 ] || [ "${passed}" -ne 0 ] || ! grep -q "^test .* FAILED$" "${LOG}"; then
        echo "FAIL ${name}: its test passed with the control off" >&2
        keep_log "${name}"
        status=1
        continue
    fi
    # And each one it names is seen failing: one that matched no test, or
    # was never run, is not a test that caught the control's removal.
    unfailed=""
    while read -r test; do
        # The name as it is, not as a pattern: a failed test's path holds it.
        # And read all of it, so `grep -F` does not stop at the first match:
        # an earlier stage of the pipe would then die of SIGPIPE, which
        # pipefail reports as a failure of the pipeline, and a test that did
        # fail would read as one that was not seen. A name may have spaces
        # in it, as a doctest's does.
        if ! grep -E '^test .+ \.\.\. FAILED$' "${LOG}" | sed -E 's/^test (.+) \.\.\. FAILED$/\1/' |
            grep -F -- "${test}" >/dev/null; then
            unfailed="${unfailed} ${test}"
        fi
    done < <(named_tests "${args}")
    if [ -n "${unfailed}" ]; then
        echo "FAIL ${name}: with the control off, these tests were not seen failing:${unfailed}" >&2
        keep_log "${name}"
        status=1
        continue
    fi
    if ! run_cargo "${args}"; then
        echo "FAIL ${name}: its test fails with the control on" >&2
        grep -E "^test .* FAILED$|panicked at" "${LOG}" >&2
        keep_log "${name}"
        status=1
        continue
    fi
    echo "ok ${name}: its test fails with the control off and passes with it on" >&2
done
exit "${status}"
