// SPDX-License-Identifier: Apache-2.0

//! `atomic eval` as CI runs it, against the scripted fake model:
//!
//! - `--live` calls the model and records each answer; without it, the same
//!   evals replay from the recordings with no model call, and say they are a
//!   historical check;
//! - a pass rate that drops below the atom's threshold fails the command,
//!   with the failing case in its JUnit file and its JSON report; one that
//!   stays at or above the threshold passes;
//! - an atom's scorers are reported as not run, since there is no judge;
//! - a case selected by its identity runs alone.

use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const ATOMIC: &str = env!("CARGO_BIN_EXE_atomic");

fn scratch(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
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

fn eval(dir: &Path, project: &Path, url: &str, args: &[&str]) -> Output {
    let mut cmd = Command::new(ATOMIC);
    cmd.arg("eval")
        .args(args)
        .current_dir(project)
        .env("HOME", dir.join("home"))
        .env("ATOMIC_HOME", dir.join("atomic-home"))
        .env("USER", "dev")
        .env("ATOMIC_MODEL_BASE_URL", url)
        .env("ATOMIC_MODEL", "synthetic")
        .env_remove("ATOMIC_MODEL_API_KEY_ENV")
        .env_remove("CODEX_HOME")
        .env_remove("ATOMIC_DATABASE_URL")
        .stdin(Stdio::null());
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
        let live = tokio::task::spawn_blocking(move || eval(&d, &p, &u, &["--live"]))
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
        let replayed = tokio::task::spawn_blocking(move || eval(&d, &p, &u, &[]))
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
fn a_pass_rate_at_its_threshold_passes() {
    on_codex_runtime(async {
        let dir = scratch("threshold");
        // Two of three cases may pass.
        let project = project(&dir, Some(0.6));
        let cases = project.join("evals/note/cases.jsonl");
        let mut rows = std::fs::read_to_string(&cases).expect("cases");
        rows.push_str(
            &(json!({"input": {"text": "Export crashes"}, "expect": ["output.ticket == 'BUG-9'"]})
                .to_string()
                + "\n"),
        );
        std::fs::write(&cases, rows).expect("cases");
        let model = FakeModel::respond(answer).await.expect("fake model");
        let url = model.base_url();
        let (d, p, u) = (dir.clone(), project.clone(), url.clone());
        let out = tokio::task::spawn_blocking(move || eval(&d, &p, &u, &["note", "--live"]))
            .await
            .expect("eval");
        assert!(out.status.success(), "{}", text(&out));
        assert!(
            String::from_utf8_lossy(&out.stdout)
                .contains("PASS note: 2 of 3 cases passed, pass rate 0.667, threshold 0.600"),
            "{}",
            text(&out)
        );
        let _ = std::fs::remove_dir_all(&dir);
    });
}
