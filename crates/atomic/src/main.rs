// SPDX-License-Identifier: Apache-2.0

//! The `atomic` command-line program.

// The workflows' futures, spans around them included, nest deeply inside
// Codex's; proving them Send needs more than the default query depth.
#![recursion_limit = "512"]

use anyhow::Context;
use atomic_core::codex::{self, CodexPaths, ModelEndpoint};
use atomic_core::project;
use atomic_core::state::ProjectState;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

mod engine;
mod evals;
mod failure;
mod gates;
mod init;
mod prompt;
mod runs;
mod settings;
mod setup;
mod show;
mod shown;
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
    /// Show every setting the project resolves to here; with --explain,
    /// where each came from and why, secrets masked.
    Config(settings::ConfigArgs),
    /// Make a new project here: a manifest, a sample atom and schemas, a
    /// workflow and an eval case. In a directory that holds code, under
    /// `.atomic/`.
    Init(init::InitArgs),
    /// Run an atom or a workflow in the foreground and print its output.
    Run(runs::RunArgs),
    /// Run atoms' eval cases and fail when an atom's pass rate is below its
    /// threshold; model calls are answered from cassettes unless --live.
    Eval(evals::EvalArgs),
    /// Continue runs.
    #[command(subcommand)]
    Runs(Runs),
    /// Run the project's runtime in the foreground: recover unfinished runs
    /// and keep running until stopped.
    Serve(runs::ServeArgs),
    /// Run commands for a control plane: one request on stdin, one response
    /// on stdout, and an exit code from a fixed set.
    Runner,
    /// Write JSON Schemas for editors: workflow files and the manifest, with
    /// the project's atom names and schema IDs.
    #[command(args_conflicts_with_subcommands = true)]
    Schemas {
        /// A project directory; defaults to the current project.
        path: Option<PathBuf>,
        #[command(subcommand)]
        command: Option<SchemasCommand>,
    },
    /// Enroll your operator credential, list gates and decide them.
    #[command(subcommand)]
    Gates(gates::GatesCommand),
    /// Model access: sign in with ChatGPT so Codex atoms run on your plan.
    #[command(subcommand)]
    Model(ModelCommand),
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
    /// Show everything the store recorded about a run: its steps, which
    /// attempt ran each and which came from checkpoints, its input and
    /// output, its spend, its gates and its transcripts.
    Show(show::ShowArgs),
}

#[derive(Debug, Subcommand)]
enum SchemasCommand {
    /// Print the JSON Schemas an atom's input and output compile to, as
    /// atomic enforces them, compact forms expanded.
    Expand {
        /// The atom, by name in the current project or by file.
        atom: String,
    },
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
    /// Panic with this message, to see what a panic writes.
    Panic {
        /// The message.
        message: String,
    },
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
            sign_in: None,
        }
    }
}

fn main() -> std::process::ExitCode {
    // A panic's message can quote what it was handling: it is redacted like
    // every other line atomic writes.
    atomic_core::redact::install_panic_hook();
    atomic_exec::set_log(|line| atomic_core::redact::log(line));
    match real_main() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        // Reported here, redacted, rather than by the default `{:?}` of a
        // returned error.
        Err(e) => {
            atomic_core::redact::log(error_line(&e));
            std::process::ExitCode::FAILURE
        }
    }
}

/// Send the last spans, unless a signal says to stop at once.
async fn flush_spans() {
    if atomic_app::telemetry::flush_unless_signalled().await {
        atomic_core::log_line!("stopping now, without sending the last spans");
        std::process::exit(130);
    }
}

/// The line `main` reports an error that reached it with, redacted: an
/// error's chain, never its `Debug` form.
fn error_line(e: &anyhow::Error) -> String {
    atomic_core::redact::bounded(&format!("error: {e:#}"), atomic_core::redact::MAX_TEXT)
}

