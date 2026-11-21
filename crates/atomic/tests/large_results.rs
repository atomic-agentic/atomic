// SPDX-License-Identifier: Apache-2.0

//! Large step results go by reference, through the `atomic` binary, with
//! exec atoms' commands confined as they always are.
//!
//! One exec atom writes a 200 MB patch to its outputs directory
//! (`ATOMIC_OUTPUTS`, inside its scratch directory, the one place it may
//! write) and names it in its output; the next reads it in its inputs
//! directory (`ATOMIC_INPUTS`), which it may only read. Between them:
//!
//! - the patch reaches the second atom intact, as an artifact reference;
//! - the step outputs, and so the run's checkpoints, hold the reference and
//!   never the bytes: the durable store stays small, and the patch is in the
//!   artifact store once;
//! - the second atom cannot change what it is given, nor through it the
//!   store;
//! - a run killed after the first step's checkpoint resumes without making
//!   the patch again;
//! - a run killed after the patch was stored but before the first step's
//!   checkpoint resumes as its effects say: a step that declares none is
//!   parked as `outcome_unknown`, and one that declares itself idempotent
//!   runs again, storing a second patch beside the first.
//!
//! The patches are made by the atoms' commands and checked by the next one,
//! both streaming, so the test never holds them in memory. Each patch carries
//! a fresh nonce, so a step that ran twice stores two. The tests run one at
//! a time and remove their files, so at most one 200 MB patch is on disk.
//!
//! Exec atoms' commands run confined, so these tests need Codex's Linux
//! sandbox and run with the `sandbox-tests` feature; with it on, a sandbox
//! that cannot start fails them.

#![cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#![allow(clippy::unwrap_used)]

mod support;

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;

const ATOMIC: &str = env!("CARGO_BIN_EXE_atomic");
/// The big patch's size.
const BIG: u64 = 200 * 1024 * 1024;
/// A patch for the tests about killing a run.
const SMALL: u64 = 1024 * 1024;

/// One test at a time, so their patches are never on disk together.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        // The inputs directories are read-only.
        let _ = Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(&self.0)
            .status();
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch(tag: &str) -> Scratch {
    support::require_sandbox();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-large-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in [
        "home",
        "project/atoms",
        "project/workflows",
        "project/tools",
    ] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    Scratch(dir)
}

fn object(properties: Value) -> Value {
    let required: Vec<String> = properties.as_object().unwrap().keys().cloned().collect();
    json!({"type": "object", "additionalProperties": false, "required": required, "properties": properties})
}

fn write_atom(project: &Path, name: &str, config: &str, input: Value, output: Value) {
    let atoms = project.join("atoms");
    std::fs::write(
        atoms.join(format!("{name}.atom.astro")),
        format!(
            "---\nimport type {{ Props, Input, Output }} from '../.atomic/types/{name}';\n\
             import type {{ AtomConfig }} from '../.atomic/types/authoring';\n\
             const {{ input }}: Props = Astro.props;\n\
             export const config: AtomConfig<Input, Output> = {{ {config} }};\n\
             ---\n"
        ),
    )
    .unwrap();
    for (role, schema) in [("input", input), ("output", output)] {
        std::fs::write(
            atoms.join(format!("{name}.{role}.schema.json")),
            schema.to_string(),
        )
        .unwrap();
    }
}

