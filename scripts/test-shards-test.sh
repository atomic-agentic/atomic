#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Tests for scripts/test-shards.py, with a stand-in for cargo, and for
# scripts/cache-trim.py, in a throwaway target directory. No Rust toolchain is
# needed.
#
#   scripts/test-shards-test.sh
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)" || exit 1
trap 'rm -rf "${WORK}"' EXIT
SHARDS="${ROOT}/scripts/test-shards.py"
failures=0

fail() {
    echo "FAIL $*" >&2
    failures=$((failures + 1))
}

# A stand-in for cargo, for the workspace FAKE_WS describes: its packages,
# their targets and each target's tests. It answers `cargo metadata` and
# `cargo test [selectors] [-- --list]` as cargo would, and logs every call to
# FAKE_LOG. FAKE_HIDDEN names a target the workspace's metadata leaves out
# that a plain `cargo test` still runs, as a new kind of target would be;
# FAKE_FAIL names an integration test whose run fails.
mkdir -p "${WORK}/bin"
cat >"${WORK}/bin/cargo" <<'PY'
#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
with open(os.environ["FAKE_LOG"], "a") as log:
    log.write(" ".join(args) + "\n")
with open(os.environ["FAKE_WS"]) as f:
    ws = json.load(f)
targets = [(p["name"], t) for p in ws["packages"] for t in p["targets"]]
if args[0] == "metadata":
    print(json.dumps({"target_directory": "/nowhere", "packages": [
        {"name": p["name"], "targets": [
            {"name": t["name"], "kind": t["kind"], "test": t.get("test", True), "doctest": t.get("doctest", True)}
            for t in p["targets"] if t["name"] != os.environ.get("FAKE_HIDDEN")]}
        for p in ws["packages"]]}))
    sys.exit(0)
if args[0] == "build":
    sys.exit(0)
assert args[0] == "test", args
ours, theirs = (args[1:args.index("--")], args[args.index("--") + 1:]) if "--" in args else (args[1:], [])
listing = "--list" in theirs
picked = set()
if "--lib" in ours: picked.add("lib")
if "--bins" in ours: picked.add("bin")
if "--doc" in ours: picked.add("doc")
named = [ours[i + 1] for i, a in enumerate(ours) if a == "--test"]
default = not (picked or named)
known = {t["name"] for _, t in targets}
for n in named:
    if n not in known:
        print(f"error: no test target named `{n}`")
        sys.exit(101)
code = 0
def emit(header, tests):
    print(header)
    for t in tests:
        print(f"{t}: test")
    print(f"{len(tests)} tests, 0 benchmarks")
for kind in ("lib", "bin", "test"):
    for _, t in targets:
        if t["kind"][0] != kind:
            continue
        if not (default or kind in picked or (kind == "test" and t["name"] in named)):
            continue
        src = {"lib": "unittests src/lib.rs", "bin": "unittests src/main.rs", "test": f"tests/{t['name']}.rs"}[kind]
        header = f"     Running {src} (target/debug/deps/{t['name']}-0123456789abcdef)"
        if listing:
            emit(header, t["tests"])
        elif os.environ.get("FAKE_FAIL") == t["name"]:
            print(header); print("test result: FAILED. 0 passed; 1 failed"); code = 101
if default or "doc" in picked:
    for _, t in targets:
        if t["kind"][0] == "lib" and t.get("doctests"):
            if listing:
                emit(f"   Doc-tests {t['name']}", t["doctests"])
sys.exit(code)
PY
chmod +x "${WORK}/bin/cargo"
export PATH="${WORK}/bin:${PATH}" FAKE_LOG="${WORK}/cargo.log" FAKE_WS="${WORK}/ws.json"
unset FAKE_HIDDEN FAKE_FAIL GITHUB_ACTIONS

