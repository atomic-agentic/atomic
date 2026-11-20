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

use crate::engine::EngineArgs;
use crate::failure::Failure;
use crate::target;
use anyhow::Context;
use atomic_core::AtomConfig;
use atomic_core::artifact::LocalStore;
use atomic_core::atom::{self, Atom};
use atomic_core::cassette::{Cassette, Mode, Tally};
use atomic_core::codex::{CodexPaths, ModelEndpoint};
use atomic_core::collections;
use atomic_core::eval::{self, AtomSubject, Options, Report, Sample};
use atomic_core::lite::{LiteRuntime, Wire};
use atomic_core::runtime::{Artifacts, CodexRuntime, Runtimes};
use atomic_core::state::ProjectState;
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
    /// The reports of an earlier revision (a file `--report` wrote): each
    /// scorer's mean may not fall further below its mean there than the
    /// scorer's `max_drop`.
    #[arg(long, value_name = "PATH")]
    pub baseline: Option<PathBuf>,
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
    let files = collections::collect(&found)?;
    let mut registry = Registry::new();
    for schema in &files["schemas"] {
        registry.load_file(&schema.path)?;
    }
    let mut atoms = Vec::new();
    for source in &files["atoms"] {
        atoms.push(atom::load(&source.path, &mut registry)?);
    }
    let chosen = choose(&atoms, &args.atoms, &cwd)?;
    let baseline: Vec<Report> = match &args.baseline {
        Some(path) => read_reports(path)?,
        None => Vec::new(),
    };
    let source = match (args.live, args.no_cassettes) {
        (true, _) => Source::Recording,
        (_, true) => Source::Live,
        _ => Source::Recorded,
    };
    let needs_model = chosen.iter().any(|(a, _)| calls_model(a));
    // A recording answers only the request it was made for, endpoint and
    // model included, so replaying needs them configured as they were.
    let endpoint = args.engine.endpoint(needs_model)?;
    let store = LocalStore::open(state.dir().join("artifacts")).context("the artifact store")?;

    let mut reports = Vec::new();
    let mut tallies = Vec::new();
    for (atom, settings) in &chosen {
        let cases = eval::load_cases(atom, settings)?;
        let cassette = match (calls_model(atom), source) {
            (false, _) | (true, Source::Live) => None,
            (true, Source::Recorded) => Some(
                Cassette::start(
                    &settings.dir.join("cassettes"),
                    &endpoint.base_url,
                    Mode::Replay,
                )
                .await?,
            ),
            (true, Source::Recording) => Some(
                Cassette::start(
                    &settings.dir.join("cassettes"),
                    &endpoint.base_url,
                    Mode::Record,
                )
                .await?,
            ),
        };
        let runtimes = runtimes(&state, paths, &endpoint, cassette.as_ref(), &store)?;
        let subject = AtomSubject {
            atom,
            runtimes: &runtimes,
        };
        let options = Options {
            select: args.cases.clone(),
            sample: args.sample.map(|size| Sample {
                size,
                seed: args.seed,
            }),
            judge: None,
            baseline: baseline.iter().find(|r| r.atom == atom.name),
            revision: args.revision.clone(),
        };
        let report = eval::run(atom, cases, settings, &subject, options).await;
        tallies.push(cassette.as_ref().map(Cassette::tally));
        reports.push(report);
    }

    if let Some(path) = &args.junit {
        std::fs::write(path, eval::junit_all(&reports))
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
            "passed": reports.iter().all(|r| r.passed),
            "source": match source {
                Source::Recorded => "recorded",
                Source::Recording => "recording",
                Source::Live => "live",
            },
            "reports": reports,
            "cassettes": tallies.iter().zip(&reports).filter_map(|(t, r)| {
                t.as_ref().map(|t| json!({
                    "atom": r.atom, "replayed": t.replayed, "recorded": t.recorded, "missed": t.missed,
                }))
            }).collect::<Vec<_>>(),
            "scorers_skipped": skipped.iter().map(|(atom, names)| json!({
                "atom": atom, "scorers": names,
                "reason": "atomic eval has no judge to call, so scorers are not run",
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        print(&reports, &tallies, &skipped, source);
    }
    let failed: Vec<&str> = reports
        .iter()
        .filter(|r| !r.passed)
        .map(|r| r.atom.as_str())
        .collect();
    if failed.is_empty() {
        Ok(())
    } else if json {
        // The report above is the one JSON document on stdout; it says it
        // failed.
        std::process::exit(1);
    } else {
        Err(Failure::new(
            "eval.failed",
            format!("the evals of {} did not pass", failed.join(", ")),
            "read each failing case above, fix the atom or its cases, and run atomic eval again",
        )
        .into())
    }
}

/// The atoms `targets` name, with their eval settings: every atom with eval
/// cases when none is named.
fn choose<'a>(
    atoms: &'a [Atom],
    targets: &[String],
    cwd: &Path,
) -> anyhow::Result<Vec<(&'a Atom, eval::Settings)>> {
    // The project an atom belongs to, which its eval directory must be in.
    let project_of = |atom: &Atom| -> anyhow::Result<atomic_core::project::Project> {
        Ok(atomic_core::project::discover(
            atom.file.parent().unwrap_or(Path::new("/")),
        )?)
    };
    let mut chosen = Vec::new();
    if targets.is_empty() {
        for atom in atoms {
            let settings = eval::settings(atom, &project_of(atom)?)?;
            if !eval::load_cases(atom, &settings)?.is_empty() {
                chosen.push((atom, settings));
            }
        }
        if chosen.is_empty() {
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
        chosen.push((atom, eval::settings(atom, &project_of(atom)?)?));
    }
    Ok(chosen)
}

fn calls_model(atom: &Atom) -> bool {
    matches!(atom.kind.as_str(), "lite" | "codex")
}

/// Reports a run wrote with `--report`, or an `--json` output.
fn read_reports(path: &Path) -> anyhow::Result<Vec<Report>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let value: Value =
        serde_json::from_str(&text).with_context(|| format!("{} is not JSON", path.display()))?;
    let reports = match value {
        Value::Object(mut fields) => fields.remove("reports").unwrap_or(Value::Null),
        other => other,
    };
    serde_json::from_value(reports).with_context(|| {
        format!(
            "{} holds no eval reports: give a file `atomic eval --report` wrote",
            path.display()
        )
    })
}

/// The runtimes atoms run on, their model calls through `cassette` if there
/// is one.
fn runtimes(
    state: &ProjectState,
    paths: &CodexPaths,
    endpoint: &ModelEndpoint,
    cassette: Option<&Cassette>,
    store: &LocalStore,
) -> anyhow::Result<Runtimes> {
    let mut sent = endpoint.clone();
    if let Some(cassette) = cassette {
        sent.base_url = cassette.base_url(&endpoint.base_url);
    }
    let mut lite = LiteRuntime::new(sent.clone(), AtomConfig::default());
    // The wire the real endpoint takes, not the one the cassette's address
    // would suggest.
    lite.wire = Wire::for_base_url(&endpoint.base_url);
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
        },
        lite,
        exec: atomic_exec::Backend::default(),
        clock: None,
    })
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
        println!(
            "{} {}: {passed} of {} cases passed, pass rate {:.3}, threshold {:.3}",
            if report.passed { "PASS" } else { "FAIL" },
            report.atom,
            report.cases.len(),
            report.pass_rate,
            report.threshold
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
                "  cassettes: {} replayed, {} recorded, {} without a recording",
                t.replayed, t.recorded, t.missed
            );
        }
        for failure in &report.failures {
            println!("  {failure}");
        }
    }
}
