// SPDX-License-Identifier: Apache-2.0

//! An exec step's build caches are its run's own. `atomic run` runs a
//! workflow whose exec step, under Codex's Linux sandbox, writes into the
//! caches it is pointed at and tries to reach the directory that holds every
//! run's caches. It may write its own, finds them empty, cannot create
//! anything beside them, and they are gone when the run ends; the next run's
//! exec step gets caches of its own, and finds nothing the first wrote.
//!
//! Needs bubblewrap and unprivileged user namespaces, so it runs with the
//! `sandbox-tests` feature; with the feature on, a sandbox that cannot start
//! fails it.

#![cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#![allow(clippy::unwrap_used)]

mod support;

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// What the exec step's command does: list its caches, write into them,
/// and try to make a directory beside them, as another run's would be.
const PROBE: &str = r#"import json, os, sys
json.load(sys.stdin)
caches = os.environ.get('ATOMIC_CACHE_DIR', '')
listed = sorted(os.listdir(caches)) if caches and os.path.isdir(caches) else ['<none>']
def attempt(f):
    try:
        f()
        return 'ok'
    except Exception as e:
        return type(e).__name__
def plant():
    os.makedirs(os.path.join(caches, 'xdg'), exist_ok=True)
    with open(os.path.join(caches, 'planted'), 'w') as f:
        f.write('from an earlier run')
def beside():
    os.makedirs(os.path.join(os.path.dirname(caches), 'another-run'))
json.dump({'caches': caches, 'listed': ' '.join(listed), 'planted': attempt(plant),
           'beside': attempt(beside)}, sys.stdout)
"#;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A project whose `build` workflow runs one exec step, [`PROBE`].
fn project(at: &Path) -> PathBuf {
    let project = at.join("project");
    write(&project.join("atomic.yaml"), "atomic: 1\nname: caches\n");
    write(&project.join("tools/probe.py"), PROBE);
    write(
        &project.join("atoms/probe.atom.astro"),
        "---\nimport type { Props, Input, Output } from '../.atomic/types/probe';\n\
         import type { AtomConfig } from '../.atomic/types/authoring';\n\
         const { input }: Props = Astro.props;\n\
         export const config: AtomConfig<Input, Output> = { description: 'Probe the caches.', \
         kind: 'exec', exec: { command: ['python3', 'tools/probe.py'] } };\n---\n",
    );
    write(
        &project.join("atoms/probe.input.schema.json"),
        &json!({"type": "object", "additionalProperties": false, "required": ["text"],
                "properties": {"text": {"type": "string"}}})
        .to_string(),
    );
    let field = json!({"type": "string"});
    write(
        &project.join("atoms/probe.output.schema.json"),
        &json!({"type": "object", "additionalProperties": false,
                "required": ["caches", "listed", "planted", "beside"],
                "properties": {"caches": field, "listed": field, "planted": field,
                               "beside": field}})
        .to_string(),
    );
    write(
        &project.join("workflows/build.workflow.yaml"),
        "atomic: 1\nname: build\n\
         input: { $ref: ../atoms/probe.input.schema.json }\n\
         output: { $ref: ../atoms/probe.output.schema.json }\n\
         steps:\n  probe:\n    atom: probe\n    with: { text: input.text }\n\
         returns: steps.probe.output\n",
    );
    project
}

/// Run `build` as run `run` of the project at `dir`, and return its result.
fn probe(dir: &Path, run: &str) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_atomic"))
        .args(["--json", "run", "build", "--run-id", run])
        .args(["--data", r#"{"text": "build"}"#])
        .current_dir(dir.join("project"))
        .env("HOME", dir.join("home"))
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("XDG_CONFIG_HOME", dir.join("home/.config"))
        .env("ATOMIC_DETECT_OLLAMA_ADDR", "127.0.0.1:9")
        .env_remove("ATOMIC_HOME")
        .env_remove("CODEX_HOME")
        .env_remove("ATOMIC_DATABASE_URL")
        .env_remove("ATOMIC_EXECUTOR_ID")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    doc["output"].clone()
}

/// What a probe found in its caches, but the empty, read-only placeholders
/// Codex's sandbox keeps in every directory a command may write.
fn found(probe: &Value) -> String {
    probe["listed"]
        .as_str()
        .unwrap_or_default()
        .split(' ')
        .filter(|name| !["", ".agents", ".aws", ".codex", ".git"].contains(name))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Every file under `dir`.
fn files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files(&path));
        } else {
            found.push(path);
        }
    }
    found
}

#[test]
fn an_exec_steps_caches_are_its_runs_alone_and_go_with_it() {
    support::require_sandbox();
    // Not under /tmp, where atomic keeps no scratch.
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-exec-caches-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).unwrap();
    project(&dir);

    let first = probe(&dir, "first");
    let caches = PathBuf::from(first["caches"].as_str().unwrap());
    assert_eq!(caches.parent().and_then(Path::file_name).unwrap(), "cache");
    assert!(caches.starts_with(dir.join("data")), "{first}");
    // Its own, empty but for the sandbox's read-only placeholders, and
    // writable; nothing beside them.
    assert_eq!(found(&first), "", "{first}");
    assert_eq!(first["planted"], "ok", "{first}");
    assert_ne!(first["beside"], "ok", "{first}");
    // Gone with the run.
    assert!(!caches.exists(), "{caches:?} outlived its run");
    let all = caches.parent().unwrap();
    assert!(files(all).is_empty(), "left in {all:?}: {:?}", files(all));

    // The next run's are its own, and hold nothing the first wrote.
    let second = probe(&dir, "second");
    assert_ne!(second["caches"], first["caches"]);
    assert_eq!(found(&second), "", "{second}");
    assert_ne!(second["beside"], "ok", "{second}");
    let _ = std::fs::remove_dir_all(&dir);
}