cat >"${WORK}/ws.json" <<'EOF'
{"packages": [
  {"name": "pkg-a", "targets": [
    {"name": "pkg_a", "kind": ["lib"], "tests": ["unit::one", "unit::two"], "doctests": ["src/lib.rs - f (line 3)"]},
    {"name": "tool", "kind": ["bin"], "tests": ["main_test"]},
    {"name": "slow", "kind": ["test"], "tests": ["s1", "s2"]},
    {"name": "quick", "kind": ["test"], "tests": ["q1"]},
    {"name": "demo", "kind": ["example"], "test": false, "tests": []},
    {"name": "build-script-build", "kind": ["custom-build"], "tests": []}]},
  {"name": "pkg-b", "targets": [
    {"name": "pkg_b", "kind": ["lib"], "tests": ["b::t"]},
    {"name": "crash", "kind": ["test"], "tests": ["every_failpoint", "sites_listed"]}]}
]}
EOF
table() { printf '%s\n' "$@" >"${WORK}/table.txt"; }
GOOD=('[1]' 'test slow' '' '[2]' 'lib' 'bins' 'doc' 'examples' 'test quick' '' '[crash]' 'test crash')

# expect <exit code> <text> <arguments...>: the script exits so and says that.
expect() {
    local code="$1" text="$2" out
    shift 2
    : >"${FAKE_LOG}"
    out="$(python3 "${SHARDS}" "$@" --table "${WORK}/table.txt" 2>&1)"
    if [ $? -ne "${code}" ] || ! grep -qF -- "${text}" <<<"${out}"; then
        fail "$* with table <$(tr '\n' ' ' <"${WORK}/table.txt")>: expected exit ${code} and '${text}': ${out}"
    fi
}

# A table that places every target once passes both checks.
table "${GOOD[@]}"
expect 0 "the shard table: ok" check-table
expect 0 "every test in exactly one shard: ok" check
expect 0 "unsplit: 10 tests in 7 binaries" check

# The control: an integration test removed from every shard fails both
# checks, and every shard refuses to run.
table '[2]' 'lib' 'bins' 'doc' 'examples' 'test quick' '' '[crash]' 'test crash'
expect 1 "test slow (pkg-a test slow) is in no shard" check-table
expect 1 "test slow (pkg-a test slow) is in no shard" check
expect 1 "is in no shard" run 2
grep -q '^test' "${FAKE_LOG}" && fail "a shard ran with a test target in no shard"
# So do the library, binary and doctest units, and the examples.
for unit in lib bins doc examples; do
    table '[1]' 'test slow' 'test quick' 'test crash' $(printf '%s\n' lib bins doc examples | grep -vx "${unit}")
    expect 1 "${unit} (" check-table
done

# A target in two shards, a line naming nothing, a malformed line, a
# duplicated or empty shard are refused.
table "${GOOD[@]}" '[4]' 'test slow'
expect 1 "test slow is in shard [1] and again in [4]" check-table
table "${GOOD[@]}" 'test gone'
expect 1 "lists test gone, which the workspace does not have" check-table
table "${GOOD[@]}" 'tests slow'
expect 1 "expected lib, bins, doc, examples or \`test <name>\`" check-table
table "${GOOD[@]}" '[1]' 'test other'
expect 1 "shard [1] is defined twice" check-table
table "${GOOD[@]}" '[4]'
expect 1 "shard [4] runs nothing" check-table

# Targets that cargo's output cannot tell apart are refused: two integration
# tests of one name, which `--test` selects together, and two libraries of
# one name, whose doctests carry no hash.
cp "${WORK}/ws.json" "${WORK}/ws-good.json"
table "${GOOD[@]}"
python3 - "${WORK}/ws-good.json" "${WORK}/ws.json" '{"name": "quick", "kind": ["test"], "tests": ["q2"]}' <<'EOF'
import json, sys
ws = json.load(open(sys.argv[1]))
ws["packages"][1]["targets"].append(json.loads(sys.argv[3]))
json.dump(ws, open(sys.argv[2], "w"))
EOF
expect 1 "test target quick is in pkg-a, pkg-b" check-table
python3 - "${WORK}/ws-good.json" "${WORK}/ws.json" <<'EOF'
import json, sys
ws = json.load(open(sys.argv[1]))
ws["packages"][1]["targets"][0]["name"] = "pkg_a"
json.dump(ws, open(sys.argv[2], "w"))
EOF
expect 1 "library pkg_a is in pkg-a, pkg-b" check-table
cp "${WORK}/ws-good.json" "${WORK}/ws.json"

