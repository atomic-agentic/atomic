// SPDX-License-Identifier: Apache-2.0

//! Random-kill spike: atomic's load pattern under repeated `SIGKILL`.
//!
//! The parent test re-runs this test binary as a child process, kills it at a
//! random moment, and repeats until it has killed it [`KILLS`] times. Each child
//! recovers what the previous one left behind, starts the workflows for the
//! current round, and makes progress until it is killed. A last child runs with
//! no kill, drains every workflow to completion and checks the invariants.
//!
//! The load pattern is the shape of an atomic workflow:
//!
//! - long steps (a model turn stands in as a sleep inside the step body);
//! - a `recv` between steps, released by a message from outside (a gate);
//! - children enqueued on a concurrency-capped queue and awaited;
//! - a transaction step that writes an application row;
//! - a cron schedule that keeps starting short workflows.
//!
//! What "no completed step repeated" means here: every step body appends a
//! line to an fsync'd run log and returns a fresh nonce, which its checkpoint
//! records. A step may run more than once only inside the at-least-once window
//! (killed after its effect, before its checkpoint committed). So for every
//! step, the nonce its checkpoint recorded must be the *last* run in the log:
//! a run logged after the recorded one means a step ran again after it had
//! completed. Transaction steps commit their row with their checkpoint, so each
//! must have written exactly one row.
//!
//! Set `ATOMIC_SPIKE_SEED` to reproduce a run; the seed is printed.

use durare::{
    DurableContext, DurableEngine, EngineConfig, Error, ListFilter, Result, ScheduleOptions,
    SqliteProvider, StateProvider, WorkflowOptions, WorkflowQueue, params,
};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How many times the parent kills a child.
const KILLS: u32 = 100;
/// Pipelines started per round.
const PIPELINES_PER_ROUND: usize = 3;
/// Children each pipeline enqueues.
const CHILDREN: i64 = 3;
/// A new round of pipelines starts every this many child processes.
const SPAWNS_PER_ROUND: u32 = 10;
/// How long a long step's body runs.
const LONG_STEP: Duration = Duration::from_millis(120);

const ENV_ROLE: &str = "ATOMIC_SPIKE_ROLE";
const ENV_DIR: &str = "ATOMIC_SPIKE_DIR";
const ENV_ROUND: &str = "ATOMIC_SPIKE_ROUND";

type Nonces = Vec<(String, String)>;

// ---------------------------------------------------------------------------
// The parent: spawn, kill, repeat, then drain and check.

