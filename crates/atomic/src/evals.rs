// SPDX-License-Identifier: Apache-2.0

//! `atomic eval`: run atoms' eval cases and fail when they do.
//!
//! A thin command over [`atomic_core::eval`]: it loads each atom's cases,
//! runs them through the atom, prints each atom's pass rate against its
//! threshold, and exits non-zero when any atom's report fails, which is
//! what fails CI.
//!
//! Model calls go through cassettes kept beside the cases
//! (`evals/<atom>/cassettes/`, see [`atomic_core::cassette`]). By default a
//! call is answered from its recording, with no model call, and the result
//! is a historical check of recorded answers, never a live pass. `--live`
//! calls the model and records each answer. `--no-cassettes` calls the
//! model and records nothing.
//!
//! A live call is metered in the project's usage ledger under the atom's
//! budget and the defaults, as a workflow step's is; a replay makes no call
//! and spends nothing.
//!
//! Until evals run in disposable fixture worktrees, `atomic eval` refuses an
//! atom that could write: a Codex atom with a `workspace-write` sandbox, and
//! an exec atom that declares any effect. It runs a Codex atom only live
//! (`--no-cassettes`), since cassettes do not yet stand in for every part of
//! a Codex thread's endpoint.

use crate::engine::EngineArgs;
use crate::failure::Failure;
use crate::target;
use anyhow::Context;
use atomic_app::metering::{BudgetPlan, RunBudget};
use atomic_app::pipeline::LAYOUT_VERSION;
use atomic_core::AtomConfig;
use atomic_core::artifact::LocalStore;
use atomic_core::atom::{self, Atom};
use atomic_core::budget::{Budget, Resolved};
use atomic_core::cassette::{Cassette, Mode, Tally};
use atomic_core::codex::{CodexPaths, ModelEndpoint};
use atomic_core::collections;
use atomic_core::eval::{self, AtomSubject, Options, Report, Sample};
use atomic_core::lite::{LimitField, LiteRuntime, Wire};
use atomic_core::runtime::{Artifacts, CodexRuntime, Runtimes};
use atomic_core::state::ProjectState;
use atomic_core::usage::{Call, Limit, Meter, MeterFuture, Metering, Outcome, Ticket};
use atomic_durable::{DurableRuntime, Durare};
use atomic_schema::registry::Registry;
use clap::Args;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// `atomic eval`'s arguments.
#[derive(Debug, Args)]
pub struct EvalArgs {
    /// Atoms to evaluate, by name or by file; every atom that has eval cases
    /// when none is given.
    pub atoms: Vec<String>,
    /// Run only this case, by its identity (`case.yaml`, `cases.jsonl:3`);
    /// a file's name runs all of its cases. Repeatable.
    #[arg(long = "case", value_name = "ID")]
    pub cases: Vec<String>,
    /// Run a seeded sample of this many cases of each atom.
    #[arg(long, value_name = "N")]
    pub sample: Option<usize>,
    /// The sample's seed: the same seed picks the same cases.
    #[arg(long, default_value_t = 0, requires = "sample")]
    pub seed: u64,
    /// Write the results as JUnit XML to this file.
    #[arg(long, value_name = "PATH")]
    pub junit: Option<PathBuf>,
    /// Write the reports as JSON to this file, to compare a later run with.
    #[arg(long, value_name = "PATH")]
    pub report: Option<PathBuf>,
    /// The reports of an earlier revision (a file `--report` wrote), for each
    /// scorer's `max_drop`. Refused for now: `atomic eval` has no judge, so
    /// it has no scorer mean to compare.
    #[arg(long, value_name = "PATH")]
    pub baseline: Option<PathBuf>,
    /// Report an atom's scorers as skipped instead of failing it: without a
    /// judge, `atomic eval` cannot run them.
    #[arg(long)]
    pub allow_unscored: bool,
    /// Report an atom atomic eval will not run yet as skipped, instead of
    /// failing the command. Either way the other atoms run.
    #[arg(long)]
    pub skip_refused: bool,
    /// The revision this run evaluates, recorded in its reports.
    #[arg(long)]
    pub revision: Option<String>,
    /// Call the model and record each answer in the cassettes, replacing
    /// any recording of the same request.
    #[arg(long)]
    pub live: bool,
    /// Call the model and record nothing.
    #[arg(long, conflicts_with = "live")]
    pub no_cassettes: bool,
    #[command(flatten)]
    pub engine: EngineArgs,
}