fn real_main() -> anyhow::Result<()> {
    // A Codex helper, such as the Linux sandbox inside a turn, runs before
    // anything else: no state directory is resolved or created, since its
    // HOME is the turn's own scratch directory.
    if codex::is_helper(std::env::args_os()) {
        return codex::run_helper();
    }
    // A helper child serves one call and exits, before anything else starts:
    // no state directory, no Codex dispatch.
    if std::env::args_os().nth(1).as_deref()
        == Some(atomic_core::helper_runtime::CHILD_ARG.as_ref())
    {
        std::process::exit(atomic_core::helper_runtime::serve());
    }
    // The runner role needs no state directory and no Codex: it runs one
    // command for a control plane and exits with a code from its contract.
    if std::env::args_os().nth(1).is_some_and(|a| a == "runner") {
        std::process::exit(runner());
    }
    let parsed = Cli::try_parse();
    // Help and the version create nothing on disk, and print only themselves:
    // they need no Codex either (Codex's helpers were dispatched above).
    let quiet = parsed.as_ref().err().is_some_and(|e| {
        use clap::error::ErrorKind;
        matches!(
            e.kind(),
            ErrorKind::DisplayHelp
                | ErrorKind::DisplayVersion
                | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        )
    });
    if quiet && let Err(e) = &parsed {
        e.exit();
    }
    // The runner role after global flags, such as `atomic --json runner`.
    if let Ok(Cli {
        command: Some(Command::Runner),
        ..
    }) = &parsed
    {
        std::process::exit(atomic_exec::runner::serve_stdio(
            &mut atomic_core::redact::stderr_lines(),
        ));
    }
    // A command line clap refuses is said now, before anything is touched.
    let cli = match parsed {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };
    // `atomic model` keeps nothing in a project's state and starts no Codex
    // thread, so it creates no state and has no Codex home.
    if let Some(Command::Model(_)) = &cli.command {
        let json = cli.json;
        let project = found_project(&cli);
        let Some(Command::Model(command)) = cli.command else {
            unreachable!("matched above")
        };
        return codex::without_threads(async move {
            let outcome = match project {
                Ok(project) => model(command, project.as_deref(), json).await,
                Err(e) => Err(e),
            };
            if let Err(e) = outcome {
                failure::report(&failure::analyze(&e), json);
                std::process::exit(1);
            }
            Ok(())
        });
    }
    // Resolve and create the state directory before Codex starts, so its home
    // is never the user's own. Without one, nothing starts: there is no Codex
    // home to give it.
    let state = state_root(Some(&cli))
        .and_then(|root| ProjectState::resolve(&root).map_err(anyhow::Error::from))
        .and_then(|s| s.create().map(|()| s).map_err(anyhow::Error::from));
    let state = match state {
        Ok(state) => state,
        Err(e) => {
            failure::report(&failure::analyze(&e), cli.json);
            std::process::exit(1);
        }
    };
    codex::dispatch(&state.codex_home(), move |paths| async move {
        // Only this role, the command itself, exports spans; Codex's helper
        // roles never reach here. Spans go where the standard
        // OTEL_EXPORTER_OTLP_* variables say, if they say, from this runtime;
        // a bad setting is reported and the command runs without them.
        if let Err(e) = atomic_app::telemetry::init_from_env() {
            atomic_core::log_line!("warning: telemetry is off: {e:#}");
        }
        let json = cli.json;
        if let Err(e) = run(cli, paths, state).await {
            failure::report(&failure::analyze(&e), json);
            flush_spans().await;
            std::process::exit(1);
        }
        flush_spans().await;
        Ok(())
    })
}