# Tests cargo runs that no shard selects, which the table cannot see, fail
# the list check: the hidden target is left out of the metadata, so the table
# passes, but the unsplit run still lists its tests.
table '[1]' 'test slow' '' '[2]' 'lib' 'bins' 'doc' 'examples' '' '[crash]' 'test crash'
FAKE_HIDDEN=quick expect 0 "the shard table: ok" check-table
FAKE_HIDDEN=quick expect 1 "tests the unsplit run lists that no shard runs:" check
FAKE_HIDDEN=quick expect 1 "quick-0123456789abcdef: 1 test (q1)" check

table "${GOOD[@]}"

# A shard runs its units, each kind of unit as cargo selects it, with the
# features CI's acceptance check uses.
features="$(python3 -c 'import sys; sys.path.insert(0, sys.argv[1]); import acceptance; print(acceptance.FEATURES)' "${ROOT}/scripts")"
expect 0 "" run 2
want="test --workspace --locked --features ${features} --no-fail-fast --lib --bins --test quick
test --workspace --locked --features ${features} --no-fail-fast --doc
build --workspace --locked --features ${features} --examples"
[ "$(grep -v '^metadata' "${FAKE_LOG}")" = "${want}" ] || fail "shard 2 ran: $(cat "${FAKE_LOG}")"
expect 0 "" run crash
[ "$(grep -v '^metadata' "${FAKE_LOG}")" = "test --workspace --locked --features ${features} --no-fail-fast --test crash" ] ||
    fail "the crash shard ran: $(cat "${FAKE_LOG}")"
# A failing command fails the shard, and the commands after it still run.
FAKE_FAIL=quick expect 1 "exited 101" run 2
grep -q -- '--doc' "${FAKE_LOG}" || fail "a failure stopped the shard's other commands"
expect 1 "no shard [9]" run 9
expect 2 "run takes a shard" run
expect 2 "run takes a shard" check 1

# The repository's own table places every test file on disk, so a new one
# fails here too, before any build.
python3 - "${ROOT}" >"${WORK}/ws.json" <<'EOF'
import glob, json, os, sys
root = sys.argv[1]
packages = []
for crate in sorted(glob.glob(os.path.join(root, "crates", "*", "Cargo.toml"))):
    d = os.path.dirname(crate)
    t = [{"name": os.path.basename(d).replace("-", "_"), "kind": ["lib"], "tests": []}]
    if os.path.exists(os.path.join(d, "src", "main.rs")):
        t.append({"name": "main", "kind": ["bin"], "tests": []})
    t += [{"name": os.path.basename(f)[:-3], "kind": ["test"], "tests": []} for f in sorted(glob.glob(os.path.join(d, "tests", "*.rs")))]
    t += [{"name": os.path.basename(f)[:-3], "kind": ["example"], "test": False, "tests": []} for f in sorted(glob.glob(os.path.join(d, "examples", "*.rs")))]
    packages.append({"name": os.path.basename(d), "targets": t})
print(json.dumps({"packages": packages}))
EOF
out="$(python3 "${SHARDS}" check-table 2>&1)" || fail "scripts/test-shards.txt against the test files on disk: ${out}"

# CI runs every shard of the table: the test job's matrix names each shard but
# the crash suite, its job's name counts them, and the crash job runs that one.
out="$(python3 - "${ROOT}" 2>&1 <<'EOF'
import os, re, sys
root = sys.argv[1]
with open(os.path.join(root, "scripts", "test-shards.txt"), encoding="utf-8") as f:
    table = re.findall(r"^\[([A-Za-z0-9_-]+)\]", f.read(), re.M)
with open(os.path.join(root, ".github", "workflows", "ci.yml"), encoding="utf-8") as f:
    ci = f.read()
m = re.search(r"^ {8}shard: \[([^\]]*)\]$", ci, re.M)
matrix = [s.strip() for s in m.group(1).split(",")] if m else []
problems = []
if sorted(matrix + ["crash"]) != sorted(table):
    problems.append(f"the test job's matrix {matrix} and the crash job are not the table's shards {table}")
