// SPDX-License-Identifier: Apache-2.0

//! Budgets on the change-request workflow, against the scripted fake model
//! and the durable usage ledger:
//!
//! - every model call has a ledger attempt, made before the call and settled
//!   once with what the provider reported, the repair turn's included;
//! - each call is priced at settlement, and the dollars charged are what the
//!   calls cost;
//! - a cap with no room for the next call parks the run before the call is
//!   made; a rejected answer's spend is kept; releasing the park admits the
//!   call afresh against the caps as stored, so it parks again until a
//!   person raises the cap, and then the run goes on;
//! - a cap smaller than one call's reservation fails configuration;
//! - killing the process after the provider was called and before the usage
//!   was settled, again and again, leaves each attempt charged at its bound
//!   until admission parks the run at the cap; a resume with a larger budget
//!   configured, released, parks again; once the attempts are reconciled the
//!   run goes on;
//! - killing the process just after the park is recorded keeps one park;
//! - a run killed and resumed reports the same usage as an uninterrupted
//!   one, plus only the calls that really ran again;
//! - two child runs racing for their parent's allowance cannot both take
//!   its last reservation, and the one refused goes on once its park is
//!   released;
//! - a lite call that failed is charged at its bound if it could have been
//!   sent, as is an answer with no usage, and released if it never was;
//! - a dollar cap on an unpriced model fails configuration, and a call to
//!   a model the budget does not price is not made under one;
//! - a Codex turn reserves the active time its atom has left, not one
//!   request's bound, and a turn that runs out of it parks the run.

// Codex's turn futures nest deeply inside the workflow's; proving them Send
// needs more than the default query depth.
#![recursion_limit = "512"]

use atomic_app::metering::RunBudget;
use atomic_app::workflows::{
    CHANGE_REQUEST, ChangeRequestResult, ChangeRequestRun, propose, register_change_request,
};
use atomic_atoms::{ChangeRequest, DraftTask};
use atomic_core::AtomConfig;
use atomic_core::budget::{Budget, BudgetError, BudgetSettings, CallBound, Level, Price, resolve};
use atomic_core::codex::{CodexPaths, ModelEndpoint};
use atomic_core::lite::LiteRuntime;
use atomic_core::runtime::{Artifacts, CodexRuntime, Runtimes, run_lite_atom};
use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use atomic_core::usage::Metering;
use atomic_durable::usage::{Attempt, Caps, Settlement};
use atomic_durable::{DurableRuntime, Durare, RunStatus, parks, release, run_atom_at};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const DRAFT: &str = r#"{"title": "Charge for every unit in the cart", "priority": 1, "criteria": ["Two units at 125 cents total 250 cents."]}"#;
const REVIEW: &str =
    r#"{"verdict": "approve", "summary": "The two-unit criterion holds.", "findings": []}"#;

/// What one call reserves in these tests: 100 input and 50 output tokens.
const BOUND: i64 = 150;

fn input() -> ChangeRequestRun {
    ChangeRequestRun {
        request: "Two units at 1.25 should cost 2.50.".into(),
        base: "1".repeat(40),
        commit: "2".repeat(40),
    }
}

/// Budget settings with a small call bound, an atom cap of `atom` tokens
/// and a run cap of `run` tokens.
fn settings(atom: u64, run: u64) -> BudgetSettings {
    let mut s = BudgetSettings {
        call: CallBound {
            input_tokens: 100,
            output_tokens: 50,
            minutes: 1.0,
        },
        ..BudgetSettings::default()
    };
    s.atom.tokens = atom;
    s.workflow.tokens = run;
    s
}

fn scratch(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("atomic-budgets-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["home", "cwd"] {
        std::fs::create_dir_all(dir.join(sub)).expect("scratch");
    }
    dir
}

fn runtimes(dir: &Path, base_url: &str) -> Arc<Runtimes> {
    let endpoint = ModelEndpoint {
        base_url: base_url.to_owned(),
        model: "synthetic".into(),
        api_key_env: None,
    };
    Arc::new(Runtimes {
        codex: CodexRuntime {
            paths: CodexPaths::without_helpers(),
            codex_home: dir.join("home"),
            guard: atomic_core::codex::StateGuard::NoProjectState,
            cwd: dir.join("cwd"),
            endpoint: endpoint.clone(),
            defaults: AtomConfig::default(),
            artifacts: Artifacts::none(),
            meter: Metering::none(),
            backend: Default::default(),
            caches: None,
        },
        lite: LiteRuntime::new(endpoint, AtomConfig::default()),
        exec: Default::default(),
        exec_meter: Default::default(),
        clock: None,
    })
}

async fn engine(dir: &Path, base_url: &str, budgets: BudgetSettings) -> Durare {
    let mut rt = Durare::sqlite(&dir.join("atomic.db"))
        .await
        .expect("engine");
    register_change_request(&mut rt, runtimes(dir, base_url), Arc::new(budgets))
        .expect("the budgets resolve");
    rt
}

/// How a run stood when [`outcome`] stopped watching it.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Finished,
    Parked(String),
}

/// Watch run `id` until it finishes or parks.
async fn outcome(rt: &Durare, id: &str) -> Outcome {
    parked(rt, id, 1).await
}

