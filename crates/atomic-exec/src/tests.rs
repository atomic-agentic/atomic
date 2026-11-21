// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::path::{Path, PathBuf};

fn shell(script: &str, stdin: &[u8], time: Duration) -> Result<Completed, ExecError> {
    LocalBackend::default().run(&Command {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: std::env::temp_dir(),
        env: vec![("ONLY".into(), "this".into())],
        stdin: stdin.to_vec(),
        time_limit: time,
        output_limit: 1024,
        writable: Vec::new(),
        read_only: Vec::new(),
        network: Vec::new(),
        secret_env: Default::default(),
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
            writable: Vec::new(),
            read_only: Vec::new(),
            network: Vec::new(),
            secret_env: Default::default(),
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
            writable: Vec::new(),
            read_only: Vec::new(),
            network: Vec::new(),
            secret_env: Default::default(),
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
/// the same step left, as one whose process stopped did, but an attempt
/// this process still has open keeps its directory, however late another
/// attempt of its step opens: a cancelled attempt that finishes opening
/// after its step's next attempt takes nothing from it.
#[test]
fn opening_an_attempt_removes_only_what_no_open_attempt_of_its_step_holds() {
    let src = source("again");
    let local = backend("again");
    // What a stopped process's attempt left.
    let step = workspace::step_dir(local.scratch_root(), "run-1", "fix");
    let left = step.join("999999999.1-a0");
    std::fs::create_dir_all(&left).expect("left");
    std::fs::write(left.join("planted"), "x").expect("write");
    let first = local.open(&opening(&src, "fix", "a1")).expect("opens");
    let first_scratch = first.scratch.clone().expect("scratch");
    assert!(!left.exists(), "the stopped attempt's scratch is left");
    assert_eq!(std::fs::read_dir(&first_scratch).expect("read").count(), 0);
    std::fs::write(first_scratch.join("working"), "x").expect("write");
    // Another attempt of the step opens while the first is still open.
    let late = local
        .open(&opening(&src, "fix", "a2"))
        .expect("opens again");
    let late_scratch = late.scratch.clone().expect("scratch");
    assert_ne!(first_scratch, late_scratch);
    assert!(
        first_scratch.join("working").exists(),
        "an open attempt's scratch was taken"
    );
    // Closing one leaves the other alone.
    local.close(&late).expect("closes");
    assert!(!late_scratch.exists());
    assert!(first_scratch.join("working").exists());
    local.close(&first).expect("closes");
    assert!(!first_scratch.exists());
    assert!(
        !step.exists(),
        "the step's directory goes with its last attempt"
    );
    // Nothing set aside is left behind.
    let aside = set_aside_left(local.scratch_root());
    assert!(aside.is_empty(), "{aside:?}");
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(local.scratch_root());
}

/// What is left set aside under `root`, to be removed.
fn set_aside_left(root: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".removing-"))
        .map(|e| e.path())
        .collect()
}

/// An attempt that is open already is not opened again, and nothing of it
/// is taken.
#[test]
fn an_attempt_that_is_open_is_not_opened_again() {
    let src = source("twice");
    let local = backend("twice");
    let open = local.open(&opening(&src, "fix", "a1")).expect("opens");
    let scratch = open.scratch.clone().expect("scratch");
    std::fs::write(scratch.join("working"), "x").expect("write");
    // What a stopped process left: the refused open still removes it.
    let left = scratch.parent().expect("step").join("999999999.1-a0");
    std::fs::create_dir_all(&left).expect("left");
    let again = local.open(&opening(&src, "fix", "a1"));
    assert!(
        matches!(&again, Err(e) if e.to_string().contains("exists already")),
        "{again:?}"
    );
    assert!(scratch.join("working").exists());
    assert!(!left.exists());
    assert!(
        set_aside_left(local.scratch_root()).is_empty(),
        "a refused open left what it set aside"
    );
    local.close(&open).expect("closes");
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(local.scratch_root());
}

/// Many threads opening and closing attempts of one step at once never
/// take one another's scratch.
#[test]
fn attempts_of_one_step_opened_and_closed_at_once_keep_their_scratch() {
    let src = source("crowd");
    let local = std::sync::Arc::new(backend("crowd"));
    let threads: Vec<_> = (0..16)
        .map(|t| {
            let (local, src) = (local.clone(), src.clone());
            std::thread::spawn(move || {
                for i in 0..300 {
                    let ws = local
                        .open(&opening(&src, "fix", &format!("t{t}-{i}")))
                        .expect("opens");
                    let scratch = ws.scratch.clone().expect("scratch");
                    std::fs::write(scratch.join("mine"), format!("{t}-{i}"))
                        .expect("its scratch is there");
                    assert_eq!(
                        std::fs::read_to_string(scratch.join("mine")).expect("still there"),
                        format!("{t}-{i}")
                    );
                    local.close(&ws).expect("closes");
                }
            })
        })
        .collect();
    for t in threads {
        t.join().expect("no attempt lost its scratch");
    }
    assert!(set_aside_left(local.scratch_root()).is_empty());
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(local.scratch_root());
}

/// What a killed process moved aside to remove, and did not, goes when a
/// backend next connects; this process's own is left to it.
#[test]
fn connecting_removes_what_a_killed_process_set_aside() {
    let local = backend("aside");
    std::fs::create_dir_all(local.scratch_root()).expect("root");
    let dead = local.scratch_root().join(".removing-999999999.1-0");
    std::fs::create_dir_all(dead.join("deep")).expect("dead");
    let ours = local.scratch_root().join(format!(
        ".removing-{}-0",
        tagged(std::process::id(), "x")
            .split('-')
            .next()
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&ours).expect("ours");
    local.connect().expect("connects");
    assert!(
        !dead.exists(),
        "a killed process's set-aside scratch is left"
    );
    assert!(ours.exists(), "this process's own was taken");
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
    // What a stopped process's attempt of the step left: a read-only tree
    // inside, one that is read-only itself, and a stray file.
    let step = workspace::step_dir(local.scratch_root(), "run-1", "fix");
    let first = step.join("999999999.1-a1");
    std::fs::create_dir_all(&first).expect("left");
    read_only(&first);
    let unmovable = step.join("999999999.1-a0");
    std::fs::create_dir_all(unmovable.join("inside")).expect("left");
    std::fs::set_permissions(&unmovable, std::fs::Permissions::from_mode(0o555)).expect("chmod");
    std::fs::write(step.join("stray"), "x").expect("stray file");
    let second = local
        .open(&opening(&src, "fix", "a2"))
        .expect("opens over a read-only tree");
    assert!(!first.exists());
    assert!(
        !unmovable.exists(),
        "a directory that cannot be moved is left"
    );
    assert!(!step.join("stray").exists(), "a stray file is left");
    assert!(
        set_aside_left(local.scratch_root()).is_empty(),
        "something set aside is left"
    );
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
    std::fs::write(tmp.join("left"), "by a turn").expect("write");
    local.connect().expect("connects");
    assert!(tmp.is_dir(), "and the sweep");
    assert!(!tmp.join("left").exists(), "emptied when no turn runs");
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
        writable: Vec::new(),
        read_only: Vec::new(),
        network: Vec::new(),
        secret_env: Default::default(),
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

/// Whether process `pid` still runs: `kill -0` reaches it, and on Linux it
/// is not a zombie waiting to be reaped.
fn alive(pid: &str) -> bool {
    let reached = std::process::Command::new("/bin/sh")
        .args(["-c", &format!("kill -0 {pid} 2>/dev/null")])
        .status()
        .expect("kill")
        .success();
    let zombie = || {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit(')')
                .next()
                .unwrap_or("")
                .trim_start()
                .starts_with('Z')
        })
    };
    reached && !(cfg!(target_os = "linux") && zombie())
}

/// Wait until `pid` is gone, or fail with `what`.
fn gone(pid: &str, what: &str) {
    let started = std::time::Instant::now();
    while alive(pid) {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{what}: {pid} survived"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn scratch_file(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("atomic-exec-{tag}-{}", std::process::id()))
}

/// /proc/<pid>/stat gives the process group on Linux only.
#[cfg(target_os = "linux")]
#[test]
fn a_command_runs_in_a_process_group_of_its_own() {
    // Fields 1 and 5 of /proc/<pid>/stat: the process and its group.
    let done = shell(
        "read -r pid _ _ _ group _ < /proc/self/stat; printf '%s %s' \"$pid\" \"$group\"",
        b"",
        Duration::from_secs(10),
    )
    .expect("runs");
    let text = String::from_utf8_lossy(&done.stdout).into_owned();
    let (pid, group) = text.split_once(' ').expect("two fields");
    assert_eq!(pid, group, "{text}");
}

/// /proc/<pid>/stat gives the session and terminal on Linux only.
#[cfg(target_os = "linux")]
#[test]
fn a_command_runs_in_a_session_of_its_own_with_no_terminal() {
    // Fields 1, 6 and 7 of /proc/<pid>/stat: the process, its session and
    // its controlling terminal (0 for none).
    let done = shell(
        "read -r pid _ _ _ _ session tty _ < /proc/self/stat; printf '%s %s %s ' \
         \"$pid\" \"$session\" \"$tty\"; (exec 3</dev/tty) 2>/dev/null && printf tty || printf none",
        b"",
        Duration::from_secs(10),
    )
    .expect("runs");
    let text = String::from_utf8_lossy(&done.stdout).into_owned();
    let fields: Vec<&str> = text.split(' ').collect();
    assert_eq!(fields[0], fields[1], "its own session: {text}");
    assert_eq!(fields[2], "0", "no controlling terminal: {text}");
    assert_eq!(fields[3], "none", "/dev/tty opened: {text}");
}

#[test]
fn a_command_past_its_time_limit_is_stopped_with_everything_it_started() {
    let file = scratch_file("group");
    let script = format!("sleep 30 & printf '%s' \"$!\" > '{}'; wait", file.display());
    let error = shell(&script, b"", Duration::from_millis(500)).expect_err("times out");
    assert!(matches!(error, ExecError::TimedOut { .. }), "{error}");
    let pid = std::fs::read_to_string(&file).expect("the background pid");
    let _ = std::fs::remove_file(&file);
    gone(&pid, "the command's own child outlived it");
}

#[test]
fn what_a_command_leaves_behind_ends_with_it_and_holds_nothing_open() {
    // The child keeps the command's stdout: unless it is stopped too, the
    // command's output never ends.
    let file = scratch_file("left");
    let script = format!(
        "sleep 30 & printf '%s' \"$!\" > '{}'; printf done",
        file.display()
    );
    let started = std::time::Instant::now();
    let done = shell(&script, b"", Duration::from_secs(20)).expect("runs");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the step waited for what the command left"
    );
    assert_eq!(done.stdout, b"done");
    let pid = std::fs::read_to_string(&file).expect("the background pid");
    let _ = std::fs::remove_file(&file);
    gone(&pid, "what it left");
}

#[test]
fn a_leftover_that_left_the_group_holds_the_output_only_briefly() {
    // A child in a session of its own, beyond the group's reach, keeps the
    // command's stdout open for half a minute.
    let file = scratch_file("setsid");
    let script = format!(
        "python3 -c 'import os, time; os.setsid(); open(\"{}\", \"w\").write(str(os.getpid())); time.sleep(30)' & \
         while [ ! -s '{}' ]; do sleep 0.05; done; printf done",
        file.display(),
        file.display()
    );
    let started = std::time::Instant::now();
    let done = shell(&script, b"", Duration::from_secs(20)).expect("runs");
    let pid = std::fs::read_to_string(&file).expect("the leftover's pid");
    let _ = std::fs::remove_file(&file);
    let _ = std::process::Command::new("/bin/sh")
        .args(["-c", &format!("kill -KILL {pid}")])
        .status();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the step waited {:?} for the leftover",
        started.elapsed()
    );
    assert_eq!(done.stdout, b"done");
}

