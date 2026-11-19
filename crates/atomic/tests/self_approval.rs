// SPDX-License-Identifier: Apache-2.0

//! An atom cannot approve its own gate through its shell.
//!
//! A run waits at a gate. A step of the same project runs under Codex's
//! `workspace-write` sandbox, with the real Linux sandbox helper, in a
//! worktree inside atomic's state directory, as worktree steps do. The
//! scripted fake model's command then tries every way to the approval a
//! shell has:
//!
//! - writing the approval into the gate store, `atomic.db` in the state
//!   directory;
//! - reading the state directory at all;
//! - reading the operator's credential from the kernel session keyring,
//!   where the OS keyring caches it, by its key id and by searching;
//! - connecting to a Unix socket outside the state directory, as it would
//!   to the Secret Service on the D-Bus session bus or an admin socket.
//!
//! Each is refused, the gate stays open, and the run passes it only once
//! the operator decides with the credential they enrolled.
//!
//! Needs bubblewrap, unprivileged user namespaces and `keyctl`, so it runs
//! with the `sandbox-tests` feature (and `ATOMIC_DEV_SANDBOX=on` in the
//! development container); with the feature on, a sandbox or keyring that
//! cannot start fails the test.

#![cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#![recursion_limit = "512"]
#![allow(clippy::unwrap_used)]

use atomic_core::Sandbox;
use atomic_core::codex::{self, CodexPaths, ModelEndpoint, TurnSpec};
use atomic_core::credential::{self, MemorySecrets};
use atomic_core::git::Git;
use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use atomic_core::worktree::Worktrees;
use atomic_durable::gate::{
    self, DecisionCommand, Deployment, GateSpec, HumanCredential, OnTimeout, Principal, Resolution,
    Tier, Verdict,
};
use atomic_durable::{DurableRuntime, Durare, DurareContext};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn operator() -> Principal {
    Principal::local("dev")
}

