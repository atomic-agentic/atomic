// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::path::Path;

fn shell(script: &str, stdin: &[u8], time: Duration) -> Result<Completed, ExecError> {
    LocalBackend::default().run(&Command {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: std::env::temp_dir(),
        env: vec![("ONLY".into(), "this".into())],
        stdin: stdin.to_vec(),
        time_limit: time,
        output_limit: 1024,
    })
}

#[test]
fn a_command_gets_its_input_and_exactly_its_environment() {
    let done = shell(
        "cat; printf ' %s %s' \"$ONLY\" \"${HOME-unset}\"",
        b"input",
        Duration::from_secs(10),
    )
    .expect("runs");
    assert!(done.success());
    assert_eq!(done.stdout, b"input this unset");
}

#[test]
fn a_nonzero_exit_completes_with_its_code_and_stderr() {
    let done = shell(
        "printf 'why' >&2; exit 3",
        &vec![b'x'; 1_048_576],
        Duration::from_secs(10),
    )
    .expect("completes");
    assert_eq!(done.code, Some(3));
    assert_eq!(done.stderr, b"why");
}

#[test]
fn a_command_past_its_time_limit_is_stopped() {
    let started = std::time::Instant::now();
    let error = shell("sleep 5", b"", Duration::from_millis(200)).expect_err("stopped");
    assert!(matches!(error, ExecError::TimedOut { .. }), "{error}");
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[test]
fn output_past_its_limit_is_refused() {
    let error =
        shell("head -c 2048 /dev/zero", b"", Duration::from_secs(10)).expect_err("too large");
    assert!(matches!(error, ExecError::TooLarge { .. }), "{error}");
}

#[test]
fn no_shell_expands_the_arguments() {
    let done = LocalBackend::default()
        .run(&Command {
            program: "printf".into(),
            args: vec!["%s".into(), "$HOME;`id`".into()],
            cwd: std::env::temp_dir(),
            env: Vec::new(),
            stdin: Vec::new(),
            time_limit: Duration::from_secs(10),
            output_limit: 1024,
        })
        .expect("runs");
    assert_eq!(done.stdout, b"$HOME;`id`");
}

#[test]
fn a_missing_program_cannot_start() {
    let error = LocalBackend::default()
        .run(&Command {
            program: "atomic-no-such-program".into(),
            args: Vec::new(),
            cwd: std::env::temp_dir(),
            env: Vec::new(),
            stdin: Vec::new(),
            time_limit: Duration::from_secs(10),
            output_limit: 1024,
        })
        .expect_err("missing");
    assert!(matches!(error, ExecError::Start { .. }), "{error}");
}

fn source(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("atomic-exec-source-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("source");
    dir
}

/// A backend with scratch directories of its own for one test, under the
/// build's target directory: a scratch root under the system's temporary
/// directory is refused at connect.
fn backend(tag: &str) -> LocalBackend {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/atomic-exec-tests")
        .join(format!("scratch-{tag}-{}", std::process::id()));
    let _ = workspace::remove_tree(&root);
    LocalBackend::in_dir(root)
}

/// The name this process gives its own attempts' directories.
fn tagged(pid: u32, attempt: &str) -> String {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    let start = stat
        .rsplit_once(')')
        .and_then(|(_, after)| after.split_whitespace().nth(19))
        .unwrap_or("0")
        .to_owned();
    format!("{pid}.{start}-{attempt}")
}

fn opening(src: &Path, step: &str, attempt: &str) -> Open {
    Open {
        label: "fix tests".into(),
        run: "run-1".into(),
        step: step.into(),
        attempt: attempt.into(),
        source: Some(src.to_owned()),
        sandbox: Sandbox::WorkspaceWrite,
        writable_roots: vec![std::env::temp_dir()],
        env: [("DECLARED".to_owned(), "yes".to_owned())].into(),
    }
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn a_local_workspace_is_its_source_with_a_private_scratch_and_a_local_shell() {
    let src = source("open");
    let local = backend("open");
    let request = opening(&src, "fix", "a1");
    let ws = local.open(&request).expect("opens");
    assert_eq!(ws.root.as_ref(), Some(&src));
    assert_eq!(ws.shell, Shell::Local);
    assert_eq!(ws.sandbox, Sandbox::WorkspaceWrite);
    assert_eq!(ws.writable_roots, request.writable_roots);
    assert_eq!(ws.env, request.env);
    let scratch = ws.scratch.clone().expect("a scratch directory");
    assert!(scratch.is_dir());
    assert!(
        scratch.starts_with(local.scratch_root()),
        "{} is under the backend's scratch root",
        scratch.display()
    );
    assert!(
        !scratch.starts_with(&src),
        "scratch is outside the workspace"
    );
    assert_eq!(std::fs::read_dir(&scratch).expect("read").count(), 0);
    #[cfg(unix)]
    {
        assert_eq!(mode(local.scratch_root()), 0o700);
        assert_eq!(mode(scratch.parent().expect("step")), 0o700);
        assert_eq!(mode(&scratch), 0o700);
    }

    // Another step of the same run gets a directory of its own.
    let other = local.open(&opening(&src, "review", "b1")).expect("opens");
    assert_ne!(
        other.scratch.as_ref().and_then(|s| s.parent()),
        scratch.parent()
    );

    std::fs::write(scratch.join("left"), "behind").expect("write");
    local.close(&ws).expect("closes");
    assert!(!scratch.exists(), "the scratch directory goes");
    assert!(src.is_dir(), "the source stays");
    local.close(&ws).expect("closing twice succeeds");
    local.close(&other).expect("closes");
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(local.scratch_root());
}

/// An attempt that runs again starts with nothing an earlier attempt of
/// the same step left, finished or not.
#[test]
fn opening_an_attempt_removes_what_an_earlier_attempt_of_its_step_left() {
    let src = source("again");
    let local = backend("again");
    let first = local.open(&opening(&src, "fix", "a1")).expect("opens");
    let first_scratch = first.scratch.clone().expect("scratch");
    std::fs::write(first_scratch.join("planted"), "x").expect("write");
    // The first attempt never closed: its process stopped, say.
    let second = local
        .open(&opening(&src, "fix", "a2"))
        .expect("opens again");
    let second_scratch = second.scratch.clone().expect("scratch");
    assert_ne!(first_scratch, second_scratch);
    assert!(
        !first_scratch.exists(),
        "the earlier attempt's scratch is gone"
    );
    assert_eq!(std::fs::read_dir(&second_scratch).expect("read").count(), 0);
    // Closing the replaced attempt late leaves the new one alone.
    local.close(&first).expect("closes");
    assert!(second_scratch.is_dir());
    local.close(&second).expect("closes");
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(local.scratch_root());
}

/// Connecting removes the scratch of attempts whose process was killed,
/// and keeps that of attempts whose process still runs.
#[test]
fn connecting_sweeps_the_scratch_of_killed_attempts_only() {
    let local = backend("sweep");
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("a process that will be killed");
    let step = workspace::step_dir(local.scratch_root(), "run-1", "fix");
    let theirs = step.join(tagged(child.id(), "a1"));
    std::fs::create_dir_all(&theirs).expect("its scratch");
    std::fs::write(theirs.join("left"), "behind").expect("write");
    let ours = workspace::step_dir(local.scratch_root(), "run-1", "check")
        .join(tagged(std::process::id(), "b1"));
    std::fs::create_dir_all(&ours).expect("our scratch");
    // A directory naming this process's id with another start time belongs
    // to an earlier process that had the same id: it goes.
    let reused = workspace::step_dir(local.scratch_root(), "run-1", "old")
        .join(format!("{}.1-c1", std::process::id()));
    std::fs::create_dir_all(&reused).expect("an earlier process's scratch");

    local.connect().expect("connects");
    assert!(theirs.is_dir(), "a live attempt's scratch stays");

    child.kill().expect("kill -9");
    child.wait().expect("reaped");
    local.connect().expect("connects");
    assert!(!theirs.exists(), "a killed attempt's scratch goes");
    assert!(!step.exists(), "and its step's directory with it");
    assert!(ours.is_dir(), "this process's own attempts stay");
    #[cfg(target_os = "linux")]
    assert!(
        !reused.exists(),
        "an earlier process with the same id is dead"
    );
    let _ = workspace::remove_tree(local.scratch_root());
}

/// What an attempt left that cannot be written, such as a Go module cache,
/// is removed all the same, at open and at connect; nothing it leaves stops
/// a later run.
#[test]
fn read_only_trees_are_removed_at_open_and_at_connect() {
    use std::os::unix::fs::PermissionsExt;
    let read_only = |dir: &Path| {
        let cache = dir.join("go/pkg/mod/example.com/m@v1");
        std::fs::create_dir_all(&cache).expect("cache");
        std::fs::write(cache.join("go.mod"), "module m").expect("file");
        for d in [cache.as_path(), cache.parent().expect("parent")] {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o555)).expect("chmod");
        }
    };
    let src = source("read-only");
    let local = backend("read-only");
    let first = local.open(&opening(&src, "fix", "a1")).expect("opens");
    read_only(first.scratch.as_ref().expect("scratch"));
    let second = local
        .open(&opening(&src, "fix", "a2"))
        .expect("opens over a read-only tree");
    assert!(!first.scratch.as_ref().expect("scratch").exists());
    read_only(second.scratch.as_ref().expect("scratch"));
    local.close(&second).expect("closes a read-only tree");
    assert!(!second.scratch.as_ref().expect("scratch").exists());

    // A dead attempt's read-only tree goes at connect.
    let mut child = std::process::Command::new("true").spawn().expect("process");
    let dead = workspace::step_dir(local.scratch_root(), "run-1", "dead")
        .join(format!("{}.1-d1", child.id()));
    child.wait().expect("reaped");
    std::fs::create_dir_all(&dead).expect("dead scratch");
    read_only(&dead);
    local.connect().expect("connects");
    assert!(!dead.exists());
    let _ = workspace::remove_tree(local.scratch_root());
    let _ = std::fs::remove_dir_all(&src);
}

/// A scratch root where a sandboxed turn may write is refused: under the
/// system's temporary directory at connect, and under the workspace or one
/// of its writable roots at open.
#[test]
fn a_scratch_root_a_sandboxed_turn_could_write_is_refused() {
    for root in [
        PathBuf::from("/tmp/atomic-exec-refused"),
        std::env::temp_dir().join("atomic-exec-refused"),
    ] {
        let error = LocalBackend::in_dir(root.clone())
            .connect()
            .expect_err("refused");
        assert!(
            error
                .to_string()
                .contains("where a sandboxed turn may write"),
            "{}: {error}",
            root.display()
        );
    }
    assert!(
        LocalBackend::default().connect().is_err(),
        "the default is for tests only"
    );
    let src = source("inside");
    for root in [src.join(".atomic/scratch"), std::env::temp_dir().join("x")] {
        let error = LocalBackend::in_dir(root.clone())
            .open(&opening(&src, "fix", "a1"))
            .expect_err("refused");
        assert!(
            error
                .to_string()
                .contains("where a sandboxed turn may write"),
            "{}: {error}",
            root.display()
        );
        assert!(!root.exists(), "nothing was created");
    }
    let _ = std::fs::remove_dir_all(&src);
}

/// Every workspace on a backend shares one private temporary directory,
/// outside every attempt's scratch.
#[test]
fn turns_on_one_backend_share_a_private_temporary_directory() {
    let src = source("tmp");
    let local = backend("tmp");
    let a = local.open(&opening(&src, "fix", "a1")).expect("opens");
    let b = local.open(&opening(&src, "check", "b1")).expect("opens");
    let tmp = a.tmp.clone().expect("a temporary directory");
    assert_eq!(b.tmp.as_ref(), Some(&tmp));
    assert!(tmp.starts_with(local.scratch_root()));
    assert!(!tmp.starts_with(a.scratch.as_ref().expect("scratch")));
    assert_eq!(mode(&tmp), 0o700);
    local.close(&a).expect("closes");
    local.close(&b).expect("closes");
    assert!(tmp.is_dir(), "it outlives the workspaces");
    local.connect().expect("connects");
    assert!(tmp.is_dir(), "and the sweep");
    let _ = workspace::remove_tree(local.scratch_root());
    let _ = std::fs::remove_dir_all(&src);
}

#[test]
fn only_errors_that_may_pass_make_a_workspace_unavailable() {
    use std::io::{Error, ErrorKind};
    for kind in [
        ErrorKind::StorageFull,
        ErrorKind::QuotaExceeded,
        ErrorKind::ResourceBusy,
    ] {
        assert!(workspace::transient(&Error::from(kind)), "{kind:?}");
    }
    assert!(workspace::transient(&Error::from_raw_os_error(
        libc::EMFILE
    )));
    for kind in [
        ErrorKind::PermissionDenied,
        ErrorKind::NotADirectory,
        ErrorKind::AlreadyExists,
    ] {
        assert!(!workspace::transient(&Error::from(kind)), "{kind:?}");
    }
    // A scratch root that cannot be made because a file is in its way will
    // not change by trying again: the error is permanent.
    let src = source("unavailable");
    let blocker = backend("blocker").scratch_root().to_owned();
    std::fs::create_dir_all(blocker.parent().expect("parent")).expect("parent");
    std::fs::write(&blocker, "a file").expect("file");
    let error = LocalBackend::in_dir(blocker.join("scratch"))
        .open(&opening(&src, "fix", "a1"))
        .expect_err("refused");
    assert!(
        matches!(&error, ExecError::Workspace { message, .. } if !message.contains("sandboxed")),
        "{error}"
    );
    let _ = std::fs::remove_file(&blocker);
    let _ = std::fs::remove_dir_all(&src);
}

#[test]
fn a_workspace_with_no_source_has_no_files_and_no_shell() {
    let ws = LocalBackend::default()
        .open(&Open {
            label: "classify".into(),
            ..Open::default()
        })
        .expect("opens");
    assert_eq!(ws.root, None);
    assert_eq!(ws.scratch, None);
    assert_eq!(ws.shell, Shell::None);
    assert!(matches!(
        ws.require_root(),
        Err(ExecError::Workspace { .. })
    ));
    LocalBackend::default().close(&ws).expect("closes");
}

#[test]
fn a_source_that_is_not_a_directory_is_refused() {
    let error = LocalBackend::default()
        .open(&Open {
            label: "fix".into(),
            source: Some(std::env::temp_dir().join("atomic-exec-no-such-source")),
            ..Open::default()
        })
        .expect_err("refused");
    assert!(matches!(error, ExecError::Workspace { .. }), "{error}");
    assert!(error.to_string().contains("fix"), "{error}");
}

/// A command run in a local workspace ends exactly as the same command run
/// directly as a child process.
#[test]
fn a_command_in_a_local_workspace_runs_as_it_would_directly() {
    let src = source("same");
    std::fs::write(src.join("input.txt"), "from the source").expect("write");
    let ws = LocalBackend::default()
        .open(&Open {
            label: "same".into(),
            source: Some(src.clone()),
            ..Open::default()
        })
        .expect("opens");
    let scratch = ws.scratch.clone().expect("scratch").display().to_string();
    let command = Command {
        program: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            "cat input.txt; cat; printf ' %s' \"$HOME\"; exit 4".into(),
        ],
        cwd: ws.root.clone().expect("root"),
        env: vec![("HOME".into(), scratch)],
        stdin: b" and stdin".to_vec(),
        time_limit: Duration::from_secs(10),
        output_limit: 1024,
    };
    let through = LocalBackend::default().run(&command).expect("runs");
    let direct = process::run(
        Path::new("/bin/sh"),
        &command.args,
        Some(&command.cwd),
        &command.env,
        &command.stdin,
        process::Limits {
            time: command.time_limit,
            stdout: command.output_limit,
            stderr: process::STDERR_LIMIT,
        },
    )
    .expect("runs");
    assert_eq!(through, direct);
    assert_eq!(through.code, Some(4));
    LocalBackend::default().close(&ws).expect("closes");
    let _ = std::fs::remove_dir_all(&src);
}

