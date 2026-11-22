#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Tests for scripts/controls-touched.py, in throwaway git repositories. No
# Rust toolchain is needed.
#
#   scripts/controls-touched-test.sh
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)" || exit 1
trap 'rm -rf "${WORK}"' EXIT
TOUCHED="${ROOT}/scripts/controls-touched.py"
failures=0

fail() {
    echo "FAIL $*" >&2
    failures=$((failures + 1))
}

# A repository whose first commit holds three rows on two files: a and b
# switch src/one.rs, c switches src/two.rs with other cargo arguments.
REPO="${WORK}/repo"
fresh() {
    rm -rf "${REPO}"
    mkdir -p "${REPO}/scripts/controls" "${REPO}/src"
    cd "${REPO}" || exit 1
    git init -q -b main
    git config user.name test
    git config user.email test@example.invalid
    git config commit.gpgsign false
    git config core.hooksPath /dev/null
    printf 'fn one() { guard_a(); guard_b(); }\n' >src/one.rs
    printf 'fn two() { guard_c(); }\n' >src/two.rs
    printf 'fn other() {}\n' >src/other.rs
    echo 'a|src/one.rs|guard_a();|/*off*/|-p x --lib -- test_a' >scripts/controls/a.row
    echo 'b|src/one.rs|guard_b();|/*off*/|-p x --lib -- test_b' >scripts/controls/b.row
    echo 'c|src/two.rs|guard_c();|/*off*/|-p y --test t -- test_c' >scripts/controls/c.row
    git add -A
    git commit -qm base
    BASE="$(git rev-parse HEAD)"
}
commit() {
    git add -A
    git commit -qm change --allow-empty
}
# Run the script from BASE to HEAD; its output in OUT, its error in ERR.
run() {
    code=0
    python3 "${SCRIPT:-${TOUCHED}}" "${BASE}" HEAD "$@" >"${WORK}/out" 2>"${WORK}/err" || code=$?
    OUT="$(cat "${WORK}/out")"
    ERR="$(cat "${WORK}/err")"
}
expect() {
    local what="$1" want="$2"
    if [ "${code}" -ne 0 ] || [ "${OUT}" != "${want}" ]; then
        fail "${what}: wanted '${want}', got '${OUT}' (exit ${code}, ${ERR})"
    fi
}
refused() {
    local what="$1" says="$2"
    if [ "${code}" -ne 1 ] || ! grep -qF -- "${says}" <<<"${ERR}"; then
        fail "${what}: wanted a refusal saying '${says}', got exit ${code}: ${ERR}"
    fi
}

# Each rule is tested by removing it: with a rule taken out of a copy of the
# script, its case above passes what it should not.
mkdir -p "${WORK}/off"
off() {
    local name="$1" from="$2" to="$3"
    SCRIPT="${WORK}/off/${name}.py"
    python3 - "${TOUCHED}" "${SCRIPT}" "${from}" "${to}" <<'PY'
import sys
src, dst, old, new = sys.argv[1:]
text = open(src).read()
if old not in text:
    sys.exit(f"the text to switch off is not in {src}: {old}")
open(dst, "w").write(text.replace(old, new, 1))
PY
}
fresh
printf 'fn other() { 1; }\n' >src/other.rs
commit
run
expect "a change to no row's file" ""

fresh
echo 'b|src/one.rs|guard_b();|/*off*/|-p x --lib -- test_b test_b2' >scripts/controls/b.row
commit
run
expect "a changed row" "b"

fresh
printf 'fn one() { guard_a(); guard_b(); } // moved\n' >src/one.rs
commit
run
expect "a change to the file two rows switch" "$(printf 'a\nb')"

fresh
echo 'd|src/other.rs|fn other|fn nothing|-p x --lib -- test_d' >scripts/controls/d.row
commit
run
expect "an added row" "d"

