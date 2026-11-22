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
    # A copy that could not be made is no rule removed: say so, rather than
    # let the case run against nothing and read as passing.
    [ $? -eq 0 ] || fail "off ${name}: the text to switch off is not in ${TOUCHED}"
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
[ "${code}" -eq 0 ] && [ "${OUT}" = "" ] || fail "with the test-name rule off, a renamed test still selected: ${OUT}"
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
[ "${code}" -eq 0 ] && [ "${OUT}" = "" ] || fail "with the --test target rule off, a change under it still selected: ${OUT}"
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

# A change to test code a crate's targets share selects each row whose
# --test target is in that crate, and only those.
shared_fixture() {
    fresh
    mkdir -p crates/k/tests/support crates/k/tests/t crates/k/tests/u crates/m/tests/fixtures
    printf 'mod support;\n' >crates/k/tests/t.rs
    printf 'pub fn help() {}\n' >crates/k/tests/support/mod.rs
    printf 'pub fn own() {}\n' >crates/k/tests/t/own.rs
    printf 'fn main() {}\n' >crates/k/tests/u/main.rs
    printf 'fn m() {}\n' >crates/m/tests/v.rs
    printf 'data\n' >crates/m/tests/fixtures/one.txt
    echo 'f|src/one.rs|guard_a();|/*off*/|-p k --test u -- test_f' >scripts/controls/f.row
    echo 'g|src/one.rs|guard_b();|/*off*/|-p m --test v -- test_g' >scripts/controls/g.row
    commit
    BASE="$(git rev-parse HEAD)"
}
shared_fixture
printf 'pub fn help() { 1; }\n' >crates/k/tests/support/mod.rs
commit
run
expect "a change to a crate's shared test code" "$(printf 'c\nf')"
off shared 'crates.add(parts[1])
    return crates' 'pass
    return crates'
run
[ "${code}" -eq 0 ] && [ "${OUT}" = "" ] || fail "with the shared test code rule off, a change to it still selected: ${OUT}"
unset SCRIPT

shared_fixture
printf '{}\n' >crates/k/tests/data.json
commit
BASE="$(git rev-parse HEAD)"
printf '{"changed": true}\n' >crates/k/tests/data.json
commit
run
expect "a change to a non-Rust file at the top of a crate's tests" "$(printf 'c\nf')"
off shared-file 'if not parts[3].endswith(".rs"):
                crates.add(parts[1])' 'if not parts[3].endswith(".rs"):
                pass'
run
[ "${code}" -eq 0 ] && [ "${OUT}" = "" ] || fail "with the top-level shared file rule off, a change to it still selected: ${OUT}"
unset SCRIPT

# A shared test helper outside tests/, listed with the rows it serves.
helper_fixture() {
    fresh
    mkdir -p crates/h/src crates/k/tests
    printf 'pub fn fake() {}\n' >crates/h/src/testing.rs
    printf 'fn t() {}\n' >crates/k/tests/t.rs
    echo 'p|src/other.rs|fn other|fn o1|-p h --lib --features fake-model -- test_p' >scripts/controls/p.row
    echo 'q|src/other.rs|fn other|fn o2|--workspace --features x/postgres-tests,x/fake-model --test t -- test_q' >scripts/controls/q.row
    echo 'r|src/other.rs|fn other|fn o3|-p h --lib --features postgres-tests -- test_r' >scripts/controls/r.row
    printf '# helpers\ncrates/h/src/testing.rs: %s\n' "$1" >scripts/controls-shared.txt
    commit
    BASE="$(git rev-parse HEAD)"
}
helper_fixture '--test feature=fake-model'
printf 'pub fn fake() { 1; }\n' >crates/h/src/testing.rs
commit
run
expect "a listed helper serves --test rows and its feature's rows" "$(printf 'c\np\nq')"
off helper 'or any(serves(row, words) for words in helpers)' ''
run
[ "${code}" -eq 0 ] && [ "${OUT}" = "" ] || fail "with the listed-helper rule off, a change to it still selected: ${OUT}"
unset SCRIPT

helper_fixture 'feature=postgres-tests'
printf 'pub fn fake() { 1; }\n' >crates/h/src/testing.rs
commit
run
expect "a helper serving a feature, named in a list of features" "$(printf 'q\nr')"

helper_fixture 'package=h'
printf 'pub fn fake() { 1; }\n' >crates/h/src/testing.rs
commit
run
expect "a helper serving a package: its rows and the workspace's" "$(printf 'p\nq\nr')"
off package 'if word.startswith("package=") and (built is None' 'if False and (built is None'
run
[ "${code}" -eq 0 ] && [ "${OUT}" = "" ] || fail "with the package rule off, a change to the helper still selected: ${OUT}"
unset SCRIPT

# Each way cargo is told a feature turns it on.
for spelling in '-F fake-model' '-Ffake-model' '-F=fake-model' '--features=y/fake-model' '--all-features'; do
    helper_fixture 'feature=fake-model'
    git rm -q scripts/controls/p.row scripts/controls/q.row
    echo "s|src/other.rs|fn other|fn o4|-p h --lib ${spelling} -- test_s" >scripts/controls/s.row
    commit
    BASE="$(git rev-parse HEAD)"
    printf 'pub fn fake() { 1; }\n' >crates/h/src/testing.rs
    commit
    run
    expect "a feature turned on with ${spelling}" "s"
