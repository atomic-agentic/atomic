// SPDX-License-Identifier: Apache-2.0

//! The gate store and durable gates, written against [`DurableRuntime`]
//! alone and run on SQLite and (with `postgres-tests`) PostgreSQL:
//!
//! - a gate is opened, listed and decided with no work graph, and the
//!   decision completes the workflow's wait;
//! - a replayed decision returns the original result; a different decision
//!   on a decided gate gets a typed conflict carrying the winner; a command
//!   id reused for another decision is refused;
//! - only an approver, deciding on the proposal the gate shows, decides it;
//! - only the credential a principal enrolled decides for it;
//! - a wake-up the store does not back passes nothing;
//! - a deadline that passes expires the gate, which authorizes nothing, and
//!   a later decision gets the expiry as its conflict;
//! - changed content cannot reuse an earlier approval;
//! - a run forked from, or imported as, an approved run does not pass its
//!   gate on the copied checkpoints: the store has no approval for it, and
//!   an approval copied in from another run names that run.

use super::*;
use crate::runtime::{Durare, RunStatus};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

fn unique(tag: &str) -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{tag}_{}_{n}_{nanos}", std::process::id())
}

async fn sqlite() -> Durare {
    let path = std::env::temp_dir().join(format!("{}.db", unique("atomic_gates")));
    Durare::sqlite(&path).await.expect("sqlite engine")
}

#[cfg(feature = "postgres-tests")]
async fn postgres() -> Durare {
    let url = crate::testing::database().await;
    Durare::postgres(&url, &unique("atomic_gates"))
        .await
        .expect("postgres engine")
}

fn operator() -> Principal {
    Principal::local("dev")
}

/// The operator's credential, enrolled.
async fn me<R: DurableRuntime>(rt: &R) -> HumanCredential {
    let credential = HumanCredential::new(operator(), "the operator's secret");
    enroll(rt, &credential).await.expect("enroll");
    credential
}

const EVIDENCE: &str = "sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

/// The input of the test workflow: what its gate shows and how long it waits.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Gated {
    show: Value,
    timeout_ms: Option<u64>,
}

fn spec(input: &Gated) -> GateSpec {
    GateSpec {
        workflow: "gated".into(),
        title: "Accept the reviewed summary".into(),
        show: input.show.clone(),
        evidence: vec![EVIDENCE.into()],
        approvers: resolve(
            "accept",
            None,
            &Deployment::Local {
                operator: operator(),
            },
        )
        .expect("approvers"),
        tier: Tier::High,
        timeout: input.timeout_ms.map(Duration::from_millis),
        on_timeout: OnTimeout::Reject,
        question: None,
    }
}

fn register<R: DurableRuntime>(rt: &mut R) {
    rt.register("gated", |ctx: R::Context, input: Gated| async move {
        gate(&ctx, "accept", spec(&input)).await
    });
}

fn input(timeout_ms: Option<u64>) -> Gated {
    Gated {
        show: json!({"draft": "Two units cost 2.50.", "review": "approve"}),
        timeout_ms,
    }
}

