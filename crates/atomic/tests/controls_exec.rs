// SPDX-License-Identifier: Apache-2.0

//! The controls as an exec step: an exec atom reads a candidate commit out of
//! git, copies it into the scratch directory of its own attempt, and asks the
//! step's trusted `scripts/controls-check.sh`, once per declared row, whether
//! that row's test fails with its control off and passes with it back. A row
//! whose test still passes with the control off fails the step, which is the
//! whole point of the step.
//!
//! It runs the real driver, `scripts/controls-exec.py`, and the real
//! `scripts/controls-check.sh` it drives, as included from this checkout, so
//! what is tested is what CI and a workflow run. Only `cargo` is stood in for:
//! a stand-in in the fixture's `bin` answers the tests the fixture's rows name,
//! and its `sandbox` answer is a probe of this sandbox, so the row's own
//! command is what proves the confinement. The step runs confined: it reaches
//! no network, writes nothing outside its scratch directory, cannot read
//! atomic's private state, calls no model and needs no provider.
//!
//! What is proved is what the *coding step* made, and not otherwise. The
//! candidate is a real `Worktrees` attempt: its worktree is prepared from the
//! base, a change is written in it, and [`Worktrees::commit`] records the
//! commit, its base and the ref that keeps it alive. The step is asked about
//! that record, and nothing else -- an easier commit, another repository, a
//! commit the attempt's ref does not name, and a row the candidate does not
//! carry are each refused. The candidate is read out of git's object store and
//! never out of a working tree, which a case shows by leaving the source dirty,
//! with one control already switched off, and asking for the same commit. The
//! copy is then *bound* to that commit rather than committed: its HEAD and index
//! name the candidate and its tree, reached through one alternate to the object
//! store the candidate came out of, and the copy holds no commit of its own --
//! atomic makes git commits outside the sandbox, and a scratch copy has no need
//! of one. The row's own command reports the copy's state from inside it, so a
//! copy that carried a commit of its own would say so and fail the step.
//!
//! With what it is proved is likewise settled outside the change. The proof
//! runner and the driver are named and hashed by the step's own declaration --
//! the atom's `env`, in a file whose hash the run pins -- and a candidate that
//! carries its own check script printing a passing line, or its own driver,
//! gets no verdict from those bytes: the first is never started, the second
//! refuses to answer. A row cannot rewrite the runner it is started through
//! either: the sandbox refuses the write through that link, which the row's own
//! probe reports, and the driver hashes those bytes again when the row ends.
//!
//! Needs bubblewrap and unprivileged user namespaces, so it runs with the
//! `sandbox-tests` feature; with the feature on, a sandbox that cannot start
//! fails the test.

#![cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#![recursion_limit = "512"]

mod support;

use atomic_core::codex::{CodexPaths, ExecSandbox, StateGuard};
use atomic_core::git::{Git, Signature};
use atomic_core::runtime::{self, EffectIdentity, RunMetadata};
use atomic_core::testing::on_codex_runtime;
use atomic_core::typed::AtomError;
use atomic_core::worktree::{Caches, StepCommit, Worktrees};
use atomic_exec::{ExecutionBackend, LocalBackend};
use atomic_schema::registry::Registry;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

static FIXTURE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

/// The step's driver, as this checkout holds it.
const DRIVER: &str = include_str!("../../../scripts/controls-exec.py");
/// The proof runner the step is declared with, as this checkout holds it. The
/// candidate carries a copy of the same file, because every tree in this
/// repository does; the step never starts that copy.
const CHECK: &str = include_str!("../../../scripts/controls-check.sh");

/// What a row's result says when the row is proved.
const PROVED: &str = "its test fails with the control off and passes with it on";

/// A check script that proves nothing, in the shape a change that wanted a free
/// pass would hold it: every row it is asked about "passes", nothing is switched
/// off, and it exits 0. It claims the passing words a proved row really earns,
/// which is why the step must not run the copy the change carries.
fn forged_check() -> String {
    format!(
        "#!/usr/bin/env bash\n\
         # every control passes, as its own candidate says\n\
         if [ \"$#\" -ge 1 ]; then\n\
         \x20 echo \"ok ${{1}}: {PROVED}\"\n\
         fi\n\
         exit 0\n"
    )
}

/// The step's exec atom. Its `PATH` is all the confined command is given
/// besides its own `HOME`, `TMPDIR` and effect identity: the fixture's stand-in
/// `cargo` first, then the image's `git`, `bash` and `python3`. The places the
/// stand-in probes are named in what the atom declares, so that the sandbox is
/// proved by the row's own command rather than beside it.
///
/// `command` is the driver's own path and `pins` the three entries that say
/// what this step will run and prove with: where the proof runner is, and the
/// SHA-256 of its bytes and of the driver's. They are here, in the atom, and not
/// in the input, because the input comes from the change's own record and must
/// not be able to name the thing that judges it.
/// What the row's command can reach, and what it is told to probe: the stand-in
/// `bin` a row runs with, the project, the candidate's repository, and the
/// private state it must not read. A row that writes in the first three, or
/// reads the last, says so through its own output.
struct Probe<'a> {
    bin: &'a Path,
    project: &'a Path,
    source: &'a Path,
    secret: &'a Path,
}

fn atom_source(name: &str, command: &str, pins: &str, probe: &Probe, address: &str) -> String {
    format!(
        "---\n\
         import type {{ Props, Input, Output }} from '../.atomic/types/{name}';\n\
         import type {{ AtomConfig }} from '../.atomic/types/authoring';\n\
         const {{ input }}: Props = Astro.props;\n\
         export const config: AtomConfig<Input, Output> = {{\n\
         \x20 description: 'Prove each declared control row by removing it.',\n\
         \x20 kind: 'exec',\n\
         \x20 exec: {{ command: ['python3', '{command}'] }},\n\
         \x20 env: {{\n\
         \x20\x20 PATH: '{bin}:/usr/bin:/bin',\n\
         \x20\x20 CONTROL_FILES: 'src/good.rs src/sandbox.rs src/blind.rs',\n\
         \x20\x20 PROBE_SOURCE: '{source}',\n\
         \x20\x20 PROBE_PROJECT: '{project}',\n\
         \x20\x20 PROBE_STATE: '{secret}',\n\
         \x20\x20 PROBE_ADDRESS: '{address}',\n\
         {pins}\
         \x20 }},\n\
         \x20 effects: {{ external: 'read-only' }},\n\
         }};\n---\n",
        name = name,
        command = command,
        pins = pins,
        bin = probe.bin.display(),
        project = probe.project.display(),
        source = probe.source.display(),
        secret = probe.secret.display(),
        address = address,
    )
}

