// SPDX-License-Identifier: Apache-2.0

//! Metering atoms' model calls against the durable usage ledger.
//!
//! A [`RunBudget`] holds a workflow run's resolved caps and the ledger. For
//! each atom step it makes a [`Metering`] that admits every model call
//! against the atom's own allowance and the run's, under a request id made
//! from the run, the step's stable position and the call's index, so a
//! re-run of the step after a crash makes new attempts at the same requests
//! instead of new requests. Each admission reserves the call's whole bound
//! from the price table and call bound in [`BudgetSettings`], and the call
//! is held to it: it is sent with the bound's output tokens as its output
//! limit and cut off when the bound's time runs out. Each settlement records
//! what the provider reported, priced the same way.
//!
//! A run's caps, prices and call bounds are resolved once, before it starts,
//! into a [`BudgetPlan`] the run keeps: a resumed run is metered as it began,
//! whatever the settings say by then.

use atomic_core::budget::{
    Budget, BudgetError, BudgetSettings, CallBound, Level, Price, Resolved, resolve,
};
use atomic_core::usage::{
    Call, Exhausted, Limit, Meter, MeterError, MeterFuture, Metering, Outcome, Ticket, Usage,
};
use atomic_durable::StepInfo;
use atomic_durable::usage::{
    Admission, Caps, Charge, Ledger, Request, Scope, Settlement, heartbeat_every,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

/// A run's budgets, resolved once: the run's caps, each model atom's caps
/// and model by name, and the call bound and prices of the models the run
/// calls. A run keeps the plan it started with, and each atom runs on the
/// model the plan names for it. Stored with the run, so it reads past fields
/// a later version adds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BudgetPlan {
    /// The run's caps, children included.
    pub run: Resolved,
    /// Each model atom's caps, by atom name.
    pub atoms: BTreeMap<String, Resolved>,
    /// The model each model atom calls, by atom name.
    pub models: BTreeMap<String, String>,
    /// The most one call may use, when its model has no maximum of its own.
    pub call: PlannedCall,
    /// The price table, cut to the models the run calls.
    #[serde(default)]
    pub pricing: BTreeMap<String, PlannedPrice>,
}

/// [`atomic_core::budget::CallBound`], as a plan keeps it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlannedCall {
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens, including reasoning.
    pub output_tokens: u64,
    /// Active minutes.
    pub minutes: f64,
}

/// [`Price`], as a plan keeps it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlannedPrice {
    /// US dollars per million input tokens.
    pub input_usd_per_mtok: f64,
    /// US dollars per million output tokens.
    pub output_usd_per_mtok: f64,
    /// The most input tokens one call takes, if not the call bound's.
    #[serde(default)]
    pub max_input_tokens: Option<u64>,
    /// The most output tokens one call returns, if not the call bound's.
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
}

impl BudgetPlan {
    /// Resolve the plan of a run with its own limits `declared` whose model
    /// atoms are `atoms`, each by name with its limits and model, against
    /// `settings`. Fails on a budget that cannot be resolved, such as a
    /// dollar cap on an unpriced model or a cap with no room for one call.
    pub fn resolve(
        settings: &BudgetSettings,
        declared: &Budget,
        atoms: &[(&str, &Budget, &str)],
    ) -> Result<Self, (Option<String>, BudgetError)> {
        let models: Vec<&str> = atoms.iter().map(|(_, _, model)| *model).collect();
        let run = resolve(declared, Level::Workflow, settings, &models).map_err(|e| (None, e))?;
        let mut resolved = BTreeMap::new();
        for (name, budget, model) in atoms {
            let caps = resolve(budget, Level::Atom, settings, &[model])
                .map_err(|e| (Some((*name).to_owned()), e))?;
            resolved.insert((*name).to_owned(), caps);
        }
        Ok(Self {
            run,
            atoms: resolved,
            models: atoms
                .iter()
                .map(|(name, _, model)| ((*name).to_owned(), (*model).to_owned()))
                .collect(),
            call: PlannedCall {
                input_tokens: settings.call.input_tokens,
                output_tokens: settings.call.output_tokens,
                minutes: settings.call.minutes,
            },
            pricing: settings
                .pricing
                .iter()
                .filter(|(model, _)| models.contains(&model.as_str()))
                .map(|(model, p)| {
                    (
                        model.clone(),
                        PlannedPrice {
                            input_usd_per_mtok: p.input_usd_per_mtok,
                            output_usd_per_mtok: p.output_usd_per_mtok,
                            max_input_tokens: p.max_input_tokens,
                            max_output_tokens: p.max_output_tokens,
                        },
                    )
                })
                .collect(),
        })
    }

