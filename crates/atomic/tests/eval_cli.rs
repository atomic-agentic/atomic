// SPDX-License-Identifier: Apache-2.0

//! `atomic eval` as CI runs it, against the scripted fake model:
//!
//! - `--live` calls the model and records each answer; without it, the same
//!   evals replay from the recordings with no model call, and say they are a
//!   historical check;
//! - a pass rate that drops below the atom's threshold fails the command,
//!   with the failing case in its JUnit file and its JSON report; one that
//!   stays at or above the threshold passes;
//! - an atom's scorers, which need a judge it does not have, fail it unless
//!   `--allow-unscored` reports them as skipped; `--baseline` is refused;
//! - a case selected by its identity runs alone, and with no atom named only
//!   the atoms that have it run;
//! - a call with no recording fails the eval whatever its pass rate;
//! - editing the atom's output schema makes every recording of it unusable,
//!   in plain output and in `--json`, and says to record it again;
//! - a live eval is metered in the usage ledger, and a budget with no room
//!   for a call stops it before any;
//! - a replay reads no API key and sends none;
//! - an atom that could write, or a Codex atom under cassettes, is refused;
//! - JUnit holds a suite per atom, `--report` writes the reports, and
//!   `--sample` runs a seeded sample.

use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const ATOMIC: &str = env!("CARGO_BIN_EXE_atomic");

fn scratch(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    // Not under /tmp: the project's scratch directories may not be where a
    // sandboxed turn may write.
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "atomic-eval-cli-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).expect("scratch");
    dir
}

fn object(properties: Value) -> Value {
    let required: Vec<String> = properties
        .as_object()
        .expect("properties")
        .keys()
        .cloned()
        .collect();
    json!({"type": "object", "additionalProperties": false, "required": required, "properties": properties})
}

/// A project with two lite atoms, `note`, which files a ticket for a note,
/// and `graded`, which also declares a scorer, each with eval cases.
fn project(dir: &Path, threshold: Option<f64>) -> PathBuf {
    let project = dir.join("project");
    std::fs::create_dir_all(project.join("atoms")).expect("atoms");
    std::fs::write(project.join("atomic.yaml"), "atomic: 1\nname: tickets\n").expect("manifest");
    let evals = threshold
        .map(|t| format!(", evals: {{ pass_rate: {t} }}"))
        .unwrap_or_default();
    let atoms = project.join("atoms");
    std::fs::write(
        atoms.join("note.atom.astro"),
        format!(
            "---\nimport type {{ Props, Input, Output }} from '../.atomic/types/note';\n\
             import type {{ AtomConfig }} from '../.atomic/types/authoring';\n\
             const {{ input }}: Props = Astro.props;\n\
             export const config: AtomConfig<Input, Output> = {{ description: 'File a ticket.', kind: 'lite'{evals} }};\n\
             ---\nFile a ticket for: {{input.text}}\n"
        ),
    )
    .expect("atom");
    std::fs::write(
        atoms.join("graded.atom.astro"),
        "---\nimport type { Props, Input, Output, Case, Judges } from '../.atomic/types/graded';\n\
         import type { AtomConfig } from '../.atomic/types/authoring';\n\
         const { input }: Props = Astro.props;\n\
         export const config: AtomConfig<Input, Output, Case, Judges> = { description: 'Grade a ticket.', kind: 'lite',\n\
           judges: {reviewer: {input: {type:'object',additionalProperties:false,required:['text'],properties:{text:{type:'string'}}},\n\
                               output: {type:'object',additionalProperties:false,required:['ok'],properties:{ok:{type:'boolean'}}}}},\n\
           scorers: {quality: {judge:'reviewer', with:({input}) => ({text: input.text}), score:({judge}) => judge.ok ? 1 : 0, min_mean: 0.5}} };\n\
         ---\nGrade: {input.text}\n",
    )
    .expect("atom");
    for name in ["note", "graded"] {
        for (role, schema) in [
            ("input", object(json!({"text": {"type": "string"}}))),
            ("output", object(json!({"ticket": {"type": "string"}}))),
        ] {
            std::fs::write(
                atoms.join(format!("{name}.{role}.schema.json")),
                schema.to_string(),
            )
            .expect("schema");
        }
    }
    let cases = |atom: &str, rows: &[(&str, &str)]| {
        let dir = project.join("evals").join(atom);
        std::fs::create_dir_all(&dir).expect("evals");
        let lines: Vec<String> = rows
            .iter()
            .map(|(text, assertion)| {
                json!({"input": {"text": text}, "expect": [assertion]}).to_string()
            })
            .collect();
        std::fs::write(dir.join("cases.jsonl"), lines.join("\n") + "\n").expect("cases");
    };
    cases(
        "note",
        &[
            ("Export crashes", "output.ticket == 'BUG-1'"),
            ("Add dark mode", "output.ticket.startsWith('FEAT')"),
        ],
    );
    cases("graded", &[("Slow page", "output.ticket.size() > 0")]);
    project
}

/// The fake model's answer for a request: a ticket by what the note says.
fn answer(request: &Value) -> Reply {
    let text = request.to_string();
    let ticket = if text.contains("Export crashes") {
        "BUG-1"
    } else if text.contains("dark mode") {
        "FEAT-2"
    } else {
        "PERF-3"
    };
    Reply::Text(json!({ "ticket": ticket }).to_string())
}

