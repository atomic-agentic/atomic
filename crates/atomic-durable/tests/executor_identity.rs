// SPDX-License-Identifier: Apache-2.0

//! Executor identity across processes.
//!
//! Two worker processes share one SQLite database, each with its own executor
//! id, and each runs a workflow that parks inside its second step. Worker `a` is
//! killed and restarted under the same id with recovery on launch. The restart
//! must recover only `a`'s workflow: `b`'s workflow is still running in a live
//! process and must not run again.
//!
//! What durare does (and what this test pins down):
//!
//! - The executor id comes from `DBOS__VMID`, else `EngineConfig::executor_id`,
//!   else `"local"`, and is stored as each run's owner.
//! - Recovery on launch takes only the pending runs owned by this executor id.
//!   `recover_pending_for(ids)` recovers named owners that are known to have
//!   stopped; plain `recover()` takes every owner and is unsafe in a fleet.
//! - Nothing fences a stale owner that is still alive: two live processes with
//!   the same id would both run the workflow. That is what ownership epochs add.

use durare::{
    DurableContext, DurableEngine, EngineConfig, Error, ListFilter, Result, SqliteProvider,
};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ENV_ROLE: &str = "ATOMIC_IDENTITY_ROLE";
const ENV_DIR: &str = "ATOMIC_IDENTITY_DIR";
const ENV_EXECUTOR: &str = "ATOMIC_IDENTITY_EXECUTOR";