/// The three pins, as an atom's `env` holds them. An empty one is a declaration
/// that names nothing, which the driver refuses rather than defaults.
fn pins(runner: &str, runner_sha: &str, driver_sha: &str) -> String {
    format!(
        "\x20\x20 CONTROL_RUNNER: '{runner}',\n\
         \x20\x20 CONTROL_RUNNER_SHA256: '{runner_sha}',\n\
         \x20\x20 CONTROL_DRIVER_SHA256: '{driver_sha}',\n"
    )
}

/// The same three pins with the runner named the only way a declaration written
/// into a repository file can name it: by a path of its own, beside the driver
/// that `exec.command` starts. A checkout is copied to another machine, moved to
/// another directory and mounted into a container, and a declaration holding the
/// path it was written at then names whatever stands there; this one follows the
/// driver, and the two hashes are what make it the same runner either way.
fn relative_pins(runner_sha: &str, driver_sha: &str) -> String {
    pins("../../harness/controls-check.sh", runner_sha, driver_sha)
}

/// What the step is asked, and what it answers. The input is the coding step's
/// own record -- the commit, the base it was made on, and the ref that keeps it
/// -- plus the rows to prove; the output is exactly the one JSON value the
/// driver writes, so a field the driver stops reporting, or a new one, fails
/// here instead of passing on a stand-in's shape.
const INPUT_SCHEMA: &str = concat!(
    "type: object\n",
    "additionalProperties: false\n",
    "required: [source, commit, bound, rows]\n",
    "properties:\n",
    "  source: {type: string}\n",
    "  commit: {type: string}\n",
    "  bound:\n",
    "    type: object\n",
    "    additionalProperties: false\n",
    "    required: [reference, base, admitted]\n",
    "    properties:\n",
    "      reference: {type: string}\n",
    "      base: {type: string}\n",
    "      admitted: {type: string}\n",
    "  rows: {type: array, items: {type: string}}\n",
    "  row_seconds: {type: integer}\n",
    "  setup_seconds: {type: integer}\n",
    "  report_seconds: {type: integer}\n",
    "  step_seconds: {type: integer}\n",
);

const OUTPUT_SCHEMA: &str = concat!(
    "type: object\n",
    "additionalProperties: false\n",
    "required: [ok, error, source, commit, tree, bound, rows, passed, failed]\n",
    "properties:\n",
    "  ok: {type: boolean}\n",
    "  error: {type: [string, \"null\"]}\n",
    "  source: {type: [string, \"null\"]}\n",
    "  commit: {type: [string, \"null\"]}\n",
    "  tree: {type: [string, \"null\"]}\n",
    "  bound:\n",
    "    type: [object, \"null\"]\n",
    "    additionalProperties: false\n",
    "    required: [reference, base, admitted]\n",
    "    properties:\n",
    "      reference: {type: string}\n",
    "      base: {type: string}\n",
    "      admitted: {type: string}\n",
    "  rows:\n",
    "    type: array\n",
    "    items:\n",
    "      type: object\n",
    "      additionalProperties: false\n",
    "      required: [name, verdict, detail]\n",
    "      properties:\n",
    "        name: {type: string}\n",
    "        verdict: {type: string}\n",
    "        detail: {type: string}\n",
    "  passed: {type: integer}\n",
    "  failed: {type: integer}\n",
);

/// A stand-in for `cargo` that answers the way cargo answers, so that the
/// driver and `scripts/controls-check.sh` do all the switching, restoring and
/// reading between them. Each test a row names is answered as
/// `fixture/cargo.json` in the tree being checked says:
///
/// - `fails-off` fails while a file a row switches differs from HEAD, as a
///   test that really guards its control does;
/// - `blind` passes however it stands, which is the row the step exists to
///   catch;
/// - `sandbox` fails whenever the sandbox let its command reach something it
///   should not, naming what it reached in the test's name, so a row whose
///   command reached the network, wrote outside its scratch, or read a denied
///   directory fails the step and says which. The same probe also asks how the
///   copy it runs in is bound to its candidate -- a HEAD and an index naming
///   that commit, its files as its tree holds them, and no commit object of the
///   copy's own, because a commit made inside the sandbox is what this step no
///   longer does -- and reports anything else in the same words.
const STAND_IN: &str = r#"#!/usr/bin/env python3
import json
import os
import socket
import subprocess
import sys

names, after, skip = [], 0, 0
for a in sys.argv[1:]:
    if skip:
        skip = 0
        continue
    if after:
        if not a.startswith('-'):
            names.append(a)
        continue
    if a == '--':
        after = 1
        continue
    if a in ('-p', '--package', '--features', '--test', '--bin', '--example', '--color'):
        skip = 1