#[test]
fn a_hundred_random_kills_on_sqlite() {
    let seed = std::env::var("ATOMIC_SPIKE_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(1)
                | 1
        });
    println!("ATOMIC_SPIKE_SEED={seed}");
    let mut rng = XorShift(seed);

    let dir = std::env::temp_dir().join(format!("atomic-kill-spike-{}-{seed}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the spike directory");

    let mut kills = 0;
    let mut spawns = 0;
    let mut finished_early = 0;
    let started = Instant::now();
    while kills < KILLS {
        let round = spawns / SPAWNS_PER_ROUND;
        let mut child = spawn(&dir, "work", round);
        spawns += 1;
        // Mostly short lives, so kills land in startup and recovery too, with
        // some long enough for whole pipelines to finish.
        let life = Duration::from_millis(if rng.below(4) == 0 {
            300 + rng.below(1200)
        } else {
            20 + rng.below(400)
        });
        let deadline = Instant::now() + life;
        let mut exited = None;
        while Instant::now() < deadline {
            if let Some(status) = child.try_wait().expect("poll the child") {
                exited = Some(status);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        match exited {
            Some(status) => {
                let errors = std::fs::read_to_string(dir.join("errors.log")).unwrap_or_default();
                assert!(
                    status.success(),
                    "a work child failed on its own: {status}\n{errors}"
                );
                finished_early += 1;
            }
            None => {
                child.kill().expect("SIGKILL the child");
                child.wait().expect("reap the child");
                kills += 1;
            }
        }
    }
    println!(
        "{kills} kills over {spawns} processes ({finished_early} exited by themselves) in {:.1?}",
        started.elapsed()
    );

    let rounds = spawns.div_ceil(SPAWNS_PER_ROUND);
    let output = spawn(&dir, "drain", rounds.saturating_sub(1))
        .wait_with_output()
        .expect("run the drain child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("{stdout}");
    assert!(
        output.status.success(),
        "drain failed: {}\n{stderr}",
        output.status
    );
    assert!(stdout.contains("SPIKE OK"), "drain did not report success");
    let _ = std::fs::remove_dir_all(&dir);
}

fn spawn(dir: &Path, role: &str, round: u32) -> Child {
    let exe = std::env::current_exe().expect("this test binary");
    let mut cmd = Command::new(exe);
    cmd.args([
        "--exact",
        "spike_child",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(ENV_ROLE, role)
    .env(ENV_DIR, dir)
    .env(ENV_ROUND, round.to_string())
    .stdin(Stdio::null());
    if role == "work" {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    } else {
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    cmd.spawn().expect("spawn a child")
}

// ---------------------------------------------------------------------------
// The child: one process lifetime.

/// Not a test on its own: the parent runs it in a child process.
#[test]
#[ignore = "run by a_hundred_random_kills_on_sqlite in a child process"]
fn spike_child() {
    let Ok(role) = std::env::var(ENV_ROLE) else {
        return;
    };
    let dir = PathBuf::from(std::env::var(ENV_DIR).expect("spike directory"));
    let round: u32 = std::env::var(ENV_ROUND)
        .expect("round")
        .parse()
        .expect("round number");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let result = runtime.block_on(child_main(&dir, &role, round));
    if let Err(e) = result {
        eprintln!("child error: {e}");
        // Work children's output is discarded; keep their errors for the parent.
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("errors.log"))
        {
            let _ = f.write_all(format!("{role} child (round {round}): {e:?}\n").as_bytes());
        }
        std::process::exit(1);
    }
}

async fn child_main(dir: &Path, role: &str, round: u32) -> Result<()> {
    let url = format!("sqlite://{}?mode=rwc", dir.join("durare.db").display());
    let provider: Arc<dyn StateProvider> = Arc::new(SqliteProvider::connect(&url).await?);
    let config = EngineConfig {
        executor_id: Some("spike".into()),
        recover_on_launch: Some(true),
        ..EngineConfig::default()
    };
    let mut engine = DurableEngine::with_config(provider, config).await?;
    // A workflow can be alive through every kill; the default cap of 100
    // recoveries would park it.
    for name in ["pipeline", "child", "ticker", "audit"] {
        engine.max_recovery_attempts_for(name, i32::MAX);
    }
    engine.register_queue(WorkflowQueue::new("work").worker_concurrency(2));
    let run_log = RunLog::open(dir)?;
    register(&mut engine, run_log.clone());
    engine.launch().await?;

    if engine.get_schedule("tick").await?.is_none() && role == "work" {
        engine
            .create_schedule("tick", "ticker", "* * * * * *", ScheduleOptions::new())
            .await?;
    }

    // Start the pipelines of every round so far; their ids make the start
    // idempotent. A slow machine can kill every child of a round before any
    // of them starts its pipelines, and the gates below must not be sent to
    // workflows that were never created.
    for r in 0..=round {
        for p in 0..PIPELINES_PER_ROUND {
            let id = pipeline_id(r, p);
            engine
                .start::<_, Nonces>("pipeline", (), WorkflowOptions::with_id(&id))
                .await?;
        }
    }

    // Open the gate of every pipeline so far, a little later, so kills land
    // while pipelines wait in recv as well as after.
    tokio::time::sleep(Duration::from_millis(60)).await;
    for r in 0..=round {
        for p in 0..PIPELINES_PER_ROUND {
            let id = pipeline_id(r, p);
            engine
                .send_with_idempotency_key(&id, "go", "gate", &format!("gate-{id}"))
                .await?;
        }
    }

    if role == "work" {
        // Keep working until killed; exit cleanly if everything finishes.
        let until = Instant::now() + Duration::from_secs(30);
        while Instant::now() < until && !all_done(&engine, round).await? {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        return Ok(());
    }

    // Drain: stop the schedule, wait for everything to finish, then check.
    engine.delete_schedule("tick").await?;
    let until = Instant::now() + Duration::from_secs(120);
    loop {
        let open = open_workflows(&engine).await?;
        if open.is_empty() {
            break;
        }
        if Instant::now() > until {
            return Err(Error::app(format!("workflows never finished: {open:?}")));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Read the transaction steps' rows through a transaction of our own.
    let tx_rows: Vec<(String, i64)> = engine
        .start::<_, Vec<(String, i64)>>("audit", (), WorkflowOptions::with_id("audit"))
        .await?
        .result()
        .await?;
    engine.shutdown(Duration::from_secs(5)).await?;
    check(&engine, dir, round, tx_rows.into_iter().collect()).await
}

fn pipeline_id(round: u32, p: usize) -> String {
    format!("pipeline-r{round}-p{p}")
}

async fn all_done(engine: &DurableEngine, round: u32) -> Result<bool> {
    for p in 0..PIPELINES_PER_ROUND {
        let rows = engine
            .list_workflows(&ListFilter {
                workflow_ids: vec![pipeline_id(round, p)],
                ..ListFilter::default()
            })
            .await?;
        if rows.iter().any(|w| w.status != "SUCCESS") || rows.is_empty() {
            return Ok(false);
        }
    }
    Ok(open_workflows(engine).await?.is_empty())
}

async fn open_workflows(engine: &DurableEngine) -> Result<Vec<String>> {
    let rows = engine
        .list_workflows(&ListFilter {
            status: vec!["PENDING".into(), "ENQUEUED".into()],
            ..ListFilter::default()
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(|w| format!("{} {}", w.id, w.status))
        .collect())
}

// ---------------------------------------------------------------------------
// The workflows.

fn register(engine: &mut DurableEngine, log: RunLog) {
    let l = log.clone();
    engine.register("pipeline", move |ctx: DurableContext, (): ()| {
        let log = l.clone();
        async move {
            let wf = ctx.workflow_id().to_owned();
            let mut nonces = Nonces::new();

            nonces.push(("long-1".into(), long_step(&ctx, &log, &wf, "long-1").await?));

            let go: Option<String> = ctx.recv("gate", Duration::from_secs(3600)).await?;
            if go.as_deref() != Some("go") {
                return Err(Error::app(format!("{wf}: gate returned {go:?}")));
            }

            nonces.push(("long-2".into(), long_step(&ctx, &log, &wf, "long-2").await?));

            let mut children = Vec::new();
            for i in 0..CHILDREN {
                children.push(
                    ctx.enqueue_workflow::<_, Nonces>(
                        "work",
                        "child",
                        i,
                        WorkflowOptions::default(),
                    )
                    .await?,
                );
            }
            for child in children {
                nonces.extend(child.result().await?);
            }

            let row_wf = wf.clone();
            ctx.transaction("record", move |tx| {
                let row_wf = row_wf.clone();
                Box::pin(async move {
                    tx.execute(
                        "CREATE TABLE IF NOT EXISTS spike_effects (workflow_id TEXT NOT NULL)",
                        &params![],
                    )
                    .await?;
                    tx.execute(
                        "INSERT INTO spike_effects (workflow_id) VALUES (?)",
                        &params![row_wf],
                    )
                    .await?;
                    Ok(())
                })
            })
            .await?;

            nonces.push(("final".into(), long_step(&ctx, &log, &wf, "final").await?));
            Ok(nonces)
        }
    });

    let l = log.clone();
    engine.register("child", move |ctx: DurableContext, i: i64| {
        let log = l.clone();
        async move {
            let wf = ctx.workflow_id().to_owned();
            let step = format!("child-{i}");
            let nonce = long_step(&ctx, &log, &wf, &step).await?;
            Ok(vec![(format!("{wf}/{step}"), nonce)])
        }
    });

    engine.register("audit", move |ctx: DurableContext, (): ()| async move {
        ctx.transaction("count", |tx| {
            Box::pin(async move {
                let rows = tx
                    .query_all(
                        "SELECT workflow_id, COUNT(*) AS n FROM spike_effects GROUP BY workflow_id",
                        &params![],
                    )
                    .await?;
                rows.iter()
                    .map(|r| Ok((r.try_get::<String>("workflow_id")?, r.try_get::<i64>("n")?)))
                    .collect::<Result<Vec<_>>>()
            })
        })
        .await
    });

    engine.register(
        "ticker",
        move |ctx: DurableContext, _: serde_json::Value| {
            let log = log.clone();
            async move {
                let wf = ctx.workflow_id().to_owned();
                let nonce = {
                    let log = log.clone();
                    let wf = wf.clone();
                    ctx.step("tick", move || async move { log.run(&wf, "tick") })
                        .await?
                };
                Ok(vec![("tick".to_string(), nonce)])
            }
        },
    );
}

/// A step whose body logs a run, then takes a while, then returns its nonce.
async fn long_step(ctx: &DurableContext, log: &RunLog, wf: &str, step: &str) -> Result<String> {
    let log = log.clone();
    let wf = wf.to_owned();
    let name = step.to_owned();
    ctx.step(step, move || async move {
        let nonce = log.run(&wf, &name)?;
        tokio::time::sleep(LONG_STEP).await;
        Ok(nonce)
    })
    .await
}

// ---------------------------------------------------------------------------
// The run log: one fsync'd line per step body run.

#[derive(Clone)]
struct RunLog {
    path: Arc<PathBuf>,
}

static NONCE: AtomicU64 = AtomicU64::new(0);

impl RunLog {
    fn open(dir: &Path) -> Result<Self> {
        Ok(Self {
            path: Arc::new(dir.join("runs.log")),
        })
    }

    /// Records one run of `step` in `wf` and returns its nonce.
    fn run(&self, wf: &str, step: &str) -> Result<String> {
        let nonce = format!(
            "{}-{}",
            std::process::id(),
            NONCE.fetch_add(1, Ordering::SeqCst)
        );
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&*self.path)
            .map_err(|e| Error::app(format!("open run log: {e}")))?;
        // One write call per line, so a kill cannot leave half a line behind.
        f.write_all(format!("{wf}\t{step}\t{nonce}\n").as_bytes())
            .map_err(|e| Error::app(format!("write run log: {e}")))?;
        f.sync_all()
            .map_err(|e| Error::app(format!("sync run log: {e}")))?;
        Ok(nonce)
    }
}

// ---------------------------------------------------------------------------
// The checks.

async fn check(
    engine: &DurableEngine,
    dir: &Path,
    last_round: u32,
    tx_rows: BTreeMap<String, i64>,
) -> Result<()> {
    // Every run of every step, in log order.
    let text = std::fs::read_to_string(dir.join("runs.log"))
        .map_err(|e| Error::app(format!("read run log: {e}")))?;
    let mut runs: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for line in text.lines() {
        let mut parts = line.split('\t');
        let (Some(wf), Some(step), Some(nonce)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(Error::app(format!("malformed run log line {line:?}")));
        };
        runs.entry((wf.into(), step.into()))
            .or_default()
            .push(nonce.into());
    }

    let mut problems = Vec::new();
    let mut step_problems = Vec::new();
    let mut checked_steps = 0;
    let mut repeats_in_window = 0;
    let mut verify = |wf: &str, step: &str, recorded: &str| {
        checked_steps += 1;
        match runs.get(&(wf.to_owned(), step.to_owned())) {
            None => {
                step_problems.push(format!("{wf}/{step}: recorded {recorded} but never logged"))
            }
            Some(list) => {
                repeats_in_window += list.len() - 1;
                if list.last().map(String::as_str) != Some(recorded) {
                    step_problems.push(format!(
                        "{wf}/{step}: completed as {recorded}, then ran again: {list:?}"
                    ));
                }
            }
        }
    };

    let all = engine.list_workflows(&ListFilter::default()).await?;
    let mut by_name: BTreeMap<String, usize> = BTreeMap::new();
    for w in &all {
        *by_name.entry(w.name.clone()).or_default() += 1;
        if w.status != "SUCCESS" {
            problems.push(format!(
                "{} ({}) ended {}: {:?}",
                w.id, w.name, w.status, w.error
            ));
            continue;
        }
        if w.name != "pipeline" && w.name != "ticker" {
            continue;
        }
        let nonces: Nonces = serde_json::from_value(w.output.clone().unwrap_or_default())
            .map_err(|e| Error::app(format!("{} output: {e}", w.id)))?;
        match w.name.as_str() {
            "pipeline" => {
                for (step, nonce) in &nonces {
                    match step.split_once('/') {
                        // A child's step, reported through the pipeline.
                        Some((child, child_step)) => verify(child, child_step, nonce),
                        None => verify(&w.id, step, nonce),
                    }
                }
                let expected = 3 + CHILDREN as usize;
                if nonces.len() != expected {
                    problems.push(format!(
                        "{}: {} step results, expected {expected}",
                        w.id,
                        nonces.len()
                    ));
                }
            }
            "ticker" => {
                for (step, nonce) in &nonces {
                    verify(&w.id, step, nonce);
                }
            }
            _ => {}
        }
    }

    problems.extend(step_problems);

    let expected_pipelines = (last_round as usize + 1) * PIPELINES_PER_ROUND;
    let pipelines = by_name.get("pipeline").copied().unwrap_or(0);
    if pipelines != expected_pipelines {
        problems.push(format!(
            "{pipelines} pipelines, expected {expected_pipelines}"
        ));
    }

    // Transaction steps: exactly one committed row per pipeline.
    for w in all.iter().filter(|w| w.name == "pipeline") {
        match tx_rows.get(&w.id) {
            Some(1) => {}
            other => problems.push(format!("{}: transaction rows {other:?}, expected 1", w.id)),
        }
    }

    println!(
        "workflows by name: {by_name:?}; steps checked: {checked_steps}; \
         re-runs inside the at-least-once window: {repeats_in_window}"
    );
    if problems.is_empty() {
        println!("SPIKE OK");
        Ok(())
    } else {
        for p in &problems {
            println!("PROBLEM: {p}");
        }
        Err(Error::app(format!("{} problems", problems.len())))
    }
}

// ---------------------------------------------------------------------------

/// A small deterministic generator, so a seed reproduces a run's kill timings.
struct XorShift(u64);

impl XorShift {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}
