// SPDX-License-Identifier: Apache-2.0

//! `atomic gates`: enroll the local operator's credential, list gates and
//! decide them, from any terminal on the machine.
//!
//! Every decision carries the operator's credential from the configured
//! store (see [`atomic_core::credential`]); the gate store checks it.
//! Enrollment is a person's explicit command, run from a terminal.

use anyhow::{Context, bail};
use atomic_core::credential;
use atomic_core::state::ProjectState;
use atomic_durable::Durare;
use atomic_durable::gate::{
    self, DecisionCommand, GateFilter, HumanCredential, Principal, Verdict,
};
use clap::{Args, Subcommand};
use std::io::IsTerminal;

#[derive(Debug, Subcommand)]
pub enum GatesCommand {
    /// Enroll your operator credential for this project, from a terminal.
    Enroll {
        /// Replace the credential the gate store knows for you, after
        /// losing it or to rotate it.
        #[arg(long)]
        replace: bool,
    },
    /// List the project's gates, open ones only unless --all.
    List {
        /// Resolved gates too.
        #[arg(long)]
        all: bool,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Approve a gate.
    Approve(DecideArgs),
    /// Reject a gate.
    Reject(DecideArgs),
}

#[derive(Debug, Args)]
pub struct DecideArgs {
    /// The gate's id, as `atomic gates list` shows it.
    gate: String,
    /// A comment to record with the decision.
    #[arg(long)]
    comment: Option<String>,
    /// The proposal digest you looked at; defaults to the gate's.
    #[arg(long)]
    proposal: Option<String>,
    /// A command id, to send the same decision again safely; a new one by
    /// default.
    #[arg(long)]
    command_id: Option<String>,
}

/// The local operator: the account running this command.
fn operator() -> anyhow::Result<(String, Principal)> {
    let user = credential::local_user().context("no user name in $USER or $LOGNAME")?;
    let principal = Principal::local(&user);
    Ok((user, principal))
}

pub async fn run(command: GatesCommand, state: &ProjectState) -> anyhow::Result<()> {
    // The store the project's runs use: its SQLite database, or PostgreSQL
    // when ATOMIC_DATABASE_URL names one.
    let database = crate::engine::database(state)?;
    let rt = Durare::open(&database, None, atomic_app::pipeline::LAYOUT_VERSION)
        .await
        .with_context(|| {
            format!(
                "open the gate store at {}",
                crate::engine::describe(&database)
            )
        })?;
    match command {
        GatesCommand::Enroll { replace } => enroll(&rt, state, replace).await,
        GatesCommand::List { all, json } => list(&rt, all, json).await,
        GatesCommand::Approve(args) => decide(&rt, state, args, Verdict::Approve).await,
        GatesCommand::Reject(args) => decide(&rt, state, args, Verdict::Reject).await,
    }
}

async fn enroll(rt: &Durare, state: &ProjectState, replace: bool) -> anyhow::Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!("atomic gates enroll is a person's command: run it from a terminal");
    }
    let (user, principal) = operator()?;
    let store = credential::configured_store(state.root())?;
    let account = credential::operator_account(state, &user);
    let outcome = if replace {
        // The gate store first: if keeping the secret then fails, the old
        // one no longer decides anything, and running this again recovers.
        let secret = credential::fresh_secret()?;
        let outcome =
            gate::replace_enrollment(rt, &HumanCredential::new(principal.clone(), secret.clone()))
                .await?;
        store.save(&account, &secret)?;
        outcome
    } else {
        let (secret, _) = credential::load_or_create(store.as_ref(), &account)?;
        match gate::enroll(rt, &HumanCredential::new(principal.clone(), secret)).await {
            Ok(outcome) => outcome,
            Err(gate::EnrollError::Different(_)) => bail!(
                "the gate store knows another credential for {principal}; if you lost yours, \
                 or want a new one, run atomic gates enroll --replace"
            ),
            Err(e) => return Err(e.into()),
        }
    };
    let said = match outcome {
        gate::Enrollment::Enrolled => "enrolled",
        gate::Enrollment::AlreadyEnrolled => "already enrolled",
        gate::Enrollment::Replaced => "enrolled a new credential for",
    };
    println!("{said} {principal}");
    Ok(())
}

async fn list(rt: &Durare, all: bool, json: bool) -> anyhow::Result<()> {
    let gates = gate::gates(
        rt,
        GateFilter {
            open_only: !all,
            run: None,
        },
    )
    .await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&gates)?);
        return Ok(());
    }
    for g in &gates {
        let state = match &g.resolution {
            None => "open".to_owned(),
            Some(gate::Resolution::Expired { .. }) => "expired".to_owned(),
            Some(gate::Resolution::Decided(d)) => format!("{:?} by {}", d.verdict, d.by),
        };
        println!("{}  {}  [{}]", g.id, g.title, state.to_lowercase());
        println!("    shows: {}", serde_json::to_string(&g.show)?);
    }
    Ok(())
}

async fn decide(
    rt: &Durare,
    state: &ProjectState,
    args: DecideArgs,
    verdict: Verdict,
) -> anyhow::Result<()> {
    let (user, principal) = operator()?;
    let store = credential::configured_store(state.root())?;
    let account = credential::operator_account(state, &user);
    let secret = credential::operator_secret(store.as_ref(), &account)?.with_context(|| {
        format!("{principal} has no operator credential here; run atomic gates enroll")
    })?;
    let found = gate::find(rt, &args.gate)
        .await?
        .with_context(|| format!("no gate {}", args.gate))?;
    let command_id = match args.command_id {
        Some(id) => id,
        None => format!(
            "cli-{}",
            credential::fresh_secret()?.get(..32).unwrap_or_default()
        ),
    };
    let decision = gate::decide(
        rt,
        &HumanCredential::new(principal, secret),
        DecisionCommand {
            command_id,
            gate: found.id.clone(),
            verdict,
            comment: args.comment,
            proposal: args.proposal.unwrap_or(found.proposal),
        },
    )
    .await?;
    println!(
        "{} {:?} by {} ({})",
        found.id, decision.verdict, decision.by, decision.command
    );
    // The decision is recorded; waking the run is not part of it. Say so if
    // it fails, rather than leave the run to its deadline unannounced.
    if let Err(e) = gate::redeliver(rt).await {
        eprintln!(
            "warning: the decision is recorded, but waking run {} failed: {e}; atomic serve or \
             atomic runs resume {} delivers it",
            found.run, found.run
        );
    }
    Ok(())
}
