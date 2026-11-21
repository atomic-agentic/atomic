// SPDX-License-Identifier: Apache-2.0

//! A workspace whose open was still under way when its run was cancelled is
//! closed once it opens: nothing is left on the backend by a run that gave
//! up waiting for it.

use atomic_core::AtomConfig;
use atomic_core::codex::{CodexPaths, ModelEndpoint, StateGuard, TurnSpec};
use atomic_core::runtime::{Artifacts, CodexRuntime, run_turn};
use atomic_core::usage::Metering;
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
fn a_workspace_that_opens_after_its_run_was_cancelled_is_closed() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .thread_stack_size(16 << 20)
        .enable_all()
        .build()
        .expect("runtime");
    let closed = rt.block_on(async {
        let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("atomic-cancelled-open-{}", std::process::id()));
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
            endpoint: ModelEndpoint {
                base_url: "http://127.0.0.1:9/v1".into(),
                model: "unused".into(),
                api_key_env: None,
            },
            defaults: AtomConfig::default(),
            artifacts: Artifacts::none(),
            meter: Metering::none(),
            caches: None,
        };
        let turn =
            tokio::spawn(async move { run_turn(&runtime, &backend, &TurnSpec::default()).await });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        // The run is cancelled while its workspace is opening; then it opens.
        turn.abort();
        let _ = turn.await;
        {
            let (lock, cv) = &*go;
            *lock.lock().expect("lock") = true;
            cv.notify_all();
        }
        let started = std::time::Instant::now();
        while closed.load(Ordering::SeqCst) == 0 && started.elapsed().as_secs() < 5 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let _ = std::fs::remove_dir_all(&dir);
        closed.load(Ordering::SeqCst)
    });
    assert_eq!(
        closed, 1,
        "the workspace that opened after the cancel was closed"
    );
}