/// Watch run `id` until it finishes or has parked `n` times.
async fn parked(rt: &Durare, id: &str, n: usize) -> Outcome {
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        match rt.status(id).await.expect("status") {
            Some(RunStatus::Succeeded) => return Outcome::Finished,
            Some(RunStatus::Failed(e)) => panic!("run {id} failed: {e}"),
            _ => {}
        }
        if let Some(p) = parks(rt, id).await.expect("parks").into_iter().nth(n - 1) {
            return Outcome::Parked(p.reason);
        }
        assert!(
            Instant::now() < until,
            "run {id} neither finished nor parked"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn attempts(rt: &Durare, run: &str) -> Vec<Attempt> {
    rt.ledger().attempts(run).await.expect("attempts")
}

async fn charged(rt: &Durare, scope: &str) -> i64 {
    rt.ledger()
        .allowance(scope)
        .await
        .expect("allowance")
        .map_or(0, |a| a.charged.tokens)
}

/// The request ids of `attempts`, with the run taken off: `<step>:<call>`.
fn requests(run: &str, attempts: &[Attempt]) -> Vec<(String, i64, String)> {
    attempts
        .iter()
        .map(|a| {
            (
                a.request
                    .strip_prefix(&format!("{run}:"))
                    .unwrap_or(&a.request)
                    .to_owned(),
                a.attempt,
                a.state.clone(),
            )
        })
        .collect()
}

#[test]
fn every_model_call_has_an_attempt_admitted_first_and_settled_once() {
    on_codex_runtime(async {
        // The draft's first answer does not fit, so it takes a repair turn.
        let model = FakeModel::start(vec![
            Reply::Text("not json".into()),
            Reply::Text(DRAFT.into()),
            Reply::Text(REVIEW.into()),
        ])
        .await
        .expect("fake model");
        let dir = scratch("every");
        // Priced at one and two dollars per million input and output tokens:
        // one input and one output token cost three micro-dollars, and a
        // whole bound 100 + 2 * 50.
        let mut priced = settings(10_000, 100_000);
        priced.pricing.insert(
            "synthetic".into(),
            Price {
                input_usd_per_mtok: 1.0,
                output_usd_per_mtok: 2.0,
                max_input_tokens: None,
                max_output_tokens: None,
            },
        );
        let rt = engine(&dir, &model.base_url(), priced).await;
        rt.launch().await.expect("launch");
        rt.start(CHANGE_REQUEST, "every", input())
            .await
            .expect("start");
        let out: ChangeRequestResult = rt.result("every").await.expect("finishes");
        assert_eq!(out.verdict.summary, "The two-unit criterion holds.");
        assert_eq!(model.calls(), 3);

        // One attempt per call, each settled with the fake's usage (one
        // input and one output token): the draft's two turns, then the
        // review's one, keyed by step position and call index.
        let journal = attempts(&rt, "every").await;
        assert_eq!(
            requests("every", &journal),
            [
                ("0:0".to_owned(), 1, "settled".to_owned()),
                ("0:1".to_owned(), 1, "settled".to_owned()),
                ("2:0".to_owned(), 1, "settled".to_owned()),
            ]
        );
        for a in &journal {
            assert_eq!(a.usage, Some((1, 1)), "{a:?}");
            assert_eq!(a.reserved.tokens, BOUND, "the whole bound was reserved");
            assert_eq!(a.reserved.micro_usd, 200, "priced at the model's rate");
            assert_eq!(a.charged.micro_usd, 3, "settled at what it cost");
        }
        // A lite call's request is larger than this tiny call bound's 100
        // input tokens, and its maximum says so; a Codex turn's input is
        // not bounded by its caller, so its maximum is the bound.
        assert!(journal[0].excess_bound() > 0, "{:?}", journal[0]);
        assert_eq!(journal[2].excess_bound(), 0, "{:?}", journal[2]);
        // Each call was sent with the bound's output tokens as its limit.
        assert_eq!(model.requests()[0]["max_tokens"], 50);
        assert_eq!(charged(&rt, "run:every").await, 6);
        assert_eq!(
            rt.ledger()
                .allowance("run:every")
                .await
                .expect("allowance")
                .expect("exists")
                .charged
                .micro_usd,
            9,
            "three calls at three micro-dollars each"
        );
        assert_eq!(charged(&rt, "atom:every:0").await, 4);
        assert_eq!(charged(&rt, "atom:every:2").await, 2);
        rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    });
}

#[test]
fn a_cap_parks_the_run_before_the_call_and_keeps_rejected_spend() {
    on_codex_runtime(async {
        // Room for one call to the draft: its answer does not fit, and the
        // repair turn finds no room.
        let model = FakeModel::start(vec![Reply::Text("not json".into())])
            .await
            .expect("fake model");
        let dir = scratch("cap");
        let rt = engine(&dir, &model.base_url(), settings(150, 100_000)).await;
        rt.launch().await.expect("launch");
        rt.start(CHANGE_REQUEST, "capped", input())
            .await
            .expect("start");
        let Outcome::Parked(reason) = outcome(&rt, "capped").await else {
            panic!("the run should park");
        };
        assert!(reason.contains("atom:capped:0"), "{reason}");
        assert_eq!(model.calls(), 1, "the repair turn was not dispatched");
        assert_eq!(
            requests("capped", &attempts(&rt, "capped").await),
            [("0:0".to_owned(), 1, "settled".to_owned())],
            "the rejected answer's spend is kept"
        );
        assert_eq!(charged(&rt, "atom:capped:0").await, 2);
        // It parked instead of continuing: no review step.
        let names: Vec<String> = rt
            .steps("capped")
            .await
            .expect("steps")
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names[..2], ["draft.turn", "draft.park"]);
        assert!(!names.iter().any(|n| n.starts_with("review")), "{names:?}");
        assert_eq!(
            rt.status("capped").await.expect("status"),
            Some(RunStatus::Pending)
        );
        rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    });
}

#[test]
fn a_released_park_admits_again_against_the_caps_as_stored() {
    on_codex_runtime(async {
        // Room for one call to the draft: its answer does not fit, and the
        // repair turn finds no room. Released, it finds none again; once a
        // person raises the cap, the draft and the review run.
        let model = FakeModel::start(vec![
            Reply::Text("not json".into()),
            Reply::Text(DRAFT.into()),
            Reply::Text(REVIEW.into()),
        ])
        .await
        .expect("fake model");
        let dir = scratch("release");
        let rt = engine(&dir, &model.base_url(), settings(150, 100_000)).await;
        rt.launch().await.expect("launch");
        rt.start(CHANGE_REQUEST, "released", input())
            .await
            .expect("start");
        assert!(matches!(outcome(&rt, "released").await, Outcome::Parked(_)));
        assert_eq!(model.calls(), 1);

        // Released with no more room: admission refuses again, with no call.
        let first = parks(&rt, "released").await.expect("parks")[0].clone();
        release(&rt, &first).await.expect("release");
        let Outcome::Parked(again) = parked(&rt, "released", 2).await else {
            panic!("parks again");
        };
        assert!(again.contains("atom:released:0"), "{again}");
        assert_eq!(model.calls(), 1, "no call without room");

        // Raised by a person and released: the turn runs again, from its
        // first call, as new attempts at the same requests.
        rt.ledger()
            .raise(
                "atom:released:0",
                Caps {
                    tokens: 1_000,
                    millis: 0,
                    micro_usd: None,
                },
            )
            .await
            .expect("raise")
            .expect("exists");
        let second = parks(&rt, "released").await.expect("parks")[1].clone();
        release(&rt, &second).await.expect("release");
        let out: ChangeRequestResult = rt.result("released").await.expect("finishes");
        assert_eq!(out.verdict.summary, "The two-unit criterion holds.");
        assert_eq!(model.calls(), 3);
        assert_eq!(
            requests("released", &attempts(&rt, "released").await)[..2],
            [
                ("0:0".to_owned(), 1, "settled".to_owned()),
                ("0:0".to_owned(), 2, "settled".to_owned()),
            ],
            "the re-run is a new attempt at the first turn's request"
        );
        assert_eq!(charged(&rt, "atom:released:0").await, 4);
        rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    });
}

#[test]
fn a_cap_smaller_than_one_call_fails_configuration() {
    on_codex_runtime(async {
        let dir = scratch("none");
        let mut rt = Durare::sqlite(&dir.join("atomic.db"))
            .await
            .expect("engine");
        // Each call reserves 150 tokens; an atom cap of 100 never admits one.
        let refused = register_change_request(
            &mut rt,
            runtimes(&dir, "http://127.0.0.1:9/v1"),
            Arc::new(settings(100, 100_000)),
        )
        .expect_err("a cap below one call");
        let message = refused.to_string();
        assert!(matches!(refused, BudgetError::BelowCall(_)), "{refused:?}");
        assert!(
            message.contains("budget.call.input_tokens + budget.call.output_tokens"),
            "{message}"
        );
        // The run's cap is held to the same rule.
        assert!(
            register_change_request(
                &mut rt,
                runtimes(&dir, "http://127.0.0.1:9/v1"),
                Arc::new(settings(10_000, 149)),
            )
            .is_err()
        );
    });
}

// ---------------------------------------------------------------------------
// Crash tests: the run in a child process, killed at a named failpoint.

const ENV_ROLE: &str = "ATOMIC_BUDGETS_ROLE";
const ENV_DIR: &str = "ATOMIC_BUDGETS_DIR";
const ENV_URL: &str = "ATOMIC_BUDGETS_URL";
const ENV_RUN: &str = "ATOMIC_BUDGETS_RUN";
const ENV_CAPS: &str = "ATOMIC_BUDGETS_CAPS";
const ENV_PARKS: &str = "ATOMIC_BUDGETS_PARKS";
/// `<call minutes>,<atom minutes>`, for a child with those time caps.
const ENV_TIMES: &str = "ATOMIC_BUDGETS_TIMES";
/// `<input tokens>,<output tokens>`, for a child with that call bound.
const ENV_CALL: &str = "ATOMIC_BUDGETS_CALL";
/// `<request>,<input tokens>,<output tokens>`: once that request's attempt
/// has recorded that usage, the child stops as atomic does on a signal,
/// settling the calls it has in flight as interrupted, and exits
/// [`STOPPED`].
const ENV_STOP_AT: &str = "ATOMIC_BUDGETS_STOP_AT";
/// How a child stopped by [`ENV_STOP_AT`] exits.
const STOPPED: i32 = 3;

/// Not a test on its own: the crash tests run it in a child process.
#[test]
#[ignore = "run by the crash tests in a child process"]
fn budgets_child() {
    let (Ok(role), Ok(dir), Ok(url), Ok(run), Ok(caps)) = (
        std::env::var(ENV_ROLE),
        std::env::var(ENV_DIR),
        std::env::var(ENV_URL),
        std::env::var(ENV_RUN),
        std::env::var(ENV_CAPS),
    ) else {
        return;
    };
    let (atom, total) = caps.split_once(',').expect("caps");
    let mut budgets = settings(
        atom.parse().expect("atom cap"),
        total.parse().expect("run cap"),
    );
    if let Some((call, atom)) = std::env::var(ENV_TIMES)
        .ok()
        .and_then(|t| t.split_once(',').map(|(c, a)| (c.to_owned(), a.to_owned())))
    {
        budgets.call.minutes = call.parse().expect("call minutes");
        budgets.atom.minutes = atom.parse().expect("atom minutes");
    }
    if let Some((input, output)) = std::env::var(ENV_CALL)
        .ok()
        .and_then(|t| t.split_once(',').map(|(i, o)| (i.to_owned(), o.to_owned())))
    {
        budgets.call.input_tokens = input.parse().expect("call input tokens");
        budgets.call.output_tokens = output.parse().expect("call output tokens");
    }
    on_codex_runtime(async move {
        let dir = PathBuf::from(dir);
        let rt = engine(&dir, &url, budgets).await;
        rt.launch().await.expect("launch");
        if let Ok(stop) = std::env::var(ENV_STOP_AT) {
            let mut parts = stop.split(',');
            let (request, usage) = (
                parts.next().expect("request").to_owned(),
                (
                    parts.next().and_then(|n| n.parse().ok()).expect("input"),
                    parts.next().and_then(|n| n.parse().ok()).expect("output"),
                ),
            );
            let ledger = rt.ledger();
            let watched = run.clone();
            tokio::spawn(async move {
                loop {
                    let reported = ledger
                        .attempts(&watched)
                        .await
                        .ok()
                        .and_then(|a| a.into_iter().find(|a| a.request == request))
                        .and_then(|a| a.usage);
                    if reported == Some(usage) {
                        atomic_app::metering::interrupt_in_flight().await;
                        std::process::exit(STOPPED);
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            });
        }
        if role == "start" {
            rt.start(CHANGE_REQUEST, &run, input())
                .await
                .expect("start");
        } else {
            rt.recover().await.expect("recover");
        }
        let n = std::env::var(ENV_PARKS)
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(1);
        let seen = match parked(&rt, &run, n).await {
            Outcome::Finished => "finished".to_owned(),
            Outcome::Parked(reason) => format!("parked: {reason}"),
        };
        std::fs::write(dir.join("outcome"), seen).expect("write the outcome");
        rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    });
}

/// Run the child as `role` on run `run`, with caps `atom,run`, killed at
/// `failpoint` if one is given. Returns whether it exited cleanly, and what
/// it saw if it did.
async fn child(
    dir: &Path,
    url: &str,
    run: &str,
    role: &str,
    caps: &str,
    failpoint: Option<&str>,
) -> Option<String> {
    child_until(dir, url, run, role, caps, failpoint, 1).await
}

/// The child as `role` on run `run` with caps `atom,run`, started and left
/// running, for a test to kill when it chooses.
fn spawn_child(dir: &Path, url: &str, run: &str, role: &str, caps: &str) -> std::process::Child {
    spawn_child_with(dir, url, run, role, caps, "")
}

/// As [`spawn_child`], with time caps `times` (see [`ENV_TIMES`]), if any.
fn spawn_child_with(
    dir: &Path,
    url: &str,
    run: &str,
    role: &str,
    caps: &str,
    times: &str,
) -> std::process::Child {
    let _ = std::fs::remove_file(dir.join("outcome"));
    let mut cmd = Command::new(std::env::current_exe().expect("this test binary"));
    if !times.is_empty() {
        cmd.env(ENV_TIMES, times);
    }
    cmd.args(["--exact", "budgets_child", "--ignored", "--nocapture"])
        .env(ENV_ROLE, role)
        .env(ENV_DIR, dir)
        .env(ENV_URL, url)
        .env(ENV_RUN, run)
        .env(ENV_CAPS, caps)
        .env(ENV_PARKS, "1")
        .env_remove("ATOMIC_FAILPOINT")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the child")
}

/// Run the child as `role` with time caps `times` to its end, and return
/// what it saw, if it exited cleanly.
async fn child_timed(
    dir: &Path,
    url: &str,
    run: &str,
    role: &str,
    caps: &str,
    times: &str,
) -> Option<String> {
    let mut process = spawn_child_with(dir, url, run, role, caps, times);
    let status = tokio::task::spawn_blocking(move || process.wait().expect("the child"))
        .await
        .expect("child");
    if status.success() {
        Some(std::fs::read_to_string(dir.join("outcome")).expect("the child's outcome"))
    } else {
        None
    }
}

/// As [`child`], watching for the `parks`th park.
async fn child_until(
    dir: &Path,
    url: &str,
    run: &str,
    role: &str,
    caps: &str,
    failpoint: Option<&str>,
    parks: usize,
) -> Option<String> {
    let _ = std::fs::remove_file(dir.join("outcome"));
    let mut cmd = Command::new(std::env::current_exe().expect("this test binary"));
    cmd.args(["--exact", "budgets_child", "--ignored", "--nocapture"])
        .env(ENV_ROLE, role)
        .env(ENV_DIR, dir)
        .env(ENV_URL, url)
        .env(ENV_RUN, run)
        .env(ENV_CAPS, caps)
        .env(ENV_PARKS, parks.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match failpoint {
        Some(f) => cmd.env("ATOMIC_FAILPOINT", f),
        None => cmd.env_remove("ATOMIC_FAILPOINT"),
    };
    let status = tokio::task::spawn_blocking(move || cmd.status().expect("run the child"))
        .await
        .expect("child");
    if status.success() {
        Some(std::fs::read_to_string(dir.join("outcome")).expect("the child's outcome"))
    } else {
        None
    }
}

/// How long after its admission the running attempt's last heartbeat was
/// recorded, in milliseconds, or 0 before it has one.
async fn beat_after_admission(dir: &Path) -> i64 {
    let Ok(db) =
        sqlx::SqlitePool::connect(&format!("sqlite://{}", dir.join("atomic.db").display())).await
    else {
        return 0;
    };
    let seen: Option<i64> = sqlx::query_scalar(
        "SELECT MAX(h.seen_at - a.admitted_at) FROM atomic_usage_attempts a \
         JOIN atomic_usage_heartbeats h ON h.request_id = a.request_id AND h.attempt = a.attempt \
         WHERE a.state = 'reserved'",
    )
    .fetch_one(&db)
    .await
    .unwrap_or(None);
    db.close().await;
    seen.unwrap_or(0)
}

async fn reader(dir: &Path) -> Durare {
    Durare::sqlite(&dir.join("atomic.db"))
        .await
        .expect("engine")
}

#[test]
fn killing_after_the_call_and_before_settlement_ends_parked_at_the_cap() {
    on_codex_runtime(async {
        // Three drafts for the killed calls, then the run that goes on.
        let model = FakeModel::start(vec![
            Reply::Text(DRAFT.into()),
            Reply::Text(DRAFT.into()),
            Reply::Text(DRAFT.into()),
            Reply::Text(DRAFT.into()),
            Reply::Text(REVIEW.into()),
        ])
        .await
        .expect("fake model");
        let dir = scratch("unsettled");
        let url = model.base_url();
        // The draft's allowance has room for three whole bounds.
        let caps = "450,100000";

        // Each process calls the provider for the draft, then dies before
        // the usage is recorded.
        let mut role = "start";
        for kill in 1..=3 {
            let seen = child(
                &dir,
                &url,
                "unsettled",
                role,
                caps,
                Some("usage:before-settle:unsettled:0:0"),
            )
            .await;
            assert_eq!(seen, None, "process {kill} was killed");
            assert_eq!(model.calls(), kill, "one more call, the re-run's");
            role = "recover";
        }
        // The next recovery finds no room for a fourth attempt: it parks,
        // with no call made.
        let seen = child(
            &dir,
            &url,
            "unsettled",
            "recover",
            caps,
            Some("usage:before-settle:unsettled:0:0"),
        )
        .await
        .expect("the recovery parked instead of dying");
        assert!(seen.starts_with("parked: "), "{seen}");
        assert!(seen.contains("450 of 450 tokens"), "{seen}");
        assert_eq!(model.calls(), 3);

        let rt = reader(&dir).await;
        // Every attempt stays charged at its bound: recovery released none.
        assert_eq!(
            requests("unsettled", &attempts(&rt, "unsettled").await),
            [
                ("0:0".to_owned(), 1, "reserved".to_owned()),
                ("0:0".to_owned(), 2, "reserved".to_owned()),
                ("0:0".to_owned(), 3, "reserved".to_owned()),
            ]
        );
        assert_eq!(charged(&rt, "atom:unsettled:0").await, 3 * BOUND);
        assert_eq!(charged(&rt, "run:unsettled").await, 3 * BOUND);
        drop(rt);

        // Resuming leaves it parked, even with ten times the budget
        // configured: no call, no attempt, the same one park.
        let seen = child(&dir, &url, "unsettled", "recover", "4500,1000000", None)
            .await
            .expect("the resumed run");
        assert!(seen.starts_with("parked: "), "{seen}");
        assert_eq!(model.calls(), 3);
        let rt = reader(&dir).await;
        assert_eq!(attempts(&rt, "unsettled").await.len(), 3);
        assert_eq!(parks(&rt, "unsettled").await.expect("parks").len(), 1);
        assert_eq!(
            rt.ledger()
                .allowance("atom:unsettled:0")
                .await
                .expect("allowance")
                .expect("exists")
                .caps
                .tokens,
            450,
            "resuming did not raise the cap"
        );

        // Released, the resumed run with ten times the budget configured
        // admits its call afresh, against the cap as stored: it parks again,
        // still with no call.
        let first = parks(&rt, "unsettled").await.expect("parks")[0].clone();
        release(&rt, &first).await.expect("release");
        drop(rt);
        let seen = child_until(&dir, &url, "unsettled", "recover", "4500,1000000", None, 2)
            .await
            .expect("the released run");
        assert!(seen.contains("450 of 450 tokens"), "{seen}");
        assert_eq!(model.calls(), 3);

        // A person enters the three attempts' real usage;
        // released again, the run goes on.
        let rt = reader(&dir).await;
        for a in rt.ledger().unsettled("unsettled").await.expect("unsettled") {
            rt.ledger()
                .settle(
                    &a.request,
                    a.attempt,
                    Settlement::Reconciled {
                        input_tokens: 1,
                        output_tokens: 1,
                        micro_usd: 0,
                    },
                )
                .await
                .expect("reconcile");
        }
        let second = parks(&rt, "unsettled").await.expect("parks")[1].clone();
        release(&rt, &second).await.expect("release");
        drop(rt);
        // Watching past the two parks it has, for the run to finish.
        let seen = child_until(&dir, &url, "unsettled", "recover", caps, None, 3)
            .await
            .expect("the reconciled run");
        assert_eq!(seen, "finished");
        assert_eq!(model.calls(), 5);
        let rt = reader(&dir).await;
        assert_eq!(charged(&rt, "atom:unsettled:0").await, 4 * 2);
    });
}

#[test]
fn killing_the_process_during_a_codex_turn_lets_the_recovered_run_finish() {
    on_codex_runtime(async {
        // The review's first turn never answers in time: the process is
        // killed while it is in flight. The recovered run's turn answers.
        let model = FakeModel::start(vec![
            Reply::Text(DRAFT.into()),
            Reply::Delayed {
                after: Duration::from_secs(120),
                reply: Box::new(Reply::Text(REVIEW.into())),
            },
            Reply::Text(REVIEW.into()),
        ])
        .await
        .expect("fake model");
        let dir = scratch("midturn");
        let url = model.base_url();
        // Default time caps: the turn reserves the atom's whole 30 minutes.
        let caps = "10000,100000";
        let mut process = spawn_child(&dir, &url, "midturn", "start", caps);
        let until = Instant::now() + Duration::from_secs(60);
        while model.calls() < 2 {
            assert!(
                Instant::now() < until,
                "the review's turn reached the model"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        process.kill().expect("kill the child");
        let _ = process.wait();

        // Recovered, the dead turn is charged the time it took, not the 30
        // minutes it reserved, so the review runs again and the run ends.
        let seen = child(&dir, &url, "midturn", "recover", caps, None)
            .await
            .expect("the recovered run");
        assert_eq!(seen, "finished");
        assert_eq!(model.calls(), 3);
        let rt = reader(&dir).await;
        assert_eq!(
            requests("midturn", &attempts(&rt, "midturn").await),
            [
                ("0:0".to_owned(), 1, "settled".to_owned()),
                ("2:0".to_owned(), 1, "reserved".to_owned()),
                ("2:0".to_owned(), 2, "settled".to_owned()),
            ]
        );
        let review = attempts(&rt, "midturn").await;
        let dead = &review[1];
        assert_eq!(dead.charged.tokens, BOUND, "its tokens stay at the bound");
        assert!(dead.charged.millis < 60_000, "{dead:?}");
        assert!(dead.reserved.millis >= 29 * 60_000, "{dead:?}");
    });
}

#[test]
fn a_run_recovered_after_its_dead_turn_would_have_lapsed_is_charged_only_its_run_time() {
    on_codex_runtime(async {
        // Scaled down: the review's turn reserves its atom's 3 s and beats
        // every 300 ms. Killed about 0.9 s in and recovered after the 3 s
        // have passed, the dead turn is charged the time it ran, up to a
        // beat after its last heartbeat, and the review has the rest of its
        // time to run again.
        let model = FakeModel::start(vec![
            Reply::Text(DRAFT.into()),
            Reply::Delayed {
                after: Duration::from_secs(120),
                reply: Box::new(Reply::Text(REVIEW.into())),
            },
            Reply::Text(REVIEW.into()),
        ])
        .await
        .expect("fake model");
        let dir = scratch("lapsed");
        let url = model.base_url();
        let caps = "10000,100000";
        let mut process = spawn_child_with(&dir, &url, "lapsed", "start", caps, "0.01,0.05");
        let until = Instant::now() + Duration::from_secs(60);
        while model.calls() < 2 {
            assert!(
                Instant::now() < until,
                "the review's turn reached the model"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Killed once its turn has recorded a heartbeat a beat or more after
        // admission, however slowly a loaded machine runs it: recovery
        // charges it until a beat after that, at least 600 ms.
        let beaten = Instant::now() + Duration::from_secs(60);
        while beat_after_admission(&dir).await < 300 {
            assert!(
                Instant::now() < beaten,
                "the review's turn never recorded a heartbeat"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        process.kill().expect("kill the child");
        let _ = process.wait();
        // Down past the whole reservation.
        tokio::time::sleep(Duration::from_secs(4)).await;
        let seen = child_timed(&dir, &url, "lapsed", "recover", caps, "0.01,0.05")
            .await
            .expect("the recovered run");
        assert_eq!(seen, "finished");
        let rt = reader(&dir).await;
        let review = attempts(&rt, "lapsed").await;
        let dead = &review[1];
        assert_eq!(dead.state, "reserved");
        assert_eq!(dead.reserved.millis, 3_000, "{dead:?}");
        assert!(
            dead.charged.millis < 2_000,
            "the downtime was not charged: {dead:?}"
        );
        // Its heartbeats carried it past the first beat: it is charged the
        // time it ran.
        assert!(
            dead.charged.millis >= 600,
            "the time it ran was charged: {dead:?}"
        );
    });
}

#[test]
fn killing_just_after_the_park_is_recorded_keeps_one_park() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text("not json".into())])
            .await
            .expect("fake model");
        let dir = scratch("afterpark");
        let url = model.base_url();
        let caps = "150,100000";
        let seen = child(
            &dir,
            &url,
            "afterpark",
            "start",
            caps,
            Some("usage:after-park"),
        )
        .await;
        assert_eq!(seen, None, "killed after the park was recorded");
        let seen = child(&dir, &url, "afterpark", "recover", caps, None)
            .await
            .expect("the recovered run");
        assert!(seen.starts_with("parked: "), "{seen}");
        let rt = reader(&dir).await;
        assert_eq!(parks(&rt, "afterpark").await.expect("parks").len(), 1);
        assert_eq!(model.calls(), 1, "recovery made no call");
        assert_eq!(attempts(&rt, "afterpark").await.len(), 1);
        // The recovered run waits on the park it recorded.
        assert_eq!(
            rt.status("afterpark").await.expect("status"),
            Some(RunStatus::Pending)
        );
    });
}

#[test]
fn a_killed_and_resumed_run_reports_the_same_usage_plus_only_real_retries() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![
            // The uninterrupted run.
            Reply::Text(DRAFT.into()),
            Reply::Text(REVIEW.into()),
            // Killed after the draft's checkpoint, then resumed.
            Reply::Text(DRAFT.into()),
            Reply::Text(REVIEW.into()),
            // Killed after the draft's call, before its settlement, then
            // resumed: the draft really runs again.
            Reply::Text(DRAFT.into()),
            Reply::Text(DRAFT.into()),
            Reply::Text(REVIEW.into()),
        ])
        .await
        .expect("fake model");
        let dir = scratch("resumed");
        let url = model.base_url();
        let caps = "10000,100000";

        let seen = child(&dir, &url, "whole", "start", caps, None).await;
        assert_eq!(seen.as_deref(), Some("finished"));
        assert_eq!(model.calls(), 2);

        assert_eq!(
            child(
                &dir,
                &url,
                "checkpointed",
                "start",
                caps,
                Some("change-request:after-draft")
            )
            .await,
            None
        );
        let seen = child(&dir, &url, "checkpointed", "recover", caps, None).await;
        assert_eq!(seen.as_deref(), Some("finished"));
        assert_eq!(model.calls(), 4, "the checkpointed draft did not run again");

        assert_eq!(
            child(
                &dir,
                &url,
                "retried",
                "start",
                caps,
                Some("usage:before-settle:retried:0:0")
            )
            .await,
            None
        );
        let seen = child(&dir, &url, "retried", "recover", caps, None).await;
        assert_eq!(seen.as_deref(), Some("finished"));
        assert_eq!(model.calls(), 7, "the unsettled draft ran again");

        let rt = reader(&dir).await;
        let whole = attempts(&rt, "whole").await;
        let checkpointed = attempts(&rt, "checkpointed").await;
        // Reusing the checkpoint added nothing: the same attempts, the same
        // usage.
        assert_eq!(
            requests("whole", &whole),
            requests("checkpointed", &checkpointed)
        );
        assert_eq!(charged(&rt, "run:whole").await, 4);
        assert_eq!(charged(&rt, "run:checkpointed").await, 4);
        // The retried run spent the same, plus the call that really ran
        // again, charged at its bound since its usage was never settled.
        assert_eq!(
            requests("retried", &attempts(&rt, "retried").await),
            [
                ("0:0".to_owned(), 1, "reserved".to_owned()),
                ("0:0".to_owned(), 2, "settled".to_owned()),
                ("2:0".to_owned(), 1, "settled".to_owned()),
            ]
        );
        assert_eq!(charged(&rt, "run:retried").await, 4 + BOUND);
    });
}

// ---------------------------------------------------------------------------
// Parent and children.

/// The child workflow: one metered lite atom, under the parent allowance
/// its budget sits in.
const PAIR_CHILD: &str = "pair-child";

#[test]
fn parallel_children_cannot_both_take_the_last_reservation() {
    on_codex_runtime(async {
        // Two child runs, each its own workflow, race for the parent's
        // allowance. The first call is slow, so the second child's
        // admission comes while it is in flight.
        let model = FakeModel::start(vec![
            Reply::Delayed {
                after: Duration::from_secs(2),
                reply: Box::new(Reply::Text(DRAFT.into())),
            },
            Reply::Text(DRAFT.into()),
        ])
        .await
        .expect("fake model");
        let dir = scratch("pair");
        let mut rt = Durare::sqlite(&dir.join("atomic.db"))
            .await
            .expect("engine");
        // The parent has room for one whole bound at a time, though not two:
        // one after the other, once the first has settled at its two tokens,
        // both children's calls fit. Each child run, and its atom, has room
        // for many.
        let parent = atomic_durable::usage::Scope {
            id: "run:pair".into(),
            caps: Caps {
                tokens: 2 * BOUND - 1,
                millis: 3_600_000,
                micro_usd: None,
            },
        };
        let budget = RunBudget::new(
            rt.ledger(),
            Arc::new(settings(10_000, 100_000)),
            &Budget::default(),
            &["synthetic"],
        )
        .expect("budget")
        .under(parent);
        let caps = budget.atom(&Budget::default(), "synthetic").expect("caps");
        let lite = runtimes(&dir, &model.base_url()).lite.clone();
        rt.register(
            PAIR_CHILD,
            move |ctx: atomic_durable::DurareContext, name: String| {
                let (budget, caps, lite) = (budget.clone(), caps.clone(), lite.clone());
                async move {
                    run_atom_at(
                        &ctx,
                        "draft",
                        move |step| {
                            let mut lite = lite.clone();
                            lite.meter = budget.meter(&step, &caps);
                            let request = ChangeRequest {
                                request: name.clone(),
                            };
                            propose(async move { run_lite_atom(&DraftTask, &request, &lite).await })
                        },
                        Vec::new(),
                    )
                    .await
                }
            },
        );
        rt.launch().await.expect("launch");
        for child in ["pair-a", "pair-b"] {
            rt.start(PAIR_CHILD, child, child.to_owned())
                .await
                .expect("start");
        }
        // One child parks; the other's call is the only one dispatched.
        let until = Instant::now() + Duration::from_secs(60);
        let (loser, winner, park) = loop {
            let mut found = None;
            for (child, other) in [("pair-a", "pair-b"), ("pair-b", "pair-a")] {
                if let Some(p) = parks(&rt, child).await.expect("parks").into_iter().next() {
                    found = Some((child, other, p));
                }
            }
            if let Some(found) = found {
                break found;
            }
            assert!(Instant::now() < until, "a child parked");
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert!(park.reason.contains("run:pair"), "{}", park.reason);
        assert_eq!(model.calls(), 1, "only one child's call was dispatched");
        assert!(
            attempts(&rt, loser).await.is_empty(),
            "the refusal wrote nothing"
        );
        assert!(charged(&rt, "run:pair").await < 2 * BOUND);
        let _: atomic_durable::Published = rt.result(winner).await.expect("the winner finishes");
        assert_eq!(parks(&rt, winner).await.expect("parks").len(), 0);
        // The refusal was for the moment only: released, the other child's
        // call fits beside the winner's settled spend.
        release(&rt, &park).await.expect("release");
        let _: atomic_durable::Published = rt.result(loser).await.expect("the loser finishes");
        assert_eq!(model.calls(), 2);
        assert_eq!(charged(&rt, "run:pair").await, 4);
        rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    });
}

#[test]
fn a_lite_call_that_fails_is_charged_by_whether_it_could_have_been_sent() {
    on_codex_runtime(async {
        let dir = scratch("failed");
        let rt = Durare::sqlite(&dir.join("atomic.db"))
            .await
            .expect("engine");
        let budget = RunBudget::new(
            rt.ledger(),
            Arc::new(settings(10_000, 100_000)),
            &Budget::default(),
            &["synthetic"],
        )
        .expect("budget");
        let caps = budget.atom(&Budget::default(), "synthetic").expect("caps");
        let step = |run: &str| atomic_durable::StepInfo {
            workflow_id: run.to_owned(),
            step_id: 0,
        };
        let request = ChangeRequest {
            request: "Two units at 1.25 should cost 2.50.".into(),
        };
        // The provider answered with an error: the call was sent, and what
        // it spent is unknown, so it stays charged at its bound. An answer
        // with no usage is held the same way.
        let model = FakeModel::start(vec![
            Reply::Http {
                status: 500,
                body: "{}".into(),
            },
            Reply::WithoutUsage(DRAFT.into()),
        ])
        .await
        .expect("fake model");
        let mut lite = runtimes(&dir, &model.base_url()).lite.clone();
        lite.meter = budget.meter(&step("sent"), &caps);
        assert!(run_lite_atom(&DraftTask, &request, &lite).await.is_err());
        lite.meter = budget.meter(&step("silent"), &caps);
        run_lite_atom(&DraftTask, &request, &lite)
            .await
            .expect("an answer without usage");
        for run in ["sent", "silent"] {
            assert_eq!(
                requests(run, &attempts(&rt, run).await),
                [("0:0".to_owned(), 1, "reserved".to_owned())],
                "{run}"
            );
            assert_eq!(charged(&rt, &format!("atom:{run}:0")).await, BOUND);
        }
        // No connection was ever made: nothing was sent, and nothing spent.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = closed.local_addr().expect("addr").port();
        drop(closed);
        let mut lite = runtimes(&dir, &format!("http://127.0.0.1:{port}/v1"))
            .lite
            .clone();
        lite.meter = budget.meter(&step("unsent"), &caps);
        assert!(run_lite_atom(&DraftTask, &request, &lite).await.is_err());
        assert_eq!(
            requests("unsent", &attempts(&rt, "unsent").await),
            [("0:0".to_owned(), 1, "released".to_owned())]
        );
        assert_eq!(charged(&rt, "atom:unsent:0").await, 0);
    });
}

/// Settings whose lite requests are bounded at `call` minutes, under an
/// atom cap of `atom` minutes.
fn timed(call: f64, atom: f64) -> BudgetSettings {
    let mut s = settings(10_000, 100_000);
    s.call.minutes = call;
    s.atom.minutes = atom;
    s
}

#[test]
fn a_codex_turn_longer_than_a_request_bound_runs_within_its_atoms_time() {
    on_codex_runtime(async {
        // Scaled down: a request is bounded at 0.6 s and the atom at 6 s, so
        // a 1.5 s review turn is a long build turn under a 10-minute request
        // bound and a 30-minute atom cap, with room to spare.
        let model = FakeModel::start(vec![
            Reply::Text(DRAFT.into()),
            Reply::Delayed {
                after: Duration::from_millis(1_500),
                reply: Box::new(Reply::Text(REVIEW.into())),
            },
        ])
        .await
        .expect("fake model");
        let dir = scratch("longturn");
        let rt = engine(&dir, &model.base_url(), timed(0.01, 0.1)).await;
        rt.launch().await.expect("launch");
        rt.start(CHANGE_REQUEST, "longturn", input())
            .await
            .expect("start");
        assert_eq!(outcome(&rt, "longturn").await, Outcome::Finished);
        let journal = attempts(&rt, "longturn").await;
        // The lite request reserved its own bound; the turn, all its atom's
        // time.
        assert_eq!(journal[0].reserved.millis, 600);
        assert_eq!(journal[1].reserved.millis, 6_000, "{:?}", journal[1]);
        assert!(journal[1].charged.millis >= 1_500, "{:?}", journal[1]);
        // A turn that completed is charged what it reported: one input and
        // one output token from its one request.
        assert_eq!(journal[1].usage, Some((1, 1)), "{:?}", journal[1]);
        assert_eq!(journal[1].charged.tokens, 2, "{:?}", journal[1]);
        rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    });
}

#[test]
fn a_codex_turn_that_runs_out_of_its_atoms_time_parks_the_run() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![
            Reply::Text(DRAFT.into()),
            Reply::Delayed {
                after: Duration::from_secs(20),
                reply: Box::new(Reply::Text(REVIEW.into())),
            },
        ])
        .await
        .expect("fake model");
        let dir = scratch("outoftime");
        let rt = engine(&dir, &model.base_url(), timed(0.01, 0.02)).await;
        rt.launch().await.expect("launch");
        rt.start(CHANGE_REQUEST, "outoftime", input())
            .await
            .expect("start");
        let Outcome::Parked(reason) = outcome(&rt, "outoftime").await else {
            panic!("the run should park");
        };
        assert!(reason.contains("cut off"), "{reason}");
        assert!(
            reason.contains("the allowance atom:outoftime:2"),
            "{reason}"
        );
        let park = parks(&rt, "outoftime").await.expect("parks")[0].clone();
        assert_eq!(park.step, "review.park");
        rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    });
}