/// Add atom `name` with `config` (TypeScript, after `kind`) and `rows` as its
/// JSONL cases.
fn add_atom(project: &Path, name: &str, config: &str, rows: &[(&str, &str)]) {
    let atoms = project.join("atoms");
    std::fs::write(
        atoms.join(format!("{name}.atom.astro")),
        format!(
            "---\nimport type {{ Props, Input, Output }} from '../.atomic/types/{name}';\n\
             import type {{ AtomConfig }} from '../.atomic/types/authoring';\n\
             const {{ input }}: Props = Astro.props;\n\
             export const config: AtomConfig<Input, Output> = {{ description: 'Test.', {config} }};\n\
             ---\nFile a ticket for: {{input.text}}\n"
        ),
    )
    .expect("atom");
    for (role, schema) in [
        ("input", object(json!({"text": {"type": "string"}}))),
        ("output", object(json!({"ticket": {"type": "string"}}))),
    ] {
        std::fs::write(
            atoms.join(format!("{name}.{role}.schema.json")),
            schema.to_string(),
        )
        .expect("schema");
    }
    if !rows.is_empty() {
        let dir = project.join("evals").join(name);
        std::fs::create_dir_all(&dir).expect("evals");
        let lines: Vec<String> = rows
            .iter()
            .map(|(text, assertion)| {
                json!({"input": {"text": text}, "expect": [assertion]}).to_string()
            })
            .collect();
        std::fs::write(dir.join("cases.jsonl"), lines.join("\n") + "\n").expect("cases");
    }
}

/// Run `atomic eval` off the test's runtime thread.
async fn eval_async(dir: &Path, project: &Path, url: &str, args: &[&str]) -> Output {
    let (d, p, u) = (dir.to_owned(), project.to_owned(), url.to_owned());
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    tokio::task::spawn_blocking(move || {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        eval(&d, &p, &u, &args)
    })
    .await
    .expect("eval")
}

fn eval(dir: &Path, project: &Path, url: &str, args: &[&str]) -> Output {
    eval_with(dir, project, url, args, &[])
}

