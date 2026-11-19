// SPDX-License-Identifier: Apache-2.0

//! Budgets on the change-request workflow, against the scripted fake model
//! and the durable usage ledger:
//!
//! - every model call has a ledger attempt, made before the call and settled
//!   once with what the provider reported, the repair turn's included;
//! - a cap with no room for the next call parks the run before the call is
//!   made; a rejected answer's spend is kept; a resumed run stays parked, even
//!   with a larger budget configured;
//! - killing the process after the provider was called and before the usage
//!   was settled, again and again, leaves each attempt charged at its bound
//!   until admission parks the run at the cap;
//! - a run killed and resumed reports the same usage as an uninterrupted
//!   one, plus only the calls that really ran again;
//! - two parallel children cannot both take the run's last reservation;
//! - a dollar cap on an unpriced model fails configuration.

// Codex's turn futures nest deeply inside the workflow's; proving them Send
// needs more than the default query depth.
#![recursion_limit = "512"]

use atomic_app::metering::RunBudget;
use atomic_app::workflows::{
    CHANGE_REQUEST, ChangeRequestResult, ChangeRequestRun, propose, register_change_request,
};
use atomic_atoms::{ChangeRequest, DraftTask};
use atomic_core::AtomConfig;
use atomic_core::budget::{Budget, BudgetError, BudgetSettings, CallBound, Level, resolve};
use atomic_core::codex::{CodexPaths, ModelEndpoint};
use atomic_core::lite::LiteRuntime;
use atomic_core::runtime::{Artifacts, CodexRuntime, Runtimes, run_lite_atom};
use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use atomic_core::usage::Metering;
use atomic_durable::usage::Attempt;
use atomic_durable::{DurableRuntime, Durare, RunStatus, parks, run_atom_at};
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
        },
        lite: LiteRuntime::new(endpoint, AtomConfig::default()),
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
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        match rt.status(id).await.expect("status") {
            Some(RunStatus::Succeeded) => return Outcome::Finished,
            Some(RunStatus::Failed(e)) => panic!("run {id} failed: {e}"),
            _ => {}
        }
        if let Some(p) = parks(rt, id).await.expect("parks").into_iter().next() {
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
        let rt = engine(&dir, &model.base_url(), settings(10_000, 100_000)).await;
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
            assert_eq!(a.excess_bound(), 0, "no excess left unreserved");
        }
        assert_eq!(charged(&rt, "run:every").await, 6);
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
fn a_cap_smaller_than_one_call_parks_with_no_call_made() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(DRAFT.into())])
            .await
            .expect("fake model");
        let dir = scratch("none");
        let rt = engine(&dir, &model.base_url(), settings(100, 100_000)).await;
        rt.launch().await.expect("launch");
        rt.start(CHANGE_REQUEST, "tiny", input())
            .await
            .expect("start");
        assert!(matches!(outcome(&rt, "tiny").await, Outcome::Parked(_)));
        assert_eq!(model.calls(), 0);
        assert!(
            attempts(&rt, "tiny").await.is_empty(),
            "a refusal writes nothing"
        );
        rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    });
}

// ---------------------------------------------------------------------------
// Crash tests: the run in a child process, killed at a named failpoint.