#[test]
fn a_codex_turn_cut_off_with_a_request_in_flight_is_charged_what_it_reported_and_one_bound() {
    on_codex_runtime(async {
        // One request is bounded at two tokens. The review turn's first two
        // requests report two tokens each and call a tool, past its
        // reservation; its third is in flight when the atom's time runs out.
        let tool = || Reply::Tool {
            name: "no_such_tool".into(),
            arguments: serde_json::json!({}),
        };
        let model = FakeModel::start(vec![
            Reply::Text(DRAFT.into()),
            tool(),
            tool(),
            // Held well past the cut-off, so the turn is always stopped
            // waiting on it, however slow the runner.
            Reply::Delayed {
                after: Duration::from_secs(120),
                reply: Box::new(Reply::Text(REVIEW.into())),
            },
        ])
        .await
        .expect("fake model");
        let dir = scratch("inflight");
        // Nine seconds for the turn: ample for its first two requests.
        let mut budgets = timed(0.01, 0.15);
        budgets.call.input_tokens = 1;
        budgets.call.output_tokens = 1;
        let rt = engine(&dir, &model.base_url(), budgets).await;
        rt.launch().await.expect("launch");
        rt.start(CHANGE_REQUEST, "inflight", input())
            .await
            .expect("start");
        let Outcome::Parked(reason) = outcome(&rt, "inflight").await else {
            panic!("the run should park");
        };
        assert!(reason.contains("cut off"), "{reason}");
        assert_eq!(model.calls(), 4, "the turn's third request was made");
        let journal = attempts(&rt, "inflight").await;
        let turn = &journal[1];
        assert_eq!(turn.reserved.tokens, 2, "{turn:?}");
        // What its requests reported, and the bound of the one in flight;
        // the reported usage is kept, and the attempt left open.
        assert_eq!(turn.usage, Some((2, 2)), "{turn:?}");
        assert_eq!(turn.charged.tokens, 4 + 2, "{turn:?}");
        assert_eq!(turn.overshoot(), 4, "{turn:?}");
        assert_eq!(turn.state, "reserved", "{turn:?}");
        rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    });
}

