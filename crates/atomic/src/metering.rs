// SPDX-License-Identifier: Apache-2.0

//! Metering atoms' model calls against the durable usage ledger.
//!
//! A [`RunBudget`] holds a workflow run's resolved caps and the ledger. For
//! each atom step it makes a [`Metering`] that admits every model call
//! against the atom's own allowance and the run's, under a request id made
//! from the run, the step's stable position and the call's index, so a
//! re-run of the step after a crash makes new attempts at the same requests
//! instead of new requests. Each admission reserves the call's whole bound
//! from the price table and call bound in [`BudgetSettings`]; each
//! settlement records what the provider reported, priced the same way.

use atomic_core::budget::{Budget, BudgetError, BudgetSettings, Level, Resolved, resolve};
use atomic_core::usage::{
    Call, Exhausted, Meter, MeterError, MeterFuture, Metering, Outcome, Ticket,
};
use atomic_durable::StepInfo;
use atomic_durable::usage::{Admission, Caps, Charge, Ledger, Request, Scope, Settlement};
use std::sync::Arc;
use std::time::Instant;

/// A run's caps, for metering its atoms.
#[derive(Clone, Debug)]
pub struct RunBudget {
    ledger: Ledger,
    settings: Arc<BudgetSettings>,
    run: Resolved,
}

/// A count as the ledger holds it.
fn int(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
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
        })
    }

    /// The run's resolved caps.
    pub fn caps(&self) -> &Resolved {
        &self.run
    }

    /// The caps of an atom calling `model` with its own limits `declared`.
    pub fn atom(&self, declared: &Budget, model: &str) -> Result<Resolved, BudgetError> {
        resolve(declared, Level::Atom, &self.settings, &[model])
    }

    /// The meter for the atom step `step`, with caps `atom`.
    pub fn meter(&self, step: &StepInfo, atom: &Resolved) -> Metering {
        let run = step.workflow_id.as_str();
        Metering::new(Arc::new(LedgerMeter {
            ledger: self.ledger.clone(),
            settings: self.settings.clone(),
            run: run.to_owned(),
            step: step.step_id,
            chain: vec![
                Scope::run(run, caps(&self.run)),
                Scope::atom(run, step.step_id, caps(atom)),
            ],
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
}

fn store(e: impl std::fmt::Display) -> MeterError {
    MeterError::Store(e.to_string())
}

impl Meter for LedgerMeter {
    fn admit(&self, call: Call) -> MeterFuture<'_, Ticket> {
        Box::pin(async move {
            let bound = self.settings.reservation(&call.model);
            // The whole bound is reserved, so the reservation is also the
            // documented maximum: no excess is left unreserved.
            let whole = Charge {
                tokens: int(bound.tokens),
                millis: int(bound.millis),
                micro_usd: int(bound.micro_usd),
            };
            let request = Request {
                id: format!("{}:{}:{}", self.run, self.step, call.index),
                run: self.run.clone(),
                model: call.model.clone(),
                chain: self.chain.clone(),
                reserve: whole,
                maximum: whole,
            };
            match self.ledger.admit(request).await.map_err(store)? {
                Admission::Admitted { request, attempt } => {
                    // Admitted and reserved, and not yet dispatched.
                    if atomic_durable::failpoints::armed() {
                        atomic_durable::failpoint(&format!("atom:after-admit:{request}"));
                    }
                    Ok(Ticket {
                        request,
                        attempt,
                        model: call.model,
                        admitted: Instant::now(),
                    })
                }
                Admission::Refused { reason, .. } => Err(Exhausted(reason).into()),
            }
        })
    }

    fn settle(&self, ticket: Ticket, outcome: Outcome) -> MeterFuture<'_, ()> {
        Box::pin(async move {
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
