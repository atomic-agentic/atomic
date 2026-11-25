#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Tests for scripts/controls-exec.py, the controls-as-an-exec-step driver: a
# throwaway repository whose rows switch a synthetic file, the real
# scripts/controls-check.sh run as the step's trusted proof runner, and a
# stand-in for cargo that answers the way cargo does. No Rust toolchain, no
# network and no model are needed.
#
#   scripts/controls-exec-test.sh
#   scripts/controls-exec-test.sh guard-tree-equality
#   scripts/controls-exec-test.sh guard-runner-pin
#
# Every case runs the driver as an exec atom's command is run: JSON on stdin,
# one JSON value on stdout, stderr for diagnostics, and the three pins the step
# is declared with -- this file's bytes, the proof runner's bytes, and which
# file holds them. The candidate is bound to a ref of the shape atomic keeps a
# coding step's attempt under, so the cases say what a step would: the commit
# atomic recorded, and nothing else. The rows' tests are the stand-in's; the
# binding, the switch, the copy, the restore and the verdicts are all real.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)" || exit 1
trap 'chmod -R u+w "${WORK}" 2>/dev/null; rm -rf "${WORK}"' EXIT
DRIVER="${ROOT}/scripts/controls-exec.py"
REPO="${WORK}/repo"
LOG="${WORK}/cargo-args"
# Where the step's declaration points the proof runner: outside every candidate
# made here, as a workflow's atom file names a script of the harness's.
HARNESS="${WORK}/harness"
# The ref atomic keeps a coding step's attempt under, and the base it records
# with it. Every case that gets as far as switching anything binds to this.
REF="refs/atomic/run-1/steps/code/1"
# Every file any row here switches, so the stand-in cargo can tell whether a
# control is off, whichever file its row points at.
CONTROL_FILES="src/guard.rs src/other.rs src/slow.rs src/stub.rs src/lose.rs src/swap.rs src/probe.rs"
failures=0

fail() {
    echo "FAIL $*" >&2
    failures=$((failures + 1))
}

[ -f "${DRIVER}" ] || {
    echo "${DRIVER} is not there" >&2
    exit 1
}
mkdir -p "${HARNESS}"
cp "${ROOT}/scripts/controls-check.sh" "${HARNESS}/controls-check.sh"
chmod 755 "${HARNESS}/controls-check.sh"
sha_of() { sha256sum "$1" | cut -d' ' -f1; }
RUNNER="${HARNESS}/controls-check.sh"
RUNNER_SHA="$(sha_of "${RUNNER}")"
DRIVER_SHA="$(sha_of "${DRIVER}")"
[ -n "${RUNNER_SHA}" ] || {
    echo "the proof runner's hash could not be taken" >&2
    exit 1
}
# Nothing here needs the network, and a suite that could reach it would read a
# real host as part of its answer.
unset HTTP_PROXY HTTPS_PROXY ALL_PROXY http_proxy https_proxy all_proxy

# ---------------------------------------------------------------- the tools

# pins [RUNNER [RUNNER_SHA [DRIVER_SHA]]]: the pins a case runs with. Passing an
# empty one stands for a declaration that names nothing, which is a refusal and
# not a default.
pins() {
    PINS=(
        "CONTROL_RUNNER=${1-${RUNNER}}"
        "CONTROL_RUNNER_SHA256=${2-${RUNNER_SHA}}"
        "CONTROL_DRIVER_SHA256=${3-${DRIVER_SHA}}"
    )
}
pins

# A stand-in for cargo. Each test a row names is answered as fixture/cargo.json
# in the tree being checked says it should be:
#
#   fails-off   fails while the file the row switches differs from HEAD, as a
#               test that really guards the control does
#   always-fail fails however that file stands
#   blind       passes however it stands: the test that never noticed
#   missing     runs nothing, so nothing of it is seen failing
#   build       with the control off, nothing compiles
#   hang        never answers, for a row past its time limit
#   flood       writes tens of megabytes with the control back, for the cap on
#               what a row may write
#   swap        replaces the proof runner it is being run through, as a test
#               that found its own check script would
#   probe       asks, from inside the copy while the row is running, how that
#               copy is bound to the candidate -- what its own object store
#               holds, whether HEAD and the index say the same thing, and
#               whether anything in the checkout it stands in for moved. It
#               writes what it finds to $PROBE_OUT for the case to read, and
#               fails as a guard's test must when the copy is not what the step
#               says it is
#
# It logs each call's arguments to $CARGO_LOG, outside the copy, so a case can
# say how often cargo was asked, and notes its own pid to $CARGO_PID so a case
# can ask whether anything of a stopped row is left.
mkdir -p "${WORK}/bin"
cat >"${WORK}/bin/cargo" <<'PY'
#!/usr/bin/env python3
import glob
import json
import os
import signal
import subprocess
import sys
import time

args = sys.argv[1:]
with open(os.environ["CARGO_LOG"], "a") as log:
    log.write(" ".join(args) + "\n")
with open("fixture/cargo.json") as handle:
    kinds = json.load(handle)


