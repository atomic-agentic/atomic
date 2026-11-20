// SPDX-License-Identifier: Apache-2.0

//! `atomic parks`: list the runs parked on a budget cap, and release one,
//! raising its cap, as the local operator with their credential.
//!
//! The release goes through `atomic_durable::release::release_park`, which
//! checks the credential against the one the operator enrolled, raises the
//! cap, marks the park released and journals who did it, in one transaction.

use anyhow::Context;
use atomic_core::credential;
use atomic_core::state::ProjectState;
use atomic_durable::Durare;
use atomic_durable::gate::{HumanCredential, Principal};
use atomic_durable::release::{self, Raise, ReleaseCommand};
use atomic_durable::usage::Caps;
use clap::{Args, Subcommand};

#[derive(Debug, Subcommand)]
pub enum ParksCommand {
    /// List the parks not yet released.
    List,
    /// Release a park, raising a cap first: the parked turn runs again.
    Release(ReleaseArgs),
}

#[derive(Debug, Args)]
pub struct ReleaseArgs {
    /// The park's id, as `atomic parks list` shows it.
    park: String,
    /// The allowance to raise; by default the one that refused the call.
    #[arg(long)]
    scope: Option<String>,
    /// Raise the token cap to at least this.
    #[arg(long)]
    tokens: Option<i64>,
    /// Raise the active-time cap to at least this many minutes.
    #[arg(long)]
    minutes: Option<f64>,
    /// Raise the dollar cap to at least this.
    #[arg(long)]
    cost_usd: Option<f64>,
    /// A comment to record with the release.
    #[arg(long)]
    comment: Option<String>,
    /// A command id, to send the same release again safely.
    #[arg(long)]
    command_id: Option<String>,
}

/// The local operator's enrolled credential for this project.
fn operator_credential(state: &ProjectState) -> anyhow::Result<HumanCredential> {
    let user = credential::local_user().context("no user name in $USER or $LOGNAME")?;
    let principal = Principal::local(&user);
    let store = credential::configured_store(state.root())?;
    let account = credential::operator_account(state, &user);
    let secret = credential::operator_secret(store.as_ref(), &account)?.with_context(|| {
        format!("{principal} has no operator credential here; run atomic gates enroll")
    })?;
    Ok(HumanCredential::new(principal, secret))
}

pub async fn run(command: ParksCommand, state: &ProjectState) -> anyhow::Result<()> {
    let database = crate::engine::database(state);
    let rt = Durare::open(&database, None, atomic_app::pipeline::LAYOUT_VERSION)
        .await
        .with_context(|| format!("open the store at {}", crate::engine::describe(&database)))?;
    match command {
        ParksCommand::List => {
            for park in release::waiting(&rt).await? {
                println!("{}  run {}  {}", park.id, park.run, park.reason);
            }
            Ok(())
        }
        ParksCommand::Release(args) => {
            let credential = operator_credential(state)?;
            let park = release::waiting(&rt)
                .await?
                .into_iter()
                .find(|p| p.id == args.park);
            let raise =
                if args.tokens.is_some() || args.minutes.is_some() || args.cost_usd.is_some() {
                    let scope = match (&args.scope, park.as_ref().and_then(|p| p.scope())) {
                        (Some(scope), _) => scope.clone(),
                        (None, Some(scope)) => scope.to_owned(),
                        (None, None) => anyhow::bail!(
                            "park {} names no allowance; say which to raise with --scope",
                            args.park
                        ),
                    };
                    Some(Raise {
                        scope,
                        caps: Caps {
                            tokens: args.tokens.unwrap_or(0),
                            millis: args.minutes.map_or(0, |m| (m * 60_000.0) as i64),
                            micro_usd: args.cost_usd.map(|d| (d * 1_000_000.0) as i64),
                        },
                    })
                } else {
                    None
                };
            let command_id = match args.command_id {
                Some(id) => id,
                None => format!(
                    "cli-{}",
                    credential::fresh_secret()?.get(..32).unwrap_or_default()
                ),
            };
            let released = release::release_park(
                &rt,
                &credential,
                ReleaseCommand {
                    command_id,
                    park: args.park,
                    raise,
                    comment: args.comment,
                },
            )
            .await?;
            match &released.raised {
                Some(a) => println!(
                    "{} released by {}; {} now allows {} tokens",
                    released.park.id, released.by, a.scope, a.caps.tokens
                ),
                None => println!("{} released by {}", released.park.id, released.by),
            }
            Ok(())
        }
    }
}