/// Start the test workflow and wait until its gate is in the store.
async fn start_gated<R: DurableRuntime>(rt: &R, input: Gated) -> (String, Gate) {
    let run = unique("gated");
    rt.start("gated", &run, input).await.expect("start");
    let id = gate_id(&run, "accept");
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(g) = find(rt, &id).await.expect("find") {
            return (run, g);
        }
        assert!(Instant::now() < until, "gate {id} never opened");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The run's result, failing the test instead of waiting forever when a
/// run that should finish keeps waiting.
async fn outcome<R: DurableRuntime>(rt: &R, run: &str) -> Result<Resolution> {
    tokio::time::timeout(Duration::from_secs(30), rt.result::<Resolution>(run))
        .await
        .unwrap_or_else(|_| panic!("{run} is still waiting at its gate"))
}

fn command(gate: &Gate, id: &str, verdict: Verdict) -> DecisionCommand {
    DecisionCommand {
        command_id: id.into(),
        gate: gate.id.clone(),
        verdict,
        comment: Some("looks right".into()),
        proposal: gate.proposal.clone(),
        answer: None,
    }
}

async fn a_gate_is_opened_listed_and_decided<R: DurableRuntime>(mut rt: R) {
    register(&mut rt);
    rt.launch().await.expect("launch");
    let (run, opened) = start_gated(&rt, input(None)).await;

    assert_eq!(opened.id, format!("{run}:accept"));
    assert_eq!(opened.run, run);
    assert_eq!(opened.workflow, "gated");
    assert_eq!(opened.step, "accept");
    assert_eq!(opened.title, "Accept the reviewed summary");
    assert_eq!(opened.show, input(None).show);
    assert_eq!(opened.evidence, [EVIDENCE]);
    assert_eq!(
        opened.proposal,
        proposal_digest(&opened.title, &opened.show, &opened.evidence)
    );
    assert_eq!(opened.approvers, [operator()]);
    assert_eq!(opened.tier, Tier::High);
    assert_eq!(opened.deadline, None);
    assert_eq!(opened.resolution, None);
    let open = gates(
        &rt,
        GateFilter {
            open_only: true,
            run: Some(run.clone()),
        },
    )
    .await
    .expect("list");
    assert_eq!(open, std::slice::from_ref(&opened));

    let decision = decide(
        &rt,
        &me(&rt).await,
        command(&opened, "c1", Verdict::Approve),
    )
    .await
    .expect("decided");
    assert_eq!(decision.verdict, Verdict::Approve);
    assert_eq!(decision.by, operator());
    assert_eq!(decision.comment.as_deref(), Some("looks right"));
    assert_eq!(decision.command, "c1");

    let resolution: Resolution = outcome(&rt, &run).await.expect("the run passes the gate");
    assert_eq!(resolution, Resolution::Decided(decision.clone()));
    assert!(resolution.approved());
    let stored = find(&rt, &opened.id).await.expect("find").expect("stored");
    assert_eq!(stored.resolution, Some(Resolution::Decided(decision)));
    assert!(
        gates(
            &rt,
            GateFilter {
                open_only: true,
                run: Some(run.clone())
            }
        )
        .await
        .expect("list")
        .is_empty()
    );
    let names: Vec<String> = rt
        .steps(&run)
        .await
        .expect("steps")
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names.first().map(String::as_str), Some("accept.gate"));
    assert_eq!(names.last().map(String::as_str), Some("accept.decision"));
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

async fn a_replayed_decision_returns_the_original_and_another_conflicts<R: DurableRuntime>(
    mut rt: R,
) {
    register(&mut rt);
    rt.launch().await.expect("launch");
    let (run, opened) = start_gated(&rt, input(None)).await;

    let first = decide(
        &rt,
        &me(&rt).await,
        command(&opened, "c1", Verdict::Approve),
    )
    .await
    .expect("decided");
    let replay = decide(
        &rt,
        &me(&rt).await,
        command(&opened, "c1", Verdict::Approve),
    )
    .await;
    assert_eq!(replay, Ok(first.clone()), "a replay returns the original");

    let winning = Resolution::Decided(first.clone());
    let other = decide(&rt, &me(&rt).await, command(&opened, "c2", Verdict::Reject)).await;
    assert_eq!(
        other,
        Err(DecideError::Conflict {
            gate: opened.id.clone(),
            winning: winning.clone()
        }),
        "a different decision gets the winner"
    );
    let other_again = decide(&rt, &me(&rt).await, command(&opened, "c2", Verdict::Reject)).await;
    assert_eq!(other_again, other, "and so does its replay");

    let reused = decide(&rt, &me(&rt).await, command(&opened, "c1", Verdict::Reject)).await;
    assert_eq!(reused, Err(DecideError::CommandReused("c1".into())));

    assert_eq!(outcome(&rt, &run).await, Ok(winning.clone()));
    let stored = find(&rt, &opened.id).await.expect("find").expect("stored");
    assert_eq!(
        stored.resolution,
        Some(winning),
        "the first decision stands"
    );
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

async fn only_an_approver_on_the_shown_proposal_decides<R: DurableRuntime>(mut rt: R) {
    register(&mut rt);
    rt.launch().await.expect("launch");
    let (run, opened) = start_gated(&rt, input(None)).await;

    let stranger = Principal::local("someone-else");
    let theirs = HumanCredential::new(stranger.clone(), "their own secret");
    enroll(&rt, &theirs).await.expect("enroll");
    assert_eq!(
        decide(&rt, &theirs, command(&opened, "c1", Verdict::Approve)).await,
        Err(DecideError::NotAnApprover {
            gate: opened.id.clone(),
            who: stranger
        })
    );
    let mut stale = command(&opened, "c2", Verdict::Approve);
    stale.proposal = proposal_digest("Accept the reviewed summary", &json!({"draft": "old"}), &[]);
    assert_eq!(
        decide(&rt, &me(&rt).await, stale.clone()).await,
        Err(DecideError::ProposalChanged {
            gate: opened.id.clone(),
            shown: stale.proposal.clone(),
            current: opened.proposal.clone(),
        })
    );
    let mut missing = command(&opened, "c3", Verdict::Approve);
    missing.gate = format!("{run}:nowhere");
    assert_eq!(
        decide(&rt, &me(&rt).await, missing.clone()).await,
        Err(DecideError::NoSuchGate(missing.gate))
    );
    let still = find(&rt, &opened.id).await.expect("find").expect("stored");
    assert_eq!(still.resolution, None, "none of those decided anything");

    // The refused commands recorded nothing, so their ids are still free.
    let decision = decide(&rt, &me(&rt).await, command(&opened, "c1", Verdict::Reject))
        .await
        .expect("decided");
    let resolution: Resolution = outcome(&rt, &run).await.expect("finishes");
    assert_eq!(resolution, Resolution::Decided(decision));
    assert!(!resolution.approved(), "a rejection does not pass the gate");
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

/// Wait until `run` has recorded at least `n` operations.
/// A decision counts only with the credential its principal enrolled: one
/// with no credential enrolled, or with a guessed one, records nothing, and
/// enrolling again cannot replace the first credential.
async fn only_the_enrolled_credential_decides<R: DurableRuntime>(mut rt: R) {
    register(&mut rt);
    rt.launch().await.expect("launch");
    let (run, opened) = start_gated(&rt, input(None)).await;

    let claimed = HumanCredential::new(operator(), "a guess");
    assert!(!enrolled(&rt, &operator()).await.expect("enrolled"));
    assert_eq!(
        decide(&rt, &claimed, command(&opened, "c1", Verdict::Approve)).await,
        Err(DecideError::NotEnrolled(operator()))
    );
    let mine = me(&rt).await;
    assert!(enrolled(&rt, &operator()).await.expect("enrolled"));
    assert_eq!(
        enroll(&rt, &mine).await.expect("again"),
        Enrollment::AlreadyEnrolled
    );
    assert!(matches!(
        enroll(&rt, &claimed).await,
        Err(EnrollError::Different(p)) if p == operator()
    ));
    assert_eq!(
        decide(&rt, &claimed, command(&opened, "c1", Verdict::Approve)).await,
        Err(DecideError::BadCredential(operator()))
    );
    let still = find(&rt, &opened.id).await.expect("find").expect("stored");
    assert_eq!(still.resolution, None, "neither decided anything");

    // Rotating replaces the credential: the old one no longer decides.
    let rotated = HumanCredential::new(operator(), "the rotated secret");
    assert_eq!(
        replace_enrollment(&rt, &rotated).await.expect("rotate"),
        Enrollment::Replaced
    );
    assert_eq!(
        replace_enrollment(&rt, &rotated).await.expect("again"),
        Enrollment::AlreadyEnrolled
    );
    assert_eq!(
        decide(&rt, &mine, command(&opened, "c1", Verdict::Approve)).await,
        Err(DecideError::BadCredential(operator()))
    );
    replace_enrollment(&rt, &mine).await.expect("back");

    // The refused commands recorded nothing, so their id is still free.
    let decision = decide(&rt, &mine, command(&opened, "c1", Verdict::Approve))
        .await
        .expect("decided");
    assert_eq!(decision.by, operator());
    assert!(outcome(&rt, &run).await.expect("finishes").approved());
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

/// A question gate takes only an answer that fits its question, records
/// it as the decision, and, asked to remember, keeps it in the feedback
/// log; an approval gate takes no answer.
async fn a_question_gate_takes_only_an_answer_that_fits<R: DurableRuntime>(mut rt: R) {
    rt.register("asked", |ctx: R::Context, remember: bool| async move {
        let mut s = spec(&input(None));
        s.workflow = "asked".into();
        s.title = "Which channel?".into();
        s.question = Some(Question {
            answer: json!({"enum": ["stable", "beta"]}),
            remember,
        });
        gate(&ctx, "pick", s).await
    });
    register(&mut rt);
    rt.launch().await.expect("launch");
    let me = me(&rt).await;
    let mut results = Vec::new();
    for remember in [true, false] {
        let run = unique("asked");
        rt.start("asked", &run, remember).await.expect("start");
        let id = gate_id(&run, "pick");
        let opened = loop {
            if let Some(g) = find(&rt, &id).await.expect("find") {
                break g;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert_eq!(opened.question.as_ref().map(|q| q.remember), Some(remember));
        let answering = |id: &str, answer: Option<Value>| DecisionCommand {
            answer,
            ..command(&opened, &format!("{run}-{id}"), Verdict::Approve)
        };
        for (cmd, why) in [
            (
                answering("c1", Some(json!("nightly"))),
                "does not answer it",
            ),
            (answering("c2", None), "answer it"),
            (
                DecisionCommand {
                    answer: Some(json!("beta")),
                    ..command(&opened, &format!("{run}-c3"), Verdict::Reject)
                },
                "a rejection gives no answer",
            ),
        ] {
            match decide(&rt, &me, cmd).await {
                Err(DecideError::BadAnswer { why: got, .. }) => {
                    assert!(got.contains(why), "{got}")
                }
                other => panic!("{other:?}"),
            }
        }
        let decision = decide(&rt, &me, answering("c4", Some(json!("beta"))))
            .await
            .expect("answered");
        assert_eq!(decision.answer, Some(json!("beta")));
        let resolution = outcome(&rt, &run).await.expect("finishes");
        assert_eq!(resolution, Resolution::Decided(decision.clone()));
        results.push((id, remember));
    }
    let log = feedback(&rt).await.expect("feedback");
    assert_eq!(
        log.len(),
        1,
        "only the gate that asked to remember: {log:?}"
    );
    assert_eq!(log[0].gate, results[0].0);
    assert_eq!(log[0].ask, "Which channel?");
    assert_eq!(log[0].answer, json!("beta"));
    assert_eq!(log[0].by, operator());

    // An approval gate takes no answer.
    let (_, plain) = start_gated(&rt, input(None)).await;
    let cmd = DecisionCommand {
        answer: Some(json!("beta")),
        ..command(&plain, "c5", Verdict::Approve)
    };
    assert!(matches!(
        decide(&rt, &me, cmd).await,
        Err(DecideError::BadAnswer { .. })
    ));
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

async fn until_steps<R: DurableRuntime>(rt: &R, run: &str, n: usize) {
    let until = Instant::now() + Duration::from_secs(20);
    while rt.steps(run).await.expect("steps").len() < n {
        assert!(Instant::now() < until, "{run} never reached {n} operations");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn a_wake_up_the_store_does_not_back_passes_nothing<R: DurableRuntime>(mut rt: R) {
    register(&mut rt);
    rt.launch().await.expect("launch");
    let (run, opened) = start_gated(&rt, input(None)).await;
    let before = rt.steps(&run).await.expect("steps").len();

    // A message on the gate's topic that claims an approval, with no
    // decision in the store behind it.
    rt.send(
        &run,
        &topic(&opened.id),
        json!({"verdict": "approve", "by": "local:dev"}),
        "forged",
    )
    .await
    .expect("send");
    // The run wakes, checks the store, finds the gate open and waits again.
    until_steps(&rt, &run, before + 2).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rt.status(&run).await, Ok(Some(RunStatus::Pending)));
    let names: Vec<String> = rt
        .steps(&run)
        .await
        .expect("steps")
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert!(
        names.iter().filter(|n| *n == "accept.decision").count() == 1,
        "one check, which found the gate open: {names:?}"
    );

    let decision = decide(
        &rt,
        &me(&rt).await,
        command(&opened, "c1", Verdict::Approve),
    )
    .await
    .expect("decided");
    assert_eq!(outcome(&rt, &run).await, Ok(Resolution::Decided(decision)));
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

async fn a_passed_deadline_expires_the_gate_and_a_later_decision_conflicts<R: DurableRuntime>(
    mut rt: R,
) {
    register(&mut rt);
    rt.launch().await.expect("launch");
    let started = now_ms();
    let (run, opened) = start_gated(&rt, input(Some(1_000))).await;
    let deadline = opened.deadline.expect("a deadline");
    assert!(
        (started..=now_ms() + 1_000).contains(&deadline) && deadline >= opened.opened_at + 1_000,
        "the deadline is the opening time plus the timeout"
    );

    let resolution: Resolution = outcome(&rt, &run).await.expect("finishes");
    let Resolution::Expired { at, on_timeout } = resolution.clone() else {
        panic!("expected an expiry, got {resolution:?}");
    };
    assert!(at >= deadline);
    assert_eq!(on_timeout, OnTimeout::Reject);
    assert!(!resolution.approved(), "an expiry does not pass the gate");

    assert_eq!(
        decide(
            &rt,
            &me(&rt).await,
            command(&opened, "late", Verdict::Approve)
        )
        .await,
        Err(DecideError::Conflict {
            gate: opened.id.clone(),
            winning: resolution,
        })
    );
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

async fn a_decision_after_the_deadline_records_the_expiry<R: DurableRuntime>(rt: R) {
    // The gate's run is gone, as if atomic were offline: only the store is
    // left, and its clock still runs.
    let run = unique("offline");
    let id = gate_id(&run, "accept");
    let past = now_ms() - 1;
    let spec = spec(&input(Some(1)));
    let proposal = proposal_digest(&spec.title, &spec.show, &spec.evidence);
    let (i, r, p, approvers) = (
        id.clone(),
        run.clone(),
        proposal.clone(),
        encode(&spec.approvers).expect("encode"),
    );
    rt.control_transaction(move |mut tx| {
        let (i, r, p, a) = (i.clone(), r.clone(), p.clone(), approvers.clone());
        Box::pin(async move {
            tx.execute(
                "INSERT INTO atomic_gates (gate_id, run, workflow, step, title, show, evidence, \
                 proposal, approvers, tier, opened_at, deadline, on_timeout, state) \
                 VALUES (?, ?, 'gated', 'accept', 't', '{}', '[]', ?, ?, 'high', ?, ?, 'reject', \
                 'open')",
                &[
                    Param::Text(i),
                    Param::Text(r),
                    Param::Text(p),
                    Param::Text(a),
                    Param::Int(past - 1),
                    Param::Int(past),
                ],
            )
            .await
            .map(|_| ())
        })
    })
    .await
    .expect("insert");
    rt.launch().await.expect("launch");
    let gate = find(&rt, &id).await.expect("find").expect("stored");
    let refused = decide(
        &rt,
        &me(&rt).await,
        command(&gate, "late", Verdict::Approve),
    )
    .await;
    let Err(DecideError::Conflict {
        winning: Resolution::Expired { at, .. },
        ..
    }) = refused
    else {
        panic!("expected the expiry as the conflict, got {refused:?}");
    };
    assert!(at >= past);
    let stored = find(&rt, &id).await.expect("find").expect("stored");
    assert!(matches!(
        stored.resolution,
        Some(Resolution::Expired { .. })
    ));
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

async fn changed_content_cannot_reuse_an_approval<R: DurableRuntime>(mut rt: R) {
    // The workflow shows what its input says; a second run of the same
    // workflow showing different content opens its own gate, and the
    // first gate's approval does not decide it.
    register(&mut rt);
    rt.launch().await.expect("launch");
    let (first_run, first) = start_gated(&rt, input(None)).await;
    let approved = decide(&rt, &me(&rt).await, command(&first, "c1", Verdict::Approve))
        .await
        .expect("decided");
    assert!(outcome(&rt, &first_run).await.expect("finishes").approved());

    let changed = Gated {
        show: json!({"draft": "Two units cost 2.75.", "review": "approve"}),
        timeout_ms: None,
    };
    let (second_run, second) = start_gated(&rt, changed).await;
    assert_ne!(second.proposal, first.proposal, "new content, new proposal");
    // The earlier command, sent at the new gate with its old proposal, is
    // refused: it was made on other content.
    let mut reuse = command(&first, "c9", Verdict::Approve);
    reuse.gate = second.id.clone();
    assert_eq!(
        decide(&rt, &me(&rt).await, reuse).await,
        Err(DecideError::ProposalChanged {
            gate: second.id.clone(),
            shown: first.proposal.clone(),
            current: second.proposal.clone(),
        })
    );
    assert_eq!(
        find(&rt, &second.id)
            .await
            .expect("find")
            .expect("stored")
            .resolution,
        None
    );
    let _ = approved;

    // Tampering with what a gate's row was opened on does not pass the
    // gate either: the run checks the row against what it shows.
    let id = second.id.clone();
    rt.control_transaction(move |mut tx| {
        let id = id.clone();
        Box::pin(async move {
            tx.execute(
                "UPDATE atomic_gates SET proposal = 'sha256:00' WHERE gate_id = ?",
                &[Param::Text(id)],
            )
            .await
            .map(|_| ())
        })
    })
    .await
    .expect("tamper");
    rt.send(
        &second_run,
        &topic(&second.id),
        json!({"gate": second.id}),
        "tampered",
    )
    .await
    .expect("send");
    let refused = outcome(&rt, &second_run).await;
    assert!(
        matches!(&refused, Err(DurableError::Failed(m)) if m.contains("changed content needs a new gate")),
        "{refused:?}"
    );
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

async fn redelivery_wakes_a_run_whose_decider_stopped_early<R: DurableRuntime>(mut rt: R) {
    register(&mut rt);
    rt.launch().await.expect("launch");
    let (run, opened) = start_gated(&rt, input(None)).await;
    // A decision committed straight to the store, with no wake-up: what a
    // decider that stopped between the two leaves behind.
    let id = opened.id.clone();
    rt.control_transaction(move |mut tx| {
        let id = id.clone();
        Box::pin(async move {
            tx.execute(
                "UPDATE atomic_gates SET state = 'approved', decided_by = 'local:dev', \
                 decided_at = 1, command_id = 'c1' WHERE gate_id = ?",
                &[Param::Text(id)],
            )
            .await
            .map(|_| ())
        })
    })
    .await
    .expect("decide without waking");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rt.status(&run).await, Ok(Some(RunStatus::Pending)));
    assert!(redeliver(&rt).await.expect("redeliver") >= 1);
    let resolution: Resolution = outcome(&rt, &run).await.expect("finishes");
    assert!(resolution.approved());
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

#[test]
fn the_proposal_digest_ignores_key_order_and_sees_every_change() {
    let a = json!({"b": 1, "a": {"y": [1, 2], "x": null}});
    let b: Value =
        serde_json::from_str(r#"{"a": {"x": null, "y": [1, 2]}, "b": 1}"#).expect("json");
    let e = vec![EVIDENCE.to_owned()];
    assert_eq!(proposal_digest("t", &a, &e), proposal_digest("t", &b, &e));
    assert_ne!(proposal_digest("t", &a, &e), proposal_digest("u", &a, &e));
    assert_ne!(proposal_digest("t", &a, &e), proposal_digest("t", &a, &[]));
    assert_ne!(
        proposal_digest("t", &a, &e),
        proposal_digest("t", &json!({"b": 2, "a": {"y": [1, 2], "x": null}}), &e)
    );
}

/// Run the test workflow through an approval. Returns the run, its gate and
/// the position of the step that read the decision.
async fn approved_run(rt: &Durare) -> (String, Gate, i32) {
    let (run, opened) = start_gated(rt, input(None)).await;
    decide(
        rt,
        &me(rt).await,
        command(&opened, &unique("c"), Verdict::Approve),
    )
    .await
    .expect("decided");
    assert!(outcome(rt, &run).await.expect("finishes").approved());
    let decision = rt
        .steps(&run)
        .await
        .expect("steps")
        .into_iter()
        .rfind(|s| s.name == "accept.decision")
        .expect("a decision step")
        .step_id;
    (run, opened, decision)
}

/// A copy of gate `from`'s row under gate id `id`, still naming `from`'s run:
/// an approval someone wrote for another run.
async fn copy_gate_row(rt: &Durare, from: &str, id: &str) {
    let (from, id) = (from.to_owned(), id.to_owned());
    rt.control_transaction(move |mut tx| {
        let (from, id) = (from.clone(), id.clone());
        Box::pin(async move {
            let rest = "run, workflow, step, title, show, evidence, proposal, approvers, tier, \
                        opened_at, deadline, on_timeout, state, decided_by, decided_at, comment, \
                        command_id";
            tx.execute(
                &format!(
                    "INSERT INTO atomic_gates (gate_id, {rest}) \
                     SELECT ?, {rest} FROM atomic_gates WHERE gate_id = ?"
                ),
                &[Param::Text(id), Param::Text(from)],
            )
            .await
            .map(|_| ())
        })
    })
    .await
    .expect("copy the row");
}

async fn a_forked_run_does_not_reuse_the_original_approval(mut rt: Durare) {
    register(&mut rt);
    rt.launch().await.expect("launch");
    let (run, opened, decision) = approved_run(&rt).await;

    // A fork from the decision step copies the gate's opening and its
    // recorded wake-up, and reads the decision again, in the fork's name.
    let fork = unique("fork");
    rt.engine
        .fork_workflow::<Value>(&run, decision, durare::WorkflowOptions::with_id(&fork))
        .await
        .expect("fork");
    let refused = outcome(&rt, &fork).await;
    assert!(
        matches!(&refused, Err(DurableError::Failed(m)) if m.contains("is not in the gate store")),
        "{refused:?}"
    );

    // And an approval copied into the fork's gate id still names the
    // original run, so it does not pass either.
    let copied = unique("fork");
    copy_gate_row(&rt, &opened.id, &gate_id(&copied, "accept")).await;
    rt.engine
        .fork_workflow::<Value>(&run, decision, durare::WorkflowOptions::with_id(&copied))
        .await
        .expect("fork");
    let refused = outcome(&rt, &copied).await;
    assert!(
        matches!(&refused, Err(DurableError::Failed(m)) if m.contains(&format!("belongs to run {run}"))),
        "{refused:?}"
    );
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

async fn an_imported_run_does_not_reuse_an_exported_approval(mut rt: Durare) {
    register(&mut rt);
    rt.launch().await.expect("launch");
    let (run, _, decision) = approved_run(&rt).await;

    // Export the approved run and import it under a new id, unfinished, with
    // its checkpoints up to the decision: everything a forged import could
    // say, including a recorded wake-up.
    let forged = unique("imported");
    let mut exported = rt
        .engine
        .export_workflow(&run, false)
        .await
        .expect("export");
    for w in &mut exported {
        w.workflow_status
            .insert("workflow_uuid".into(), json!(forged.clone()));
        w.workflow_status.insert("status".into(), json!("PENDING"));
        w.workflow_status.insert("output".into(), Value::Null);
        w.operation_outputs.retain(|o| {
            o.get("function_id")
                .and_then(Value::as_i64)
                .is_some_and(|f| f < i64::from(decision))
        });
        for o in &mut w.operation_outputs {
            o.insert("workflow_uuid".into(), json!(forged.clone()));
        }
        for table in [
            &mut w.workflow_events,
            &mut w.workflow_events_history,
            &mut w.streams,
        ] {
            table.clear();
        }
    }
    rt.engine.import_workflow(&exported).await.expect("import");
    rt.recover().await.expect("recover");
    let refused = outcome(&rt, &forged).await;
    assert!(
        matches!(&refused, Err(DurableError::Failed(m)) if m.contains("is not in the gate store")),
        "{refused:?}"
    );
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

macro_rules! on_backend {
    ($backend:ident, $($name:ident),+ $(,)?) => {
        mod $backend {
            $(
                #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
                async fn $name() {
                    super::$name(super::$backend().await).await
                }
            )+
        }
    };
}

on_backend!(
    sqlite,
    a_gate_is_opened_listed_and_decided,
    a_replayed_decision_returns_the_original_and_another_conflicts,
    only_an_approver_on_the_shown_proposal_decides,
    only_the_enrolled_credential_decides,
    a_question_gate_takes_only_an_answer_that_fits,
    a_wake_up_the_store_does_not_back_passes_nothing,
    a_passed_deadline_expires_the_gate_and_a_later_decision_conflicts,
    a_decision_after_the_deadline_records_the_expiry,
    changed_content_cannot_reuse_an_approval,
    redelivery_wakes_a_run_whose_decider_stopped_early,
    a_forked_run_does_not_reuse_the_original_approval,
    an_imported_run_does_not_reuse_an_exported_approval,
    importing_a_decision_checkpoint_does_not_authorize_another_run,
    a_fork_after_the_decision_still_requires_its_own_approval,
    an_approved_gate_replays_in_its_original_run,
    concurrent_replays_return_one_recorded_decision,
);

#[cfg(feature = "postgres-tests")]
on_backend!(
    postgres,
    a_gate_is_opened_listed_and_decided,
    a_replayed_decision_returns_the_original_and_another_conflicts,
    only_an_approver_on_the_shown_proposal_decides,
    only_the_enrolled_credential_decides,
    a_question_gate_takes_only_an_answer_that_fits,
    a_wake_up_the_store_does_not_back_passes_nothing,
    a_passed_deadline_expires_the_gate_and_a_later_decision_conflicts,
    a_decision_after_the_deadline_records_the_expiry,
    changed_content_cannot_reuse_an_approval,
    redelivery_wakes_a_run_whose_decider_stopped_early,
    a_forked_run_does_not_reuse_the_original_approval,
    an_imported_run_does_not_reuse_an_exported_approval,
    importing_a_decision_checkpoint_does_not_authorize_another_run,
    a_fork_after_the_decision_still_requires_its_own_approval,
    an_approved_gate_replays_in_its_original_run,
    concurrent_replays_return_one_recorded_decision,
);

async fn importing_a_decision_checkpoint_does_not_authorize_another_run(mut rt: Durare) {
    register(&mut rt);
    rt.launch().await.expect("launch");
    let (run, opened, _) = approved_run(&rt).await;
    for rejected_row in [false, true] {
        let forged = unique("complete_import");
        if rejected_row {
            let id = gate_id(&forged, "accept");
            copy_gate_row(&rt, &opened.id, &id).await;
            let current_run = forged.clone();
            rt.control_transaction(move |mut tx| {
                let (id, run) = (id.clone(), current_run.clone());
                Box::pin(async move {
                    tx.execute(
                        "UPDATE atomic_gates SET run = ?, state = 'rejected' WHERE gate_id = ?",
                        &[Param::Text(run), Param::Text(id)],
                    )
                    .await
                    .map(|_| ())
                })
            })
            .await
            .expect("a rejection for the imported run");
        }
        let mut exported = rt
            .engine
            .export_workflow(&run, false)
            .await
            .expect("export");
        for workflow in &mut exported {
            workflow
                .workflow_status
                .insert("workflow_uuid".into(), json!(forged.clone()));
            workflow
                .workflow_status
                .insert("status".into(), json!("PENDING"));
            workflow
                .workflow_status
                .insert("output".into(), Value::Null);
            // Keep all checkpoints, including the transaction that checked the gate store.
            for operation in &mut workflow.operation_outputs {
                operation.insert("workflow_uuid".into(), json!(forged.clone()));
            }
            workflow.workflow_events.clear();
            workflow.workflow_events_history.clear();
            workflow.streams.clear();
        }
        rt.engine.import_workflow(&exported).await.expect("import");
        rt.recover().await.expect("recover");
        let result = outcome(&rt, &forged).await;
        if rejected_row {
            assert!(
                matches!(
                    result,
                    Ok(Resolution::Decided(Decision {
                        verdict: Verdict::Reject,
                        ..
                    }))
                ),
                "the checkpoint overrode the store's rejection: {result:?}"
            );
        } else {
            assert!(
                matches!(result, Err(DurableError::Failed(_))),
                "new run reused an approval without its own gate-store row: {result:?}"
            );
        }
    }
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

async fn a_fork_after_the_decision_still_requires_its_own_approval(mut rt: Durare) {
    rt.register(
        "gated",
        |ctx: crate::runtime::DurareContext, input: Gated| async move {
            let resolution = gate(&ctx, "accept", spec(&input)).await?;
            ctx.step("after-gate", |_| async { Ok(()) }).await?;
            Ok(resolution)
        },
    );
    rt.launch().await.expect("launch");
    let (run, _, decision) = approved_run(&rt).await;
    let fork = unique("fork_after_decision");
    rt.engine
        .fork_workflow::<Value>(&run, decision + 1, durare::WorkflowOptions::with_id(&fork))
        .await
        .expect("fork after decision");
    let result = outcome(&rt, &fork).await;
    assert!(
        matches!(result, Err(DurableError::Failed(_))),
        "fork reused the cached approval: {result:?}"
    );
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

async fn an_approved_gate_replays_in_its_original_run(mut rt: Durare) {
    let executions = std::sync::Arc::new(AtomicUsize::new(0));
    let counter = executions.clone();
    rt.register(
        "gated",
        move |ctx: crate::runtime::DurareContext, input: Gated| {
            let counter = counter.clone();
            async move {
                let resolution = gate(&ctx, "accept", spec(&input)).await?;
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(DurableError::Interrupted(
                        "resume after authorization".into(),
                    ));
                }
                Ok(resolution)
            }
        },
    );
    rt.launch().await.expect("launch");
    let (run, opened) = start_gated(&rt, input(None)).await;
    decide(
        &rt,
        &me(&rt).await,
        command(&opened, &unique("decision"), Verdict::Approve),
    )
    .await
    .expect("decide");
    assert!(matches!(
        outcome(&rt, &run).await,
        Err(DurableError::Interrupted(_))
    ));
    let steps = rt.steps(&run).await.expect("steps");
    rt.recover().await.expect("recover");
    assert!(outcome(&rt, &run).await.expect("recovered gate").approved());
    assert_eq!(executions.load(Ordering::SeqCst), 2);
    assert_eq!(
        rt.steps(&run).await.expect("steps").len(),
        steps.len(),
        "revalidation must not add durable operations"
    );
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

async fn concurrent_replays_return_one_recorded_decision<R: DurableRuntime>(mut rt: R) {
    register(&mut rt);
    rt.launch().await.expect("launch");
    let (_, opened) = start_gated(&rt, input(None)).await;
    let cmd = command(&opened, &unique("same_command"), Verdict::Approve);
    let person = me(&rt).await;
    let (a, b) = tokio::join!(decide(&rt, &person, cmd.clone()), decide(&rt, &person, cmd));
    assert_eq!(a.expect("first caller"), b.expect("replayed caller"));
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
}

#[cfg(feature = "postgres-tests")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_raced_command_id_cannot_decide_another_gate() {
    use sqlx::Connection;
    let url = std::env::var("ATOMIC_TEST_DATABASE_URL").expect("test Postgres");
    let schema = unique("command_race");
    let separator = if url.contains('?') { '&' } else { '?' };
    let url = format!("{url}{separator}application_name={schema}");
    let mut rt = Durare::postgres(&url, &schema).await.expect("engine");
    register(&mut rt);
    rt.launch().await.expect("launch");
    let (_, gate_a) = start_gated(&rt, input(None)).await;
    let (_, gate_b) = start_gated(&rt, input(None)).await;
    let rt = std::sync::Arc::new(rt);
    let mut connection = sqlx::PgConnection::connect(&url).await.expect("connection");
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET search_path TO {schema}")))
        .execute(&mut connection)
        .await
        .expect("schema");
    let mut monitor = sqlx::PgConnection::connect(&url).await.expect("monitor");
    let mut lock = connection.begin().await.expect("lock transaction");
    sqlx::query("SELECT gate_id FROM atomic_gates WHERE gate_id = $1 FOR UPDATE")
        .bind(&gate_a.id)
        .fetch_one(&mut *lock)
        .await
        .expect("lock gate A");
    let command_id = unique("same_command");
    let request_a = command(&gate_a, &command_id, Verdict::Approve);
    let person = me(rt.as_ref()).await;
    let raced_rt = rt.clone();
    let raced_person = person.clone();
    let first =
        tokio::spawn(async move { decide(raced_rt.as_ref(), &raced_person, request_a).await });
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let blocked: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE application_name = $1 AND wait_event_type = 'Lock' AND query LIKE 'UPDATE atomic_gates SET state = %'")
            .bind(&schema).fetch_one(&mut monitor).await.expect("waiting queries");
        if blocked > 0 {
            break;
        }
        assert!(
            Instant::now() < until,
            "decide A never blocked on its gate update"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    decide(
        rt.as_ref(),
        &person,
        command(&gate_b, &command_id, Verdict::Approve),
    )
    .await
    .expect("B wins command id");
    lock.commit().await.expect("unlock gate A");
    let rejected = first.await.expect("decider task");
    assert!(
        matches!(rejected, Err(DecideError::CommandReused(_))),
        "{rejected:?}"
    );
    let stored = find(rt.as_ref(), &gate_a.id)
        .await
        .expect("find")
        .expect("gate A");
    rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    assert!(
        stored.resolution.is_none(),
        "command reuse returned an error but durably decided gate A: {:?}",
        stored.resolution
    );
}
