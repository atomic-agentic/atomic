// SPDX-License-Identifier: Apache-2.0

//! `atomic gates`: enroll the local operator's credential, list gates and
//! decide them, from any terminal on the machine.
//!
//! Every decision carries the operator's credential from the configured
//! store (see [`atomic_core::credential`]); the gate store checks it.
//! Enrollment is a person's explicit command, run from a terminal.

use crate::shown;
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
    Approve {
        #[command(flatten)]
        gate: GateArg,
        #[command(flatten)]
        args: DecideArgs,
    },
    /// Reject a gate.
    Reject {
        #[command(flatten)]
        gate: GateArg,
        #[command(flatten)]
        args: DecideArgs,
    },
    /// Answer a question gate.
    Answer {
        /// The gate's id, as `atomic gates list` shows it.
        gate: String,
        /// The answer: JSON, or else the text itself.
        answer: String,
        #[command(flatten)]
        args: DecideArgs,
    },
}

#[derive(Debug, Args)]
pub struct GateArg {
    /// The gate's id, as `atomic gates list` shows it.
    gate: String,
}

#[derive(Debug, Args)]
pub struct DecideArgs {
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
    let rt = Durare::open(
        &database,
        None,
        atomic_app::pipeline::LAYOUT_VERSION,
        &crate::engine::store_project(state)?,
    )
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
        GatesCommand::Approve { gate, args } => {
            decide(&rt, state, &gate.gate, args, Verdict::Approve, None).await
        }
        GatesCommand::Reject { gate, args } => {
            decide(&rt, state, &gate.gate, args, Verdict::Reject, None).await
        }
        GatesCommand::Answer { gate, answer, args } => {
            decide(&rt, state, &gate, args, Verdict::Approve, Some(answer)).await
        }
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
    println!("{said} {}", shown::line(principal.as_str()));
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
        // For a program, but often read on a terminal: what a model wrote
        // in a gate is escaped there too, and reads as the same JSON.
        println!("{}", shown::json(&serde_json::to_string_pretty(&gates)?));
        return Ok(());
    }
    // Titles, what gates show and their choices can come from a model: all
    // of it is printed through the sanitizer.
    for g in &gates {
        let state = match &g.resolution {
            None => "open".to_owned(),
            Some(gate::Resolution::Expired { .. }) => "expired".to_owned(),
            Some(gate::Resolution::Defaulted { answer, .. }) => {
                format!("silence selected {}", shown::value(answer))
            }
            Some(gate::Resolution::Decided(d)) => {
                let by = shown::line(d.by.as_str());
                match &d.answer {
                    Some(a) => format!("answered {} by {by}", shown::value(a)),
                    None => format!("{} by {by}", format!("{:?}", d.verdict).to_lowercase()),
                }
            }
        };
        println!(
            "{}  {}  [{state}]",
            shown::line(&g.id),
            shown::line(&g.title)
        );
        if !g.show.is_null() {
            println!("    shows: {}", shown::value(&g.show));
        }
        if let Some(q) = &g.question {
            match q.choices() {
                Some(choices) => {
                    let listed: Vec<String> = choices
                        .iter()
                        .enumerate()
                        .map(|(i, c)| format!("{}) {}", shown::label(i), shown::value(c)))
                        .collect();
                    println!("    choices: {}", listed.join("  "));
                }
                None => println!("    answer: {}", shown::value(&q.answer)),
            }
            if let (Some(default), Some(_)) = (&q.default, g.deadline) {
                println!("    silence selects: {}", shown::value(default));
            }
        }
    }
    Ok(())
}

/// The local operator's credential for this project, from the configured
/// store.
pub(crate) fn operator_credential(state: &ProjectState) -> anyhow::Result<HumanCredential> {
    let (user, principal) = operator()?;
    let store = credential::configured_store(state.root())?;
    let account = credential::operator_account(state, &user);
    let secret = credential::operator_secret(store.as_ref(), &account)?.with_context(|| {
        format!("{principal} has no operator credential here; run atomic gates enroll")
    })?;
    Ok(HumanCredential::new(principal, secret))
}

/// Decide `found` as the local operator, with their credential, and wake
/// its run.
#[allow(clippy::too_many_arguments)] // each is one part of the decision
pub(crate) async fn decide_as_operator(
    rt: &Durare,
    state: &ProjectState,
    found: &gate::Gate,
    verdict: Verdict,
    comment: Option<String>,
    answer: Option<serde_json::Value>,
    proposal: Option<String>,
    command_id: Option<String>,
) -> anyhow::Result<gate::Decision> {
    let credential = operator_credential(state)?;
    let command_id = match command_id {
        Some(id) => id,
        None => format!(
            "cli-{}",
            credential::fresh_secret()?.get(..32).unwrap_or_default()
        ),
    };
    let decision = gate::decide(
        rt,
        &credential,
        DecisionCommand {
            command_id,
            gate: found.id.clone(),
            verdict,
            comment,
            proposal: proposal.unwrap_or_else(|| found.proposal.clone()),
            answer,
        },
    )
    .await?;
    // The decision is recorded; waking the run is not part of it. Say so if
    // it fails, rather than leave the run to its deadline unannounced.
    if let Err(e) = gate::redeliver(rt).await {
        atomic_core::redact::Line::new()
            .plain("warning: the decision is recorded, but waking run ")
            .shown(&found.run)
            .plain(" failed: ")
            .redacted(&e.to_string())
            .plain("; atomic serve or atomic runs resume ")
            .shown(&found.run)
            .plain(" delivers it")
            .log();
    }
    // A park released while its run was down is delivered here too.
    if let Err(e) = atomic_durable::release::redeliver(rt).await {
        atomic_core::log_line!("warning: waking runs whose parks were released failed: {e}");
    }
    Ok(decision)
}

async fn decide(
    rt: &Durare,
    state: &ProjectState,
    gate_id: &str,
    args: DecideArgs,
    verdict: Verdict,
    answer: Option<String>,
) -> anyhow::Result<()> {
    // Raw: an error is redacted, then escaped, where it is reported.
    let missing = format!("no gate {gate_id}");
    let found = gate::find(rt, gate_id).await?.context(missing)?;
    // Read as the prompt reads it.
    let answer = answer.map(|text| match &found.question {
        Some(q) => shown::answer(q, &text),
        None => serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)),
    });
    let decision = decide_as_operator(
        rt,
        state,
        &found,
        verdict,
        args.comment,
        answer,
        args.proposal,
        args.command_id,
    )
    .await?;
    let what = match &decision.answer {
        Some(a) => format!("answered {}", shown::value(a)),
        None => format!("{:?}", decision.verdict).to_lowercase(),
    };
    println!(
        "{} {what} by {} ({})",
        shown::line(&found.id),
        shown::line(decision.by.as_str()),
        shown::line(&decision.command)
    );
    Ok(())
}