def control_off():
    """Whether a file the rows in this tree switch is switched off right now."""
    for path in os.environ.get("CONTROL_FILES", "").split():
        if not os.path.exists(path):
            continue
        done = subprocess.run(
            ["git", "diff", "--quiet", "--", path],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        if done.returncode != 0:
            return True
    return False


def ask(*args):
    """Ask git one question and answer with its stdout, or nothing at all.

    Run from inside the copy, which is the tree the row is switching. A git
    that cannot be asked answers with nothing, which every reader below treats
    as the thing it was asked about being absent, as the row's own guard does.
    """
    done = subprocess.run(["git", *args], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    return done.stdout.decode("utf-8", "replace")


def own_objects():
    """What the copy's own object store holds, counted by type.

    Only the loose objects under this repository's `.git/objects` are counted,
    directory by directory: what the copy can read through an alternate is not
    what it holds, and a commit made here shows up here.
    """
    counts = {"commit": 0, "tree": 0, "blob": 0}
    root = os.path.join(".git", "objects")
    try:
        entries = sorted(os.listdir(root))
    except OSError:
        return counts
    for entry in entries:
        path = os.path.join(root, entry)
        if entry in ("info", "pack") or not os.path.isdir(path):
            continue
        for leaf in sorted(os.listdir(path)):
            kind = ask("cat-file", "-t", entry + leaf).strip()
            if kind in counts:
                counts[kind] += 1
    return counts


def copy_bound(off):
    """Why this copy is not the candidate the step says it is; empty means it is.

    Asked while the row is running, from inside the copy, which is where the
    claim has to hold: the step says it copied the candidate's tree out of the
    object store and bound the copy to that commit without making a commit of
    its own anywhere inside the sandbox.
    """
    wrong = []
    objects = own_objects()
    if objects["commit"]:
        wrong.append(f"the copy holds {objects['commit']} commit object(s) of its own")
    if not ask("rev-parse", "--verify", "-q", "HEAD").strip():
        wrong.append("the copy has no HEAD, so nothing here is bound to a candidate")
    if not ask("rev-parse", "--verify", "-q", "HEAD^{tree}").strip():
        wrong.append("the copy's HEAD names no tree")
    if not ask("ls-files", "--", "scripts/controls-check.sh").strip():
        wrong.append("the copy's index does not hold the files it was copied with")
    # What git says is not as it holds it committed. With a control switched
    # off, the one file that row names is meant to differ from HEAD, and the
    # step's own holder for this row's runner link is meant to be there
    # untracked; with nothing switched, neither may be.
    listed = [
        line
        for line in ask("status", "--porcelain").splitlines()
        if line.strip()
        and ".controls-exec-runner-" not in line
        and not (off and "src/probe.rs" in line)
    ]
    if listed:
        wrong.append(
            (
                "the copy holds something it does not hold committed: "
                if off
                else "the copy is not as its tree holds it with nothing switched: "
            )
            + "; ".join(listed)
        )
    for path in ("README", "scripts/controls-check.sh", "fixture/cargo.json"):
        if not os.path.exists(path):
            wrong.append(f"the copy holds no {path}")
    return wrong


def probe(off):
    """Record what the copy looks like from inside it, and what is wrong with it.

    The facts go to `$PROBE_OUT`, one phase per line as `key=value` fields the
    case can read exactly; anything wrong goes to `$PROBE_OUT.wrong` in its own
    words, so that a reason with a space in it cannot move a field into the
    wrong place. Both phases are recorded whatever they answer, because a case
    that only ever saw the good answer would not know the difference.
    """
    wrong = copy_bound(off)
    report = [
        "off" if off else "on",
        "head=" + ask("rev-parse", "--verify", "-q", "HEAD").strip(),
        "tree=" + ask("rev-parse", "--verify", "-q", "HEAD^{tree}").strip(),
        "index=" + str(len(ask("ls-files").splitlines())),
        "commits=" + str(own_objects()["commit"]),
        "blobs=" + str(own_objects()["blob"]),
    ]
    with open(os.environ.get("PROBE_OUT", os.devnull), "a") as handle:
        handle.write(" ".join(report) + "\n")
    if wrong:
        with open(os.environ.get("PROBE_OUT", os.devnull) + ".wrong", "a") as handle:
            handle.write(("off" if off else "on") + ": " + "; ".join(wrong) + "\n")
    return wrong


names, after, skip = [], 0, 0
for a in args:
    if skip:
        skip = 0
        continue
    if after:
        if not a.startswith("-"):
            names.append(a)
        continue
    if a == "--":
        after = 1
        continue
    if a in ("-p", "--package", "--features", "--test", "--bin", "--example", "--color"):
        skip = 1
code = 0
for name in names:
    kind = kinds.get(name, "fails-off")
    if kind == "missing":
        print("test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out")
        continue
    if kind == "lose" and control_off():
        # The run loses the originals it saved, as a build that cleans up
        # after itself does: the script cannot put the file back and says so.
        for entry in os.listdir("target/controls-check"):
            if entry != "manifest":
                os.remove(os.path.join("target/controls-check", entry))
    if kind == "swap" and not control_off():
        # Writes through the link the row is being run through, which is the
        # trusted runner itself: what the driver checks again when the row ends.
        found = glob.glob(
            os.path.join(
                os.environ.get("TMPDIR", ""),
                "controls-exec-*",
                "copy",
                ".controls-exec-runner-*",
                "controls-check.sh",
            )
        )
        for path in found:
            with open(path, "w") as handle:
                handle.write("#!/usr/bin/env bash\n# not the proof runner any more\nexit 0\n")
        print("swapped " + " ".join(found))
    bad_probe = []
    if kind == "probe":
        # Looks at the copy from inside it, in both phases, and fails as a
        # guard's test must while the file its row switches is switched off.
        bad_probe = probe(control_off())
        if bad_probe:
            print("the copy is not what the step says: " + "; ".join(bad_probe))
    if kind in ("hang", "stubborn"):
        with open(os.environ["CARGO_PID"], "w") as handle:
            handle.write(str(os.getpid()))
        sys.stdout.flush()
        if kind == "stubborn":
            # Ignores the polite stop, as a build stuck in a driver does: only
            # the kill, and only aimed at the whole group, ends it.
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
        time.sleep(600)
    if kind == "build" and control_off():
        print("error: could not compile `fixture` (lib) due to 1 previous error")
        sys.exit(101)
    if kind == "flood" and not control_off():
        # More than a row may write, in the shape scripts/controls-check.sh
        # echoes back on its own stderr when its test fails with the control
        # on: some seven megabytes through the driver's pipe.
        line = "test fixture::flood_guard ... FAILED\n"
        for _ in range(200000):
            sys.stdout.write(line)
        sys.exit(101)
    fails = (
        kind in ("always-fail", "lose", "swap", "flood")
        or (kind == "fails-off" and control_off())
        or (kind == "probe" and (bool(bad_probe) or control_off()))
    )
    if fails:
        print(f"test fixture::{name} ... FAILED")
        print("test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured")
        code = 101
    else:
        print(f"test fixture::{name} ... ok")
        print("test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured")
sys.exit(code)
PY
chmod +x "${WORK}/bin/cargo"

# The candidate's commit and tree, asked of git rather than remembered.
head_commit() { git -C "${REPO}" rev-parse HEAD; }
head_tree() { git -C "${REPO}" rev-parse 'HEAD^{tree}'; }

# commit_candidate REPO MESSAGE: commit what is staged on top of what is there,
# and bind the new commit as the ref a coding step's attempt is kept under, so
# that the case asks about a commit atomic could have recorded. The ref is
# updated by hand here because nothing else in this repository runs git.
commit_candidate() {
    local repo="$1" message="$2"
    shift 2
    git -C "${repo}" -c user.name=test -c user.email=test@example.invalid \
        -c commit.gpgSign=false commit -qm "${message}" "$@"
    git -C "${repo}" update-ref "${REF}" HEAD
}

# bind REPO: point the attempt's ref at the repository's current commit.
bind() { git -C "$1" update-ref "${REF}" HEAD; }

# run_driver JSON [SOURCE [TMP [PATHS]]]: run the driver as a command run.
# TMP defaults to a directory of its own; PATHS is a PATH to use instead.
run_driver() {
    local json="$1" source="${2:-${REPO}}" tmp="${3:-${WORK}/tmp}" paths="${4:-${WORK}/bin:${PATH}}"
    # Most historical cases supply only the immediate parent. Their fixture
    # admission is that same parent; the dedicated multi-step case supplies an
    # older admitted SHA explicitly. The production driver requires the field.
    if [ "${NO_ADMITTED_INJECTION:-0}" != 1 ]; then
        json="$(printf '%s' "${json}" | python3 -c '
import json, sys
raw = sys.stdin.read()
try:
    value = json.loads(raw)
    bound = value.get("bound") if isinstance(value, dict) else None
    if isinstance(bound, dict) and isinstance(bound.get("base"), str):
        bound.setdefault("admitted", bound["base"])
    print(json.dumps(value))
except (ValueError, TypeError):
    print(raw)
')"
    fi
    mkdir -p "${WORK}/tmp"
    rm -f "${LOG}" "${WORK}/cargo-pid" "${WORK}/probe.txt" "${WORK}/probe.txt.wrong"
    out="$(printf '%s' "${json}" |
        env TMPDIR="${tmp}" CARGO_LOG="${LOG}" CARGO_PID="${WORK}/cargo-pid" \
            PROBE_OUT="${WORK}/probe.txt" \
            CONTROL_FILES="${CONTROL_FILES}" PATH="${paths}" \
            "${PINS[@]}" \
            timeout "${DRIVER_TIMEOUT:-300}" python3 "${DRIVER}" 2>"${WORK}/stderr.log")"
    code=$?
    err="$(cat "${WORK}/stderr.log")"
}

# input SOURCE ROWS...: the JSON an exec atom's command would be handed: this
# commit, bound to the ref that names it, with the commit under it as its base.
input() {
    local source="$1" rows="" name
    shift
    for name in "$@"; do
        [ -z "${rows}" ] || rows+=", "
        rows+="\"${name}\""
    done
    printf \
        '{"source": "%s", "commit": "%s", "bound": {"reference": "%s", "base": "%s", "admitted": "%s"}, "rows": [%s]}' \
        "${source}" "$(git -C "${source}" rev-parse HEAD)" "${REF}" \
        "$(git -C "${source}" rev-parse 'HEAD^')" \
        "$(git -C "${source}" rev-parse 'HEAD^')" "${rows}"
}

# refused LABEL JSON: the input is refused before any control is touched, with
# exit 2 and the reason on stdout, where whoever reads the step sees it. Nothing
# reaches cargo and nothing is left in the scratch.
refused() {
    run_driver "$2"
    [ "${code}" -eq 2 ] || fail "$1: exit ${code}, not 2: ${out}"
    [ ! -e "${LOG}" ] || fail "$1: cargo ran"
    no_scratch "$1"
}

# verdict NAME: that row's verdict in the last run's output.
verdict() {
    printf '%s' "${out}" | python3 -c '
import json, sys
data = json.load(sys.stdin)
for row in data["rows"]:
    if row["name"] == sys.argv[1]:
        print(row["verdict"])
' "$1"
}

# row NAME FILE OLD NEW TESTS...: one row file, as scripts/controls holds it.
make_row() {
    local name="$1" file="$2" old="$3" new="$4"
    shift 4
    printf '%s|%s|%s|%s|--lib -- %s\n' "${name}" "src/${file}.rs" "${old}" "${new}" "$*" \
        >"${REPO}/scripts/controls/${name}.row"
}

# A fresh candidate: the check script as the candidate holds it (which is never
# what the step runs), rows that switch synthetic files, and what the tests
# those rows name answer. Two commits, as a coding step's candidate has one
# parent: the base it started from.
fresh() {
    chmod -R u+w "${REPO}" 2>/dev/null
    rm -rf "${REPO}"
    mkdir -p "${REPO}/scripts/controls" "${REPO}/src" "${REPO}/fixture"
    cp "${ROOT}/scripts/controls-check.sh" "${REPO}/scripts/"
    # A file that .gitignore names but the commit holds, as atomic's own tree
    # holds none and any tree could: the copy must hold it too, and an ordinary
    # `git add` in the copy would leave it out.
    printf '/target/\n/src/tracked-ignored.rs\n' >"${REPO}/.gitignore"
    printf 'pub fn tracked_although_ignored() {}\n' >"${REPO}/src/tracked-ignored.rs"
    printf 'what the step started from\n' >"${REPO}/README"
    cat >"${REPO}/Cargo.toml" <<'TOML'
[package]
name = "controls-fixture"
version = "0.1.0"
edition = "2021"
TOML
    cat >"${REPO}/src/guard.rs" <<'RS'
pub fn guard() -> bool {
    let guard_on = true;
    guard_on
}
RS
    for name in other slow stub lose swap probe; do
        cat >"${REPO}/src/${name}.rs" <<RS
pub fn ${name}() -> bool {
    let ${name}_on = true;
    ${name}_on
}
RS
    done
    printf 'pub mod guard;\npub mod other;\npub mod slow;\npub mod stub;\npub mod lose;\npub mod swap;\npub mod probe;\n' \
        >"${REPO}/src/lib.rs"
    # One row per shape of answer. `other`, `hang` and `swap` switch files of
    # their own, so they can run beside another row; the rest switch src/guard.rs.
    make_row good guard 'let guard_on = true;' 'let guard_on = false;' good_guard
    make_row blind guard 'let guard_on = true;' 'let guard_on = false;' blind_guard
    make_row stale guard 'a switch that is not in the file' 'nothing at all' stale_guard
    make_row missing guard 'let guard_on = true;' 'let guard_on = false;' \
        missing_guard missing_second
    make_row build guard 'let guard_on = true;' 'let guard_on = false;' build_guard
    make_row on-fail guard 'let guard_on = true;' 'let guard_on = false;' on_fail_guard
    make_row hang slow 'let slow_on = true;' 'let slow_on = false;' hang_guard
    make_row stubborn stub 'let stub_on = true;' 'let stub_on = false;' stubborn_guard
    make_row lose lose 'let lose_on = true;' 'let lose_on = false;' lose_guard
    make_row other other 'let other_on = true;' 'let other_on = false;' other_guard
    make_row flood other 'let other_on = true;' 'let other_on = false;' flood_guard
    make_row swap swap 'let swap_on = true;' 'let swap_on = false;' swap_guard
    make_row probe probe 'let probe_on = true;' 'let probe_on = false;' probe_guard
    # A row whose control is the proof script itself, written out rather than by
    # make_row because make_row names files under src/.
    printf '%s|%s|%s|%s|--lib -- runner_guard\n' \
        runner 'scripts/controls-check.sh' '#!/usr/bin/env bash' '#!/bin/sh' \
        >"${REPO}/scripts/controls/runner.row"
    cat >"${REPO}/fixture/cargo.json" <<'JSON'
{
  "good_guard": "fails-off",
  "blind_guard": "blind",
  "stale_guard": "fails-off",
  "missing_guard": "fails-off",
  "missing_second": "missing",
  "build_guard": "build",
  "on_fail_guard": "always-fail",
  "hang_guard": "hang",
  "stubborn_guard": "stubborn",
  "lose_guard": "lose",
  "other_guard": "fails-off",
  "flood_guard": "flood",
  "swap_guard": "swap",
  "probe_guard": "probe",
  "runner_guard": "fails-off"
}
JSON
    git -C "${REPO}" init -q --initial-branch=main
    git -C "${REPO}" add README Cargo.toml src/lib.rs
    commit_candidate "${REPO}" base
    # -f because the tree holds a file .gitignore names, which is the point.
    git -C "${REPO}" add -A -f
    commit_candidate "${REPO}" candidate
    rm -f "${LOG}"
}

# no_scratch LABEL: nothing was left under TMPDIR, which is where the copy and
# every control switched off in it live.
no_scratch() {
    [ -z "$(ls -A "${WORK}/tmp" 2>/dev/null)" ] ||
        fail "$1: something is left under TMPDIR: $(ls -A "${WORK}/tmp")"
}

# nothing_left LABEL WORD: no live process of a stopped row is left. WORD is the
# test name that row's cargo was asked to run, so only that row's processes count
# here, and only those started from the stand-in: this check names both in its own
# command line, so it must not answer itself. A process that has already died and
# only waits to be reaped -- state Z -- is not a live one: it can switch no file
# and hold no CPU, and in a container whose init does not reap, it stays in /proc
# for the rest of the run. The pid the row wrote counts only while it still looks
# like the cargo it was, because a pid that went can be given to someone else.
nothing_left() {
    local label="$1" word="$2" left=""
    left="$(python3 - "${WORK}/bin/cargo" "${WORK}/cargo-pid" "${word}" <<'PY'
import os
import sys
import time

stand_in, pid = sys.argv[1], ""
try:
    pid = open(sys.argv[2]).read().strip()
except OSError:
    pass


def living():
    """Every live process of that row, described."""
    found = {}
    for entry in os.listdir("/proc"):
        if not entry.isdigit() or entry == str(os.getpid()):
            continue
        try:
            stat = open(f"/proc/{entry}/stat").read()
            cmd = open(f"/proc/{entry}/cmdline", "rb").read()
        except OSError:
            continue
        text = cmd.decode("utf-8", "replace").replace("\x00", " ")
        rest = stat.rsplit(") ", 1)[-1].split()
        # rest is: state, ppid, pgrp, session, ...
        if rest[0] == "Z":
            continue
        ours = stand_in in text and sys.argv[3] in text
        if ours or (entry == pid and stand_in in text):
            found[entry] = f"{entry} {rest[0]} ppid={rest[1]} pgrp={rest[2]} {text[:70]}"
    return found


left = living()
waited = 0.0
while left and waited < 15:
    time.sleep(0.1)
    waited += 0.1
    left = living()
print(" | ".join(left.values()))
PY
)"
    [ -z "${left}" ] || fail "${label}: something of the stopped row is left: ${left}"
}

# restored: the trusted runner as this repository holds it, so that a case which
# had it replaced does not carry that into the next one.
restore_runner() {
    cp "${ROOT}/scripts/controls-check.sh" "${RUNNER}"
    [ "$(sha_of "${RUNNER}")" = "${RUNNER_SHA}" ] ||
        fail "$1: the proof runner is not back as it was pinned"
}

fresh

# These focused cases are also the named tests in the driver's own control
# rows. Each makes the guarded condition false while leaving a valid candidate
# and proof row, so removing the guard makes the case fail.
if [ "${1:-}" = "guard-tree-equality" ]; then
    mkdir -p "${WORK}/status-shim"
    export REAL_GIT
    REAL_GIT="$(command -v git)"
    cat >"${WORK}/status-shim/git" <<'PY'
#!/usr/bin/env python3
import os
import sys

if sys.argv[-2:] == ["status", "--porcelain"] and os.getcwd().endswith("/copy"):
    print(" M src/guard.rs")
    sys.exit(0)
os.execv(os.environ["REAL_GIT"], [os.environ["REAL_GIT"], *sys.argv[1:]])
PY
    chmod 755 "${WORK}/status-shim/git"
    run_driver "$(input "${REPO}" good)" "${REPO}" "${WORK}/tmp" \
        "${WORK}/status-shim:${WORK}/bin:${PATH}"
    [ "${code}" -eq 2 ] || fail "copy tree mismatch: exit ${code}, not 2: ${out} ${err}"
    grep -q "copy holds a file git does not hold as committed" <<<"${out}" ||
        fail "copy tree mismatch: missing typed refusal: ${out}"
    [ ! -e "${LOG}" ] || fail "copy tree mismatch: a row ran before refusal"
    no_scratch "copy tree mismatch"
    [ "${failures}" -eq 0 ] || exit 1
    echo "controls-exec tree-equality guard test passed" >&2
    exit 0
fi

if [ "${1:-}" = "guard-runner-pin" ]; then
    cp "${RUNNER}" "${WORK}/untrusted-runner.sh"
    printf '\n# candidate-controlled bytes beyond the declared pin\n' >>"${WORK}/untrusted-runner.sh"
    chmod 755 "${WORK}/untrusted-runner.sh"
    pins "${WORK}/untrusted-runner.sh" "${RUNNER_SHA}"
    run_driver "$(input "${REPO}" good)"
    [ "${code}" -eq 2 ] || fail "runner pin mismatch: exit ${code}, not 2: ${out} ${err}"
    grep -q "whose bytes hash to .* not the .* pins" <<<"${out}" ||
        fail "runner pin mismatch: missing typed refusal: ${out}"
    [ ! -e "${LOG}" ] || fail "runner pin mismatch: a row ran before refusal"
    no_scratch "runner pin mismatch"
    [ "${failures}" -eq 0 ] || exit 1
    echo "controls-exec runner-pin guard test passed" >&2
    exit 0
fi

