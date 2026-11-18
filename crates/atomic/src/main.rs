// SPDX-License-Identifier: Apache-2.0

//! The `atomic` command-line program.

use anyhow::Context;
use atomic_core::codex::{self, CodexPaths, ModelEndpoint};
use atomic_core::project;
use atomic_core::state::ProjectState;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

mod engine;
mod failure;
mod runs;
mod setup;
mod skills;
mod target;

#[derive(Debug, Parser)]
#[command(
    name = "atomic",
    version,
    about = "Small, typed, durable agents on Codex."
)]
struct Cli {
    /// Print results and errors as JSON on stdout; an error's action is a
    /// field of its own.
    #[arg(long, global = true)]
    json: bool,
    /// The project: the directory holding its atomic.yaml (or
    /// .atomic/atomic.yaml), or the manifest file itself.
    #[arg(long, global = true, value_name = "PATH")]
    project: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Check Astro atoms and workflows, regenerating editor declarations.
    Check {
        /// An atom file or project directory; defaults to the current project.
        path: Option<PathBuf>,
    },
    /// Render an Astro prompt without calling a model or running scripts.
    Render(RenderArgs),
    /// Run an atom or a workflow in the foreground and print its output.
    Run(runs::RunArgs),
    /// Continue runs.
    #[command(subcommand)]
    Runs(Runs),
    /// Run the project's runtime in the foreground: recover unfinished runs
    /// and keep running until stopped.
    Serve(runs::ServeArgs),
    /// Run commands for a control plane: one request on stdin, one response
    /// on stdout, and an exit code from a fixed set.
    Runner,
    /// Install atomic's authoring skills for a coding agent.
    Setup(setup::SetupArgs),
    /// Developer diagnostics.
    #[command(subcommand, hide = true)]
    Debug(Debug),
}

#[derive(Debug, Subcommand)]
enum Runs {
    /// Continue an unfinished run in the foreground, from its checkpoints,
    /// with the definitions and input it started with.
    Resume(runs::ResumeArgs),
}

#[derive(Debug, Args)]
struct RenderArgs {
    /// The .atom.astro file.
    atom: PathBuf,
    /// JSON input file.
    #[arg(long)]
    input: PathBuf,
    /// Stable context checkpoint identity; reuse restores the original values.
    #[arg(long)]
    run_id: Option<String>,
    /// Current dispatch step.
    #[arg(long, default_value = "preview")]
    step_id: String,
    /// Public context settings JSON; needed only when creating a contextual run.
    #[arg(long)]
    context_settings: Option<PathBuf>,
    /// Explicit configuration profile.
    #[arg(long)]
    profile: Option<String>,
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
    // The runner role needs no state directory and no Codex: it runs one
    // command for a control plane and exits with a code from its contract.
    if std::env::args_os().nth(1).is_some_and(|a| a == "runner") {
        std::process::exit(runner());
    }
    let parsed = Cli::try_parse();
    // Help and the version create nothing on disk.
    let quiet = parsed.as_ref().err().is_some_and(|e| {
        use clap::error::ErrorKind;
        matches!(
            e.kind(),
            ErrorKind::DisplayHelp
                | ErrorKind::DisplayVersion
                | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        )
    });
    // Resolve and create the state directory before Codex starts, so its home
    // is never the user's own. A failure here is reported once the normal entry
    // point runs; Codex's helper invocations never need it.
    let state = if quiet {
        Err(anyhow::anyhow!("no state directory"))
    } else {
        state_root(parsed.as_ref().ok())
            .and_then(|root| ProjectState::resolve(&root).map_err(anyhow::Error::from))
            .and_then(|s| s.create().map(|()| s).map_err(anyhow::Error::from))
    };
    let codex_home = match &state {
        Ok(s) => s.codex_home(),
        // Never created: Codex finds no home and creates nothing.
        Err(_) => std::env::temp_dir().join("atomic-no-state-directory"),
    };
    codex::dispatch(&codex_home, move |paths| async move {
        let cli = match parsed {
            Ok(cli) => cli,
            Err(_) => Cli::parse(),
        };
        let json = cli.json;
        let outcome = match state {
            Ok(state) => run(cli, paths, state).await,
            Err(e) => Err(e),
        };
        if let Err(e) = outcome {
            failure::report(&failure::analyze(&e), json);
            std::process::exit(1);
        }
        Ok(())
    })
}

