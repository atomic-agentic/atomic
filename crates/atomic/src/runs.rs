// SPDX-License-Identifier: Apache-2.0

//! `atomic run`, `atomic runs resume` and `atomic serve`: the foreground
//! control plane.
//!
//! Each takes the project's execution lock first, so only one process at a
//! time executes a project's runs; another is refused and told who holds
//! it. `atomic run` starts a new durable run and prints its id before
//! anything runs, so a run cut short can be continued. `atomic runs resume`
//! continues that same run from its checkpoints, with the definitions and
//! input it started with: nothing finished runs again, and a gate keeps its
//! original deadline. `atomic serve` recovers every unfinished run this
//! process may recover and keeps running, so gates decided from another
//! terminal let their runs go on, until it is stopped.

use crate::engine::{self, EngineArgs};
use crate::failure::{Failure, Said};
use crate::target;
use atomic_app::pipeline::PIPELINE;
use atomic_core::codex::CodexPaths;
use atomic_core::state::ProjectState;
use atomic_durable::{DurableError, DurableRuntime, RunStatus};
use clap::Args;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How often `atomic serve` wakes runs whose gates were decided.
const REDELIVERY: Duration = Duration::from_secs(5);

/// The state of a run the engine parked for its recovery count.
const PARKED: &str = "MAX_RECOVERY_ATTEMPTS_EXCEEDED";

/// How long a command follows a run whose workflow code panicked until the
/// engine has parked it.
const PARKING: std::time::Duration = std::time::Duration::from_secs(30);

/// How long a stopping process gives running work to reach a checkpoint.
const SHUTDOWN: Duration = Duration::from_secs(10);

/// `atomic run`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// An atom or workflow name in the project, or the path to an
    /// .atom.astro or .workflow.yaml file.
    pub target: String,
    /// A JSON file with the input, or - to read it from standard input.
    #[arg(long, value_name = "FILE")]
    pub input: Option<PathBuf>,
    /// The input as JSON on the command line.
    #[arg(long, value_name = "JSON")]
    pub data: Option<String>,
    /// The id to give the run; by default a new one.
    #[arg(long)]
    pub run_id: Option<String>,
    /// For a workflow that changes code: the repository, already on disk,
    /// its worktrees are made from. The run never writes to it.
    #[arg(long, value_name = "REPO", requires = "fixture_rev")]
    pub fixture: Option<PathBuf>,
    /// The revision of `--fixture` the run starts from: a commit ID or a
    /// branch or tag name, pinned to its commit when the run is admitted.
    #[arg(long, value_name = "REV", requires = "fixture")]
    pub fixture_rev: Option<String>,
    #[command(flatten)]
    pub engine: EngineArgs,
}

/// Bind `run` to its fixture, as `args` name one, before anything runs:
/// a run that changes code needs one, and every git-commit input must fit
/// it.
fn bind_fixture(
    run: &mut atomic_app::pipeline::PipelineRun,
    args: &RunArgs,
    state: &ProjectState,
    cwd: &Path,
) -> anyhow::Result<()> {
    use atomic_core::fixture::{self, FixtureError};
    let (Some(repo), Some(rev)) = (&args.fixture, &args.fixture_rev) else {
        if run.pipeline.changes_code() {
            return Err(Failure::new(
                "fixture.required",
                format!(
                    "workflow `{}` changes code in a worktree, and no fixture repository is \
                     named; atomic never works in your own checkout",
                    run.pipeline.name
                ),
                "name the repository and the revision to start from: --fixture <repo> \
                 --fixture-rev <commit or branch>",
            )
            .into());
        }
        return Ok(());
    };
    let repo = if repo.is_absolute() {
        repo.clone()
    } else {
        cwd.join(repo)
    };
    let fail = |e: FixtureError| -> anyhow::Error {
        let (kind, action) = match &e {
            FixtureError::Path { .. } => (
                "fixture.path",
                "give --fixture the path of a git repository on this machine",
            ),
            FixtureError::Revision { .. } => (
                "fixture.revision",
                "give --fixture-rev a commit ID, or a branch or tag name, of that repository",
            ),
            FixtureError::Unsupported(_) => (
                "fixture.unsupported",
                "prepare the fixture so its commit holds everything as files, then bind it again",
            ),
            FixtureError::Schema(_) => (
                "fixture.schema",
                "fix the workflow's input schema: type each commit field through `properties` \
                 and `items` only",
            ),
            FixtureError::Occupied(_) => (
                "fixture.snapshot",
                "move what is there out of the way, after checking no run is working in it, \
                 then run again",
            ),
            FixtureError::Input { .. } => (
                "fixture.input",
                "give each git-commit field of the input a commit of the fixture, and its base \
                 the fixture's revision",
            ),
            FixtureError::Git(_) | FixtureError::Io { .. } => (
                "fixture.snapshot",
                "fix what the description names, then run again",
            ),
        };
        Failure::new(kind, e.to_string(), action).into()
    };
    let binding = fixture::bind(state, &repo, rev).map_err(fail)?;
    fixture::check_commits(&binding, &run.pipeline.input, &run.input).map_err(fail)?;
    run.pipeline.fixture = Some(binding);
    Ok(())
}