if [ "${1:-}" = "guard-selected-harness" ]; then
    NO_ADMITTED_INJECTION=1 run_driver "$(printf \
        '{"source":"%s","commit":"%s","bound":{"reference":"%s","base":"%s"},"rows":["good"]}' \
        "${REPO}" "$(head_commit)" "${REF}" "$(git -C "${REPO}" rev-parse 'HEAD^')")"
    [ "${code}" -eq 2 ] || fail "missing admission pin: ${out} ${err}"
    grep -q "bound holds no admitted" <<<"${out}" ||
        fail "missing admission pin: no typed refusal: ${out}"
    [ ! -e "${LOG}" ] || fail "missing admission pin reached cargo"

    # A standard selected integration test proves the row through the real
    # runner. The stand-in cargo prints libtest lines, as a malicious custom
    # harness could, so the driver must inspect the admitted Cargo manifests.
    mkdir -p "${REPO}/tests"
    printf '#[test]\nfn good_guard() {}\n' >"${REPO}/tests/guard.rs"
    printf '%s|%s|%s|%s|--test guard -- good_guard\n' \
        good src/guard.rs 'let guard_on = true;' 'let guard_on = false;' \
        >"${REPO}/scripts/controls/good.row"
    git -C "${REPO}" add tests/guard.rs scripts/controls/good.row
    commit_candidate "${REPO}" "standard selected test"
    git -C "${REPO}" -c user.name=test -c user.email=test@example.invalid \
        -c commit.gpgSign=false commit --allow-empty -qm "candidate with same standard target"
    bind "${REPO}"
    run_driver "$(input "${REPO}" good)"
    [ "${code}" -eq 0 ] || fail "standard libtest target: ${out} ${err}"
    [ "$(verdict good)" = "passed" ] || fail "standard libtest target verdict: ${out}"

    cat >>"${REPO}/Cargo.toml" <<'TOML'
[[test]]
name = "guard"
path = "tests/guard.rs"
harness = false
TOML
    git -C "${REPO}" add Cargo.toml
    commit_candidate "${REPO}" "selected custom harness spoof"
    refused "selected harness false" "$(input "${REPO}" good)"
    grep -q "nonstandard or disabled libtest harness" <<<"${out}" ||
        fail "selected harness false: no typed refusal: ${out}"

    fresh
    cat >>"${REPO}/Cargo.toml" <<'TOML'
[[test]]
name = "unrelated"
path = "tests/unrelated.rs"
harness = false
TOML
    mkdir -p "${REPO}/tests"
    printf 'fn main() {}\n' >"${REPO}/tests/unrelated.rs"
    git -C "${REPO}" add Cargo.toml tests/unrelated.rs
    commit_candidate "${REPO}" "unrelated custom harness"
    run_driver "$(input "${REPO}" good)"
    [ "${code}" -eq 0 ] || fail "unrelated custom target blocked selected lib: ${out} ${err}"

    fresh
    cat >>"${REPO}/Cargo.toml" <<'TOML'
[lib]
path = "src/other.rs"
TOML
    git -C "${REPO}" add Cargo.toml
    commit_candidate "${REPO}" "selected target redirected"
    refused "selected lib target redirection" "$(input "${REPO}" good)"
    grep -q "changes selected Cargo target" <<<"${out}" ||
        fail "selected target redirection: no typed refusal: ${out}"

    fresh
    admitted="$(git -C "${REPO}" rev-parse 'HEAD^')"
    cat >>"${REPO}/Cargo.toml" <<'TOML'
[lib]
path = "src/other.rs"
TOML
    git -C "${REPO}" add Cargo.toml
    commit_candidate "${REPO}" "prior step redirects target"
    git -C "${REPO}" -c user.name=test -c user.email=test@example.invalid \
        -c commit.gpgSign=false commit --allow-empty -qm "later candidate keeps redirected target"
    bind "${REPO}"
    run_driver "$(printf \
        '{"source":"%s","commit":"%s","bound":{"reference":"%s","base":"%s","admitted":"%s"},"rows":["good"]}' \
        "${REPO}" "$(head_commit)" "${REF}" "$(git -C "${REPO}" rev-parse 'HEAD^')" "${admitted}")"
    [ "${code}" -eq 2 ] || fail "multi-step admitted target redirection: ${out} ${err}"
    grep -q "changes selected Cargo target" <<<"${out}" ||
        fail "multi-step admitted target redirection: no typed refusal: ${out}"
    [ ! -e "${LOG}" ] || fail "multi-step admitted target redirection reached cargo"

    fresh
    printf '%s|%s|%s|%s|--workspace --lib -- good_guard\n' \
        good src/guard.rs 'let guard_on = true;' 'let guard_on = false;' \
        >"${REPO}/scripts/controls/good.row"
    mkdir -p "${REPO}/extra/src"
    printf '[package]\nname="extra"\nversion="0.1.0"\nedition="2021"\n' \
        >"${REPO}/extra/Cargo.toml"
    printf 'pub fn extra() {}\n' >"${REPO}/extra/src/lib.rs"
    cat >>"${REPO}/Cargo.toml" <<'TOML'
[dependencies]
extra = { path = "extra" }
TOML
    git -C "${REPO}" add Cargo.toml extra scripts/controls/good.row
    commit_candidate "${REPO}" "workspace gains in-tree path dependency"
    refused "candidate-only implicit workspace member" "$(input "${REPO}" good)"
    grep -q "changes the workspace package set" <<<"${out}" ||
        fail "implicit workspace member: no typed refusal: ${out}"

    fresh
    printf '%s|%s|%s|%s|--workspace --lib -- good_guard\n' \
        good src/guard.rs 'let guard_on = true;' 'let guard_on = false;' \
        >"${REPO}/scripts/controls/good.row"
    mkdir -p "${REPO}/extra/src"
    printf '[package]\nname="extra"\nversion="0.1.0"\nedition="2021"\n' \
        >"${REPO}/extra/Cargo.toml"
    printf 'pub fn extra() {}\n' >"${REPO}/extra/src/lib.rs"
    cat >>"${REPO}/Cargo.toml" <<'TOML'
[build_dependencies]
extra = { path = "extra" }
TOML
    git -C "${REPO}" add Cargo.toml extra scripts/controls/good.row
    commit_candidate "${REPO}" "legacy build dependency adds workspace member"
    refused "legacy build dependency workspace member" "$(input "${REPO}" good)"
    grep -q "changes the workspace package set" <<<"${out}" ||
        fail "legacy build dependency member: no typed refusal: ${out}"

    fresh
    printf 'pub fn other() {}\n' >"${REPO}/src/other.rs"
    rm "${REPO}/src/lib.rs"
    ln -s other.rs "${REPO}/src/lib.rs"
    git -C "${REPO}" add -A
    commit_candidate "${REPO}" "selected lib source becomes symlink"
    refused "selected symlinked source" "$(input "${REPO}" good)"
    grep -q "symlinked Cargo target source" <<<"${out}" ||
        fail "selected symlinked source: no typed refusal: ${out}"

    fresh
    mkdir -p "${REPO}/extra/src"
    printf '[package]\nname="extra"\nversion="0.1.0"\nedition="2021"\n' \
        >"${REPO}/extra/Cargo.toml"
    printf 'pub fn extra() {}\n' >"${REPO}/extra/src/lib.rs"
    ln -s extra "${REPO}/alias"
    cat >>"${REPO}/Cargo.toml" <<'TOML'
[dependencies]
extra = { path = "alias" }
TOML
    git -C "${REPO}" add -A
    commit_candidate "${REPO}" "path dependency uses symlink alias"
    refused "symlinked path dependency alias" "$(input "${REPO}" good)"
    grep -q "in-tree path dependency" <<<"${out}" ||
        fail "symlinked path dependency alias: no typed refusal: ${out}"

    fresh
    printf '%s|%s|%s|%s|--tests -- good_guard\n' \
        good src/guard.rs 'let guard_on = true;' 'let guard_on = false;' \
        >"${REPO}/scripts/controls/good.row"
    git -C "${REPO}" add scripts/controls/good.row
    commit_candidate "${REPO}" "unsupported broad selector"
    refused "unsupported broad tests selector" "$(input "${REPO}" good)"
    grep -q "broad Cargo target selector" <<<"${out}" ||
        fail "unsupported broad selector: no typed refusal: ${out}"

    fresh
    printf '%s|%s|%s|%s|--workspace -p controls-fixture --lib -- good_guard\n' \
        good src/guard.rs 'let guard_on = true;' 'let guard_on = false;' \
        >"${REPO}/scripts/controls/good.row"
    git -C "${REPO}" add scripts/controls/good.row
    commit_candidate "${REPO}" "mixed workspace package selector"
    refused "mixed workspace package selector" "$(input "${REPO}" good)"
    grep -q "combines --workspace with -p" <<<"${out}" ||
        fail "mixed workspace package selector: no typed refusal: ${out}"
    [ "${failures}" -eq 0 ] || exit 1
    echo "controls-exec selected-harness guard test passed" >&2
    exit 0
fi

# A row whose test fails with its control off and passes with it on passes the
# step, and cargo was asked twice: once with the control off, once with it back.
run_driver "$(input "${REPO}" good)"
[ "${code}" -eq 0 ] || fail "good row: exit ${code}, not 0: ${err}"
[ "$(verdict good)" = "passed" ] || fail "good row: verdict $(verdict good): ${out}"
grep -q '"ok": true' <<<"${out}" || fail "good row: the output is not ok: ${out}"
grep -q '"passed": 1' <<<"${out}" || fail "good row: ${out}"
grep -q "\"reference\": \"${REF}\"" <<<"${out}" ||
    fail "good row: the answer does not name what it was bound to: ${out}"
[ "$(grep -c . "${LOG}")" -eq 2 ] ||
    fail "good row: cargo was asked $(grep -c . "${LOG}") times, not twice: $(cat "${LOG}")"
no_scratch "good row"

# A large batch of distinct blobs exercises the streaming copy. A duplicate
# blob at a path whose parent has not been made yet must also be written: the
# first occurrence of that blob is at README in the candidate's root.
fresh
mkdir -p "${REPO}/src/extras"
for n in $(seq 1 2200); do
    printf 'unique object %s\n' "${n}" >"${REPO}/src/extras/${n}.txt"
done
mkdir -p "${REPO}/src/extras/copies"
cp "${REPO}/README" "${REPO}/src/extras/copies/README"
git -C "${REPO}" add src/extras
commit_candidate "${REPO}" "many distinct blobs"
DRIVER_TIMEOUT=30 run_driver "$(input "${REPO}" good)"
[ "${code}" -eq 0 ] || fail "many distinct blobs: exit ${code}, not 0: ${err} ${out}"
[ "$(verdict good)" = "passed" ] ||
    fail "many distinct blobs: verdict $(verdict good): ${out}"
no_scratch "many distinct blobs"

# Refusal cleanup must not close cat-file's buffered stdin while its feeder
# holds the stream lock on a full pipe. This Git stand-in forwards all normal
# questions, then leaves cat-file's stdin unread long enough to block the
# feeder, emits a malformed header, and waits for its parent to stop it. The
# driver must answer with a typed refusal within its short deadline.
mkdir -p "${WORK}/git-shim"
cat >"${WORK}/git-shim/git" <<'PY'
#!/usr/bin/env python3
import fcntl
import os
import sys
import time

if 'cat-file' not in sys.argv or '--batch' not in sys.argv:
    os.execv(os.environ['REAL_GIT'], [os.environ['REAL_GIT'], *sys.argv[1:]])
parent = os.getppid()
try:
    fcntl.fcntl(0, fcntl.F_SETPIPE_SZ, 4096)
except OSError:
    pass
with open(os.environ['PIPE_MARKER'], 'w') as marker:
    marker.write(str(fcntl.fcntl(0, fcntl.F_GETPIPE_SZ)))
time.sleep(0.3)
sys.stdout.write('not a git object header\n')
sys.stdout.flush()
while os.getppid() == parent:
    time.sleep(0.1)
PY
chmod 755 "${WORK}/git-shim/git"
export REAL_GIT="$(command -v git)"
export PIPE_MARKER="${WORK}/git-shim/pipe-size"
DRIVER_TIMEOUT=6 run_driver "$(input "${REPO}" good)" "${REPO}" "${WORK}/tmp" \
    "${WORK}/git-shim:${WORK}/bin:${PATH}"
[ -s "${PIPE_MARKER}" ] || fail "cat-file unread stdin: shim did not record its pipe size"
if [ -s "${PIPE_MARKER}" ]; then
    pipe_size="$(cat "${PIPE_MARKER}")"
    [ "${pipe_size}" -lt 88000 ] ||
        fail "cat-file unread stdin: pipe ${pipe_size} bytes can hold all object ids"
fi
[ "${code}" -eq 2 ] || fail "cat-file unread stdin: exit ${code}, not 2: ${out} ${err}"
case "${out}" in
*"not a header"*) ;;
*) fail "cat-file unread stdin: no typed malformed-header refusal: ${out}" ;;
esac
no_scratch "cat-file unread stdin"

fresh

