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
use std::sync::Arc;
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

/// An engine on a schema of its own, which the returned guard drops.
#[cfg(feature = "postgres-tests")]
async fn postgres() -> (Durare, crate::testing::TestSchema) {
    let schema = crate::testing::schema("usage").await;
    let rt = Durare::postgres(schema.url(), schema.name())
        .await
        .expect("postgres engine");
    (rt, schema)
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
        longest: None,
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
    // Unknown usage leaves the tokens and dollars charged at the bound; the
    // time is what passed, which is less here than was reserved.
    assert_eq!(
        ledger
            .settle(&format!("{run}:1:0"), 2, Settlement::Unknown)
            .await
            .expect("settle"),
        Settled::Uncertain
    );
    let full = charged(&ledger, &format!("run:{run}")).await;
    assert_eq!((full.tokens, full.micro_usd), (300, 150));
    assert!(full.millis < 3_000, "{full:?}");

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
    // The refusal reserved nothing; the dead attempts' time it found, in a
    // transaction of its own, stands.
    assert_eq!(ledger.attempts(&run).await.expect("attempts").len(), 3);
    let after = charged(&ledger, &format!("run:{run}")).await;
    assert_eq!(
        (after.tokens, after.micro_usd),
        (full.tokens, full.micro_usd)
    );
    assert!(after.millis <= full.millis, "{after:?} after {full:?}");
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
    // Another atom's call, while the first is in flight: the run has no
    // time left for it.
    let mut other = request(&run, 2, 0, 1_000_000, 1_000_000);
    for scope in &mut other.chain {
        scope.caps.millis = 1_500;
    }
    match ledger.admit(other).await.expect("admit") {
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
    let atom = charged(&ledger, &format!("atom:{run}:1")).await;
    assert_eq!((atom.tokens, atom.micro_usd), (10, 2));
    assert!(atom.millis < BOUND.millis, "the time it took: {atom:?}");
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
    let Admission::Admitted {
        millis, bounded_by, ..
    } = ledger.admit(turn.clone()).await.expect("admit")
    else {
        panic!("admitted");
    };
    assert_eq!(millis, 20_000);
    assert_eq!(bounded_by, Some(format!("atom:{run}:1")));
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
    let Admission::Admitted { millis, .. } = ledger.admit(turn.clone()).await.expect("admit")
    else {
        panic!("admitted");
    };
    assert_eq!(millis, 8_000);
    ledger
        .settle(
            &format!("{run}:1:1"),
            1,
            Settlement::Reported {
                input_tokens: 1,
                output_tokens: 1,
                millis: 7_500,
                micro_usd: 0,
            },
        )
        .await
        .expect("settle");
    // 500 ms left, below one request's 1000: refused, with nothing spent.
    turn.id = format!("{run}:1:2");
    let before = charged(&ledger, &format!("atom:{run}:1")).await;
    match ledger.admit(turn).await.expect("admit") {
        Admission::Refused { scope, reason } => {
            assert_eq!(scope, format!("atom:{run}:1"));
            assert!(
                reason.contains("less active time left than one request's bound"),
                "{reason}"
            );
        }
        other => panic!("refused: {other:?}"),
    }
    assert_eq!(charged(&ledger, &format!("atom:{run}:1")).await, before);
}

async fn an_amount_a_call_reserves_none_of_is_not_held_to_its_cap<R: DurableRuntime>(rt: R) {
    let ledger = rt.ledger();
    // A call that reported more than it reserved takes its atom past its
    // token cap, its dollar cap, or both.
    for (tag, tokens_over, dollars_over) in [
        ("tokens", true, false),
        ("dollars", false, true),
        ("both", true, true),
    ] {
        let run = unique(&format!("none-of-{tag}"));
        let capped = |call: u32| {
            let cap = if tokens_over { 150 } else { 1_000 };
            let mut r = request(&run, 1, call, cap, cap);
            for scope in &mut r.chain {
                scope.caps.micro_usd = Some(if dollars_over { 60 } else { 1_000 });
            }
            r
        };
        let (req, attempt) = admitted(&ledger.admit(capped(0)).await.expect("admit"));
        ledger
            .settle(
                &req,
                attempt,
                Settlement::Reported {
                    input_tokens: 150,
                    output_tokens: 50,
                    millis: 10,
                    micro_usd: 80,
                },
            )
            .await
            .expect("settle");
        let atom = charged(&ledger, &format!("atom:{run}:1")).await;
        assert_eq!((atom.tokens, atom.micro_usd), (200, 80), "{tag}");
        // Another model call is refused on what it is over; work that
        // reserves only time, such as a command, is admitted on the time
        // left.
        match ledger.admit(capped(1)).await.expect("admit") {
            Admission::Refused { reason, .. } => {
                assert_eq!(
                    reason.contains("tokens charged"),
                    tokens_over,
                    "{tag}: {reason}"
                );
                assert_eq!(
                    reason.contains("micro-dollars charged"),
                    dollars_over,
                    "{tag}: {reason}"
                );
            }
            other => panic!("{tag}: refused: {other:?}"),
        }
        let time = Charge {
            tokens: 0,
            millis: 500,
            micro_usd: 0,
        };
        let work = Request {
            id: format!("{run}:1:2"),
            model: "(command)".into(),
            reserve: time,
            maximum: time,
            turn: true,
            ..capped(2)
        };
        match ledger.admit(work).await {
            Ok(Admission::Admitted { .. }) => {}
            other => panic!("{tag}: the command is admitted: {other:?}"),
        }
        let atom = charged(&ledger, &format!("atom:{run}:1")).await;
        assert_eq!((atom.tokens, atom.micro_usd), (200, 80), "{tag}");
    }
}

async fn a_turn_with_a_limit_of_its_own_reserves_no_more_than_it<R: DurableRuntime>(rt: R) {
    let ledger = rt.ledger();
    let run = unique("longest");
    let mut turn = request(&run, 1, 0, 1_000_000, 1_000_000);
    turn.chain[0].caps.millis = 30_000;
    turn.chain[1].caps.millis = 20_000;
    turn.turn = true;
    turn.longest = Some(5_000);
    // Its own limit, not all the 20 s its atom has left, and that limit,
    // not an allowance, bounds it.
    let Admission::Admitted {
        millis, bounded_by, ..
    } = ledger.admit(turn.clone()).await.expect("admit")
    else {
        panic!("admitted");
    };
    assert_eq!(millis, 5_000);
    assert_eq!(bounded_by, None);
    assert_eq!(
        charged(&ledger, &format!("atom:{run}:1")).await.millis,
        5_000
    );
    // It ran its five seconds. With less left than its limit, the next
    // takes what is left, bounded by the allowance.
    ledger
        .settle(
            &format!("{run}:1:0"),
            1,
            Settlement::Reported {
                input_tokens: 0,
                output_tokens: 0,
                millis: 5_000,
                micro_usd: 0,
            },
        )
        .await
        .expect("settle");
    turn.id = format!("{run}:1:1");
    turn.longest = Some(60_000);
    let Admission::Admitted {
        millis, bounded_by, ..
    } = ledger.admit(turn).await.expect("admit")
    else {
        panic!("admitted");
    };
    assert_eq!(millis, 15_000);
    assert_eq!(bounded_by, Some(format!("atom:{run}:1")));
    // A limit below one request's bound reserves that bound, still its own.
    ledger
        .settle(
            &format!("{run}:1:1"),
            1,
            Settlement::Reported {
                input_tokens: 0,
                output_tokens: 0,
                millis: 1_000,
                micro_usd: 0,
            },
        )
        .await
        .expect("settle");
    let mut short = request(&run, 1, 2, 1_000_000, 1_000_000);
    short.turn = true;
    short.longest = Some(200);
    let Admission::Admitted {
        millis, bounded_by, ..
    } = ledger.admit(short).await.expect("admit")
    else {
        panic!("admitted");
    };
    assert_eq!(millis, BOUND.millis);
    assert_eq!(bounded_by, None);
}

async fn unreported_and_dead_attempts_are_charged_the_time_they_took<R: DurableRuntime>(rt: R) {
    let ledger = rt.ledger();
    let run = unique("elapsed");
    // A turn reserving all of a long allowance reports nothing: its tokens
    // stay at the bound, its time is what passed, not the hour reserved.
    let mut turn = request(&run, 1, 0, 1_000_000, 1_000_000);
    turn.chain[0].caps.millis = 3_600_000;
    turn.chain[1].caps.millis = 3_600_000;
    turn.turn = true;
    admitted(&ledger.admit(turn.clone()).await.expect("admit"));
    assert_eq!(
        ledger
            .settle(&format!("{run}:1:0"), 1, Settlement::Unknown)
            .await
            .expect("settle"),
        Settled::Uncertain
    );
    let a = &ledger.attempts(&run).await.expect("attempts")[0];
    assert_eq!(a.state, "reserved");
    assert_eq!(a.charged.tokens, BOUND.tokens);
    assert!(a.charged.millis < 60_000, "{a:?}");
    // A second unknown settlement measures again and keeps the larger,
    // charged once.
    let settled_once = a.charged.millis;
    ledger
        .settle(&format!("{run}:1:0"), 1, Settlement::Unknown)
        .await
        .expect("settle");
    let again = ledger.attempts(&run).await.expect("attempts")[0]
        .charged
        .millis;
    assert!(
        again >= settled_once && again < 60_000,
        "{again} after {settled_once}"
    );
    assert_eq!(
        charged(&ledger, &format!("atom:{run}:1")).await.millis,
        again
    );
    // An attempt whose process died is never settled; when its request is
    // admitted again, it is charged the time it took, so the new attempt
    // has room.
    turn.id = format!("{run}:1:1");
    admitted(&ledger.admit(turn.clone()).await.expect("admit"));
    let Admission::Admitted { millis, .. } = ledger.admit(turn).await.expect("admit") else {
        panic!("the re-run is admitted");
    };
    assert!(millis > 3_000_000, "most of the hour is left: {millis}");
    let attempts = ledger.attempts(&run).await.expect("attempts");
    let dead = attempts
        .iter()
        .find(|a| a.request.ends_with(":1:1") && a.attempt == 1)
        .expect("the dead attempt");
    assert_eq!(dead.state, "reserved");
    assert!(dead.charged.millis < 60_000, "{dead:?}");
    assert_eq!(dead.charged.tokens, BOUND.tokens);
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
        assert!(
            ledger.raise(&format!("run:{run}"), caps).await.is_err(),
            "{bad}"
        );
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

async fn a_dead_call_is_never_charged_its_processs_downtime<R: DurableRuntime>(rt: R) {
    use std::sync::atomic::AtomicI64;
    const MINUTE: i64 = 60_000;
    let t = Arc::new(AtomicI64::new(1_000_000_000));
    let clock = t.clone();
    let ledger = rt.ledger().with_clock(move || clock.load(Ordering::SeqCst));
    let run = unique("downtime");
    // A turn that reserves its atom's whole half hour, under a run with
    // room for one call's tokens.
    let mut turn = request(&run, 1, 0, 100, 1_000_000);
    turn.chain[0].caps.millis = 60 * MINUTE;
    turn.chain[1].caps.millis = 30 * MINUTE;
    turn.turn = true;
    admitted(&ledger.admit(turn.clone()).await.expect("admit"));
    // It runs two minutes, saying so, and its process dies.
    t.fetch_add(2 * MINUTE, Ordering::SeqCst);
    ledger
        .heartbeat(&format!("{run}:1:0"), 1)
        .await
        .expect("heartbeat");
    // Recovered 25 minutes later, its atom admits its next call: the dead
    // turn is charged until a heartbeat after its last, not the downtime.
    t.fetch_add(25 * MINUTE, Ordering::SeqCst);
    turn.id = format!("{run}:1:1");
    let refused = ledger.admit(turn.clone()).await.expect("admit");
    // The run has no tokens left for it, so it is refused; the time the
    // dead turn was found to have taken stands all the same.
    assert!(matches!(refused, Admission::Refused { .. }), "{refused:?}");
    let dead = |attempts: Vec<Attempt>| {
        attempts
            .into_iter()
            .find(|a| a.request.ends_with(":1:0"))
            .expect("the dead turn")
    };
    let every = heartbeat_every(30 * MINUTE);
    let held = dead(ledger.attempts(&run).await.expect("attempts")).charged;
    assert_eq!(held.millis, 2 * MINUTE + every, "{held:?}");
    assert_eq!(held.tokens, BOUND.tokens, "its tokens stay at the bound");
    assert_eq!(
        charged(&ledger, &format!("atom:{run}:1")).await.millis,
        2 * MINUTE + every
    );
    // Found dead again, it is charged no more.
    ledger.admit(turn.clone()).await.expect("admit");
    assert_eq!(
        dead(ledger.attempts(&run).await.expect("attempts"))
            .charged
            .millis,
        2 * MINUTE + every
    );
    // Its owner was alive after all, and settles it as unknown at 28
    // minutes: the larger measure stands, charged once.
    t.fetch_add(MINUTE, Ordering::SeqCst);
    ledger
        .settle(&format!("{run}:1:0"), 1, Settlement::Unknown)
        .await
        .expect("settle");
    assert_eq!(
        dead(ledger.attempts(&run).await.expect("attempts"))
            .charged
            .millis,
        28 * MINUTE
    );
    assert_eq!(
        charged(&ledger, &format!("atom:{run}:1")).await.millis,
        28 * MINUTE
    );
}

async fn a_partly_known_attempt_is_charged_its_reported_usage_and_one_more_bound<
    R: DurableRuntime,
>(
    rt: R,
) {
    let ledger = rt.ledger();
    let run = unique("partial");
    admitted(
        &ledger
            .admit(request(&run, 1, 0, 1_000_000, 1_000_000))
            .await
            .expect("admit"),
    );
    let in_flight = Charge {
        tokens: 30,
        millis: 0,
        micro_usd: 20,
    };
    // Reported past its reservation, before a request in flight: the
    // reported usage and the in-flight bound, recorded, and still open.
    assert_eq!(
        ledger
            .settle(
                &format!("{run}:1:0"),
                1,
                Settlement::Partial {
                    input_tokens: 400,
                    output_tokens: 50,
                    micro_usd: 70,
                    in_flight,
                },
            )
            .await
            .expect("settle"),
        Settled::Uncertain
    );
    let a = &ledger.attempts(&run).await.expect("attempts")[0];
    assert_eq!(a.state, "reserved", "open to reconciliation");
    assert_eq!(a.usage, Some((400, 50)));
    assert_eq!((a.charged.tokens, a.charged.micro_usd), (480, 90));
    assert_eq!(a.overshoot(), 480 - BOUND.tokens);
    assert_eq!(charged(&ledger, &format!("atom:{run}:1")).await.tokens, 480);
    assert_eq!(charged(&ledger, &format!("run:{run}")).await.tokens, 480);
    // Reported below its reservation: the reservation, at least.
    let small = unique("partial-small");
    admitted(
        &ledger
            .admit(request(&small, 1, 0, 1_000_000, 1_000_000))
            .await
            .expect("admit"),
    );
    ledger
        .settle(
            &format!("{small}:1:0"),
            1,
            Settlement::Partial {
                input_tokens: 5,
                output_tokens: 5,
                micro_usd: 1,
                in_flight,
            },
        )
        .await
        .expect("settle");
    let a = &ledger.attempts(&small).await.expect("attempts")[0];
    assert_eq!(
        (a.charged.tokens, a.charged.micro_usd),
        (BOUND.tokens, BOUND.micro_usd)
    );
    assert_eq!(a.usage, Some((5, 5)));
    // It reported usage, so it spent something: it cannot be released.
    assert!(ledger.release(&format!("{small}:1:0"), 1).await.is_err());
    assert_eq!(
        ledger.attempts(&small).await.expect("attempts")[0].state,
        "reserved"
    );
    // Its whole usage, entered once learned, replaces the charge.
    ledger
        .settle(
            &format!("{run}:1:0"),
            1,
            Settlement::Reconciled {
                input_tokens: 420,
                output_tokens: 55,
                micro_usd: 80,
            },
        )
        .await
        .expect("reconcile");
    let a = &ledger.attempts(&run).await.expect("attempts")[0];
    assert_eq!((a.state.as_str(), a.charged.tokens), ("settled", 475));
    assert_eq!(charged(&ledger, &format!("atom:{run}:1")).await.tokens, 475);
}

/// A turn's record of what it reported so far: `input` and `output`
/// tokens costing `micro` micro-dollars, and a request in flight of
/// `in_flight` tokens and ten micro-dollars.
fn so_far(input: i64, output: i64, micro: i64, in_flight: i64) -> Settlement {
    Settlement::Partial {
        input_tokens: input,
        output_tokens: output,
        micro_usd: micro,
        in_flight: Charge {
            tokens: in_flight,
            millis: 0,
            micro_usd: 10,
        },
    }
}

async fn progress_is_charged_until_the_settlement_replaces_it<R: DurableRuntime>(rt: R) {
    let ledger = rt.ledger();
    let run = unique("progress");
    let (req, attempt) = admitted(
        &ledger
            .admit(request(&run, 1, 0, 10_000, 10_000))
            .await
            .expect("admit"),
    );
    let (run_scope, atom_scope) = (format!("run:{run}"), format!("atom:{run}:1"));
    // Past its 100-token reservation: what it reported and the request in
    // flight, and the attempt stays open.
    assert_eq!(
        ledger
            .progress(&req, attempt, so_far(100, 50, 60, 100))
            .await
            .expect("progress"),
        Settled::Uncertain
    );
    let a = &ledger.attempts(&run).await.expect("attempts")[0];
    assert_eq!(a.state, "reserved");
    assert_eq!(a.usage, Some((100, 50)));
    assert_eq!((a.charged.tokens, a.charged.micro_usd), (250, 70));
    for scope in [&run_scope, &atom_scope] {
        let c = charged(&ledger, scope).await;
        assert_eq!((c.tokens, c.micro_usd), (250, 70), "{scope}");
    }
    // A smaller record, as a late one would be, lowers nothing.
    ledger
        .progress(&req, attempt, so_far(10, 5, 1, 0))
        .await
        .expect("progress");
    let a = &ledger.attempts(&run).await.expect("attempts")[0];
    assert_eq!(a.usage, Some((100, 50)));
    assert_eq!((a.charged.tokens, a.charged.micro_usd), (250, 70));
    assert_eq!(charged(&ledger, &run_scope).await.tokens, 250);
    // It cannot be released: it reported usage.
    assert!(ledger.release(&req, attempt).await.is_err());
    // The settlement replaces the record with what it used.
    let used = Settlement::Reported {
        input_tokens: 120,
        output_tokens: 60,
        millis: 10,
        micro_usd: 80,
    };
    assert_eq!(
        ledger.settle(&req, attempt, used).await.expect("settle"),
        Settled::Now
    );
    for scope in [&run_scope, &atom_scope] {
        let c = charged(&ledger, scope).await;
        assert_eq!((c.tokens, c.micro_usd), (180, 80), "{scope}");
    }
    // A record after the settlement changes nothing.
    assert_eq!(
        ledger
            .progress(&req, attempt, so_far(500, 500, 500, 100))
            .await
            .expect("progress"),
        Settled::Already
    );
    assert_eq!(charged(&ledger, &run_scope).await.tokens, 180);
    // Progress is only ever a partial settlement.
    let (other, attempt) = admitted(
        &ledger
            .admit(request(&run, 2, 0, 10_000, 10_000))
            .await
            .expect("admit"),
    );
    assert!(
        ledger
            .progress(&other, attempt, Settlement::Unknown)
            .await
            .is_err()
    );
}

async fn progress_racing_its_settlement_leaves_what_was_used<R: DurableRuntime>(rt: R) {
    let ledger = rt.ledger();
    let run = unique("race_progress");
    for call in 0..10 {
        let (req, attempt) = admitted(
            &ledger
                .admit(request(&run, 1, call, 1_000_000, 1_000_000))
                .await
                .expect("admit"),
        );
        let (recording, settling) = (ledger.clone(), ledger.clone());
        let (r1, r2) = (req.clone(), req.clone());
        let record = tokio::spawn(async move {
            recording
                .progress(&r1, attempt, so_far(300, 300, 300, 100))
                .await
        });
        let settle = tokio::spawn(async move {
            settling
                .settle(
                    &r2,
                    attempt,
                    Settlement::Reported {
                        input_tokens: 1,
                        output_tokens: 1,
                        millis: 1,
                        micro_usd: 1,
                    },
                )
                .await
        });
        record.await.expect("task").expect("progress");
        settle.await.expect("task").expect("settle");
    }
    // Whichever came first, each call is charged what it used: a record
    // after the settlement is refused, and a settlement after a record
    // replaces it.
    let c = charged(&ledger, &format!("run:{run}")).await;
    assert_eq!((c.tokens, c.micro_usd), (20, 10));
    assert!(
        ledger
            .attempts(&run)
            .await
            .expect("attempts")
            .iter()
            .all(|a| a.state == "settled" && a.usage == Some((1, 1)))
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
                    // The schema outlives the engine.
                    let (rt, _schema) = super::postgres().await;
                    super::$name(rt).await;
                }
            )*
        }
    };
}

on_sqlite!(
    progress_is_charged_until_the_settlement_replaces_it,
    progress_racing_its_settlement_leaves_what_was_used,
    a_call_is_reserved_before_dispatch_and_settled_once,
    a_dead_call_is_never_charged_its_processs_downtime,
    unreported_and_dead_attempts_are_charged_the_time_they_took,
    a_partly_known_attempt_is_charged_its_reported_usage_and_one_more_bound,
    a_turn_reserves_the_active_time_left,
    a_turn_with_a_limit_of_its_own_reserves_no_more_than_it,
    an_amount_a_call_reserves_none_of_is_not_held_to_its_cap,
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
    progress_is_charged_until_the_settlement_replaces_it,
    progress_racing_its_settlement_leaves_what_was_used,
    a_call_is_reserved_before_dispatch_and_settled_once,
    a_dead_call_is_never_charged_its_processs_downtime,
    unreported_and_dead_attempts_are_charged_the_time_they_took,
    a_partly_known_attempt_is_charged_its_reported_usage_and_one_more_bound,
    a_turn_reserves_the_active_time_left,
    a_turn_with_a_limit_of_its_own_reserves_no_more_than_it,
    an_amount_a_call_reserves_none_of_is_not_held_to_its_cap,
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
