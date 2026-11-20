// SPDX-License-Identifier: Apache-2.0

//! Eval cases and the eval runner.
//!
//! An atom's cases live in `evals/<atom>/` beside the manifest, or in the
//! directory its `evals` field names, relative to the atom file. A `.yaml`
//! file holds one case and is identified by its path in that directory; a
//! `.jsonl` file holds one case per line and each is identified by its file
//! and line, `cases.jsonl:3`. Either way a case is the same two keys:
//!
//! - `input`: a JSON value that fits the atom's input contract;
//! - `expect`: a nonempty list of CEL expressions over `input` and `output`,
//!   each of which must evaluate to `true`.
//!
//! A case may also carry `case`, metadata for the atom's scorers, checked
//! against the atom's `case` schema, and `prepared`, the values an atom's
//! `prepare` would return. Any other key fails the load.
//!
//! [`run`] runs every case through a [`Subject`] (the atom on a model, or a
//! recording of it), checks each case's assertions against its output, and
//! reports a pass rate against the atom's threshold. With a [`Judge`], it
//! also scores each passing output with the atom's scorers: each scorer's
//! mean must reach its `min_mean`, and, against a baseline report from an
//! earlier revision, must not drop by more than its `max_drop`. A report
//! that does not pass is what makes CI fail.
//!
//! The threshold is the atom's `evals.pass_rate`, between zero and one; it
//! defaults to one, every case.

use crate::atom::Atom;
use atomic_schema::expr::{self, Scope};
use atomic_schema::registry::Registry;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

/// One eval case.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Case {
    /// Its identity: the case file's path in the eval directory, with
    /// `:<line>` for a case in a JSONL file.
    pub id: String,
    /// The atom's input.
    pub input: Value,
    /// CEL assertions over `input` and `output`.
    pub expect: Vec<String>,
    /// Metadata for the atom's scorers, `null` when the case gives none.
    pub case: Value,
    /// The values the atom's `prepare` would return, if the case gives them.
    pub prepared: Option<Value>,
}

/// Where an atom's cases are and the pass rate they must reach.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// The eval directory.
    pub dir: PathBuf,
    /// The pass rate CI requires, between zero and one.
    pub pass_rate: f64,
}

/// Why an atom's cases could not be loaded.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{}{}: {message}", file.display(), line.map(|l| format!(":{l}")).unwrap_or_default())]
pub struct CaseError {
    /// The file.
    pub file: PathBuf,
    /// The line, for a JSONL case or a YAML node.
    pub line: Option<usize>,
    /// What is wrong.
    pub message: String,
}

/// The keys a case may have.
const KEYS: &[&str] = &["input", "expect", "case", "prepared"];

/// `atom`'s eval settings, in a project whose manifest is in `project_dir`.
/// Its `evals` field is either the eval directory, relative to the atom
/// file, or a mapping of `dir` and `pass_rate`.
pub fn settings(atom: &Atom, project_dir: &Path) -> Result<Settings, CaseError> {
    let fail = |message: String| CaseError {
        file: atom.file.clone(),
        line: None,
        message,
    };
    let beside = |dir: &str| atom.file.parent().unwrap_or(Path::new("")).join(dir);
    let default = project_dir.join("evals").join(&atom.name);
    let (dir, pass_rate) = match atom.frontmatter.get("evals") {
        None => (default, 1.0),
        Some(Value::String(dir)) => (beside(dir), 1.0),
        Some(Value::Object(fields)) => {
            if let Some(other) = fields
                .keys()
                .find(|k| !["dir", "pass_rate"].contains(&k.as_str()))
            {
                return Err(fail(format!(
                    "evals: unknown field `{other}`: evals has dir and pass_rate"
                )));
            }
            let dir = match fields.get("dir") {
                None => default,
                Some(Value::String(dir)) => beside(dir),
                Some(_) => return Err(fail("evals.dir must be a path".into())),
            };
            let pass_rate = match fields.get("pass_rate") {
                None => 1.0,
                Some(v) => v
                    .as_f64()
                    .filter(|r| (0.0..=1.0).contains(r))
                    .ok_or_else(|| fail("evals.pass_rate must be between zero and one".into()))?,
            };
            (dir, pass_rate)
        }
        Some(_) => {
            return Err(fail(
                "evals must be a directory or a mapping of dir and pass_rate".into(),
            ));
        }
    };
    Ok(Settings { dir, pass_rate })
}