/// The project root a command's state belongs to: the project it works on,
/// or, outside any project, the working directory.
fn state_root(cli: Option<&Cli>) -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir().context("read the working directory")?;
    let flag = cli.and_then(|c| c.project.as_deref());
    let target = match cli.and_then(|c| c.command.as_ref()) {
        Some(Command::Run(args)) => Some(args.target.as_str()),
        _ => None,
    };
    match target::project_for(flag, target, &cwd) {
        Ok(p) => Ok(p.root().to_owned()),
        // Outside any project, state is keyed by the working directory.
        Err(e)
            if flag.is_none()
                && target.is_none()
                && matches!(
                    e.downcast_ref::<project::ProjectError>(),
                    Some(project::ProjectError::NotFound(_))
                ) =>
        {
            Ok(cwd)
        }
        Err(e) => Err(e),
    }
}

/// `atomic runner`: one request on stdin, one response on stdout.
fn runner() -> i32 {
    /// Run commands for a control plane.
    #[derive(Debug, Parser)]
    #[command(name = "atomic runner", version)]
    struct RunnerCli {}
    let args = std::env::args_os().skip(1);
    match RunnerCli::try_parse_from(args) {
        Ok(RunnerCli {}) => atomic_exec::runner::serve_stdio(),
        Err(e) if !e.use_stderr() => {
            let _ = e.print();
            atomic_exec::runner::code::CLEAN
        }
        Err(e) => {
            let _ = e.print();
            atomic_exec::runner::code::USAGE
        }
    }
}

