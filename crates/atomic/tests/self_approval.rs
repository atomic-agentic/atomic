// SPDX-License-Identifier: Apache-2.0

//! An atom cannot approve its own gate through its shell.
//!
//! A run waits at a gate. A step of the same project runs under Codex's
//! `workspace-write` sandbox, with the real Linux sandbox helper, in a
//! worktree inside atomic's state directory, as worktree steps do, and with
//! a writable root over the whole state directory, as a misplaced cache
//! would be. The scripted fake model's command then tries every way to the
//! approval a shell has:
//!
//! - writing the approval into the gate store, `private/atomic.db`;
//! - listing the private state, or reading Codex's own database;
//! - reading the operator's credential from the kernel keyrings, where the
//!   OS keyring caches it: by its key id, by searching, and after linking
//!   every keyring `/proc/keys` lists into its own;
//! - planting a key of its own;
//! - connecting to a Unix socket, as it would to the Secret Service on the
//!   D-Bus session bus or an admin socket.
//!
//! Each is refused (while this process can read the key), the gate stays
//! open, and the run passes it only once the operator decides with the
//! credential they enrolled. A second test runs a turn through `atomic`'s
//! own entry point, so the helper is where `dispatch` puts it, inside the
//! state directory, with a bubblewrap that has `--argv0` and one that has
//! not.
//!
//! Needs bubblewrap, unprivileged user namespaces and `keyctl`, so it runs
//! with the `sandbox-tests` feature (and `ATOMIC_DEV_SANDBOX=on` in the
//! development container); with the feature on, a sandbox or keyring that
//! cannot start fails the test.

#![cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#![recursion_limit = "512"]
#![allow(clippy::unwrap_used)]

mod support;

use atomic_core::Sandbox;
use atomic_core::codex::{CodexPaths, ModelEndpoint, PrivateState, StateGuard, TurnSpec};
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
import ctypes, glob, os, platform, socket, sqlite3, sys, time
state, gate, key, description, sock = sys.argv[1:6]

def attempt(name, f):
    try:
        print(f"{name}=ok:{f()}", flush=True)
    except Exception as e:
        print(f"{name}=refused:{type(e).__name__}: {e}", flush=True)

def forge():
    db = sqlite3.connect(os.path.join(state, "private", "atomic.db"), timeout=10)
    n = db.execute(
        "UPDATE atomic_gates SET state = 'approved', decided_by = 'local:dev', "
        "decided_at = ?, command_id = 'forged' WHERE gate_id = ? AND state = 'open'",
        (int(time.time() * 1000), gate),
    ).rowcount
    db.commit()
    if n != 1:
        raise RuntimeError(f"{n} rows changed")
    return "approved"

def codex_db():
    leaked = []
    for f in glob.glob(os.path.join(state, "codex", "*.sqlite*")):
        try:
            with open(f, "rb") as h:
                if h.read(15) == b"SQLite format 3":
                    leaked.append(f)
        except OSError:
            pass
    if not leaked:
        raise PermissionError("no Codex database readable")
    return leaked

libc = ctypes.CDLL(None, use_errno=True)
libc.syscall.restype = ctypes.c_long
SYS_KEYCTL, SYS_ADD_KEY = {"x86_64": (250, 248), "aarch64": (219, 217)}[platform.machine()]
KEYCTL_LINK, KEYCTL_SEARCH, KEYCTL_READ, SESSION = 8, 10, 11, -3

def call(nr, *args):
    r = libc.syscall(ctypes.c_long(nr), *args)
    if r < 0:
        e = ctypes.get_errno()
        raise OSError(e, os.strerror(e))
    return r

def keyctl(*args):
    return call(SYS_KEYCTL, *[a if isinstance(a, bytes) else ctypes.c_long(a) for a in args])

def read(id):
    buf = ctypes.create_string_buffer(256)
    n = libc.syscall(ctypes.c_long(SYS_KEYCTL), ctypes.c_long(KEYCTL_READ), ctypes.c_long(id),
                     buf, ctypes.c_long(256))
    if n < 0:
        e = ctypes.get_errno()
        raise OSError(e, os.strerror(e))
    return buf.raw[:n].decode()

def search():
    return read(keyctl(KEYCTL_SEARCH, SESSION, b"user", description.encode(), 0))

def link_everything():
    linked = 0
    with open("/proc/keys") as keys:
        for line in keys:
            fields = line.split()
            if len(fields) > 7 and fields[7] == "keyring":
                try:
                    keyctl(KEYCTL_LINK, int(fields[0], 16), SESSION)
                    linked += 1
                except OSError:
                    pass
    return f"{linked} linked, then {search()}"