/// The project a command names or is run in, if there is one: `None` outside
/// any project.
fn found_project(cli: &Cli) -> anyhow::Result<Option<PathBuf>> {
    let cwd = std::env::current_dir().context("read the working directory")?;
    match target::project_for(cli.project.as_deref(), None, &cwd) {
        Ok(p) => Ok(Some(p.root().to_owned())),
        Err(e)
            if cli.project.is_none()
                && matches!(
                    e.downcast_ref::<project::ProjectError>(),
                    Some(project::ProjectError::NotFound(_))
                ) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// The project root a command's state belongs to: the project it works on,
/// or, outside any project, the working directory.
fn state_root(cli: Option<&Cli>) -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir().context("read the working directory")?;
    let flag = cli.and_then(|c| c.project.as_deref());
    let target = match cli.and_then(|c| c.command.as_ref()) {
        Some(Command::Run(args)) => Some(args.target.as_str()),
        Some(Command::Eval(args)) => args.atoms.first().map(String::as_str),
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
        Ok(RunnerCli {}) => {
            atomic_exec::runner::serve_stdio(&mut atomic_core::redact::stderr_lines())
        }
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
    // Before anything else: a store written by a newer atomic is refused.
    atomic_durable::store::open_sqlite(&state.database()).await?;
    // Turn directories a killed atomic left behind.
    codex::sweep_turn_temps(state.dir())?;
    let (json, project) = (cli.json, cli.project);
    let project = project.as_deref();
    match cli.command {
        Some(Command::Gates(command)) => gates::run(command, &state).await,
        Some(Command::Model(_)) => unreachable!("`atomic model` runs without a state directory"),
        Some(Command::Check { path }) => {
            tokio::task::spawn_blocking(move || check(path, json)).await?
        }
        Some(Command::Run(args)) => runs::run(args, project, &paths, json).await,
        Some(Command::Eval(args)) => evals::run(args, project, &paths, json).await,
        Some(Command::Runs(Runs::Resume(args))) => runs::resume(args, project, &paths, json).await,
        Some(Command::Runs(Runs::Show(args))) => show::show(args, project, json).await,
        Some(Command::Serve(args)) => runs::serve(args, project, &paths).await,
        Some(Command::Runner) => Ok(()),
        Some(Command::Render(args)) => tokio::task::spawn_blocking(move || render(args)).await?,
        Some(Command::Config(args)) => settings::config(&args, project, json),
        Some(Command::Init(args)) => init::init(args),
        Some(Command::Schemas { path, command }) => match command {
            Some(SchemasCommand::Expand { atom }) => {
                tokio::task::spawn_blocking(move || expand(&atom)).await?
            }
            None => tokio::task::spawn_blocking(move || schemas(path)).await?,
        },
        Some(Command::Setup(args)) => setup::run(args),
        None => {
            println!("atomic {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(Command::Debug(Debug::Panic { message })) => panic!("{message}"),
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
            atomic_core::collections::load_schemas(&project, registry).map_err(anyhow::Error::msg)
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
                serde_json::json!({
                    "checked": {"atoms": 1, "workflows": 0, "toolset_operations": 0},
                    "warnings": []
                })
            );
        } else {
            println!("checked {}", atom.file.display());
        }
        return Ok(());
    }
    let project = project::discover(&path)?;
    // Each manifest's settings, read as `atomic run` from its directory
    // would read them, contacting nothing: a setting run would refuse fails
    // here instead.
    let mut manifests = vec![project.clone()];
    if project.members.is_some() {
        manifests.extend(project::members(&project)?);
    }
    for manifest in &manifests {
        settings::check(&manifest.dir).map_err(|e| {
            failure::Failure::new(
                "config.invalid",
                format!("the configuration cannot be read: {e}"),
                "fix the setting it names; `atomic config --explain` lists every setting and \
                 where it comes from",
            )
        })?;
    }
    let files = collections::collect(&project)?;
    collections::load_schemas(&project, &mut registry).map_err(anyhow::Error::msg)?;
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
            // Redacted here, so the JSON form is too, not only the log lines.
            let w = format!("{}: {w}", loaded.file.display());
            warnings.push(atomic_core::redact::text(&w).into_owned());
        }
    }
    // Each OpenAPI toolset must give tools whose schemas compile.
    let mut operations = 0;
    for source in &files["toolsets"] {
        let text = std::fs::read_to_string(&source.path)?;
        let spec = if source.path.extension().is_some_and(|e| e == "json") {
            serde_json::from_str(&text)?
        } else {
            atomic_schema::yaml::load(&source.path, &text)?.value
        };
        let tools = atomic_schema::openapi::operations(&spec)
            .map_err(|e| anyhow::anyhow!("{}: {e}", source.path.display()))?;
        for tool in &tools {
            for schema in std::iter::once(&tool.input).chain(tool.output.as_ref()) {
                atomic_schema::registry::Registry::new()
                    .validator_for(schema)
                    .map_err(|e| {
                        anyhow::anyhow!("{}: operation `{}`: {e}", source.path.display(), tool.name)
                    })?;
            }
        }
        operations += tools.len();
    }
    if json {
        println!(
            "{}",
            serde_json::json!({
                "checked": {
                    "atoms": atoms.len(),
                    "workflows": files["workflows"].len(),
                    "toolset_operations": operations,
                },
                "warnings": warnings,
            })
        );
    } else {
        for w in &warnings {
            atomic_core::log_line!("warning: {w}");
        }
        println!(
            "checked {} Astro atoms, {} workflows and {} toolset operations",
            atoms.len(),
            files["workflows"].len(),
            operations
        );
    }
    Ok(())
}