/// `atomic runs resume`.
#[derive(Debug, Args)]
pub struct ResumeArgs {
    /// The id `atomic run` printed.
    pub run_id: String,
    /// On a shared store, assert that the executor that owns the run has
    /// stopped, so this process may take the run over. Wrong, it lets two
    /// processes run the same steps at once.
    #[arg(long)]
    pub owner_stopped: bool,
    #[command(flatten)]
    pub engine: EngineArgs,
}

/// `atomic serve`.
#[derive(Debug, Args)]
pub struct ServeArgs {
    #[command(flatten)]
    pub engine: EngineArgs,
}

/// A new run id: the time it started and some randomness.
fn new_run_id() -> anyhow::Result<String> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut random = [0u8; 5];
    getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!("random run id: {e}"))?;
    let random: String = random.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("run-{millis:x}-{random}"))
}

/// Print a run's result: its output alone, or with `json`, an object that
/// names the run too.
/// What a model or a command wrote is in it: escaped for the terminal, and
/// still the same JSON.
fn print_result(run_id: &str, output: &Value, json: bool) -> anyhow::Result<()> {
    let text = if json {
        crate::shown::compact(&json!({"run": run_id, "status": "succeeded", "output": output}))?
    } else {
        crate::shown::pretty(output)?
    };
    println!("{text}");
    Ok(())
}