/// How model calls are answered.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    /// From recordings: a historical check.
    Recorded,
    /// From the model, recording each answer.
    Recording,
    /// From the model.
    Live,
}

impl Source {
    fn describe(self) -> &'static str {
        match self {
            Self::Recorded => {
                "replayed from cassettes: a historical check of recorded answers, not a live pass"
            }
            Self::Recording => "live, each answer recorded in the cassettes",
            Self::Live => "live, nothing recorded",
        }
    }
}

/// An eval run's metering: the ledger-backed budget each live call of one
/// atom is admitted against.
struct Metered {
    budget: RunBudget,
    caps: Resolved,
    run: String,
}

/// The atom on its runtimes, each case's live calls metered as one atom
/// step of the eval's run.
struct Subject<'a> {
    atom: &'a Atom,
    runtimes: Runtimes,
    metered: Option<Metered>,
    next: std::sync::atomic::AtomicI32,
}

impl eval::Subject for Subject<'_> {
    fn run<'a>(&'a self, case: &'a eval::Case) -> eval::BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            let mut runtimes = self.runtimes.clone();
            if let Some(m) = &self.metered {
                let step = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let meter = m.budget.meter(
                    &atomic_durable::StepInfo {
                        workflow_id: m.run.clone(),
                        step_id: step,
                    },
                    &m.caps,
                );
                runtimes.codex.meter = meter.clone();
                runtimes.lite.meter = meter;
            }
            AtomSubject {
                atom: self.atom,
                runtimes: &runtimes,
            }
            .run(case)
            .await
        })
    }
}

/// A replay's meter: it spends nothing and records nothing, and holds each
/// call to the output limit a live call is held to, so a replayed request is
/// the one that was recorded.
struct Replayed {
    output_tokens: u64,
}

impl Meter for Replayed {
    fn admit(&self, call: Call) -> MeterFuture<'_, Ticket> {
        Box::pin(async move {
            Ok(Ticket {
                request: format!("replay:{}", call.index),
                attempt: 1,
                model: call.model,
                admitted: std::time::Instant::now(),
                limit: Limit {
                    output_tokens: Some(self.output_tokens),
                    time: None,
                },
                bounded_by: None,
            })
        })
    }

    fn settle(&self, _: Ticket, _: Outcome) -> MeterFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

/// Why `atomic eval` will not run `atom` from `source` yet, and how to
/// change that, if it will not.
fn refusal(atom: &Atom, source: Source) -> Option<(String, &'static str)> {
    let writes = atom.frontmatter.get("sandbox").and_then(Value::as_str) == Some("workspace-write");
    if atom.kind == "codex" && writes {
        return Some((
            "it may write files, and atomic eval would run it in the project's own checkout".into(),
            "evaluate it once fixture worktrees are available, or make it read-only",
        ));
    }
    if atom.kind == "exec" {
        // Its command runs in the project, for every case and every replay:
        // only one declared to read and nothing more is run.
        match atom.external() {
            Some(atomic_core::atom::External::ReadOnly) => {}
            Some(atomic_core::atom::External::Idempotent) => {
                return Some((
                    "it writes to an outside system, and atomic eval never runs such an atom"
                        .into(),
                    "cover it with code tests instead of eval cases",
                ));
            }
            None => {
                return Some((
                    "it declares no effects, so atomic eval cannot tell that its command only \
                     reads"
                        .into(),
                    "if the command only reads, declare `effects: { external: 'read-only' }`",
                ));
            }
        }
    }
    if atom.kind == "codex" && source != Source::Live {
        return Some((
            "it is a Codex atom, and cassettes do not yet record a Codex thread's calls \
             faithfully"
                .into(),
            "evaluate it live with --no-cassettes",
        ));
    }
    None
}

/// The model `atom` calls on `endpoint`.
fn model_of(atom: &Atom, endpoint: &ModelEndpoint) -> String {
    atom.frontmatter
        .get("model")
        .and_then(Value::as_str)
        .map_or_else(|| endpoint.model.clone(), str::to_owned)
}