# The proof runner that ran is the one the step pinned, in the copy's own place.
# The case above proves that by outcome rather than by path: the candidate holds
# the check script at its own scripts/controls-check.sh, and that copy really
# switches, so a stub there cannot be what did this.
run_driver "$(input "${REPO}" blind)"
[ "${code}" -eq 1 ] || fail "blind row: exit ${code}, not 1: ${err}"
[ "$(verdict blind)" = "blind" ] || fail "blind row: verdict $(verdict blind): ${out}"
grep -q "blind: its test passed with the control off" <<<"${err}" ||
    fail "blind row: stderr does not say the test passed with it off: ${err}"
grep -q "1 blind" <<<"${err}" || fail "blind row: the summary does not count it: ${err}"
no_scratch "blind row"

# A row whose switch is no longer in the file fails as stale, and cargo is
# never asked: a control that has moved is not a control that was proved.
run_driver "$(input "${REPO}" stale)"
[ "${code}" -eq 1 ] || fail "stale row: exit ${code}: ${err}"
[ "$(verdict stale)" = "switch-stale" ] || fail "stale row: verdict $(verdict stale): ${out}"
[ ! -e "${LOG}" ] || fail "stale row: cargo ran: $(cat "${LOG}")"

# A row one of whose tests matched nothing fails as not seen failing, though
# nothing passed either: the driver reads the script's reason, not its exit
# code alone.
run_driver "$(input "${REPO}" missing)"
[ "${code}" -eq 1 ] || fail "missing row: exit ${code}: ${err}"
[ "$(verdict missing)" = "not-seen-failing" ] ||
    fail "missing row: verdict $(verdict missing): ${out}"
grep -q "not seen failing" <<<"${err}" || fail "missing row: ${err}"

# A row whose removal breaks the build fails as such, not as a blind one.
run_driver "$(input "${REPO}" build)"
[ "${code}" -eq 1 ] || fail "build row: exit ${code}: ${err}"
[ "$(verdict build)" = "build-failed" ] || fail "build row: verdict $(verdict build): ${out}"

# A row whose test fails with the control on fails as such: a red suite is not
# a proved control.
run_driver "$(input "${REPO}" on-fail)"
[ "${code}" -eq 1 ] || fail "on-fail row: exit ${code}: ${err}"
[ "$(verdict on-fail)" = "fails-when-on" ] ||
    fail "on-fail row: verdict $(verdict on-fail): ${out}"

# A row that never answers is stopped inside its own cap and reported, and the
# row after it still runs. Without the cap the whole command would be stopped
# at its five minutes and the step would say nothing about which row hung.
start="$(date +%s)"
# row_seconds is at its floor here so the case does not wait four minutes;
# what it measures is that the row is stopped early and reported, not the
# default itself.
run_driver "$(printf \
    '{"source": "%s", "commit": "%s", "bound": {"reference": "%s", "base": "%s"},
      "rows": ["hang", "good"], "row_seconds": 5, "setup_seconds": 10, "step_seconds": 300}' \
    "${REPO}" "$(head_commit)" "${REF}" "$(git -C "${REPO}" rev-parse 'HEAD^')")"
ran=$(( $(date +%s) - start ))
[ "${code}" -eq 1 ] || fail "hang row: exit ${code}: ${err}"
[ "$(verdict hang)" = "timed-out" ] || fail "hang row: verdict $(verdict hang): ${out}"
[ "$(verdict good)" = "passed" ] || fail "hang row: the next row did not run: ${out}"
grep -q "ran past its cap" <<<"${err}" || fail "hang row: ${err}"
[ "${ran}" -lt 60 ] || fail "hang row: the run took ${ran}s, so the row was not stopped early"
no_scratch "hang row"
# Nothing of the stopped row is left: the whole group is stopped, not just the
# row's own process, because scripts/controls-check.sh runs cargo in the background inside the script.
nothing_left "hang row" hang_guard
# Anything that case did leave is that case's to stop, so that a mutation run
# does not carry a sleeping cargo into the next case.
python3 - "${WORK}/cargo-pid" <<'PY'
import os
import sys
try:
    pid = int(open(sys.argv[1]).read().strip())
except (OSError, ValueError):
    raise SystemExit(0)
try:
    os.kill(pid, 15)
except ProcessLookupError:
    pass
PY

# A row whose cargo will not take the polite stop is still ended: the kill
# goes to the whole group, because scripts/controls-check.sh runs cargo in the
# background of its own, and a build left running after a stopped row would
# still be switching the copy's files while the next row runs.
start="$(date +%s)"
run_driver "$(printf \
    '{"source": "%s", "commit": "%s", "bound": {"reference": "%s", "base": "%s"},
      "rows": ["stubborn", "good"], "row_seconds": 5, "setup_seconds": 10, "step_seconds": 300}' \
    "${REPO}" "$(head_commit)" "${REF}" "$(git -C "${REPO}" rev-parse 'HEAD^')")"
ran=$(( $(date +%s) - start ))
[ "${code}" -eq 1 ] || fail "a stubborn row: exit ${code}: ${err}"
[ "$(verdict stubborn)" = "timed-out" ] ||
    fail "a stubborn row: verdict $(verdict stubborn): ${out}"
[ "$(verdict good)" = "passed" ] || fail "a stubborn row: the next row did not run: ${out}"
[ "${ran}" -lt 60 ] || fail "a stubborn row: the run took ${ran}s"
no_scratch "a stubborn row"
nothing_left "a stubborn row" stubborn_guard
python3 - "${WORK}/cargo-pid" <<'PY'
import os
import sys
try:
    pid = int(open(sys.argv[1]).read().strip())
except (OSError, ValueError):
    raise SystemExit(0)
try:
    os.kill(pid, 9)  # it ignores the polite one, by design
except ProcessLookupError:
    pass
PY

# A row whose run lost the originals it saved cannot put its control back, and
# ends without its result line. That is not a row that passed, and the copy is
# left switched off, so the driver says which: a control left off is a worse
# answer than a control that was never proved.
run_driver "$(input "${REPO}" lose)"
[ "${code}" -eq 1 ] || fail "a row that lost its originals: exit ${code}: ${err}"
[ "$(verdict lose)" = "left-switched" ] ||
    fail "a row that lost its originals: verdict $(verdict lose): ${out}"
grep -q "left its saved originals" <<<"${err}" || fail "a row that lost its originals: ${err}"
no_scratch "a row that lost its originals"

# What a row writes is capped as it arrives, not after. The row's command here
# writes some forty megabytes in the shape the check script echoes back when a
# test fails with the control on, which is far past what a row may write. The
# cap is applied while the bytes come, so the row is stopped and reported
# promptly rather than read to the end first: a limit that is only measured
# after the pipe has been emptied is a limit on nothing.
start="$(date +%s)"
run_driver "$(printf \
    '{"source": "%s", "commit": "%s", "bound": {"reference": "%s", "base": "%s"},
      "rows": ["flood"], "row_seconds": 120, "setup_seconds": 30, "step_seconds": 290}' \
    "${REPO}" "$(head_commit)" "${REF}" "$(git -C "${REPO}" rev-parse 'HEAD^')")"
ran=$(( $(date +%s) - start ))
[ "${code}" -eq 1 ] || fail "a flooding row: exit ${code}: ${err}"
[ "$(verdict flood)" = "errored" ] || fail "a flooding row: verdict $(verdict flood): ${out}"
grep -q "wrote more than its output limit" <<<"${err}" || fail "a flooding row: ${err}"
[ "${ran}" -lt 60 ] || fail "a flooding row: the run took ${ran}s, so its output was read to the end"
no_scratch "a flooding row"

# The cap bounds what is kept as well as what is read: the bytes held back for
# the answer are cut at the limit rather than merely counted, so a row that
# writes in chunks the limit does not divide leaves nothing past it. This asks
# the question of `drain`, the function `run` empties a row's pipes with, on a
# stream that answers as a pipe does -- in chunks, and then nothing.
capped="$(python3 - "${DRIVER}" <<'PY'
import importlib.util
import sys

spec = importlib.util.spec_from_file_location("controls_exec", sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class Stream:
    """A pipe that answers in chunks the limit does not divide, then ends."""

    def __init__(self):
        self.left = 3 * 3000

    def read(self, size):
        take = min(size, self.left, 3000)
        self.left -= take
        return b"x" * take


stopped = []
kept, overflow, total = module.drain(Stream(), 4096, lambda: stopped.append(True))
print(f"{len(kept)} {overflow} {total} {len(stopped)} {kept == b'x' * 4096}")
PY
)"
[ "${capped}" = "4096 True 9000 1 True" ] ||
    fail "a capped row's kept bytes: ${capped}, not 4096 of the 9000 read, stopped once, and nothing past the limit"

# A row that replaces the proof runner it is being run through -- which is the
# trusted file itself, reached through the link the step made -- gets no verdict
# at all, and nothing after it is believed. Without the check made when a row
# ends this would read as a row that passed, because the bytes that answered are
# the candidate's by then.
run_driver "$(printf \
    '{"source": "%s", "commit": "%s", "bound": {"reference": "%s", "base": "%s"},
      "rows": ["swap", "other"], "row_seconds": 60, "setup_seconds": 30, "step_seconds": 300}' \
    "${REPO}" "$(head_commit)" "${REF}" "$(git -C "${REPO}" rev-parse 'HEAD^')")"
[ "${code}" -eq 1 ] || fail "a row that replaced the runner: exit ${code}: ${err}"
[ "$(verdict swap)" = "runner-changed" ] ||
    fail "a row that replaced the runner: verdict $(verdict swap): ${out}"
[ "$(verdict other)" = "runner-changed" ] ||
    fail "a row that replaced the runner: the row after it was run anyway: ${out}"
grep -q "not the .* this step started with" <<<"${err}" ||
    fail "a row that replaced the runner: stderr does not say the runner changed: ${err}"
grep -q "other_guard" "${LOG}" &&
    fail "a row that replaced the runner: the next row's cargo ran: $(cat "${LOG}")"
no_scratch "a row that replaced the runner"
restore_runner "a row that replaced the runner"

# ------------------------------------------------------------------ trust
#
# What is proved, and with what, is decided before anything is switched off:
# the commit is the one atomic recorded, and the runner is the file the step was
# declared with. These are the two ways a change could otherwise forge a
# passing verdict for itself.

# The candidate's own copy of the check script is never what runs. Here the
# candidate holds a stub there that prints the passing line for every row and
# exits 0, which is exactly what a change that wanted a free pass would hold: a
# switch that is never thrown. The step runs the pinned runner instead, so the
# blind row is still blind and the step still fails.
stub_repo="${WORK}/stub"
rm -rf "${stub_repo}"
cp -a "${REPO}" "${stub_repo}"
printf '#!/usr/bin/env bash\n# every control passes, as its own candidate says\nif [ "$#" -ge 1 ]; then\n  echo "ok ${1}: its test fails with the control off and passes with it on"\nfi\nexit 0\n' \
    >"${stub_repo}/scripts/controls-check.sh"
chmod 755 "${stub_repo}/scripts/controls-check.sh"
bind "${stub_repo}"
run_driver "$(input "${stub_repo}" blind)"
[ "${code}" -eq 1 ] || fail "a candidate that fakes its check script: exit ${code}, not 1: ${err}
${err}"
[ "$(verdict blind)" = "blind" ] ||
    fail "a candidate that fakes its check script: verdict $(verdict blind), not blind: ${out}"
grep -q "1 blind" <<<"${err}" || fail "a candidate that fakes its check script: ${err}"
# And the stub really is what the candidate holds, so this case tested the
# forgery rather than a tree that happened to behave.
grep -q "every control passes" "${stub_repo}/scripts/controls-check.sh" ||
    fail "a candidate that fakes its check script: the case's own stub is not there"
no_scratch "a candidate that fakes its check script"

# The same forgery beside a control that really bites: the honest row still
# passes, because the runner that ran was the trusted one, and the forged one
# still says nothing.
run_driver "$(printf \
    '{"source": "%s", "commit": "%s", "bound": {"reference": "%s", "base": "%s"},
      "rows": ["good"], "row_seconds": 120}' \
    "${stub_repo}" "$(git -C "${stub_repo}" rev-parse HEAD)" "${REF}" \
    "$(git -C "${stub_repo}" rev-parse 'HEAD^')")"
[ "${code}" -eq 0 ] || fail "a forged script beside a real control: exit ${code}: ${err}
${err}"
[ "$(verdict good)" = "passed" ] || fail "a forged script beside a real control: ${out}"

# A row whose control is the proof runner itself cannot be proved by running the
# runner, so it is refused and named rather than passed.
run_driver "$(printf \
    '{"source": "%s", "commit": "%s", "bound": {"reference": "%s", "base": "%s"},
      "rows": ["runner"], "row_seconds": 120}' \
    "${REPO}" "$(head_commit)" "${REF}" "$(git -C "${REPO}" rev-parse 'HEAD^')")"
[ "${code}" -eq 2 ] || fail "a row that switches the proof runner: exit ${code}, not 2: ${out}"
grep -q "switches scripts/controls-check.sh itself" <<<"${out}" ||
    fail "a row that switches the proof runner: ${out}"