fn schemas(path: Option<PathBuf>) -> anyhow::Result<()> {
    use atomic_core::{atom, collections, editor};
    let path = path.unwrap_or(std::env::current_dir()?);
    let project = project::discover(&path)?;
    let files = collections::collect(&project)?;
    let mut registry = atomic_schema::registry::Registry::new();
    collections::load_schemas(&project, &mut registry).map_err(anyhow::Error::msg)?;
    let mut atoms = std::collections::BTreeMap::new();
    for source in &files["atoms"] {
        let atom = atom::load(&source.path, &mut registry)?;
        atoms.insert(atom.name.clone(), atom);
    }
    // Schema IDs a contract can reference by name; a file's own URI is not one.
    let mut ids: Vec<String> = registry
        .entries()
        .map(|entry| entry.id.clone())
        .filter(|id| !id.starts_with("file:") && !id.starts_with("atomic:"))
        .collect();
    ids.sort();
    ids.dedup();
    let dir = project.dir.join(&project.generated).join("schemas");
    std::fs::create_dir_all(&dir)?;
    for (name, schema) in [
        (
            "workflow.schema.json",
            editor::workflow_schema(&atoms, &ids),
        ),
        ("manifest.schema.json", editor::manifest_schema()),
    ] {
        let file = dir.join(name);
        std::fs::write(&file, serde_json::to_string_pretty(&schema)? + "\n")?;
        println!("wrote {}", file.display());
    }
    let relative = project.generated.join("schemas");
    println!(
        "Point an editor at them with a first line, the path relative to the file; for \
         workflows/x.workflow.yaml:\n  # yaml-language-server: $schema=../{}/workflow.schema.json\n\
         and for atomic.yaml:\n  # yaml-language-server: $schema={}/manifest.schema.json",
        relative.display(),
        relative.display()
    );
    Ok(())
}

fn expand(name: &str) -> anyhow::Result<()> {
    use atomic_core::{atom, collections};
    let file = std::path::Path::new(name);
    let file = if file.is_file() {
        file.to_owned()
    } else {
        let project = project::discover(&std::env::current_dir()?)?;
        let files = collections::collect(&project)?;
        files["atoms"]
            .iter()
            .find(|source| source.name == name)
            .map(|source| source.path.clone())
            .ok_or_else(|| anyhow::anyhow!("no atom named `{name}` in {}", project.name))?
    };
    let mut registry = atomic_schema::registry::Registry::new();
    load_project_schemas(
        file.parent().unwrap_or(std::path::Path::new(".")),
        &mut registry,
    )?;
    let atom = atom::load(&file, &mut registry)?;
    // Each contract as a standalone JSON Schema 2020-12 document.
    let standalone = |contract: &serde_json::Value| {
        let mut out = serde_json::Map::new();
        out.insert(
            "$schema".into(),
            serde_json::json!("https://json-schema.org/draft/2020-12/schema"),
        );
        if let Some(fields) = contract.as_object() {
            for (key, value) in fields {
                out.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        serde_json::Value::Object(out)
    };
    let contracts = &atom.program.contracts;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "input": standalone(&contracts["Input"]),
            "output": standalone(&contracts["Output"]),
        }))?
    );
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
                    context::checkpoint(&atom, &state.context_dir(), &keys, run_id, resolve)
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
            origins: None,
            step: None,
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

/// What `atomic debug codex-prompt` and `atomic debug probe` say on stderr:
/// their turn is not metered.
const UNMETERED: &str = "atomic debug: this turn is not metered: no budget caps it and the \
                         usage ledger does not record it";

