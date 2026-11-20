// SPDX-License-Identifier: Apache-2.0

//! Gates across process termination, with no work graph in the build:
//!
//! - a child process opens a gate and is killed while it waits; a new child
//!   recovers the run, the gate keeps its original deadline, and a decision
//!   made from a second process (this test, standing in for a second
//!   terminal) lets the recovered run pass;
//! - a deadline that passes while no process runs rejects on recovery.
//!
//! The children are this test binary run again with an ignored test
//! selected; the parent kills them with `SIGKILL`.

use atomic_durable::gate::{
    DecisionCommand, Deployment, Gate, GateSpec, HumanCredential, OnTimeout, Principal, Resolution,
    Tier, Verdict, decide, enroll, find, gate, gate_id, resolve,
};
use atomic_durable::{DurableRuntime, Durare, DurareContext};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const ENV_ROLE: &str = "ATOMIC_GATES_ROLE";
const ENV_STORE: &str = "ATOMIC_GATES_STORE";
const ENV_RUN: &str = "ATOMIC_GATES_RUN";
const ENV_TIMEOUT: &str = "ATOMIC_GATES_TIMEOUT_MS";

/// Where the children's store is: a SQLite file, or a PostgreSQL schema.
#[derive(Clone, Debug)]
enum Store {
    Sqlite(PathBuf),
    #[cfg_attr(not(feature = "postgres-tests"), allow(dead_code))]
    Postgres(String, String),
}

impl Store {
    fn encode(&self) -> String {
        match self {
            Self::Sqlite(p) => format!("sqlite:{}", p.display()),
            Self::Postgres(url, schema) => format!("postgres:{schema}:{url}"),
        }
    }

    fn decode(s: &str) -> Self {
        if let Some(p) = s.strip_prefix("sqlite:") {
            return Self::Sqlite(PathBuf::from(p));
        }
        let rest = s.strip_prefix("postgres:").expect("a store");
        let (schema, url) = rest.split_once(':').expect("schema:url");
        Self::Postgres(url.to_owned(), schema.to_owned())
    }

    async fn open(&self) -> Durare {
        match self {
            Self::Sqlite(p) => Durare::sqlite(p).await.expect("sqlite engine"),
            Self::Postgres(url, schema) => Durare::postgres(url, schema)
                .await
                .expect("postgres engine"),
        }
    }
}

fn unique(tag: &str) -> String {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{tag}_{}_{n}_{nanos}", std::process::id())
}

fn sqlite_store() -> Store {
    let dir = std::env::temp_dir().join(unique("atomic_gates_kill"));
    std::fs::create_dir_all(&dir).expect("scratch");
    Store::Sqlite(dir.join("atomic.db"))
}

/// A store on a schema of its own for the parent and its child processes;
/// the parent's guard drops it, whatever the children did.
#[cfg(feature = "postgres-tests")]
async fn postgres_store() -> (Store, atomic_durable::testing::TestSchema) {
    // A suite that cannot reach its backend fails; it never skips.
    let schema = atomic_durable::testing::schema("gates_kill").await;
    (
        Store::Postgres(schema.url().to_owned(), schema.name().to_owned()),
        schema,
    )
}

