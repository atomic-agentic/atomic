// SPDX-License-Identifier: Apache-2.0

//! Large step results go by reference, through the `atomic` binary.
//!
//! One exec atom writes a 200 MB patch to the directory its command is given
//! for files and names it in its output; the next reads it through the
//! directory its command is given for its input's artifacts. Between them:
//!
//! - the patch reaches the second atom intact, as an artifact reference;
//! - the step outputs, and so the run's checkpoints, hold the reference and
//!   never the bytes: the durable store stays small, and the patch is in the
//!   artifact store once;
//! - a run killed after the first step and resumed does not make the patch
//!   again, nor copy it: the store still holds it once.
//!
//! The patch is made by the atom's command and checked by the next one, both
//! streaming, so the test itself never holds it in memory. Its files live
//! under the test's target directory and are removed afterwards.

#![allow(clippy::unwrap_used)]

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const ATOMIC: &str = env!("CARGO_BIN_EXE_atomic");
/// The patch's size.
const SIZE: u64 = 200 * 1024 * 1024;

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch(tag: &str) -> Scratch {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-large-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in [
        "home",
        "tmp",
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

/// Two exec atoms: one writes a 200 MB patch and names it in its output,
/// logging each run; the other reads the patch it is given and reports its
/// size and digest.
fn project(dir: &Path) -> PathBuf {
    let project = dir.join("project");
    std::fs::write(project.join("atomic.yaml"), "atomic: 1\nname: patches\n").unwrap();
    std::fs::write(
        project.join("tools/make.py"),
        format!(
            "import json, os, sys\n\
             json.load(sys.stdin)\n\
             open(os.environ['MAKE_LOG'], 'a').write('made\\n')\n\
             line = b'+ a line of the synthetic patch, the same each time\\n'\n\
             chunk = line * ((1 << 20) // len(line) + 1)\n\
             left = {SIZE}\n\
             with open(os.path.join(os.environ['ATOMIC_OUTPUTS'], 'change.diff'), 'wb') as f:\n\
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
         digest = patch.removeprefix('artifact://sha256/')\n\
         h, size = hashlib.sha256(), 0\n\
         with open(os.path.join(os.environ['ATOMIC_INPUTS'], digest), 'rb') as f:\n\
         \x20   while block := f.read(1 << 20):\n\
         \x20       h.update(block)\n\
         \x20       size += len(block)\n\
         json.dump({'size': size, 'sha256': h.hexdigest()}, sys.stdout)\n",
    )
    .unwrap();
    let patch = json!({"$ref": "atomic:artifact", "mediaType": "text/x-diff"});
    write_atom(
        &project,
        "make-patch",
        &format!(
            "description: 'Make a patch.', kind: 'exec', exec: {{ command: ['python3', 'tools/make.py'] }}, env: {{ MAKE_LOG: '{}' }}",
            dir.join("make.log").display()
        ),
        object(json!({"title": {"type": "string"}})),
        object(json!({"patch": patch})),
    );
    write_atom(
        &project,
        "check-patch",
        "description: 'Check a patch.', kind: 'exec', exec: { command: ['python3', 'tools/check.py'] }",
        object(json!({"patch": patch})),
        object(json!({"size": {"type": "integer"}, "sha256": {"type": "string"}})),
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
        // The exec atoms' scratch directories, and so their outputs, stay
        // under the test's target directory.
        .env("TMPDIR", dir.join("tmp"))
        .env("USER", "dev")
        // These atoms call no model; resuming a run still asks for an
        // endpoint, which nothing here calls.
        .env("ATOMIC_MODEL_BASE_URL", "http://127.0.0.1:9/v1")
        .env("ATOMIC_MODEL", "unused")
        .env_remove("ATOMIC_DATABASE_URL")
        .env_remove("ATOMIC_FAILPOINT")
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

/// The blobs under the artifact store at `store`, with their sizes.
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

/// What a finished run must show: the patch arrived whole, by reference, and
/// nothing durable holds its bytes.
fn check_by_reference(dir: &Path, out: &Output) {
    let result: Value =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", text(out)));
    let output = &result["output"];
    assert_eq!(output["size"], json!(SIZE), "{result}");
    let state = state(dir);
    let store = state.join("artifacts");
    let sizes = blobs(&store);
    assert_eq!(
        sizes.iter().filter(|s| **s == SIZE).count(),
        1,
        "the patch is stored once: {sizes:?}"
    );
    // The reference the steps passed is the patch's own digest.
    let digest = output["sha256"].as_str().unwrap();
    let blob = store
        .join("blobs/sha256")
        .join(&digest[..2])
        .join(&digest[2..]);
    assert_eq!(
        std::fs::metadata(&blob).unwrap().len(),
        SIZE,
        "{}",
        blob.display()
    );
    // Checkpoints, the ledger and the gate store are all in the durable
    // store, which never held the patch.
    let durable = size_of(&state.join("private"));
    assert!(
        durable < 8 * 1024 * 1024,
        "the durable store holds {durable} bytes"
    );
    // Nothing is left in the commands' scratch space.
    let left = size_of(&dir.join("tmp"));
    assert!(left < 1024 * 1024, "{left} bytes left in scratch");
}

#[test]
fn a_200_mb_patch_passes_between_atoms_by_reference() {
    let scratch = scratch("pass");
    let dir = &scratch.0;
    project(dir);
    let out = atomic(
        dir,
        &["--json", "run", "patch", "--data", r#"{"title": "big"}"#],
        &[],
    );
    assert!(out.status.success(), "{}", text(&out));
    check_by_reference(dir, &out);
    assert_eq!(
        std::fs::read_to_string(dir.join("make.log")).unwrap(),
        "made\n"
    );
}

#[test]
fn a_run_killed_after_the_patch_resumes_without_making_or_copying_it() {
    let scratch = scratch("resume");
    let dir = &scratch.0;
    project(dir);
    let out = atomic(
        dir,
        &[
            "run",
            "patch",
            "--run-id",
            "patch-1",
            "--data",
            r#"{"title": "big"}"#,
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
    check_by_reference(dir, &out);
    assert_eq!(
        std::fs::read_to_string(dir.join("make.log")).unwrap(),
        "made\n",
        "the finished step did not run again"
    );
}