const ENV_ROLE: &str = "ATOMIC_BUDGETS_ROLE";
const ENV_DIR: &str = "ATOMIC_BUDGETS_DIR";
const ENV_URL: &str = "ATOMIC_BUDGETS_URL";
const ENV_RUN: &str = "ATOMIC_BUDGETS_RUN";
const ENV_CAPS: &str = "ATOMIC_BUDGETS_CAPS";

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
    let budgets = settings(
        atom.parse().expect("atom cap"),
        total.parse().expect("run cap"),
    );
    on_codex_runtime(async move {
        let dir = PathBuf::from(dir);
        let rt = engine(&dir, &url, budgets).await;
        rt.launch().await.expect("launch");
        if role == "start" {
            rt.start(CHANGE_REQUEST, &run, input())
                .await
                .expect("start");
        } else {
            rt.recover().await.expect("recover");
        }
        let seen = match outcome(&rt, &run).await {
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
    let _ = std::fs::remove_file(dir.join("outcome"));
    let mut cmd = Command::new(std::env::current_exe().expect("this test binary"));
    cmd.args(["--exact", "budgets_child", "--ignored", "--nocapture"])
        .env(ENV_ROLE, role)
        .env(ENV_DIR, dir)
        .env(ENV_URL, url)
        .env(ENV_RUN, run)
        .env(ENV_CAPS, caps)
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

async fn reader(dir: &Path) -> Durare {
    Durare::sqlite(&dir.join("atomic.db"))
        .await
        .expect("engine")
}

#[test]
fn killing_after_the_call_and_before_settlement_ends_parked_at_the_cap() {
    on_codex_runtime(async {
        let model = FakeModel::start(Vec::new()).await.expect("fake model");
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

/// A child's run, boxed. Written over any context, as workflow code is: the
/// compiler proves the future `Send` from the trait's bounds.
type Child = std::pin::Pin<
    Box<dyn Future<Output = atomic_durable::Result<atomic_durable::Published>> + Send>,
>;

/// One of two children of the `pair` workflow, each a metered lite atom.
fn pair_child<C: atomic_durable::WorkflowContext>(
    ctx: C,
    name: &'static str,
    budget: RunBudget,
    caps: atomic_core::budget::Resolved,
    mut lite: LiteRuntime,
) -> Child {
    Box::pin(async move {
        run_atom_at(
            &ctx,
            name,
            move |step| {
                lite.meter = budget.meter(&step, &caps);
                let request = ChangeRequest {
                    request: name.into(),
                };
                propose(async move { run_lite_atom(&DraftTask, &request, &lite).await })
            },
            Vec::new(),
        )
        .await
    })
}

#[test]
fn parallel_children_cannot_both_take_the_last_reservation() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(DRAFT.into()), Reply::Text(DRAFT.into())])
            .await
            .expect("fake model");
        let dir = scratch("pair");
        let mut rt = Durare::sqlite(&dir.join("atomic.db"))
            .await
            .expect("engine");
        // The run has room for one call, even once that call has settled at
        // its two tokens; each child has room for many. (The ledger's own
        // tests race many admissions at once against one allowance.)
        let budget = RunBudget::new(
            rt.ledger(),
            Arc::new(settings(10_000, 151)),
            &Budget::default(),
            &["synthetic"],
        )
        .expect("budget");
        let lite = runtimes(&dir, &model.base_url()).lite.clone();
        rt.register("pair", move |ctx: atomic_durable::DurareContext, (): ()| {
            let (budget, lite) = (budget.clone(), lite.clone());
            async move {
                let caps = budget.atom(&Budget::default(), "synthetic").expect("caps");
                let a = pair_child(ctx.clone(), "a", budget.clone(), caps.clone(), lite.clone());
                let b = pair_child(ctx, "b", budget, caps, lite);
                let (a, b) = tokio::join!(a, b);
                Ok(vec![a?, b?])
            }
        });
        rt.launch().await.expect("launch");
        rt.start("pair", "pair", ()).await.expect("start");
        assert!(matches!(outcome(&rt, "pair").await, Outcome::Parked(_)));
        // Let the winner finish its publication.
        let until = Instant::now() + Duration::from_secs(30);
        while !rt
            .steps("pair")
            .await
            .expect("steps")
            .iter()
            .any(|s| s.name.ends_with(".publish") && s.output.is_some())
        {
            assert!(Instant::now() < until, "the admitted child published");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(model.calls(), 1, "only one child's call was dispatched");
        let journal = attempts(&rt, "pair").await;
        assert_eq!(journal.len(), 1, "{journal:?}");
        assert_eq!(parks(&rt, "pair").await.expect("parks").len(), 1);
        assert!(charged(&rt, "run:pair").await <= 151);
        rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
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