/// A backend that states whatever it is given, as a runner on the wire
/// would, and counts how often it is asked.
struct Stating {
    statement: serde_json::Value,
    asked: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl ExecutionBackend for Stating {
    fn connect(&self) -> Result<Statement, ExecError> {
        self.asked
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        serde_json::from_value(self.statement.clone()).map_err(|e| ExecError::Io {
            program: "connect".into(),
            message: e.to_string(),
        })
    }
    fn open(&self, request: &Open) -> Result<Workspace, ExecError> {
        LocalBackend::default().open(request)
    }
    fn run(&self, command: &Command) -> Result<Completed, ExecError> {
        LocalBackend::default().run(command)
    }
    fn close(&self, workspace: &Workspace) -> Result<(), ExecError> {
        LocalBackend::default().close(workspace)
    }
}

#[test]
fn only_a_durable_statement_is_durable_and_anything_unrecognized_is_ephemeral() {
    use serde_json::json;
    assert_eq!(Durability::from_statement("durable"), Durability::Durable);
    for value in ["ephemeral", "snapshot", "Durable", "durable ", ""] {
        assert_eq!(
            Durability::from_statement(value),
            Durability::Ephemeral,
            "{value:?}"
        );
    }
    let read = |statement: serde_json::Value| -> Durability {
        serde_json::from_value::<Statement>(statement)
            .expect("any statement is read")
            .durability
    };
    assert_eq!(read(json!({"durability": "durable"})), Durability::Durable);
    for statement in [
        json!({"durability": "ephemeral"}),
        json!({"durability": "snapshotted"}),
        json!({"durability": 1}),
        json!({"durability": null}),
        json!({"durability": {"kind": "durable"}}),
        json!({}),
        json!({"durability": "durable-ish", "other": true}),
    ] {
        assert_eq!(
            read(statement.clone()),
            Durability::Ephemeral,
            "{statement}"
        );
    }
    // What a backend states round-trips by name.
    for durability in [Durability::Durable, Durability::Ephemeral] {
        let text = serde_json::to_value(Statement { durability }).expect("encodes");
        assert_eq!(text, json!({"durability": durability.as_str()}));
        assert_eq!(read(text), durability);
    }
}

#[test]
fn the_local_backend_states_that_its_workspaces_are_durable() {
    let local = backend("durable");
    assert_eq!(
        local.connect().expect("connects").durability,
        Durability::Durable
    );
    assert_eq!(Backend::default().durability(), Durability::Durable);
    assert_eq!(
        Backend::connect(local.clone())
            .expect("connects")
            .durability(),
        Durability::Durable
    );
}

#[test]
fn a_backend_states_its_durability_once_at_connect() {
    let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let backend = Backend::connect(Stating {
        statement: serde_json::json!({"durability": "snapshot"}),
        asked: asked.clone(),
    })
    .expect("connects");
    assert_eq!(backend.durability(), Durability::Ephemeral);
    assert_eq!(
        backend.connect().expect("stated").durability,
        Durability::Ephemeral
    );
    let shared = backend.clone();
    assert_eq!(shared.durability(), Durability::Ephemeral);
    assert_eq!(asked.load(std::sync::atomic::Ordering::Relaxed), 1);
}