/// Load `atom`'s cases from `settings.dir`, in order of their identities.
/// A missing directory holds no cases. Every case is checked now: its keys,
/// its input against the atom's input contract, its `case` against the
/// atom's case schema, and each assertion parsed and checked against the
/// input and output contracts.
pub fn load_cases(atom: &Atom, settings: &Settings) -> Result<Vec<Case>, CaseError> {
    let mut files = Vec::new();
    collect(&settings.dir, &mut files).map_err(|e| CaseError {
        file: settings.dir.clone(),
        line: None,
        message: format!("read the eval directory: {e}"),
    })?;
    files.sort();
    let contracts = Contracts::of(atom).map_err(|message| CaseError {
        file: atom.file.clone(),
        line: None,
        message,
    })?;
    let mut cases = Vec::new();
    for file in files {
        let relative = file
            .strip_prefix(&settings.dir)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        let text = std::fs::read_to_string(&file).map_err(|e| CaseError {
            file: file.clone(),
            line: None,
            message: format!("read the case: {e}"),
        })?;
        if relative.ends_with(".jsonl") {
            for (n, line) in text.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let at = |message: String| CaseError {
                    file: file.clone(),
                    line: Some(n + 1),
                    message,
                };
                let value: Value = serde_json::from_str(line)
                    .map_err(|e| at(format!("a case is one JSON object per line: {e}")))?;
                cases.push(
                    case_from(&value, format!("{relative}:{}", n + 1), &contracts).map_err(at)?,
                );
            }
        } else {
            let doc = atomic_schema::yaml::load(&file, &text).map_err(|e| CaseError {
                file: file.clone(),
                line: Some(e.at.line),
                message: e.message,
            })?;
            let at = |message: String| CaseError {
                file: file.clone(),
                line: doc.position("").map(|p| p.line),
                message,
            };
            cases.push(case_from(&doc.value, relative.clone(), &contracts).map_err(at)?);
        }
    }
    if !cases.is_empty() && atom.external() == Some(crate::atom::External::Idempotent) {
        return Err(CaseError {
            file: settings.dir.clone(),
            line: None,
            message: format!(
                "`{}` writes to an outside system, and atomic never runs such an atom in an eval: \
                 cover it with code tests instead of eval cases",
                atom.name
            ),
        });
    }
    Ok(cases)
}

/// Every `.yaml` and `.jsonl` file under `dir`.
fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, out)?;
        } else if path
            .extension()
            .is_some_and(|e| e == "yaml" || e == "jsonl")
        {
            out.push(path);
        }
    }
    Ok(())
}

/// The schemas a case is checked against.
struct Contracts {
    input: Value,
    output: Value,
    case: Value,
}

impl Contracts {
    fn of(atom: &Atom) -> Result<Self, String> {
        let contract = |key: &str| {
            atom.program
                .contracts
                .get(key)
                .cloned()
                .ok_or_else(|| format!("the atom has no {key} contract"))
        };
        Ok(Self {
            input: contract("Input")?,
            output: contract("Output")?,
            case: atom.case.clone(),
        })
    }
}

fn fits(schema: &Value, value: &Value, what: &str) -> Result<(), String> {
    let validator =
        jsonschema::validator_for(schema).map_err(|e| format!("{what}: the schema: {e}"))?;
    let errors: Vec<String> = validator
        .iter_errors(value)
        .map(|e| {
            let at = e.instance_path().to_string();
            format!("{}: {e}", if at.is_empty() { "/" } else { &at })
        })
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!("{what} does not fit: {}", errors.join("; ")))
    }
}

