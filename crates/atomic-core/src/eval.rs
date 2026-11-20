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
use crate::project::Project;
use atomic_schema::expr::{self, Scope, Shape};
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
    /// The project the cases belong to: their directory is under its root,
    /// and they are found as its collections are.
    pub project: Project,
    /// Whether the directory was named by the atom, so must exist.
    pub declared: bool,
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

/// `atom`'s eval settings in `project`. Its `evals` field is either the
/// eval directory, relative to the atom file, or a mapping of `dir` and
/// `pass_rate`. The directory must be inside the project.
pub fn settings(atom: &Atom, project: &Project) -> Result<Settings, CaseError> {
    let fail = |message: String| CaseError {
        file: atom.file.clone(),
        line: None,
        message,
    };
    let beside = |dir: &str| atom.file.parent().unwrap_or(Path::new("")).join(dir);
    let default = project.dir.join("evals").join(&atom.name);
    let (dir, pass_rate, declared) = match atom.frontmatter.get("evals") {
        None => (default, 1.0, false),
        Some(Value::String(dir)) => (beside(dir), 1.0, true),
        Some(Value::Object(fields)) => {
            if let Some(other) = fields
                .keys()
                .find(|k| !["dir", "pass_rate"].contains(&k.as_str()))
            {
                return Err(fail(format!(
                    "evals: unknown field `{other}`: evals has dir and pass_rate"
                )));
            }
            let (dir, declared) = match fields.get("dir") {
                None => (default, false),
                Some(Value::String(dir)) => (beside(dir), true),
                Some(_) => return Err(fail("evals.dir must be a path".into())),
            };
            let pass_rate = match fields.get("pass_rate") {
                None => 1.0,
                Some(v) => v
                    .as_f64()
                    .filter(|r| (0.0..=1.0).contains(r))
                    .ok_or_else(|| fail("evals.pass_rate must be between zero and one".into()))?,
            };
            (dir, pass_rate, declared)
        }
        Some(_) => {
            return Err(fail(
                "evals must be a directory or a mapping of dir and pass_rate".into(),
            ));
        }
    };
    // Inside the project, however it is written: no absolute path, `..` or
    // link leads out of it.
    let root = std::fs::canonicalize(&project.dir)
        .map_err(|e| fail(format!("the project directory: {e}")))?;
    let dir = match std::fs::canonicalize(&dir) {
        Ok(real) => real,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !declared => dir,
        Err(e) => {
            return Err(fail(format!(
                "the eval directory {} the atom names: {e}",
                dir.display()
            )));
        }
    };
    if !dir.starts_with(&root) && dir.exists() {
        return Err(fail(format!(
            "the eval directory {} is outside the project at {}",
            dir.display(),
            root.display()
        )));
    }
    Ok(Settings {
        dir,
        pass_rate,
        project: project.clone(),
        declared,
    })
}

/// The most cases one atom may have, and the largest case file read.
const MAX_CASES: usize = 10_000;
const MAX_FILE: u64 = 16 * 1024 * 1024;

/// Load `atom`'s cases from `settings.dir`, in order of their identities.
/// The directory is walked as the project's collections are, following no
/// link; a default directory that is missing holds no cases. Every case is
/// checked now: its keys, its input against the atom's input contract, its
/// `case` against the atom's case schema, and each assertion parsed and
/// checked against the input and output contracts.
pub fn load_cases(atom: &Atom, settings: &Settings) -> Result<Vec<Case>, CaseError> {
    let at_dir = |message: String| CaseError {
        file: settings.dir.clone(),
        line: None,
        message,
    };
    let files = crate::collections::walk(&settings.project, &settings.dir)
        .map_err(|e| at_dir(format!("read the eval directory: {e}")))?;
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
        let at = |line: Option<usize>, message: String| CaseError {
            file: file.clone(),
            line,
            message,
        };
        if relative.ends_with(".yml") {
            return Err(at(None, "a YAML case file ends in .yaml".into()));
        }
        let jsonl = relative.ends_with(".jsonl");
        if !jsonl && !relative.ends_with(".yaml") {
            continue;
        }
        if std::fs::metadata(&file).is_ok_and(|m| m.len() > MAX_FILE) {
            return Err(at(
                None,
                format!("a case file is at most {MAX_FILE} bytes; split it"),
            ));
        }
        let text =
            std::fs::read_to_string(&file).map_err(|e| at(None, format!("read the case: {e}")))?;
        if jsonl {
            for (n, line) in text.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let line_no = Some(n + 1);
                let value = strict_json(line)
                    .map_err(|e| at(line_no, format!("a case is one JSON object per line: {e}")))?;
                cases.push(
                    case_from(&value, format!("{relative}:{}", n + 1), &contracts)
                        .map_err(|(_, message)| at(line_no, message))?,
                );
            }
        } else {
            let doc = atomic_schema::yaml::load(&file, &text)
                .map_err(|e| at(Some(e.at.line), e.message))?;
            cases.push(case_from(&doc.value, relative.clone(), &contracts).map_err(
                |(pointer, message)| {
                    let line = doc
                        .position(&pointer)
                        .or_else(|| doc.position(""))
                        .map(|p| p.line);
                    at(line, message)
                },
            )?);
        }
        if cases.len() > MAX_CASES {
            return Err(at_dir(format!(
                "an atom has at most {MAX_CASES} eval cases"
            )));
        }
    }
    if !cases.is_empty() && atom.external() == Some(crate::atom::External::Idempotent) {
        return Err(at_dir(format!(
            "`{}` writes to an outside system, and atomic never runs such an atom in an eval: \
             cover it with code tests instead of eval cases",
            atom.name
        )));
    }
    Ok(cases)
}

