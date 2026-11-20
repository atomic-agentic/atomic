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
use crate::failure::Failure;
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
    #[command(flatten)]
    pub engine: EngineArgs,
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
fn print_result(run_id: &str, output: &Value, json: bool) -> anyhow::Result<()> {
    let text = if json {
        serde_json::to_string(&json!({"run": run_id, "status": "succeeded", "output": output}))?
    } else {
        serde_json::to_string_pretty(output)?
    };
    println!("{text}");
    Ok(())
}

/// Wait for run `run_id` and turn its outcome into the command's.
///
/// A signal stops the command as one stops `atomic serve` (see [`stop`]):
/// the run stays unfinished, for `atomic runs resume`.
async fn finish(
    engine: &engine::Engine,
    run_id: &str,
    json: bool,
    signals: &mut Signals,
    who: &str,
) -> anyhow::Result<()> {
    let follow = async {
        let outcome = engine.rt.result::<Value>(run_id).await;
        if matches!(outcome, Err(DurableError::Interrupted(_))) {
            // This execution stopped without an outcome, as one whose exec
            // command's result was lost does, and the engine recovers it in
            // this process; follow the run by its id to the outcome recovery
            // reaches.
            return engine.rt.result::<Value>(run_id).await;
        }
        outcome
    };
    let outcome = tokio::select! {
        outcome = follow => Some(outcome),
        signal = signals.next() => {
            eprintln!(
                "{who}: {signal}: run {run_id} stays unfinished; continue it with \
                 `atomic runs resume {run_id}`"
            );
            None
        }
    };
    let Some(outcome) = outcome else {
        stop(signals, who, Exit::Signalled).await
    };
    let _ = engine.rt.shutdown(SHUTDOWN).await;
    match outcome {
        Ok(output) => print_result(run_id, &output, json),
        Err(DurableError::Failed(why)) if atomic_app::pipeline::dead_lettered(run_id) => {
            Err(Failure::new(
                "run.dead_lettered",
                format!("run {run_id} failed: {why}"),
                "find why its step keeps stopping, such as a command whose runner dies under it, \
                 fix that, and start a new run",
            )
            .into())
        }
        Err(DurableError::Failed(why)) => Err(Failure::new(
            "run.failed",
            format!("run {run_id} failed: {why}"),
            "fix what the description names and start a new run; this run's record stays in \
             the store",
        )
        .into()),
        Err(e)
            if matches!(
                engine.rt.status(run_id).await,
                Ok(Some(RunStatus::Other(ref state))) if state == "MAX_RECOVERY_ATTEMPTS_EXCEEDED"
            ) =>
        {
            Err(Failure::new(
                "run.parked",
                format!(
                    "run {run_id} was recovered more times than the engine allows, so it is \
                     parked: {e}"
                ),
                format!("continue it with `atomic runs resume {run_id}`"),
            )
            .into())
        }
        Err(e) => Err(Failure::new(
            "run.interrupted",
            format!("run {run_id} did not finish: {e}"),
            format!("fix the cause, then continue the same run with `atomic runs resume {run_id}`"),
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
    // After the input is read: until then a signal stops atomic as usual.
    let mut signals = Signals::new()?;
    let run = target::load(&found, &args.target, &cwd, input, &engine::deployment())?;
    let engine = engine::open(
        &state,
        paths,
        &args.engine,
        "atomic run",
        target::needs_model(&run),
        engine::Takes::Own,
    )
    .await?;
    let run_id = match args.run_id {
        Some(id) => id,
        None => new_run_id()?,
    };
    // The engine treats a run id as an idempotency key: starting one that
    // exists would wait for that run, whatever it runs, instead of this one.
    // Checked under the lock, so no other local process starts it meanwhile.
    if let Some(existing) = engine.rt.run_record(&run_id).await? {
        let _ = engine.rt.shutdown(SHUTDOWN).await;
        let state = match existing.status {
            RunStatus::Pending => "unfinished".to_owned(),
            RunStatus::Succeeded => "finished".to_owned(),
            RunStatus::Failed(_) => "failed".to_owned(),
            RunStatus::Other(state) => state.to_lowercase(),
        };
        return Err(Failure::new(
            "run.exists",
            format!("there is already a run {run_id} in this project's store, and it is {state}"),
            format!(
                "give a new id, or leave out --run-id for one; to continue or read that run, use \
                 `atomic runs resume {run_id}`"
            ),
        )
        .into());
    }
    engine.launch().await?;
    // Before anything runs, so a run cut short can be resumed by its id.
    eprintln!(
        "run {run_id}: {} in project {}",
        run.pipeline.name, found.name
    );
    engine.rt.start(PIPELINE, &run_id, run).await?;
    finish(&engine, &run_id, json, &mut signals, "atomic run").await
}

/// The state of the project a command that names no target works on.
fn state(project: Option<&Path>) -> anyhow::Result<ProjectState> {
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
    let record = engine.rt.run_record(id).await?;
    match record.as_ref().map(|r| &r.status) {
        None => {
            let _ = engine.rt.shutdown(SHUTDOWN).await;
            Err(Failure::new(
                "run.not_found",
                format!("no run {id} in this project's store ({})", engine.store),
                "give the id `atomic run` printed, from the project that ran it (or pass \
                 --project)",
            )
            .into())
        }
        Some(RunStatus::Failed(why)) => {
            let _ = engine.rt.shutdown(SHUTDOWN).await;
            Err(Failure::new(
                "run.failed",
                format!("run {id} already failed, so there is nothing to resume: {why}"),
                "fix what the description names and start a new run",
            )
            .into())
        }
        Some(RunStatus::Succeeded) => {
            engine.launch().await?;
            finish(&engine, id, json, &mut signals, "atomic runs resume").await
        }
        Some(RunStatus::Other(state)) if state != PARKED => {
            let _ = engine.rt.shutdown(SHUTDOWN).await;
            Err(Failure::new(
                "run.not_resumable",
                format!("run {id} is {state}, so it cannot be resumed"),
                "start a new run",
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
                return Err(Failure::new(
                    "run.other_layout",
                    format!(
                        "run {id} was started by an atomic whose durable step layout is {}, and \
                         this atomic's is {}, so it cannot continue the run",
                        record.version,
                        engine.rt.version()
                    ),
                    "continue it with an atomic of the layout that started it, or start a new run",
                )
                .into());
            }
            // The execution lock covers this host only. On a shared store
            // another live process may own the run, and nothing fences it.
            let owner = record.owner().unwrap_or_default();
            if engine.shared && owner != engine.rt.executor_id() && !args.owner_stopped {
                let _ = engine.rt.shutdown(SHUTDOWN).await;
                return Err(Failure::new(
                    "run.owned",
                    format!(
                        "run {id} is owned by executor {}, not this one ({}), and may still be \
                         running there",
                        owner,
                        engine.rt.executor_id()
                    ),
                    format!(
                        "let executor {} finish it, or restart its `atomic serve`; if that \
                         executor has stopped for good, run `atomic runs resume {id} \
                         --owner-stopped`",
                        owner
                    ),
                )
                .into());
            }
            eprintln!("run {id}: resuming");
            // Queued before this engine dispatches, so it takes the run once.
            engine.rt.resume(id).await?;
            atomic_durable::failpoint("resume:queued");
            engine.launch().await?;
            // A gate decided while the run was down may never have woken it.
            atomic_durable::gate::redeliver(&engine.rt).await?;
            finish(&engine, id, json, &mut signals, "atomic runs resume").await
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
    eprintln!(
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
        eprintln!("{who}: {again}: stopping now");
    }
    // The engine's drain ends at its time limit whether or not its steps
    // did, so whatever still runs is stopped here, and its end is never
    // recorded: this process exits first.
    let stopped = atomic_exec::process::stop_all();
    if stopped > 0 {
        eprintln!(
            "{who}: stopped {stopped} command(s) that had not finished; their runs continue with \
             the next `atomic serve` or `atomic runs resume`"
        );
    }
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
    eprintln!(
        "atomic serve: project {}, store {}, executor {}",
        state.root().display(),
        engine.store,
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
    // The engine parks a run it has recovered too often instead of
    // recovering it once more: name each one.
    for id in parked(engine.rt.runs_in(PARKED).await?).difference(&before) {
        eprintln!(
            "atomic serve: run {id} was recovered more times than the engine allows, so it is \
             parked; continue it with `atomic runs resume {id}`"
        );
    }
    // A gate decided while its run was down may never have woken it.
    atomic_durable::gate::redeliver(&engine.rt).await?;
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
        eprintln!(
            "atomic serve: left {} unfinished run(s) started by an atomic of another durable step \
             layout, which this one ({}) cannot continue: {}; continue them with an atomic of \
             their layout",
            stranded.len(),
            engine.rt.version(),
            stranded.join(", ")
        );
    }
    eprintln!("atomic serve: recovered {recovered} unfinished run(s); serving until stopped");
    // A decider that stopped before waking its run is caught up within a
    // few seconds, not only at the next start.
    let signal = loop {
        tokio::select! {
            signal = signals.next() => break signal,
            _ = tokio::time::sleep(REDELIVERY) => {
                if let Err(e) = atomic_durable::gate::redeliver(&engine.rt).await {
                    eprintln!("atomic serve: waking runs whose gates were decided: {e}");
                }
            }
        }
    };
    eprintln!("atomic serve: {signal}");
    stop(&mut signals, "atomic serve", Exit::Served).await
}