fn case_from(value: &Value, id: String, contracts: &Contracts) -> Result<Case, String> {
    let Value::Object(fields) = value else {
        return Err("a case is a mapping of input and expect".into());
    };
    if let Some(other) = fields.keys().find(|k| !KEYS.contains(&k.as_str())) {
        return Err(format!(
            "unknown case field `{other}`: a case has input and expect (and, for scorers, case)"
        ));
    }
    let input = fields
        .get("input")
        .cloned()
        .ok_or("a case needs `input`, the atom's input")?;
    let expect = match fields.get("expect") {
        None => return Err("a case needs `expect`, a nonempty list of CEL assertions".into()),
        Some(Value::Array(items)) if !items.is_empty() => items
            .iter()
            .map(|e| e.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or("each `expect` entry is a CEL expression, as a string")?,
        Some(_) => return Err("`expect` must be a nonempty list of CEL assertions".into()),
    };
    fits(&contracts.input, &input, "the input")?;
    let case = fields.get("case").cloned().unwrap_or(Value::Null);
    if fields.contains_key("case") {
        fits(&contracts.case, &case, "the case metadata")?;
    }
    let scope = Scope::new()
        .with("input", &contracts.input)
        .with("output", &contracts.output);
    for (n, source) in expect.iter().enumerate() {
        expr::parse(source)
            .and_then(|e| e.check(&scope))
            .map_err(|e| format!("expect[{n}] `{source}`: {e}"))?;
    }
    Ok(Case {
        id,
        input,
        expect,
        case,
        prepared: fields.get("prepared").cloned(),
    })
}

// ---------------------------------------------------------------------------
// The runner.

/// A boxed future, for the object-safe [`Subject`] and [`Judge`].
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What produces an atom's output for a case: the atom on a model, or a
/// recording of it. An error, such as an output that breaks the atom's
/// contract or its `validate` rules, fails the case.
pub trait Subject: Send + Sync {
    /// The atom's output for `case`.
    fn run<'a>(&'a self, case: &'a Case) -> BoxFuture<'a, Result<Value, String>>;
}

/// What answers the atom's judges: each scorer's judge input in, its
/// output, checked against the judge's output schema, back.
pub trait Judge: Send + Sync {
    /// Judge `input` with the judge named `judge`.
    fn judge<'a>(&'a self, judge: &'a str, input: Value) -> BoxFuture<'a, Result<Value, String>>;
}

/// How one case ended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaseResult {
    /// The case.
    pub id: String,
    /// Whether every assertion held.
    pub passed: bool,
    /// Why not: the run's error, or each assertion that did not hold.
    pub failures: Vec<String>,
    /// The output, when the atom gave one.
    pub output: Option<Value>,
}

/// How one scorer did.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScorerResult {
    /// The scorer.
    pub name: String,
    /// Its mean over the cases it scored, if it scored any.
    pub mean: Option<f64>,
    /// The mean it must reach.
    pub min_mean: f64,
    /// The baseline's mean, when there is a baseline that scored it.
    pub baseline: Option<f64>,
    /// How far below the baseline its mean may fall, if it declares it.
    pub max_drop: Option<f64>,
    /// Whether it passed.
    pub passed: bool,
    /// Why not.
    pub reason: Option<String>,
}

/// An eval run's result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// The atom.
    pub atom: String,
    /// The revision it ran at, if the caller named one.
    pub revision: Option<String>,
    /// Each case, in order.
    pub cases: Vec<CaseResult>,
    /// The share of cases that passed.
    pub pass_rate: f64,
    /// The share required.
    pub threshold: f64,
    /// Each scorer, when a judge was given.
    pub scorers: Vec<ScorerResult>,
    /// Whether the run passes: the pass rate reaches the threshold and every
    /// scorer passes.
    pub passed: bool,
    /// Why it does not, one line each.
    pub failures: Vec<String>,
}

/// A seeded sample of the cases: `size` of them, chosen the same way for
/// the same seed and case set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    /// How many cases.
    pub size: usize,
    /// The seed.
    pub seed: u64,
}

/// How to run.
#[derive(Default)]
pub struct Options<'a> {
    /// Run only these cases, by identity; a file's identity selects all of
    /// its cases. Empty runs every case.
    pub select: Vec<String>,
    /// Run a seeded sample of what is selected.
    pub sample: Option<Sample>,
    /// Scores the atom's scorers with this judge; without one they are not
    /// run.
    pub judge: Option<&'a dyn Judge>,
    /// The report of an earlier revision, for each scorer's `max_drop`.
    pub baseline: Option<&'a Report>,
    /// The revision this run is at, recorded in its report.
    pub revision: Option<String>,
}

/// The cases `options` selects, in order.
pub fn select(cases: Vec<Case>, options: &Options<'_>) -> Vec<Case> {
    let mut chosen: Vec<Case> = if options.select.is_empty() {
        cases
    } else {
        cases
            .into_iter()
            .filter(|c| {
                options.select.iter().any(|s| {
                    c.id == *s
                        || c.id
                            .strip_prefix(s.as_str())
                            .is_some_and(|r| r.starts_with(':'))
                })
            })
            .collect()
    };
    if let Some(Sample { size, seed }) = options.sample
        && size < chosen.len()
    {
        // A seeded Fisher-Yates shuffle with splitmix64, then the first
        // `size`, kept in case order.
        let mut state = seed;
        let mut next = || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        let mut order: Vec<usize> = (0..chosen.len()).collect();
        for i in (1..order.len()).rev() {
            let j = usize::try_from(next() % (i as u64 + 1)).unwrap_or(0);
            order.swap(i, j);
        }
        let mut keep: Vec<usize> = order.into_iter().take(size).collect();
        keep.sort_unstable();
        chosen = keep.into_iter().map(|i| chosen[i].clone()).collect();
    }
    chosen
}

