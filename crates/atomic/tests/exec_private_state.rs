// SPDX-License-Identifier: Apache-2.0

//! An exec atom's command runs under the same sandbox helper as a turn's
//! commands and is kept out of atomic's private state: it cannot read the
//! project's database, write a gate decision into it, or read the operator
//! credential store, nor the person's own secrets in their home directory,
//! on the local backend or through a runner process. It
//! can still read the project, write its own scratch directory and open a
//! network connection.
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
def read_other_db():
    with open(os.environ['OTHER_DB'], 'rb') as f:
        f.read(16)
def read_credential():
    with open(credential) as f:
        f.read()
def read_home_secret():
    with open(os.environ['HOME_SECRET']) as f:
        f.read()
def read_project():
    with open('atomic.yaml') as f:
        f.read()
def unix_socket():
    import socket
    c = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    c.settimeout(5)
    c.connect(os.environ['TARGET_SOCKET'])
    c.close()
def tcp_socket():
    import socket
    c = socket.create_connection(('127.0.0.1', int(os.environ['TARGET_PORT'])), timeout=5)
    c.close()
def write_home():
    with open(os.path.join(os.environ['HOME'], 'mine'), 'w') as f:
        f.write('x')
json.dump({name: attempt(f) for name, f in [('read_db', read_db), ('write_gate', write_gate),
    ('read_credential', read_credential), ('read_home_secret', read_home_secret), ('read_other_db', read_other_db), ('read_project', read_project), ('write_home', write_home), ('unix_socket', unix_socket), ('tcp_socket', tcp_socket)]}
    | {'env': ' '.join(sorted(n for n in os.environ if n != 'LC_CTYPE'))},
    sys.stdout)
"#;

/// Write a project with one exec atom whose command tries [`PROBE`] on what
/// `env` names.
fn exec_atom(project: &Path, env: &[(&str, String)]) {
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
         env: { @ENV@ } };\n---\n"
            .replace(
                "@ENV@",
                &env.iter()
                    .map(|(k, v)| format!("{k}: '{v}'"))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
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
             required: [read_db, write_gate, read_credential, read_home_secret, read_other_db, read_project, write_home, unix_socket, tcp_socket, env]\n\
             properties:\n  read_db: {field}\n  write_gate: {field}\n  read_credential: {field}\n  read_home_secret: {field}\n  read_other_db: {field}\n\
             \x20 read_project: {field}\n  write_home: {field}\n  unix_socket: {field}\n  tcp_socket: {field}\n  env: {field}\n"
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
        &runtime::Artifacts::none(),
    )
    .await
    .expect("the command runs");
    out
}

#[test]
fn an_exec_atom_cannot_reach_atomics_private_state() {
    support::require_sandbox();
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
        // The person's home, with a key in it, as the sandbox finds it.
        std::env::set_var("HOME", dir.join("person"));
    }
    let home_secret = dir.join("person/.ssh/id_ed25519");
    std::fs::create_dir_all(home_secret.parent().expect("ssh")).expect("ssh");
    std::fs::write(&home_secret, "the person's key").expect("key");
    on_codex_runtime(async move {
        // The layout atomic keeps: state under the home's `projects/`, and
        // every project's private state under its `private/`.
        let state = dir.join("home/projects/p");
        std::fs::create_dir_all(&state).expect("state");
        let db = dir.join("home/private/p/atomic.db");
        std::fs::create_dir_all(db.parent().expect("private")).expect("private");
        atomic_durable::store::open_sqlite(&db)
            .await
            .expect("the store");
        let credentials = atomic_core::state::credentials_dir().expect("credentials");
        std::fs::create_dir_all(&credentials).expect("credentials");
        let credential = credentials.join("local-dev");
        std::fs::write(&credential, "the operator's secret").expect("credential");

        // Another project's state under the same atomic home.
        let other_db = dir.join("home/private/other/atomic.db");
        std::fs::create_dir_all(other_db.parent().expect("private")).expect("other");
        std::fs::write(&other_db, "SQLite format 3").expect("other db");
        // A socket a confined command must not reach, such as a bus, and a
        // TCP port.
        // Its path goes through a short link in /tmp: a Unix socket's path
        // must fit in 108 bytes, and a deep checkout's target directory
        // need not.
        let short = Path::new("/tmp").join(format!(
            "atomic-eps-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        std::os::unix::fs::symlink(&dir, &short).expect("short link");
        let socket = short.join("bus.sock");
        let _bus = std::os::unix::net::UnixListener::bind(&socket).expect("unix socket");
        assert!(
            dir.join("bus.sock").exists(),
            "the socket is in the test's directory"
        );
        // The positive control: outside the sandbox, the socket answers.
        let _reached = std::os::unix::net::UnixStream::connect(&socket).expect("reached");
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").expect("tcp");
        let port = tcp.local_addr().expect("port").port();
        let project = dir.join("project");
        let text = |p: &Path| p.display().to_string();
        exec_atom(
            &project,
            &[
                ("TARGET_DB", text(&db)),
                ("TARGET_CREDENTIAL", text(&credential)),
                ("HOME_SECRET", text(&home_secret)),
                ("OTHER_DB", text(&other_db)),
                ("TARGET_SOCKET", text(&socket)),
                ("TARGET_PORT", port.to_string()),
            ],
        );
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
                "ATOMIC_EFFECT_KEY ATOMIC_RUN_ID ATOMIC_STEP_ID HOME HOME_SECRET OTHER_DB \
                 TARGET_CREDENTIAL TARGET_DB TARGET_PORT TARGET_SOCKET TMPDIR",
                "{what}"
            );
            let denied = |path: &Path| {
                format!(
                    "PermissionError: [Errno 13] Permission denied: '{}'",
                    path.display()
                )
            };
            assert_eq!(out["read_db"], denied(&db), "{what}");
            assert_eq!(
                out["write_gate"], "OperationalError: unable to open database file",
                "{what}"
            );
            assert_eq!(out["read_credential"], denied(&credential), "{what}");
            assert_eq!(out["read_home_secret"], denied(&home_secret), "{what}");
            assert_eq!(out["read_other_db"], denied(&other_db), "{what}");
            // No network and no sockets, until atoms can declare what they
            // need.
            for refused in ["unix_socket", "tcp_socket"] {
                assert_eq!(
                    out[refused], "PermissionError: [Errno 1] Operation not permitted",
                    "{what}: {refused}"
                );
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
        let _ = std::fs::remove_file(&short);
        let _ = std::fs::remove_dir_all(&dir);
    });
}
