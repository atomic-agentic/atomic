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

# A stand-in for cargo. FAKE_CARGO says how it behaves: `honest` fails the
# test while any control is switched off, `blind` always passes it, `lose`
# also deletes the saved originals, `partial` fails only one of a
# control's tests, `unreadable` makes the manifest
# unreadable, `kill` kills the script outright, and `hang` waits, with the
# control off, until it is stopped.
mkdir -p "${WORK}/bin"
cat >"${WORK}/bin/cargo" <<'SH'
#!/usr/bin/env bash
off() {
    grep -qE 'HARDENING\.iter\(\)\.take\(0\)|ATOMIC_CONTROL_REMOVED|let names: Vec<String> = \{ let _ = names|PermissionProfile::Disabled|exclude_slash_tmp\*/ false|paths\.into_iter\(\)\.take\(0\)|black_box\(false\)' \
        crates/atomic-core/src/git.rs crates/atomic-core/src/codex.rs crates/atomic-exec/src/lib.rs
}
case "${FAKE_CARGO}" in
lose) rm -f target/controls-check/crates* ;;
unreadable) chmod 000 target/controls-check/manifest ;;
kill) kill -KILL "${PPID}"; exit 0 ;;
hang) : >cargo-started; exec sleep 300 ;;
partial)
    if off; then
        echo "test one ... FAILED"
        echo "test two ... ok"
        echo "test result: FAILED. 1 passed; 1 failed"
        exit 101
    fi
    ;;
esac
if [ "${FAKE_CARGO}" != blind ] && off; then
    echo "test guard ... FAILED"
    echo "test result: FAILED. 0 passed; 1 failed"
    exit 101
fi
echo "test guard ... ok"
echo "test result: ok. 1 passed; 0 failed"
SH
chmod +x "${WORK}/bin/cargo"

# A fresh copy of the repository's part the script touches, committed.
fresh() {
    local repo="${WORK}/repo"
    chmod -R u+w "${repo}" 2>/dev/null
    rm -rf "${repo}"
    mkdir -p "${repo}/scripts" "${repo}/crates/atomic-core/src" "${repo}/crates/atomic-exec/src"
    cp "${ROOT}/scripts/controls-check.sh" "${repo}/scripts/"
    cp "${ROOT}/crates/atomic-core/src/git.rs" "${ROOT}/crates/atomic-core/src/codex.rs" \
        "${repo}/crates/atomic-core/src/"
    cp "${ROOT}/crates/atomic-exec/src/lib.rs" "${repo}/crates/atomic-exec/src/"
    git -C "${repo}" init -q
    git -C "${repo}" -c user.name=test -c user.email=test@example.invalid add -A
    git -C "${repo}" -c user.name=test -c user.email=test@example.invalid -c commit.gpgSign=false \
        commit -qm base
    echo "${repo}"
}

# run MODE ARGS...: run the script in the copy; sets `code` and `out`.
run() {
    local mode="$1"
    shift
    out="$(cd "${repo}" && FAKE_CARGO="${mode}" PATH="${WORK}/bin:${PATH}" \
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
[ "$(grep -c '^ok ' <<<"${out}")" -eq 7 ] || fail "honest: not seven controls ok: ${out}"
untouched || fail "honest: the source is left modified"
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

# An unknown name is refused, and nothing is edited.
repo="$(fresh)"
run honest no-such-control
[ "${code}" -eq 2 ] && grep -q "no control is named 'no-such-control'" <<<"${out}" ||
    fail "unknown name: exit ${code}: ${out}"
untouched || fail "unknown name: the source is edited"

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

if [ "${failures}" -ne 0 ]; then
    echo "${failures} controls-check test(s) failed" >&2
    exit 1
fi
echo "controls-check tests passed" >&2
