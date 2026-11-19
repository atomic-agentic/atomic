// SPDX-License-Identifier: Apache-2.0

//! The usage ledger, on SQLite and (with `postgres-tests`) PostgreSQL:
//!
//! - a call's attempt is journaled and reserved at admission, before
//!   dispatch, and settled once: a second settlement changes nothing;
//! - an attempt that is never settled stays charged at its bound; a re-run
//!   is a new attempt with its own reservation, until admission refuses;
//! - a refusal writes nothing, and the caps an allowance was created with
//!   hold however a later admission states them;
//! - parallel children cannot both take their parent's last reservation;
//! - only a call that was never dispatched is released;
//! - usage past the reservation is recorded, against its declared bound.

use super::*;
use crate::runtime::{DurableRuntime, Durare};
use std::sync::atomic::{AtomicUsize, Ordering};

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
    let path = std::env::temp_dir().join(format!("{}.db", unique("atomic_usage")));
    Durare::sqlite(&path).await.expect("sqlite engine")
}

#[cfg(feature = "postgres-tests")]
async fn postgres() -> Durare {
    let url = crate::testing::database().await;
    Durare::postgres(&url, &unique("atomic_usage"))
        .await
        .expect("postgres engine")
}

const BOUND: Charge = Charge {
    tokens: 100,
    millis: 1_000,
    micro_usd: 50,
};

fn caps(tokens: i64) -> Caps {
    Caps {
        tokens,
        millis: 1_000_000,
        micro_usd: None,
    }
}

fn request(run: &str, step: i32, call: u32, run_cap: i64, atom_cap: i64) -> Request {
    Request {
        id: format!("{run}:{step}:{call}"),
        run: run.to_owned(),
        model: "synthetic".into(),
        chain: vec![
            Scope::run(run, caps(run_cap)),
            Scope::atom(run, step, caps(atom_cap)),
        ],
        reserve: BOUND,
        maximum: BOUND,
    }
}

fn admitted(a: &Admission) -> (String, i64) {
    match a {
        Admission::Admitted { request, attempt } => (request.clone(), *attempt),
        Admission::Refused { reason, .. } => panic!("refused: {reason}"),
    }
}

async fn charged(ledger: &Ledger, scope: &str) -> Charge {
    ledger
        .allowance(scope)
        .await
        .expect("read")
        .expect("allowance exists")
        .charged
}

async fn a_call_is_reserved_before_dispatch_and_settled_once<R: DurableRuntime>(rt: R) {
    let ledger = rt.ledger();
    let run = unique("once");
    let (req, attempt) = admitted(
        &ledger
            .admit(request(&run, 1, 0, 1_000, 1_000))
            .await
            .expect("admit"),
    );
    assert_eq!(attempt, 1);

    // Before dispatch: journaled, and charged at its bound on every scope.
    let journal = ledger.attempts(&run).await.expect("attempts");
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].state, "reserved");
    assert_eq!(journal[0].charged, BOUND);
    assert_eq!(charged(&ledger, &format!("run:{run}")).await, BOUND);
    assert_eq!(charged(&ledger, &format!("atom:{run}:1")).await, BOUND);

    let reported = Settlement::Reported {
        input_tokens: 30,
        output_tokens: 12,
        millis: 200,
        micro_usd: 7,
    };
    assert_eq!(
        ledger
            .settle(&req, attempt, reported)
            .await
            .expect("settle"),
        Settled::Now
    );
    let used = Charge {
        tokens: 42,
        millis: 200,
        micro_usd: 7,
    };
    assert_eq!(charged(&ledger, &format!("run:{run}")).await, used);
    assert_eq!(charged(&ledger, &format!("atom:{run}:1")).await, used);

    // Settled once: a repeat, even with other numbers, changes nothing.
    let again = Settlement::Reported {
        input_tokens: 99,
        output_tokens: 99,
        millis: 999,
        micro_usd: 99,
    };
    assert_eq!(
        ledger
            .settle(&req, attempt, again)
            .await
            .expect("settle again"),
        Settled::Already
    );
    assert_eq!(charged(&ledger, &format!("run:{run}")).await, used);
    let journal = ledger.attempts(&run).await.expect("attempts");
    assert_eq!(journal[0].state, "settled");
    assert_eq!(journal[0].usage, Some((30, 12)));
}