/// Check `case`'s assertions against `output`: the failures, empty when
/// every one holds.
pub fn check(case: &Case, output: &Value) -> Vec<String> {
    case.expect
        .iter()
        .filter_map(|source| {
            match expr::evaluate(source, &[("input", &case.input), ("output", output)]) {
                Ok(Value::Bool(true)) => None,
                Ok(other) => Some(format!("`{source}` gave {other}, not true")),
                Err(e) => Some(format!("`{source}`: {e}")),
            }
        })
        .collect()
}

/// Run `atom`'s `cases` as `options` selects through `subject`, against
/// `settings`' threshold.
pub async fn run(
    atom: &Atom,
    cases: Vec<Case>,
    settings: &Settings,
    subject: &dyn Subject,
    options: Options<'_>,
) -> Report {
    let cases = select(cases, &options);
    let mut results = Vec::with_capacity(cases.len());
    for case in &cases {
        let result = match subject.run(case).await {
            Err(e) => CaseResult {
                id: case.id.clone(),
                passed: false,
                failures: vec![e],
                output: None,
            },
            Ok(output) => {
                let failures = check(case, &output);
                CaseResult {
                    id: case.id.clone(),
                    passed: failures.is_empty(),
                    failures,
                    output: Some(output),
                }
            }
        };
        results.push(result);
    }
    let passed_cases = results.iter().filter(|r| r.passed).count();
    let pass_rate = if results.is_empty() {
        0.0
    } else {
        passed_cases as f64 / results.len() as f64
    };
    let mut failures = Vec::new();
    if results.is_empty() {
        failures.push(format!("`{}` has no eval cases to run", atom.name));
    } else if pass_rate < settings.pass_rate {
        failures.push(format!(
            "pass rate {:.3} ({passed_cases} of {}) is below the threshold {:.3}",
            pass_rate,
            results.len(),
            settings.pass_rate
        ));
    }
    let scorers = match options.judge {
        Some(judge) => score(atom, &cases, &results, judge, options.baseline).await,
        None => Vec::new(),
    };
    for s in scorers.iter().filter(|s| !s.passed) {
        failures.push(format!(
            "scorer `{}`: {}",
            s.name,
            s.reason.as_deref().unwrap_or("failed")
        ));
    }
    Report {
        atom: atom.name.clone(),
        revision: options.revision,
        cases: results,
        pass_rate,
        threshold: settings.pass_rate,
        scorers,
        passed: failures.is_empty(),
        failures,
    }
}

