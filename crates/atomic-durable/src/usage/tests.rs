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
//! - usage past the reservation is recorded, against its declared bound;
//! - admission refuses on active time and on dollars as it does on tokens;
//! - an attempt left reserved can be reconciled or released once, and a
//!   person can raise a cap, never lower one;
//! - a refusal's reason comes from the state it refused, however many
//!   admissions and settlements race it.

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
    // A suite that cannot reach its backend fails; it never skips.
    let url = std::env::var("ATOMIC_TEST_DATABASE_URL")
        .expect("the postgres-tests feature needs ATOMIC_TEST_DATABASE_URL");
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
        turn: false,
    }
}

fn admitted(a: &Admission) -> (String, i64) {
    match a {
        Admission::Admitted {
            request, attempt, ..
        } => (request.clone(), *attempt),
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

async fn admission_refuses_on_time_and_on_dollars<R: DurableRuntime>(rt: R) {
    let ledger = rt.ledger();
    // Room for any number of tokens, but for one call's time.
    let run = unique("millis");
    let mut timed = request(&run, 1, 0, 1_000_000, 1_000_000);
    for scope in &mut timed.chain {
        scope.caps.millis = 1_500;
    }
    admitted(&ledger.admit(timed.clone()).await.expect("admit"));
    timed.id = format!("{run}:1:1");
    match ledger.admit(timed).await.expect("admit") {
        Admission::Refused { reason, .. } => {
            assert!(reason.contains("1000 of 1500 ms"), "{reason}");
            assert!(!reason.contains("tokens charged"), "{reason}");
        }
        other => panic!("refused on time: {other:?}"),
    }
    // Room for any number of tokens and any time, but for one call's cost.
    let run = unique("micro");
    let mut priced = request(&run, 1, 0, 1_000_000, 1_000_000);
    for scope in &mut priced.chain {
        scope.caps.micro_usd = Some(80);
    }
    admitted(&ledger.admit(priced.clone()).await.expect("admit"));
    priced.id = format!("{run}:1:1");
    match ledger.admit(priced.clone()).await.expect("admit") {
        Admission::Refused { reason, .. } => {
            assert!(reason.contains("50 of 80 micro-dollars"), "{reason}");
        }
        other => panic!("refused on cost: {other:?}"),
    }
    // Settled at its real cost, the first call leaves room for the second.
    ledger
        .settle(
            &format!("{run}:1:0"),
            1,
            Settlement::Reported {
                input_tokens: 1,
                output_tokens: 1,
                millis: 1,
                micro_usd: 3,
            },
        )
        .await
        .expect("settle");
    assert_eq!(charged(&ledger, &format!("run:{run}")).await.micro_usd, 3);
    admitted(&ledger.admit(priced).await.expect("admit"));
}

async fn a_reserved_attempt_is_reconciled_or_released_once<R: DurableRuntime>(rt: R) {
    let ledger = rt.ledger();
    let run = unique("reconcile");
    for call in 0..2 {
        admitted(
            &ledger
                .admit(request(&run, 1, call, 1_000, 1_000))
                .await
                .expect("admit"),
        );
    }
    assert_eq!(ledger.unsettled(&run).await.expect("unsettled").len(), 2);
    // Its usage learned later: charged as used, its time as reserved.
    let learned = Settlement::Reconciled {
        input_tokens: 7,
        output_tokens: 3,
        micro_usd: 2,
    };
    let first = format!("{run}:1:0");
    assert_eq!(
        ledger.settle(&first, 1, learned).await.expect("reconcile"),
        Settled::Now
    );
    assert_eq!(
        ledger.settle(&first, 1, learned).await.expect("again"),
        Settled::Already
    );
    // Confirmed unused: released.
    let second = format!("{run}:1:1");
    assert_eq!(
        ledger.release(&second, 1).await.expect("release"),
        Settled::Now
    );
    assert_eq!(
        ledger.release(&second, 1).await.expect("again"),
        Settled::Already
    );
    assert!(ledger.unsettled(&run).await.expect("unsettled").is_empty());
    assert_eq!(
        charged(&ledger, &format!("atom:{run}:1")).await,
        Charge {
            tokens: 10,
            millis: BOUND.millis,
            micro_usd: 2,
        }
    );
}

async fn a_person_raises_a_cap_and_never_lowers_one<R: DurableRuntime>(rt: R) {
    let ledger = rt.ledger();
    let run = unique("raise");
    admitted(
        &ledger
            .admit(request(&run, 1, 0, 100, 10_000))
            .await
            .expect("admit"),
    );
    let scope = format!("run:{run}");
    assert!(matches!(
        ledger
            .admit(request(&run, 1, 1, 100, 10_000))
            .await
            .expect("admit"),
        Admission::Refused { .. }
    ));
    let raised = ledger
        .raise(
            &scope,
            Caps {
                tokens: 250,
                millis: 10,
                micro_usd: Some(1),
            },
        )
        .await
        .expect("raise")
        .expect("exists");
    // Tokens rose; time is not lowered; no dollar cap is added.
    assert_eq!(
        raised.caps,
        Caps {
            tokens: 250,
            millis: 1_000_000,
            micro_usd: None,
        }
    );
    admitted(
        &ledger
            .admit(request(&run, 1, 1, 100, 10_000))
            .await
            .expect("admit"),
    );
    assert_eq!(
        ledger
            .raise(&format!("run:{run}-none"), caps(1))
            .await
            .expect("raise"),
        None
    );
}

async fn a_refusal_is_explained_by_the_state_it_refused<R: DurableRuntime>(rt: R) {
    let ledger = rt.ledger();
    for round in 0..5 {
        let run = unique(&format!("reason{round}"));
        // Room for three bounds at once. Children admit and settle at once,
        // so the room changes all the time; every refusal must still say
        // which cap had no room and how much was charged.
        let children: Vec<_> = (1..=8)
            .map(|step| {
                let ledger = ledger.clone();
                let req = request(&run, step, 0, 300, 10_000);
                tokio::spawn(async move {
                    match ledger.admit(req).await.expect("admit") {
                        Admission::Admitted {
            request, attempt, ..
        } => {
                            ledger
                                .settle(
                                    &request,
                                    attempt,
                                    Settlement::Reported {
                                        input_tokens: 1,
                                        output_tokens: 1,
                                        millis: 1,
                                        micro_usd: 1,
                                    },
                                )
                                .await
                                .expect("settle");
                            None
                        }
                        Admission::Refused { reason, .. } => Some(reason),
                    }
                })
            })
            .collect();
        for child in children {
            if let Some(reason) = child.await.expect("task") {
                let explained = reason
                    .split_once(": ")
                    .map(|(_, why)| why)
                    .unwrap_or_default();
                assert!(
                    explained.contains("tokens charged, and the call reserves 100"),
                    "a refusal with its reason: {reason:?}"
                );
            }
        }
    }
}

async fn a_turn_reserves_the_active_time_left<R: DurableRuntime>(rt: R) {
    let ledger = rt.ledger();
    let run = unique("turn");
    let mut turn = request(&run, 1, 0, 1_000_000, 1_000_000);
    turn.chain[0].caps.millis = 30_000;
    turn.chain[1].caps.millis = 20_000;
    turn.turn = true;
    // All the time the narrowest allowance has, not the call bound's.
    let Admission::Admitted { millis, .. } = ledger.admit(turn.clone()).await.expect("admit") else {
        panic!("admitted");
    };
    assert_eq!(millis, 20_000);
    ledger
        .settle(
            &format!("{run}:1:0"),
            1,
            Settlement::Reported {
                input_tokens: 1,
                output_tokens: 1,
                millis: 12_000,
                micro_usd: 0,
            },
        )
        .await
        .expect("settle");
    // The next turn gets what is left.
    turn.id = format!("{run}:1:1");
    let Admission::Admitted { millis, .. } = ledger.admit(turn.clone()).await.expect("admit") else {
        panic!("admitted");
    };
    assert_eq!(millis, 8_000);
    // With none left, it is refused.
    turn.id = format!("{run}:1:2");
    match ledger.admit(turn).await.expect("admit") {
        Admission::Refused { scope, reason } => {
            assert_eq!(scope, format!("atom:{run}:1"));
            assert!(reason.contains("no active time left"), "{reason}");
        }
        other => panic!("refused: {other:?}"),
    }
}

async fn amounts_out_of_range_are_refused_and_reported_usage_is_held<R: DurableRuntime>(rt: R) {
    let ledger = rt.ledger();
    let run = unique("range");
    for bad in [-1, MOST + 1] {
        let mut req = request(&run, 1, 0, 1_000, 1_000);
        req.reserve.tokens = bad;
        assert!(ledger.admit(req).await.is_err(), "reserve {bad}");
        let mut req = request(&run, 1, 0, 1_000, 1_000);
        req.chain[1].caps.millis = bad;
        assert!(ledger.admit(req).await.is_err(), "cap {bad}");
        let mut req = request(&run, 1, 0, 1_000, 1_000);
        req.chain[0].caps.micro_usd = Some(bad);
        assert!(ledger.admit(req).await.is_err(), "dollar cap {bad}");
    }
    assert!(
        ledger.attempts(&run).await.expect("attempts").is_empty(),
        "nothing was written"
    );
    admitted(
        &ledger
            .admit(request(&run, 1, 0, MOST, MOST))
            .await
            .expect("admit"),
    );
    for bad in [-1, MOST + 1] {
        let caps = Caps {
            tokens: bad,
            millis: 1,
            micro_usd: None,
        };
        assert!(ledger.raise(&format!("run:{run}"), caps).await.is_err(), "{bad}");
    }
    // Usage past MOST, or below zero, is held to [0, MOST].
    ledger
        .settle(
            &format!("{run}:1:0"),
            1,
            Settlement::Reported {
                input_tokens: i64::MAX,
                output_tokens: -5,
                millis: -1,
                micro_usd: i64::MAX,
            },
        )
        .await
        .expect("settle");
    let a = &ledger.attempts(&run).await.expect("attempts")[0];
    assert_eq!(a.usage, Some((MOST, 0)));
    assert_eq!(
        a.charged,
        Charge {
            tokens: MOST,
            millis: 0,
            micro_usd: MOST,
        }
    );
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
    a_turn_reserves_the_active_time_left,
    amounts_out_of_range_are_refused_and_reported_usage_is_held,
    admission_refuses_on_time_and_on_dollars,
    a_reserved_attempt_is_reconciled_or_released_once,
    a_person_raises_a_cap_and_never_lowers_one,
    a_refusal_is_explained_by_the_state_it_refused,
    an_unsettled_attempt_stays_charged_until_admission_refuses,
    caps_hold_as_first_created,
    parallel_children_cannot_both_take_the_last_reservation,
    only_a_call_never_dispatched_is_released,
    usage_past_the_reservation_is_recorded_against_its_bound,
);

#[cfg(feature = "postgres-tests")]
on_postgres!(
    a_call_is_reserved_before_dispatch_and_settled_once,
    a_turn_reserves_the_active_time_left,
    amounts_out_of_range_are_refused_and_reported_usage_is_held,
    admission_refuses_on_time_and_on_dollars,
    a_reserved_attempt_is_reconciled_or_released_once,
    a_person_raises_a_cap_and_never_lowers_one,
    a_refusal_is_explained_by_the_state_it_refused,
    an_unsettled_attempt_stays_charged_until_admission_refuses,
    caps_hold_as_first_created,
    parallel_children_cannot_both_take_the_last_reservation,
    only_a_call_never_dispatched_is_released,
    usage_past_the_reservation_is_recorded_against_its_bound,
);
