// SPDX-License-Identifier: Apache-2.0

//! Eval cases and the runner, against the scripted fake model:
//!
//! - cases kept many to a JSONL file run the same as cases kept one to a
//!   YAML file, with the same model calls;
//! - a case missing `expect`, or with an empty one, an unknown key, an input
//!   that breaks the contract or an assertion that names no field, fails the
//!   load, at its file and line;
//! - a pass rate below the atom's threshold fails the report;
//! - a scorer whose mean falls below its minimum, or further below a
//!   baseline's than its tolerance, fails the report;
//! - selection and seeded sampling choose the same cases every time;
//! - the report reads as JUnit XML.

use super::*;
use crate::codex::{CodexPaths, ModelEndpoint};
use crate::runtime::{Artifacts, CodexRuntime, Runtimes};
use crate::testing::{FakeModel, Reply};
use crate::typed::AtomConfig;
use serde_json::json;
use std::sync::Mutex;

/// A test project's atom with its input `{text}` and output `{ticket}`
/// (at most six characters), and `config` as its configuration.
fn atom_with(config: &str) -> (PathBuf, Atom) {
    let root = crate::atom::tests::project("Ticket for: {input.text}", config, None);
    if config.contains("scorers") {
        // Scorers are typed by the atom's case and judge schemas.
        std::fs::write(
            root.join("test.atom.astro"),
            format!(
                "---\nimport type {{ Props, Input, Output, Case, Judges }} from './.atomic/types/test';\n\
                 import type {{ AtomConfig }} from './.atomic/types/authoring';\n\
                 const {{ input }}: Props = Astro.props;\n\
                 export const config: AtomConfig<Input, Output, Case, Judges> = {config};\n---\n\
                 Ticket for: {{input.text}}\n"
            ),
        )
        .expect("source");
    }
    let atom =
        crate::atom::load(&root.join("test.atom.astro"), &mut Registry::new()).expect("loads");
    (root, atom)
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("dir")).expect("dir");
    std::fs::write(path, text).expect("write");
}

fn runtimes(model: &FakeModel, root: &Path) -> Runtimes {
    for sub in ["home", "cwd"] {
        std::fs::create_dir_all(root.join(sub)).expect("dir");
    }
    let endpoint = ModelEndpoint {
        base_url: model.base_url(),
        model: "synthetic".into(),
        api_key_env: None,
    };
    Runtimes {
        codex: CodexRuntime {
            paths: CodexPaths::without_helpers(),
            codex_home: root.join("home"),
            guard: crate::codex::StateGuard::NoProjectState,
            cwd: root.join("cwd"),
            endpoint: endpoint.clone(),
            defaults: AtomConfig::default(),
            artifacts: Artifacts::none(),
            meter: crate::usage::Metering::none(),
        },
        lite: crate::lite::LiteRuntime::new(endpoint, AtomConfig::default()),
        exec: Default::default(),
        clock: None,
    }
}

const CASES: [(&str, &str); 3] = [
    ("Export crashes", "output.ticket == 'BUG-1'"),
    ("Add dark mode", "output.ticket == 'FEAT-2'"),
    ("Why is it slow", "output.ticket.size() <= 6"),
];

/// The model's answers, one per case in order; the second does not fit its
/// case's assertion.
fn answers() -> Vec<Reply> {
    ["BUG-1", "FEAT-9", "Q-3"]
        .iter()
        .map(|t| Reply::Text(json!({ "ticket": t }).to_string()))
        .collect()
}