def control_off():
    """Whether a file a row here switches is switched off right now."""
    for path in os.environ.get('CONTROL_FILES', '').split():
        if not os.path.exists(path):
            continue
        done = subprocess.run(
            ['git', 'diff', '--quiet', '--', path],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        if done.returncode != 0:
            return True
    return False


def ask(*args):
    """Ask git one question from inside the copy, and answer with its stdout."""
    done = subprocess.run(['git', *args], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    return done.stdout.decode('utf-8', 'replace')


def own(kind):
    """How many objects of one type the copy's own object store holds.

    Only the loose objects under this repository's `.git/objects`, directory by
    directory: what the copy reaches through an alternate is not what it holds,
    so a commit made inside the sandbox is counted here and a commit merely read
    through the trusted store is not.
    """
    root = os.path.join('.git', 'objects')
    try:
        entries = sorted(os.listdir(root))
    except OSError:
        return 0
    found = 0
    for entry in entries:
        path = os.path.join(root, entry)
        if entry in ('info', 'pack') or not os.path.isdir(path):
            continue
        for leaf in sorted(os.listdir(path)):
            if ask('cat-file', '-t', entry + leaf).strip() == kind:
                found += 1
    return found


def bound_wrong():
    """How this copy is not the candidate the step says it copied, as facts.

    Asked from inside the copy while a row runs, which is where the claim has to
    hold. The step says it wrote the candidate's tree out of the object store and
    bound the copy to that commit -- a HEAD, an index, nothing else -- without
    making a commit of its own anywhere inside the sandbox. Each thing returned
    here is the step having done something else, and is named for the test the
    row's own command then fails.
    """
    free = []
    commits = own('commit')
    if commits:
        free.append(f'the copy holds {commits} commit object(s) of its own')
    if not ask('rev-parse', '--verify', '-q', 'HEAD').strip():
        free.append('the copy has no HEAD, so nothing here is bound to a candidate')
    if not ask('rev-parse', '--verify', '-q', 'HEAD^{tree}').strip():
        free.append("the copy's HEAD names no tree")
    if not ask('ls-files', '--', 'scripts/controls-check.sh').strip():
        free.append("the copy's index does not hold the files it was copied with")
    listed = [
        line
        for line in ask('status', '--porcelain').splitlines()
        if line.strip()
        # The holder this step made for this row's runner link, and the state
        # directory the proof runner keeps its originals in, are the run's own
        # and meant to be there. The one file this row switches is meant to
        # differ while the control is off, and to match again with it back.
        and '.controls-exec-runner-' not in line
        and 'target/' not in line
        and line != '?? target'
        and not (control_off() and 'src/sandbox.rs' in line)
    ]
    if listed:
        free.append('the copy holds ' + ' '.join(line.replace(' ', '-') for line in listed))
    for path in ('src/lib.rs', 'scripts/controls-check.sh', 'fixture/cargo.json'):
        if not os.path.exists(path):
            free.append(f'the copy holds no {path}')
    return free


def reached():
    """What this command can still reach though it should be confined, as a
    list; empty means it is confined. Everything it tries belongs to the test
    that stands here: a loopback listener it started, directories it made.
    Nothing leaves this machine, so this answers the same way with the network
    down."""
    free = []
    address = os.environ.get('PROBE_ADDRESS', '')
    if address:
        host, port = address.split(':')
        try:
            socket.create_connection((host, int(port)), timeout=5).close()
            free.append('network ' + address)
        except Exception:
            pass
    for where in [os.environ.get('PROBE_SOURCE', ''), os.environ.get('PROBE_PROJECT', '')]:
        if not where:
            continue
        try:
            with open(os.path.join(where, 'sandbox-probe'), 'w') as handle:
                handle.write('free')
            free.append('write into ' + where)
        except Exception:
            pass
    secret = os.environ.get('PROBE_STATE', '')
    if secret:
        try:
            with open(secret) as handle:
                if handle.read().strip() == 'secret-in-the-state':
                    free.append('state ' + secret)
        except Exception:
            pass
    # The link this row was started through points at the proof runner the
    # step's declaration named. Writing through it would let a row rewrite the
    # script that reads its own verdict, so the sandbox has to refuse it too.
    # Opened for append with nothing written: bytes the row changed would be
    # caught by the driver as well, and this is meant to say only what the
    # confinement allowed.
    link = os.path.join('.controls-exec-runner-sandbox', 'controls-check.sh')
    try:
        with open(link, 'a'):
            free.append('write through ' + link)
    except Exception:
        pass
    return free


free = reached() + bound_wrong()
print('sandbox probe: ' + ('confined' if not free else 'reached: ' + '; '.join(free)))
with open('fixture/cargo.json') as handle:
    kinds = json.load(handle)
off = control_off()


def reached_as_name(name):
    """A failing test's name, with what the probe reached in it. A confined
    command's own output is the only thing that gets out of the sandbox, and
    scripts/controls-check.sh echoes a failing test's name back when its test
    fails with the control on, so this is how a leak says what it leaked. The
    row's own test name is still in it, which is what lets the script see that
    test failing."""
    tags = '-'.join(t.replace('/', '-').replace(' ', '-').replace(':', '-') for t in free)
    return name + ('_reached_' + tags if free else '')


code = 0
for name in names:
    kind = kinds.get(name, 'fails-off')
    if kind == 'sandbox':
        # Off and confined: it caught the removal, as a guard's test must, and
        # with the control back it passes: a proved row. Anything the command
        # reached fails it whichever way the control stands, so the step ends
        # on "its test fails with the control on" and says what was reached.
        fails = free or (off and not free)
    else:
        fails = off and kind != 'blind'
    if fails:
        print('test fixture::' + reached_as_name(name) + ' ... FAILED')
        print('test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured')
        code = 101
    else:
        print('test fixture::' + name + ' ... ok')
        print('test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured')
sys.exit(code)
"#;

/// One row, as `scripts/controls` holds it: name, file, switch, replacement,
/// and the test it names after `--`.
fn row(name: &str, tests: &str) -> String {
    format!("{name}|src/{name}.rs|let {name}_on = true;|let {name}_on = false;|--lib -- {tests}\n")
}

/// git, asked the way atomic asks it: no hooks, no drivers, no signing, no
/// pager, and neither the tree's nor the user's configuration. This test makes
/// the candidate's repository itself, but a worktree an atom wrote to would be
/// read the same way, and nothing here may start a daemon that outlives the
/// command.
fn git(where_: &Path, args: &[&str]) -> String {
    let mut command = std::process::Command::new("git");
    command
        .current_dir(where_)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args([
            "--no-replace-objects",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.pager=cat",
        ])
        .args(args);
    let out = command.output().expect("git");
    assert!(
        out.status.success(),
        "git {args:?} exited with {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The change's repository as it stands before the coding step: the real
/// `scripts/controls-check.sh`, three rows each switching a file of its own so
/// that any of them may run beside another, the answers the stand-in gives, and
/// a second copy of this step's driver, which is what a change that wanted to
/// answer for itself would carry. Returns the base commit, asked of git rather
/// than remembered.
fn base_of(repo: &Path) -> String {
    for dir in ["scripts/controls", "src", "fixture", "tools"] {
        std::fs::create_dir_all(repo.join(dir)).expect("base dir");
    }
    std::fs::write(repo.join("scripts/controls-check.sh"), CHECK).expect("check script");
    // Committed executable, as this repository holds it: the driver copies the
    // commit's own modes, and refuses a copy it may not run.
    std::fs::set_permissions(
        repo.join("scripts/controls-check.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("check script executable");
    // The driver the change carries, one line away from the one the step is
    // declared with. It is a real file of the tree, executable, and answers for
    // itself if anything runs it.
    std::fs::write(
        repo.join("tools/controls-exec.py"),
        format!("{DRIVER}\n# the change's own copy, not the declared driver\n"),
    )
    .expect("planted driver");
    std::fs::set_permissions(
        repo.join("tools/controls-exec.py"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("planted driver executable");
    let mut modules = String::new();
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"control-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("fixture Cargo manifest");
    let mut answers = serde_json::Map::new();
    for (name, kind) in [
        ("good", "fails-off"),
        ("blind", "blind"),
        ("sandbox", "sandbox"),
    ] {
        std::fs::write(
            repo.join(format!("scripts/controls/{name}.row")),
            row(name, &format!("{name}_guard")),
        )
        .expect("row");
        std::fs::write(
            repo.join(format!("src/{name}.rs")),
            format!("pub fn {name}() -> bool {{\n    let {name}_on = true;\n    {name}_on\n}}\n"),
        )
        .expect("switched file");
        modules.push_str(&format!("pub mod {name};\n"));
        answers.insert(format!("{name}_guard"), Value::String(kind.into()));
    }
    std::fs::write(repo.join("src/lib.rs"), modules).expect("lib");
    std::fs::write(
        repo.join("fixture/cargo.json"),
        format!("{}\n", Value::Object(answers)),
    )
    .expect("answers");
    git(repo, &["init", "-q", "--initial-branch=main"]);
    git(repo, &["add", "-A"]);
    git(
        repo,
        &[
            "-c",
            "user.name=candidate",
            "-c",
            "user.email=candidate@localhost",
            "-c",
            "commit.gpgSign=false",
            "commit",
            "-qm",
            "base",
        ],
    );
    git(repo, &["rev-parse", "HEAD"]).trim().to_owned()
}

/// A listener on loopback that accepts nothing, for the row's command to try
/// to reach. It is the only endpoint either side could reach at all: nothing
/// here goes off this machine, and a row whose command connects to it is a row
/// with a network, which fails the step.
fn loopback_listener() -> (std::net::TcpListener, String) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("listener");
    let address = listener.local_addr().expect("address").to_string();
    (listener, address)
}

/// Every copy the step made and did not remove: its scratch holds one per run,
/// and each must be gone when the step has answered.
fn left_copies(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .fold(Vec::new(), |mut found, entry| {
            let path = entry.path();
            if path.is_dir() {
                if path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("controls-exec-"))
                {
                    found.push(path);
                } else {
                    found.extend(left_copies(&path));
                }
            }
            found
        })
}

/// The SHA-256 of a file's bytes, as the step's declaration pins them.
fn sha256_of(path: &Path) -> String {
    let bytes = std::fs::read(path).expect("bytes to hash");
    let mut digest = Sha256::new();
    digest.update(&bytes);
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct Fixture {
    /// Where everything of this run lives.
    dir: PathBuf,
    /// The project the step's atom belongs to: the exec command's own
    /// directory, which it reads and may not write.
    project: PathBuf,
    /// The change whose controls are being proved: the repository atomic
    /// records its attempts in.
    source: PathBuf,
    /// The worktrees of that repository, as a code-changing step's run has.
    worktrees: Worktrees,
    /// The commit the coding step started from.
    base: String,
    /// What atomic recorded for the coding step: its commit, its base, and the
    /// ref that keeps it. This is what the step is bound to.
    candidate: StepCommit,
    /// The tree of the candidate commit, which is what the step must name.
    tree: String,
    /// The base commit, in a repository of the change's own choosing, holding a
    /// `good` row that passes: what the step would prove if it believed a
    /// `source` it was handed.
    easy: PathBuf,
    /// The state directory the guard is for, as a project's run has one.
    state: PathBuf,
    /// A directory the backend keeps out of its commands' reach, as it keeps
    /// every other attempt's scratch and the state's private half, with a
    /// secret in it for the row's command to try to read.
    private: PathBuf,
}

impl Fixture {
    fn new(address: &str) -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "atomic-controls-exec-{}-{}",
            std::process::id(),
            FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).expect("fixture dir");
        let dir = std::fs::canonicalize(&dir).expect("canonical");
        let (project, source, state, easy) = (
            dir.join("project"),
            dir.join("candidate"),
            dir.join("state"),
            dir.join("easy"),
        );
        let private = dir.join("private");
        let harness = dir.join("harness");
        for sub in ["atoms", "tools"] {
            std::fs::create_dir_all(project.join(sub)).expect("project");
        }
        std::fs::create_dir_all(&harness).expect("harness");
        std::fs::write(project.join("atomic.yaml"), "atomic: 1\nname: controls\n")
            .expect("manifest");
        std::fs::write(project.join("tools/controls-exec.py"), DRIVER).expect("driver");
        // The proof runner the step is declared with, outside every candidate
        // this fixture makes: what a change cannot supply.
        let runner = harness.join("controls-check.sh");
        std::fs::write(&runner, CHECK).expect("harness runner");
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755))
            .expect("harness runner executable");
        let stand_in = dir.join("bin/cargo");
        std::fs::write(&stand_in, STAND_IN).expect("stand-in");
        std::fs::set_permissions(&stand_in, std::fs::Permissions::from_mode(0o755))
            .expect("stand-in executable");
        // A place the backend denies, with something in it worth reading: what
        // the row's command tries and must not get.
        std::fs::create_dir_all(&private).expect("private dir");
        std::fs::write(private.join("secret"), "secret-in-the-state").expect("secret");
        let base = base_of(&source);
        let worktrees = Worktrees::new(
            Git::find().expect("git").discover(&source).expect("repo"),
            state.join("worktrees"),
        );
        let candidate = Self::code(
            &worktrees,
            &base,
            "code",
            "The coding step's change",
            &|tree| {
                std::fs::write(
                    tree.join("src/made.rs"),
                    "pub fn made() -> u8 {\n    1\n}\n",
                )
                .expect("the step's change");
            },
        )
        .expect("the coding step's candidate");
        let spec = format!("{}^{{tree}}", candidate.commit);
        let tree = git(&source, &["rev-parse", spec.as_str()])
            .trim()
            .to_owned();
        // A second repository, holding a `good` row of its own whose test
        // passes, and no ref of atomic's at all.
        std::fs::create_dir_all(&easy).expect("easy dir");
        base_of(&easy);
        // The honest step names its runner the portable way, in the words an
        // atom file in a repository can hold. `unpinned` keeps an absolute name,
        // so that a runner whose bytes are not the pinned ones is refused however
        // it is spelled.
        let declared = relative_pins(
            &sha256_of(&runner),
            &sha256_of(&project.join("tools/controls-exec.py")),
        );
        for (name, command, env) in [
            (
                "controls",
                "tools/controls-exec.py".to_string(),
                declared.clone(),
            ),
            (
                "planted",
                format!("{}/tools/controls-exec.py", source.display()),
                declared.clone(),
            ),
            (
                "unpinned",
                "tools/controls-exec.py".to_string(),
                pins(
                    &runner.display().to_string(),
                    &"0".repeat(64),
                    &sha256_of(&project.join("tools/controls-exec.py")),
                ),
            ),
        ] {
            // Each atom's own contract sidecars: an atom's types and schemas are
            // named after it, so a second atom of the same shape carries the
            // same pair under its own name.
            for (role, text) in [("input", INPUT_SCHEMA), ("output", OUTPUT_SCHEMA)] {
                std::fs::write(
                    project.join(format!("atoms/{name}.{role}.schema.yaml")),
                    text,
                )
                .expect("schema");
            }
            std::fs::write(
                project.join(format!("atoms/{name}.atom.astro")),
                atom_source(
                    name,
                    &command,
                    &env,
                    &Probe {
                        bin: &dir.join("bin"),
                        project: &project,
                        source: &source,
                        secret: &private.join("secret"),
                    },
                    address,
                ),
            )
            .expect("atom");
        }
        Self {
            dir,
            project,
            source,
            worktrees,
            base,
            candidate,
            tree,
            easy,
            state,
            private,
        }
    }

    /// One coding step's attempt, the way a code-changing step makes it: a
    /// worktree prepared from `base`, `change` writing in it, and the commit
    /// recorded under a ref of its own. Nothing here is what a model said; it
    /// is what atomic recorded.
    fn code(
        worktrees: &Worktrees,
        base: &str,
        step: &str,
        message: &str,
        change: &dyn Fn(&Path),
    ) -> Result<StepCommit, String> {
        let worktree = worktrees
            .prepare("controls-run", step, base)
            .map_err(|e| format!("prepare {step}: {e}"))?;
        change(&worktree.path);
        worktrees
            .commit(&worktree, message, &Signature::default())
            .map_err(|e| format!("commit {step}: {e}"))
    }

    /// A second candidate for the same run: the same base, a different change.
    /// Here, the change that replaces the check script its own tree carries.
    fn forged(&self) -> StepCommit {
        Self::code(
            &self.worktrees,
            &self.base,
            "spoof",
            "Replace the check script",
            &|tree| {
                let script = tree.join("scripts/controls-check.sh");
                std::fs::write(&script, forged_check()).expect("forged script");
                std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                    .expect("forged script executable");
            },
        )
        .expect("the spoofing step's candidate")
    }

    /// What the step is asked to prove: the coding step's commit, as atomic
    /// recorded it, and these rows of it.
    fn input(&self, rows: &[&str]) -> Value {
        self.bound_by(&self.candidate, rows)
    }

    /// The same question asked of another step's record, which is how a case
    /// shows that the record, and not the input's own say-so, decides.
    fn bound_by(&self, recorded: &StepCommit, rows: &[&str]) -> Value {
        json!({
            "source": self.source.display().to_string(),
            "commit": recorded.commit,
            "bound": {"reference": recorded.reference, "base": recorded.base, "admitted": self.candidate.base},
            "rows": rows,
            // The rows switch files of their own, which is what lets them
            // share a step; each still has its own time to answer in.
            "row_seconds": 90,
        })
    }
}

/// Run one of the step's atoms as a durable exec step's attempt runs it,
/// keeping the metadata with the output: what it says about the model is what
/// the step did.
async fn step(
    fixture: &Fixture,
    atom: &str,
    backend: &Arc<dyn ExecutionBackend>,
    input: &Value,
) -> Result<(Value, RunMetadata), AtomError> {
    let mut registry = Registry::new();
    let atom = atomic_core::atom::load(
        &fixture
            .project
            .join("atoms")
            .join(format!("{atom}.atom.astro")),
        &mut registry,
    )
    .expect("loads");
    let compiled = atomic_core::compile::compile(&atom, &registry).expect("compiles");
    let done = runtime::run_exec(
        &atom,
        &compiled,
        input,
        &EffectIdentity {
            run_id: "controls-1".into(),
            step_id: "prove".into(),
        },
        backend.clone(),
        None,
        &runtime::Artifacts::none(),
        &atomic_core::usage::Metering::none(),
    )
    .await;
    if let Ok((_, meta)) = &done {
        // An exec step has no model to call: it is no turn, and nothing here
        // could have been billed for one.
        assert_eq!(meta.model, "exec", "{input}");
        assert_eq!(meta.turns, 0, "{input}");
        assert_eq!(meta.total_tokens, 0, "{input}");
    }
    done
}

/// The honest step's answer, or the test's failure naming it.
async fn proved(fixture: &Fixture, backend: &Arc<dyn ExecutionBackend>, rows: &[&str]) -> Value {
    let input = fixture.input(rows);
    step(fixture, "controls", backend, &input)
        .await
        .unwrap_or_else(|e| panic!("the step failed: {e}\ninput: {input}"))
        .0
}

/// The honest step's refusal: the step fails, and what its command said is
/// returned for the case to read.
async fn refused(fixture: &Fixture, backend: &Arc<dyn ExecutionBackend>, input: &Value) -> String {
    match step(fixture, "controls", backend, input).await {
        Ok((out, _)) => panic!("what should be refused passed the step: {out}\ninput: {input}"),
        Err(e) => e.to_string(),
    }
}

/// Copy the source visible to this test, including edits staged for this
/// release, but only files tracked by git plus the new row under test. The
/// source tree's worktree and target directory never become the candidate.
fn real_source_at(where_: &Path) {
    const COPY: &str = r#"
import os, pathlib, shutil, subprocess, sys
source, dest = map(pathlib.Path, sys.argv[1:])
names = subprocess.run(['git', 'ls-files', '-z'], cwd=source, check=True, stdout=subprocess.PIPE).stdout
for raw in filter(None, names.split(b'\0')):
    name = os.fsdecode(raw)
    src, out = source / name, dest / name
    out.parent.mkdir(parents=True, exist_ok=True)
    if src.is_symlink():
        out.symlink_to(os.readlink(src))
    elif src.is_file():
        shutil.copy2(src, out)
row = pathlib.Path('scripts/controls/context-contract-carries-declaration.row')
out = dest / row
out.parent.mkdir(parents=True, exist_ok=True)
shutil.copy2(source / row, out)
"#;
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root");
    let result = std::process::Command::new("python3")
        .args(["-c", COPY])
        .arg(source)
        .arg(where_)
        .output()
        .expect("copy tracked source");
    assert!(
        result.status.success(),
        "copy tracked source: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    git(where_, &["init", "-q", "--initial-branch=main"]);
    git(where_, &["add", "-A"]);
    git(
        where_,
        &[
            "-c",
            "user.name=candidate",
            "-c",
            "user.email=candidate@localhost",
            "-c",
            "commit.gpgSign=false",
            "commit",
            "-qm",
            "base",
        ],
    );
}

/// The dependency objects are supplied by the pinned image as read-only
/// inputs. Cargo's home, lock files and compiled target belong only to this
/// attempt, under `caches`.
fn real_cargo_caches(fixture: &Fixture) -> Caches {
    let caches = Caches::new(fixture.dir.join("real-row-cache")).for_exec("prove");
    caches.create().expect("fresh run cache");
    let home = caches.dir().join("cargo-home");
    std::fs::create_dir(&home).expect("cargo home");
    let trusted = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").expect("operator home")).join(".cargo")
        });
    let trusted = trusted.canonicalize().expect("operator Cargo home");
    assert!(
        trusted.join("git").is_dir(),
        "trusted offline Cargo git cache"
    );
    std::os::unix::fs::symlink(trusted.join("git"), home.join("git"))
        .expect("read-only Git cache link");
    let registry = home.join("registry");
    std::fs::create_dir(&registry).expect("run-local registry view");
    for name in ["index", "cache", "src"] {
        let source = trusted.join("registry").join(name);
        assert!(source.is_dir(), "trusted offline Cargo registry {name}");
        std::os::unix::fs::symlink(source, registry.join(name)).expect("read-only registry link");
    }
    caches
}

fn operator_toolchain_bin() -> PathBuf {
    let output = std::process::Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .expect("operator rustc");
    assert!(output.status.success(), "operator Rust sysroot");
    let sysroot = String::from_utf8(output.stdout).expect("UTF-8 Rust sysroot");
    let bin = PathBuf::from(sysroot.trim()).join("bin");
    for executable in ["cargo", "rustc"] {
        assert!(bin.join(executable).is_file(), "operator {executable}");
    }
    bin.canonicalize().expect("operator toolchain bin")
}

async fn real_row_step(
    fixture: &Fixture,
    backend: &Arc<dyn ExecutionBackend>,
    caches: &Caches,
    input: &Value,
) -> Result<(Value, RunMetadata), AtomError> {
    let mut registry = Registry::new();
    let atom = atomic_core::atom::load(
        &fixture.project.join("atoms/controls.atom.astro"),
        &mut registry,
    )
    .expect("loads real control atom");
    let compiled = atomic_core::compile::compile(&atom, &registry).expect("compiles");
    let identity = EffectIdentity {
        run_id: "controls-real-row".into(),
        step_id: "prove".into(),
    };
    let mut input = input.clone();
    input["row_seconds"] = json!(240);
    let workspace =
        runtime::open_exec_workspace(&atom, &identity, backend.clone(), Some(caches)).await?;
    runtime::run_exec_opened(
        &atom,
        &compiled,
        &input,
        &identity,
        workspace,
        None,
        &runtime::Artifacts::none(),
        &atomic_core::usage::Metering::none(),
    )
    .await
}

#[test]
fn a_real_repository_row_proves_with_cold_target_and_a_blind_variant_fails() {
    support::require_sandbox();
    let fixture = {
        let mut fixture = Fixture::new("");
        std::fs::remove_dir_all(&fixture.source).expect("remove stand-in candidate");
        std::fs::create_dir(&fixture.source).expect("real candidate source");
        real_source_at(&fixture.source);
        fixture.base = git(&fixture.source, &["rev-parse", "HEAD"])
            .trim()
            .to_owned();
        fixture.worktrees = Worktrees::new(
            Git::find()
                .expect("git")
                .discover(&fixture.source)
                .expect("repo"),
            fixture.state.join("real-worktrees"),
        );
        fixture.candidate = Fixture::code(
            &fixture.worktrees,
            &fixture.base,
            "real",
            "A clean candidate for the real control row",
            &|tree| {
                std::fs::write(tree.join("real-control-candidate.txt"), "candidate\n")
                    .expect("candidate change");
            },
        )
        .expect("real candidate commit");
        fixture.tree = git(
            &fixture.source,
            &[
                "rev-parse",
                &format!("{}^{{tree}}", fixture.candidate.commit),
            ],
        )
        .trim()
        .to_owned();
        let trusted_bin = operator_toolchain_bin();
        let home = fixture.dir.join("real-row-cache/cargo-home");
        let declared = format!(
            "{}         \x20\x20 CARGO_HOME: '{}',\n         \x20\x20 CARGO_NET_OFFLINE: 'true',\n",
            relative_pins(
                &sha256_of(&fixture.dir.join("harness/controls-check.sh")),
                &sha256_of(&fixture.project.join("tools/controls-exec.py")),
            ),
            home.display(),
        );
        std::fs::write(
            fixture.project.join("atoms/controls.atom.astro"),
            atom_source(
                "controls",
                "tools/controls-exec.py",
                &declared,
                &Probe {
                    bin: &trusted_bin,
                    project: &fixture.project,
                    source: &fixture.source,
                    secret: &fixture.private.join("secret"),
                },
                "",
            ),
        )
        .expect("real control atom");
        fixture
    };
    let caches = real_cargo_caches(&fixture);
    let target = PathBuf::from(&caches.env()["CARGO_TARGET_DIR"]);
    assert!(!target.exists(), "real row begins with a cold target cache");
    eprintln!(
        "real controls row cache provenance: target={}, home={}, trusted={}, toolchain={}, network=off",
        target.display(),
        caches.dir().join("cargo-home").display(),
        std::env::var("CARGO_HOME").unwrap_or_else(|_| "$HOME/.cargo".into()),
        operator_toolchain_bin().display(),
    );
    on_codex_runtime(async move {
        let scratch = fixture.dir.join("real-scratch");
        std::fs::create_dir_all(&scratch).expect("scratch");
        std::os::unix::fs::symlink(
            env!("CARGO_BIN_EXE_atomic"),
            fixture.dir.join("bin/codex-linux-sandbox"),
        )
        .expect("sandbox helper");
        let paths = CodexPaths::with_helpers(
            PathBuf::from(env!("CARGO_BIN_EXE_atomic")),
            fixture.dir.join("bin/codex-linux-sandbox"),
        );
        let sandbox = Arc::new(
            ExecSandbox::new(
                &paths,
                StateGuard::deny_state_dir(&fixture.state).expect("guard"),
                fixture.dir.join("real-codex"),
            )
            .expect("confinement"),
        );
        let backend: Arc<dyn ExecutionBackend> = Arc::new(
            LocalBackend::in_dir(scratch.clone())
                .denying([fixture.private.clone()])
                .confined(sandbox),
        );
        let started = std::time::Instant::now();
        let (passed, meta) = real_row_step(
            &fixture,
            &backend,
            &caches,
            &fixture.input(&["context-contract-carries-declaration"]),
        )
        .await
        .expect("real row off/fail/on/pass");
        eprintln!(
            "real controls row wall_ms={}",
            started.elapsed().as_millis()
        );
        assert_eq!(meta.model, "exec");
        assert_eq!(meta.turns, 0);
        assert_eq!(meta.total_tokens, 0);
        assert_eq!(passed["rows"][0]["verdict"], "passed", "{passed}");
        assert_eq!(passed["passed"], 1, "{passed}");
        assert_eq!(passed["commit"], fixture.candidate.commit, "{passed}");
        assert_eq!(passed["tree"], fixture.tree, "{passed}");
        assert!(
            target.is_dir(),
            "Cargo compiled into this run's target cache"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(300));
        assert!(
            left_copies(&scratch).is_empty(),
            "runner left scratch copies"
        );
        let blind = Fixture::code(
            &fixture.worktrees,
            &fixture.base,
            "blind-real",
            "A row whose named test ignores its switch",
            &|tree| {
                let row = tree.join("scripts/controls/context-contract-carries-declaration.row");
                let original = std::fs::read_to_string(&row).expect("real row");
                let changed = original.replace(
                    "typescript::tests::an_atom_with_no_context_declares_no_context_type typescript::tests::atom_declarations_match_the_authoring_projection",
                    "typescript::tests::output_is_deterministic",
                );
                assert_ne!(original, changed, "row test was replaced");
                std::fs::write(row, changed).expect("blind row");
            },
        ).expect("blind candidate");
        let blind_started = std::time::Instant::now();
        let error = real_row_step(
            &fixture,
            &backend,
            &caches,
            &fixture.bound_by(&blind, &["context-contract-carries-declaration"]),
        )
        .await
        .expect_err("blind real row must fail")
        .to_string();
        eprintln!(
            "blind real controls row wall_ms={}",
            blind_started.elapsed().as_millis()
        );
        assert!(
            error.contains("its test passed with the control off"),
            "{error}"
        );
        assert!(
            left_copies(&scratch).is_empty(),
            "blind row left scratch copies"
        );
    });
}

#[test]
fn a_control_row_proves_itself_in_the_sandbox_and_a_blind_row_fails_the_step() {
    support::require_sandbox();
    let (listener, address) = loopback_listener();
    let fixture = Fixture::new(&address);
    on_codex_runtime(async move {
        let scratch = fixture.dir.join("scratch");
        std::fs::create_dir_all(&scratch).expect("scratch");
        std::os::unix::fs::symlink(
            env!("CARGO_BIN_EXE_atomic"),
            fixture.dir.join("bin/codex-linux-sandbox"),
        )
        .expect("sandbox helper");
        let paths = CodexPaths::with_helpers(
            PathBuf::from(env!("CARGO_BIN_EXE_atomic")),
            fixture.dir.join("bin/codex-linux-sandbox"),
        );
        let sandbox = Arc::new(
            ExecSandbox::new(
                &paths,
                StateGuard::deny_state_dir(&fixture.state).expect("guard"),
                fixture.dir.join("codex"),
            )
            .expect("confinement"),
        );
        let backend: Arc<dyn ExecutionBackend> = Arc::new(
            LocalBackend::in_dir(scratch.clone())
                .denying([fixture.dir.join("scratch-debug"), fixture.private.clone()])
                .confined(sandbox),
        );
        // Two rows in one step. `good` is an ordinary control: its test fails
        // with it off and passes with it back. `sandbox` is this sandbox: its
        // test fails, and says what it reached, if the row's command reaches
        // out of the sandbox at all.
        let proven = proved(&fixture, &backend, &["good", "sandbox"]).await;
        assert_eq!(
            proven["rows"],
            json!([
                {"detail": PROVED, "name": "good", "verdict": "passed"},
                {"detail": PROVED, "name": "sandbox", "verdict": "passed"},
            ]),
            "the rows' results: {proven}"
        );
        assert_eq!(proven["ok"], true, "{proven}");
        assert_eq!(proven["error"], Value::Null, "{proven}");
        assert_eq!(proven["passed"], 2, "{proven}");
        assert_eq!(proven["failed"], 0, "{proven}");
        // What was proved is the commit the coding step made, as atomic
        // recorded it, and what was switched off was that commit's tree.
        assert_eq!(proven["commit"], fixture.candidate.commit, "{proven}");
        assert_eq!(
            proven["source"],
            fixture.source.display().to_string(),
            "{proven}"
        );
        assert_eq!(proven["tree"], fixture.tree, "{proven}");
        assert_eq!(
            proven["bound"],
            json!({
                "reference": fixture.candidate.reference,
                "base": fixture.candidate.base,
                "admitted": fixture.candidate.base,
            }),
            "the answer does not name what the step was bound to: {proven}"
        );
        assert_eq!(
            fixture.candidate.reference, "refs/atomic/controls-run/steps/code/1",
            "the ref atomic keeps a step's attempt under"
        );
        // The copy, and every control switched off in it, are gone.
        assert_eq!(
            left_copies(&scratch),
            Vec::<PathBuf>::new(),
            "a copy is left under the scratch root"
        );
        assert!(
            !fixture.source.join("sandbox-probe").exists(),
            "the row's command wrote into the change it was asked about"
        );
        assert!(
            !fixture.project.join("sandbox-probe").exists(),
            "the row's command wrote into the project"
        );
        // And the candidate's tree is exactly as committed: the step read it
        // and wrote nothing in it.
        let candidate_tree = format!("{}^{{tree}}", fixture.candidate.commit);
        assert_eq!(
            git(&fixture.source, &["rev-parse", candidate_tree.as_str()],).trim(),
            fixture.tree,
            "the step changed the candidate"
        );

        // The same step again, as a step that runs at least once does: the
        // same answer, since a second attempt starts from the same commit and
        // makes a copy of its own.
        let again = proved(&fixture, &backend, &["good", "sandbox"]).await;
        assert_eq!(again, proven, "a second attempt answered differently");

        // A row whose test still passes with its control off fails the step,
        // names itself, and says so where atomic keeps the end of the
        // command's stderr. This is the thing the step exists to catch.
        let input = fixture.input(&["blind"]);
        let blind = step(&fixture, "controls", &backend, &input).await;
        let error = match blind {
            Ok(out) => panic!("a blind row passed the step: {}", out.0),
            Err(e) => e.to_string(),
        };
        for needle in ["exited with code 1", "blind", "1 blind"] {
            assert!(
                error.contains(needle),
                "the failure says {error:?}, which does not contain {needle:?}"
            );
        }

        // A row the candidate does not carry is refused before anything runs,
        // and the step fails: a change cannot declare a control its own tree
        // does not hold.
        let missing = step(&fixture, "controls", &backend, &fixture.input(&["nosuch"]))
            .await
            .expect_err("a row the candidate does not hold must fail the step");
        let error = missing.to_string();
        for needle in ["exited with code 2", "refused", "nosuch"] {
            assert!(
                error.contains(needle),
                "the refusal says {error:?}, which does not contain {needle:?}"
            );
        }

        // The source dirty, with the good control already switched off and a
        // file no commit holds. The step is asked about the commit, so it
        // copies the commit: the proof runner finds its file exactly as
        // committed, and puts back exactly what it found. The uncommitted state
        // is no input, and nothing of it is lost.
        let switched = "let good_on = false;\n";
        let good = fixture.source.join("src/good.rs");
        let before = std::fs::read_to_string(&good).expect("good");
        std::fs::write(&good, before.replace("let good_on = true;", switched))
            .expect("switch the source");
        std::fs::write(fixture.source.join("stray"), "untracked").expect("stray file");
        let dirty = proved(&fixture, &backend, &["good"]).await;
        assert_eq!(dirty["rows"][0]["verdict"], "passed", "{dirty}");
        assert_eq!(dirty["tree"], fixture.tree, "{dirty}");
        assert_eq!(
            std::fs::read_to_string(&good).expect("good after"),
            before.replace("let good_on = true;", switched),
            "the step put back something other than what it found"
        );
        assert!(
            fixture.source.join("stray").exists(),
            "the step removed a file the candidate's commit does not hold"
        );
        assert_eq!(
            left_copies(&scratch),
            Vec::<PathBuf>::new(),
            "a copy is left under the scratch root"
        );

        // ------------------------------------------------ what it is bound to
        //
        // The step proves the commit the coding step made. Asked about anything
        // else it refuses, because a proof about another commit is no proof
        // about the change that needs reviewers.

        // A repository of the change's own choosing, holding a `good` row whose
        // test passes, with a fine commit that nothing of atomic's names. This
        // is the easier proof a step could fall through to if the `source` it
        // was handed were believed.
        let elsewhere = json!({
            "source": fixture.easy.display().to_string(),
            "commit": git(&fixture.easy, &["rev-parse", "HEAD"]).trim(),
            "bound": {
                "reference": fixture.candidate.reference,
                "base": fixture.candidate.base,
                "admitted": fixture.candidate.base,
            },
            "rows": ["good"],
        });
        let error = refused(&fixture, &backend, &elsewhere).await;
        let easy = fixture.easy.display().to_string();
        for needle in ["exited with code 2", "names no commit in", easy.as_str()] {
            assert!(
                error.contains(needle),
                "the refusal says {error:?}, which does not contain {needle:?}"
            );
        }

        // A second attempt, in the same repository, whose whole change is a
        // forged check script. It is a real commit that a real step's attempt
        // made, so it is not refused for that; what is refused is asking the
        // coding step's ref about it.
        let spoofed = fixture.forged();
        assert_eq!(
            spoofed.reference, "refs/atomic/controls-run/steps/spoof/1",
            "the ref the spoofing attempt is kept under"
        );
        let swapped = json!({
            "source": fixture.source.display().to_string(),
            "commit": spoofed.commit,
            "bound": {
                "reference": fixture.candidate.reference,
                "base": fixture.candidate.base,
                "admitted": fixture.candidate.base,
            },
            "rows": ["blind"],
        });
        let error = refused(&fixture, &backend, &swapped).await;
        for needle in [
            "exited with code 2",
            "names ",
            "this step was asked to prove",
            "not the change that step made",
        ] {
            assert!(
                error.contains(needle),
                "the refusal says {error:?}, which does not contain {needle:?}"
            );
        }

        // The spoofed candidate proved with its own record: the change holds a
        // check script there that prints the passing line for every row and
        // switches nothing, which is exactly what a change wanting a free pass
        // would hold. The step runs the runner its declaration named, so the
        // blind row is still blind and the step still fails, and the honest row
        // is still proved.
        let forged_rows = fixture.bound_by(&spoofed, &["blind"]);
        let blind = step(&fixture, "controls", &backend, &forged_rows).await;
        let error = match blind {
            Ok(out) => panic!("a forged check script passed a blind row: {}", out.0),
            Err(e) => e.to_string(),
        };
        for needle in [
            "exited with code 1",
            "blind: its test passed with the control off",
            "1 blind",
        ] {
            assert!(
                error.contains(needle),
                "the failure says {error:?}, which does not contain {needle:?}"
            );
        }
        let honest = step(
            &fixture,
            "controls",
            &backend,
            &fixture.bound_by(&spoofed, &["good"]),
        )
        .await
        .unwrap_or_else(|e| panic!("the honest row of the spoofed candidate: {e}"))
        .0;
        assert_eq!(honest["rows"][0]["verdict"], "passed", "{honest}");
        assert_eq!(honest["passed"], 1, "{honest}");
        // And the forgery is really in that candidate, so the case above tested
        // the forgery rather than a tree that happened to behave.
        let in_candidate = format!("{}:scripts/controls-check.sh", spoofed.commit);
        assert!(
            git(&fixture.source, &["show", in_candidate.as_str()]).contains("every control passes"),
            "the spoofed candidate does not hold the forged script"
        );

        // ------------------------------------------------------------ trust
        //
        // A driver the change carries answers for itself or not at all: the one
        // that runs is checked against the bytes the step was declared with
        // before it reads anything, so planted bytes cannot report a pass.
        let planted = step(&fixture, "planted", &backend, &fixture.input(&["good"]))
            .await
            .expect_err("a driver that is not the one declared must fail the step");
        let error = planted.to_string();
        for needle in [
            "exited with code 2",
            "not the driver this step was declared with",
        ] {
            assert!(
                error.contains(needle),
                "the refusal says {error:?}, which does not contain {needle:?}"
            );
        }
        // The declaration that honest step just ran with is a portable one: the
        // runner is named by a path of its own, beside the driver, with nothing in
        // it that holds only for the directory this checkout happens to sit in.
        // Everything above -- the proved rows, the planted driver, the forgery
        // below -- ran through that relative name, resolved against the driver's
        // own pinned entry point; this fixture makes a fresh temporary directory
        // for every run, so a declaration that had written down where the tree
        // was when it was written could not have named this runner at all.
        let atom = std::fs::read_to_string(fixture.project.join("atoms/controls.atom.astro"))
            .expect("atom");
        let declared = atom
            .lines()
            .find(|line| line.contains("CONTROL_RUNNER:"))
            .expect("the step's atom declares its proof runner")
            .to_owned();
        assert!(
            declared.contains("../../harness/controls-check.sh"),
            "the step does not name its runner beside its own driver: {declared}"
        );
        for where_ in ["/tmp/", "/home/", "'/"] {
            assert!(
                !declared.contains(where_),
                "the step declares the runner by an absolute path ({where_}), which no \
                 checkout can carry: {declared}"
            );
        }
        // A proof runner whose bytes are not what the declaration pins is
        // refused the same way, naming both hashes so a stale pin reads as one.
        let wrong = step(&fixture, "unpinned", &backend, &fixture.input(&["good"]))
            .await
            .expect_err("a runner that is not the one declared must fail the step");
        let error = wrong.to_string();
        for needle in [
            "exited with code 2",
            "whose bytes hash to",
            "CONTROL_RUNNER_SHA256 pins",
            "not the runner this step was declared with",
        ] {
            assert!(
                error.contains(needle),
                "the refusal says {error:?}, which does not contain {needle:?}"
            );
        }

        // ------------------------------------------------------------ its time
        //
        // The step's own wall time is its own: rows that cannot all fit it,
        // beside the setup reserve, are refused where the reason can still be
        // said, because a row that never runs is a control that was never
        // proved.
        let too_many = json!({
            "source": fixture.source.display().to_string(),
            "commit": fixture.candidate.commit,
            "bound": {
                "reference": fixture.candidate.reference,
                "base": fixture.candidate.base,
                "admitted": fixture.candidate.base,
            },
            "rows": ["good", "sandbox"],
            "row_seconds": 240,
        });
        let error = refused(&fixture, &backend, &too_many).await;
        for needle in ["exited with code 2", "do not fit the step"] {
            assert!(
                error.contains(needle),
                "the refusal says {error:?}, which does not contain {needle:?}"
            );
        }

        drop(listener);
        let _ = std::fs::remove_dir_all(&fixture.dir);
    });
}
