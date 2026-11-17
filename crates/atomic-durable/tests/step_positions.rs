// SPDX-License-Identifier: Apache-2.0

//! Step positions under concurrency and for queued children, across kills.
//!
//! An atom's turn, its reviewers and its repair rounds are sibling steps, and
//! some run concurrently. durare keys each checkpoint by a position counter per
//! workflow execution. This test pins down that positions do not depend on
//! timing: concurrent steps finish in a different order on every run, yet a run
//! that is killed several times and recovered records the same positions, and
//! the same child workflow ids, as an uninterrupted reference run.
//!
//! durare reserves a position when a durable call is *constructed*, not when it
//! is polled, so the order in which concurrent steps complete cannot move them.

use durare::{
    DurableContext, DurableEngine, EngineConfig, Error, InMemoryProvider, ListFilter, Result,
    SqliteProvider, StateProvider, WorkflowOptions, WorkflowQueue,
};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ENV_DIR: &str = "ATOMIC_POSITIONS_DIR";
const KILLS: u32 = 8;
const ID: &str = "fan-1";

/// Every recorded step of the parent and its children: (workflow, position, name).
type Positions = Vec<(String, i32, String)>;

#[test]
fn positions_survive_kills_and_completion_order() {
    // The reference: one uninterrupted run in memory.
    let reference = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            let provider: Arc<dyn StateProvider> = Arc::new(InMemoryProvider::new());
            let engine = engine_over(provider, false).await?;
            engine
                .start::<_, ()>("fan", (), WorkflowOptions::with_id(ID))
                .await?
                .result()
                .await?;
            let p = positions(&engine).await;
            engine.shutdown(Duration::from_secs(5)).await?;
            p
        })
        .expect("reference run");
    assert_eq!(
        reference.len(),
        4 + 3 + 1 + 3 * 2,
        "reference positions: {reference:?}"
    );

    // The same workflow on SQLite, killed repeatedly and recovered.
    let dir = std::env::temp_dir().join(format!(
        "atomic-step-positions-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create directory");
    for kill in 0..KILLS {
        let mut child = spawn(&dir).spawn().expect("spawn worker");
        std::thread::sleep(Duration::from_millis(40 + 35 * u64::from(kill)));
        let _ = child.kill();
        child.wait().expect("reap worker");
    }
    let out = spawn(&dir)
        .env("ATOMIC_POSITIONS_REPORT", "1")
        .stdout(Stdio::piped())
        .output()
        .expect("final worker");
    assert!(out.status.success(), "final worker failed: {}", out.status);
    let report = String::from_utf8_lossy(&out.stdout);
    let line = report
        .lines()
        .find_map(|l| l.split_once("POSITIONS ").map(|(_, json)| json))
        .unwrap_or_else(|| panic!("no positions report; worker stdout:\n{report}"));
    let recovered: Positions = serde_json::from_str(line).expect("positions JSON");
    assert_eq!(recovered, reference);
    let _ = std::fs::remove_dir_all(&dir);
}

fn spawn(dir: &Path) -> Command {
    let mut cmd = Command::new(std::env::current_exe().expect("this test binary"));
    cmd.args([
        "--exact",
        "positions_child",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(ENV_DIR, dir)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::inherit());
    cmd
}

/// Not a test on its own: the parent runs it in a worker process.
#[test]
#[ignore = "run by positions_survive_kills_and_completion_order in a child process"]
fn positions_child() {
    let Ok(dir) = std::env::var(ENV_DIR) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let report = std::env::var("ATOMIC_POSITIONS_REPORT").is_ok();
    let result = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            let url = format!("sqlite://{}?mode=rwc", dir.join("durare.db").display());
            let provider: Arc<dyn StateProvider> = Arc::new(SqliteProvider::connect(&url).await?);
            let engine = engine_over(provider, true).await?;
            engine
                .start::<_, ()>("fan", (), WorkflowOptions::with_id(ID))
                .await?
                .result()
                .await?;
            // Children finish before the parent's last step, but wait for any
            // stragglers so the report is complete.
            let until = Instant::now() + Duration::from_secs(60);
            loop {
                let open = engine
                    .list_workflows(&ListFilter {
                        status: vec!["PENDING".into(), "ENQUEUED".into()],
                        ..ListFilter::default()
                    })
                    .await?;
                if open.is_empty() || Instant::now() > until {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            if report {
                let p = positions(&engine).await?;
                println!(
                    "POSITIONS {}",
                    serde_json::to_string(&p).map_err(|e| Error::app(e.to_string()))?
                );
            }
            engine.shutdown(Duration::from_secs(5)).await
        });
    if let Err(e) = result {
        eprintln!("worker: {e}");
        std::process::exit(1);
    }
}

async fn engine_over(provider: Arc<dyn StateProvider>, recover: bool) -> Result<DurableEngine> {
    let config = EngineConfig {
        executor_id: Some("positions".into()),
        recover_on_launch: Some(recover),
        ..EngineConfig::default()
    };
    let mut engine = DurableEngine::with_config(provider, config).await?;
    engine.register_queue(WorkflowQueue::new("children").worker_concurrency(2));
    engine.register("fan", |ctx: DurableContext, (): ()| async move {
        // Four concurrent steps whose durations vary from run to run.
        let a = ctx.step("a0", || async { jitter().await });
        let b = ctx.step("a1", || async { jitter().await });
        let c = ctx.step("a2", || async { jitter().await });
        let d = ctx.step("a3", || async { jitter().await });
        let (a, b, c, d) = tokio::join!(a, b, c, d);
        let _ = (a?, b?, c?, d?);

        let mut children = Vec::new();
        for i in 0..3_i64 {
            children.push(
                ctx.enqueue_workflow::<_, ()>("children", "leaf", i, WorkflowOptions::default())
                    .await?,
            );
        }
        for child in children {
            child.result().await?;
        }
        ctx.step("last", || async { jitter().await }).await?;
        Ok(())
    });
    engine.register("leaf", |ctx: DurableContext, _i: i64| async move {
        let x = ctx.step("x", || async { jitter().await });
        let y = ctx.step("y", || async { jitter().await });
        let (x, y) = tokio::join!(x, y);
        let _ = (x?, y?);
        Ok(())
    });
    engine.launch().await?;
    Ok(engine)
}

/// Sleeps 0 to 60 ms, chosen from the clock, so completion order varies.
async fn jitter() -> Result<u64> {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()) % 61)
        .unwrap_or(0);
    tokio::time::sleep(Duration::from_millis(ms)).await;
    Ok(ms)
}

/// The parent's steps, then each child's, sorted by workflow id and position.
async fn positions(engine: &DurableEngine) -> Result<Positions> {
    let mut out = Positions::new();
    let mut ids = vec![ID.to_string()];
    let children = engine
        .list_workflows(&ListFilter {
            parent_workflow_ids: vec![ID.to_string()],
            ..ListFilter::default()
        })
        .await?;
    ids.extend(children.into_iter().map(|w| w.id));
    ids.sort();
    for id in ids {
        for s in engine.get_workflow_steps(&id).await? {
            out.push((id.clone(), s.step_id, s.name));
        }
    }
    Ok(out)
}
