// SPDX-License-Identifier: Apache-2.0

//! The `atomic` command-line program.

use anyhow::Context;
use atomic_core::codex::{self, CodexPaths, ModelEndpoint};
use atomic_core::state::ProjectState;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "atomic",
    version,
    about = "Small, typed, durable agents on Codex."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Developer diagnostics.
    #[command(subcommand, hide = true)]
    Debug(Debug),
}

#[derive(Debug, Subcommand)]
enum Debug {
    /// Send one prompt through a Codex thread and print the model's answer.
    CodexPrompt(PromptArgs),
    /// Open the durable store and run one Codex turn in this one process.
    Probe(PromptArgs),
    /// Print the project's state directory.
    StateDir,
}

#[derive(Debug, Args)]
struct PromptArgs {
    /// Base URL of an OpenAI-compatible Responses API.
    #[arg(long, env = "ATOMIC_MODEL_BASE_URL")]
    base_url: String,
    /// Model id at that endpoint.
    #[arg(long, env = "ATOMIC_MODEL")]
    model: String,
    /// Environment variable that holds the endpoint's API key, if it needs one.
    #[arg(long)]
    api_key_env: Option<String>,
    /// The prompt.
    #[arg(required = true, trailing_var_arg = true)]
    prompt: Vec<String>,
}

impl PromptArgs {
    fn endpoint(&self) -> ModelEndpoint {
        ModelEndpoint {
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            api_key_env: self.api_key_env.clone(),
        }
    }
}

fn main() -> anyhow::Result<()> {
    // Resolve and create the state directory before Codex starts, so its home
    // is never the user's own. A failure here is reported once the normal entry
    // point runs; Codex's helper invocations never need it.
    let state = std::env::current_dir()
        .context("read the working directory")
        .and_then(|cwd| ProjectState::resolve(&cwd).map_err(anyhow::Error::from))
        .and_then(|s| s.create().map(|()| s).map_err(anyhow::Error::from));
    let codex_home = match &state {
        Ok(s) => s.codex_home(),
        // Never created: Codex finds no home and creates nothing.
        Err(_) => std::env::temp_dir().join("atomic-no-state-directory"),
    };
    codex::dispatch(&codex_home, move |paths| async move {
        let state = state?;
        run(Cli::parse(), paths, state).await
    })
}

async fn run(cli: Cli, paths: CodexPaths, state: ProjectState) -> anyhow::Result<()> {
    match cli.command {
        None => {
            println!("atomic {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(Command::Debug(Debug::StateDir)) => {
            println!("{}", state.dir().display());
            Ok(())
        }
        Some(Command::Debug(Debug::CodexPrompt(args))) => {
            let answer = prompt(&paths, &state, &args).await?;
            println!("{answer}");
            Ok(())
        }
        Some(Command::Debug(Debug::Probe(args))) => {
            let runs = atomic_durable::probe(&state.database()).await?;
            println!(
                "durable store {}: {runs} probe run(s)",
                state.database().display()
            );
            let answer = prompt(&paths, &state, &args).await?;
            println!("codex: {answer}");
            Ok(())
        }
    }
}

async fn prompt(
    paths: &CodexPaths,
    state: &ProjectState,
    args: &PromptArgs,
) -> anyhow::Result<String> {
    let cwd: PathBuf = state.root().to_owned();
    codex::one_turn(
        paths,
        &state.codex_home(),
        &cwd,
        &args.endpoint(),
        &args.prompt.join(" "),
    )
    .await
}