#[test]
fn a_request_in_flight_is_estimated_at_the_larger_of_one_bound_and_the_last_request() {
    on_codex_runtime(async {
        let dir = scratch("estimate");
        let rt = Durare::sqlite(&dir.join("atomic.db"))
            .await
            .expect("engine");
        // One request is bounded at 10 input and 5 output tokens, at one
        // micro-dollar an input token and two an output token: 15 tokens and
        // 20 micro-dollars.
        let mut settings = settings(10_000_000, 100_000_000);
        settings.call.input_tokens = 10;
        settings.call.output_tokens = 5;
        settings.pricing.insert(
            "synthetic".into(),
            Price {
                input_usd_per_mtok: 1.0,
                output_usd_per_mtok: 2.0,
                max_input_tokens: None,
                max_output_tokens: None,
            },
        );
        let plan = atomic_app::metering::BudgetPlan::resolve(
            &settings,
            &Budget::default(),
            &[("turn", &Budget::default(), "synthetic")],
            &[],
        )
        .expect("a plan");
        let budget = RunBudget::pinned(rt.ledger(), &plan);
        let meter = budget.meter(
            &atomic_durable::StepInfo {
                workflow_id: "estimate".into(),
                step_id: 1,
            },
            &plan.atoms["turn"],
        );
        let (ticket, _) = meter
            .admit(atomic_core::usage::Call {
                index: 0,
                model: "synthetic".into(),
                input_bound: None,
                turn: true,
                work: false,
            })
            .await
            .expect("admitted");
        // It reported 100 and 20, and its last request 60 and 30: larger
        // than one request's bound in tokens (90 to 15) and in dollars (120
        // to 20 micro-dollars).
        let usage = |input_tokens, output_tokens| atomic_core::usage::Usage {
            input_tokens,
            output_tokens,
        };
        meter
            .settle(
                ticket,
                atomic_core::usage::Outcome::Partial {
                    reported: usage(100, 20),
                    last: Some(usage(60, 30)),
                },
            )
            .await
            .expect("settled");
        let attempt = &rt.ledger().attempts("estimate").await.expect("attempts")[0];
        assert_eq!(attempt.usage, Some((100, 20)));
        assert_eq!(attempt.charged.tokens, 120 + 90, "{attempt:?}");
        assert_eq!(attempt.charged.micro_usd, 140 + 120, "{attempt:?}");
    });
}