fn plain_git(dir: &Path, args: &[&str]) {
    let output = Command::new(Git::find().unwrap().program())
        .args([
            "-c",
            "user.name=synthetic-author",
            "-c",
            "user.email=synthetic@example.invalid",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgSign=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
}

/// What the atom's command runs: every way to an approval it has, each
/// reported as `<name>=ok:<what>` or `<name>=refused:<why>`.
const PROBE: &str = r#"
import ctypes, os, platform, socket, sqlite3, sys, time
state, gate, key, description, sock = sys.argv[1:6]

def attempt(name, f):
    try:
        print(f"{name}=ok:{f()}", flush=True)
    except Exception as e:
        print(f"{name}=refused:{type(e).__name__}: {e}", flush=True)

def forge():
    db = sqlite3.connect(os.path.join(state, "atomic.db"), timeout=10)
    n = db.execute(
        "UPDATE atomic_gates SET state = 'approved', decided_by = 'local:dev', "
        "decided_at = ?, command_id = 'forged' WHERE gate_id = ? AND state = 'open'",
        (int(time.time() * 1000), gate),
    ).rowcount
    db.commit()
    if n != 1:
        raise RuntimeError(f"{n} rows changed")
    return "approved"

libc = ctypes.CDLL(None, use_errno=True)
libc.syscall.restype = ctypes.c_long
SYS_KEYCTL = {"x86_64": 250, "aarch64": 219}[platform.machine()]
KEYCTL_SEARCH, KEYCTL_READ, SESSION = 10, 11, -3

def keyctl(*args):
    r = libc.syscall(ctypes.c_long(SYS_KEYCTL), *args)
    if r < 0:
        e = ctypes.get_errno()
        raise OSError(e, os.strerror(e))
    return r

def read(id):
    buf = ctypes.create_string_buffer(256)
    n = keyctl(ctypes.c_long(KEYCTL_READ), ctypes.c_long(id), buf, ctypes.c_long(256))
    return buf.raw[:n].decode()

def search():
    found = keyctl(ctypes.c_long(KEYCTL_SEARCH), ctypes.c_long(SESSION), b"user",
                   description.encode(), ctypes.c_long(0))
    return read(found)

def connect():
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(sock)
    return "connected"

attempt("forge", forge)
attempt("list", lambda: os.listdir(state))
attempt("key", lambda: read(int(key)))
attempt("search", search)
attempt("connect", connect)
"#;

/// Add `secret` to this process's session keyring as a `user` key named
/// `description`, as the OS keyring's cache does, and return its id. The
/// session keyring is shared with the children that inherit it, so a child
/// adds it for this process too.
fn cache_in_session_keyring(description: &str, secret: &str) -> String {
    let add = r#"
import ctypes, platform, sys
libc = ctypes.CDLL(None, use_errno=True)
libc.syscall.restype = ctypes.c_long
SYS_ADD_KEY = {"x86_64": 248, "aarch64": 217}[platform.machine()]
secret = sys.argv[2].encode()
r = libc.syscall(ctypes.c_long(SYS_ADD_KEY), b"user", sys.argv[1].encode(), secret,
                 ctypes.c_size_t(len(secret)), ctypes.c_long(-3))
if r < 0:
    sys.exit(f"add_key: errno {ctypes.get_errno()}")
print(r)
"#;
    let output = Command::new("python3")
        .args(["-c", add, description, secret])
        .output()
        .unwrap();
    // The keyring is part of what this test checks: without it, it fails.
    assert!(
        output.status.success(),
        "cannot add a key to the session keyring: {output:?}"
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn unlink_from_session_keyring(key: &str) {
    let _ = Command::new("python3")
        .args([
            "-c",
            "import ctypes,platform,sys; l=ctypes.CDLL(None); \
             l.syscall({'x86_64':250,'aarch64':219}[platform.machine()], 9, int(sys.argv[1]), -3)",
            key,
        ])
        .status();
}

fn register(rt: &mut Durare) {
    rt.register("gated", |ctx: DurareContext, (): ()| async move {
        let spec = GateSpec {
            workflow: "gated".into(),
            title: "Accept the change".into(),
            show: json!({"change": "a.txt"}),
            evidence: Vec::new(),
            approvers: gate::resolve(
                "accept",
                None,
                &Deployment::Local {
                    operator: operator(),
                },
            )
            .map_err(|e| atomic_durable::DurableError::Failed(e.to_string()))?,
            tier: Tier::High,
            timeout: None,
            on_timeout: OnTimeout::Reject,
        };
        gate::gate(&ctx, "accept", spec).await
    });
}

/// Run one workspace-write turn in `cwd`, denied `state`, whose model runs
/// `command` once, and return what the command printed.
async fn sandboxed(dir: &Path, state: &Path, cwd: &Path, command: &str) -> String {
    let model = FakeModel::start(vec![
        Reply::Tool {
            name: "exec_command".into(),
            arguments: json!({"cmd": command, "workdir": cwd, "yield_time_ms": 20000}),
        },
        Reply::Text("Done.".into()),
    ])
    .await
    .unwrap();
    let paths = CodexPaths::with_helpers(
        PathBuf::from(env!("CARGO_BIN_EXE_atomic")),
        dir.join("bin/codex-linux-sandbox"),
    );
    let answer = codex::run(
        &paths,
        &state.join("codex"),
        cwd,
        &ModelEndpoint {
            base_url: model.base_url(),
            model: "synthetic".into(),
            api_key_env: None,
        },
        &TurnSpec {
            prompt: "Do the work.".into(),
            sandbox: Sandbox::WorkspaceWrite,
            state_dir: Some(state.to_owned()),
            ..TurnSpec::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(answer, "Done.");
    let requests = model.requests();
    let follow_up = requests.get(1).expect("Codex answered the tool call");
    let output = follow_up["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "function_call_output")
        .map(|item| item["output"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>()
        .join("\n");
    eprintln!("the command in the sandbox printed: {output}");
    output
}

#[test]
fn an_atom_cannot_approve_its_own_gate_through_its_shell() {
    on_codex_runtime(async {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("atomic-self-approval-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["project", "state/codex", "bin"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let dir = std::fs::canonicalize(dir).unwrap();
        let (project, state) = (dir.join("project"), dir.join("state"));
        plain_git(&project, &["init", "-q"]);
        std::fs::write(project.join("a.txt"), "base\n").unwrap();
        plain_git(&project, &["add", "a.txt"]);
        plain_git(&project, &["commit", "-q", "-m", "base"]);
        std::os::unix::fs::symlink(
            env!("CARGO_BIN_EXE_atomic"),
            dir.join("bin/codex-linux-sandbox"),
        )
        .unwrap();
        std::fs::write(dir.join("bin/probe.py"), PROBE).unwrap();

        // A run waits at its gate, in the gate store in the state directory.
        let mut rt = Durare::sqlite(&state.join("atomic.db")).await.unwrap();
        register(&mut rt);
        rt.launch().await.unwrap();
        let run = format!("self-approval-{}", std::process::id());
        rt.start("gated", &run, ()).await.unwrap();
        let id = gate::gate_id(&run, "accept");
        let opened = loop {
            if let Some(g) = gate::find(&rt, &id).await.unwrap() {
                break g;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };

        // The operator enrolled a credential, which the OS keyring caches
        // in the session keyring this process (and so atomic) holds.
        let secrets = MemorySecrets::default();
        let account = format!("operator:dev@{}", std::process::id());
        let secret = credential::new_operator_secret(&secrets, &account).unwrap();
        let mine = HumanCredential::new(operator(), secret.clone());
        gate::enroll(&rt, &mine).await.unwrap();
        let description = format!("atomic:{account}");
        let key = cache_in_session_keyring(&description, &secret);

        // A socket the command could reach if it could connect at all.
        let sock = dir.join("admin.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();

        // The atom's step: a worktree inside the state directory.
        let repo = Git::find().unwrap().discover(&project).unwrap();
        let worktrees = Worktrees::new(repo, state.join("worktrees"));
        let base = worktrees.pin_base("self-approval", "main").unwrap();
        let wt = worktrees
            .prepare("self-approval", "implement", &base)
            .unwrap();
        let command = format!(
            "printf 'changed\\n' > a.txt; echo wrote-exit=$?; \
             python3 {} {} {} {key} {description} {}",
            dir.join("bin/probe.py").display(),
            state.display(),
            opened.id,
            sock.display(),
        );
        let output = sandboxed(&dir, &state, &wt.path, &command).await;
        unlink_from_session_keyring(&key);
        drop(listener);

        // The step still works in its worktree...
        assert!(output.contains("wrote-exit=0"), "{output}");
        assert_eq!(
            std::fs::read_to_string(wt.path.join("a.txt")).unwrap(),
            "changed\n"
        );
        // ...and every way to the approval is refused.
        for attempt in ["forge", "list", "key", "search", "connect"] {
            assert!(
                output.contains(&format!("{attempt}=refused:")),
                "{attempt} was not refused: {output}"
            );
        }
        assert!(!output.contains(&secret), "the secret leaked: {output}");
        let still = gate::find(&rt, &id).await.unwrap().unwrap();
        assert_eq!(still.resolution, None, "the gate is still open");

        // The operator, with their credential, decides; the run passes.
        let decision = gate::decide(
            &rt,
            &mine,
            DecisionCommand {
                command_id: "human".into(),
                gate: opened.id.clone(),
                verdict: Verdict::Approve,
                comment: None,
                proposal: opened.proposal.clone(),
            },
        )
        .await
        .unwrap();
        let resolution: Resolution = tokio::time::timeout(Duration::from_secs(30), rt.result(&run))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolution, Resolution::Decided(decision));
        rt.shutdown(Duration::from_secs(5)).await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    });
}