async fn prompt(
    paths: &CodexPaths,
    state: &ProjectState,
    args: &PromptArgs,
) -> anyhow::Result<String> {
    // The only model call atomic makes without a meter: said, never silent.
    atomic_core::log_line!("{UNMETERED}");
    // A backend of its own, beside the one `atomic run` and `atomic serve`
    // use: this command takes no execution lock, and connecting empties a
    // backend's shared temporary directory, which their turns may be using.
    let backend = atomic_exec::Backend::connect(atomic_exec::LocalBackend::in_dir(
        state.dir().join("scratch-debug"),
    ))
    .context("connect to the execution backend")?;
    let runtime = atomic_core::runtime::CodexRuntime {
        paths: paths.clone(),
        codex_home: state.codex_home(),
        cwd: state.root().to_owned(),
        endpoint: args.endpoint(),
        defaults: atomic_core::AtomConfig::default(),
        artifacts: atomic_core::runtime::Artifacts::none(),
        meter: atomic_core::usage::Metering::none(),
        guard: codex::StateGuard::for_project(state)?,
        backend,
    };
    let spec = codex::TurnSpec {
        prompt: args.prompt.join(" "),
        ..codex::TurnSpec::default()
    };
    Ok(atomic_core::runtime::run_turn(&runtime, &spec).await?)
}

/// `atomic model`.
#[derive(Debug, Subcommand)]
enum ModelCommand {
    /// Sign in with ChatGPT, through Codex's login, so Codex atoms run on
    /// your plan.
    Login {
        /// Sign in with a code on another device, for a machine with no
        /// browser.
        #[arg(long)]
        device_auth: bool,
    },
    /// Say who is signed in (the email masked), and where the sign-in is
    /// kept, never a token.
    Status,
    /// Revoke the sign-in where the issuer allows, and forget every copy.
    Logout,
}

async fn model(
    command: ModelCommand,
    project: Option<&std::path::Path>,
    json: bool,
) -> anyhow::Result<()> {
    use atomic_core::signin::SignIn;
    // Saying who is signed in creates nothing.
    let sign_in = match command {
        ModelCommand::Status => SignIn::located(project)?,
        _ => SignIn::configured(project)?,
    };
    match command {
        ModelCommand::Status => match sign_in.account()? {
            // The email is shown masked, built afresh from its first
            // character and its domain, never as the sign-in holds it.
            Some(a) if json => {
                let account = serde_json::json!({
                    "mode": a.mode,
                    "email": a.email.as_deref().map(atomic_core::signin::mask_email),
                    "plan": a.plan,
                    "store": a.store,
                });
                println!(
                    "{}",
                    serde_json::json!({"signed_in": true, "account": account})
                );
                Ok(())
            }
            Some(a) => {
                let mut said = format!("signed in with {}", a.mode);
                if let Some(email) = a.email.as_deref().map(atomic_core::signin::mask_email) {
                    said.push_str(&format!(" as {email}"));
                }
                if let Some(plan) = &a.plan {
                    said.push_str(&format!(" ({plan} plan)"));
                }
                println!(
                    "{said}, kept in {}; Codex atoms use it when no model endpoint is configured",
                    a.store
                );
                Ok(())
            }
            None => Err(failure::Failure::new(
                "model.signed_out",
                "no one is signed in with ChatGPT",
                "sign in with `atomic model login` (or `atomic model login --device-auth`)",
            )
            .into()),
        },
        ModelCommand::Login { device_auth } => {
            sign_in.login(device_auth).await?;
            println!("signed in; Codex atoms use it when no model endpoint is configured");
            Ok(())
        }
        ModelCommand::Logout => {
            let forgot = sign_in.logout().await?;
            let mut removed = Vec::new();
            if forgot.file {
                removed.push("the credential file store");
            }
            if forgot.keyring == Ok(true) {
                removed.push("the OS keyring");
            }
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "signed_out": !removed.is_empty(),
                        "file": forgot.file,
                        "keyring": forgot.keyring.as_ref().ok(),
                        "keyring_error": forgot.keyring.as_ref().err(),
                    })
                );
                return Ok(());
            }
            if removed.is_empty() {
                println!("no one was signed in");
            } else {
                println!(
                    "signed out: forgot the sign-in in {}",
                    removed.join(" and ")
                );
            }
            if let Err(why) = &forgot.keyring {
                println!("the OS keyring could not be checked: {why}");
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mains_error_line_is_redacted() {
        let sample = concat!("glpat-", "MAINerrorLINEsample0123");
        let e = anyhow::anyhow!("the provider said {sample}").context("start Codex");
        let line = error_line(&e);
        assert!(
            line == "error: start Codex: the provider said [REDACTED]",
            "the line was not redacted as expected"
        );
    }
}