#[test]
fn a_call_to_a_model_the_budget_does_not_price_is_not_made_under_a_dollar_cap() {
    on_codex_runtime(async {
        let dir = scratch("unpricedcall");
        let rt = Durare::sqlite(&dir.join("atomic.db"))
            .await
            .expect("engine");
        let mut priced = settings(10_000, 100_000);
        priced.workflow.cost_usd = Some(1.0);
        priced.pricing.insert(
            "synthetic".into(),
            Price {
                input_usd_per_mtok: 1.0,
                output_usd_per_mtok: 1.0,
                max_input_tokens: None,
                max_output_tokens: None,
            },
        );
        let budget = RunBudget::new(
            rt.ledger(),
            Arc::new(priced),
            &Budget::default(),
            &["synthetic"],
        )
        .expect("budget");
        let caps = budget.atom(&Budget::default(), "synthetic").expect("caps");
        let model = FakeModel::start(vec![Reply::Text(DRAFT.into())])
            .await
            .expect("fake model");
        // The atom asks for another model than the budget was resolved for.
        let mut lite = runtimes(&dir, &model.base_url()).lite.clone();
        lite.endpoint.model = "unpriced".into();
        lite.meter = budget.meter(
            &atomic_durable::StepInfo {
                workflow_id: "unpricedcall".into(),
                step_id: 0,
            },
            &caps,
        );
        let request = ChangeRequest {
            request: "Two units at 1.25 should cost 2.50.".into(),
        };
        let refused = run_lite_atom(&DraftTask, &request, &lite)
            .await
            .expect_err("refused");
        assert!(refused.to_string().contains("has no price"), "{refused}");
        assert_eq!(model.calls(), 0, "the call was not made");
        assert!(attempts(&rt, "unpricedcall").await.is_empty());
    });
}