[ ! -e "${LOG}" ] || fail "a row that switches the proof runner: cargo ran"
no_scratch "a row that switches the proof runner"

# A pinned runner that is not the file the pin describes is refused before
# anything runs, naming both hashes so that a stale pin reads as what it is.
printf '#!/usr/bin/env bash\n# a stub pretending to be the runner\nexit 0\n' \
    >"${WORK}/pretend.sh"
chmod 755 "${WORK}/pretend.sh"
pins "${WORK}/pretend.sh"
refused "a runner whose bytes are not what is pinned" "$(input "${REPO}" good)"
grep -q "whose bytes hash to .* not the .* pins" <<<"${out}" ||
    fail "a runner whose bytes are not what is pinned: ${out}"

# A declaration that lives in a repository can only name the runner by a path of
# its own, so a relative `CONTROL_RUNNER` is taken against the directory of the
# driver that is running: the entry point the step's `exec.command` already
# names, whose bytes `CONTROL_DRIVER_SHA256` already pins. Here the driver runs
# from the harness beside the runner it names, in exactly the words an atom whose
# `exec.command` is `scripts/controls-exec.py` and whose `env` names
# `scripts/controls-check.sh` would write.
run_declared() {
    local driver="$1"
    out="$(printf '%s' "$(input "${REPO}" good)" |
        env TMPDIR="${WORK}/tmp" CARGO_LOG="${LOG}" CARGO_PID="${WORK}/cargo-pid" \
            PROBE_OUT="${WORK}/probe.txt" CONTROL_FILES="${CONTROL_FILES}" \
            PATH="${WORK}/bin:${PATH}" "${PINS[@]}" \
            python3 "${driver}" 2>"${WORK}/stderr.log")"
    code=$?
    err="$(cat "${WORK}/stderr.log")"
}
cp "${DRIVER}" "${HARNESS}/controls-exec.py"
mkdir -p "${HARNESS}/scripts"
cp "${ROOT}/scripts/controls-check.sh" "${HARNESS}/scripts/controls-check.sh"
chmod 755 "${HARNESS}/scripts/controls-check.sh"
pins "scripts/controls-check.sh" "${RUNNER_SHA}" "${DRIVER_SHA}"
run_declared "${HARNESS}/controls-exec.py"
[ "${code}" -eq 0 ] || fail "a runner named beside the driver: exit ${code}, not 0: ${out}
${err}"
[ "$(verdict good)" = "passed" ] || fail "a runner named beside the driver: ${out}"
no_scratch "a runner named beside the driver"
# The very same words, in a checkout that has moved: the relative name follows
# the driver, and the pinned bytes are the proof that it reached the same
# runner. A declaration that had written the old absolute path would now name
# whatever stands where the tree used to be.
moved="${WORK}/moved-checkout"
rm -rf "${moved}"
mkdir -p "${moved}/scripts"
cp "${DRIVER}" "${moved}/scripts/controls-exec.py"
cp "${ROOT}/scripts/controls-check.sh" "${moved}/scripts/controls-check.sh"
chmod 755 "${moved}/scripts/controls-check.sh"
pins "controls-check.sh" "${RUNNER_SHA}" "${DRIVER_SHA}"
run_declared "${moved}/scripts/controls-exec.py"
[ "${code}" -eq 0 ] || fail "the same declaration after the checkout moved: exit ${code}: ${out}
${err}"
[ "$(verdict good)" = "passed" ] || fail "the same declaration after the checkout moved: ${out}"
no_scratch "the same declaration after the checkout moved"
# A relative name that reaches nothing is refused for what it is asked, naming
# the path the driver's own directory led to, and never treated as a default.
pins "nowhere" "${RUNNER_SHA}" "${DRIVER_SHA}"
refused "a relative runner that is not there" "$(input "${REPO}" good)"
grep -q "which cannot be resolved" <<<"${out}" ||
    fail "a relative runner that is not there: ${out}"
grep -q "${ROOT}/scripts/nowhere" <<<"${out}" ||
    fail "a relative runner that is not there: it does not say where it looked: ${out}"
pins
# A runner named inside the change is refused for that, whatever its bytes: the
# change cannot supply the thing that proves it.
pins "${stub_repo}/scripts/controls-check.sh" "$(sha_of "${stub_repo}/scripts/controls-check.sh")"
refused "a runner inside the change" "$(input "${stub_repo}" good)"
grep -q "inside the change" <<<"${out}" || fail "a runner inside the change: ${out}"
# A runner this user cannot read is refused for what it is, and not treated as
# absent: under a user above modes the case would show nothing, so it says so.
pins "${WORK}/locked-runner.sh" "${RUNNER_SHA}"
cp "${RUNNER}" "${WORK}/locked-runner.sh"
chmod 000 "${WORK}/locked-runner.sh"
if [ "$(id -u)" -eq 0 ]; then
    fail "the unreadable runner case shows nothing to the superuser, so it proves nothing here"
fi
refused "a runner that cannot be read" "$(input "${REPO}" good)"
grep -q "cannot be read" <<<"${out}" || fail "a runner that cannot be read: ${out}"
chmod 600 "${WORK}/locked-runner.sh"
rm -f "${WORK}/locked-runner.sh"
# A pin that names nothing is a refusal, not a default, for either half.
pins "${RUNNER}" "" "${DRIVER_SHA}"
refused "a runner the declaration does not pin" "$(input "${REPO}" good)"
grep -q "CONTROL_RUNNER_SHA256 is ''" <<<"${out}" ||
    fail "a runner the declaration does not pin: ${out}"
pins "${RUNNER}" "${RUNNER_SHA}" ""
refused "a driver the declaration does not pin" "$(input "${REPO}" good)"
grep -q "CONTROL_DRIVER_SHA256 is ''" <<<"${out}" ||
    fail "a driver the declaration does not pin: ${out}"
pins
# A copy of this driver, run in place of the one the step was declared with,
# refuses rather than answering: planted bytes cannot report a pass. The pins it
# is given are the honest ones, which is the point -- what is running is not
# what they describe.
cp "${DRIVER}" "${WORK}/other-driver.py"
printf '# and a line, so the bytes differ\n' >>"${WORK}/other-driver.py"
out="$(printf '%s' "$(input "${REPO}" good)" |
    env TMPDIR="${WORK}/tmp" CARGO_LOG="${LOG}" CARGO_PID="${WORK}/cargo-pid" \
        CONTROL_FILES="${CONTROL_FILES}" PATH="${WORK}/bin:${PATH}" "${PINS[@]}" \
        python3 "${WORK}/other-driver.py" 2>/dev/null)"
code=$?
[ "${code}" -eq 2 ] || fail "a driver that is not the one pinned: exit ${code}, not 2: ${out}"
grep -q "not the driver this step was declared with" <<<"${out}" ||
    fail "a driver that is not the one pinned: ${out}"
[ ! -e "${LOG}" ] || fail "a driver that is not the one pinned: cargo ran"
no_scratch "a driver that is not the one pinned"

# ------------------------------------------------------------ what it is bound to
#
# The step proves the commit a coding step made, which is the commit atomic
# recorded and kept under a ref of its own. Asked about anything else it refuses,
# because a proof about another commit -- an easier one, or one from a repository
# of the change's own choosing -- is no proof about the change.

# A second repository whose row passes easily, asked about beside the bound one.
# Its commit is a fine commit; nothing of atomic's names it.
easy="${WORK}/easy"
rm -rf "${easy}"
mkdir -p "${easy}"
printf 'nothing to prove here\n' >"${easy}/README"
git -C "${easy}" init -q --initial-branch=main
git -C "${easy}" add -A
git -C "${easy}" -c user.name=test -c user.email=test@example.invalid commit -qm base
git -C "${easy}" -c user.name=test -c user.email=test@example.invalid \
    -c commit.gpgSign=false commit --allow-empty -qm "second"
run_driver "$(printf \
    '{"source": "%s", "commit": "%s", "bound": {"reference": "%s", "base": "%s"}, "rows": ["good"]}' \
    "${easy}" "$(git -C "${easy}" rev-parse HEAD)" "${REF}" \
    "$(git -C "${easy}" rev-parse 'HEAD^')")"
[ "${code}" -eq 2 ] || fail "an easy repository in the change's place: exit ${code}, not 2: ${out}"
grep -q "names no commit in ${easy}" <<<"${out}" ||
    fail "an easy repository in the change's place: ${out}"
[ ! -e "${LOG}" ] || fail "an easy repository in the change's place: cargo ran"
no_scratch "an easy repository in the change's place"

# The same repository, the same rows, another commit: the base, whose tree holds
# no rows at all, is a real commit that the attempt's ref does not name.
run_driver "$(printf \
    '{"source": "%s", "commit": "%s", "bound": {"reference": "%s", "base": "%s"}, "rows": ["good"]}' \
    "${REPO}" "$(git -C "${REPO}" rev-parse 'HEAD^')" "${REF}" "$(head_commit)")"
[ "${code}" -eq 2 ] || fail "a commit the ref does not name: exit ${code}, not 2: ${out}"
grep -q "names $(head_commit), not the $(git -C "${REPO}" rev-parse 'HEAD^')" <<<"${out}" ||
    fail "a commit the ref does not name: ${out}"
[ ! -e "${LOG}" ] || fail "a commit the ref does not name: cargo ran"

# A candidate with two parents is nothing a step's attempt made, and a ref that
# names it names an attempt no step ran.
git -C "${REPO}" checkout -qb side
git -C "${REPO}" -c user.name=test -c user.email=test@example.invalid \
    -c commit.gpgSign=false commit --allow-empty -qm "on a side line"
git -C "${REPO}" checkout -q main
git -C "${REPO}" -c user.name=test -c user.email=test@example.invalid \
    -c commit.gpgSign=false commit --allow-empty -qm "on the main line"
git -C "${REPO}" -c user.name=test -c user.email=test@example.invalid \
    -c commit.gpgSign=false merge --no-ff -q -m merged side
merged="$(git -C "${REPO}" rev-parse HEAD)"
# The commit, then one word per parent: three for a merge.
[ "$(git -C "${REPO}" rev-list --parents -n 1 "${merged}" | wc -w)" -eq 3 ] ||
    fail "the merge case made no merge commit: $(git -C "${REPO}" rev-list --parents -n 1 "${merged}")"
bind "${REPO}"
run_driver "$(printf \
    '{"source": "%s", "commit": "%s", "bound": {"reference": "%s", "base": "%s"}, "rows": ["good"]}' \
    "${REPO}" "${merged}" "${REF}" "$(git -C "${REPO}" rev-parse 'HEAD^1')")"
[ "${code}" -eq 2 ] || fail "a merge commit as the candidate: exit ${code}, not 2: ${out}"
grep -q "as its parent, not the" <<<"${out}" || fail "a merge commit as the candidate: ${out}"
[ ! -e "${LOG}" ] || fail "a merge commit as the candidate: cargo ran"
fresh

# What the copy is made of is the commit, never the working tree. Here the
# working tree holds the control already switched off, uncommitted, and a stray
# file: copying the tree would hand the check script a file that differs from
# HEAD, which it refuses outright. This passes, and names the commit's tree,
# because nothing was read out of the tree at all.
printf 'pub fn guard() -> bool {\n    let guard_on = false;\n    guard_on\n}\n' \
    >"${REPO}/src/guard.rs"
printf 'uncommitted scratch\n' >"${REPO}/stray.txt"
dirty_hash="$(sha256sum "${REPO}/src/guard.rs")"
run_driver "$(input "${REPO}" good)"
[ "${code}" -eq 0 ] || fail "a dirty tree: exit ${code}, not 0: ${err}"
grep -q "$(head_tree)" <<<"${out}" ||
    fail "a dirty tree: the output does not name the commit's tree $(head_tree): ${out}"
[ "$(sha256sum "${REPO}/src/guard.rs")" = "${dirty_hash}" ] ||
    fail "a dirty tree: the working tree it was given was changed"
git -C "${REPO}" diff --quiet -- src/guard.rs &&
    fail "a dirty tree: the file is no longer dirty, so this case tested nothing"
[ -e "${REPO}/stray.txt" ] || fail "a dirty tree: a stray file went missing"

# The same input twice gives the same answer, byte for byte, and the same
# exit: a step runs at least once, so a second attempt reports exactly what the
# first did.
first="${out}" first_code="${code}"
run_driver "$(input "${REPO}" good)"
[ "${code}" -eq "${first_code}" ] || fail "rerun: exit ${code}, not ${first_code}: ${err}"
[ "${out}" = "${first}" ] || fail "rerun: the output differs: ${first} vs ${out}"
git -C "${REPO}" status --porcelain | grep -q '^?? stray.txt' ||
    fail "after reruns: the working tree changed: $(git -C "${REPO}" status --porcelain)"

# A row the candidate does not hold is refused before anything runs, naming
# itself: a change cannot declare a control its own tree does not carry.
run_driver "$(input "${REPO}" no-such-row)"
[ "${code}" -eq 2 ] || fail "an undeclared row: exit ${code}, not 2: ${err}"
grep -q '"error"' <<<"${out}" || fail "an undeclared row: no error in the output: ${out}"
grep -q "holds no scripts/controls/no-such-row.row" <<<"${out}" ||
    fail "an undeclared row: ${out}"