#[tokio::test]
async fn cases_in_a_jsonl_file_run_the_same_as_cases_one_to_a_file() {
    let (root, atom) = atom_with("{}");
    // The same three cases, one to a YAML file and all in one JSONL file.
    for (n, (text, assertion)) in CASES.iter().enumerate() {
        write(
            &root.join(format!("yaml/{n}.yaml")),
            &format!("input:\n  text: {text}\nexpect:\n  - \"{assertion}\"\n"),
        );
    }
    let lines: Vec<String> = CASES
        .iter()
        .map(|(text, assertion)| {
            json!({"input": {"text": text}, "expect": [assertion]}).to_string()
        })
        .collect();
    write(&root.join("jsonl/cases.jsonl"), &(lines.join("\n") + "\n"));

    let mut reports = Vec::new();
    for dir in ["yaml", "jsonl"] {
        let settings = Settings {
            dir: root.join(dir),
            pass_rate: 0.5,
        };
        let cases = load_cases(&atom, &settings).expect("cases load");
        assert_eq!(cases.len(), 3, "{dir}");
        let model = FakeModel::start(answers()).await.expect("fake model");
        let rt = runtimes(&model, &root.join(format!("rt-{dir}")));
        let subject = AtomSubject {
            atom: &atom,
            runtimes: &rt,
        };
        let report = run(&atom, cases, &settings, &subject, Options::default()).await;
        assert_eq!(model.calls(), 3, "{dir}: one call per case");
        reports.push(report);
    }
    let ids: Vec<Vec<String>> = reports
        .iter()
        .map(|r| r.cases.iter().map(|c| c.id.clone()).collect())
        .collect();
    assert_eq!(ids[0], ["0.yaml", "1.yaml", "2.yaml"]);
    assert_eq!(ids[1], ["cases.jsonl:1", "cases.jsonl:2", "cases.jsonl:3"]);
    // The same outcomes, case for case.
    let outcomes = |r: &Report| {
        r.cases
            .iter()
            .map(|c| (c.passed, c.output.clone(), c.failures.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(outcomes(&reports[0]), outcomes(&reports[1]));
    assert_eq!(reports[0].pass_rate, reports[1].pass_rate);
    assert!((reports[0].pass_rate - 2.0 / 3.0).abs() < 1e-9);
    assert!(!reports[0].cases[1].passed);
    assert!(
        reports[0].passed && reports[1].passed,
        "above a 0.5 threshold"
    );
}

#[test]
fn a_case_that_breaks_the_format_fails_the_load_where_it_is() {
    let (root, atom) = atom_with("{}");
    let settings = |dir: &str| Settings {
        dir: root.join(dir),
        pass_rate: 1.0,
    };
    for (n, (text, says)) in [
        ("input: {text: hi}\n", "needs `expect`"),
        ("input: {text: hi}\nexpect: []\n", "nonempty list"),
        (
            "input: {text: hi}\nexpect: [true]\n",
            "CEL expression, as a string",
        ),
        ("expect: [\"true\"]\n", "needs `input`"),
        (
            "input: {text: hi}\nexpect: [\"true\"]\noutput: {}\n",
            "unknown case field `output`",
        ),
        (
            "input: {text: ''}\nexpect: [\"true\"]\n",
            "the input does not fit",
        ),
        (
            "input: {text: hi}\nexpect: [\"output.tickett == 'x'\"]\n",
            "expect[0]",
        ),
    ]
    .iter()
    .enumerate()
    {
        let dir = format!("bad-{n}");
        write(&root.join(&dir).join("case.yaml"), text);
        let error = load_cases(&atom, &settings(&dir)).expect_err(text);
        assert!(error.message.contains(says), "{text}: {error}");
        assert!(error.file.ends_with("case.yaml"), "{error}");
    }
    // In a JSONL file, the line that is wrong.
    write(
        &root.join("lines/cases.jsonl"),
        "{\"input\": {\"text\": \"a\"}, \"expect\": [\"true\"]}\n\n{\"input\": {\"text\": \"b\"}}\n",
    );
    let error = load_cases(&atom, &settings("lines")).expect_err("no expect on line 3");
    assert_eq!(error.line, Some(3), "{error}");
    assert!(error.message.contains("needs `expect`"), "{error}");
    // A directory that is not there holds no cases.
    assert!(
        load_cases(&atom, &settings("absent"))
            .expect("none")
            .is_empty()
    );
}

#[test]
fn an_atoms_evals_field_names_its_directory_and_threshold() {
    let (root, atom) = atom_with("{}");
    let s = super::settings(&atom, &root).expect("defaults");
    assert_eq!((s.dir, s.pass_rate), (root.join("evals/test"), 1.0));
    let (root, atom) = atom_with("{evals: {dir: 'cases', pass_rate: 0.8}}");
    let s = super::settings(&atom, &root).expect("declared");
    assert_eq!((s.dir, s.pass_rate), (root.join("cases"), 0.8));
    let (root, atom) = atom_with("{evals: 'elsewhere'}");
    assert_eq!(
        super::settings(&atom, &root).expect("a path").dir,
        root.join("elsewhere")
    );
    for bad in ["{evals: {pass_rate: 2}}", "{evals: {pass_rate: -0.5}}"] {
        let (root, atom) = atom_with(bad);
        assert!(super::settings(&atom, &root).is_err(), "{bad}");
    }
}

/// A subject that answers each case from a table, and counts its runs.
struct Table {
    outputs: BTreeMap<String, Value>,
    runs: Mutex<usize>,
}

impl Subject for Table {
    fn run<'a>(&'a self, case: &'a Case) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            *self.runs.lock().expect("runs") += 1;
            self.outputs
                .get(&case.id)
                .cloned()
                .ok_or_else(|| "the atom failed".to_owned())
        })
    }
}