#[test]
fn a_dollar_cap_on_an_unpriced_model_fails_configuration() {
    on_codex_runtime(async {
        let dir = scratch("unpriced");
        let mut budgets = settings(10_000, 100_000);
        budgets.workflow.cost_usd = Some(1.0);
        let mut rt = Durare::sqlite(&dir.join("atomic.db"))
            .await
            .expect("engine");
        let refused = register_change_request(
            &mut rt,
            runtimes(&dir, "http://127.0.0.1:9/v1"),
            Arc::new(budgets.clone()),
        )
        .expect_err("an unpriced dollar cap");
        assert_eq!(
            refused,
            BudgetError::Unpriced {
                model: "synthetic".into()
            }
        );
        assert!(refused.to_string().contains("cap tokens or minutes"));
        // Priced, it resolves.
        budgets.pricing.insert(
            "synthetic".into(),
            atomic_core::budget::Price {
                input_usd_per_mtok: 1.0,
                output_usd_per_mtok: 2.0,
                max_input_tokens: None,
                max_output_tokens: None,
            },
        );
        let r = resolve(
            &Budget::default(),
            Level::Workflow,
            &budgets,
            &["synthetic"],
        )
        .expect("priced");
        assert_eq!(r.micro_usd, Some(1_000_000));
    });
}