[ ! -e "${LOG}" ] || fail "an undeclared row: cargo ran"
no_scratch "an undeclared row"

# Two rows naming one file in one step are refused: one run of the script
# saves the file it edits and puts it back, so the second would save what the
# first left switched off and report a control that is off as one that passes.
run_driver "$(printf \
    '{"source": "%s", "commit": "%s", "bound": {"reference": "%s", "base": "%s"},
      "rows": ["good", "blind"], "row_seconds": 120}' \
    "${REPO}" "$(head_commit)" "${REF}" "$(git -C "${REPO}" rev-parse 'HEAD^')")"
[ "${code}" -eq 2 ] || fail "two rows, one file: exit ${code}, not 2: ${err}"
grep -q "both switch src/guard.rs" <<<"${out}" || fail "two rows, one file: ${out}"
[ ! -e "${LOG}" ] || fail "two rows, one file: cargo ran"

# Two rows naming different files run in one step when the input asks for it.
# Both are declared and both are run: the input is the whole declared set, so
# nothing is left out of the answer.
run_driver "$(printf \
    '{"source": "%s", "commit": "%s", "bound": {"reference": "%s", "base": "%s"},
      "rows": ["good", "other"], "row_seconds": 120}' \
    "${REPO}" "$(head_commit)" "${REF}" "$(git -C "${REPO}" rev-parse 'HEAD^')")"
[ "${code}" -eq 0 ] || fail "two rows, two files: exit ${code}, not 0: ${err}
${err}"
[ "$(verdict good)" = "passed" ] || fail "two rows, two files: ${out}"
[ "$(verdict other)" = "passed" ] || fail "two rows, two files: ${out}"
grep -q '"passed": 2' <<<"${out}" || fail "two rows, two files: ${out}"

# What the copy is, and how it is bound, is reported by the row itself. The
# `probe` kind of the stand-in cargo looks at the tree it is running in -- from
# inside the scratch of the step's own attempt, while its control is off and
# again with it back -- and asks what that repository holds. It must have a HEAD
# and an index naming the candidate and its tree, its files exactly as that tree
# holds them, and no commit object of its own: making a commit is what this step
# no longer does anywhere near the sandbox, and a copy that carries a commit of
# its own is a copy that was committed rather than bound.
run_driver "$(input "${REPO}" probe)"
[ "${code}" -eq 0 ] || fail "the copy's own binding: exit ${code}, not 0: ${err}
${err}"
[ "$(verdict probe)" = "passed" ] || fail "the copy's own binding: verdict $(verdict probe): ${out}"
[ ! -e "${WORK}/probe.txt.wrong" ] ||
    fail "the copy is bound wrongly: $(cat "${WORK}/probe.txt.wrong")"
bound="$(python3 - "${WORK}/probe.txt" "$(head_commit)" "$(head_tree)" <<'PY'
import sys

phases = {}
for line in open(sys.argv[1]):
    fields = line.split()
    if fields:
        phases[fields[0]] = dict(field.split("=", 1) for field in fields[1:])
bad = []
for phase in ("off", "on"):
    found = phases.get(phase)
    if found is None:
        bad.append(f"the row never reported the {phase} phase")
        continue
    if found.get("head") != sys.argv[2]:
        bad.append(f"{phase}: HEAD is {found.get('head')}, not the candidate {sys.argv[2]}")
    if found.get("tree") != sys.argv[3]:
        bad.append(f"{phase}: the copy's tree is {found.get('tree')}, not {sys.argv[3]}")
    if found.get("commits") != "0":
        bad.append(
            f"{phase}: the copy holds {found.get('commits')} commit object(s) of its own, "
            "so a commit was made inside the sandbox"
        )
    if not int(found.get("index", "0")):
        bad.append(f"{phase}: the copy's index holds no file, so nothing is staged as committed")
print("; ".join(bad))
PY
)"
[ -z "${bound}" ] || fail "the copy's own binding: ${bound}"
no_scratch "the copy's own binding"

# The step's own wall time is its own, and an input whose declared rows cannot
# all fit it is refused where the reason can still be said: a row that never
# runs is a control that was never proved, and one that starts and does not
# finish leaves no answer at all.
refused "rows that cannot all fit the step" \
    "$(printf '{"source": "%s", "commit": "%s", "bound": {"reference": "%s", "base": "%s"},
        "rows": ["good", "other"], "row_seconds": 240}' \
        "${REPO}" "$(head_commit)" "${REF}" "$(git -C "${REPO}" rev-parse 'HEAD^')")"
grep -q "do not fit the step" <<<"${out}" || fail "rows that cannot all fit the step: ${out}"

# A row the step's clock reaches before its turn comes is reported as unrun and
# fails the step: the budget running out is news about the controls left over,
# not a reason to say nothing. The input cannot ask for this -- rows that cannot
# all fit the step are refused above, and a row stopped at its own cap leaves the
# next one room of its own -- so it happens only when setup spends more than its
# reserve. That is what this case does out of the driver's sight: a git that
# takes 3s to answer spends the step's time during setup's object and manifest
# questions. The one row that starts runs into the point where the step stops for its
# answer reserve, and the row behind it never starts.
slow_git="${WORK}/slow-git"
mkdir -p "${slow_git}"
cat >"${slow_git}/git" <<SH
#!/usr/bin/env bash
sleep 3
exec $(command -v git) "\$@"
SH
chmod +x "${slow_git}/git"
start="$(date +%s)"
run_driver "$(printf \
    '{"source": "%s", "commit": "%s", "bound": {"reference": "%s", "base": "%s"},
      "rows": ["hang", "good"], "row_seconds": 25, "setup_seconds": 10, "step_seconds": 90}' \
    "${REPO}" "$(head_commit)" "${REF}" "$(git -C "${REPO}" rev-parse 'HEAD^')")" \
    "${REPO}" "${WORK}/tmp" "${slow_git}:${WORK}/bin:${PATH}"
ran=$(( $(date +%s) - start ))
[ "${code}" -eq 1 ] || fail "a step out of time: exit ${code}: ${err}"
# The running row is stopped and reported as unproved. Depending on whether
# the trusted runner finishes restoring during its stop grace, it reports a
# timeout or that it left a switched file in the disposable copy.
case "$(verdict hang)" in
    timed-out | left-switched) ;;
    *) fail "a step out of time: hang read $(verdict hang): ${out}" ;;
esac
grep -q "ran past its cap" <<<"${out}" || fail "a step out of time: ${out}"
[ "$(verdict good)" = "no-time" ] || fail "a step out of time: good read $(verdict good): ${out}"
grep -q "unrun (good)" <<<"${err}" || fail "a step out of time: the summary: ${err}"
grep -q "was never switched off" <<<"${out}" || fail "a step out of time: ${out}"
[ -e "${LOG}" ] && grep -q "good_guard" "${LOG}" &&
    fail "a step out of time: the unrun row's cargo ran: $(cat "${LOG}")"
# The whole point of the step's own clock: the driver answers inside the exec
# command's five minutes, so that someone reads a verdict rather than a kill.
# The step asked for 90s and its rows had to stop 20s short of that; the answer
# and the cleanup live in the difference, and a run that takes the whole step to
# say nothing is the failure this reserve exists to prevent.
[ "${ran}" -lt 110 ] || fail "a step out of time: the run took ${ran}s, past its own 90s step"
nothing_left "a step out of time" hang_guard
no_scratch "a step out of time"

# The rows the driver is given come from the candidate even when the working
# tree the step starts in holds others: a change cannot get a control it does
# not carry by leaving one in its own directory.
fresh
printf 'uncommitted-name|src/guard.rs|nothing|nothing|--lib -- other_guard\n' \
    >"${REPO}/scripts/controls/uncommitted.row"
run_driver "$(input "${REPO}" good)"
[ "${code}" -eq 0 ] || fail "a row left in the working tree: exit ${code}: ${err}"
[ -e "${REPO}/scripts/controls/uncommitted.row" ] ||
    fail "a row left in the working tree: the file went missing"

# A file past what one object may be is refused while it is being read, rather
# than read into memory and refused afterwards: the copy is streamed to its
# destination a chunk at a time, so what a candidate's tree holds does not set
# this step's memory. The file is sparse, so this costs the disk a megabyte and
# the reading three hundred.
big="${WORK}/big"
rm -rf "${big}"
mkdir -p "${big}"
printf 'what the step started from\n' >"${big}/README"
git -C "${big}" init -q --initial-branch=main
git -C "${big}" add -A
commit_candidate "${big}" base
dd if=/dev/zero of="${big}/sparse.bin" bs=1048576 count=1 seek=300 status=none
git -C "${big}" add -A
commit_candidate "${big}" "a very large file"
run_driver "$(input "${big}" good)"
[ "${code}" -eq 2 ] || fail "a candidate holding a very large file: exit ${code}: ${out}"
case "${out}" in
*"more than the"*"this copies"*) ;;
*) fail "a candidate holding a very large file: ${out}" ;;
esac
[ ! -e "${LOG}" ] || fail "a candidate holding a very large file: cargo ran"
no_scratch "a candidate holding a very large file"

# The same cap, applied where the object has not been read at all: a header that
# declares more than this copies is refused out of the header, and the `git
# cat-file --batch` that was about to stream the body is still sitting there with
# the rest of it in its pipe. A refusal that returns while that child still runs
# has left running the very thing the cap exists to stop, and a shell case that
# only sees the refusal and the removed scratch cannot tell the difference. So
# this git answers one header for an object of 256 MiB and then stays: if the
# refusal does not stop it and reap it, it is still here after the driver has
# answered. The process is named by the arguments the driver started it with, and
# this scan names them too, so it must not answer itself.
big_header="${WORK}/big-header"
rm -rf "${big_header}"
mkdir -p "${big_header}"
printf 'what the step started from\n' >"${big_header}/README"
git -C "${big_header}" init -q --initial-branch=main
git -C "${big_header}" add -A
commit_candidate "${big_header}" base
printf 'and a change on top of it\n' >"${big_header}/CHANGES"
git -C "${big_header}" add -A
commit_candidate "${big_header}" candidate
mkdir -p "${WORK}/big-header-git"
cat >"${WORK}/big-header-git/git" <<SH
#!/usr/bin/env bash
case " \$* " in
*" cat-file "*)
    printf 'a41ee5dd5a1d1fd3de5829b78ef378000e198542 blob 268435457\n'
    sleep 600
    ;;
esac
exec $(command -v git) "\$@"
SH
chmod +x "${WORK}/big-header-git/git"
run_driver "$(input "${big_header}" good)" "${big_header}" "${WORK}/tmp" \
    "${WORK}/big-header-git:${WORK}/bin:${PATH}"
[ "${code}" -eq 2 ] || fail "an object refused from its header: exit ${code}, not 2: ${out}"
grep -q "more than the 268435456 this copies" <<<"${out}" ||
    fail "an object refused from its header: ${out}"
[ ! -e "${LOG}" ] || fail "an object refused from its header: cargo ran"
no_scratch "an object refused from its header"
cat_left="$(python3 - <<'PY'
import os
import time

found = {}
waited = 0.0
while waited < 10:
    here = set()
    for entry in os.listdir("/proc"):
        if not entry.isdigit() or entry == str(os.getpid()):
            continue
        try:
            cmd = open(f"/proc/{entry}/cmdline", "rb").read()
            stat = open(f"/proc/{entry}/stat").read()
        except OSError:
            continue
        text = cmd.decode("utf-8", "replace").replace("\x00", " ")
        if "cat-file" not in text or "--batch" not in text or "big-header-git" in text:
            continue
        if stat.rsplit(") ", 1)[-1].split()[0] == "Z":
            continue
        here[entry] = f"{entry} {stat.rsplit(') ', 1)[-1].split()[0]} {text[:80]}"
    if not here:
        break
    found = here
    time.sleep(0.2)
    waited += 0.2
print(" | ".join(found.values()))
PY
)"
[ -z "${cat_left}" ] ||
    fail "an object refused from its header: git cat-file is still running: ${cat_left}"

# The rows the driver is given come from the candidate, so a commit whose
# scripts/controls holds none has no control to run, whatever the input names.
git -C "${REPO}" rm -qr scripts/controls
commit_candidate "${REPO}" "no rows"
run_driver "$(input "${REPO}" good)"
[ "${code}" -eq 2 ] || fail "a commit with no rows: exit ${code}: ${err}"
grep -q "holds no scripts/controls/good.row" <<<"${out}" ||
    fail "a commit with no rows: ${out}"

# ---------------------------------------------------------------- refusals
#
# Everything the input can get wrong is refused before any control is touched,
# with exit 2 and the reason on stdout, where whoever reads the step sees it.
# None of these reaches cargo at all.