fn eval_with(dir: &Path, project: &Path, url: &str, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(ATOMIC);
    cmd.arg("eval")
        .args(args)
        .current_dir(project)
        .env("HOME", dir.join("home"))
        .env("ATOMIC_HOME", dir.join("atomic-home"))
        .env("USER", "dev")
        .env_remove("ATOMIC_MODEL_API_KEY_ENV")
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_HOME")
        .env_remove("ATOMIC_DATABASE_URL")
        // Nothing listens on the discard port, so detection never finds a
        // local Ollama and an endpoint comes only from what a test sets.
        .env("ATOMIC_DETECT_OLLAMA_ADDR", "127.0.0.1:9")
        .stdin(Stdio::null());
    // With no URL, the endpoint comes from the project's files.
    if url.is_empty() {
        cmd.env_remove("ATOMIC_MODEL_BASE_URL")
            .env_remove("ATOMIC_MODEL");
    } else {
        cmd.env("ATOMIC_MODEL_BASE_URL", url)
            .env("ATOMIC_MODEL", "synthetic");
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("atomic eval")
}

fn text(out: &Output) -> String {
    format!(
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn atomic_eval_records_replays_offline_and_fails_ci_on_a_pass_rate_drop() {
    on_codex_runtime(async {
        let dir = scratch("ci");
        let project = project(&dir, None);
        let model = FakeModel::respond(answer).await.expect("fake model");
        let url = model.base_url();

        // Recorded live: every atom with cases, three model calls.
        let (d, p, u) = (dir.clone(), project.clone(), url.clone());
        let live =
            tokio::task::spawn_blocking(move || eval(&d, &p, &u, &["--live", "--allow-unscored"]))
                .await
                .expect("eval");
        assert!(live.status.success(), "{}", text(&live));
        assert_eq!(model.calls(), 3);
        let out = String::from_utf8_lossy(&live.stdout);
        assert!(out.contains("PASS note: 2 of 2 cases passed"), "{out}");
        // Scorers are reported as not run: there is no judge.
        assert!(
            out.contains("scorers not run (quality)"),
            "the skipped scorer is named: {out}"
        );

        // Replayed: the same results, no model call, and labelled.
        let (d, p, u) = (dir.clone(), project.clone(), url.clone());
        let replayed = tokio::task::spawn_blocking(move || eval(&d, &p, &u, &["--allow-unscored"]))
            .await
            .expect("eval");
        assert!(replayed.status.success(), "{}", text(&replayed));
        assert_eq!(model.calls(), 3, "the replay made no model call");
        let out = String::from_utf8_lossy(&replayed.stdout);
        assert!(out.contains("a historical check"), "{out}");
        assert!(
            out.contains("2 replayed, 0 recorded, 0 without a recording"),
            "{out}"
        );

        // A recorded answer that no longer satisfies its case, once the
        // case expects something else, fails it on replay with no call.
        let cases = project.join("evals/note/cases.jsonl");
        let original = std::fs::read_to_string(&cases).expect("cases");
        std::fs::write(
            &cases,
            original.replace("output.ticket == 'BUG-1'", "output.ticket == 'BUG-2'"),
        )
        .expect("cases");
        let out = eval_async(&dir, &project, &url, &["note", "--json"]).await;
        assert!(!out.status.success(), "{}", text(&out));
        let report: Value = serde_json::from_slice(&out.stdout).expect("one JSON report");
        assert_eq!(report["cassettes"][0]["replayed"], 2, "{report}");
        assert_eq!(report["cassettes"][0]["missed"], 0, "{report}");
        assert!(
            report["reports"][0]["cases"][0]["failures"][0]
                .as_str()
                .is_some_and(|f| f.contains("BUG-2")),
            "{report}"
        );
        assert_eq!(model.calls(), 3);
        std::fs::write(&cases, original).expect("cases");

        // A regression: a case the recorded answers do not satisfy drops the
        // pass rate below the atom's threshold, and CI fails.
        let cases = project.join("evals/note/cases.jsonl");
        let mut rows = std::fs::read_to_string(&cases).expect("cases");
        rows.push_str(
            &(json!({"input": {"text": "Export crashes"}, "expect": ["output.ticket == 'BUG-9'"]})
                .to_string()
                + "\n"),
        );
        std::fs::write(&cases, rows).expect("cases");
        let junit = dir.join("evals.xml");
        let (d, p, u, j) = (
            dir.clone(),
            project.clone(),
            url.clone(),
            junit.display().to_string(),
        );
        let failed = tokio::task::spawn_blocking(move || {
            eval(&d, &p, &u, &["note", "--junit", &j, "--json"])
        })
        .await
        .expect("eval");
        assert!(!failed.status.success(), "{}", text(&failed));
        let report: Value = serde_json::from_slice(&failed.stdout).expect("one JSON report");
        assert_eq!(report["passed"], false);
        assert_eq!(report["source"], "recorded");
        assert_eq!(report["reports"][0]["cases"][2]["passed"], false);
        let xml = std::fs::read_to_string(&junit).expect("junit");
        assert!(xml.contains("name=\"cases.jsonl:3\""), "{xml}");
        assert!(
            xml.contains(
                "name=\"run\">\n      <failure message=\"pass rate 0.667 (2 of 3) is below the \
                 threshold 1.000\"/>"
            ),
            "{xml}"
        );
        assert_eq!(model.calls(), 3);

        // A case selected by its identity runs alone, and passes.
        let (d, p, u) = (dir.clone(), project.clone(), url.clone());
        let one = tokio::task::spawn_blocking(move || {
            eval(&d, &p, &u, &["note", "--case", "cases.jsonl:1"])
        })
        .await
        .expect("eval");
        assert!(one.status.success(), "{}", text(&one));
        assert!(
            String::from_utf8_lossy(&one.stdout).contains("1 of 1 cases passed"),
            "{}",
            text(&one)
        );
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn a_pass_rate_exactly_at_its_threshold_passes() {
    on_codex_runtime(async {
        let dir = scratch("threshold");
        // Two of four cases may pass, and exactly two do.
        let project = project(&dir, Some(0.5));
        let cases = project.join("evals/note/cases.jsonl");
        let mut rows = std::fs::read_to_string(&cases).expect("cases");
        for wrong in ["BUG-9", "BUG-8"] {
            rows.push_str(
                &(json!({"input": {"text": "Export crashes"}, "expect": [format!("output.ticket == '{wrong}'")]})
                    .to_string()
                    + "\n"),
            );
        }
        std::fs::write(&cases, rows).expect("cases");
        let model = FakeModel::respond(answer).await.expect("fake model");
        let url = model.base_url();
        let out = eval_async(&dir, &project, &url, &["note", "--live"]).await;
        assert!(out.status.success(), "{}", text(&out));
        assert!(
            String::from_utf8_lossy(&out.stdout)
                .contains("PASS note: 2 of 4 cases passed, pass rate 0.500, threshold 0.500"),
            "{}",
            text(&out)
        );
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn declared_scorers_fail_the_atom_unless_allowed_and_a_baseline_is_refused() {
    on_codex_runtime(async {
        let dir = scratch("scorers");
        let project = project(&dir, None);
        let model = FakeModel::respond(answer).await.expect("fake model");
        let url = model.base_url();
        let out = eval_async(&dir, &project, &url, &["graded", "--no-cassettes"]).await;
        assert!(!out.status.success(), "{}", text(&out));
        assert!(
            text(&out).contains("pass --allow-unscored"),
            "{}",
            text(&out)
        );
        let junit = dir.join("graded.xml");
        let j = junit.display().to_string();
        let out = eval_async(
            &dir,
            &project,
            &url,
            &[
                "graded",
                "--no-cassettes",
                "--allow-unscored",
                "--junit",
                &j,
            ],
        )
        .await;
        assert!(out.status.success(), "{}", text(&out));
        let xml = std::fs::read_to_string(&junit).expect("junit");
        assert!(xml.contains("skipped=\"1\""), "{xml}");
        // A baseline cannot be compared without scorer means.
        let report = dir.join("report.json");
        std::fs::write(&report, "[]").expect("report");
        let r = report.display().to_string();
        let out = eval_async(&dir, &project, &url, &["note", "--baseline", &r]).await;
        assert!(!out.status.success());
        assert!(
            text(&out).contains("no scorer mean to compare"),
            "{}",
            text(&out)
        );
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn a_call_with_no_recording_fails_whatever_the_pass_rate() {
    on_codex_runtime(async {
        let dir = scratch("missing");
        let project = project(&dir, Some(0.5));
        let model = FakeModel::respond(answer).await.expect("fake model");
        let url = model.base_url();
        let out = eval_async(&dir, &project, &url, &["note", "--live"]).await;
        assert!(out.status.success(), "{}", text(&out));
        // A new case, never recorded: two of three still pass, above 0.5.
        let cases = project.join("evals/note/cases.jsonl");
        let mut rows = std::fs::read_to_string(&cases).expect("cases");
        rows.push_str(
            &(json!({"input": {"text": "A brand new note"}, "expect": ["true"]}).to_string()
                + "\n"),
        );
        std::fs::write(&cases, rows).expect("cases");
        let out = eval_async(&dir, &project, &url, &["note", "--json"]).await;
        assert!(!out.status.success(), "{}", text(&out));
        let report: Value = serde_json::from_slice(&out.stdout).expect("one JSON report");
        assert_eq!(report["cassettes"][0]["missed"], 1);
        assert!(
            report["reports"][0]["failures"]
                .to_string()
                .contains("no usable recording"),
            "{report}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn a_live_eval_is_metered_and_stopped_by_its_budget() {
    on_codex_runtime(async {
        let dir = scratch("metered");
        let project = project(&dir, None);
        let model = FakeModel::respond(answer).await.expect("fake model");
        let url = model.base_url();
        // Without cassettes: the model is called, nothing is recorded, and
        // every call is in the ledger.
        let out = eval_async(&dir, &project, &url, &["note", "--no-cassettes", "--json"]).await;
        assert!(out.status.success(), "{}", text(&out));
        assert_eq!(model.calls(), 2);
        assert!(!project.join("evals/note/cassettes").exists());
        let report: Value = serde_json::from_slice(&out.stdout).expect("one JSON report");
        assert_eq!(report["source"], "live");
        assert_eq!(report["usage"][0]["calls"], 2, "{report}");
        assert!(
            report["usage"][0]["tokens"].as_i64().unwrap_or(0) > 0,
            "{report}"
        );
        // Without its ledger, a live eval does not start.
        let (d, p, u) = (dir.clone(), project.clone(), url.clone());
        let unreachable = [
            (
                "ATOMIC_DATABASE_URL",
                "postgres://atomic@127.0.0.1:9/atomic",
            ),
            ("ATOMIC_EXECUTOR_ID", "eval-test"),
        ];
        let out = tokio::task::spawn_blocking(move || {
            eval_with(&d, &p, &u, &["note", "--no-cassettes"], &unreachable)
        })
        .await
        .expect("eval");
        assert!(!out.status.success(), "{}", text(&out));
        assert!(text(&out).contains("usage ledger"), "{}", text(&out));
        assert_eq!(model.calls(), 2, "no call was made");
        // A budget with no room for one call stops the eval before any.
        add_atom(
            &project,
            "capped",
            "kind: 'lite', budget: { tokens: 1000 }",
            &[("Export crashes", "true")],
        );
        let out = eval_async(&dir, &project, &url, &["capped", "--live"]).await;
        assert!(!out.status.success(), "{}", text(&out));
        assert!(text(&out).contains("atom capped: budget"), "{}", text(&out));
        assert_eq!(model.calls(), 2, "no call was made");
        // Every budget is resolved before any call: an atom after it that
        // could run makes none either.
        let out = eval_async(&dir, &project, &url, &["note", "capped", "--live"]).await;
        assert!(!out.status.success(), "{}", text(&out));
        assert_eq!(model.calls(), 2, "no call was made");
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn a_replay_reads_no_api_key_and_sends_none() {
    on_codex_runtime(async {
        let dir = scratch("keyless");
        let project = project(&dir, None);
        let model = FakeModel::respond(answer).await.expect("fake model");
        let url = model.base_url();
        let secret = [
            ("ATOMIC_MODEL_API_KEY_ENV", "EVAL_TEST_KEY"),
            ("EVAL_TEST_KEY", "sk-test-secret"),
        ];
        let (d, p, u) = (dir.clone(), project.clone(), url.clone());
        let live = tokio::task::spawn_blocking(move || {
            eval_with(&d, &p, &u, &["note", "--live"], &secret)
        })
        .await
        .expect("eval");
        assert!(live.status.success(), "{}", text(&live));
        let (d, p, u) = (dir.clone(), project.clone(), url.clone());
        let replay = tokio::task::spawn_blocking(move || {
            eval_with(&d, &p, &u, &["note", "--json"], &secret)
        })
        .await
        .expect("eval");
        assert!(replay.status.success(), "{}", text(&replay));
        let report: Value = serde_json::from_slice(&replay.stdout).expect("one JSON report");
        assert_eq!(report["cassettes"][0]["replayed"], 2, "{report}");
        assert_eq!(
            report["cassettes"][0]["keyed"], 0,
            "a key reached the replay: {report}"
        );
        for entry in std::fs::read_dir(project.join("evals/note/cassettes")).expect("cassettes") {
            let tape = std::fs::read_to_string(entry.expect("entry").path()).expect("tape");
            assert!(!tape.contains("sk-test-secret"), "{tape}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    });
}

/// A ChatGPT sign-in in the file store of the configuration directory
/// `config`, outside the checkout (atomic refuses a sign-in the repository
/// could have supplied), with the variables that choose it; state stays in
/// `dir`, outside /tmp.
fn signed_in(dir: &Path, config: &Path) -> Vec<(String, String)> {
    let auth = config.join("atomic/credentials/codex-auth");
    std::fs::create_dir_all(&auth).expect("auth home");
    let id_token = atomic_core::testing::fake_id_token("dev@example.invalid", "account-1");
    std::fs::write(
        auth.join("auth.json"),
        json!({"auth_mode": "chatgpt", "OPENAI_API_KEY": null, "tokens": {
            "id_token": id_token, "access_token": "synthetic-access-token",
            "refresh_token": "synthetic-refresh-token", "account_id": "account-1"},
            "last_refresh": "2099-01-01T00:00:00Z"})
        .to_string(),
    )
    .expect("auth.json");
    vec![
        // Empty is unset: state and credentials in the XDG directories.
        ("ATOMIC_HOME".into(), String::new()),
        (
            "XDG_DATA_HOME".into(),
            dir.join("data").display().to_string(),
        ),
        ("XDG_CONFIG_HOME".into(), config.display().to_string()),
        ("ATOMIC_CREDENTIAL_STORE".into(), "file".into()),
    ]
}

#[test]
fn signed_in_a_replay_still_finds_what_detection_recorded_and_a_shared_ledger_refuses_it() {
    on_codex_runtime(async {
        let dir = scratch("signed-in");
        let project = project(&dir, None);
        let model = FakeModel::respond(answer).await.expect("fake model");
        // Detected: the fake answers Ollama's model list.
        let addr = model
            .base_url()
            .trim_start_matches("http://")
            .trim_end_matches("/v1")
            .to_owned();
        let detect = ("ATOMIC_DETECT_OLLAMA_ADDR".to_owned(), addr);
        let (d, p, e) = (dir.clone(), project.clone(), detect.clone());
        let live = tokio::task::spawn_blocking(move || {
            eval_with(&d, &p, "", &["note", "--live"], &[(&e.0, &e.1)])
        })
        .await
        .expect("eval");
        assert!(live.status.success(), "{}", text(&live));
        let recorded = model.calls();
        // Signed in now: a replay reads no sign-in, and finds the recording.
        let home =
            std::env::temp_dir().join(format!("atomic-eval-signed-in-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let mut env = signed_in(&dir, &home);
        env.push(detect.clone());
        env.push(("ATOMIC_MODEL".into(), "synthetic".into()));
        let (d, p, e) = (dir.clone(), project.clone(), env.clone());
        let replay = tokio::task::spawn_blocking(move || {
            let e: Vec<(&str, &str)> = e.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            eval_with(&d, &p, "", &["note", "--json"], &e)
        })
        .await
        .expect("eval");
        assert!(replay.status.success(), "{}", text(&replay));
        let report: Value = serde_json::from_slice(&replay.stdout).expect("one JSON report");
        assert_eq!(report["cassettes"][0]["replayed"], 2, "{report}");
        assert_eq!(model.calls(), recorded, "a replay called the model");
        // A live eval metered in a shared ledger refuses the sign-in.
        env.push((
            "ATOMIC_DATABASE_URL".into(),
            "postgres://atomic:atomic@127.0.0.1:9/atomic".into(),
        ));
        env.push(("ATOMIC_EXECUTOR_ID".into(), "host-1".into()));
        let (d, p, e) = (dir.clone(), project.clone(), env.clone());
        let shared = tokio::task::spawn_blocking(move || {
            let e: Vec<(&str, &str)> = e.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            eval_with(&d, &p, "", &["note", "--live"], &e)
        })
        .await
        .expect("eval");
        assert!(!shared.status.success(), "{}", text(&shared));
        assert!(
            text(&shared).contains("a sign-in is personal"),
            "{}",
            text(&shared)
        );
        assert!(
            text(&shared).contains("the store is shared"),
            "{}",
            text(&shared)
        );
        assert_eq!(model.calls(), recorded, "a refused eval called the model");
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn a_replay_needs_the_endpoint_it_was_recorded_for_and_no_key() {
    on_codex_runtime(async {
        let dir = scratch("identity");
        let project = project(&dir, None);
        let model = FakeModel::respond(answer).await.expect("fake model");
        let url = model.base_url();
        let out = eval_async(&dir, &project, &url, &["note", "--live"]).await;
        assert!(out.status.success(), "{}", text(&out));
        // The project's files now choose the endpoint, and the person's key
        // is named for it: a key atomic withholds from such an endpoint.
        std::fs::write(
            project.join("atomic.yaml"),
            format!("atomic: 1\nname: tickets\nmodel:\n  base_url: {url}\n  name: synthetic\n"),
        )
        .expect("manifest");
        let key = [
            ("ATOMIC_MODEL_API_KEY_ENV", "MY_KEY"),
            ("MY_KEY", "sk-withheld"),
        ];
        // A replay reads no key, so the withheld one does not stop it.
        let (d, p) = (dir.clone(), project.clone());
        let replay =
            tokio::task::spawn_blocking(move || eval_with(&d, &p, "", &["note", "--json"], &key))
                .await
                .expect("eval");
        assert!(replay.status.success(), "{}", text(&replay));
        let report: Value = serde_json::from_slice(&replay.stdout).expect("one JSON report");
        assert_eq!(report["cassettes"][0]["replayed"], 2, "{report}");
        assert_eq!(report["cassettes"][0]["keyed"], 0, "{report}");
        // A live eval calls the endpoint as a run would, and is refused.
        let (d, p) = (dir.clone(), project.clone());
        let live =
            tokio::task::spawn_blocking(move || eval_with(&d, &p, "", &["note", "--live"], &key))
                .await
                .expect("eval");
        assert!(!live.status.success(), "{}", text(&live));
        assert!(text(&live).contains("is not used"), "{}", text(&live));
        assert_eq!(model.calls(), 2, "no call after the recording");
        // Without any endpoint, a replay says what it needs.
        std::fs::write(project.join("atomic.yaml"), "atomic: 1\nname: tickets\n")
            .expect("manifest");
        let (d, p) = (dir.clone(), project.clone());
        let none = tokio::task::spawn_blocking(move || eval_with(&d, &p, "", &["note"], &[]))
            .await
            .expect("eval");
        assert!(!none.status.success(), "{}", text(&none));
        assert!(text(&none).contains("recorded for"), "{}", text(&none));
        let _ = std::fs::remove_dir_all(&dir);
    });
}

/// What a recording answers is the whole request it was made for, the
/// generation schema among it: `note`'s recordings are recorded once, and
/// then each of two edits to its output schema — one property's description,
/// and a property added — leaves every case without a recording, in plain
/// output and in `--json`, with no model call to fill the gap. The schema
/// back as it was, the same recordings answer again.
#[test]
fn an_edited_output_schema_makes_a_recording_unusable_and_says_to_record_again() {
    on_codex_runtime(async {
        let dir = scratch("schema-edit");
        let project = project(&dir, None);
        let model = FakeModel::respond(answer).await.expect("fake model");
        let url = model.base_url();

        // Recorded live: `note`'s two cases, two calls.
        let out = eval_async(&dir, &project, &url, &["note", "--live"]).await;
        assert!(out.status.success(), "{}", text(&out));
        let recorded = model.calls();
        assert_eq!(recorded, 2, "one call per case");

        // The control: with nothing edited, the same eval is answered whole
        // from those recordings, and says it is a historical check.
        let out = eval_async(&dir, &project, &url, &["note"]).await;
        assert!(out.status.success(), "{}", text(&out));
        let said = text(&out);
        assert!(said.contains("a historical check"), "{said}");
        assert!(
            said.contains("PASS note: 2 of 2 cases passed"),
            "the recording is usable: {said}"
        );
        assert!(
            said.contains("2 replayed, 0 recorded, 0 without a recording"),
            "{said}"
        );
        assert_eq!(model.calls(), recorded, "a replay called the model");

        let schema = project.join("atoms/note.output.schema.json");
        let original = std::fs::read_to_string(&schema).expect("the output schema");
        let contract: Value = serde_json::from_str(&original).expect("the schema is JSON");
        // Neither edit changes what the recorded answers say: one writes a
        // property's description, the other adds an optional property.
        let mut described = contract.clone();
        described["properties"]["ticket"]["description"] = json!("The ticket's own ID.");
        let mut widened = contract;
        widened["properties"]["severity"] = json!({"type": "string"});

        for edited in [described, widened] {
            std::fs::write(&schema, edited.to_string()).expect("the edited schema");
            // Plain: each case's failure names the miss, and the eval says
            // how many calls had no recording and how to record them.
            let out = eval_async(&dir, &project, &url, &["note"]).await;
            assert!(
                !out.status.success(),
                "the edit was accepted: {}",
                text(&out)
            );
            let said = text(&out);
            for part in [
                "no recording answers this request",
                "the prompt, schema, model or settings differ from every recorded one",
                "run the eval with --live to record it",
                "2 model call(s) had no usable recording (2 missing, 0 corrupt): \
                 record them with --live",
            ] {
                assert!(said.contains(part), "{part}: {said}");
            }
            // `--json`: the same miss in the report, and every recording of
            // the edited atom counted as missing, none replayed.
            let out = eval_async(&dir, &project, &url, &["note", "--json"]).await;
            assert!(!out.status.success(), "{}", text(&out));
            let report: Value = serde_json::from_slice(&out.stdout).expect("one JSON report");
            assert_eq!(report["passed"], false, "{report}");
            assert_eq!(report["source"], "recorded", "{report}");
            assert_eq!(report["cassettes"][0]["replayed"], 0, "{report}");
            assert_eq!(report["cassettes"][0]["missed"], 2, "{report}");
            assert_eq!(
                report["cassettes"][0]["corrupt"], 0,
                "a miss read as corrupt: {report}"
            );
            let cases = report["reports"][0]["cases"].as_array().expect("two cases");
            assert_eq!(cases.len(), 2, "{report}");
            for case in cases {
                assert_eq!(case["passed"], false, "{case}");
                let failures = case["failures"]
                    .as_array()
                    .expect("the failed case names its failures");
                assert!(
                    failures
                        .iter()
                        .filter_map(Value::as_str)
                        .any(|f| f.contains("run the eval with --live to record it")),
                    "the case says to record again: {case}"
                );
            }
            // The schema back as it was: those same recordings answer, which
            // is what shows the edit alone made them useless.
            std::fs::write(&schema, &original).expect("the schema back");
            let out = eval_async(&dir, &project, &url, &["note", "--json"]).await;
            assert!(out.status.success(), "{}", text(&out));
            let report: Value = serde_json::from_slice(&out.stdout).expect("one JSON report");
            assert_eq!(report["cassettes"][0]["replayed"], 2, "{report}");
            assert_eq!(report["cassettes"][0]["missed"], 0, "{report}");
        }
        assert_eq!(model.calls(), recorded, "a miss reached the model");
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn atoms_it_cannot_run_yet_are_listed_and_the_rest_run() {
    on_codex_runtime(async {
        let dir = scratch("refused");
        let project = project(&dir, None);
        let row = [("Export crashes", "true")];
        add_atom(
            &project,
            "writer",
            "kind: 'codex', sandbox: 'workspace-write'",
            &row,
        );
        add_atom(&project, "reader", "kind: 'codex'", &row);
        add_atom(
            &project,
            "undeclared",
            "kind: 'exec', exec: { command: ['true'] }",
            &row,
        );
        add_atom(
            &project,
            "sender",
            "kind: 'exec', exec: { command: ['true'] }, effects: { external: 'idempotent' }",
            &[],
        );
        add_atom(
            &project,
            "lookup",
            "kind: 'exec', exec: { command: ['true'] }, effects: { external: 'read-only' }",
            &row,
        );
        let model = FakeModel::respond(answer).await.expect("fake model");
        let url = model.base_url();
        for (atom, args, says) in [
            ("writer", &["--no-cassettes"][..], "may write files"),
            ("undeclared", &[][..], "declares no effects"),
            ("sender", &[][..], "writes to an outside system"),
            ("reader", &[][..], "is a Codex atom"),
            ("reader", &["--live"][..], "is a Codex atom"),
        ] {
            let mut all = vec![atom];
            all.extend_from_slice(args);
            let out = eval_async(&dir, &project, &url, &all).await;
            assert!(!out.status.success(), "{atom} {args:?}: {}", text(&out));
            assert!(text(&out).contains(says), "{atom}: {}", text(&out));
            assert!(
                text(&out).contains(&format!("REFUSED {atom}")),
                "{}",
                text(&out)
            );
        }
        // An exec atom declared to only read is not refused.
        let out = eval_async(&dir, &project, &url, &["lookup", "--json"]).await;
        let report: Value = serde_json::from_slice(&out.stdout).expect("one JSON report");
        assert_eq!(report["refused"], json!([]), "{report}");
        assert_eq!(report["reports"][0]["atom"], "lookup");

        // Refused atoms are listed with their reasons; the others run, and
        // the command fails while any is refused, unless told to skip them.
        let junit = dir.join("mixed.xml");
        let j = junit.display().to_string();
        let mixed = [
            "note",
            "writer",
            "undeclared",
            "--live",
            "--junit",
            j.as_str(),
        ];
        let out = eval_async(&dir, &project, &url, &mixed).await;
        assert!(!out.status.success(), "{}", text(&out));
        let said = text(&out);
        assert!(said.contains("PASS note"), "{said}");
        assert!(
            said.contains("REFUSED writer") && said.contains("REFUSED undeclared"),
            "{said}"
        );
        let xml = std::fs::read_to_string(&junit).expect("junit");
        assert!(
            xml.contains("<testsuite name=\"writer\" tests=\"1\" failures=\"1\""),
            "{xml}"
        );
        let mut skipping = mixed.to_vec();
        skipping.push("--skip-refused");
        let out = eval_async(&dir, &project, &url, &skipping).await;
        assert!(out.status.success(), "{}", text(&out));
        assert!(text(&out).contains("SKIPPED writer"), "{}", text(&out));
        let xml = std::fs::read_to_string(&junit).expect("junit");
        assert!(
            xml.contains(
                "<testsuite name=\"writer\" tests=\"1\" failures=\"0\" errors=\"0\" skipped=\"1\""
            ),
            "{xml}"
        );
        assert_eq!(model.calls(), 4, "only note's cases reached the model");
        // Skipping every atom chosen is no pass: something must run.
        let out = eval_async(
            &dir,
            &project,
            &url,
            &["writer", "undeclared", "--skip-refused"],
        )
        .await;
        assert!(!out.status.success(), "{}", text(&out));
        assert!(
            text(&out).contains("can run none of the atoms chosen"),
            "{}",
            text(&out)
        );
        // With no atom named, one that writes outside and has cases is listed
        // with the refused, and the eval goes on.
        let sender_cases = project.join("evals/sender");
        std::fs::create_dir_all(&sender_cases).expect("evals");
        std::fs::write(
            sender_cases.join("cases.jsonl"),
            json!({"input": {"text": "Export crashes"}, "expect": ["true"]}).to_string() + "\n",
        )
        .expect("cases");
        let out = eval_async(&dir, &project, &url, &["--json", "--allow-unscored"]).await;
        let report: Value = serde_json::from_slice(&out.stdout).expect("one JSON report");
        assert!(
            report["refused"].to_string().contains("\"sender\""),
            "{report}"
        );
        assert!(
            report["reports"]
                .as_array()
                .is_some_and(|r| r.iter().any(|r| r["atom"] == "note")),
            "{report}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn selection_sampling_reports_and_junit_cover_every_atom() {
    on_codex_runtime(async {
        let dir = scratch("select");
        let project = project(&dir, None);
        add_atom(&project, "silent", "kind: 'lite'", &[]);
        let model = FakeModel::respond(answer).await.expect("fake model");
        let url = model.base_url();
        let junit = dir.join("all.xml");
        let report = dir.join("reports.json");
        let (j, r) = (junit.display().to_string(), report.display().to_string());
        let out = eval_async(
            &dir,
            &project,
            &url,
            &["--live", "--allow-unscored", "--junit", &j, "--report", &r],
        )
        .await;
        assert!(out.status.success(), "{}", text(&out));
        // One suite per atom with cases; the atom with none is named.
        let xml = std::fs::read_to_string(&junit).expect("junit");
        assert!(xml.contains("<testsuite name=\"graded\""), "{xml}");
        assert!(xml.contains("<testsuite name=\"note\""), "{xml}");
        assert!(
            text(&out).contains("no eval cases: silent"),
            "{}",
            text(&out)
        );
        let reports: Vec<atomic_core::eval::Report> =
            serde_json::from_str(&std::fs::read_to_string(&report).expect("report"))
                .expect("reports");
        assert_eq!(reports.len(), 2);

        // With no atom named, a case selects only the atoms that have it:
        // only `note` has a second line.
        let out = eval_async(&dir, &project, &url, &["--case", "cases.jsonl:2", "--json"]).await;
        assert!(out.status.success(), "{}", text(&out));
        let report: Value = serde_json::from_slice(&out.stdout).expect("one JSON report");
        assert_eq!(
            report["reports"].as_array().map(Vec::len),
            Some(1),
            "{report}"
        );
        assert_eq!(report["reports"][0]["atom"], "note");
        // A case no atom has is an error.
        let out = eval_async(&dir, &project, &url, &["--case", "cases.jsonl:9"]).await;
        assert!(!out.status.success());
        assert!(text(&out).contains("names no eval case"), "{}", text(&out));

        // A seeded sample of one.
        let out = eval_async(
            &dir,
            &project,
            &url,
            &["note", "--sample", "1", "--seed", "3"],
        )
        .await;
        assert!(out.status.success(), "{}", text(&out));
        assert!(text(&out).contains("1 of 1 cases passed"), "{}", text(&out));

        // A failing eval without --json exits non-zero and says why.
        let cases = project.join("evals/note/cases.jsonl");
        let mut rows = std::fs::read_to_string(&cases).expect("cases");
        rows.push_str(
            &(json!({"input": {"text": "Export crashes"}, "expect": ["output.ticket == 'BUG-9'"]})
                .to_string()
                + "\n"),
        );
        std::fs::write(&cases, rows).expect("cases");
        let out = eval_async(&dir, &project, &url, &["note"]).await;
        assert_eq!(out.status.code(), Some(1), "{}", text(&out));
        assert!(
            text(&out).contains("the evals of note did not pass"),
            "{}",
            text(&out)
        );
        let _ = std::fs::remove_dir_all(&dir);
    });
}

/// What a model answered reaches no terminal raw through `eval --json`,
/// and reads back as the same text.
#[test]
fn an_evals_hostile_answer_reaches_no_terminal_raw() {
    on_codex_runtime(async {
        let dir = scratch("hostile-eval");
        let project = project(&dir, None);
        let ticket = "BUG\u{1b}[2J\u{7f}\u{9b}\u{202e}\u{E0041}";
        let model =
            FakeModel::respond(move |_| Reply::Text(json!({ "ticket": ticket }).to_string()))
                .await
                .expect("fake model");
        let url = model.base_url();
        let out = eval_async(&dir, &project, &url, &["note", "--live", "--json"]).await;
        let raw = String::from_utf8_lossy(&out.stdout).into_owned();
        for c in ['\u{1b}', '\u{7f}', '\u{9b}', '\u{202e}', '\u{E0041}'] {
            assert!(
                !raw.contains(c),
                "U+{:04X} reached the terminal",
                u32::from(c)
            );
        }
        let report: Value = serde_json::from_str(&raw).expect("one JSON report");
        assert!(report.to_string().contains("BUG"), "{report}");
        assert_eq!(
            report["reports"][0]["cases"][0]["output"]["ticket"], ticket,
            "the answer read back whole"
        );
    });
}

#[test]
fn a_plain_evals_hostile_answer_reaches_no_terminal_raw() {
    on_codex_runtime(async {
        let dir = scratch("hostile-plain-eval");
        let project = project(&dir, None);
        // An assertion that gives the answer itself back fails, and its
        // failure quotes what the model wrote.
        std::fs::write(
            project.join("evals").join("note").join("cases.jsonl"),
            json!({"input": {"text": "Export crashes"}, "expect": ["output.ticket"]}).to_string()
                + "\n",
        )
        .expect("cases");
        let ticket = "BUG\u{1b}[2J\u{7f}\u{9b}\u{202e}\u{E0041}";
        let model =
            FakeModel::respond(move |_| Reply::Text(json!({ "ticket": ticket }).to_string()))
                .await
                .expect("fake model");
        let url = model.base_url();
        let out = eval_async(&dir, &project, &url, &["note", "--live"]).await;
        let raw = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        for c in ['\u{1b}', '\u{7f}', '\u{9b}', '\u{202e}', '\u{E0041}'] {
            assert!(
                !raw.contains(c),
                "U+{:04X} reached the terminal",
                u32::from(c)
            );
        }
        assert!(raw.contains("failed"), "the case failed: {raw:?}");
        assert!(
            raw.contains("\\u{007f}") && raw.contains("\\u{202e}"),
            "the answer shown escaped: {raw:?}"
        );
    });
}

#[test]
fn a_team_atom_refuses_a_case_of_the_default_origin_before_any_call() {
    on_codex_runtime(async {
        let dir = scratch("case-origin");
        let project = project(&dir, None);
        // An atom that takes only team input, and a case that does not say
        // where its input came from: external, by default.
        add_atom(
            &project,
            "reviewed",
            "trust: 'team'",
            &[("Export crashes", "output.ticket.size() > 0")],
        );
        let model = FakeModel::respond(answer).await.expect("fake model");
        let out = eval_async(
            &dir,
            &project,
            &model.base_url(),
            &["reviewed", "--no-cassettes"],
        )
        .await;
        assert!(!out.status.success(), "{}", text(&out));
        let said = text(&out);
        for part in [
            "cases.jsonl:1",
            "is external data",
            "atom `reviewed` accepts team input at most",
        ] {
            assert!(said.contains(part), "{part}: {said}");
        }
        assert_eq!(model.calls(), 0, "a model was called");
    });
}

#[test]
fn naming_several_atoms_refuses_an_untrusted_case_before_any_atom_runs() {
    on_codex_runtime(async {
        let dir = scratch("case-origin-several");
        let project = project(&dir, None);
        // `note` comes first and would run live; `reviewed` takes only team
        // input and has a case of the default origin.
        add_atom(
            &project,
            "reviewed",
            "trust: 'team'",
            &[("Export crashes", "output.ticket.size() > 0")],
        );
        let model = FakeModel::respond(answer).await.expect("fake model");
        let out = eval_async(
            &dir,
            &project,
            &model.base_url(),
            &["note", "reviewed", "--live"],
        )
        .await;
        assert!(!out.status.success(), "{}", text(&out));
        assert!(
            text(&out).contains("atom `reviewed` accepts team input at most"),
            "{}",
            text(&out)
        );
        // Refused before `note` ran: no call, and nothing recorded for it.
        assert_eq!(model.calls(), 0, "an atom ran before the refusal");
        assert!(!project.join("evals/note/cassettes").exists());
    });
}