/// Score each passing case's output with each of `atom`'s scorers.
async fn score(
    atom: &Atom,
    cases: &[Case],
    results: &[CaseResult],
    judge: &dyn Judge,
    baseline: Option<&Report>,
) -> Vec<ScorerResult> {
    let declared: BTreeMap<String, Value> = atom
        .frontmatter
        .get("scorers")
        .and_then(Value::as_object)
        .map(|s| s.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    let registry = Registry::new();
    let mut out = Vec::new();
    for (name, scorer) in declared {
        let min_mean = scorer
            .get("min_mean")
            .and_then(Value::as_f64)
            .unwrap_or(1.0);
        let max_drop = scorer.get("max_drop").and_then(Value::as_f64);
        let judge_name = scorer
            .get("judge")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let previous = baseline.and_then(|b| {
            b.scorers
                .iter()
                .find(|s| s.name == name)
                .and_then(|s| s.mean)
        });
        let mut scores = Vec::new();
        let mut error = None;
        for (case, result) in cases.iter().zip(results) {
            let Some(output) = result.output.as_ref().filter(|_| result.passed) else {
                continue;
            };
            let run_id = format!("eval:{}:{}", atom.name, case.id);
            let values = crate::evaluation::Evaluation {
                run_id: &run_id,
                input: &case.input,
                output,
                case: &case.case,
                snapshot: None,
                step_id: None,
            };
            let scored = async {
                let input = crate::evaluation::judge_input(atom, &registry, &name, &values)?;
                let judged = judge.judge(&judge_name, input).await?;
                crate::evaluation::judge_score(atom, &name, &judged, None, None, &run_id)
            }
            .await;
            match scored {
                Ok(s) => scores.push(s),
                Err(e) => {
                    error = Some(format!("case {}: {e}", case.id));
                    break;
                }
            }
        }
        let mut result = ScorerResult {
            name: name.clone(),
            mean: None,
            min_mean,
            baseline: previous,
            max_drop,
            passed: false,
            reason: None,
        };
        if let Some(e) = error {
            result.reason = Some(e);
        } else if scores.is_empty() {
            result.reason = Some("no passing case to score".into());
        } else {
            match crate::evaluation::aggregate(atom, &name, &scores) {
                Err(e) => result.reason = Some(e),
                Ok(aggregate) => {
                    let mean = aggregate.get("mean").and_then(Value::as_f64).unwrap_or(0.0);
                    result.mean = Some(mean);
                    let reached = aggregate.get("passed") == Some(&Value::Bool(true));
                    let dropped = match (previous, max_drop) {
                        (Some(before), Some(drop)) if before - mean > drop + f64::EPSILON => {
                            Some(before - mean)
                        }
                        _ => None,
                    };
                    result.passed = reached && dropped.is_none();
                    result.reason = if !reached {
                        Some(format!("mean {mean:.3} is below its minimum {min_mean:.3}"))
                    } else {
                        dropped.map(|d| {
                            format!(
                                "mean {mean:.3} dropped {d:.3} from the baseline's {:.3}, past \
                                 its tolerance {:.3}",
                                previous.unwrap_or_default(),
                                max_drop.unwrap_or_default()
                            )
                        })
                    };
                }
            }
        }
        out.push(result);
    }
    out
}

/// `report` as JUnit XML: one test suite for the atom, a test case per eval
/// case and per scorer.
pub fn junit(report: &Report) -> String {
    fn escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }
    let failed_cases = report.cases.iter().filter(|c| !c.passed).count();
    let failed_scorers = report.scorers.iter().filter(|s| !s.passed).count();
    let threshold_failed = usize::from(report.pass_rate < report.threshold);
    let tests = report.cases.len() + report.scorers.len() + 1;
    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites>\n  <testsuite name=\"{}\" \
         tests=\"{tests}\" failures=\"{}\">\n",
        escape(&report.atom),
        failed_cases + failed_scorers + threshold_failed
    );
    let mut case = |name: String, failure: Option<String>| {
        xml.push_str(&format!(
            "    <testcase classname=\"{}\" name=\"{}\"",
            escape(&report.atom),
            escape(&name)
        ));
        match failure {
            None => xml.push_str("/>\n"),
            Some(message) => xml.push_str(&format!(
                ">\n      <failure message=\"{}\"/>\n    </testcase>\n",
                escape(&message)
            )),
        }
    };
    for c in &report.cases {
        case(c.id.clone(), (!c.passed).then(|| c.failures.join("; ")));
    }
    for s in &report.scorers {
        case(
            format!("scorer:{}", s.name),
            (!s.passed).then(|| s.reason.clone().unwrap_or_default()),
        );
    }
    case(
        "pass-rate".into(),
        (report.pass_rate < report.threshold).then(|| {
            format!(
                "pass rate {:.3} is below the threshold {:.3}",
                report.pass_rate, report.threshold
            )
        }),
    );
    xml.push_str("  </testsuite>\n</testsuites>\n");
    xml
}

/// The atom itself as a [`Subject`]: each case's input run on `runtimes`,
/// as team-curated input, its output checked against the atom's contract
/// and its `validate` rules as any run's is.
pub struct AtomSubject<'a> {
    /// The atom.
    pub atom: &'a Atom,
    /// Where it runs.
    pub runtimes: &'a crate::runtime::Runtimes,
}

impl Subject for AtomSubject<'_> {
    fn run<'a>(&'a self, case: &'a Case) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            if case.prepared.is_some() {
                return Err(
                    "a case's `prepared` values are not supported by this runner yet".into(),
                );
            }
            let mut registry = Registry::new();
            let compiled =
                crate::compile::compile(self.atom, &registry).map_err(|e| e.to_string())?;
            let run_id = format!("eval:{}:{}", self.atom.name, case.id);
            crate::runtime::run_declarative(
                self.atom,
                &compiled,
                &mut registry,
                &case.input,
                &run_id,
                "team",
                self.runtimes,
            )
            .await
            .map(|(output, _)| output)
            .map_err(|e| e.to_string())
        })
    }
}

#[cfg(test)]
mod tests;