if f"name: Test (${{{{ matrix.shard }}}}/{len(matrix)})" not in ci:
    problems.append(f"the test job's name does not count its {len(matrix)} shards")
# The summary job ends failed, never skipped, when the run is cancelled.
summary = re.search(r"(?ms)^  rust:\n(.*?)(?=^  \S)", ci)
if not summary or "    if: ${{ always() }}" not in summary.group(1):
    problems.append("the summary job \"Rust (ubuntu-24.04)\" does not run with always()")
for shard in ("${{ matrix.shard }}", "crash"):
    if f"scripts/test-shards.py run {shard}" not in ci:
        problems.append(f"no step runs `scripts/test-shards.py run {shard}`")
print("\n".join(problems))
sys.exit(1 if problems else 0)
EOF
)" || fail ".github/workflows/ci.yml against scripts/test-shards.txt: ${out}"

# cache-trim.py keeps the dependencies' entries in each profile and removes
# the workspace's (by the hashes of their fingerprints), and all else.
T="${WORK}/target"
mkdir -p "${T}/debug/deps" "${T}/debug/build/serde-1111/out" "${T}/debug/build/pkg-a-2222/out" \
    "${T}/debug/.fingerprint/serde-1111" "${T}/debug/.fingerprint/pkg-a-3333" "${T}/debug/.fingerprint/pkg-a-2222" \
    "${T}/debug/incremental/pkg_a-x" "${T}/debug/examples" "${T}/v8-mirror/v1" "${T}/tmp" "${T}/release/deps"
touch "${T}/debug/deps/libserde-1111.rlib" "${T}/debug/deps/serde-1111.d" "${T}/debug/deps/libpkg_a-3333.rlib" \
    "${T}/debug/deps/libpkg_a-3333.rmeta" "${T}/debug/deps/slow-3333" "${T}/debug/deps/slow-3333.d" \
    "${T}/debug/tool" "${T}/debug/tool.d" "${T}/debug/examples/demo" "${T}/v8-mirror/v1/librusty_v8.a" \
    "${T}/CACHEDIR.TAG" "${T}/tmp/scratch" "${T}/release/deps/libpkg_a-4444.rlib" "${T}/release/deps/libserde-5555.rlib"
mkdir -p "${T}/release/.fingerprint/pkg-a-4444" "${T}/release/.fingerprint/serde-5555"
printf '{"target_directory": "%s", "packages": [{"name": "pkg-a"}, {"name": "pkg-b"}]}' "${T}" >"${WORK}/meta.json"
# Outside CI, and without --force, it refuses and removes nothing.
before="$(cd "${T}" && find . | LC_ALL=C sort)"
python3 "${ROOT}/scripts/cache-trim.py" --metadata "${WORK}/meta.json" >/dev/null 2>&1
[ $? -eq 2 ] || fail "cache-trim.py ran outside CI without --force"
[ "$(cd "${T}" && find . | LC_ALL=C sort)" = "${before}" ] || fail "cache-trim.py removed files when it refused"
GITHUB_ACTIONS=true python3 "${ROOT}/scripts/cache-trim.py" --metadata "${WORK}/meta.json" >/dev/null || fail "cache-trim.py failed in CI"
python3 "${ROOT}/scripts/cache-trim.py" --force --metadata "${WORK}/meta.json" >/dev/null || fail "cache-trim.py --force failed"
got="$(cd "${T}" && find . -mindepth 1 | LC_ALL=C sort | tr '\n' ' ')"
want="./CACHEDIR.TAG ./debug ./debug/.fingerprint ./debug/.fingerprint/serde-1111 ./debug/build ./debug/build/serde-1111 ./debug/build/serde-1111/out ./debug/deps ./debug/deps/libserde-1111.rlib ./debug/deps/serde-1111.d ./release ./release/.fingerprint ./release/.fingerprint/serde-5555 ./release/deps ./release/deps/libserde-5555.rlib "
[ "${got}" = "${want}" ] || fail "cache-trim.py left: ${got}"

