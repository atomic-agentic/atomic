// SPDX-License-Identifier: Apache-2.0

//! An exec atom's command cannot read another live attempt's scratch
//! directory, its `HOME` and `TMPDIR`, nor what another backend of the
//! project keeps in its scratch root, though it reads and writes its own:
//! the project's scratch roots are denied to a confined command, its own
//! directory apart, on the local backend and through a runner process.
//!
//! Needs bubblewrap and unprivileged user namespaces, so it runs with the
//! `sandbox-tests` feature; with the feature on, a sandbox that cannot start
//! fails the test.

#![cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#![recursion_limit = "512"]

mod support;

use atomic_core::codex::{CodexPaths, ExecSandbox, StateGuard};
use atomic_core::runtime::{self, EffectIdentity};
use atomic_core::testing::on_codex_runtime;
use atomic_exec::{ExecutionBackend, LocalBackend};
use atomic_schema::registry::Registry;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Leaves a secret in its `HOME` and `TMPDIR`, then holds its attempt open
/// until the test leaves the file `RELEASE` names, for up to 60 s.
const KEEPER: &str = r#"import json, os, sys, time
json.load(sys.stdin)
for var in ['HOME', 'TMPDIR']:
    with open(os.path.join(os.environ[var], 'secret-' + var), 'w') as f:
        f.write('secret-of-the-keeper')
for _ in range(600):
    if os.path.exists(os.environ['RELEASE']):
        break
    time.sleep(0.1)
json.dump({'kept': 'yes' if os.path.exists(os.environ['RELEASE']) else 'no'}, sys.stdout)
"#;

/// Looks for the keeper's secret everywhere under the backend's scratch
/// root, and for the one planted in another backend's, while the keeper
/// still holds its attempt open, and uses its own `HOME` and `TMPDIR`.
const PROBER: &str = r#"import json, os, sys
json.load(sys.stdin)
root = os.environ['SCRATCH_ROOT']
def count(under, secret):
    found = 0
    for top, dirs, files in os.walk(under):
        for name in files:
            try:
                with open(os.path.join(top, name)) as f:
                    found += secret in f.read()
            except Exception:
                pass
    return found
found = count(root, 'secret-of-the-keeper')
sibling_found = count(os.environ['SIBLING_ROOT'], 'secret-in-the-sibling')
try:
    listed = str(sorted(os.listdir(root)))
except Exception as e:
    listed = type(e).__name__
own = []
for var in ['HOME', 'TMPDIR']:
    path = os.path.join(os.environ[var], 'mine-' + var)
    with open(path, 'w') as f:
        f.write('mine')
    with open(path) as f:
        own.append(f.read())
json.dump({'found': found, 'sibling_found': sibling_found, 'listed': listed,
           'own': ' '.join(own)}, sys.stdout)
"#;

/// Write exec atom `name`, whose command runs `script`, with `env`
/// declared, and whose output has the string fields `fields`.
fn exec_atom(project: &Path, name: &str, script: &str, env: &str, fields: &[&str]) {
    for sub in ["atoms", "tools"] {
        std::fs::create_dir_all(project.join(sub)).expect("project");
    }
    std::fs::write(project.join("atomic.yaml"), "atomic: 1\nname: scratch\n").expect("manifest");
    std::fs::write(project.join(format!("tools/{name}.py")), script).expect("script");
    std::fs::write(
        project.join(format!("atoms/{name}.atom.astro")),
        format!(
            "---\nimport type {{ Props, Input, Output }} from '../.atomic/types/{name}';\n\
             import type {{ AtomConfig }} from '../.atomic/types/authoring';\n\
             const {{ input }}: Props = Astro.props;\n\
             export const config: AtomConfig<Input, Output> = {{ description: 'Scratch.', \
             kind: 'exec', exec: {{ command: ['python3', 'tools/{name}.py'] }}, \
             env: {{ {env} }} }};\n---\n"
        ),
    )
    .expect("atom");
    std::fs::write(
        project.join(format!("atoms/{name}.input.schema.yaml")),
        "type: object\nadditionalProperties: false\nproperties: {}\n",
    )
    .expect("input");
    let properties: String = fields
        .iter()
        .map(|f| format!("  {f}: {{type: [string, integer]}}\n"))
        .collect();
    std::fs::write(
        project.join(format!("atoms/{name}.output.schema.yaml")),
        format!(
            "type: object\nadditionalProperties: false\nrequired: [{}]\nproperties:\n{properties}",
            fields.join(", ")
        ),
    )
    .expect("output");
}