    /// The call bound and prices the plan meters under.
    pub fn settings(&self) -> BudgetSettings {
        BudgetSettings {
            call: CallBound {
                input_tokens: self.call.input_tokens,
                output_tokens: self.call.output_tokens,
                minutes: self.call.minutes,
            },
            pricing: self
                .pricing
                .iter()
                .map(|(model, p)| {
                    (
                        model.clone(),
                        Price {
                            input_usd_per_mtok: p.input_usd_per_mtok,
                            output_usd_per_mtok: p.output_usd_per_mtok,
                            max_input_tokens: p.max_input_tokens,
                            max_output_tokens: p.max_output_tokens,
                        },
                    )
                })
                .collect(),
            ..BudgetSettings::default()
        }
    }

    /// Check that the plan has caps and a model for each of `atoms`, every
    /// atom a run calls a model from.
    pub fn covers(&self, atoms: &[&str]) -> Result<(), String> {
        for atom in atoms {
            if !self.atoms.contains_key(*atom) || !self.models.contains_key(*atom) {
                return Err(format!(
                    "atom {atom} calls a model, and the plan has no caps or model for it"
                ));
            }
        }
        Ok(())
    }

    /// Check a plan that came from outside, such as one pinned in a run's
    /// input: its settings in range, every cap leaving room for one call to
    /// its models, and a dollar cap only over priced models.
    pub fn validate(&self) -> Result<(), String> {
        let settings = self.settings();
        settings.validate().map_err(|e| e.to_string())?;
        let priced = |model: &str, caps: &Resolved, whose: &str| {
            if caps.micro_usd.is_some() && !settings.pricing.contains_key(model) {
                let unpriced = BudgetError::Unpriced {
                    model: model.to_owned(),
                };
                return Err(format!("{whose}: {unpriced}"));
            }
            Ok(())
        };
        for (atom, model) in &self.models {
            let caps = self.atoms.get(atom).ok_or_else(|| {
                format!("atom {atom} calls {model:?} and the plan has no caps for it")
            })?;
            settings
                .check_room(caps, Level::Atom, model)
                .map_err(|e| format!("atom {atom}: {e}"))?;
            settings
                .check_room(&self.run, Level::Workflow, model)
                .map_err(|e| format!("the run: {e}"))?;
            priced(model, caps, &format!("atom {atom}"))?;
            priced(model, &self.run, "the run")?;
        }
        Ok(())
    }
}

/// A run's caps, for metering its atoms.
#[derive(Clone, Debug)]
pub struct RunBudget {
    ledger: Ledger,
    settings: Arc<BudgetSettings>,
    run: Resolved,
    /// The allowances the run sits under, root first, as a child run sits
    /// under its parent's.
    parents: Vec<Scope>,
}

/// A count as the ledger holds it: at most its largest amount.
fn int(v: u64) -> i64 {
    i64::try_from(v).map_or(atomic_durable::usage::MOST, |n| {
        n.min(atomic_durable::usage::MOST)
    })
}

/// Caps as the ledger holds them.
fn caps(r: &Resolved) -> Caps {
    Caps {
        tokens: int(r.tokens),
        millis: int(r.millis),
        micro_usd: r.micro_usd.map(int),
    }
}

impl RunBudget {
    /// The caps of a run that may call `models`, its own limits from
    /// `declared` and the rest from `settings`. Fails, before anything runs,
    /// on a dollar cap the price table cannot back.
    pub fn new(
        ledger: Ledger,
        settings: Arc<BudgetSettings>,
        declared: &Budget,
        models: &[&str],
    ) -> Result<Self, BudgetError> {
        let run = resolve(declared, Level::Workflow, &settings, models)?;
        Ok(Self {
            ledger,
            settings,
            run,
            parents: Vec::new(),
        })
    }

    /// The run's resolved caps.
    pub fn caps(&self) -> &Resolved {
        &self.run
    }

    /// The caps of a run metered under `plan`, as it was resolved.
    pub fn pinned(ledger: Ledger, plan: &BudgetPlan) -> Self {
        Self {
            ledger,
            settings: Arc::new(plan.settings()),
            run: plan.run.clone(),
            parents: Vec::new(),
        }
    }

    /// The same caps, for a run that sits under `parent`: every call is also
    /// charged against it, before the run's own allowance, so children of
    /// one parent share its room and cannot both take its last reservation.
    pub fn under(mut self, parent: Scope) -> Self {
        self.parents.push(parent);
        self
    }

    /// The caps of an atom calling `model` with its own limits `declared`.
    pub fn atom(&self, declared: &Budget, model: &str) -> Result<Resolved, BudgetError> {
        resolve(declared, Level::Atom, &self.settings, &[model])
    }