# Only pushes to main save caches: every use of rust-cache-save in ci.yml is
# gated on main, and nothing else in the workflows or rust-setup saves one,
# whether by actions/cache/save, by actions/cache's own post step, or by
# another cache action. Checked against the repository's files, then against
# copies with the gate removed and with a save added, which must fail.
cat >"${WORK}/saves.py" <<'EOF'
import re, sys
problems = []
ci = sys.argv[1]
text = open(ci, encoding="utf-8").read()
# The steps of ci.yml's jobs, each from its `      - ` line to the next.
steps = re.split(r"(?m)^(?=      - )", text)
saves = [s for s in steps if "uses: ./.github/actions/rust-cache-save" in s]
if not saves:
    problems.append(f"{ci}: no step uses rust-cache-save")
for step in saves:
    m = re.search(r"(?m)^\s+if: (.*)$", step)
    cond = m.group(1) if m else ""
    if "github.ref == 'refs/heads/main'" not in cond or "||" in cond:
        problems.append(f"{ci}: a rust-cache-save step is not gated on main: {step.strip().splitlines()[0]}")
    # Nor saved before the build it saves has succeeded: gated on a step that
    # builds, not only on the setup.
    if not re.search(r"steps\.(?!setup\.)[\w-]+\.outcome == 'success'", cond):
        problems.append(f"{ci}: a rust-cache-save step is not gated on its build: {cond}")
for path in sys.argv[1:]:
    for n, line in enumerate(open(path, encoding="utf-8"), 1):
        if re.search(r"uses:\s*(actions/cache(/save)?@|Swatinem/rust-cache|mozilla-actions/sccache)", line):
            problems.append(f"{path}:{n}: saves a cache outside rust-cache-save: {line.strip()}")
print("\n".join(problems))
sys.exit(1 if problems else 0)
EOF
CI="${ROOT}/.github/workflows/ci.yml"
SETUP="${ROOT}/.github/actions/rust-setup/action.yml"
CODEQL="${ROOT}/.github/workflows/codeql.yml"
out="$(python3 "${WORK}/saves.py" "${CI}" "${SETUP}" "${CODEQL}" 2>&1)" || fail "cache saves: ${out}"
sed "s/ \&\& github.ref == 'refs\/heads\/main'//" "${CI}" >"${WORK}/ci-ungated.yml"
cmp -s "${CI}" "${WORK}/ci-ungated.yml" && fail "the control found no main gate to remove"
python3 "${WORK}/saves.py" "${WORK}/ci-ungated.yml" "${SETUP}" "${CODEQL}" >/dev/null 2>&1 &&
    fail "a rust-cache-save step not gated on main passed"
sed "s/ \&\& steps.clippy.outcome == 'success' \&\& steps.clippy-features.outcome == 'success'/ \&\& steps.setup.outcome == 'success'/" "${CI}" >"${WORK}/ci-early.yml"
cmp -s "${CI}" "${WORK}/ci-early.yml" && fail "the control found no clippy gate to replace"
python3 "${WORK}/saves.py" "${WORK}/ci-early.yml" "${SETUP}" "${CODEQL}" >/dev/null 2>&1 &&
    fail "a save gated only on the setup passed"
sed 's|actions/cache/restore@|actions/cache/save@|' "${SETUP}" >"${WORK}/setup-saves.yml"
python3 "${WORK}/saves.py" "${CI}" "${WORK}/setup-saves.yml" "${CODEQL}" >/dev/null 2>&1 &&
    fail "a cache save in rust-setup passed"
sed 's|actions/cache/restore@|actions/cache@|' "${CODEQL}" >"${WORK}/codeql-saves.yml"
python3 "${WORK}/saves.py" "${CI}" "${SETUP}" "${WORK}/codeql-saves.yml" >/dev/null 2>&1 &&
    fail "actions/cache, which saves in its post step, passed in codeql.yml"

if [ "${failures}" -ne 0 ]; then
    echo "scripts/test-shards-test.sh: ${failures} failure(s)" >&2
    exit 1
fi
echo "scripts/test-shards-test.sh: ok"