/// A command's secret variables reach it on a pipe whose descriptor its
/// environment names, and are never in its environment.
#[test]
fn secret_variables_reach_a_command_on_a_pipe_and_never_its_environment() {
    let done = LocalBackend::default()
        .run(&Command {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                r#"printf '[%s]' "${K-unset}"; env; echo ---; tr '\0' '\n' < "/dev/fd/$ATOMIC_SECRET_ENV_FD""#
                    .into(),
            ],
            cwd: std::env::temp_dir(),
            env: vec![("ONLY".into(), "this".into())],
            time_limit: Duration::from_secs(10),
            output_limit: 4096,
            secret_env: SecretEnv(vec![("K".into(), "the-secret-value".into())]),
            ..Command::default()
        })
        .expect("runs");
    assert!(done.success(), "{}", String::from_utf8_lossy(&done.stderr));
    let out = String::from_utf8_lossy(&done.stdout);
    let (env, piped) = out.split_once("---\n").expect("both parts");
    assert!(env.starts_with("[unset]"), "{out}");
    assert!(
        !env.contains("the-secret-value"),
        "in its environment: {out}"
    );
    assert!(env.contains("ONLY=this"), "{out}");
    assert_eq!(piped, "K=the-secret-value\n");
    // Its Debug form names the variable and hides its value.
    let shown = format!(
        "{:?}",
        SecretEnv(vec![("K".into(), "the-secret-value".into())])
    );
    assert_eq!(shown, r#"["K=<hidden>"]"#);
}