/// Two exec atoms: one writes a patch of `size` bytes, starting with a fresh
/// nonce, and names it in its output; the other tries to change the patch
/// it is given, then reads it and reports its size and digest. `effects` is
/// the first atom's effects declaration, if any.
fn project(dir: &Path, size: u64, effects: &str) -> PathBuf {
    let project = dir.join("project");
    std::fs::write(project.join("atomic.yaml"), "atomic: 1\nname: patches\n").unwrap();
    std::fs::write(
        project.join("tools/make.py"),
        format!(
            "import json, os, sys\n\
             json.load(sys.stdin)\n\
             line = b'+ a line of the synthetic patch, the same each time\\n'\n\
             chunk = line * ((1 << 20) // len(line) + 1)\n\
             nonce = os.urandom(16).hex().encode() + b'\\n'\n\
             left = {size} - len(nonce)\n\
             with open(os.path.join(os.environ['ATOMIC_OUTPUTS'], 'change.diff'), 'wb') as f:\n\
             \x20   f.write(nonce)\n\
             \x20   while left:\n\
             \x20       n = min(left, len(chunk))\n\
             \x20       f.write(chunk[:n])\n\
             \x20       left -= n\n\
             json.dump({{'patch': {{'$file': 'change.diff', 'mediaType': 'text/x-diff'}}}}, sys.stdout)\n"
        ),
    )
    .unwrap();
    std::fs::write(
        project.join("tools/check.py"),
        "import hashlib, json, os, sys\n\
         patch = json.load(sys.stdin)['patch']\n\
         path = os.path.join(os.environ['ATOMIC_INPUTS'], patch.removeprefix('artifact://sha256/'))\n\
         changed = []\n\
         for attempt in ('append', 'replace', 'chmod'):\n\
         \x20   try:\n\
         \x20       if attempt == 'append': open(path, 'ab').write(b'x')\n\
         \x20       if attempt == 'replace': open(path + '.new', 'wb').write(b'x'); os.replace(path + '.new', path)\n\
         \x20       if attempt == 'chmod': os.chmod(path, 0o644); open(path, 'ab').write(b'x')\n\
         \x20       changed.append(attempt)\n\
         \x20   except OSError:\n\
         \x20       pass\n\
         h, size = hashlib.sha256(), 0\n\
         with open(path, 'rb') as f:\n\
         \x20   while block := f.read(1 << 20):\n\
         \x20       h.update(block)\n\
         \x20       size += len(block)\n\
         json.dump({'size': size, 'sha256': h.hexdigest(), 'changed': changed}, sys.stdout)\n",
    )
    .unwrap();
    let patch = json!({"$ref": "atomic:artifact", "mediaType": "text/x-diff"});
    let effects = if effects.is_empty() {
        String::new()
    } else {
        format!(", effects: {{ external: '{effects}' }}")
    };
    write_atom(
        &project,
        "make-patch",
        &format!(
            "description: 'Make a patch.', kind: 'exec', exec: {{ command: ['python3', 'tools/make.py'] }}{effects}"
        ),
        object(json!({"title": {"type": "string"}})),
        object(json!({"patch": patch})),
    );
    write_atom(
        &project,
        "check-patch",
        "description: 'Check a patch.', kind: 'exec', exec: { command: ['python3', 'tools/check.py'] }",
        object(json!({"patch": patch})),
        object(json!({
            "size": {"type": "integer"},
            "sha256": {"type": "string"},
            "changed": {"type": "array", "items": {"type": "string"}}
        })),
    );
    std::fs::write(
        project.join("workflows/patch.workflow.yaml"),
        "atomic: 1\nname: patch\n\
         input: { $ref: ../atoms/make-patch.input.schema.json }\n\
         output: { $ref: ../atoms/check-patch.output.schema.json }\n\
         steps:\n\
         \x20 make:\n    atom: make-patch\n    with: { title: input.title }\n\
         \x20 check:\n    after: [make]\n    atom: check-patch\n    with: { patch: steps.make.output.patch }\n\
         returns: steps.check.output\n",
    )
    .unwrap();
    project
}

fn atomic(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(ATOMIC);
    cmd.args(args)
        .current_dir(dir.join("project"))
        .env("HOME", dir.join("home"))
        .env("ATOMIC_HOME", dir.join("atomic-home"))
        .env("USER", "dev")
        // These atoms call no model; resuming a run still asks for an
        // endpoint, which nothing here calls.
        .env("ATOMIC_MODEL_BASE_URL", "http://127.0.0.1:9/v1")
        .env("ATOMIC_MODEL", "unused")
        .env_remove("ATOMIC_DATABASE_URL")
        .env_remove("ATOMIC_FAILPOINT")
        .env_remove("ATOMIC_EXEC_MAX_FILE_BYTES")
        .env_remove("ATOMIC_EXEC_MAX_STEP_BYTES")
        .stdin(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// The total size of the files under `dir`.
fn size_of(dir: &Path) -> u64 {
    let mut total = 0;
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path).unwrap();
        total += if meta.is_dir() {
            size_of(&path)
        } else {
            meta.len()
        };
    }
    total
}

/// The sizes of the blobs in the artifact store at `store`.
fn blobs(store: &Path) -> Vec<u64> {
    let mut sizes = Vec::new();
    for prefix in std::fs::read_dir(store.join("blobs/sha256"))
        .into_iter()
        .flatten()
        .flatten()
    {
        for blob in std::fs::read_dir(prefix.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            sizes.push(blob.metadata().unwrap().len());
        }
    }
    sizes
}

/// The project's state directory.
fn state(dir: &Path) -> PathBuf {
    let out = atomic(dir, &["debug", "state-dir"], &[]);
    assert!(out.status.success(), "{}", text(&out));
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
}