/// Wait for run `run_id` and turn its outcome into the command's.
///
/// A signal stops the command as one stops `atomic serve` (see [`stop`]):
/// the run stays unfinished, for `atomic runs resume`.
///
/// While it waits, the terminal prompt shows each gate the run opens, so the
/// person running the command can decide it there (see [`crate::prompt`]).
async fn finish(
    engine: &engine::Engine,
    state: &ProjectState,
    run_id: &str,
    json: bool,
    signals: &mut Signals,
    who: &str,
) -> anyhow::Result<()> {
    // Whether the engine was seen to park a run whose workflow code
    // panicked, before this command stopped.
    let parked_seen = std::cell::Cell::new(false);
    let follow = async {
        let outcome = engine.rt.result::<Value>(run_id).await;
        // A panic in atomic's own workflow code parks the run: there is no
        // outcome to wait for, but the engine parks it a moment later, and
        // stopping first would leave it to run again. Follow it there, for a
        // bounded time even if the store stops answering.
        if outcome.is_err() && atomic_app::panics::noted(run_id).is_some() {
            let parked = tokio::time::timeout(PARKING, async {
                while !matches!(
                    engine.rt.status(run_id).await,
                    Ok(Some(RunStatus::Other(ref state))) if state == PARKED
                ) {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            })
            .await;
            parked_seen.set(parked.is_ok());
            return outcome;
        }
        if matches!(outcome, Err(DurableError::Interrupted(_))) {
            // This execution stopped without an outcome, as one whose exec
            // command's result was lost does, and the engine recovers it in
            // this process; follow the run by its id to the outcome recovery
            // reaches.
            return engine.rt.result::<Value>(run_id).await;
        }
        outcome
    };
    let follow = async {
        tokio::select! {
            outcome = follow => outcome,
            () = crate::prompt::at_gates(&engine.rt, state, run_id) => {
                unreachable!("the prompt runs until the run ends")
            }
        }
    };
    let outcome = tokio::select! {
        outcome = follow => Some(outcome),
        signal = signals.next() => {
            // Names and atomic's words only, not redacted, since a run id
            // such as `rotate-secret` is not a secret, but escaped for the
            // terminal.
            let run_id = crate::shown::line(run_id);
            atomic_core::redact::Line::new()
                .shown(&format!(
                    "{who}: {signal}: run {run_id} stays unfinished; continue it with \
                     `atomic runs resume {run_id}`"
                ))
                .log();
            None
        }
    };
    let Some(outcome) = outcome else {
        stop(signals, who, Exit::Signalled).await
    };
    let _ = engine.rt.shutdown(SHUTDOWN).await;
    // Only a panic this process saw: a note kept from an earlier run of the
    // same id is not this outcome's cause.
    let panicked = match &outcome {
        Err(_) => atomic_app::panics::noted(run_id),
        Ok(_) => None,
    };
    match outcome {
        Ok(output) => print_result(run_id, &output, json),
        Err(_) if panicked.is_some() => {
            use atomic_app::panics::{HINT, HINT_END, NOT_PARKED, NOT_RETRIED, PANICKED};
            let message = panicked.unwrap_or_default();
            let end = if parked_seen.get() {
                NOT_RETRIED
            } else {
                NOT_PARKED
            };
            let cause = Said::new()
                .text("run ")
                .name(run_id)
                .text(format!("{PANICKED}{message}{end}"));
            let hint = Said::new().text(HINT).name(run_id).text(HINT_END);
            Err(Failure::named("run.panicked", cause, hint).into())
        }
        Err(DurableError::Failed(why)) if atomic_app::pipeline::dead_lettered(run_id) => {
            Err(Failure::named(
                "run.dead_lettered",
                Said::new()
                    .text("run ")
                    .name(run_id)
                    .text(format!(" failed: {why}")),
                Said::new().text(
                    "find why its step keeps stopping, such as a command whose runner dies under \
                     it, fix that, and start a new run",
                ),
            )
            .into())
        }
        Err(DurableError::Failed(why)) => Err(Failure::named(
            "run.failed",
            Said::new()
                .text("run ")
                .name(run_id)
                .text(format!(" failed: {why}")),
            Said::new().text(
                "fix what the description names and start a new run; this run's record stays in \
                 the store",
            ),
        )
        .into()),
        Err(e)
            if matches!(
                engine.rt.status(run_id).await,
                Ok(Some(RunStatus::Other(ref state))) if state == "MAX_RECOVERY_ATTEMPTS_EXCEEDED"
            ) =>
        {
            Err(Failure::named(
                "run.parked",
                Said::new().text("run ").name(run_id).text(format!(
                    " stopped without finishing more times than the engine allows, so it is \
                     parked: {e}"
                )),
                Said::new()
                    .text("find why it keeps stopping, then continue it with `atomic runs resume ")
                    .name(run_id)
                    .text("`"),
            )
            .into())
        }
        Err(e) => Err(Failure::named(
            "run.interrupted",
            Said::new()
                .text("run ")
                .name(run_id)
                .text(format!(" did not finish: {e}")),
            Said::new()
                .text("fix the cause, then continue the same run with `atomic runs resume ")
                .name(run_id)
                .text("`"),
        )
        .into()),
    }
}

/// `atomic run`.
pub async fn run(
    args: RunArgs,
    project: Option<&Path>,
    paths: &CodexPaths,
    json: bool,
) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let found = target::project_for(project, Some(&args.target), &cwd)?;
    let state = ProjectState::resolve(found.root())?;
    let input = target::input(args.input.as_deref(), args.data.as_deref(), &cwd)?;
    let mut run = target::load(&found, &args.target, &cwd, input, &engine::deployment())?;
    // Before any model is called, and before the run is recorded.
    state.create()?;
    bind_fixture(&mut run, &args, &state, &cwd)?;
    // Once the input is read and the run admitted: until then, a fixture's
    // copy included, a signal stops atomic as usual.
    let mut signals = Signals::new()?;
    let engine = engine::open(
        &state,
        paths,
        &args.engine,
        "atomic run",
        target::needs_model(&run),
        engine::Takes::Own,
    )
    .await?;
    // Refused before it starts, not at its first lite step.
    let lite = target::lite_atoms(&run);
    if engine.signed_in && !lite.is_empty() {
        let _ = engine.rt.shutdown(SHUTDOWN).await;
        return Err(Failure::new(
            "config.model",
            format!(
                "{} calls a model API itself (lite: {}), and the ChatGPT sign-in serves Codex \
                 atoms only",
                run.pipeline.name,
                lite.join(", ")
            ),
            "set ATOMIC_MODEL_BASE_URL and ATOMIC_MODEL to an OpenAI-compatible endpoint, and \
             ATOMIC_MODEL_API_KEY_ENV to the variable holding its key if it needs one",
        )
        .into());
    }
    let run_id = match args.run_id {
        Some(id) => id,
        None => new_run_id()?,
    };
    // The engine treats a run id as an idempotency key: starting one that
    // exists would wait for that run, whatever it runs, instead of this one.
    // Checked under the lock, so no other local process starts it meanwhile.
    // Taken by another project on a shared store: said with nothing of
    // their run, which this project can neither read nor resume.
    if engine.rt.run_id_taken_elsewhere(&run_id).await? {
        let _ = engine.rt.shutdown(SHUTDOWN).await;
        return Err(Failure::named(
            "run.exists",
            Said::new()
                .text("the run id ")
                .name(&run_id)
                .text(" is taken in this store by another project"),
            Said::new().text("give a new id, or leave out --run-id for one"),
        )
        .into());
    }
    if let Some(existing) = engine.rt.run_record(&run_id).await? {
        let _ = engine.rt.shutdown(SHUTDOWN).await;
        let state = match existing.status {
            RunStatus::Pending => "unfinished".to_owned(),
            RunStatus::Succeeded => "finished".to_owned(),
            RunStatus::Failed(_) => "failed".to_owned(),
            RunStatus::Other(state) => state.to_lowercase(),
        };
        return Err(Failure::named(
            "run.exists",
            Said::new()
                .text("there is already a run ")
                .name(&run_id)
                .text(format!(" in this project's store, and it is {state}")),
            Said::new()
                .text(
                    "give a new id, or leave out --run-id for one; to continue or read that run, \
                     use `atomic runs resume ",
                )
                .name(&run_id)
                .text("`"),
        )
        .into());
    }
    engine.launch().await?;
    // Before anything runs, so a run cut short can be resumed by its id.
    // Names only, not redacted, since a run id such as `rotate-secret` is
    // not a secret, but escaped for the terminal.
    atomic_core::redact::Line::new()
        .shown(&format!(
            "run {}: {} in project {}",
            crate::shown::line(&run_id),
            crate::shown::line(&run.pipeline.name),
            crate::shown::line(&found.name)
        ))
        .log();
    if let Some(f) = &run.pipeline.fixture {
        atomic_core::redact::Line::new()
            .plain("run ")
            .shown(&run_id)
            .plain(": fixture ")
            .redacted(&f.source.display().to_string())
            .plain(" at ")
            .redacted(&f.rev)
            .plain(" pinned to commit ")
            .shown(&f.commit)
            .log();
    }
    // What its input names is kept from a sweep until the input is recorded;
    // a field its contract types as an artifact must name stored content.
    use atomic_app::retention::start_keeping_input as admit;
    let typed = Some((run.pipeline.input.clone(), run.input.clone()));
    // A signal while it waits for the store stops the wait, and admission
    // ends there; one that comes once the run is recorded stops the run.
    let halt: std::sync::Arc<std::sync::atomic::AtomicBool> = Default::default();
    let told = halt.clone();
    let (artifacts, rt) = (&engine.artifacts, &engine.rt);
    let admission = admit(artifacts, rt, PIPELINE, &run_id, run, typed, told);
    tokio::pin!(admission);
    let mut signalled = None;
    let admitted = tokio::select! {
        admitted = &mut admission => admitted,
        signal = signals.next() => {
            halt.store(true, std::sync::atomic::Ordering::SeqCst);
            signalled = Some(signal);
            admission.await
        }
    };
    if let (Ok(()), Some(signal)) = (&admitted, signalled) {
        let shown = crate::shown::line(&run_id);
        atomic_core::redact::Line::new()
            .shown(&format!(
                "atomic run: {signal}: run {shown} stays unfinished; continue it with \
                 `atomic runs resume {shown}`"
            ))
            .log();
        stop(&mut signals, "atomic run", Exit::Signalled).await
    }
    if let Err(e) = admitted {
        let refused = e
            .downcast_ref::<atomic_app::retention::InputRefused>()
            .map(|r| match r {
                atomic_app::retention::InputRefused::Absent(_) => Failure::new(
                    "run.input_missing",
                    r.to_string(),
                    "store the content again, or name content the store holds, then start the run",
                ),
                atomic_app::retention::InputRefused::Locked(_) => Failure::new(
                    "store.locked",
                    r.to_string(),
                    "let the sweep finish, or stop it, then start the run again",
                ),
                atomic_app::retention::InputRefused::Stopped => Failure::new(
                    "run.stopped",
                    r.to_string(),
                    "nothing was recorded; start the run again",
                ),
                atomic_app::retention::InputRefused::Invalid(_) => Failure::new(
                    "input.invalid",
                    r.to_string(),
                    "name content of the kind the workflow's input contract asks for",
                ),
            });
        let _ = engine.rt.shutdown(SHUTDOWN).await;
        return Err(refused.map_or(e, Into::into));
    }
    finish(&engine, &state, &run_id, json, &mut signals, "atomic run").await
}