fn case(id: &str, assertion: &str) -> Case {
    Case {
        id: id.into(),
        input: json!({"text": id}),
        expect: vec![assertion.into()],
        case: json!({"ok": true}),
        prepared: None,
    }
}

fn table(outputs: &[(&str, &str)]) -> Table {
    Table {
        outputs: outputs
            .iter()
            .map(|(id, t)| ((*id).to_owned(), json!({ "ticket": t })))
            .collect(),
        runs: Mutex::new(0),
    }
}

#[tokio::test]
async fn a_pass_rate_below_the_threshold_fails_the_report() {
    let (_, atom) = atom_with("{}");
    let cases = vec![
        case("a", "output.ticket == 'A'"),
        case("b", "output.ticket == 'B'"),
        case("c", "output.ticket == 'C'"),
        case("d", "output.ticket == 'D'"),
    ];
    // Three pass; "c" answers wrong and "d" fails to run.
    let subject = table(&[("a", "A"), ("b", "B"), ("c", "X")]);
    let at = |pass_rate: f64| Settings {
        dir: PathBuf::new(),
        pass_rate,
    };
    let report = run(
        &atom,
        cases.clone(),
        &at(0.75),
        &subject,
        Options::default(),
    )
    .await;
    assert_eq!(report.pass_rate, 0.5);
    assert!(!report.passed, "{report:?}");
    assert!(
        report.failures[0].contains("below the threshold 0.750"),
        "{report:?}"
    );
    assert!(
        report.cases[2].failures[0].contains("gave false"),
        "{report:?}"
    );
    assert_eq!(report.cases[3].failures, ["the atom failed"]);
    let report = run(&atom, cases, &at(0.5), &subject, Options::default()).await;
    assert!(report.passed, "{report:?}");
    // No cases at all never passes.
    let report = run(&atom, Vec::new(), &at(0.0), &subject, Options::default()).await;
    assert!(!report.passed);
}

const SCORED: &str = "{
  case: {type:'object',additionalProperties:false,required:['ok'],properties:{ok:{type:'boolean'}}},
  judges: {reviewer: {
    input: {type:'object',additionalProperties:false,required:['text'],properties:{text:{type:'string'}}},
    output: {type:'object',additionalProperties:false,required:['ok'],properties:{ok:{type:'boolean'}}},
  }},
  scorers: {quality: {judge:'reviewer',with:({input}) => ({text:input.text}),score:({judge}) => judge.ok ? 1 : 0,min_mean:0.5,max_drop:0.25}},
}";

/// A judge that approves every text but those listed.
struct Reviewer(Vec<&'static str>);

impl Judge for Reviewer {
    fn judge<'a>(&'a self, judge: &'a str, input: Value) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            assert_eq!(judge, "reviewer");
            let text = input["text"].as_str().unwrap_or_default();
            Ok(json!({"ok": !self.0.contains(&text)}))
        })
    }
}