def plant():
    payload = b"planted"
    return call(SYS_ADD_KEY, b"user", b"atomic:planted", payload,
                ctypes.c_size_t(len(payload)), ctypes.c_long(SESSION))

def connect():
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(sock)
    return "connected"

attempt("forge", forge)
attempt("list", lambda: os.listdir(os.path.join(state, "private")))
attempt("codexdb", codex_db)
attempt("key", lambda: read(int(key)))
attempt("search", search)
attempt("link", link_everything)
attempt("plant", plant)
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

/// What this process reads from the key `key`: the positive control that
/// the keyring holds the secret where the atom looks for it.
fn read_from_session_keyring(key: &str) -> String {
    let output = Command::new("python3")
        .args([
            "-c",
            "import ctypes,platform,sys; l=ctypes.CDLL(None); l.syscall.restype=ctypes.c_long; \
             b=ctypes.create_string_buffer(256); \
             n=l.syscall({'x86_64':250,'aarch64':219}[platform.machine()], 11, int(sys.argv[1]), b, 256); \
             sys.stdout.write(b.raw[:n].decode() if n >= 0 else 'unreadable')",
            key,
        ])
        .output()
        .unwrap();
    String::from_utf8(output.stdout).unwrap()
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

/// The function call outputs Codex sent the model in its second request.
fn command_output(model: &FakeModel) -> String {
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

/// Run one workspace-write turn in `cwd` under `guard`, with a writable root
/// over all of `state`, whose model runs `command` once, and return what the
/// command printed.
async fn sandboxed(
    dir: &Path,
    state: &Path,
    guard: &StateGuard,
    cwd: &Path,
    command: &str,
) -> String {
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
    // Scratch outside `state`, which this turn may write all of.
    let scratch = dir.join("scratch-root");
    let runtime = atomic_core::runtime::CodexRuntime {
        paths,
        codex_home: state.join("codex"),
        cwd: cwd.to_owned(),
        endpoint: ModelEndpoint {
            base_url: model.base_url(),
            model: "synthetic".into(),
            api_key_env: None,
        },
        defaults: Default::default(),
        artifacts: atomic_core::runtime::Artifacts::none(),
        meter: atomic_core::usage::Metering::none(),
        guard: guard.clone(),
        backend: atomic_exec::Backend::connect(atomic_exec::LocalBackend::in_dir(scratch)).unwrap(),
    };
    let answer = atomic_core::runtime::run_turn(
        &runtime,
        &TurnSpec {
            prompt: "Do the work.".into(),
            sandbox: Sandbox::WorkspaceWrite,
            writable_roots: vec![state.to_owned()],
            ..TurnSpec::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(answer, "Done.");
    command_output(&model)
}

#[test]
fn an_atom_cannot_approve_its_own_gate_through_its_shell() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("atomic-self-approval-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in [
            "project",
            "state/codex",
            "state/private",
            "bin",
            "credentials",
        ] {
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

        // A run waits at its gate, in the gate store in the private state.
        let mut rt = Durare::sqlite(&state.join("private/atomic.db"))
            .await
            .unwrap();
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
        // The positive control: the key is there to be read.
        assert_eq!(read_from_session_keyring(&key), secret);

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
        let guard = StateGuard::Deny(PrivateState {
            state_dir: state.clone(),
            private: state.join("private"),
            also: vec![dir.join("credentials")],
        });
        let command = format!(
            "printf 'changed\\n' > a.txt; echo wrote-exit=$?; \
             python3 {} {} {} {key} {description} {}",
            dir.join("bin/probe.py").display(),
            state.display(),
            opened.id,
            sock.display(),
        );
        let output = sandboxed(&dir, &state, &guard, &wt.path, &command).await;
        let planted = Command::new("python3")
            .args(["-c", "print(open('/proc/keys').read())"])
            .output()
            .unwrap();
        unlink_from_session_keyring(&key);
        drop(listener);

        // The step still works in its worktree...
        assert!(output.contains("wrote-exit=0"), "{output}");
        assert_eq!(
            std::fs::read_to_string(wt.path.join("a.txt")).unwrap(),
            "changed\n"
        );
        // ...and every way to the approval is refused.
        for attempt in [
            "forge", "list", "codexdb", "key", "search", "link", "plant", "connect",
        ] {
            assert!(
                output.contains(&format!("{attempt}=refused:")),
                "{attempt} was not refused: {output}"
            );
        }
        assert!(!output.contains(&secret), "the secret leaked: {output}");
        assert!(
            !String::from_utf8_lossy(&planted.stdout).contains("atomic:planted"),
            "the atom planted a key"
        );
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

/// A `bwrap` that logs how it was called, and, with `argv0` false, hides
/// `--argv0` from its help, as bubblewrap before 0.9 has none.
fn wrapped_bwrap(bin: &Path, argv0: bool) -> PathBuf {
    let real = Command::new("sh")
        .args(["-c", "command -v bwrap"])
        .output()
        .unwrap();
    let real = String::from_utf8(real.stdout).unwrap().trim().to_owned();
    assert!(!real.is_empty(), "no bubblewrap on PATH");
    let filter = if argv0 { "cat" } else { "grep -v -- --argv0" };
    let log = bin.join("bwrap.log");
    std::fs::create_dir_all(bin).unwrap();
    let script = format!(
        "#!/bin/sh\n\
         echo \"$@\" >> {log}\n\
         if [ \"$1\" = --help ]; then {real} --help 2>&1 | {filter}; exit 0; fi\n\
         exec {real} \"$@\"\n",
        log = log.display(),
    );
    let wrapper = bin.join("bwrap");
    std::fs::write(&wrapper, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    log
}

/// A read-only turn run by the `atomic` binary itself, so the sandbox
/// helper is the link `dispatch` makes in Codex's home, inside the state
/// directory. It sees the project and not the private state, whether
/// bubblewrap re-runs the helper by name (`--argv0`) or by that path.
#[test]
fn a_turn_through_atomics_own_entry_point_runs_with_its_helper_in_the_state_directory() {
    support::require_sandbox();
    for argv0 in [true, false] {
        on_codex_runtime(async move {
            let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
                .join(format!("atomic-entry-{argv0}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            for sub in ["project", "home"] {
                std::fs::create_dir_all(dir.join(sub)).unwrap();
            }
            let dir = std::fs::canonicalize(dir).unwrap();
            let project = dir.join("project");
            std::fs::write(project.join("a.txt"), "the project\n").unwrap();
            let log = wrapped_bwrap(&dir.join("bwrap"), argv0);
            let state = {
                let out = Command::new(env!("CARGO_BIN_EXE_atomic"))
                    .args(["debug", "state-dir"])
                    .current_dir(&project)
                    .env("ATOMIC_HOME", dir.join("home"))
                    .output()
                    .unwrap();
                assert!(out.status.success(), "{out:?}");
                PathBuf::from(String::from_utf8(out.stdout).unwrap().trim())
            };
            // The project's private state is in the home's `private/`.
            let db = dir
                .join("home/private")
                .join(state.file_name().unwrap())
                .join("atomic.db");
            assert!(db.exists());
            let command = format!(
                "cat a.txt; echo read-exit=$?; head -c 15 {}; echo; \
                 echo db-exit=$?",
                db.display()
            );
            let model = FakeModel::start(vec![
                Reply::Tool {
                    name: "exec_command".into(),
                    arguments: json!({"cmd": command, "workdir": project, "yield_time_ms": 20000}),
                },
                Reply::Text("Done.".into()),
            ])
            .await
            .unwrap();
            let path = format!(
                "{}:{}",
                dir.join("bwrap").display(),
                std::env::var("PATH").unwrap_or_default()
            );
            let (base_url, home) = (model.base_url(), dir.join("home"));
            let project_dir = project.clone();
            let out = tokio::task::spawn_blocking(move || {
                Command::new(env!("CARGO_BIN_EXE_atomic"))
                    .args(["debug", "codex-prompt", "--base-url", &base_url])
                    .args(["--model", "synthetic", "Do", "the", "work."])
                    .current_dir(&project_dir)
                    .env("ATOMIC_HOME", home)
                    .env("PATH", path)
                    .output()
                    .unwrap()
            })
            .await
            .unwrap();
            assert!(out.status.success(), "{out:?}");
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "Done.");
            let output = command_output(&model);
            assert!(
                output.contains("the project\nread-exit=0"),
                "argv0 {argv0}: the turn did not see the project: {output}"
            );
            assert!(!output.contains("SQLite format"), "{output}");
            let calls = std::fs::read_to_string(&log).unwrap_or_default();
            let ran: Vec<&str> = calls
                .lines()
                .filter(|l| l.contains("--apply-seccomp-then-exec"))
                .collect();
            assert!(!ran.is_empty(), "bubblewrap never ran a command: {calls}");
            for line in ran {
                assert_eq!(line.contains("--argv0"), argv0, "{line}");
                if !argv0 {
                    // The helper is re-run by the path dispatch gave it.
                    assert!(
                        line.contains(&state.join("codex").display().to_string()),
                        "{line}"
                    );
                }
            }
            let _ = std::fs::remove_dir_all(&dir);
        });
    }
}