async fn run(project: &Path, name: &str, step: &str, backend: Arc<dyn ExecutionBackend>) -> Value {
    let mut registry = Registry::new();
    let atom = atomic_core::atom::load(
        &project.join(format!("atoms/{name}.atom.astro")),
        &mut registry,
    )
    .expect("loads");
    let compiled = atomic_core::compile::compile(&atom, &registry).expect("compiles");
    let (out, _) = runtime::run_exec(
        &atom,
        &compiled,
        &json!({}),
        &EffectIdentity {
            run_id: "scratch-1".into(),
            step_id: step.into(),
        },
        backend,
        None,
        &runtime::Artifacts::none(),
        &atomic_core::usage::Metering::none(),
    )
    .await
    .expect("the command runs");
    out
}

/// Whether a file under `dir` holds `needle`.
fn holds(dir: &Path, needle: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        let path = entry.path();
        if path.is_dir() {
            holds(&path, needle)
        } else {
            std::fs::read_to_string(&path).is_ok_and(|text| text.contains(needle))
        }
    })
}

#[test]
fn an_exec_command_cannot_read_another_attempts_scratch_and_keeps_its_own() {
    support::require_sandbox();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-exec-scratch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin")).expect("dir");
    let dir = std::fs::canonicalize(dir).expect("canonical");
    on_codex_runtime(async move {
        let state = dir.join("state");
        let root = state.join("scratch");
        std::fs::create_dir_all(&root).expect("scratch root");
        // Another backend's scratch root, as `atomic debug codex-prompt` has, with
        // a secret in it.
        let sibling = state.join("scratch-debug");
        std::fs::create_dir_all(sibling.join("planted")).expect("sibling root");
        std::fs::write(sibling.join("planted/secret"), "secret-in-the-sibling").expect("planted");
        let release = dir.join("release");
        let project = dir.join("project");
        exec_atom(
            &project,
            "keeper",
            KEEPER,
            &format!("RELEASE: '{}'", release.display()),
            &["kept"],
        );
        exec_atom(
            &project,
            "prober",
            PROBER,
            &format!(
                "SCRATCH_ROOT: '{}', SIBLING_ROOT: '{}'",
                root.display(),
                sibling.display()
            ),
            &["found", "sibling_found", "listed", "own"],
        );
        std::os::unix::fs::symlink(
            env!("CARGO_BIN_EXE_atomic"),
            dir.join("bin/codex-linux-sandbox"),
        )
        .expect("sandbox helper");
        let paths = CodexPaths::with_helpers(
            PathBuf::from(env!("CARGO_BIN_EXE_atomic")),
            dir.join("bin/codex-linux-sandbox"),
        );
        let sandbox = Arc::new(
            ExecSandbox::new(
                &paths,
                StateGuard::deny_state_dir(&state).expect("guard"),
                state.join("codex"),
            )
            .expect("confinement"),
        );
        let local = LocalBackend::in_dir(root.clone())
            .denying([sibling.clone()])
            .confined(sandbox);
        let backends: [(&str, Arc<dyn ExecutionBackend>); 2] = [
            ("the local backend", Arc::new(local.clone())),
            (
                "a runner process",
                Arc::new(atomic_exec::runner::ProcessRunner {
                    workspaces: local,
                    ..atomic_exec::runner::ProcessRunner::atomic(PathBuf::from(env!(
                        "CARGO_BIN_EXE_atomic"
                    )))
                }),
            ),
        ];
        for (what, backend) in backends {
            let _ = std::fs::remove_file(&release);
            let keeper = {
                let (project, backend) = (project.clone(), backend.clone());
                tokio::spawn(async move { run(&project, "keeper", "keep", backend).await })
            };
            // The keeper's secret is there before the prober looks, as this
            // test, which no sandbox confines, sees it.
            for _ in 0..300 {
                if holds(&root, "secret-of-the-keeper") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            assert!(
                holds(&root, "secret-of-the-keeper"),
                "{what}: the keeper wrote nothing"
            );
            let probed = run(&project, "prober", "probe", backend).await;
            // And still there after it looked: the keeper held its attempt.
            assert!(
                holds(&root, "secret-of-the-keeper"),
                "{what}: the keeper's secret was gone when the prober ended"
            );
            assert!(
                holds(&sibling, "secret-in-the-sibling"),
                "{what}: the other backend's secret was gone when the prober ended"
            );
            std::fs::write(&release, "").expect("release");
            let kept = keeper.await.expect("keeper");
            assert_eq!(kept["kept"], "yes", "{what}");
            assert_eq!(
                probed["sibling_found"], 0,
                "{what}: the prober read the other backend's scratch: {probed}"
            );
            assert_eq!(
                probed["found"], 0,
                "{what}: the prober read the keeper's scratch: {probed}"
            );
            // Step directories lie under the root as `<step>-<hash>`: the
            // keeper's must not show.
            let listed = probed["listed"].as_str().expect("listed");
            assert!(
                !listed.contains("'keep-"),
                "{what}: the prober saw the keeper's scratch: {probed}"
            );
            assert_eq!(
                probed["own"], "mine mine",
                "{what}: the prober lost its own HOME or TMPDIR: {probed}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    });
}
