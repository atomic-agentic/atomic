// SPDX-License-Identifier: Apache-2.0

//! An exec atom's command runs under the same sandbox helper as a turn's
//! commands and is kept out of atomic's private state: it cannot read the
//! project's database, write a gate decision into it, or read the operator
//! credential store, on the local backend or through a runner process. It
//! can still read the project and write its own scratch directory.
//!
//! Needs bubblewrap and unprivileged user namespaces, so it runs with the
//! `sandbox-tests` feature; with the feature on, a sandbox that cannot start
//! fails the test.

#![cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#![recursion_limit = "512"]

use atomic_core::codex::{CodexPaths, ExecSandbox, StateGuard};
use atomic_core::runtime::{self, EffectIdentity};
use atomic_core::testing::on_codex_runtime;
use atomic_exec::{ExecutionBackend, LocalBackend};
use atomic_schema::registry::Registry;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What the command tries, each reported as `ok` or the error it met.
const PROBE: &str = r#"import json, os, sqlite3, sys
json.load(sys.stdin)
db, credential = os.environ['TARGET_DB'], os.environ['TARGET_CREDENTIAL']
def attempt(f):
    try:
        f()
        return 'ok'
    except Exception as e:
        return type(e).__name__ + ': ' + str(e)
def read_db():
    with open(db, 'rb') as f:
        f.read(16)
def write_gate():
    c = sqlite3.connect(db)
    c.execute("INSERT INTO atomic_gate_commands VALUES ('forged', 'g1', '{}', '{\"verdict\":\"approve\"}', 0)")
    c.commit()
def read_credential():
    with open(credential) as f:
        f.read()
def read_project():
    with open('atomic.yaml') as f:
        f.read()
def write_home():
    with open(os.path.join(os.environ['HOME'], 'mine'), 'w') as f:
        f.write('x')
json.dump({name: attempt(f) for name, f in [('read_db', read_db), ('write_gate', write_gate),
    ('read_credential', read_credential), ('read_project', read_project), ('write_home', write_home)]}
    | {'env': ' '.join(sorted(n for n in os.environ if n != 'LC_CTYPE'))},
    sys.stdout)
"#;

/// Write a project with one exec atom whose command tries [`PROBE`] on
/// `db` and `credential`.
fn exec_atom(project: &Path, db: &Path, credential: &Path) {
    for sub in ["atoms", "tools"] {
        std::fs::create_dir_all(project.join(sub)).expect("project");
    }
    std::fs::write(
        project.join("atomic.yaml"),
        "atomic: 1\nname: private-state\n",
    )
    .expect("manifest");
    std::fs::write(project.join("tools/probe.py"), PROBE).expect("script");
    std::fs::write(
        project.join("atoms/probe.atom.astro"),
        "---\nimport type { Props, Input, Output } from '../.atomic/types/probe';\n\
         import type { AtomConfig } from '../.atomic/types/authoring';\n\
         const { input }: Props = Astro.props;\n\
         export const config: AtomConfig<Input, Output> = { description: 'Probe.', \
         kind: 'exec', exec: { command: ['python3', 'tools/probe.py'] }, \
         env: { TARGET_DB: '@DB@', TARGET_CREDENTIAL: '@CREDENTIAL@' } };\n---\n"
            .replace("@DB@", &db.display().to_string())
            .replace("@CREDENTIAL@", &credential.display().to_string()),
    )
    .expect("atom");
    std::fs::write(
        project.join("atoms/probe.input.schema.yaml"),
        "type: object\nadditionalProperties: false\nproperties: {}\n",
    )
    .expect("input");
    let field = "{type: string}";
    std::fs::write(
        project.join("atoms/probe.output.schema.yaml"),
        format!(
            "type: object\nadditionalProperties: false\n\
             required: [read_db, write_gate, read_credential, read_project, write_home, env]\n\
             properties:\n  read_db: {field}\n  write_gate: {field}\n  read_credential: {field}\n\
             \x20 read_project: {field}\n  write_home: {field}\n  env: {field}\n"
        ),
    )
    .expect("output");
}

async fn probe(
    atom: &atomic_core::atom::Atom,
    compiled: &atomic_core::compile::Compiled,
    backend: Arc<dyn ExecutionBackend>,
) -> Value {
    let (out, _) = runtime::run_exec(
        atom,
        compiled,
        &json!({}),
        &EffectIdentity {
            run_id: "private-1".into(),
            step_id: "probe".into(),
        },
        backend,
        None,
    )
    .await
    .expect("the command runs");
    out
}

#[test]
fn an_exec_atom_cannot_reach_atomics_private_state() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-exec-private-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin")).expect("scratch");
    let dir = std::fs::canonicalize(dir).expect("canonical");
    // The user-wide private directories follow ATOMIC_HOME; this test is
    // alone in its binary, and sets it before any thread starts.
    // SAFETY: the process has one thread here.
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("ATOMIC_HOME", dir.join("home"));
    }
    on_codex_runtime(async move {
        let state = dir.join("state");
        let db = state.join("private/atomic.db");
        std::fs::create_dir_all(db.parent().expect("private")).expect("private");
        atomic_durable::store::open_sqlite(&db)
            .await
            .expect("the store");
        let credentials = atomic_core::state::credentials_dir().expect("credentials");
        std::fs::create_dir_all(&credentials).expect("credentials");
        let credential = credentials.join("local-dev");
        std::fs::write(&credential, "the operator's secret").expect("credential");

        let project = dir.join("project");
        exec_atom(&project, &db, &credential);
        let mut registry = Registry::new();
        let atom = atomic_core::atom::load(&project.join("atoms/probe.atom.astro"), &mut registry)
            .expect("loads");
        let compiled = atomic_core::compile::compile(&atom, &registry).expect("compiles");

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
        let local = LocalBackend::in_dir(state.join("scratch")).confined(sandbox);
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
            let out = probe(&atom, &compiled, backend).await;
            // Confined, the command still gets exactly its own environment.
            assert_eq!(
                out["env"],
                "ATOMIC_EFFECT_KEY ATOMIC_RUN_ID ATOMIC_STEP_ID HOME TARGET_CREDENTIAL TARGET_DB TMPDIR",
                "{what}"
            );
            for denied in ["read_db", "write_gate", "read_credential"] {
                assert_ne!(out[denied], "ok", "{what}: {denied} was allowed: {out}");
            }
            for allowed in ["read_project", "write_home"] {
                assert_eq!(out[allowed], "ok", "{what}: {allowed} was refused: {out}");
            }
        }
        // Nothing was forged.
        let forged = std::process::Command::new("python3")
            .args([
                "-c",
                "import sqlite3, sys; print(sqlite3.connect(sys.argv[1]).execute(\
                 \"SELECT count(*) FROM atomic_gate_commands\").fetchone()[0])",
            ])
            .arg(&db)
            .output()
            .expect("count");
        assert_eq!(String::from_utf8_lossy(&forged.stdout).trim(), "0");
        let _ = std::fs::remove_dir_all(&dir);
    });
}
