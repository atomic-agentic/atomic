// SPDX-License-Identifier: Apache-2.0

//! A workspace that opens as this process stops is closed again at once, and
//! nothing runs in it: a step that waited for its workspace gives up with
//! `Stopping` rather than start a command or a model call during shutdown.
//! Alone in its binary, since stopping is for the whole process.

use atomic_core::codex::{CodexPaths, ModelEndpoint, StateGuard, TurnSpec};
use atomic_core::runtime::{Artifacts, CodexRuntime, run_turn, stop_waiting_for_backend};
use atomic_core::usage::Metering;
use atomic_core::{AtomConfig, AtomError};
use atomic_exec::{
    Backend, Command, Completed, ExecError, ExecutionBackend, LocalBackend, Open, Statement,
    Workspace,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// A local backend whose `open` waits until it is let go, and which counts
/// what it closes.
struct Held {
    local: LocalBackend,
    go: Arc<(Mutex<bool>, Condvar)>,
    closed: Arc<AtomicUsize>,
}

impl ExecutionBackend for Held {
    fn connect(&self) -> Result<Statement, ExecError> {
        Ok(LocalBackend::STATEMENT)
    }
    fn open(&self, request: &Open) -> Result<Workspace, ExecError> {
        let (lock, cv) = &*self.go;
        let mut go = lock.lock().expect("lock");
        while !*go {
            go = cv.wait(go).expect("wait");
        }
        self.local.open(request)
    }
    fn run(&self, command: &Command) -> Result<Completed, ExecError> {
        self.local.run(command)
    }
    fn close(&self, workspace: &Workspace) -> Result<(), ExecError> {
        self.closed.fetch_add(1, Ordering::SeqCst);
        self.local.close(workspace)
    }
}

#[test]
fn a_workspace_that_opens_as_the_process_stops_is_closed_and_runs_nothing() {
    // On a thread and a runtime of its own, with the deep stacks Codex's
    // turns need, so a turn that went on, failing the test, never holds the
    // test binary open.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .thread_stack_size(16 << 20)
            .enable_all()
            .build()
            .expect("runtime");
        let _ = tx.send(rt.block_on(opens_as_it_stops()));
    });
    let (ended, closed) = rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("the turn ends at once instead of going on");
    let ended = ended.expect("the turn ends at once instead of going on");
    assert!(matches!(ended, Err(AtomError::Stopping)), "{ended:?}");
    assert_eq!(closed, 1, "the workspace was closed");
}

/// How the turn ended, if it did within 20 seconds, and how many workspaces
/// the backend closed.
async fn opens_as_it_stops() -> (Option<Result<String, AtomError>>, usize) {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-stopping-open-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("cwd")).expect("cwd");
    let go = Arc::new((Mutex::new(false), Condvar::new()));
    let closed = Arc::new(AtomicUsize::new(0));
    let backend = Backend::connect(Held {
        local: LocalBackend::in_dir(dir.join("scratch")),
        go: go.clone(),
        closed: closed.clone(),
    })
    .expect("connects");
    let runtime = CodexRuntime {
        paths: CodexPaths::without_helpers(),
        codex_home: dir.join("home"),
        guard: StateGuard::NoProjectState,
        cwd: dir.join("cwd"),
        // Nothing listens there: a model call would fail with another error.
        endpoint: ModelEndpoint {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "unused".into(),
            api_key_env: None,
            sign_in: None,
        },
        defaults: AtomConfig::default(),
        artifacts: Artifacts::none(),
        meter: Metering::none(),
        caches: None,
    };
    let turn =
        tokio::spawn(async move { run_turn(&runtime, &backend, &TurnSpec::default()).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    // The process stops while the open is under way, then the open succeeds.
    stop_waiting_for_backend();
    {
        let (lock, cv) = &*go;
        *lock.lock().expect("lock") = true;
        cv.notify_all();
    }
    // Bounded: a turn that went on to its model call would not end soon.
    let ended = tokio::time::timeout(std::time::Duration::from_secs(20), turn)
        .await
        .ok()
        .map(|joined| joined.expect("the turn"));
    let _ = std::fs::remove_dir_all(&dir);
    (ended, closed.load(Ordering::SeqCst))
}