fn operator() -> Principal {
    Principal::local("dev")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Gated {
    timeout_ms: u64,
}

fn register(rt: &mut Durare) {
    rt.register("gated", |ctx: DurareContext, input: Gated| async move {
        let spec = GateSpec {
            workflow: "gated".into(),
            title: "Ship the release".into(),
            show: json!({"version": "1.2.3"}),
            evidence: Vec::new(),
            approvers: resolve(
                "ship",
                None,
                &Deployment::Local {
                    operator: operator(),
                },
            )
            .map_err(|e| atomic_durable::DurableError::Failed(e.to_string()))?,
            tier: Tier::High,
            timeout: Some(Duration::from_millis(input.timeout_ms)),
            on_timeout: OnTimeout::Reject,
        };
        gate(&ctx, "ship", spec).await
    });
}

/// Not a test on its own: the parent runs it in a child process, which
/// starts the run (role `start`) or recovers it (role `recover`), waits for
/// its result and writes it next to the store.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "run by the kill tests in a child process"]
async fn gate_child() {
    let (Ok(role), Ok(store), Ok(run), Ok(timeout)) = (
        std::env::var(ENV_ROLE),
        std::env::var(ENV_STORE),
        std::env::var(ENV_RUN),
        std::env::var(ENV_TIMEOUT),
    ) else {
        return;
    };
    let store = Store::decode(&store);
    let mut rt = store.open().await;
    register(&mut rt);
    rt.launch().await.expect("launch");
    if role == "start" {
        let timeout_ms = timeout.parse().expect("a timeout");
        rt.start("gated", &run, Gated { timeout_ms })
            .await
            .expect("start");
    } else {
        rt.recover().await.expect("recover");
    }
    let out: Resolution = rt.result(&run).await.expect("finishes");
    std::fs::write(
        result_path(&run),
        serde_json::to_string(&out).expect("encode"),
    )
    .expect("write the result");
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

fn result_path(run: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{run}.result.json"))
}

fn child(store: &Store, run: &str, role: &str, timeout_ms: u64) -> Child {
    Command::new(std::env::current_exe().expect("this test binary"))
        .args(["--exact", "gate_child", "--ignored", "--nocapture"])
        .env(ENV_ROLE, role)
        .env(ENV_STORE, store.encode())
        .env(ENV_RUN, run)
        .env(ENV_TIMEOUT, timeout_ms.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("run the child")
}

/// Wait until gate `id` is in the store, as the second process sees it.
async fn until_open(rt: &Durare, id: &str) -> Gate {
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(g) = find(rt, id).await.expect("find") {
            return g;
        }
        assert!(Instant::now() < until, "gate {id} never opened");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Wait for a child to exit and return its result.
async fn finished(mut c: Child, run: &str) -> Resolution {
    let until = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = c.try_wait().expect("wait") {
            break status;
        }
        if Instant::now() > until {
            let _ = c.kill();
            panic!("the child running {run} did not finish");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(status.success(), "the child finished: {status}");
    serde_json::from_str(&std::fs::read_to_string(result_path(run)).expect("result"))
        .expect("decode")
}

/// Kill the child that started `run` once its gate is open, and return the
/// gate as the store had it.
async fn start_and_kill(store: &Store, second: &Durare, run: &str, timeout_ms: u64) -> Gate {
    let mut first = child(store, run, "start", timeout_ms);
    let opened = until_open(second, &gate_id(run, "ship")).await;
    // Let it reach its wait, then kill it there.
    tokio::time::sleep(Duration::from_millis(200)).await;
    first.kill().expect("kill");
    let status = first.wait().expect("wait");
    assert!(!status.success(), "the first process was killed: {status}");
    assert!(!result_path(run).exists());
    opened
}

async fn a_gate_survives_a_kill_and_is_approved_from_a_second_process(store: Store) {
    let run = unique("ship");
    // The second terminal: the same store, opened by another process, which
    // never runs workflows itself.
    let second = store.open().await;
    let opened = start_and_kill(&store, &second, &run, 3_600_000).await;
    let deadline = opened.deadline.expect("a deadline");

    let recovering = child(&store, &run, "recover", 0);
    // Give the recovery time to start waiting again; a decision before then
    // waits in the run's mailbox, so either order works.
    tokio::time::sleep(Duration::from_millis(500)).await;
    // The second terminal is the operator's, with their credential.
    let credential = HumanCredential::new(operator(), "the operator's secret");
    enroll(&second, &credential).await.expect("enroll");
    let decision = decide(
        &second,
        &credential,
        DecisionCommand {
            command_id: unique("cmd"),
            gate: opened.id.clone(),
            verdict: Verdict::Approve,
            comment: None,
            proposal: opened.proposal.clone(),
        },
    )
    .await
    .expect("decided");

    let out = finished(recovering, &run).await;
    assert_eq!(out, Resolution::Decided(decision));
    let stored = find(&second, &opened.id)
        .await
        .expect("find")
        .expect("gate");
    assert_eq!(stored.deadline, Some(deadline), "the original deadline");
    assert_eq!(stored.opened_at, opened.opened_at);
    second
        .shutdown(Duration::from_secs(5))
        .await
        .expect("shutdown");
}

async fn a_deadline_that_passes_while_offline_rejects_on_recovery(store: Store) {
    let run = unique("ship");
    let second = store.open().await;
    let opened = start_and_kill(&store, &second, &run, 1_500).await;
    let deadline = opened.deadline.expect("a deadline");

    // Nothing runs while the deadline passes.
    let wait = deadline - now_ms() + 300;
    tokio::time::sleep(Duration::from_millis(u64::try_from(wait).unwrap_or(0))).await;
    let still = find(&second, &opened.id)
        .await
        .expect("find")
        .expect("gate");
    assert_eq!(still.resolution, None, "nothing has looked at it yet");

    let out = finished(child(&store, &run, "recover", 0), &run).await;
    let Resolution::Expired { at, on_timeout } = out.clone() else {
        panic!("expected an expiry, got {out:?}");
    };
    assert!(at >= deadline);
    assert_eq!(on_timeout, OnTimeout::Reject);
    assert!(!out.approved());
    second
        .shutdown(Duration::from_secs(5))
        .await
        .expect("shutdown");
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_a_gate_survives_a_kill_and_is_approved_from_a_second_process() {
    a_gate_survives_a_kill_and_is_approved_from_a_second_process(sqlite_store()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_a_deadline_that_passes_while_offline_rejects_on_recovery() {
    a_deadline_that_passes_while_offline_rejects_on_recovery(sqlite_store()).await;
}

#[cfg(feature = "postgres-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_a_gate_survives_a_kill_and_is_approved_from_a_second_process() {
    let (store, _schema) = postgres_store().await;
    a_gate_survives_a_kill_and_is_approved_from_a_second_process(store).await;
}

#[cfg(feature = "postgres-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_a_deadline_that_passes_while_offline_rejects_on_recovery() {
    let (store, _schema) = postgres_store().await;
    a_deadline_that_passes_while_offline_rejects_on_recovery(store).await;
}
