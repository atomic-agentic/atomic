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

/// The project at `root`, given a manifest if it has none.
fn project_at(root: &Path) -> Project {
    let manifest = root.join("atomic.yaml");
    if !manifest.exists() {
        std::fs::write(&manifest, "atomic: 1\n").expect("manifest");
    }
    crate::project::load(&manifest).expect("project")
}

/// Settings reading cases from `dir` under `root`, at `pass_rate`.
fn at(root: &Path, dir: &str, pass_rate: f64) -> Settings {
    Settings {
        dir: root.join(dir),
        pass_rate,
        project: project_at(root),
        declared: true,
    }
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
        sign_in: None,
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
            backend: Default::default(),
            caches: None,
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
    let mut loaded = Vec::new();
    for dir in ["yaml", "jsonl"] {
        let settings = at(&root, dir, 0.5);
        let cases = load_cases(&atom, &settings).expect("cases load");
        assert_eq!(cases.len(), 3, "{dir}");
        loaded.push(
            cases
                .iter()
                .map(|c| (c.input.clone(), c.expect.clone(), c.case.clone()))
                .collect::<Vec<_>>(),
        );
        let model = FakeModel::start(answers()).await.expect("fake model");
        let rt = runtimes(&model, &root.join(format!("rt-{dir}")));
        let subject = AtomSubject {
            atom: &atom,
            runtimes: &rt,
        };
        let report = run(&atom, cases, &settings, &subject, Options::default()).await;
        assert_eq!(model.calls(), 3, "{dir}: one call per case");
        // A case's input reaches the model as external data.
        let sent: String = model.requests().iter().map(Value::to_string).collect();
        assert!(
            sent.contains(r#"origin=\"external\">Export crashes"#),
            "{sent}"
        );
        assert!(!sent.contains(r#"origin=\"team\""#), "{sent}");
        reports.push(report);
    }
    // The same cases were loaded from each, input and assertions alike.
    assert_eq!(loaded[0], loaded[1]);
    assert_eq!(loaded[0][0].0, json!({"text": "Export crashes"}));
    assert_eq!(loaded[0][0].1, ["output.ticket == 'BUG-1'"]);
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

#[tokio::test]
async fn a_cases_origin_is_held_to_the_atoms_trust_and_labels_its_prompt() {
    let case_file = |origin: Option<&str>| {
        let label = origin.map(|o| format!("origin: {o}\n")).unwrap_or_default();
        format!(
            "{label}input:\n  text: Export crashes\nexpect:\n  - \"output.ticket == 'BUG-1'\"\n"
        )
    };
    // A team atom refuses a case of the default origin, external, as it
    // loads: before any call, naming the case, the atom, its trust and the
    // case's origin.
    let (root, team) = atom_with("{trust:'team'}");
    write(&root.join("default/a.yaml"), &case_file(None));
    let refused = load_cases(&team, &at(&root, "default", 1.0))
        .expect_err("an external case for a team atom")
        .to_string();
    for said in [
        "a.yaml",
        "case a.yaml's input",
        "is external data",
        "atom `test` accepts team input at most",
        "trust: 'team'",
    ] {
        assert!(refused.contains(said), "{said}: {refused}");
    }
    // A case its own file labels team is taken, and its input is labelled
    // team in the prompt.
    write(&root.join("reviewed/a.yaml"), &case_file(Some("team")));
    let settings = at(&root, "reviewed", 1.0);
    let cases = load_cases(&team, &settings).expect("a team case");
    assert_eq!(cases[0].origin, "team");
    let model = FakeModel::start(answers()).await.expect("fake model");
    let rt = runtimes(&model, &root.join("rt-team"));
    let subject = AtomSubject {
        atom: &team,
        runtimes: &rt,
    };
    let report = run(&team, cases, &settings, &subject, Options::default()).await;
    assert!(report.passed, "{report:?}");
    let sent: String = model.requests().iter().map(Value::to_string).collect();
    assert!(sent.contains(r#"origin=\"team\">Export crashes"#), "{sent}");
    assert!(!sent.contains(r#"origin=\"external\""#), "{sent}");

    // Trust is a minimum: an agent atom takes agent and team cases, and
    // refuses an external one, labelled or not.
    let (root, agent) = atom_with("{trust:'agent'}");
    for (origin, taken) in [
        (Some("agent"), true),
        (Some("team"), true),
        (Some("external"), false),
        (None, false),
    ] {
        let dir = format!("agent-{}", origin.unwrap_or("default"));
        write(&root.join(format!("{dir}/a.yaml")), &case_file(origin));
        let loaded = load_cases(&agent, &at(&root, &dir, 1.0));
        assert_eq!(loaded.is_ok(), taken, "{origin:?}: {loaded:?}");
    }
    // A case captured from a run, appended to a JSONL file with the origin
    // its input was recorded with, keeps it.
    write(
        &root.join("captured/cases.jsonl"),
        &(json!({"origin": "agent", "input": {"text": "Export crashes"}, "expect": ["true"]})
            .to_string()
            + "\n"),
    );
    let captured = load_cases(&agent, &at(&root, "captured", 1.0)).expect("a captured case");
    assert_eq!(captured[0].origin, "agent");
    // The hint fits the atom: an agent atom's case may say it came from a
    // model step, or from a person on the team.
    write(&root.join("hint/a.yaml"), &case_file(None));
    let hint = load_cases(&agent, &at(&root, "hint", 1.0))
        .expect_err("an external case for an agent atom")
        .to_string();
    assert!(
        hint.contains("`origin: agent` if its input came from a model step")
            && hint.contains("`origin: team` if a person on the team wrote or reviewed it"),
        "{hint}"
    );
    // A case built outside the load is checked before it runs too.
    let unchecked = Case {
        id: "built.yaml".into(),
        input: json!({"text": "Export crashes"}),
        expect: vec!["true".into()],
        case: Value::Null,
        prepared: None,
        origin: "external".into(),
    };
    let model = FakeModel::start(answers()).await.expect("fake model");
    let rt = runtimes(&model, &root.join("rt-built"));
    let subject = AtomSubject {
        atom: &agent,
        runtimes: &rt,
    };
    let refused = subject
        .run(&unchecked)
        .await
        .expect_err("an unchecked case");
    assert!(refused.contains("accepts agent input at most"), "{refused}");
    assert_eq!(model.calls(), 0);
    // An origin that is not one of the three is refused where it is written.
    write(&root.join("odd/a.yaml"), &case_file(Some("Team")));
    let odd = load_cases(&agent, &at(&root, "odd", 1.0))
        .expect_err("an unknown origin")
        .to_string();
    assert!(odd.contains("`origin` is external, agent, team"), "{odd}");
}

#[test]
fn a_case_that_breaks_the_format_fails_the_load_where_it_is() {
    let (root, atom) = atom_with("{}");
    let settings = |dir: &str| at(&root, dir, 1.0);
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
    let s = super::settings(&atom, &project_at(&root)).expect("defaults");
    assert_eq!((s.dir, s.pass_rate), (root.join("evals/test"), 1.0));
    let (root, atom) = atom_with("{evals: {dir: 'cases', pass_rate: 0.8}}");
    std::fs::create_dir(root.join("cases")).expect("dir");
    let s = super::settings(&atom, &project_at(&root)).expect("declared");
    assert_eq!((s.dir, s.pass_rate), (root.join("cases"), 0.8));
    let (root, atom) = atom_with("{evals: 'elsewhere'}");
    std::fs::create_dir(root.join("elsewhere")).expect("dir");
    assert_eq!(
        super::settings(&atom, &project_at(&root))
            .expect("a path")
            .dir,
        root.join("elsewhere")
    );
    for bad in ["{evals: {pass_rate: 2}}", "{evals: {pass_rate: -0.5}}"] {
        let (root, atom) = atom_with(bad);
        assert!(super::settings(&atom, &project_at(&root)).is_err(), "{bad}");
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
        origin: "external".into(),
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
    let (root, atom) = atom_with("{}");
    let cases = vec![
        case("a", "output.ticket == 'A'"),
        case("b", "output.ticket == 'B'"),
        case("c", "output.ticket == 'C'"),
        case("d", "output.ticket == 'D'"),
    ];
    // Three pass; "c" answers wrong and "d" fails to run.
    let subject = table(&[("a", "A"), ("b", "B"), ("c", "X")]);
    let with = |pass_rate: f64| at(&root, "evals", pass_rate);
    let report = run(
        &atom,
        cases.clone(),
        &with(0.75),
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
    let report = run(&atom, cases, &with(0.5), &subject, Options::default()).await;
    assert!(report.passed, "{report:?}");
    // No cases at all never passes.
    let report = run(&atom, Vec::new(), &with(0.0), &subject, Options::default()).await;
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
    let (root, atom) = atom_with(SCORED);
    let cases: Vec<Case> = ["a", "b", "c", "d", "e"]
        .iter()
        .map(|id| case(id, "output.ticket.size() > 0"))
        .collect();
    let subject = table(&[("a", "A"), ("b", "B"), ("c", "C"), ("d", "D"), ("e", "E")]);
    let settings = at(&root, "evals", 1.0);
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

/// A subject that fails each case with the message it is given.
struct Failing(&'static str);

impl Subject for Failing {
    fn run<'a>(&'a self, _: &'a Case) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move { Err(self.0.to_owned()) })
    }
}

#[tokio::test]
async fn the_report_reads_as_junit_xml() {
    let (root, atom) = atom_with("{}");
    let subject = table(&[("a", "A"), ("b", "B")]);
    let report = run(
        &atom,
        vec![
            case("a", "output.ticket == 'A'"),
            case("b", "output.ticket == 'C'"),
        ],
        &at(&root, "evals", 1.0),
        &subject,
        Options::default(),
    )
    .await;
    let xml = junit(&report);
    // Two cases and the run's pass rate; the second case and the rate fail.
    assert!(
        xml.contains(
            "<testsuite name=\"test\" tests=\"3\" failures=\"2\" errors=\"0\" skipped=\"0\">"
        ),
        "{xml}"
    );
    assert!(
        xml.contains("<testcase classname=\"test\" name=\"a\"/>"),
        "{xml}"
    );
    assert!(
        xml.contains("<failure message=\"`output.ticket == 'C'` gave false, not true\"/>"),
        "{xml}"
    );
    assert!(
        xml.contains("name=\"run\">\n      <failure message=\"pass rate 0.500"),
        "{xml}"
    );

    // A failure's text is escaped, its line break kept as a reference and
    // a control character XML cannot carry dropped.
    let failing = Failing("bad <b> & \"q\"\nnext\u{1}\u{1b}[0m");
    let report = run(
        &atom,
        vec![case("a", "true")],
        &at(&root, "evals", 1.0),
        &failing,
        Options::default(),
    )
    .await;
    let xml = junit(&report);
    assert!(
        xml.contains("<error message=\"bad &lt;b&gt; &amp; &quot;q&quot;&#10;next[0m\"/>"),
        "{xml}"
    );
    assert!(
        !xml.chars().any(|c| (c as u32) < 0x20 && c != '\n'),
        "{xml}"
    );
    // A case whose atom gave no output is an error, not a failure.
    assert!(xml.contains("failures=\"1\" errors=\"1\""), "{xml}");

    // A run with no cases fails, and its JUnit says so.
    let report = run(
        &atom,
        Vec::new(),
        &at(&root, "evals", 0.0),
        &subject,
        Options::default(),
    )
    .await;
    assert!(!report.passed);
    let xml = junit(&report);
    assert!(
        xml.contains("tests=\"1\" failures=\"1\" errors=\"0\""),
        "{xml}"
    );
    assert!(xml.contains("has no eval cases to run"), "{xml}");
}

#[test]
fn a_case_file_is_strict_json_or_yaml_and_errors_point_at_the_field() {
    let (root, atom) = atom_with("{}");
    // A JSONL line that repeats a key is refused, not read for its last.
    write(
        &root.join("dup/cases.jsonl"),
        "{\"input\": {\"text\": \"a\"}, \"expect\": [\"true\"], \"expect\": [\"false\"]}\n",
    );
    let error = load_cases(&atom, &at(&root, "dup", 1.0)).expect_err("a duplicate key");
    assert!(
        error.message.contains("duplicate key \"expect\""),
        "{error}"
    );
    assert_eq!(error.line, Some(1));
    // Numbers load as written, as a YAML case's do.
    let numbers = "{\"input\": {\"text\": \"a\", \"x\": [12.5, 1e3, 123456789012345678901234567890]}, \
                   \"expect\": [\"true\"]}";
    let (jsonl, yaml) = (
        strict_json(numbers).expect("a JSON line"),
        atomic_schema::yaml::load(Path::new("case.yaml"), numbers)
            .expect("a YAML document")
            .value,
    );
    // Each is the same number either way: 1e3 keeps its JSON spelling, and
    // YAML's core schema reads it as the float 1000.0.
    let (j, y) = (&jsonl["input"]["x"], &yaml["input"]["x"]);
    for n in 0..3 {
        assert!(j[n].is_number() && y[n].is_number(), "{jsonl} {yaml}");
        assert_eq!(j[n].as_f64(), y[n].as_f64(), "{jsonl} {yaml}");
    }
    assert_eq!(j[0], json!(12.5));
    assert_eq!(j[2].to_string(), "123456789012345678901234567890");
    assert_eq!(j[2], y[2]);
    // A JSONL line that is not JSON is refused, even if YAML would read it.
    write(&root.join("loose/cases.jsonl"), "input: {text: a}\n");
    assert!(load_cases(&atom, &at(&root, "loose", 1.0)).is_err());
    // A YAML case's error is at the field at fault, not the document.
    write(
        &root.join("line/case.yaml"),
        "# a case\ninput:\n  text: ''\nexpect:\n  - \"true\"\n  - \"output.nope\"\n",
    );
    let error = load_cases(&atom, &at(&root, "line", 1.0)).expect_err("an empty text");
    assert_eq!(error.line, Some(3), "{error}");
    write(
        &root.join("line/case.yaml"),
        "input:\n  text: hi\nexpect:\n  - \"true\"\n  - \"output.nope == 1\"\n",
    );
    let error = load_cases(&atom, &at(&root, "line", 1.0)).expect_err("no field nope");
    assert_eq!(error.line, Some(5), "{error}");
    // A YAML case file ends in .yaml: one ending in .yml is not skipped.
    write(
        &root.join("yml/case.yml"),
        "input: {text: hi}\nexpect: [\"true\"]\n",
    );
    let error = load_cases(&atom, &at(&root, "yml", 1.0)).expect_err(".yml");
    assert!(error.message.contains(".yaml"), "{error}");
    // A case file has a size limit.
    std::fs::create_dir_all(root.join("big")).expect("dir");
    std::fs::File::create(root.join("big/cases.jsonl"))
        .and_then(|f| f.set_len(MAX_FILE + 1))
        .expect("a large file");
    let error = load_cases(&atom, &at(&root, "big", 1.0)).expect_err("too large");
    assert!(error.message.contains("split it"), "{error}");
}

#[test]
fn the_eval_directory_stays_inside_the_project() {
    let (root, atom) = atom_with("{evals: '../elsewhere'}");
    let outside = root.parent().expect("parent").join("elsewhere");
    write(
        &outside.join("case.yaml"),
        "input: {text: hi}\nexpect: [\"true\"]\n",
    );
    let error = super::settings(&atom, &project_at(&root)).expect_err("outside");
    assert!(error.message.contains("outside the project"), "{error}");
    let (root, atom) = atom_with(&format!("{{evals: '{}'}}", outside.display()));
    let error = super::settings(&atom, &project_at(&root)).expect_err("absolute, outside");
    assert!(error.message.contains("outside the project"), "{error}");
    // A link out of the project, as the directory or inside it, leads
    // nowhere.
    let (root, atom) = atom_with("{evals: 'linked'}");
    std::os::unix::fs::symlink(&outside, root.join("linked")).expect("link");
    assert!(super::settings(&atom, &project_at(&root)).is_err());
    let (root, atom) = atom_with("{evals: 'cases'}");
    write(
        &root.join("cases/own.yaml"),
        "input: {text: hi}\nexpect: [\"true\"]\n",
    );
    std::os::unix::fs::symlink(&outside, root.join("cases/out")).expect("link");
    std::os::unix::fs::symlink(outside.join("case.yaml"), root.join("cases/one.yaml"))
        .expect("link");
    // What git and the manifest ignore is not a case either.
    write(&root.join("cases/.gitignore"), "scratch/\n");
    write(&root.join("cases/scratch/a.yaml"), "not: a case\n");
    write(&root.join("cases/drafts/b.yaml"), "not: a case\n");
    std::fs::write(
        root.join("atomic.yaml"),
        "atomic: 1\nignore: ['cases/drafts/**']\n",
    )
    .expect("manifest");
    let settings = super::settings(&atom, &project_at(&root)).expect("inside");
    let ids: Vec<String> = load_cases(&atom, &settings)
        .expect("cases")
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(ids, ["own.yaml"]);
    // A directory the atom names must exist; the default one need not.
    let (root, atom) = atom_with("{evals: 'missing'}");
    let error = super::settings(&atom, &project_at(&root)).expect_err("missing");
    assert!(error.message.contains("missing"), "{error}");
    // The atom's types refuse an evals field it does not have.
    let root = crate::atom::tests::project("x", "{evals: {dir: 'cases', other: 1}}", None);
    let error = crate::atom::load(&root.join("test.atom.astro"), &mut Registry::new())
        .expect_err("unknown");
    assert!(error.message.contains("'other' does not exist"), "{error}");
}

#[tokio::test]
async fn a_case_fits_the_atoms_case_schema_and_an_outside_writer_has_none() {
    let (root, atom) = atom_with(SCORED);
    write(
        &root.join("meta/case.yaml"),
        "input: {text: hi}\nexpect: [\"true\"]\ncase: {ok: 'yes'}\n",
    );
    let error = load_cases(&atom, &at(&root, "meta", 1.0)).expect_err("ok is boolean");
    assert!(error.message.contains("the case metadata"), "{error}");
    let (root, atom) =
        atom_with("{kind: 'exec', exec: {command: ['true']}, effects: {external: 'idempotent'}}");
    write(
        &root.join("cases/a.yaml"),
        "input: {text: hi}\nexpect: [\"true\"]\n",
    );
    let error = load_cases(&atom, &at(&root, "cases", 1.0)).expect_err("idempotent");
    assert!(error.message.contains("never runs such an atom"), "{error}");
    // An atom with no cases may write outside.
    assert!(
        load_cases(&atom, &at(&root, "none", 1.0))
            .expect("none")
            .is_empty()
    );
    // Nor does the atom run, however its cases were come by.
    let model = FakeModel::start(Vec::new()).await.expect("fake model");
    let rt = runtimes(&model, &root.join("rt"));
    let subject = AtomSubject {
        atom: &atom,
        runtimes: &rt,
    };
    let error = subject
        .run(&case("a", "true"))
        .await
        .expect_err("never run");
    assert!(error.contains("never runs such an atom"), "{error}");
    // A scorer's tolerance is a share.
    for bad in ["2", "-0.1"] {
        let config = SCORED.replace("max_drop:0.25", &format!("max_drop:{bad}"));
        let (root, _) = atom_with("{}");
        std::fs::write(
            root.join("test.atom.astro"),
            format!(
                "---\nimport type {{ Props, Input, Output, Case, Judges }} from './.atomic/types/test';\n\
                 import type {{ AtomConfig }} from './.atomic/types/authoring';\n\
                 const {{ input }}: Props = Astro.props;\n\
                 export const config: AtomConfig<Input, Output, Case, Judges> = {config};\n---\nx\n"
            ),
        )
        .expect("source");
        let error =
            crate::atom::load(&root.join("test.atom.astro"), &mut Registry::new()).expect_err(bad);
        assert!(error.message.contains("max_drop"), "{bad}: {error}");
    }
}

#[tokio::test]
async fn a_selector_that_names_no_case_fails_the_run() {
    let (root, atom) = atom_with("{}");
    let cases = vec![case("one.yaml", "true"), case("many.jsonl:1", "true")];
    let options = || Options {
        select: vec!["one.yaml".into(), "many.jsonl".into(), "typo.yaml".into()],
        ..Options::default()
    };
    assert_eq!(unmatched(&cases, &options()), ["typo.yaml"]);
    let subject = table(&[("one.yaml", "A"), ("many.jsonl:1", "B")]);
    let report = run(&atom, cases, &at(&root, "evals", 1.0), &subject, options()).await;
    assert_eq!(report.pass_rate, 1.0);
    assert!(!report.passed, "{report:?}");
    assert!(
        report.failures[0].contains("`typo.yaml` selects no case"),
        "{report:?}"
    );
}

/// A judge that approves every text, and records each one it was given.
struct Recording(Mutex<Vec<String>>);

impl Judge for Recording {
    fn judge<'a>(&'a self, _: &'a str, input: Value) -> BoxFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            let text = input["text"].as_str().unwrap_or_default().to_owned();
            self.0.lock().expect("calls").push(text);
            Ok(json!({ "ok": true }))
        })
    }
}

#[tokio::test]
async fn scorers_score_passing_cases_against_a_comparable_baseline() {
    let (root, atom) = atom_with(SCORED);
    let settings = at(&root, "evals", 0.0);
    let cases: Vec<Case> = ["a", "b", "c"]
        .iter()
        .map(|id| case(id, "output.ticket == 'OK'"))
        .collect();
    // "b" fails its assertion: it is skipped, and its output never judged.
    let subject = table(&[("a", "OK"), ("b", "NO"), ("c", "OK")]);
    let judge = Recording(Mutex::new(Vec::new()));
    let options = |baseline| Options {
        judge: Some(&judge),
        baseline,
        ..Options::default()
    };
    let first = run(&atom, cases.clone(), &settings, &subject, options(None)).await;
    assert_eq!(first.scorers[0].scored, ["a", "c"]);
    assert_eq!(first.scorers[0].skipped, ["b"]);
    assert_eq!(*judge.0.lock().expect("calls"), ["a", "c"]);
    assert!(first.passed, "{first:?}");

    // A baseline for another atom, or over other cases, is not compared:
    // the run fails rather than pass a tolerance it never checked.
    let mut other = first.clone();
    other.atom = "other".into();
    let report = run(
        &atom,
        cases.clone(),
        &settings,
        &subject,
        options(Some(&other)),
    )
    .await;
    assert!(!report.passed);
    assert!(
        report.failures[0].contains("a report for `other`"),
        "{report:?}"
    );
    let mut fewer = first.clone();
    fewer.cases.pop();
    let report = run(
        &atom,
        cases.clone(),
        &settings,
        &subject,
        options(Some(&fewer)),
    )
    .await;
    assert!(!report.passed);
    assert!(report.failures[0].contains("ran other cases"), "{report:?}");
    // A baseline that never scored the scorer cannot check its max_drop.
    let mut unscored = first.clone();
    unscored.scorers[0].mean = None;
    let report = run(
        &atom,
        cases.clone(),
        &settings,
        &subject,
        options(Some(&unscored)),
    )
    .await;
    assert!(!report.passed);
    assert!(
        report.failures[0].contains("baseline has no mean"),
        "{report:?}"
    );
    // A comparable baseline passes.
    let report = run(
        &atom,
        cases.clone(),
        &settings,
        &subject,
        options(Some(&first)),
    )
    .await;
    assert!(report.passed, "{report:?}");

    // Without a judge the scorers do not run, and the report says so.
    let report = run(&atom, cases, &settings, &subject, Options::default()).await;
    assert!(report.scorers.is_empty());
    assert_eq!(report.not_scored, ["quality"]);
    assert!(junit(&report).contains("name=\"scorer:quality\">\n      <skipped/>"));
}

#[tokio::test]
async fn a_mean_that_misses_its_minimum_by_rounding_reaches_it() {
    // Three scores of 0.7 sum to slightly under 2.1 in binary.
    let config = SCORED
        .replace("judge.ok ? 1 : 0", "0.7")
        .replace("min_mean:0.5", "min_mean:0.7");
    let (root, atom) = atom_with(&config);
    let cases: Vec<Case> = ["a", "b", "c"].iter().map(|id| case(id, "true")).collect();
    let subject = table(&[("a", "A"), ("b", "B"), ("c", "C")]);
    let judge = Reviewer(Vec::new());
    let report = run(
        &atom,
        cases,
        &at(&root, "evals", 1.0),
        &subject,
        Options {
            judge: Some(&judge),
            ..Options::default()
        },
    )
    .await;
    assert!(report.passed, "{report:?}");
}

#[test]
fn an_assertion_reads_numbers_as_the_contracts_type_them() {
    let contracts = Contracts {
        input: json!({"type": "object", "properties": {"n": {"type": "integer"}}}),
        output: json!({"type": "object", "properties": {"half": {"type": "number"}}}),
        case: json!({}),
    };
    // `1.0` is an integer to a contract that says so, and `2` a double.
    let case = Case {
        id: "typed".into(),
        input: json!({"n": 1.0}),
        expect: vec!["input.n / 2 == 0".into(), "output.half / 4.0 == 0.5".into()],
        case: Value::Null,
        prepared: None,
        origin: "external".into(),
    };
    let failures = check(&case, &json!({"half": 2}), &contracts);
    assert!(failures.is_empty(), "{failures:?}");
}