# Deletions.
fresh
git rm -q scripts/controls/b.row
commit
run
refused "a row deleted while its switch still applies" "scripts/controls/b.row is deleted, but its switch's text is still in src/one.rs"

fresh
git rm -q scripts/controls/b.row
printf 'fn one() { guard_a(); }\n' >src/one.rs
commit
run
expect "a row deleted with its guard" "a"
run --report
expect "a row deleted with its guard, reported" "deleted b: its text is in no file at the head"

fresh
git rm -q scripts/controls/c.row src/two.rs
commit
run
expect "a row deleted with its file" ""
run --report
expect "a row deleted with its file, reported" "deleted c: its text is in no file at the head"

fresh
git mv scripts/controls/c.row scripts/controls/c2.row
sed -i 's/^c|/c2|/' scripts/controls/c2.row
commit
run
expect "a renamed row" "c2"
run --report
expect "a renamed row, reported" "deleted c: c2 switches the same text in src/two.rs"

fresh
git mv src/one.rs src/renamed.rs
git rm -q scripts/controls/a.row scripts/controls/b.row
commit
run
refused "a file renamed with its rows deleted" "scripts/controls/a.row is deleted, but its switch's text is still in src/renamed.rs"

fresh
printf 'fn one() { guard_a(); }\n' >src/one.rs
printf 'fn other() { guard_b(); }\n' >src/other.rs
git rm -q scripts/controls/b.row
commit
run
refused "a switch moved to another file with its row deleted" "scripts/controls/b.row is deleted, but its switch's text is still in src/other.rs"

fresh
git mv src/one.rs src/renamed.rs
sed -i 's|src/one.rs|src/renamed.rs|' scripts/controls/a.row scripts/controls/b.row
commit
run
expect "a file renamed with its rows" "$(printf 'a\nb')"

# A change to a guarding test selects its row, wherever the test is.
fresh
echo 'e|src/two.rs|guard_c();|/*off*/|-p y --lib mod::tests::the_guard_holds -- --nocapture' >scripts/controls/e.row
printf '#[test]\nfn the_guard_holds() {}\n' >src/tests.rs
commit
BASE="$(git rev-parse HEAD)"
printf '#[test]\nfn the_guard_holds_renamed() {}\n' >src/tests.rs
commit
run
expect "a changed test the row names" "e"

fresh
echo 'e|src/two.rs|guard_c();|/*off*/|-p y --lib -- a::the_guard_holds' >scripts/controls/e.row
printf '#[test]\nfn the_guard_holds() {}\n' >src/tests.rs
commit
BASE="$(git rev-parse HEAD)"
printf '#[test]\nfn renamed() {}\n' >src/tests.rs
commit
run
expect "a test the row names, renamed away" "e"
off test-names 'if any(test and test in text' 'if any(False and test in text'
run
[ "${OUT}" = "" ] || fail "with the test-name rule off, a renamed test still selected: ${OUT}"
unset SCRIPT

fresh
mkdir -p crates/k/tests/t
printf 'mod helper;\n' >crates/k/tests/t.rs
printf 'pub fn help() {}\n' >crates/k/tests/t/helper.rs
printf 'fn other() {}\n' >crates/k/tests/tt.rs
commit
BASE="$(git rev-parse HEAD)"
printf 'pub fn help() { 1; }\n' >crates/k/tests/t/helper.rs
commit
run
expect "a change under the row's --test target" "c"
off test-target 'if parts[3] == f"{target}.rs" or (parts[3] == target and len(parts) > 4):' 'if False:'
run
[ "${OUT}" = "" ] || fail "with the --test target rule off, a change under it still selected: ${OUT}"
unset SCRIPT
fresh
mkdir -p crates/k/tests
printf 'fn other() {}\n' >crates/k/tests/tt.rs
commit
BASE="$(git rev-parse HEAD)"
printf 'fn other() { 1; }\n' >crates/k/tests/tt.rs
commit
run
expect "a change to another --test target" ""