/// The state of the project a command that names no target works on.
pub(crate) fn state(project: Option<&Path>) -> anyhow::Result<ProjectState> {
    let cwd = std::env::current_dir()?;
    let found = target::project_for(project, None, &cwd)?;
    Ok(ProjectState::resolve(found.root())?)
}

/// `atomic runs resume`.
pub async fn resume(
    args: ResumeArgs,
    project: Option<&Path>,
    paths: &CodexPaths,
    json: bool,
) -> anyhow::Result<()> {
    let mut signals = Signals::new()?;
    let state = state(project)?;
    let engine = engine::open(
        &state,
        paths,
        &args.engine,
        "atomic runs resume",
        true,
        engine::Takes::Run(args.run_id.clone()),
    )
    .await?;
    let id = &args.run_id;
    let record = engine.rt.own_run_record(id).await?;
    match record.as_ref().map(|r| &r.status) {
        None => {
            let _ = engine.rt.shutdown(SHUTDOWN).await;
            Err(Failure::named(
                "run.not_found",
                Said::new().text("no run ").name(id).text(format!(
                    " in this project's store ({})",
                    engine.store_description
                )),
                Said::new().text(
                    "give the id `atomic run` printed, from the project that ran it (or pass \
                     --project)",
                ),
            )
            .into())
        }
        Some(RunStatus::Failed(why)) => {
            let _ = engine.rt.shutdown(SHUTDOWN).await;
            Err(Failure::named(
                "run.failed",
                Said::new().text("run ").name(id).text(format!(
                    " already failed, so there is nothing to resume: {why}"
                )),
                Said::new().text("fix what the description names and start a new run"),
            )
            .into())
        }
        Some(RunStatus::Succeeded) => {
            engine.launch().await?;
            finish(
                &engine,
                &state,
                id,
                json,
                &mut signals,
                "atomic runs resume",
            )
            .await
        }
        Some(RunStatus::Other(state)) if state != PARKED => {
            let _ = engine.rt.shutdown(SHUTDOWN).await;
            Err(Failure::named(
                "run.not_resumable",
                Said::new()
                    .text("run ")
                    .name(id)
                    .text(format!(" is {state}, so it cannot be resumed")),
                Said::new().text("start a new run"),
            )
            .into())
        }
        // A run parked for its recovery count resumes with a fresh count.
        Some(RunStatus::Pending | RunStatus::Other(_)) => {
            let record = record.as_ref().expect("a status has a record");
            // The engine runs only runs of its own layout: resuming another
            // would wait for ever.
            if record.version != engine.rt.version() {
                let _ = engine.rt.shutdown(SHUTDOWN).await;
                return Err(Failure::named(
                    "run.other_layout",
                    Said::new()
                        .text("run ")
                        .name(id)
                        .text(" was started by an atomic whose durable step layout is ")
                        .name(&record.version)
                        .text(", and this atomic's is ")
                        .name(engine.rt.version())
                        .text(", so it cannot continue the run"),
                    Said::new().text(
                        "continue it with an atomic of the layout that started it, or start a \
                         new run",
                    ),
                )
                .into());
            }
            // A run parked because atomic's own workflow code panicked will
            // likely panic again: say so, and note afresh what this resume
            // finds.
            if let Some(message) = atomic_app::panics::of(id) {
                atomic_core::redact::Line::new()
                    .plain("run ")
                    .shown(id)
                    .plain(": it was parked because atomic's own workflow code panicked (")
                    .redacted(&message)
                    .plain(
                        "); resuming it, the panic will likely happen again unless this atomic \
                         has it fixed",
                    )
                    .log();
            }
            // The execution lock covers this host only. On a shared store
            // another live process may own the run, and nothing fences it.
            let owner = record.owner().unwrap_or_default();
            if engine.shared && owner != engine.rt.executor_id() && !args.owner_stopped {
                let _ = engine.rt.shutdown(SHUTDOWN).await;
                return Err(Failure::named(
                    "run.owned",
                    Said::new()
                        .text("run ")
                        .name(id)
                        .text(" is owned by executor ")
                        .name(owner)
                        .text(", not this one (")
                        .name(engine.rt.executor_id())
                        .text("), and may still be running there"),
                    Said::new()
                        .text("let executor ")
                        .name(owner)
                        .text(
                            " finish it, or restart its `atomic serve`; if that executor has \
                             stopped for good, run `atomic runs resume ",
                        )
                        .name(id)
                        .text(" --owner-stopped`"),
                )
                .into());
            }
            atomic_core::redact::Line::new()
                .shown(&format!("run {}: resuming", crate::shown::line(id)))
                .log();
            // Queued before this engine dispatches, so it takes the run once.
            engine.rt.resume(id).await?;
            // Resumed: what it does now is noted afresh.
            atomic_app::panics::forget(id);
            atomic_durable::failpoint("resume:queued");
            engine.launch().await?;
            // A gate decided, or a park released, while the run was down
            // may never have woken it.
            atomic_durable::gate::redeliver(&engine.rt).await?;
            atomic_durable::release::redeliver(&engine.rt).await?;
            finish(
                &engine,
                &state,
                id,
                json,
                &mut signals,
                "atomic runs resume",
            )
            .await
        }
    }
}

