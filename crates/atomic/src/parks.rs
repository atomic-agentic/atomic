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

/// `value`, rounded to a whole number, for the flag `flag`: refused when it
/// is not a number, below zero or too large.
fn whole(value: f64, flag: &str) -> anyhow::Result<i64> {
    if !value.is_finite() || value < 0.0 || value > i64::MAX as f64 {
        anyhow::bail!("{flag} must be a number, zero or more");
    }
    Ok(value.round() as i64)
}

pub async fn run(command: ParksCommand, state: &ProjectState) -> anyhow::Result<()> {
    let database = crate::engine::database(state)?;
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
            let raising =
                args.tokens.is_some() || args.minutes.is_some() || args.cost_usd.is_some();
            if args.scope.is_some() && !raising {
                anyhow::bail!(
                    "--scope names an allowance to raise, and no cap was given: add --tokens, \
                     --minutes or --cost-usd"
                );
            }
            if args.tokens.is_some_and(|t| t < 0) {
                anyhow::bail!("--tokens must be a whole number, zero or more");
            }
            let caps = Caps {
                tokens: args.tokens.unwrap_or(0),
                millis: match args.minutes {
                    Some(m) => whole(m * 60_000.0, "--minutes")?,
                    None => 0,
                },
                micro_usd: match args.cost_usd {
                    Some(d) => Some(whole(d * 1_000_000.0, "--cost-usd")?),
                    None => None,
                },
            };
            let credential = operator_credential(state)?;
            // The park as recorded, released or not, so a repeat of the
            // command raises what the first one did.
            let Some(park) = release::park(&rt, &args.park).await? else {
                anyhow::bail!(
                    "no park {}; `atomic parks list` lists the runs parked and not yet released",
                    args.park
                );
            };
            let raise = if raising {
                let scope = match (&args.scope, park.scope()) {
                    (Some(scope), _) => scope.clone(),
                    (None, Some(scope)) => scope.to_owned(),
                    (None, None) => anyhow::bail!(
                        "park {} names no allowance; say which of its run's to raise with --scope",
                        args.park
                    ),
                };
                Some(Raise { scope, caps })
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
            let release = release::release_park(
                &rt,
                &credential,
                ReleaseCommand {
                    command_id: command_id.clone(),
                    park: args.park,
                    raise,
                    comment: args.comment,
                },
            )
            .await?;
            let released = &release.released;
            match &released.raised {
                Some(a) => println!(
                    "{} released by {} ({}); {} now allows {} tokens",
                    released.park.id, released.by, command_id, a.scope, a.caps.tokens
                ),
                None => println!(
                    "{} released by {} ({})",
                    released.park.id, released.by, command_id
                ),
            }
            if release.raised_nothing {
                atomic_core::log_line!(
                    "warning: the caps given are no higher than {}'s; it was not raised, and the \
                     run may park again",
                    released
                        .raised
                        .as_ref()
                        .map_or("the allowance", |a| a.scope.as_str())
                );
            }
            if let Some(why) = &release.wake_failed {
                atomic_core::log_line!(
                    "warning: the release is recorded, but waking run {} failed: {why}; atomic \
                     serve or atomic runs resume {} delivers it",
                    released.park.run,
                    released.park.run
                );
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::whole;

    #[test]
    fn a_cap_flag_is_a_number_rounded_to_whole_units() {
        assert_eq!(
            whole(1.5 * 60_000.0, "--minutes").expect("a whole number"),
            90_000
        );
        assert_eq!(
            whole(0.0000015 * 1_000_000.0, "--cost-usd").expect("a whole number"),
            2
        );
        assert_eq!(whole(0.4, "--cost-usd").expect("a whole number"), 0);
        for bad in [f64::NAN, f64::INFINITY, -1.0, 1e300] {
            assert!(whole(bad, "--minutes").is_err(), "{bad}");
        }
    }
}