    /// The meter for the atom step `step`, with caps `atom`.
    pub fn meter(&self, step: &StepInfo, atom: &Resolved) -> Metering {
        self.meter_charged(step, step.step_id, atom)
    }

    /// As [`RunBudget::meter`], charging the atom allowance of the turn at
    /// `allowance` rather than `step`'s own: a review's repair rounds share
    /// the author's first turn's allowance, and its later verdicts the
    /// first verdict's. Each call is still its own ledger request, keyed by
    /// `step`.
    pub fn meter_charged(&self, step: &StepInfo, allowance: i32, atom: &Resolved) -> Metering {
        let run = step.workflow_id.as_str();
        Metering::new(Arc::new(LedgerMeter {
            ledger: self.ledger.clone(),
            settings: self.settings.clone(),
            run: run.to_owned(),
            step: step.step_id,
            chain: self
                .parents
                .iter()
                .cloned()
                .chain([
                    Scope::run(run, caps(&self.run)),
                    Scope::atom(run, allowance, caps(atom)),
                ])
                .collect(),
            dollar_capped: self.run.micro_usd.is_some() || atom.micro_usd.is_some(),
        }))
    }
}

/// One atom step's meter.
struct LedgerMeter {
    ledger: Ledger,
    settings: Arc<BudgetSettings>,
    run: String,
    step: i32,
    chain: Vec<Scope>,
    /// Whether a dollar cap applies: then every call must be priced.
    dollar_capped: bool,
}

fn store(e: impl std::fmt::Display) -> MeterError {
    MeterError::Store(e.to_string())
}

impl LedgerMeter {
    /// A call to `model` that reported `reported` and may have a request in
    /// flight: charged the reported usage and that request, taken as one
    /// request's bound, or as `last`, the last request that reported, if
    /// that was larger.
    fn partial(&self, model: &str, reported: Usage, last: Option<Usage>) -> Settlement {
        let tokens = |v: i64| u64::try_from(v).unwrap_or(0);
        let bound = self.settings.reservation(model);
        let (last_tokens, last_micro) = last.map_or((0, 0), |l| {
            (
                tokens(l.input_tokens).saturating_add(tokens(l.output_tokens)),
                self.settings
                    .cost(model, tokens(l.input_tokens), tokens(l.output_tokens)),
            )
        });
        Settlement::Partial {
            input_tokens: reported.input_tokens,
            output_tokens: reported.output_tokens,
            micro_usd: int(self.settings.cost(
                model,
                tokens(reported.input_tokens),
                tokens(reported.output_tokens),
            )),
            in_flight: Charge {
                tokens: int(bound.tokens.max(last_tokens)),
                millis: 0,
                micro_usd: int(bound.micro_usd.max(last_micro)),
            },
        }
    }
}