/// The signals that stop a foreground server, listened for from the start
/// so that none is missed between the first and the second.
struct Signals {
    #[cfg(unix)]
    streams: Vec<(&'static str, tokio::signal::unix::Signal)>,
}

impl Signals {
    /// Each stop signal this process does not ignore. One it inherited as
    /// ignored, as under `nohup` or for a shell's background job, stays
    /// ignored: registering a handler would replace that.
    fn new() -> anyhow::Result<Self> {
        #[cfg(unix)]
        {
            use atomic_exec::process::{StopSignal, ignored};
            use tokio::signal::unix::{SignalKind, signal};
            let mut streams = Vec::new();
            for (name, stop, kind) in [
                ("SIGTERM", StopSignal::Term, SignalKind::terminate()),
                ("SIGINT", StopSignal::Int, SignalKind::interrupt()),
                ("SIGHUP", StopSignal::Hup, SignalKind::hangup()),
                ("SIGQUIT", StopSignal::Quit, SignalKind::quit()),
            ] {
                if !ignored(stop) {
                    streams.push((name, signal(kind)?));
                }
            }
            Ok(Self { streams })
        }
        #[cfg(not(unix))]
        Ok(Self {})
    }

    /// The next one.
    async fn next(&mut self) -> &'static str {
        #[cfg(unix)]
        {
            if self.streams.is_empty() {
                return std::future::pending().await;
            }
            std::future::poll_fn(|cx| {
                for (name, stream) in self.streams.iter_mut() {
                    if stream.poll_recv(cx).is_ready() {
                        return std::task::Poll::Ready(*name);
                    }
                }
                std::task::Poll::Pending
            })
            .await
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            "Ctrl-C"
        }
    }
}