async fn an_unsettled_attempt_stays_charged_until_admission_refuses<R: DurableRuntime>(rt: R) {
    let ledger = rt.ledger();
    let run = unique("unsettled");
    // Room for three bounds. Each re-run of the same call (as after a crash
    // past dispatch) is a new attempt; none is ever settled.
    for n in 1..=3 {
        let (_, attempt) = admitted(
            &ledger
                .admit(request(&run, 1, 0, 300, 10_000))
                .await
                .expect("admit"),
        );
        assert_eq!(attempt, n, "a new attempt at the same request");
    }
    // Unknown usage leaves the reservation charged.
    assert_eq!(
        ledger
            .settle(&format!("{run}:1:0"), 2, Settlement::Unknown)
            .await
            .expect("settle"),
        Settled::Uncertain
    );
    let full = Charge {
        tokens: 300,
        millis: 3_000,
        micro_usd: 150,
    };
    assert_eq!(charged(&ledger, &format!("run:{run}")).await, full);

    match ledger
        .admit(request(&run, 1, 0, 300, 10_000))
        .await
        .expect("admit")
    {
        Admission::Refused { scope, reason } => {
            assert_eq!(scope, format!("run:{run}"));
            assert!(reason.contains("300 of 300 tokens"), "{reason}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    // The refusal wrote nothing.
    assert_eq!(ledger.attempts(&run).await.expect("attempts").len(), 3);
    assert_eq!(charged(&ledger, &format!("run:{run}")).await, full);
}

async fn caps_hold_as_first_created<R: DurableRuntime>(rt: R) {
    let ledger = rt.ledger();
    let run = unique("caps");
    admitted(
        &ledger
            .admit(request(&run, 1, 0, 100, 10_000))
            .await
            .expect("admit"),
    );
    // A resumed run whose budget now says ten times as much.
    let raised = ledger
        .admit(request(&run, 1, 1, 1_000, 100_000))
        .await
        .expect("admit");
    assert!(
        matches!(raised, Admission::Refused { .. }),
        "the stored cap holds: {raised:?}"
    );
    let a = ledger
        .allowance(&format!("run:{run}"))
        .await
        .expect("read")
        .expect("exists");
    assert_eq!(a.caps.tokens, 100);
    let atom = ledger
        .allowance(&format!("atom:{run}:1"))
        .await
        .expect("read")
        .expect("exists");
    assert_eq!(atom.parent.as_deref(), Some(format!("run:{run}").as_str()));
}

async fn parallel_children_cannot_both_take_the_last_reservation<R: DurableRuntime>(rt: R) {
    let ledger = rt.ledger();
    for round in 0..5 {
        let run = unique(&format!("race{round}"));
        // The run has room for exactly one bound; each child for many.
        let children: Vec<_> = (1..=4)
            .map(|step| {
                let ledger = ledger.clone();
                let req = request(&run, step, 0, 150, 10_000);
                tokio::spawn(async move { ledger.admit(req).await })
            })
            .collect();
        let mut won = 0;
        for child in children {
            match child.await.expect("task").expect("admit") {
                Admission::Admitted { .. } => won += 1,
                Admission::Refused { scope, .. } => assert_eq!(scope, format!("run:{run}")),
            }
        }
        assert_eq!(won, 1, "exactly one child takes the last reservation");
        assert_eq!(charged(&ledger, &format!("run:{run}")).await.tokens, 100);
        assert_eq!(ledger.attempts(&run).await.expect("attempts").len(), 1);
    }
}

async fn only_a_call_never_dispatched_is_released<R: DurableRuntime>(rt: R) {
    let ledger = rt.ledger();
    let run = unique("release");
    let (req, attempt) = admitted(
        &ledger
            .admit(request(&run, 1, 0, 1_000, 1_000))
            .await
            .expect("admit"),
    );
    assert_eq!(
        ledger
            .settle(&req, attempt, Settlement::NotDispatched)
            .await
            .expect("release"),
        Settled::Now
    );
    assert_eq!(
        charged(&ledger, &format!("run:{run}")).await,
        Charge::default()
    );
    assert_eq!(
        ledger.attempts(&run).await.expect("attempts")[0].state,
        "released"
    );
    // A released attempt cannot be settled into spend afterwards.
    let late = Settlement::Reported {
        input_tokens: 5,
        output_tokens: 5,
        millis: 5,
        micro_usd: 5,
    };
    assert_eq!(
        ledger.settle(&req, attempt, late).await.expect("settle"),
        Settled::Already
    );
    assert_eq!(
        charged(&ledger, &format!("run:{run}")).await,
        Charge::default()
    );
}

async fn usage_past_the_reservation_is_recorded_against_its_bound<R: DurableRuntime>(rt: R) {
    let ledger = rt.ledger();
    let run = unique("overshoot");
    // A whole-bound reservation declares no excess.
    let (req, attempt) = admitted(
        &ledger
            .admit(request(&run, 1, 0, 1_000, 1_000))
            .await
            .expect("admit"),
    );
    let whole = &ledger.attempts(&run).await.expect("attempts")[0];
    assert_eq!(whole.excess_bound(), 0);
    // The provider billed past its documented maximum: recorded, charged.
    let over = Settlement::Reported {
        input_tokens: 90,
        output_tokens: 30,
        millis: 10,
        micro_usd: 1,
    };
    ledger.settle(&req, attempt, over).await.expect("settle");
    let settled = &ledger.attempts(&run).await.expect("attempts")[0];
    assert_eq!(settled.overshoot(), 20);
    assert!(
        settled.overshoot() > settled.excess_bound(),
        "a deviation past the bound"
    );
    assert_eq!(charged(&ledger, &format!("run:{run}")).await.tokens, 120);

    // A reservation below the maximum declares the difference.
    let mut partial = request(&run, 2, 0, 1_000, 1_000);
    partial.maximum.tokens = 160;
    admitted(&ledger.admit(partial).await.expect("admit"));
    let attempts = ledger.attempts(&run).await.expect("attempts");
    let second = attempts
        .iter()
        .find(|a| a.request.ends_with(":2:0"))
        .expect("second");
    assert_eq!(second.excess_bound(), 60);
    assert_eq!(second.overshoot(), 0);
}

macro_rules! on_sqlite {
    ($($name:ident),* $(,)?) => {
        mod on_sqlite {
            $(
                #[tokio::test(flavor = "multi_thread")]
                async fn $name() {
                    super::$name(super::sqlite().await).await;
                }
            )*
        }
    };
}

#[cfg(feature = "postgres-tests")]
macro_rules! on_postgres {
    ($($name:ident),* $(,)?) => {
        mod on_postgres {
            $(
                #[tokio::test(flavor = "multi_thread")]
                async fn $name() {
                    super::$name(super::postgres().await).await;
                }
            )*
        }
    };
}

on_sqlite!(
    a_call_is_reserved_before_dispatch_and_settled_once,
    an_unsettled_attempt_stays_charged_until_admission_refuses,
    caps_hold_as_first_created,
    parallel_children_cannot_both_take_the_last_reservation,
    only_a_call_never_dispatched_is_released,
    usage_past_the_reservation_is_recorded_against_its_bound,
);

#[cfg(feature = "postgres-tests")]
on_postgres!(
    a_call_is_reserved_before_dispatch_and_settled_once,
    an_unsettled_attempt_stays_charged_until_admission_refuses,
    caps_hold_as_first_created,
    parallel_children_cannot_both_take_the_last_reservation,
    only_a_call_never_dispatched_is_released,
    usage_past_the_reservation_is_recorded_against_its_bound,
);