done

helper_fixture '--test'
printf 'crates/h/src/testing.rs: feature=\n' >scripts/controls-shared.txt
commit
run
refused "a helper line naming an empty feature" "'feature=' is none of --test"

helper_fixture '--test'
git rm -q crates/h/src/testing.rs
commit
run
refused "a listed helper that is gone" "names crates/h/src/testing.rs, which is not there"

helper_fixture '--test'
printf 'crates/h/src/testing.rs: everything\n' >scripts/controls-shared.txt
commit
run
refused "a helper line serving what is not known" "'everything' is none of --test, feature=<name> and package=<name>"

helper_fixture '--test'
printf 'crates/h/src/testing.rs\n' >scripts/controls-shared.txt
commit
run
refused "a helper line serving nothing" "is not '<path>: <--test, feature=<name> or package=<name>> ...'"

shared_fixture
printf 'data, changed\n' >crates/m/tests/fixtures/one.txt
commit
run
expect "a change to a crate's fixtures" "g"

shared_fixture
printf 'pub fn own() { 1; }\n' >crates/k/tests/t/own.rs
commit
run
expect "a change to one target's own module" "c"

shared_fixture
printf 'fn main() { 1; }\n' >crates/k/tests/u/main.rs
commit
run
expect "a change to a target's own directory" "f"

# Literal Rust includes select the source that consumes the changed bytes.
include_fixture() {
    fresh
    mkdir -p crates/k/src/mounts
    printf '%s\n' "$1" >crates/k/src/mounts/tests.rs
    printf 'before\n' >crates/k/src/mounts/host.mountinfo
    echo 'e|src/other.rs|fn other|fn nothing|-p k --lib -- include_guard' >scripts/controls/e.row
    commit
    BASE="$(git rev-parse HEAD)"
}
for kind in include_bytes include_str include; do
    include_fixture "fn include_guard() { ${kind}!(\"host.mountinfo\"); }"
    printf 'after\n' >crates/k/src/mounts/host.mountinfo
    commit
    run
    expect "${kind} data selects the guarding includer" "e"
done
include_fixture 'fn include_guard() { include_bytes!(r##"host.mountinfo"##); }'
printf '\377after\n' >crates/k/src/mounts/host.mountinfo
commit
run
expect "raw Rust path and binary included bytes" "e"

fresh
mkdir -p crates/k/tests/model_auth
printf 'fn model_auth_test() {}\n' >crates/k/tests/model_auth.rs
printf '#[path = "model_auth/fixture.rs"]\nmod fixture;\n' >crates/k/tests/model_auth_sandbox.rs
printf 'pub fn fixture() {}\n' >crates/k/tests/model_auth/fixture.rs
printf 'm|src/other.rs|fn other|fn a|-p k --test model_auth -- model_auth_test\n' >scripts/controls/m.row
printf 'n|src/other.rs|fn other|fn b|-p k --test model_auth_sandbox -- sandbox_test\n' >scripts/controls/n.row
commit
BASE="$(git rev-parse HEAD)"
printf 'pub fn fixture() { 1; }\n' >crates/k/tests/model_auth/fixture.rs
commit
run
expect "path-included helper selects both integration targets" "$(printf 'm\nn')"

# A library includes data under tests/: that is not shared integration data.
fresh
mkdir -p crates/k/src crates/k/tests/typescript
printf 'fn include_guard() { include_str!("../tests/typescript/projection.json"); }\n' >crates/k/src/typescript.rs
printf 'fn main() {}\n' >crates/k/tests/t.rs
printf '{}\n' >crates/k/tests/typescript/projection.json
echo 'e|src/other.rs|fn other|fn a|-p k --lib -- include_guard' >scripts/controls/e.row
commit
BASE="$(git rev-parse HEAD)"
printf '{"new": true}\n' >crates/k/tests/typescript/projection.json
commit
run
expect "library's included test data selects library rows only" "e"

# Resolve from the includer's directory, not from a matching basename.
include_fixture 'fn include_guard() { include_str!("host.mountinfo"); }'
mkdir -p crates/k/src/elsewhere
printf 'fn unrelated_guard() { include_str!("host.mountinfo"); }\n' >crates/k/src/elsewhere/tests.rs
printf 'elsewhere\n' >crates/k/src/elsewhere/host.mountinfo
echo 'z|src/other.rs|fn other|fn z|-p k --lib -- unrelated_guard' >scripts/controls/z.row
commit
BASE="$(git rev-parse HEAD)"
printf 'after\n' >crates/k/src/mounts/host.mountinfo
commit
run
expect "equal included basenames do not confuse ownership" "e"
off includers 'changed |= included' 'pass'
run
[ "${code}" -eq 0 ] && [ "${OUT}" = "" ] || fail "with includer expansion off, included data still selected: ${OUT}"
unset SCRIPT