fresh
commit="$(head_commit)"
base="$(git -C "${REPO}" rev-parse 'HEAD^')"
bound() { printf '"bound": {"reference": "%s", "base": "%s"}' "${REF}" "${base}"; }
refused "input that is not JSON" 'not json at all'
refused "input that is a list" '[]'
refused "a field this step does not read" \
    "$(printf '{"source": "%s", "commit": "%s", %s, "rows": ["good"], "rows_dir": "/etc"}' \
        "${REPO}" "${commit}" "$(bound)")"
refused "no source" "$(printf '{"commit": "%s", %s, "rows": ["good"]}' "${commit}" "$(bound)")"
refused "a relative source" \
    "$(printf '{"source": "repo", "commit": "%s", %s, "rows": ["good"]}' "${commit}" "$(bound)")"
refused "a source that is not there" \
    '{"source": "/nowhere-at-all/atomic", "commit": "0123456789012345678901234567890123456789", "bound": {"reference": "refs/atomic/run-1/steps/code/1", "base": "0123456789012345678901234567890123456789"}, "rows": ["good"]}'
refused "a source that is a file" \
    "$(printf '{"source": "%s/scripts/controls-check.sh", "commit": "%s", %s, "rows": ["good"]}' \
        "${REPO}" "${commit}" "$(bound)")"
mkdir -p "${WORK}/plain"
refused "a source with no git above it" \
    "$(printf '{"source": "%s/plain", "commit": "%s", %s, "rows": ["good"]}' \
        "${WORK}" "${commit}" "$(bound)")"
refused "no commit" "$(printf '{"source": "%s", %s, "rows": ["good"]}' "${REPO}" "$(bound)")"
refused "a commit that is not an id" \
    "$(printf '{"source": "%s", "commit": "HEAD", %s, "rows": ["good"]}' "${REPO}" "$(bound)")"
refused "a commit that is an option" \
    "$(printf '{"source": "%s", "commit": "--help", %s, "rows": ["good"]}' "${REPO}" "$(bound)")"
refused "a commit that is a revision range" \
    "$(printf '{"source": "%s", "commit": "HEAD~1..HEAD", %s, "rows": ["good"]}' \
        "${REPO}" "$(bound)")"
refused "a commit that is not there" \
    "$(printf '{"source": "%s", "commit": "0123456789012345678901234567890123456789", %s, "rows": ["good"]}' \
        "${REPO}" "$(bound)")"
refused "no bound" \
    "$(printf '{"source": "%s", "commit": "%s", "rows": ["good"]}' "${REPO}" "${commit}")"
refused "a bound that is not an object" \
    "$(printf '{"source": "%s", "commit": "%s", "bound": "%s", "rows": ["good"]}' \
        "${REPO}" "${commit}" "${REF}")"
refused "a bound that names no ref" \
    "$(printf '{"source": "%s", "commit": "%s", "bound": {"base": "%s"}, "rows": ["good"]}' \
        "${REPO}" "${commit}" "${base}")"
refused "a bound with a field this does not read" \
    "$(printf '{"source": "%s", "commit": "%s", "bound": {"reference": "%s", "base": "%s", "runner": "/tmp/x"}, "rows": ["good"]}' \
        "${REPO}" "${commit}" "${REF}" "${base}")"
refused "a reference outside atomic's own refs" \
    "$(printf '{"source": "%s", "commit": "%s", "bound": {"reference": "refs/heads/main", "base": "%s"}, "rows": ["good"]}' \
        "${REPO}" "${commit}" "${base}")"
refused "a reference that is a branch" \
    "$(printf '{"source": "%s", "commit": "%s", "bound": {"reference": "HEAD", "base": "%s"}, "rows": ["good"]}' \
        "${REPO}" "${commit}" "${base}")"
refused "a reference that walks the tree" \
    "$(printf '{"source": "%s", "commit": "%s", "bound": {"reference": "refs/atomic/../../..%%2fsteps/code/1", "base": "%s"}, "rows": ["good"]}' \
        "${REPO}" "${commit}" "${base}")"
refused "a reference to attempt zero" \
    "$(printf '{"source": "%s", "commit": "%s", "bound": {"reference": "refs/atomic/run-1/steps/code/0", "base": "%s"}, "rows": ["good"]}' \
        "${REPO}" "${commit}" "${base}")"
refused "a reference with no attempt" \
    "$(printf '{"source": "%s", "commit": "%s", "bound": {"reference": "refs/atomic/run-1/steps/code", "base": "%s"}, "rows": ["good"]}' \
        "${REPO}" "${commit}" "${base}")"
refused "a base that is not an id" \
    "$(printf '{"source": "%s", "commit": "%s", "bound": {"reference": "%s", "base": "HEAD^"}, "rows": ["good"]}' \
        "${REPO}" "${commit}" "${REF}")"
refused "no rows" \
    "$(printf '{"source": "%s", "commit": "%s", %s, "rows": []}' "${REPO}" "${commit}" "$(bound)")"
refused "rows that are not a list" \
    "$(printf '{"source": "%s", "commit": "%s", %s, "rows": "good"}' \
        "${REPO}" "${commit}" "$(bound)")"
refused "a row name with a path in it" \
    "$(printf '{"source": "%s", "commit": "%s", %s, "rows": ["../escape"]}' \
        "${REPO}" "${commit}" "$(bound)")"
refused "a row name that is an option" \
    "$(printf '{"source": "%s", "commit": "%s", %s, "rows": ["--x"]}' \
        "${REPO}" "${commit}" "$(bound)")"
refused "a row name with a slash in it" \
    "$(printf '{"source": "%s", "commit": "%s", %s, "rows": ["a/b"]}' \
        "${REPO}" "${commit}" "$(bound)")"
refused "a row named twice" \
    "$(printf '{"source": "%s", "commit": "%s", %s, "rows": ["good", "good"]}' \
        "${REPO}" "${commit}" "$(bound)")"
refused "the field a row_limit used to be" \
    "$(printf '{"source": "%s", "commit": "%s", %s, "rows": ["good"], "row_limit": 1}' \
        "${REPO}" "${commit}" "$(bound)")"
refused "a row_seconds that is not a number" \
    "$(printf '{"source": "%s", "commit": "%s", %s, "rows": ["good"], "row_seconds": "12"}' \
        "${REPO}" "${commit}" "$(bound)")"
refused "a row_seconds past the step's own wall time" \
    "$(printf '{"source": "%s", "commit": "%s", %s, "rows": ["good"], "row_seconds": 900}' \
        "${REPO}" "${commit}" "$(bound)")"
refused "a row_seconds of zero" \
    "$(printf '{"source": "%s", "commit": "%s", %s, "rows": ["good"], "row_seconds": 0}' \
        "${REPO}" "${commit}" "$(bound)")"
refused "a step_seconds past the exec command's five minutes" \
    "$(printf '{"source": "%s", "commit": "%s", %s, "rows": ["good"], "step_seconds": 900}' \
        "${REPO}" "${commit}" "$(bound)")"
refused "a setup_seconds with no room in it" \
    "$(printf '{"source": "%s", "commit": "%s", %s, "rows": ["good"], "setup_seconds": 0}' \
        "${REPO}" "${commit}" "$(bound)")"
refused "a setup_seconds past what the step has" \
    "$(printf '{"source": "%s", "commit": "%s", %s, "rows": ["good"], "setup_seconds": 290}' \
        "${REPO}" "${commit}" "$(bound)")"
# The three pins are read from the environment, which the step's declaration
# fills in, and never from the input, so that nothing the change carries can
# name the thing that proves it.
pins "" "" ""
refused "a declaration that names neither the runner nor the driver" "$(input "${REPO}" good)"
grep -q "CONTROL_DRIVER_SHA256 is ''" <<<"${out}" ||
    fail "a declaration that names neither the runner nor the driver: ${out}"
pins "" "" "${DRIVER_SHA}"
refused "a runner the declaration does not name" "$(input "${REPO}" good)"
grep -q "CONTROL_RUNNER is not set to a path" <<<"${out}" ||
    fail "a runner the declaration does not name: ${out}"
pins

# Without TMPDIR there is nowhere the copy and the switched-off source may
# live, so the step refuses rather than writing wherever it likes. A TMPDIR
# that is a file, and one this user cannot enter, are refused for what they
# are -- and the mode case only shows anything as a user who is not above
# modes, which is checked first so that under root it fails rather than
# passing for nothing.
out="$(printf '%s' "$(input "${REPO}" good)" |
    env -u TMPDIR CARGO_LOG="${LOG}" CARGO_PID="${WORK}/cargo-pid" \
        CONTROL_FILES="${CONTROL_FILES}" PATH="${WORK}/bin:${PATH}" "${PINS[@]}" \
        python3 "${DRIVER}" 2>/dev/null)"
code=$?
[ "${code}" -eq 2 ] || fail "no TMPDIR: exit ${code}, not 2: ${out}"
grep -q "TMPDIR is not set" <<<"${out}" || fail "no TMPDIR: ${out}"
: >"${WORK}/not-a-dir"
run_driver "$(input "${REPO}" good)" "${REPO}" "${WORK}/not-a-dir"
[ "${code}" -eq 2 ] || fail "a TMPDIR that is a file: exit ${code}: ${out}"
grep -q "is not a directory" <<<"${out}" || fail "a TMPDIR that is a file: ${out}"
mkdir -p "${WORK}/locked"
chmod 000 "${WORK}/locked"
if [ "$(id -u)" -eq 0 ]; then
    fail "the unreadable TMPDIR case shows nothing to the superuser, so it proves nothing here"
fi
run_driver "$(input "${REPO}" good)" "${REPO}" "${WORK}/locked"
[ "${code}" -eq 2 ] || fail "an unreadable TMPDIR: exit ${code}, not 2: ${out}"
grep -q "which this step cannot write" <<<"${out}" || fail "an unreadable TMPDIR: ${out}"
chmod 700 "${WORK}/locked"
# The proof runner may not live in the scratch the candidate's own commands
# write in either, whatever its bytes: that directory is where a hostile test
# could put a file, which is why every row gets a fresh link to it.
mkdir -p "${WORK}/tmp"
cp "${RUNNER}" "${WORK}/tmp/runner.sh"
pins "${WORK}/tmp/runner.sh" "${RUNNER_SHA}"
run_driver "$(input "${REPO}" good)"
[ "${code}" -eq 2 ] || fail "a runner inside this step's scratch: exit ${code}, not 2: ${out}"
grep -q "inside this step's own scratch" <<<"${out}" ||
    fail "a runner inside this step's scratch: ${out}"
[ ! -e "${LOG}" ] || fail "a runner inside this step's scratch: cargo ran"
rm -f "${WORK}/tmp/runner.sh"
no_scratch "a runner inside this step's scratch"
pins

# A source the candidate cannot be read out of is refused, with git's own
# answer rather than a silent pass. Here the repository is gone entirely,
# which is git's "not a git repository" and not an absence to step over.
repo="${REPO}"
mv "${repo}/.git" "${WORK}/stashed-git"
run_driver "$(printf '{"source": "%s", "commit": "%s", %s, "rows": ["good"]}' \
    "${repo}" "${commit}" "$(bound)")"
mv "${WORK}/stashed-git" "${repo}/.git"
[ "${code}" -eq 2 ] || fail "a source whose git is gone: exit ${code}, not 2: ${out}"
grep -q "not a git repository" <<<"${out}" || fail "a source whose git is gone: ${out}"
[ ! -e "${LOG}" ] || fail "a source whose git is gone: cargo ran"
no_scratch "a source whose git is gone"

# A git that answers but has nothing to say about the tree is refused, not
# copied: a copy with nothing in it is not the candidate, and a control that is
# not in it cannot be shown to bite. This git is honest about the ref and the
# tree and empty about everything else, as a repository with a pruned object
# store would be, and the driver's rule is what stops the run -- whoever is on
# the PATH.
mkdir -p "${WORK}/empty-git"
cat >"${WORK}/empty-git/git" <<SH
#!/usr/bin/env bash
case " \$* " in
*" ls-tree "*) exit 0 ;;
esac
exec $(command -v git) "\$@"
SH
chmod +x "${WORK}/empty-git/git"
run_driver "$(input "${REPO}" good)" "${REPO}" "${WORK}/tmp" "${WORK}/empty-git:${WORK}/bin:${PATH}"
[ "${code}" -eq 2 ] || fail "a git that holds no files: exit ${code}, not 2: ${out}"
grep -q "holds no files" <<<"${out}" || fail "a git that holds no files: ${out}"
[ ! -e "${LOG}" ] || fail "a git that holds no files: cargo ran"
no_scratch "a git that holds no files"

# --------------------------------------------------------- how a row ends
#
# The verdicts above come from the real scripts/controls-check.sh. These ask
# the one function that reads a row's ending directly, because a row can end
# in ways no fixture reaches without a Rust toolchain -- and every one of them
# must fail the step. A reason this list does not hold is never a pass.
classify_case() { # classify_case EXPECTED NAME CODE TIMED OVERFLOW STDOUT
    local got
    got="$(cd "${ROOT}" && python3 -c '
import importlib.util
import sys

spec = importlib.util.spec_from_file_location("cx", "scripts/controls-exec.py")
cx = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cx)
verdict, _ = cx.classify(
    sys.argv[1], int(sys.argv[2]), sys.argv[3] == "1", sys.argv[4] == "1",
    sys.argv[5].encode(), b"")