/// The schemas a case is checked against, and its assertions typed by.
#[derive(Clone, Debug)]
pub struct Contracts {
    /// The atom's input contract.
    pub input: Value,
    /// The atom's output contract.
    pub output: Value,
    /// The atom's case schema.
    pub case: Value,
}

impl Contracts {
    /// `atom`'s contracts.
    pub fn of(atom: &Atom) -> Result<Self, String> {
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

/// Why a case was refused: the JSON Pointer of the field at fault, and what
/// is wrong.
type Refusal = (String, String);

fn case_from(value: &Value, id: String, contracts: &Contracts) -> Result<Case, Refusal> {
    let root = |m: &str| (String::new(), m.to_owned());
    let Value::Object(fields) = value else {
        return Err(root("a case is a mapping of input and expect"));
    };
    if let Some(other) = fields.keys().find(|k| !KEYS.contains(&k.as_str())) {
        return Err((
            format!("/{}", other.replace('~', "~0").replace('/', "~1")),
            format!(
                "unknown case field `{other}`: a case has input and expect (and, for scorers, case)"
            ),
        ));
    }
    let input = fields
        .get("input")
        .cloned()
        .ok_or_else(|| root("a case needs `input`, the atom's input"))?;
    let at_expect = |m: &str| ("/expect".to_owned(), m.to_owned());
    let expect = match fields.get("expect") {
        None => {
            return Err(root(
                "a case needs `expect`, a nonempty list of CEL assertions",
            ));
        }
        Some(Value::Array(items)) if !items.is_empty() => items
            .iter()
            .map(|e| e.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| at_expect("each `expect` entry is a CEL expression, as a string"))?,
        Some(_) => {
            return Err(at_expect(
                "`expect` must be a nonempty list of CEL assertions",
            ));
        }
    };
    fits(&contracts.input, &input, "the input").map_err(|m| ("/input".to_owned(), m))?;
    let case = fields.get("case").cloned().unwrap_or(Value::Null);
    if fields.contains_key("case") {
        fits(&contracts.case, &case, "the case metadata").map_err(|m| ("/case".to_owned(), m))?;
    }
    let scope = Scope::new()
        .with("input", &contracts.input)
        .with("output", &contracts.output);
    for (n, source) in expect.iter().enumerate() {
        expr::parse(source)
            .and_then(|e| e.check(&scope))
            .map_err(|e| {
                (
                    format!("/expect/{n}"),
                    format!("expect[{n}] `{source}`: {e}"),
                )
            })?;
    }
    Ok(Case {
        id,
        input,
        expect,
        case,
        prepared: fields.get("prepared").cloned(),
    })
}

/// Parse one JSONL line as JSON, refusing an object that repeats a key: a
/// plain parse would keep the last one and hide the first. The value comes
/// from serde_json's own parse, which keeps every number as written; a
/// second pass over the same text only looks for repeated keys.
fn strict_json(line: &str) -> Result<Value, String> {
    use serde::de::{Deserializer, MapAccess, SeqAccess, Visitor};
    /// Visits a document and keeps nothing, failing on a repeated key.
    struct Unique;
    impl<'de> serde::de::DeserializeSeed<'de> for Unique {
        type Value = ();
        fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
            d.deserialize_any(Unique)
        }
    }
    impl<'de> Visitor<'de> for Unique {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a JSON value")
        }
        fn visit_bool<E>(self, _: bool) -> Result<(), E> {
            Ok(())
        }
        fn visit_i64<E>(self, _: i64) -> Result<(), E> {
            Ok(())
        }
        fn visit_u64<E>(self, _: u64) -> Result<(), E> {
            Ok(())
        }
        fn visit_f64<E>(self, _: f64) -> Result<(), E> {
            Ok(())
        }
        fn visit_str<E>(self, _: &str) -> Result<(), E> {
            Ok(())
        }
        fn visit_unit<E>(self) -> Result<(), E> {
            Ok(())
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
            while seq.next_element_seed(Unique)?.is_some() {}
            Ok(())
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            // A number may arrive as a one-entry map of serde_json's own
            // (with its arbitrary precision on); it has no repeated key.
            let mut seen = std::collections::BTreeSet::new();
            while let Some(key) = map.next_key::<String>()? {
                if !seen.insert(key.clone()) {
                    return Err(serde::de::Error::custom(format!("duplicate key {key:?}")));
                }
                map.next_value_seed(Unique)?;
            }
            Ok(())
        }
    }
    let value: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
    let mut d = serde_json::Deserializer::from_str(line);
    serde::de::DeserializeSeed::deserialize(Unique, &mut d).map_err(|e| e.to_string())?;
    Ok(value)
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
    /// The cases it scored: those that passed.
    #[serde(default)]
    pub scored: Vec<String>,
    /// The cases it did not score, because they failed.
    #[serde(default)]
    pub skipped: Vec<String>,
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
    /// The scorers the atom declares that did not run, because no judge was
    /// given: the report says nothing of them.
    #[serde(default)]
    pub not_scored: Vec<String>,
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

/// The selectors in `options` that name none of `cases`.
pub fn unmatched(cases: &[Case], options: &Options<'_>) -> Vec<String> {
    options
        .select
        .iter()
        .filter(|s| {
            !cases.iter().any(|c| {
                c.id == **s
                    || c.id
                        .strip_prefix(s.as_str())
                        .is_some_and(|r| r.starts_with(':'))
            })
        })
        .cloned()
        .collect()
}

/// How far apart two means may be and still count as equal, for sums of
/// scores that do not add exactly in binary.
const TOLERANCE: f64 = 1e-9;

/// Check `case`'s assertions against `output`, each value typed by the
/// atom's `contracts`: the failures, empty when every one holds.
pub fn check(case: &Case, output: &Value, contracts: &Contracts) -> Vec<String> {
    let (input, output_shape) = (
        Shape::Schema(&contracts.input),
        Shape::Schema(&contracts.output),
    );
    case.expect
        .iter()
        .filter_map(|source| {
            match expr::evaluate(
                source,
                &[
                    ("input", &case.input, &input),
                    ("output", output, &output_shape),
                ],
            ) {
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
    let mut failures: Vec<String> = unmatched(&cases, &options)
        .into_iter()
        .map(|s| format!("`{s}` selects no case of `{}`", atom.name))
        .collect();
    let cases = select(cases, &options);
    let contracts = Contracts::of(atom);
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
                let failures = match &contracts {
                    Ok(contracts) => check(case, &output, contracts),
                    Err(e) => vec![e.clone()],
                };
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
    // A baseline is only compared when it is this atom's, over these cases.
    let baseline = options.baseline.filter(|b| {
        let ours: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
        let theirs: Vec<&str> = b.cases.iter().map(|r| r.id.as_str()).collect();
        if b.atom != atom.name {
            failures.push(format!(
                "the baseline is a report for `{}`, not `{}`",
                b.atom, atom.name
            ));
            false
        } else if ours != theirs {
            failures.push(format!(
                "the baseline ran other cases ({} to this run's {}): run both on the same cases",
                theirs.len(),
                ours.len()
            ));
            false
        } else {
            true
        }
    });
    let declared: Vec<String> = atom
        .frontmatter
        .get("scorers")
        .and_then(Value::as_object)
        .map(|s| s.keys().cloned().collect())
        .unwrap_or_default();
    let (scorers, not_scored) = match options.judge {
        Some(judge) => (
            score(atom, &cases, &results, judge, baseline).await,
            Vec::new(),
        ),
        None => (Vec::new(), declared),
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
        not_scored,
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
        let (mut scored, mut skipped) = (Vec::new(), Vec::new());
        for (case, result) in cases.iter().zip(results) {
            let Some(output) = result.output.as_ref().filter(|_| result.passed) else {
                skipped.push(case.id.clone());
                continue;
            };
            scored.push(case.id.clone());
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
            scored,
            skipped,
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
                    // A mean that misses its minimum by rounding alone
                    // reaches it: three scores of 0.7 average 0.7.
                    let reached = aggregate.get("passed") == Some(&Value::Bool(true))
                        || (mean - min_mean).abs() <= TOLERANCE;
                    let dropped = match (previous, max_drop) {
                        (Some(before), Some(drop)) if before - mean > drop + TOLERANCE => {
                            Some(before - mean)
                        }
                        _ => None,
                    };
                    // A tolerance with a baseline that never scored this
                    // scorer cannot be checked, so it does not pass quietly.
                    let unchecked = baseline.is_some() && max_drop.is_some() && previous.is_none();
                    result.passed = reached && dropped.is_none() && !unchecked;
                    result.reason = if !reached {
                        Some(format!("mean {mean:.3} is below its minimum {min_mean:.3}"))
                    } else if unchecked {
                        Some(
                            "the baseline has no mean for it, so its max_drop cannot be checked"
                                .into(),
                        )
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
/// case, per scorer and per failure of the run as a whole, and a skipped
/// test case per scorer that was not run.
pub fn junit(report: &Report) -> String {
    let mut body = String::new();
    let (mut tests, mut failed) = (0, 0);
    let mut case = |name: &str, failure: Option<&str>, skipped: bool| {
        tests += 1;
        body.push_str(&format!(
            "    <testcase classname=\"{}\" name=\"{}\"",
            escape(&report.atom),
            escape(name)
        ));
        match (failure, skipped) {
            (Some(message), _) => {
                failed += 1;
                body.push_str(&format!(
                    ">\n      <failure message=\"{}\"/>\n    </testcase>\n",
                    escape(message)
                ));
            }
            (None, true) => body.push_str(">\n      <skipped/>\n    </testcase>\n"),
            (None, false) => body.push_str("/>\n"),
        }
    };
    for c in &report.cases {
        let failures = c.failures.join("; ");
        case(&c.id, (!c.passed).then_some(failures.as_str()), false);
    }
    for s in &report.scorers {
        let reason = s.reason.clone().unwrap_or_else(|| "failed".into());
        case(
            &format!("scorer:{}", s.name),
            (!s.passed).then_some(reason.as_str()),
            false,
        );
    }
    for name in &report.not_scored {
        case(&format!("scorer:{name}"), None, true);
    }
    // The failures of the run as a whole: no cases, the pass rate, the
    // baseline, a selector. Scorers' are their own test cases above.
    let run: Vec<&String> = report
        .failures
        .iter()
        .filter(|f| !f.starts_with("scorer `"))
        .collect();
    if run.is_empty() {
        case("run", None, false);
    }
    for (n, failure) in run.iter().enumerate() {
        let name = if n == 0 {
            "run".to_owned()
        } else {
            format!("run:{}", n + 1)
        };
        case(&name, Some(failure), false);
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites>\n  <testsuite name=\"{}\" \
         tests=\"{tests}\" failures=\"{failed}\">\n{body}  </testsuite>\n</testsuites>\n",
        escape(&report.atom),
    )
}

/// `text` as XML attribute text: markup escaped, line breaks kept as
/// character references, and the characters XML 1.0 cannot carry at all,
/// such as other control characters a model put in its output, dropped.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '\t' => out.push_str("&#9;"),
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
    out
}

/// The atom itself as a [`Subject`]: each case's input run on `runtimes`,
/// as external input, its output checked against the atom's contract and
/// its `validate` rules as any run's is. An atom that writes to an outside
/// system is never run, whatever loaded its cases.
pub struct AtomSubject<'a> {
    /// The atom.
    pub atom: &'a Atom,
    /// Where it runs.
    pub runtimes: &'a crate::runtime::Runtimes,
}

impl Subject for AtomSubject<'_> {
    fn run<'a>(&'a self, case: &'a Case) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            if self.atom.external() == Some(crate::atom::External::Idempotent) {
                return Err(format!(
                    "`{}` writes to an outside system, and atomic never runs such an atom in an \
                     eval",
                    self.atom.name
                ));
            }
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
                // A case file is not team-reviewed input: it may hold text
                // copied from anywhere, so it renders as external.
                "external",
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