#[test]
fn a_restarted_worker_recovers_only_its_own_workflows() {
    let dir = std::env::temp_dir().join(format!(
        "atomic-executor-identity-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create the test directory");

    // One worker at a time while they start: two processes starting
    // workflows on one new SQLite file at once can leave a start pending
    // (tracked separately); this test is about recovery, not that.
    let mut a = spawn(&dir, "start", "a");
    wait_for(&dir, |log| log.contains("wf-a\theld\ta"));
    let mut b = spawn(&dir, "start", "b");
    wait_for(&dir, |log| log.contains("wf-b\theld\tb"));

    a.kill().expect("SIGKILL worker a");
    a.wait().expect("reap worker a");

    // Worker a again, same executor id, recovering on launch.
    let mut a2 = spawn(&dir, "recover", "a");
    wait_for(&dir, |log| log.matches("wf-a\theld\t").count() == 2);

    std::fs::write(dir.join("release"), b"").expect("release the held steps");
    for (name, child) in [("a2", &mut a2), ("b", &mut b)] {
        let status = wait_child(child, Duration::from_secs(60));
        assert!(status, "worker {name} did not finish cleanly");
    }

    let log = std::fs::read_to_string(dir.join("runs.log")).expect("read the run log");
    let runs = |wf: &str, step: &str| -> Vec<String> {
        log.lines()
            .filter_map(|l| {
                let mut p = l.split('\t');
                (p.next() == Some(wf) && p.next() == Some(step))
                    .then(|| p.next().unwrap_or("").to_owned())
            })
            .collect()
    };
    // a's first step completed before the kill, so the restart replays it.
    assert_eq!(runs("wf-a", "first"), ["a"]);
    // a's held step never checkpointed, so the restart runs it again.
    assert_eq!(runs("wf-a", "held"), ["a", "a"]);
    // b was never stopped, so nothing of b's ran twice.
    assert_eq!(runs("wf-b", "first"), ["b"]);
    assert_eq!(runs("wf-b", "held"), ["b"]);

    let owners = std::fs::read_to_string(dir.join("owners")).expect("read the owners report");
    assert!(owners.contains("wf-a SUCCESS a"), "owners: {owners}");
    assert!(owners.contains("wf-b SUCCESS b"), "owners: {owners}");
    let _ = std::fs::remove_dir_all(&dir);
}

fn spawn(dir: &Path, role: &str, executor: &str) -> Child {
    Command::new(std::env::current_exe().expect("this test binary"))
        .args([
            "--exact",
            "identity_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ENV_ROLE, role)
        .env(ENV_DIR, dir)
        .env(ENV_EXECUTOR, executor)
        .env_remove("DBOS__VMID")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn a worker")
}

fn wait_for(dir: &Path, done: impl Fn(&str) -> bool) {
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        let log = std::fs::read_to_string(dir.join("runs.log")).unwrap_or_default();
        if done(&log) {
            return;
        }
        assert!(Instant::now() < until, "timed out; run log so far:\n{log}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_child(child: &mut Child, limit: Duration) -> bool {
    let until = Instant::now() + limit;
    while Instant::now() < until {
        if let Some(status) = child.try_wait().expect("poll worker") {
            return status.success();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    false
}

/// Not a test on its own: the parent runs it in a worker process.
#[test]
#[ignore = "run by a_restarted_worker_recovers_only_its_own_workflows in a child process"]
fn identity_child() {
    let Ok(role) = std::env::var(ENV_ROLE) else {
        return;
    };
    let dir = PathBuf::from(std::env::var(ENV_DIR).expect("directory"));
    let executor = std::env::var(ENV_EXECUTOR).expect("executor id");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    if let Err(e) = runtime.block_on(worker(&dir, &role, &executor)) {
        eprintln!("worker {executor} ({role}): {e}");
        std::process::exit(1);
    }
}

async fn worker(dir: &Path, role: &str, executor: &str) -> Result<()> {
    let url = format!("sqlite://{}?mode=rwc", dir.join("durare.db").display());
    let provider = Arc::new(SqliteProvider::connect(&url).await?);
    let config = EngineConfig {
        executor_id: Some(executor.to_owned()),
        recover_on_launch: Some(role == "recover"),
        ..EngineConfig::default()
    };
    let mut engine = DurableEngine::with_config(provider, config).await?;
    let log_dir = dir.to_owned();
    let me = executor.to_owned();
    engine.register("held", move |ctx: DurableContext, (): ()| {
        let (dir, me) = (log_dir.clone(), me.clone());
        async move {
            let wf = ctx.workflow_id().to_owned();
            {
                let (dir, me, wf) = (dir.clone(), me.clone(), wf.clone());
                ctx.step(
                    "first",
                    move || async move { log_run(&dir, &wf, "first", &me) },
                )
                .await?;
            }
            let (dir2, me2, wf2) = (dir.clone(), me.clone(), wf.clone());
            ctx.step("held", move || async move {
                log_run(&dir2, &wf2, "held", &me2)?;
                while !dir2.join("release").exists() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Ok(())
            })
            .await?;
            Ok(())
        }
    });
    engine.launch().await?;

    let id = format!("wf-{executor}");
    if role == "start" {
        engine
            .start::<_, ()>("held", (), durare::WorkflowOptions::with_id(&id))
            .await?;
    }
    // Wait for this worker's workflow to finish, then report every owner.
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        let rows = engine
            .list_workflows(&ListFilter {
                workflow_ids: vec![id.clone()],
                ..ListFilter::default()
            })
            .await?;
        if rows.iter().any(|w| w.status == "SUCCESS") {
            break;
        }
        if Instant::now() > until {
            return Err(Error::app(format!("{id} did not finish: {rows:?}")));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let all = engine.list_workflows(&ListFilter::default()).await?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("owners"))
        .map_err(|e| Error::app(e.to_string()))?;
    for w in all.iter().filter(|w| w.id == id) {
        writeln!(f, "{} {} {}", w.id, w.status, w.executor_id)
            .map_err(|e| Error::app(e.to_string()))?;
    }
    engine.shutdown(Duration::from_secs(5)).await
}

fn log_run(dir: &Path, wf: &str, step: &str, executor: &str) -> Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("runs.log"))
        .map_err(|e| Error::app(e.to_string()))?;
    // One write call per line, so a kill cannot leave half a line behind.
    f.write_all(format!("{wf}\t{step}\t{executor}\n").as_bytes())
        .map_err(|e| Error::app(e.to_string()))?;
    f.sync_all().map_err(|e| Error::app(e.to_string()))
}