# Rust escaped path spellings resolve to the same tracked file.
for path in 'host\x2emountinfo' 'host\u{2e}mountinfo'; do
    include_fixture "fn include_guard() { include_str!(\"${path}\"); }"
    printf 'after\n' >crates/k/src/mounts/host.mountinfo
    commit
    run
    expect "escaped Rust path ${path}" "e"
done

owned_data_fixture() {
    shared_fixture
    mkdir -p crates/k/tests/coding-tasks/tasks crates/k/tests/fixtures/project
    printf 'fn main() { std::path::Path::new("tests/coding-tasks/tasks"); }\n' >crates/k/tests/coding_tasks.rs
    printf 'fn main() { std::path::Path::new("tests/fixtures/project"); }\n' >crates/k/tests/t.rs
    printf 'data\n' >crates/k/tests/coding-tasks/tasks/input.txt
    printf 'data\n' >crates/k/tests/fixtures/project/input.txt
    commit
    BASE="$(git rev-parse HEAD)"
}
owned_data_fixture
printf 'new\n' >crates/k/tests/coding-tasks/tasks/input.txt
commit
run
expect "directory read by a target with no rows selects no unrelated rows" ""
owned_data_fixture
printf 'new\n' >crates/k/tests/fixtures/project/input.txt
commit
run
expect "fixture directory selects the target that names it" "c"
off data-readers 'if target and literal.startswith("tests/"):' 'if False:'
run
[ "${code}" -eq 0 ] && [ "${OUT}" = "$(printf 'c\nf')" ] || fail "with named data readers off, shared fallback was not retained: ${OUT}"
unset SCRIPT
off data-ownership 'if any(references_to(path, refs) for refs in references):' 'if False:'
run
[ "${code}" -eq 0 ] && [ "${OUT}" = "$(printf 'c\nf')" ] || fail "with data ownership off, shared fallback was not retained: ${OUT}"
unset SCRIPT
owned_data_fixture
printf 'fn main() { std::path::Path::new("tests/fixtures/project"); }\n' >crates/k/tests/u/main.rs
commit
BASE="$(git rev-parse HEAD)"
printf 'new\n' >crates/k/tests/fixtures/project/input.txt
commit
run
expect "a directory shared by two named targets selects both" "$(printf 'c\nf')"

# Comments cannot turn shared data into one target's private data.
for comment in '// "tests/fixtures/project"' '/* outer /* "tests/fixtures/project" */ end */'; do
    shared_fixture
    mkdir -p crates/k/tests/fixtures/project
    printf '%s\nfn main() {}\n' "${comment}" >crates/k/tests/t.rs
    printf 'before\n' >crates/k/tests/fixtures/project/input.txt
    commit
    BASE="$(git rev-parse HEAD)"
    printf 'after\n' >crates/k/tests/fixtures/project/input.txt
    commit
    run
    expect "a Rust comment does not claim a data directory" "$(printf 'c\nf')"
    if [[ "${comment}" == //* ]]; then
        off comment 'if text.startswith("//", i):' 'if False:'
    else
        off comment 'if text.startswith("/*", i):
            depth, i = 1, i + 2' 'if False:
            depth, i = 1, i + 2'
    fi
    run
    [ "${code}" -eq 0 ] && [ "${OUT}" = c ] || fail "with comment exclusion off, comment did not expose false ownership: ${OUT} (${ERR})"
    unset SCRIPT
done

include_fixture 'fn include_guard() { include_str!("host.mountinfo"); }'
ln -s loop.rs crates/k/src/loop.rs
commit
run
refused "a looping Rust source cannot silently lose its includers" "crates/k/src/loop.rs is a link that does not end"
include_fixture 'fn include_guard() { include_str!("host.mountinfo"); }'
printf '\377\n' >crates/k/src/mounts/tests.rs
commit
run
refused "non-UTF-8 Rust source refuses dependency discovery" "is not UTF-8"
for path in 'host\q.mountinfo' 'host\u{D800}.mountinfo' 'host\u{110000}.mountinfo'; do
    include_fixture "fn include_guard() { include_str!(\"${path}\"); }"
    printf 'after\n' >crates/k/src/mounts/host.mountinfo
    commit
    run
    refused "an undecodable Rust include path is not guessed" "Rust path escape"
done
include_fixture 'fn include_guard() { include_str!("host\0mountinfo"); }'
printf 'after\n' >crates/k/src/mounts/host.mountinfo
commit
run
refused "a NUL in an include path is refused" "has a NUL in a Rust path"

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

# A row taken off the list is run, though nothing else of it changed.
fresh
printf 'b: needs a device the runner lacks\n' >scripts/controls-local.txt
commit
BASE="$(git rev-parse HEAD)"
printf '# none\n' >scripts/controls-local.txt
commit
run
expect "a row taken off the local-only list" "b"
off released 'if name in released' 'if False'
run
[ "${code}" -eq 0 ] && [ "${OUT}" = "" ] || fail "with the released-row rule off, a row taken off the list still ran: ${OUT}"
unset SCRIPT

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
[ "${code}" -eq 0 ] && [ "${OUT}" = "" ] || fail "with the target-file rule off, a change to src/one.rs still touched: ${OUT}"
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