/// How a stopped process exits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Exit {
    /// `atomic serve`: 0 if everything reached a checkpoint, else 1.
    Served,
    /// A command stopped short of its result: 130, as for an interrupt.
    Signalled,
}

/// Stop this process after a signal: running steps have [`SHUTDOWN`] to
/// reach a checkpoint, or until another signal. Whatever command is still
/// running then is killed, with everything it started, and the process
/// exits without recording how it ended; only its exit releases the
/// execution lock, so no other process can recover a run while a command of
/// this one still runs. Each interrupted command's attempt guard decides
/// whether it may run again.
async fn stop(signals: &mut Signals, who: &str, exit: Exit) -> ! {
    atomic_core::log_line!(
        "{who}: stopping; running steps have {} seconds to reach a checkpoint (send another \
         signal to stop now)",
        SHUTDOWN.as_secs()
    );
    // Nothing begins from here; what runs may finish its step.
    atomic_app::pipeline::stop_starting_steps();
    let idle = async {
        while atomic_app::pipeline::steps_running() > 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    let forced = tokio::select! {
        _ = tokio::time::timeout(SHUTDOWN, idle) => None,
        again = signals.next() => Some(again),
    };
    if let Some(again) = forced {
        atomic_core::log_line!("{who}: {again}: stopping now");
    }
    // The engine's drain ends at its time limit whether or not its steps
    // did, so whatever still runs is stopped here, and its end is never
    // recorded: this process exits first.
    // It returns once every process this one started has ended, so the
    // execution lock, which goes with this process, outlasts them all.
    let stopped = atomic_exec::process::stop_all(&mut |what| {
        atomic_core::log_line!("{who}: {what}");
    });
    if stopped > 0 {
        atomic_core::log_line!(
            "{who}: stopped {stopped} command(s) that had not finished; their runs continue with \
             the next `atomic serve` or `atomic runs resume`"
        );
    }
    // A model call still in flight ends with this process: charge it the
    // time it ran, not the time until a later process finds it dead.
    let interrupted = tokio::time::timeout(
        Duration::from_secs(2),
        atomic_app::metering::interrupt_in_flight(),
    )
    .await
    .unwrap_or(0);
    if interrupted > 0 {
        atomic_core::log_line!(
            "{who}: settled {interrupted} model call(s) or command(s) that had not finished"
        );
    }
    // Spans still open end and are sent; a collector that does not answer
    // holds this up until another signal or the flush's deadline, whichever
    // is first.
    let flushing = tokio::task::spawn_blocking(atomic_app::telemetry::flush_blocking);
    tokio::select! {
        _ = flushing => {}
        again = signals.next() => {
            atomic_core::log_line!("{who}: {again}: stopping now, without sending the last spans");
        }
    }
    // Once more, for what started since, such as a model turn's command.
    atomic_exec::process::end_all(&mut |what| atomic_core::log_line!("{who}: {what}"));
    // Exit here, not by returning: the runtime would wait for the blocked
    // threads of the stopped commands, which never return.
    std::process::exit(match exit {
        Exit::Served if forced.is_none() && stopped == 0 => 0,
        Exit::Served => 1,
        Exit::Signalled => 130,
    })
}

/// `atomic serve`. It runs until a signal [`stop`]s it. Unfinished work
/// stays recorded for the next serve or resume.
pub async fn serve(
    args: ServeArgs,
    project: Option<&Path>,
    paths: &CodexPaths,
) -> anyhow::Result<()> {
    let mut signals = Signals::new()?;
    // One run whose commands cannot be confined here does not stop the
    // others: it stays unfinished, and serve goes on.
    atomic_app::workflows::serve_past_unconfinable_steps();
    // A step that has waited its limit for room is set aside, and keeps
    // trying, rather than stopping serve or waiting for a restart.
    atomic_core::runtime::keep_trying_past_the_limit();
    let state = state(project)?;
    let engine = engine::open(
        &state,
        paths,
        &args.engine,
        "atomic serve",
        true,
        engine::Takes::Waiting,
    )
    .await?;
    atomic_core::log_line!(
        "atomic serve: project {}, store {}, executor {}",
        state.root().display(),
        engine.store_description,
        engine.rt.executor_id()
    );
    let parked = |records: Vec<atomic_durable::RunRecord>| -> std::collections::BTreeSet<String> {
        records
            .into_iter()
            .filter(|r| matches!(&r.status, RunStatus::Other(s) if s == PARKED))
            .map(|r| r.id)
            .collect()
    };
    // Recovered before the engine dispatches: a run waiting on a resume
    // queue is the dispatcher's alone, and one recovery puts back on its
    // queue is taken there once. Recovering after the dispatcher had taken
    // a queued run would claim and queue it again, and run it twice.
    let before = parked(engine.rt.runs_in(PARKED).await?);
    let recovered = engine.rt.recover().await?;
    engine.launch().await?;
    // The engine parks a run whose workflow code panicked, and one it has
    // recovered too often, instead of recovering it once more: name each,
    // with its cause. Only this project's: another's run is not named.
    let mut now = Vec::new();
    for id in parked(engine.rt.runs_in(PARKED).await?) {
        if engine.rt.own_run_record(&id).await?.is_some() {
            now.push(id);
        }
    }
    for id in &now {
        match atomic_app::panics::of(id) {
            // Whenever it panicked, in this process's recovery or an
            // earlier process.
            Some(message) => atomic_app::panics::served(id, &message).log(),
            None if before.contains(id) => {}
            None => atomic_core::redact::Line::new()
                .plain("atomic serve: run ")
                .shown(id)
                .plain(
                    " stopped without finishing more times than the engine allows, so it is \
                     parked; find why, then continue it with `atomic runs resume ",
                )
                .shown(id)
                .plain("`")
                .log(),
        }
    }
    // A gate decided, or a park released, while its run was down may never
    // have woken it.
    atomic_durable::gate::redeliver(&engine.rt).await?;
    atomic_durable::release::redeliver(&engine.rt).await?;
    // Runs of another layout are this executor's too, but only an atomic of
    // their layout can continue them.
    let stranded: Vec<String> = engine
        .rt
        .unfinished()
        .await?
        .into_iter()
        .filter(|r| r.executor == engine.rt.executor_id() && r.version != engine.rt.version())
        .map(|r| format!("{} (layout {})", r.id, r.version))
        .collect();
    if !stranded.is_empty() {
        atomic_core::redact::Line::new()
            .plain("atomic serve: left ")
            .shown(&stranded.len().to_string())
            .plain(
                " unfinished run(s) started by an atomic of another durable step layout, which \
                 this one (",
            )
            .shown(engine.rt.version())
            .plain(") cannot continue: ")
            .shown(&stranded.join(", "))
            .plain("; continue them with an atomic of their layout")
            .log();
    }
    atomic_core::log_line!(
        "atomic serve: recovered {recovered} unfinished run(s); serving until stopped"
    );
    // A decider that stopped before waking its run is caught up within a
    // few seconds, not only at the next start.
    let signal = loop {
        tokio::select! {
            signal = signals.next() => break signal,
            _ = tokio::time::sleep(REDELIVERY) => {
                if let Err(e) = atomic_durable::gate::redeliver(&engine.rt).await {
                    atomic_core::log_line!("atomic serve: waking runs whose gates were decided: {e}");
                }
                if let Err(e) = atomic_durable::release::redeliver(&engine.rt).await {
                    atomic_core::log_line!("atomic serve: waking runs whose parks were released: {e}");
                }
            }
        }
    };
    atomic_core::log_line!("atomic serve: {signal}");
    stop(&mut signals, "atomic serve", Exit::Served).await
}