/// What a finished run must show: a patch of `size` arrived whole, by
/// reference, unchanged by the command it was given to; `stored` patches of
/// that size are in the store; and nothing durable holds their bytes.
fn check_by_reference(dir: &Path, out: &Output, size: u64, stored: usize) {
    let result: Value =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", text(out)));
    let output = &result["output"];
    assert_eq!(output["size"], json!(size), "{result}");
    assert_eq!(
        output["changed"],
        json!([]),
        "the command changed its input: {result}"
    );
    let state = state(dir);
    let store = state.join("artifacts");
    let sizes = blobs(&store);
    assert_eq!(
        sizes.iter().filter(|s| **s == size).count(),
        stored,
        "patches stored: {sizes:?}"
    );
    // The reference the steps passed is the patch's own digest, and the
    // blob is what it was.
    let digest = output["sha256"].as_str().unwrap();
    let blob = store
        .join("blobs/sha256")
        .join(&digest[..2])
        .join(&digest[2..]);
    assert_eq!(
        std::fs::metadata(&blob).unwrap().len(),
        size,
        "{}",
        blob.display()
    );
    // Checkpoints, the ledger and the gate store are all in the durable
    // store, in the project's private directory beside the state
    // directory, which never held a patch.
    let private = state
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .join("private")
        .join(state.file_name().unwrap());
    assert!(
        private.join("atomic.db").is_file(),
        "no durable store at {}",
        private.display()
    );
    let durable = size_of(&private);
    assert!(
        durable > 0 && durable < 8 * 1024 * 1024,
        "the durable store holds {durable} bytes"
    );
    // Nothing is left in the commands' scratch space, the project's in the
    // home's `scratch/`.
    let id = state.file_name().expect("a project id");
    let home = state.parent().and_then(Path::parent).expect("the atomic home");
    let scratch = home.join("scratch").join(id);
    assert!(scratch.is_dir(), "no scratch at {}", scratch.display());
    let left = size_of(&scratch);
    assert!(left < 64 * 1024, "{left} bytes left in scratch");
}

#[test]
fn a_200_mb_patch_passes_between_atoms_by_reference() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = scratch("pass");
    let dir = &scratch.0;
    project(dir, BIG, "");
    let out = atomic(
        dir,
        &["--json", "run", "patch", "--data", r#"{"title": "big"}"#],
        &[],
    );
    assert!(out.status.success(), "{}", text(&out));
    check_by_reference(dir, &out, BIG, 1);
}

#[test]
fn a_patch_over_the_file_limit_is_refused() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = scratch("limit");
    let dir = &scratch.0;
    project(dir, SMALL, "");
    let limit = (SMALL - 1).to_string();
    let out = atomic(
        dir,
        &["run", "patch", "--data", r#"{"title": "big"}"#],
        &[("ATOMIC_EXEC_MAX_FILE_BYTES", &limit)],
    );
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("at most"), "{}", text(&out));
    assert!(
        blobs(&state(dir).join("artifacts"))
            .iter()
            .all(|s| *s != SMALL)
    );
}

#[test]
fn a_run_killed_after_the_patch_was_recorded_resumes_without_making_it() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = scratch("recorded");
    let dir = &scratch.0;
    project(dir, SMALL, "");
    let out = atomic(
        dir,
        &[
            "run",
            "patch",
            "--run-id",
            "patch-1",
            "--data",
            r#"{"title": "t"}"#,
        ],
        &[("ATOMIC_FAILPOINT", "pipeline:after:make")],
    );
    assert!(
        !out.status.success(),
        "killed at the failpoint: {}",
        text(&out)
    );
    let out = atomic(dir, &["--json", "runs", "resume", "patch-1"], &[]);
    assert!(out.status.success(), "{}", text(&out));
    // One patch: the finished step did not run again.
    check_by_reference(dir, &out, SMALL, 1);
}

/// Killed after the patch was stored, before the step's checkpoint: whether
/// the command's effects happened is unknown, so a step that declares none
/// is parked, and nothing retries it.
#[test]
fn a_run_killed_after_storing_the_patch_parks_a_step_with_no_effects_declared() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = scratch("stored");
    let dir = &scratch.0;
    project(dir, SMALL, "");
    let out = atomic(
        dir,
        &[
            "run",
            "patch",
            "--run-id",
            "patch-2",
            "--data",
            r#"{"title": "t"}"#,
        ],
        &[("ATOMIC_FAILPOINT", "pipeline:ran:make")],
    );
    assert!(
        !out.status.success(),
        "killed at the failpoint: {}",
        text(&out)
    );
    assert_eq!(
        blobs(&state(dir).join("artifacts"))
            .iter()
            .filter(|s| **s == SMALL)
            .count(),
        1,
        "the patch was stored before the kill"
    );
    let out = atomic(dir, &["--json", "runs", "resume", "patch-2"], &[]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("outcome_unknown"), "{}", text(&out));
}

/// The same kill, for a step that declares itself idempotent: it runs
/// again, and the patch it makes then is the one the run uses. The first
/// stays in the store, referenced by nothing, until retention removes it.
#[test]
fn a_run_killed_after_storing_the_patch_reruns_an_idempotent_step() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = scratch("rerun");
    let dir = &scratch.0;
    project(dir, SMALL, "idempotent");
    let out = atomic(
        dir,
        &[
            "run",
            "patch",
            "--run-id",
            "patch-3",
            "--data",
            r#"{"title": "t"}"#,
        ],
        &[("ATOMIC_FAILPOINT", "pipeline:ran:make")],
    );
    assert!(
        !out.status.success(),
        "killed at the failpoint: {}",
        text(&out)
    );
    let out = atomic(dir, &["--json", "runs", "resume", "patch-3"], &[]);
    assert!(out.status.success(), "{}", text(&out));
    check_by_reference(dir, &out, SMALL, 2);
}