/// `atomic eval`.
pub async fn run(
    args: EvalArgs,
    project: Option<&Path>,
    paths: &CodexPaths,
    json: bool,
) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let first = args.atoms.first().map(String::as_str);
    let found = target::project_for(project, first, &cwd)?;
    let state = ProjectState::resolve(found.root())?;
    if args.baseline.is_some() {
        return Err(Failure::new(
            "eval.baseline",
            "atomic eval has no judge to run scorers with, so there is no scorer mean to \
             compare with a baseline",
            "leave out --baseline; compare pass rates with the reports --report writes",
        )
        .into());
    }
    let files = collections::collect(&found)?;
    let mut registry = Registry::new();
    for schema in &files["schemas"] {
        registry.load_file(&schema.path)?;
    }
    let mut atoms = Vec::new();
    for source in &files["atoms"] {
        atoms.push(atom::load(&source.path, &mut registry)?);
    }
    let source = match (args.live, args.no_cassettes) {
        (true, _) => Source::Recording,
        (_, true) => Source::Live,
        _ => Source::Recorded,
    };
    let mut chosen = choose(&atoms, &args.atoms, &args.cases, &cwd)?;
    // What cannot run yet is set aside and named, and the rest runs.
    let mut refused = Vec::new();
    chosen
        .atoms
        .retain(|(atom, _, _)| match refusal(atom, source) {
            Some((why, hint)) => {
                refused.push(json!({"atom": atom.name, "reason": why, "hint": hint}));
                false
            }
            None => true,
        });
    // Something must run: skipping every atom chosen is not a pass. (Without
    // --skip-refused the refusals are listed below, and fail the eval.)
    if args.skip_refused && chosen.atoms.is_empty() && !refused.is_empty() {
        let names: Vec<&str> = refused.iter().filter_map(|r| r["atom"].as_str()).collect();
        return Err(Failure::new(
            "eval.refused",
            format!(
                "atomic eval can run none of the atoms chosen ({}): each is refused",
                names.join(", ")
            ),
            "run atomic eval without --skip-refused to see why each is refused",
        )
        .into());
    }
    // One eval at a time, and none beside a command that runs workflows.
    state.create()?;
    let _lock = state.lock_execution("atomic eval")?;
    let needs_model = chosen.atoms.iter().any(|(a, _, _)| calls_model(a));
    // The settings `atomic run` resolves: files, variables and flags.
    let resolved = crate::engine::settings(&state, &args.engine)?;
    // A case waits for the execution backend no longer than a run would.
    atomic_core::runtime::limit_backend_wait(
        resolved
            .settings
            .backend_wait()
            .map(std::time::Duration::from_secs),
    );
    // A recording answers only the request it was made for, endpoint and
    // model included, so replaying needs them configured as they were; it
    // needs no key, so none is read. A live eval calls the endpoint as a
    // run would.
    let endpoint = match (source, needs_model) {
        (Source::Recorded, true) => crate::engine::identity(&resolved)?,
        _ => crate::engine::endpoint(&resolved, needs_model)?,
    };
    if source != Source::Recorded
        && needs_model
        && let Some(line) = resolved.announcement()
    {
        atomic_core::log_line!("{line}");
    }
    // The project's artifact store, under the same lock as runs'.
    let store = LocalStore::for_project(&state).context("the artifact store")?;
    // A live call is metered: without the ledger, none is made.
    let ledger = if source != Source::Recorded && needs_model {
        let database = crate::engine::store_of(&state, &resolved.settings);
        let executor = atomic_durable::executor_identity(resolved.settings.executor_id.as_deref())?;
        let rt = Durare::open(&database, executor.as_deref(), LAYOUT_VERSION)
            .await
            .map_err(|e| {
                Failure::new(
                    "eval.ledger",
                    format!(
                        "a live eval is metered in the project's usage ledger, and it could not \
                         be opened at {}: {e}",
                        crate::engine::describe(&database)
                    ),
                    "fix the store, or replay from cassettes without --live",
                )
            })?;
        Some(rt.ledger())
    } else {
        None
    };
    // The budget caps and prices a run is metered under.
    let budgets = resolved.settings.budget.clone();
    // Every live atom's budget is resolved before any call, so one that
    // cannot be stops the eval before it spends anything.
    let mut plans = std::collections::BTreeMap::new();
    if ledger.is_some() {
        for (atom, _, _) in chosen.atoms.iter().filter(|(a, _, _)| calls_model(a)) {
            let model = model_of(atom, &endpoint);
            let plan = BudgetPlan::resolve(
                &budgets,
                &Budget::default(),
                &[(atom.name.as_str(), &atom.budget, model.as_str())],
            )
            .map_err(|(_, e)| {
                Failure::new(
                    "eval.budget",
                    format!("atom {}: budget: {e}", atom.name),
                    "fix the atom's budget block",
                )
            })?;
            plans.insert(atom.name.clone(), plan);
        }
    }
    let exec = crate::engine::exec_backend(&state, paths, crate::engine::runner_of(&resolved)?)?;
    let unique = format!(
        "{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis()),
        std::process::id()
    );

    let mut reports = Vec::new();
    let mut tallies = Vec::new();
    let mut spent = Vec::new();
    for (atom, settings, select) in &chosen.atoms {
        let cases = eval::load_cases(atom, settings)?;
        let cassette = match (calls_model(atom), source) {
            (false, _) | (true, Source::Live) => None,
            (true, recorded) => Some(
                Cassette::start(
                    &settings.dir.join("cassettes"),
                    &settings.project.dir,
                    &endpoint.base_url,
                    if recorded == Source::Recorded {
                        Mode::Replay
                    } else {
                        Mode::Record
                    },
                )
                .await?,
            ),
        };
        let mut runtimes = runtimes(&state, paths, &endpoint, cassette.as_ref(), &store, &exec)?;
        let metered = match (&ledger, plans.get(&atom.name)) {
            (Some(ledger), Some(plan)) => {
                let model = model_of(atom, &endpoint);
                // Calls go to the model the budget was resolved for.
                runtimes.codex.defaults.model = Some(model.clone());
                runtimes.lite.defaults.model = Some(model);
                Some(Metered {
                    budget: RunBudget::pinned(ledger.clone(), plan),
                    caps: plan.atoms[&atom.name].clone(),
                    run: format!("eval:{}:{unique}", atom.name),
                })
            }
            _ => {
                if source == Source::Recorded && calls_model(atom) {
                    let model = model_of(atom, &endpoint);
                    let meter = Metering::new(std::sync::Arc::new(Replayed {
                        output_tokens: budgets.reservation(&model).output_tokens,
                    }));
                    runtimes.codex.defaults.model = Some(model.clone());
                    runtimes.lite.defaults.model = Some(model);
                    runtimes.codex.meter = meter.clone();
                    runtimes.lite.meter = meter;
                }
                None
            }
        };
        let run_id = metered.as_ref().map(|m| m.run.clone());
        let subject = Subject {
            atom,
            runtimes,
            metered,
            next: std::sync::atomic::AtomicI32::new(1),
        };
        let options = Options {
            select: select.clone(),
            sample: args.sample.map(|size| Sample {
                size,
                seed: args.seed,
            }),
            judge: None,
            baseline: None,
            revision: args.revision.clone(),
        };
        let mut report = eval::run(atom, cases, settings, &subject, options).await;
        let tally = cassette.as_ref().map(Cassette::tally);
        if let Some(t) = &tally {
            // A call the cassette could not answer is a failure whatever the
            // pass rate: the eval did not check what it was asked to.
            if t.missed + t.corrupt > 0 {
                report.failures.push(format!(
                    "{} model call(s) had no usable recording ({} missing, {} corrupt): record \
                     them with --live",
                    t.missed + t.corrupt,
                    t.missed,
                    t.corrupt
                ));
                report.passed = false;
            }
        }
        if !report.not_scored.is_empty() && !args.allow_unscored {
            report.failures.push(format!(
                "scorers {} are declared, and atomic eval has no judge to run them; pass \
                 --allow-unscored to report them as skipped",
                report.not_scored.join(", ")
            ));
            report.passed = false;
        }
        if let (Some(ledger), Some(run)) = (&ledger, run_id) {
            let attempts = ledger.attempts(&run).await.unwrap_or_default();
            spent.push(json!({
                "atom": atom.name,
                "run": run,
                "calls": attempts.len(),
                "tokens": attempts.iter().map(|a| a.charged.tokens).sum::<i64>(),
            }));
        }
        tallies.push(tally);
        reports.push(report);
    }

    let unrun: Vec<eval::Unrun> = refused
        .iter()
        .map(|r| eval::Unrun {
            atom: r["atom"].as_str().unwrap_or_default().to_owned(),
            reason: format!(
                "not run: {}; {}",
                r["reason"].as_str().unwrap_or_default(),
                r["hint"].as_str().unwrap_or_default()
            ),
            fails: !args.skip_refused,
        })
        .collect();
    if let Some(path) = &args.junit {
        std::fs::write(path, eval::junit_with(&reports, &unrun))
            .with_context(|| format!("write {}", path.display()))?;
    }
    if let Some(path) = &args.report {
        std::fs::write(path, serde_json::to_string_pretty(&reports)? + "\n")
            .with_context(|| format!("write {}", path.display()))?;
    }
    let skipped: Vec<(&str, Vec<String>)> = reports
        .iter()
        .map(|r| (r.atom.as_str(), r.not_scored.clone()))
        .filter(|(_, s)| !s.is_empty())
        .collect();
    if json {
        let out = json!({
            "passed": reports.iter().all(|r| r.passed) && (refused.is_empty() || args.skip_refused),
            "source": match source {
                Source::Recorded => "recorded",
                Source::Recording => "recording",
                Source::Live => "live",
            },
            "reports": reports,
            "refused": refused,
            "no_cases": chosen.empty,
            "cassettes": tallies.iter().zip(&reports).filter_map(|(t, r)| {
                t.as_ref().map(|t| json!({
                    "atom": r.atom, "replayed": t.replayed, "recorded": t.recorded,
                    "missed": t.missed, "corrupt": t.corrupt, "keyed": t.keyed,
                }))
            }).collect::<Vec<_>>(),
            "usage": spent,
            "scorers_skipped": skipped.iter().map(|(atom, names)| json!({
                "atom": atom, "scorers": names,
                "reason": "atomic eval has no judge to call, so scorers are not run",
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        print(&reports, &tallies, &skipped, source);
        for r in &refused {
            println!(
                "{} {}: not run: {}; {}",
                if args.skip_refused {
                    "SKIPPED"
                } else {
                    "REFUSED"
                },
                r["atom"].as_str().unwrap_or_default(),
                r["reason"].as_str().unwrap_or_default(),
                r["hint"].as_str().unwrap_or_default()
            );
        }
        if !chosen.empty.is_empty() {
            println!("no eval cases: {}", chosen.empty.join(", "));
        }
        for s in &spent {
            println!(
                "metered {}: {} model call(s), {} token(s), in the usage ledger as {} (a ledger \
                 scope, not a run `atomic runs` shows)",
                s["atom"].as_str().unwrap_or_default(),
                s["calls"],
                s["tokens"],
                s["run"].as_str().unwrap_or_default()
            );
        }
    }
    let mut failed: Vec<&str> = reports
        .iter()
        .filter(|r| !r.passed)
        .map(|r| r.atom.as_str())
        .collect();
    if !args.skip_refused {
        failed.extend(refused.iter().filter_map(|r| r["atom"].as_str()));
    }
    if failed.is_empty() {
        Ok(())
    } else if json {
        // The report above is the one JSON document on stdout; it says it
        // failed.
        atomic_exec::process::end_all(&mut |what| atomic_core::log_line!("atomic: {what}"));
        std::process::exit(1);
    } else {
        Err(Failure::new(
            "eval.failed",
            format!(
                "the evals of {} did not pass or did not run",
                failed.join(", ")
            ),
            "read each failing case above, fix the atom or its cases, and run atomic eval again",
        )
        .into())
    }
}

/// The atoms an eval runs, each with its eval settings and the case
/// selectors that apply to it, and the atoms passed over for having no cases.
struct Chosen<'a> {
    atoms: Vec<(&'a Atom, eval::Settings, Vec<String>)>,
    empty: Vec<String>,
}

/// The atoms `targets` name, or every atom with eval cases when none is
/// named, with the `selectors` that name their cases. A selector that names
/// no case of any atom chosen is an error. With no atom named, only the
/// atoms some selector names run; a named atom gets every selector, so one
/// that names none of its cases fails its report.
fn choose<'a>(
    atoms: &'a [Atom],
    targets: &[String],
    selectors: &[String],
    cwd: &Path,
) -> anyhow::Result<Chosen<'a>> {
    // The project an atom belongs to, which its eval directory must be in.
    let project_of = |atom: &Atom| -> anyhow::Result<atomic_core::project::Project> {
        Ok(atomic_core::project::discover(
            atom.file.parent().unwrap_or(Path::new("/")),
        )?)
    };
    // The selectors in `selectors` that name one of `atom`'s cases.
    let naming = |cases: &[eval::Case]| -> Vec<String> {
        selectors
            .iter()
            .filter(|s| {
                eval::unmatched(
                    cases,
                    &Options {
                        select: vec![(*s).clone()],
                        ..Options::default()
                    },
                )
                .is_empty()
            })
            .cloned()
            .collect()
    };
    let mut chosen = Chosen {
        atoms: Vec::new(),
        empty: Vec::new(),
    };
    let mut named = std::collections::BTreeSet::new();
    if targets.is_empty() {
        for atom in atoms {
            let settings = eval::settings(atom, &project_of(atom)?)?;
            let cases = match eval::load_cases(atom, &settings) {
                Ok(cases) => cases,
                // An atom that writes outside has cases it may not run: it is
                // listed with the atoms eval refuses, not an end to the eval.
                Err(_) if atom.external() == Some(atomic_core::atom::External::Idempotent) => {
                    chosen.atoms.push((atom, settings, Vec::new()));
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            if cases.is_empty() {
                chosen.empty.push(atom.name.clone());
                continue;
            }
            let select = naming(&cases);
            if !selectors.is_empty() && select.is_empty() {
                continue;
            }
            named.extend(select.iter().cloned());
            chosen.atoms.push((atom, settings, select));
        }
        if let Some(stray) = selectors.iter().find(|s| !named.contains(*s)) {
            return Err(Failure::new(
                "eval.case",
                format!("`{stray}` names no eval case of any atom in this project"),
                "name a case by its file (`case.yaml`) or its file and line (`cases.jsonl:3`)",
            )
            .into());
        }
        if chosen.atoms.is_empty() {
            return Err(Failure::new(
                "eval.none",
                "no atom in this project has eval cases",
                "add cases in evals/<atom>/, one YAML file per case or one JSONL line per case",
            )
            .into());
        }
        return Ok(chosen);
    }
    for t in targets {
        let atom = if target::is_file_target(t) {
            let file = std::fs::canonicalize(cwd.join(t)).with_context(|| format!("find {t}"))?;
            atoms
                .iter()
                .find(|a| std::fs::canonicalize(&a.file).is_ok_and(|f| f == file))
        } else {
            atoms.iter().find(|a| a.name == *t)
        };
        let Some(atom) = atom else {
            let mut names: Vec<&str> = atoms.iter().map(|a| a.name.as_str()).collect();
            names.sort_unstable();
            return Err(Failure::new(
                "target.unknown",
                format!("this project has no atom `{t}`"),
                format!(
                    "name one of {}, or give the path of an .atom.astro file",
                    names.join(", ")
                ),
            )
            .into());
        };
        chosen.atoms.push((
            atom,
            eval::settings(atom, &project_of(atom)?)?,
            selectors.to_vec(),
        ));
    }
    Ok(chosen)
}

fn calls_model(atom: &Atom) -> bool {
    matches!(atom.kind.as_str(), "lite" | "codex")
}

/// The runtimes atoms run on, their model calls through `cassette` if there
/// is one.
fn runtimes(
    state: &ProjectState,
    paths: &CodexPaths,
    endpoint: &ModelEndpoint,
    cassette: Option<&Cassette>,
    store: &LocalStore,
    exec: &atomic_exec::Backend,
) -> anyhow::Result<Runtimes> {
    let mut sent = endpoint.clone();
    if let Some(cassette) = cassette {
        sent.base_url = cassette.base_url(&endpoint.base_url);
    }
    let lite = lite_runtime(endpoint, sent.clone());
    Ok(Runtimes {
        codex: CodexRuntime {
            paths: paths.clone(),
            codex_home: state.codex_home(),
            cwd: state.root().to_owned(),
            endpoint: sent,
            defaults: AtomConfig::default(),
            artifacts: Artifacts::new(store.clone()),
            meter: atomic_core::usage::Metering::none(),
            guard: atomic_core::codex::StateGuard::for_project(state)?,
            caches: None,
        },
        lite,
        backend: exec.clone(),
        clock: None,
    })
}

/// The lite runtime for calls sent to `sent` in place of `endpoint`: with
/// the wire and output-limit field the real endpoint takes, not the ones the
/// cassette's loopback address would suggest.
fn lite_runtime(endpoint: &ModelEndpoint, sent: ModelEndpoint) -> LiteRuntime {
    let mut lite = LiteRuntime::new(sent, AtomConfig::default());
    lite.wire = Wire::for_base_url(&endpoint.base_url);
    lite.limit_field = Some(LimitField::for_endpoint(lite.wire, &endpoint.base_url));
    lite
}

/// A pass rate and its threshold to three places, or in full when three
/// places would show them equal while they are not.
fn shares(rate: f64, threshold: f64) -> (String, String) {
    let (r, t) = (format!("{rate:.3}"), format!("{threshold:.3}"));
    if r == t && rate != threshold {
        (rate.to_string(), threshold.to_string())
    } else {
        (r, t)
    }
}

fn print(
    reports: &[Report],
    tallies: &[Option<Tally>],
    skipped: &[(&str, Vec<String>)],
    source: Source,
) {
    println!("model calls: {}", source.describe());
    for (report, tally) in reports.iter().zip(tallies) {
        let passed = report.cases.iter().filter(|c| c.passed).count();
        let (rate, threshold) = shares(report.pass_rate, report.threshold);
        println!(
            "{} {}: {passed} of {} cases passed, pass rate {rate}, threshold {threshold}",
            if report.passed { "PASS" } else { "FAIL" },
            report.atom,
            report.cases.len(),
        );
        for case in report.cases.iter().filter(|c| !c.passed) {
            println!("  failed {}: {}", case.id, case.failures.join("; "));
        }
        for scorer in &report.scorers {
            println!(
                "  scorer {}: mean {} (minimum {:.3}){}",
                scorer.name,
                scorer.mean.map_or("none".to_owned(), |m| format!("{m:.3}")),
                scorer.min_mean,
                scorer
                    .reason
                    .as_deref()
                    .map(|r| format!(": {r}"))
                    .unwrap_or_default()
            );
        }
        if let Some((_, names)) = skipped.iter().find(|(a, _)| *a == report.atom) {
            println!(
                "  scorers not run ({}): atomic eval has no judge to call, so only the pass rate \
                 decides",
                names.join(", ")
            );
        }
        if let Some(t) = tally {
            println!(
                "  cassettes: {} replayed, {} recorded, {} without a recording{}",
                t.replayed,
                t.recorded,
                t.missed,
                if t.corrupt > 0 {
                    format!(", {} corrupt", t.corrupt)
                } else {
                    String::new()
                }
            );
        }
        for failure in &report.failures {
            println!("  {failure}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lite_call_through_a_cassette_is_shaped_for_the_real_endpoint() {
        let endpoint = ModelEndpoint {
            base_url: "https://api.openai.com/v1".into(),
            model: "m".into(),
            api_key_env: None,
        };
        let mut sent = endpoint.clone();
        sent.base_url = "http://127.0.0.1:4000/v1".into();
        let lite = lite_runtime(&endpoint, sent);
        assert_eq!(lite.endpoint.base_url, "http://127.0.0.1:4000/v1");
        assert_eq!(lite.wire, Wire::Responses);
        assert_eq!(lite.limit_field, Some(LimitField::MaxOutputTokens));
    }

    #[test]
    fn a_threshold_close_to_the_pass_rate_is_shown_in_full() {
        assert_eq!(shares(0.5, 0.5), ("0.500".into(), "0.500".into()));
        assert_eq!(shares(0.6667, 2.0 / 3.0).0, "0.6667");
        assert_eq!(shares(0.25, 0.5), ("0.250".into(), "0.500".into()));
    }
}