# Every row, for a full sweep.
fresh
code=0
python3 "${TOUCHED}" --all HEAD >"${WORK}/out" 2>"${WORK}/err" || code=$?
OUT="$(cat "${WORK}/out")"
expect "every row" "$(printf 'a\nb\nc')"
python3 "${TOUCHED}" --all "${BASE}" HEAD >/dev/null 2>&1 && fail "--all with two revisions was accepted"
printf 'b: needs a device the runner lacks\n' >scripts/controls-local.txt
commit
code=0
python3 "${TOUCHED}" --all HEAD >"${WORK}/out" 2>"${WORK}/err" || code=$?
OUT="$(cat "${WORK}/out")"
expect "a sweep leaves a local-only row out" "$(printf 'a\nc')"
code=0
python3 "${TOUCHED}" --all HEAD --report >"${WORK}/out" 2>"${WORK}/err" || code=$?
OUT="$(cat "${WORK}/out")"
expect "a sweep reports a local-only row" "not run in CI b: needs a device the runner lacks"

# Rows a CI runner cannot run.
fresh
printf '# comment\n\nb: needs a device the runner lacks\n' >scripts/controls-local.txt
printf 'fn one() { guard_a(); guard_b(); } // moved\n' >src/one.rs
commit
run
expect "a local-only row is not run" "a"
run --report
expect "a local-only row is reported" "not run in CI b: needs a device the runner lacks"

fresh
printf 'gone: no such row\n' >scripts/controls-local.txt
commit
run
refused "a local-only line naming no row" "names 'gone', which is not a row"

fresh
printf 'a\n' >scripts/controls-local.txt
commit
run
refused "a local-only line with no reason" "is not '<row>: <why"

fresh
printf 'a: one\na: two\n' >scripts/controls-local.txt
commit
run
refused "a local-only row named twice" "names 'a' a second time"

# Row files the script cannot read.
fresh
printf 'a|src/one.rs|guard_a();|/*off*/|-p x\na|src/one.rs|x|y|z\n' >scripts/controls/a.row
commit
run
refused "a row file with two rows" "holds 2 rows"

fresh
echo 'a|src/one.rs|guard_a();|-p x' >scripts/controls/a.row
commit
run
refused "a row with four fields" "has 3 '|' separators"

fresh
echo 'z|src/one.rs|guard_a();|/*off*/|-p x' >scripts/controls/a.row
commit
run
refused "a row named after another file" "names itself 'z'"

fresh
echo 'note' >scripts/controls/README
commit
run
refused "a file that is not a row" "is not named <name>.row"

fresh
ln -s a.row scripts/controls/l.row
commit
run
refused "a link to another control's row" "scripts/controls/l.row names itself 'a'"

# A link is followed within the tree, as controls-check.sh's `[ -f ]` follows
# it; one that leads nowhere readable is refused.
fresh
mkdir -p elsewhere
echo 'l|src/one.rs|guard_a();|/*off*/|-p x --lib -- test_l' >elsewhere/l.row
ln -s ../../elsewhere/l.row scripts/controls/l.row
printf 'fn one() { guard_a(); guard_b(); } // moved\n' >src/one.rs
commit
run
expect "a row file linked within the tree" "$(printf 'a\nb\nl')"

fresh
ln -s nowhere.row scripts/controls/l.row
commit
run
refused "a link to nothing" "scripts/controls/nowhere.row, which a row file links to, is not in the tree"

fresh
ln -s l.row scripts/controls/l.row
commit
run
refused "a link to itself" "scripts/controls/l.row is a link that does not end"

fresh
ln -s ../../../outside.row scripts/controls/l.row
commit
run
refused "a link out of the repository" "is reached by a link out of the repository"

fresh
mkdir -p elsewhere/l.row
echo x >elsewhere/l.row/x
ln -s ../../elsewhere/l.row scripts/controls/l.row
commit
run
refused "a link to a directory" "elsewhere/l.row is not a regular file"