print(verdict)
' "$2" "$3" "$4" "$5" "$6")"
    [ "${got}" = "$1" ] || fail "$1: the driver read $2 as ${got}, not $1: $6"
}

classify_case passed good 0 0 0 \
    "ok good: its test fails with the control off and passes with it on"
classify_case blind good 1 0 0 "FAIL good: its test passed with the control off"
classify_case switch-stale good 1 0 0 \
    "FAIL good: its switch no longer applies to src/guard.rs"
classify_case build-failed good 1 0 0 \
    "FAIL good: with the control off the code does not build"
classify_case no-test good 1 0 0 "FAIL good: no test ran"
classify_case not-seen-failing good 1 0 0 \
    "FAIL good: with the control off, these tests were not seen failing: a_test"
classify_case fails-when-on good 1 0 0 "FAIL good: its test fails with the control on"
classify_case errored good 101 0 0 \
    "ok good: its test fails with the control off and passes with it on"
classify_case errored good 0 0 0 "FAIL good: its test passed with the control off"
classify_case errored good 1 0 0 "FAIL good: the build agent ate my homework"
classify_case errored good 0 0 0 "ok good: something else entirely"
classify_case errored good 1 0 0 \
    "scripts/controls-check.sh: cannot create target/controls-check"
classify_case errored good 1 0 0 \
    "ok other: its test fails with the control off and passes with it on"
classify_case timed-out good 0 1 0 \
    "ok good: its test fails with the control off and passes with it on"
classify_case errored good 0 0 1 "ok good: its test fails with the control off"
classify_case blind good 1 0 0 \
    "ok good: its test fails with the control off and passes with it on
FAIL good: its test passed with the control off"

# The reasons scripts/controls-check.sh ends a row with are exactly the ones
# the driver's list holds. A message that moves without the list moving with
# it fails here, where it is a named case, rather than reading as a pass.
(cd "${ROOT}" && python3 - scripts/controls-check.sh <<'PY'
import importlib.util
import re
import sys

text = open("scripts/controls-check.sh").read()
ends = set(re.findall(r'echo "(?:ok|FAIL) \$\{name\}: ([^"&]*)', text))
spec = importlib.util.spec_from_file_location("cx", "scripts/controls-exec.py")
cx = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cx)
known = tuple(prefix for prefix, _ in cx.REASONS)
if not ends:
    sys.exit("scripts/controls-check.sh ends no row with a reason this can read")
missing = sorted(end for end in ends if not any(end.startswith(k) for k in known))
if missing:
    sys.exit(f"scripts/controls-check.sh ends rows with reasons not listed: {missing}")
PY
) || fail "the reasons a row ends with no longer match the driver's list"

# A row file that is not a regular file in the candidate holds no control, and
# is refused as that rather than read. A link inside the tree is copied happily
# and is what says so here, since a mode that refuses reads is one the user
# running this can be above.
links="${WORK}/links"
mkdir -p "${links}/scripts/controls"
cp "${ROOT}/scripts/controls-check.sh" "${links}/scripts/"
printf 'what the step started from\n' >"${links}/README"
git -C "${links}" init -q --initial-branch=main
git -C "${links}" add README
commit_candidate "${links}" base
printf 'a file\n' >"${links}/scripts/controls/other.row"
ln -s other.row "${links}/scripts/controls/good.row"
git -C "${links}" add -A
commit_candidate "${links}" linking
run_driver "$(input "${links}" good)"
[ "${code}" -eq 2 ] || fail "a row file that is a link: exit ${code}, not 2: ${out}"
grep -q "good: scripts/controls/good.row is not a regular file" <<<"${out}" ||
    fail "a row file that is a link: ${out}"
no_scratch "a row file that is a link"

# A tree holding a link that leaves it is refused, and nothing of it is left
# behind: `git ls-tree` lists the link, and the rule that reads it stops the
# run before anything is copied out or switched off.
mkdir -p "${WORK}/escape"
printf 'a file\n' >"${WORK}/escape/file.txt"
git -C "${WORK}/escape" init -q --initial-branch=main
git -C "${WORK}/escape" add -A
commit_candidate "${WORK}/escape" plain
ln -s /etc/passwd "${WORK}/escape/escape"
git -C "${WORK}/escape" add -A
commit_candidate "${WORK}/escape" escaping
run_driver "$(input "${WORK}/escape" good)"
[ "${code}" -eq 2 ] || fail "a link that leaves the copy: exit ${code}, not 2: ${out}"
grep -q "leaves the copy" <<<"${out}" || fail "a link that leaves the copy: ${out}"
no_scratch "a link that leaves the copy"

# A commit holding a submodule holds something this cannot copy out, and is
# refused rather than copied without it. `git ls-tree` calls it a gitlink, and
# the run stops before anything is switched off in a partial copy.
sub="${WORK}/submodule"
mkdir -p "${sub}"
printf 'a file\n' >"${sub}/file.txt"
git -C "${sub}" init -q --initial-branch=main
git -C "${sub}" add -A
commit_candidate "${sub}" plain
git -C "${sub}" update-index --add --cacheinfo \
    "160000,$(git -C "${sub}" rev-parse HEAD:file.txt),nested"
commit_candidate "${sub}" with-gitlink --allow-empty
run_driver "$(input "${sub}" good)"
[ "${code}" -eq 2 ] || fail "a commit with a submodule: exit ${code}, not 2: ${out}"
grep -q "only a file or a link can be copied out" <<<"${out}" ||
    fail "a commit with a submodule: ${out}"
no_scratch "a commit with a submodule"

# ------------------------------------------------- the answer before the tidy-up
#
# The verdict is the reason the step runs, and the copy's removal is tidiness. A
# command that finishes its cleanup and then answers has nothing to answer with
# when the backend's kill lands between the two, so the answer goes out first and
# the removal after it. Nothing here is that slow -- the copy is a repository, not
# a build tree -- so this says it with a filesystem that is: `python3` on the PATH
# is a stub that runs the real python3 with `shutil.rmtree` made to take three
# seconds, and to write down the moment it finished. The stdout file's own
# timestamp says when the answer was flushed: it has to be before the removal
# finished, by more than the clock can mean.
slow_py="${WORK}/slow-py"
mkdir -p "${slow_py}"
cat >"${slow_py}/sitecustomize.py" <<'PY'
import os
import shutil
import time

_real = shutil.rmtree


def _slow(path, *args, **kwargs):
    # The filesystem takes its time before it will free this tree at all, and
    # then the removal itself. Either way it is finished, and the moment is
    # written down, three seconds after whoever called it started.
    time.sleep(3)
    started = time.time()
    result = _real(path, *args, **kwargs)
    with open(os.environ["RMTREE_DONE"], "w") as handle:
        handle.write(f"{started:.3f} {time.time():.3f}\n")
    return result


shutil.rmtree = _slow
PY
cat >"${slow_py}/python3" <<SH
#!/usr/bin/env bash
if [ -n "\${RMTREE_DONE:-}" ] && [ "\$1" = "${DRIVER}" ]; then
    PYTHONPATH="${slow_py}" exec $(command -v python3) "\$@"
fi
exec $(command -v python3) "\$@"
SH
chmod 755 "${slow_py}/python3"
rm_file="${WORK}/rmtree-done"
rm -f "${LOG}" "${rm_file}"
printf '%s' "$(input "${REPO}" good)" |
    env TMPDIR="${WORK}/tmp" CARGO_LOG="${LOG}" CARGO_PID="${WORK}/cargo-pid" \
        PROBE_OUT="${WORK}/probe.txt" CONTROL_FILES="${CONTROL_FILES}" \
        RMTREE_DONE="${rm_file}" PATH="${slow_py}:${WORK}/bin:${PATH}" "${PINS[@]}" \
        python3 "${DRIVER}" >"${WORK}/slow-out.json" 2>"${WORK}/stderr.log"
slow_code=$?
slow_err="$(cat "${WORK}/stderr.log")"
[ "${slow_code}" -eq 0 ] ||
    fail "the answer goes out before the copy is removed: exit ${slow_code}: ${slow_err}"
grep -q '"ok": true' "${WORK}/slow-out.json" ||
    fail "the answer goes out before the copy is removed: ${WORK}/slow-out.json"
[ -s "${rm_file}" ] ||
    fail "the answer goes out before the copy is removed: the copy was never removed"
answered="$(stat -c %Y "${WORK}/slow-out.json")"
removed="$(stat -c %Y "${rm_file}")"
[ "${answered}" -lt "${removed}" ] ||
    fail "the answer goes out before the copy is removed: the answer was written at ${answered}
and the copy's removal finished at ${removed}, so a kill during the removal would have
lost the answer: $(cat "${rm_file}")"
no_scratch "the answer goes out before the copy is removed"

# The last thing the step does is ask whether it still has room to say anything.
# `report_seconds` of the step's wall time was kept back for exactly this, so
# reaching it with less than `MIN_SECONDS` left means something outside the
# step's accounting took the time, and what is written then may be read as half a
# verdict by whoever's kill arrives next. So the step says that instead, and
# claims nothing about rows it did not see stopped. This asks the function the
# step calls, in the two states it can be in.
late="$(python3 - "${DRIVER}" <<'PY'
import importlib.util
import sys

spec = importlib.util.spec_from_file_location("cx", sys.argv[1])
cx = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cx)
want = {"step_seconds": 90}
lines = []
for left in (1.0, -2.0, 5.0, 20.0):
    payload = {
        "ok": True,
        "error": None,
        "rows": [{"name": "good", "verdict": "passed", "detail": "proved"}],
        "passed": 1,
        "failed": 0,
    }
    try:
        cx.answer_room(want, payload, left)
        lines.append(
            f"{left} allowed rows={len(payload['rows'])} ok={payload['ok']} "
            f"failed={payload['failed']}"
        )
    except cx.Refused as e:
        lines.append(
            f"{left} refused rows={len(payload['rows'])} ok={payload['ok']} "
            f"failed={payload['failed']} {e}"
        )
print("\n".join(lines))
PY
)"
grep -q "^1.0 refused rows=0 ok=False failed=0" <<<"${late}" ||
    fail "a step out of room to answer: it kept verdicts, or said nothing: ${late}"
grep -q "less than the 5s it needs to write an answer" <<<"${late}" ||
    fail "a step out of room to answer: ${late}"
grep -q "^-2.0 refused rows=0 ok=False failed=0" <<<"${late}" ||
    fail "a step that is already late: it kept verdicts: ${late}"
grep -q "^5.0 allowed rows=1 ok=True failed=0" <<<"${late}" ||
    fail "a step with exactly the least time it needs: it refused anyway: ${late}"
grep -q "^20.0 allowed rows=1 ok=True failed=0" <<<"${late}" ||
    fail "a step with room left: it refused anyway: ${late}"

# And the guard bites, which is how a control is tested here: the threshold
# removed from a copy of the driver, and the same four questions asked of that
# copy. It is done to a copy rather than to `scripts/controls-exec.py` with a row
# because what a row would switch is the call at the end of the step, and nothing
# a case can see changes when only the call goes -- reaching the refusal needs a
# row stopped at the step's own deadline with its teardown still running, which
# is a fixture that spends the step's whole wall time and then some. What decides
# the answer is the threshold, so that is what gets removed, and both states have
# to answer the other way.
unbit="$(python3 - "${DRIVER}" "${WORK}" <<'PY'
import importlib.util
import os
import sys

text = open(sys.argv[1]).read()
needle = "    if left >= MIN_SECONDS:\n        return\n"
if text.count(needle) != 1:
    sys.exit(f"the answer reserve's threshold appears {text.count(needle)} times, not once")
off = os.path.join(sys.argv[2], "driver-no-answer-threshold.py")
open(off, "w").write(text.replace(needle, "    return\n"))
spec = importlib.util.spec_from_file_location("cx", off)
cx = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cx)
lines = []
for left in (1.0, -2.0):
    payload = {
        "ok": True,
        "error": None,
        "rows": [{"name": "good", "verdict": "passed", "detail": "proved"}],
        "passed": 1,
        "failed": 0,
    }
    try:
        cx.answer_room({"step_seconds": 90}, payload, left)
        lines.append(f"{left} allowed rows={len(payload['rows'])} ok={payload['ok']}")
    except cx.Refused as e:
        lines.append(f"{left} refused {e}")
print("\n".join(lines))
PY
)"
grep -q "^1.0 allowed rows=1 ok=True" <<<"${unbit}" ||
    fail "the answer reserve with its threshold removed: it still refused: ${unbit}"
grep -q "^-2.0 allowed rows=1 ok=True" <<<"${unbit}" ||
    fail "the answer reserve with its threshold removed: it still refused: ${unbit}"

if [ "${failures}" -ne 0 ]; then
    echo "${failures} controls-exec test(s) failed" >&2
    exit 1
fi
echo "controls-exec tests passed" >&2