async fn run(cli: Cli, paths: CodexPaths, state: ProjectState) -> anyhow::Result<()> {
    if let Some(Command::Runner) = cli.command {
        std::process::exit(atomic_exec::runner::serve_stdio());
    }
    // Before anything else: a store written by a newer atomic is refused.
    atomic_durable::store::open_sqlite(&state.database()).await?;
    let (json, project) = (cli.json, cli.project);
    let project = project.as_deref();
    match cli.command {
        Some(Command::Check { path }) => {
            tokio::task::spawn_blocking(move || check(path, json)).await?
        }
        Some(Command::Run(args)) => runs::run(args, project, &paths, json).await,
        Some(Command::Runs(Runs::Resume(args))) => runs::resume(args, project, &paths, json).await,
        Some(Command::Serve(args)) => runs::serve(args, project, &paths).await,
        Some(Command::Runner) => Ok(()),
        Some(Command::Render(args)) => tokio::task::spawn_blocking(move || render(args)).await?,
        Some(Command::Setup(args)) => setup::run(args),
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

fn write_declarations(atom: &atomic_core::atom::Atom) -> anyhow::Result<()> {
    for (name, source) in atom
        .declarations
        .as_object()
        .context("compiler declarations")?
    {
        let file = PathBuf::from(name);
        let parent = file.parent().context("declaration parent")?;
        anyhow::ensure!(
            file.starts_with(atom.program.root.join(".atomic/types")),
            "declaration outside generated directory"
        );
        let mut generated = atom.program.root.clone();
        for part in parent.strip_prefix(&atom.program.root)? {
            generated.push(part);
            match std::fs::symlink_metadata(&generated) {
                Ok(_) => anyhow::ensure!(
                    std::fs::canonicalize(&generated)?.starts_with(&atom.program.root),
                    "generated directory escapes project"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::create_dir(&generated)?
                }
                Err(error) => return Err(error.into()),
            }
        }
        anyhow::ensure!(
            std::fs::canonicalize(parent)?.starts_with(&atom.program.root),
            "generated directory symlink escapes project"
        );
        anyhow::ensure!(
            !std::fs::symlink_metadata(&file)
                .is_ok_and(|metadata| metadata.file_type().is_symlink()),
            "generated declaration is a symlink"
        );
        std::fs::write(file, source.as_str().context("declaration text")?)?;
    }
    Ok(())
}

fn load_project_schemas(
    path: &std::path::Path,
    registry: &mut atomic_schema::registry::Registry,
) -> anyhow::Result<()> {
    let path = if path.as_os_str().is_empty() {
        std::path::Path::new(".")
    } else {
        path
    };
    match project::discover(path) {
        Ok(project) => {
            let files = atomic_core::collections::collect(&project)?;
            for schema in &files["schemas"] {
                registry.load_file(&schema.path)?;
            }
            Ok(())
        }
        Err(project::ProjectError::NotFound(_)) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn check(path: Option<PathBuf>, json: bool) -> anyhow::Result<()> {
    use atomic_core::{atom, collections, compile, workflow};
    let path = path.unwrap_or(std::env::current_dir()?);
    let mut registry = atomic_schema::registry::Registry::new();
    if path.is_file() {
        load_project_schemas(
            path.parent().unwrap_or(std::path::Path::new(".")),
            &mut registry,
        )?;
        let atom = atom::load(&path, &mut registry)?;
        compile::compile(&atom, &registry)?;
        write_declarations(&atom)?;
        if json {
            println!(
                "{}",
                serde_json::json!({"checked": {"atoms": 1, "workflows": 0}, "warnings": []})
            );
        } else {
            println!("checked {}", atom.file.display());
        }
        return Ok(());
    }
    let project = project::discover(&path)?;
    let files = collections::collect(&project)?;
    for schema in &files["schemas"] {
        registry.load_file(&schema.path)?;
    }
    let mut atoms = std::collections::BTreeMap::new();
    for source in &files["atoms"] {
        let atom = atom::load(&source.path, &mut registry)?;
        compile::compile(&atom, &registry)?;
        write_declarations(&atom)?;
        let name = atom.name.clone();
        anyhow::ensure!(
            atoms.insert(name.clone(), atom).is_none(),
            "duplicate atom name {name}"
        );
    }
    let mut warnings = Vec::new();
    for source in &files["workflows"] {
        let loaded = workflow::load(&source.path, &atoms, &mut registry)?;
        for w in loaded.warnings {
            warnings.push(format!("{}: {w}", loaded.file.display()));
        }
    }
    if json {
        println!(
            "{}",
            serde_json::json!({
                "checked": {"atoms": atoms.len(), "workflows": files["workflows"].len()},
                "warnings": warnings,
            })
        );
    } else {
        for w in &warnings {
            eprintln!("warning: {w}");
        }
        println!(
            "checked {} Astro atoms and {} workflows",
            atoms.len(),
            files["workflows"].len()
        );
    }
    Ok(())
}

fn render(args: RenderArgs) -> anyhow::Result<()> {
    use atomic_core::{atom, compile, context, render};
    let mut registry = atomic_schema::registry::Registry::new();
    load_project_schemas(
        args.atom.parent().unwrap_or(std::path::Path::new(".")),
        &mut registry,
    )?;
    let atom = atom::load(&args.atom, &mut registry)?;
    let compiled = compile::compile(&atom, &registry)?;
    let state = ProjectState::resolve(&atom.program.root)?;
    state.create()?;
    let project_name = project::discover(&atom.program.root)
        .map(|project| project.name)
        .unwrap_or_else(|_| {
            atom.program
                .root
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
    let project_id = state
        .dir()
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let input: serde_json::Value = serde_json::from_slice(&std::fs::read(&args.input)?)?;
    registry
        .validator(&atom.input)?
        .validate(&input)
        .map_err(|e| anyhow::anyhow!("input: {e}"))?;
    let run_id = args.run_id.as_deref().unwrap_or("preview");
    let snapshot = if atom.context.is_some() {
        let resolve = || {
            let path = args
                .context_settings
                .as_ref()
                .ok_or("a new contextual run requires --context-settings")?;
            let settings = serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
            let metadata = serde_json::json!({"run":{"id":run_id},"step":{"id":args.step_id},"project":{"id":project_id,"name":project_name}});
            Ok((
                settings,
                metadata,
                args.profile.clone(),
                serde_json::json!({}),
            ))
        };
        Some(
            if args.run_id.is_some() {
                context::default_key_dir().and_then(|keys| {
                    context::checkpoint(&atom, &state.dir().join("context"), &keys, run_id, resolve)
                })
            } else {
                context::resolve_fresh(&atom, run_id, resolve)
            }
            .map_err(anyhow::Error::msg)?,
        )
    } else {
        None
    };
    let prompt = render::render_with_context(
        &compiled,
        &render::RunInput {
            run_id,
            input: &input,
            frontmatter: &atom.frontmatter,
            origin: "external",
        },
        snapshot.as_ref(),
        Some(&args.step_id),
    )?;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"developer":prompt.developer,"user":prompt.user})
        )?
    );
    Ok(())
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