impl Meter for LedgerMeter {
    fn admit(&self, call: Call) -> MeterFuture<'_, Ticket> {
        Box::pin(async move {
            // A dollar cap can hold only over priced calls: a call to a model
            // the plan does not price, as when the model changed since the
            // budget was resolved, is not made.
            if self.dollar_capped && !self.settings.pricing.contains_key(&call.model) {
                return Err(MeterError::Unaccountable(format!(
                    "a dollar cap applies, and model {:?} has no price in the run's budget; \
                     the call is not made",
                    call.model
                )));
            }
            let bound = self.settings.reservation(&call.model);
            // The whole bound is reserved, and the call is held to it: its
            // output limit and its time are the bound's. Its input is what
            // the caller sends; where the caller can bound it, a request
            // larger than the bound's input is declared in the maximum, so
            // the excess the ledger records as possible is true.
            let whole = Charge {
                tokens: int(bound.tokens),
                millis: int(bound.millis),
                micro_usd: int(bound.micro_usd),
            };
            let input = call
                .input_bound
                .map_or(bound.input_tokens, |b| b.max(bound.input_tokens));
            let maximum = Charge {
                tokens: int(input.saturating_add(bound.output_tokens)),
                millis: whole.millis,
                micro_usd: int(self.settings.cost(&call.model, input, bound.output_tokens)),
            };
            let request = Request {
                id: format!("{}:{}:{}", self.run, self.step, call.index),
                run: self.run.clone(),
                model: call.model.clone(),
                chain: self.chain.clone(),
                reserve: whole,
                maximum,
                turn: call.turn,
            };
            match self.ledger.admit(request).await.map_err(store)? {
                Admission::Admitted {
                    request,
                    attempt,
                    millis,
                    bounded_by,
                } => {
                    // While the call runs, it says so, so a call whose process
                    // dies is charged no time past its last word.
                    beat(&self.ledger, &request, attempt, millis);
                    // Admitted and reserved, and not yet dispatched.
                    if atomic_durable::failpoints::armed() {
                        atomic_durable::failpoint(&format!("atom:after-admit:{request}"));
                    }
                    Ok(Ticket {
                        request,
                        attempt,
                        model: call.model,
                        admitted: Instant::now(),
                        limit: Limit {
                            output_tokens: Some(bound.output_tokens),
                            // A request's own bound; for a turn, the active
                            // time its atom has left.
                            time: Some(Duration::from_millis(u64::try_from(millis).unwrap_or(0))),
                        },
                        bounded_by,
                    })
                }
                Admission::Refused { reason, .. } => Err(Exhausted(reason).into()),
            }
        })
    }

    fn progress<'a>(&'a self, ticket: &'a Ticket, outcome: Outcome) -> MeterFuture<'a, ()> {
        Box::pin(async move {
            let Outcome::Partial { reported, last } = outcome else {
                return Ok(());
            };
            let how = self.partial(&ticket.model, reported, last);
            self.ledger
                .progress(&ticket.request, ticket.attempt, how)
                .await
                .map_err(store)?;
            Ok(())
        })
    }

    fn settle(&self, ticket: Ticket, outcome: Outcome) -> MeterFuture<'_, ()> {
        Box::pin(async move {
            ended(&ticket.request, ticket.attempt);
            let how = match outcome {
                Outcome::Reported(usage) => {
                    let tokens = |v: i64| u64::try_from(v).unwrap_or(0);
                    let cost = self.settings.cost(
                        &ticket.model,
                        tokens(usage.input_tokens),
                        tokens(usage.output_tokens),
                    );
                    Settlement::Reported {
                        input_tokens: usage.input_tokens,
                        output_tokens: usage.output_tokens,
                        millis: i64::try_from(ticket.admitted.elapsed().as_millis())
                            .unwrap_or(i64::MAX),
                        micro_usd: int(cost),
                    }
                }
                Outcome::Unknown => Settlement::Unknown,
                Outcome::Partial { reported, last } => self.partial(&ticket.model, reported, last),
                Outcome::NotDispatched => Settlement::NotDispatched,
            };
            self.ledger
                .settle(&ticket.request, ticket.attempt, how)
                .await
                .map_err(store)?;
            // Mid-atom: the call is made and settled, and the atom's turn
            // goes on.
            if atomic_durable::failpoints::armed() {
                atomic_durable::failpoint(&format!("atom:after-call:{}", ticket.request));
            }
            Ok(())
        })
    }
}

/// The calls this process has admitted and not yet settled, by request and
/// attempt: the ledger each is journaled in, and its heartbeat.
type InFlight = HashMap<(String, i64), (Ledger, tokio::task::AbortHandle)>;

static IN_FLIGHT: LazyLock<Mutex<InFlight>> = LazyLock::new(Mutex::default);

/// Start the heartbeat of attempt `attempt` at `request`, which reserved
/// `millis`: every [`heartbeat_every`] of it until it settles, and never
/// past its reservation.
fn beat(ledger: &Ledger, request: &str, attempt: i64, millis: i64) {
    let every = Duration::from_millis(u64::try_from(heartbeat_every(millis)).unwrap_or(5_000));
    let until = Instant::now() + Duration::from_millis(u64::try_from(millis).unwrap_or(0)) + every;
    let (beating, id) = (ledger.clone(), request.to_owned());
    let task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            if Instant::now() > until {
                return;
            }
            // A missed heartbeat only charges more time, never less.
            let _ = beating.heartbeat(&id, attempt).await;
        }
    });
    if let Ok(mut calls) = IN_FLIGHT.lock() {
        calls.insert(
            (request.to_owned(), attempt),
            (ledger.clone(), task.abort_handle()),
        );
    }
}

/// The call of attempt `attempt` at `request` ended: stop its heartbeat.
fn ended(request: &str, attempt: i64) {
    let call = IN_FLIGHT
        .lock()
        .ok()
        .and_then(|mut calls| calls.remove(&(request.to_owned(), attempt)));
    if let Some((_, heartbeat)) = call {
        heartbeat.abort();
    }
}

/// Settle every call this process has in flight as interrupted, for a
/// process that is stopping: each is charged the time it ran, and its tokens
/// and dollars stay at their bound, or at what a turn recorded it had
/// reported so far and one request in flight, if that is more. Returns how
/// many there were.
pub async fn interrupt_in_flight() -> usize {
    let calls: Vec<_> = IN_FLIGHT
        .lock()
        .map(|mut calls| calls.drain().collect())
        .unwrap_or_default();
    let n = calls.len();
    for ((request, attempt), (ledger, heartbeat)) in calls {
        heartbeat.abort();
        let _ = ledger.interrupt(&request, attempt).await;
    }
    n
}