/// A command admitted by a process that then died is found when its step
/// runs again: its reservation is settled then, not held for good, at the
/// time its last heartbeat vouches for, and a step whose command was never
/// admitted is told apart from it.
#[test]
fn a_dead_commands_reservation_is_settled_when_it_is_found() {
    use atomic_durable::usage::{Admission, Charge, Request, Scope};
    use std::sync::atomic::{AtomicI64, Ordering};
    on_codex_runtime(async {
        let dir = scratch("dead-command");
        let rt = Durare::sqlite(&dir.join("atomic.db"))
            .await
            .expect("engine");
        let start = 1_800_000_000_000;
        let now = Arc::new(AtomicI64::new(start));
        let clock = now.clone();
        let ledger = rt.ledger().with_clock(move || clock.load(Ordering::SeqCst));
        let plan = atomic_app::metering::BudgetPlan::resolve(
            &settings(10_000_000, 100_000_000),
            &Budget::default(),
            &[],
            &[("nap", &Budget::default())],
        )
        .expect("a plan");
        let caps = |r: &atomic_core::budget::Resolved| Caps {
            tokens: i64::try_from(r.tokens).expect("tokens"),
            millis: i64::try_from(r.millis).expect("millis"),
            micro_usd: None,
        };
        // A command admitted as the step's work, as its meter admits one.
        let least = Charge {
            tokens: 0,
            millis: 500,
            micro_usd: 0,
        };
        let admitted = ledger
            .admit(Request {
                id: "dead:3:0".into(),
                run: "dead".into(),
                model: atomic_core::usage::WORK_MODEL.into(),
                chain: vec![
                    Scope::run("dead", caps(&plan.run)),
                    Scope::atom("dead", 3, caps(&plan.exec["nap"])),
                ],
                reserve: least,
                maximum: least,
                turn: true,
            })
            .await
            .expect("admit");
        let Admission::Admitted {
            request, attempt, ..
        } = admitted
        else {
            panic!("refused: {admitted:?}");
        };
        // Its last heartbeat twenty seconds in; then its process died, and
        // the step runs again an hour later.
        now.store(start + 20_000, Ordering::SeqCst);
        ledger
            .heartbeat(&request, attempt)
            .await
            .expect("heartbeat");
        now.store(start + 3_600_000, Ordering::SeqCst);
        let budget = RunBudget::pinned(ledger.clone(), &plan);
        let step = atomic_durable::StepInfo {
            workflow_id: "dead".into(),
            step_id: 3,
        };
        assert!(budget.command_admitted(&step).await.expect("ledger"));
        assert!(
            ledger.unsettled("dead").await.expect("ledger").is_empty(),
            "the dead command's reservation is still held"
        );
        // Charged until a heartbeat after its last: 20 s and 5 s more, not
        // the hour since, nor all it reserved.
        let attempts = ledger.attempts("dead").await.expect("attempts");
        assert_eq!(attempts.len(), 1, "{attempts:?}");
        assert_eq!(attempts[0].state, "settled", "{attempts:?}");
        assert_eq!(attempts[0].charged.millis, 25_000, "{attempts:?}");
        let run = ledger
            .allowance("run:dead")
            .await
            .expect("ledger")
            .expect("the run's allowance");
        assert_eq!(run.charged.millis, 25_000, "{run:?}");
        let never = atomic_durable::StepInfo {
            workflow_id: "dead".into(),
            step_id: 5,
        };
        assert!(!budget.command_admitted(&never).await.expect("ledger"));
        rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    });
}