#[tokio::test]
async fn a_scorer_below_its_minimum_or_past_its_tolerance_fails_the_report() {
    let (_, atom) = atom_with(SCORED);
    let cases: Vec<Case> = ["a", "b", "c", "d", "e"]
        .iter()
        .map(|id| case(id, "output.ticket.size() > 0"))
        .collect();
    let subject = table(&[("a", "A"), ("b", "B"), ("c", "C"), ("d", "D"), ("e", "E")]);
    let settings = Settings {
        dir: PathBuf::new(),
        pass_rate: 1.0,
    };
    let scored = |rejects: Vec<&'static str>, baseline: Option<Report>| {
        let (atom, cases, subject, settings) = (&atom, cases.clone(), &subject, &settings);
        async move {
            let judge = Reviewer(rejects);
            run(
                atom,
                cases,
                settings,
                subject,
                Options {
                    judge: Some(&judge),
                    baseline: baseline.as_ref(),
                    ..Options::default()
                },
            )
            .await
        }
    };
    // All approved: mean 1, above its minimum, and the baseline from now on.
    let baseline = scored(Vec::new(), None).await;
    assert!(baseline.passed, "{baseline:?}");
    assert_eq!(baseline.scorers[0].mean, Some(1.0));
    // Two of five rejected: mean 0.6, above its 0.5 minimum but 0.4 below
    // the baseline, past its 0.1 tolerance.
    let dropped = scored(vec!["a", "b"], Some(baseline.clone())).await;
    assert!(!dropped.passed, "{dropped:?}");
    assert_eq!(dropped.scorers[0].mean, Some(0.6));
    assert!(
        dropped.failures[0].contains("past its tolerance 0.250"),
        "{dropped:?}"
    );
    // Without a baseline the same run passes.
    assert!(scored(vec!["a", "b"], None).await.passed);
    // One rejected of five: 0.2 below the baseline, within its tolerance.
    assert!(scored(vec!["a"], Some(baseline.clone())).await.passed);
    // Three of five: mean 0.4, below its minimum, baseline or not.
    let low = scored(vec!["a", "b", "c"], None).await;
    assert!(!low.passed);
    assert!(
        low.failures[0].contains("below its minimum 0.500"),
        "{low:?}"
    );
}

#[test]
fn selection_and_seeded_sampling_choose_the_same_cases_every_time() {
    let cases: Vec<Case> = (1..=20)
        .map(|n| case(&format!("cases.jsonl:{n}"), "true"))
        .chain([case("one.yaml", "true")])
        .collect();
    let pick = |options: Options<'_>| -> Vec<String> {
        select(cases.clone(), &options)
            .into_iter()
            .map(|c| c.id)
            .collect()
    };
    // A file selects all its cases; a case selects itself.
    let file = pick(Options {
        select: vec!["cases.jsonl".into()],
        ..Options::default()
    });
    assert_eq!(file.len(), 20);
    assert_eq!(
        pick(Options {
            select: vec!["one.yaml".into(), "cases.jsonl:3".into()],
            ..Options::default()
        }),
        ["cases.jsonl:3", "one.yaml"]
    );
    let sampled = |seed| {
        pick(Options {
            sample: Some(Sample { size: 5, seed }),
            ..Options::default()
        })
    };
    assert_eq!(sampled(7), sampled(7), "the same seed, the same sample");
    assert_eq!(sampled(7).len(), 5);
    assert_ne!(sampled(7), sampled(8), "another seed, another sample");
}

#[tokio::test]
async fn the_report_reads_as_junit_xml() {
    let (_, atom) = atom_with("{}");
    let subject = table(&[("a", "A"), ("b", "<B>")]);
    let report = run(
        &atom,
        vec![
            case("a", "output.ticket == 'A'"),
            case("b", "output.ticket == 'B'"),
        ],
        &Settings {
            dir: PathBuf::new(),
            pass_rate: 1.0,
        },
        &subject,
        Options::default(),
    )
    .await;
    let xml = junit(&report);
    assert!(
        xml.contains("<testsuite name=\"test\" tests=\"3\" failures=\"2\">"),
        "{xml}"
    );
    assert!(
        xml.contains("<testcase classname=\"test\" name=\"a\"/>"),
        "{xml}"
    );
    assert!(
        xml.contains("&lt;B&gt;") || xml.contains("gave false"),
        "{xml}"
    );
    assert!(xml.contains("name=\"pass-rate\""), "{xml}");
}