fresh
printf '|src/one.rs|guard_a();|/*off*/|-p x\n' >scripts/controls/.row
commit
run
refused "a row naming no control" "scripts/controls/.row names no control"

fresh
mkdir scripts/controls/d.row
echo 'x' >scripts/controls/d.row/x
commit
run
refused "a directory among the rows" "scripts/controls/d.row is not a regular file"

# Shards: every row in exactly one, in an order that does not change, and
# rows built alike kept together.
fresh
for n in 1 2 3 4 5 6 7; do
    echo "p${n}|src/other.rs|fn other|fn o${n}|-p z --lib -- t${n}" >"scripts/controls/p${n}.row"
done
printf 'fn other() { 2; }\n' >src/other.rs
commit
run
all="${OUT}"
[ "$(wc -l <<<"${all}")" -eq 7 ] || fail "seven touched rows, got: ${all}"
joined=""
for i in 1 2 3; do
    run --shards 3 --shard "${i}"
    [ "${code}" -eq 0 ] || fail "shard ${i} of 3 failed: ${ERR}"
    first="${OUT}"
    run --shards 3 --shard "${i}"
    [ "${OUT}" = "${first}" ] || fail "shard ${i} of 3 changed between runs"
    n="$(grep -c . <<<"${OUT}")"
    [ "${n}" -ge 2 ] && [ "${n}" -le 3 ] || fail "shard ${i} of 3 holds ${n} rows, not 2 or 3"
    joined+="${OUT}"$'\n'
done
[ "$(sort <<<"${joined%$'\n'}")" = "$(sort <<<"${all}")" ] ||
    fail "the shards do not hold each row once: ${joined}"
run --shards 1 --shard 1
expect "one shard holds every row" "${all}"
run --shards 3 --shard 4
[ "${code}" -ne 0 ] || fail "a shard past the last was accepted"
run --shards 3
[ "${code}" -ne 0 ] || fail "--shards without --shard was accepted"

fresh
echo 'a|src/one.rs|guard_a();|/*off*/|-p y --test t -- test_a' >scripts/controls/a.row
printf 'fn one() { guard_a(); guard_b(); } // x\n' >src/one.rs
printf 'fn two() { guard_c(); } // x\n' >src/two.rs
commit
run --shards 2 --shard 2
expect "rows built alike share a shard" "$(printf 'a\nc')"

fresh
printf 'fn one() { guard_a(); guard_b(); } // moved\n' >src/one.rs
commit
off file 'or row["file"] in changed' ''
run
[ "${OUT}" = "" ] || fail "with the target-file rule off, a change to src/one.rs still touched: ${OUT}"
unset SCRIPT

fresh
git rm -q scripts/controls/b.row
commit
off deletion 'if unheld:' 'if False:'
run
[ "${code}" -eq 0 ] || fail "with the deletion rule off, the deletion was still refused: ${ERR}"
unset SCRIPT

fresh
git mv src/one.rs src/renamed.rs
git rm -q scripts/controls/a.row scripts/controls/b.row
commit
off deletion-file 'unheld = [f for f in files if' 'unheld = [f for f in files if f == gone["file"] and'
run
[ "${code}" -eq 0 ] || fail "with the deletion rule held to the row's own file, a rename was still refused: ${ERR}"
unset SCRIPT

fresh
printf 'gone: no such row\n' >scripts/controls-local.txt
commit
off local 'raise Refused(f"{LOCAL}:{n}: names '"'"'{name}'"'"', which is not a row")' 'pass'
run
[ "${code}" -eq 0 ] || fail "with the local-only name rule off, the line was still refused: ${ERR}"
unset SCRIPT

if [ "${failures}" -ne 0 ]; then
    echo "scripts/controls-touched-test.sh: ${failures} failed" >&2
    exit 1
fi
echo "scripts/controls-touched-test.sh: all passed"