// ---------------------------------------------------------------------------
// A Codex turn stopped or killed before it settles.

/// Replies for a review turn whose first two requests each report two
/// tokens and call a tool, past a call bound of two tokens, and whose third
/// is answered after `third`.
fn reporting_turn(third: Reply) -> Vec<Reply> {
    let tool = || Reply::Tool {
        name: "no_such_tool".into(),
        arguments: serde_json::json!({}),
    };
    vec![Reply::Text(DRAFT.into()), tool(), tool(), third]
}

/// The child, starting run `run` with a call bound of two tokens.
fn reporting_child(dir: &Path, url: &str, run: &str) -> Command {
    reporting_child_as(dir, url, run, "start")
}

/// [`reporting_child`], as `role`.
fn reporting_child_as(dir: &Path, url: &str, run: &str, role: &str) -> Command {
    let mut cmd = Command::new(std::env::current_exe().expect("this test binary"));
    cmd.args(["--exact", "budgets_child", "--ignored", "--nocapture"])
        .env(ENV_ROLE, role)
        .env(ENV_DIR, dir)
        .env(ENV_URL, url)
        .env(ENV_RUN, run)
        .env(ENV_CAPS, "10000,100000")
        .env(ENV_PARKS, "1")
        .env(ENV_CALL, "1,1")
        .env_remove("ATOMIC_FAILPOINT")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// Run `cmd` for at most a minute: how it exited, or `None` if it was still
/// running then and was killed.
async fn bounded(mut cmd: Command) -> Option<std::process::ExitStatus> {
    let mut process = cmd.spawn().expect("start the child");
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = process.try_wait().expect("the child") {
            return Some(status);
        }
        if Instant::now() > until {
            let _ = process.kill();
            let _ = process.wait();
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The review turn's first attempt in run `run`.
async fn review_turn(rt: &Durare, run: &str) -> Option<Attempt> {
    attempts(rt, run)
        .await
        .into_iter()
        .find(|a| a.request == format!("{run}:2:0") && a.attempt == 1)
}

#[test]
fn a_codex_turn_stopped_by_a_signal_is_charged_what_it_reported() {
    on_codex_runtime(async {
        // The third request is held long past the test: the process stops
        // while it is in flight, as on a signal, once the turn has recorded
        // both requests' usage.
        let model = FakeModel::start(reporting_turn(Reply::Delayed {
            after: Duration::from_secs(120),
            reply: Box::new(Reply::Text(REVIEW.into())),
        }))
        .await
        .expect("fake model");
        let dir = scratch("signalled");
        let run = "signalled";
        let mut cmd = reporting_child(&dir, &model.base_url(), run);
        cmd.env(ENV_STOP_AT, format!("{run}:2:0,2,2"));
        let status = bounded(cmd).await;
        assert_eq!(
            status.and_then(|s| s.code()),
            Some(STOPPED),
            "the child did not stop mid-turn: {status:?}"
        );

        let rt = reader(&dir).await;
        let turn = review_turn(&rt, run).await.expect("the turn");
        assert_eq!(turn.reserved.tokens, 2, "{turn:?}");
        // What it reported, and one request's bound for the one in flight:
        // not its reservation.
        assert_eq!(turn.usage, Some((2, 2)), "{turn:?}");
        assert_eq!(turn.charged.tokens, 4 + 2, "{turn:?}");
        assert_eq!(turn.state, "reserved", "{turn:?}");
        assert_eq!(charged(&rt, &format!("run:{run}")).await, 2 + 4 + 2);
    });
}

#[test]
fn a_codex_turn_killed_mid_turn_is_charged_what_it_reported() {
    on_codex_runtime(async {
        // The turn's third request, if the killed process made it, and the
        // recovered turn's answer.
        let mut replies = reporting_turn(Reply::Text(REVIEW.into()));
        replies.push(Reply::Text(REVIEW.into()));
        let model = FakeModel::start(replies).await.expect("fake model");
        let dir = scratch("killed-mid-turn");
        let url = model.base_url();
        // Killed as the turn records what its second request reported.
        let run = "killed-mid-turn";
        let mut cmd = reporting_child(&dir, &url, run);
        cmd.env(
            "ATOMIC_FAILPOINT",
            format!("usage:after-progress:{run}:2:0:2"),
        );
        let status = bounded(cmd).await;
        assert!(
            status.is_some_and(|s| !s.success()),
            "the child was not killed mid-turn: {status:?}"
        );

        let rt = reader(&dir).await;
        let turn = review_turn(&rt, run).await.expect("the turn");
        assert_eq!(turn.reserved.tokens, 2, "{turn:?}");
        assert_eq!(turn.usage, Some((2, 2)), "{turn:?}");
        assert_eq!(turn.charged.tokens, 4 + 2, "{turn:?}");
        assert_eq!(turn.state, "reserved", "{turn:?}");
        drop(rt);

        // Recovered, the review runs again as a second attempt with its own
        // reservation; the dead one keeps what it recorded.
        let mut cmd = reporting_child_as(&dir, &url, run, "recover");
        cmd.stdout(Stdio::null());
        let status = bounded(cmd).await;
        assert!(
            status.is_some_and(|s| s.success()),
            "the recovered run: {status:?}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("outcome")).expect("outcome"),
            "finished"
        );
        let rt = reader(&dir).await;
        let review: Vec<Attempt> = attempts(&rt, run)
            .await
            .into_iter()
            .filter(|a| a.request == format!("{run}:2:0"))
            .collect();
        assert_eq!(review.len(), 2, "{review:?}");
        let (dead, again) = (&review[0], &review[1]);
        assert_eq!(
            (dead.attempt, dead.state.as_str()),
            (1, "reserved"),
            "{dead:?}"
        );
        assert_eq!(dead.charged.tokens, 4 + 2, "{dead:?}");
        assert_eq!(
            (again.attempt, again.state.as_str()),
            (2, "settled"),
            "{again:?}"
        );
        assert_eq!(again.usage, Some((1, 1)), "{again:?}");
        // The draft's two tokens, the dead turn's six, the new turn's two.
        assert_eq!(charged(&rt, &format!("run:{run}")).await, 2 + 6 + 2);
        assert_eq!(charged(&rt, &format!("atom:{run}:2")).await, 6 + 2);
    });
}
