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
//! - adding a `validate` rule reuses every recording, labelled historical, and
//!   checks the rule against the recorded answers; another model reuses none;
//! - a live eval is metered in the usage ledger, and a budget with no room
//!   for a call stops it before any;
//! - a replay reads no API key and sends none;
//! - an atom that could write, or a Codex atom under cassettes, is refused;
//! - JUnit holds a suite per atom, `--report` writes the reports, and
//!   `--sample` runs a seeded sample;
//! - a project `atomic init` wrote, hand edited by nobody, records its own
//!   sample case and replays it offline to a passing report, inside a bound on
//!   the whole wall clock those commands take.

use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const ATOMIC: &str = env!("CARGO_BIN_EXE_atomic");

/// How long a command that should finish may take before the test gives up on
/// it: a suite that cannot reach its backend fails, and never hangs.
const COMMAND_BOUND: Duration = Duration::from_secs(120);

/// How long one of that command's streams may stay open after the command is
/// gone before the test gives up on reading it. What holds a stream open once
/// the command has ended is some other process holding its end of the pipe,
/// and the test waits for no process it did not start.
const PIPE_BOUND: Duration = Duration::from_secs(30);

/// Read `pipe` to its end and give the answer as one to come: the reading
/// runs on a thread of its own, so the stream is emptied the whole time
/// something else — the command, or another of its streams — is waited for.
fn read_on_a_thread(
    pipe: impl Read + Send + 'static,
) -> std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>> {
    let (came, answer) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut pipe = pipe;
        let mut bytes = Vec::new();
        let read = pipe.read_to_end(&mut bytes).map(|_| bytes);
        let _ = came.send(read);
    });
    answer
}

/// What one stream answered, waiting no longer than [`PIPE_BOUND`] for it. A
/// stream still open after that is one a process the command left behind
/// holds, and the test says so rather than wait for it. A stream whose reading
/// failed is said so too: half a stream is never taken for all of it.
fn answered(
    came: std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    which: &str,
    what: &str,
) -> Vec<u8> {
    match came.recv_timeout(PIPE_BOUND) {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => panic!("{what}: reading its {which}: {e}"),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            panic!("{what}: the thread reading its {which} ended without saying what it read")
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!(
            "{what} kept its {which} open {} s after its process was gone: something it left \
             behind holds that pipe, and this test waits for nothing it did not start",
            PIPE_BOUND.as_secs()
        ),
    }
}

/// Run what `cmd` describes to its end, writing `stdin` to it, and say what it
/// answered. Its three pipes are each carried by a thread of their own, so a
/// command that fills one stream is read while another of its streams is
/// still open.
///
/// A command still running after [`COMMAND_BOUND`] is killed and reaped and the
/// test fails, so a command that hangs is never left running for the next one;
/// a stream still open [`PIPE_BOUND`] after its process is gone fails the test
/// too, so no process this test did not start can hang it past those two
/// bounds. The threads still reading streams the test gave up on are left to
/// end when the processes holding those pipes do.
fn run_and_capture(cmd: &mut Command, stdin: &str, what: &str) -> Output {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("start {what}: {e}"));
    let said = read_on_a_thread(child.stdout.take().expect("stdout"));
    let complained = read_on_a_thread(child.stderr.take().expect("stderr"));
    // Its input is written on a thread of its own too, so a command that
    // never reads what it is given cannot stop the test from reading what it
    // says; and the write end is then closed, so a command waiting for its
    // input to end is not waiting for nothing.
    let mut input = child.stdin.take().expect("stdin");
    let given = stdin.to_owned();
    std::thread::spawn(move || {
        let _ = input.write_all(given.as_bytes());
        drop(input);
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() >= COMMAND_BOUND {
                    // Not left running for whatever the test does next.
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("{what} did not finish within {} s", COMMAND_BOUND.as_secs());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("{what}: waiting for it: {e}"),
        }
    };
    Output {
        status,
        stdout: answered(said, "stdout", what),
        stderr: answered(complained, "stderr", what),
    }
}

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

/// `atomic <args>` run in `cwd`, with nothing configured: HOME and atomic's
/// own state inside the test's scratch directory, no key and no endpoint from
/// the environment, and no local Ollama for detection to find. What a caller
/// adds to this is the only configuration the command sees.
fn atomic_command(dir: &Path, cwd: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(ATOMIC);
    cmd.args(args)
        .current_dir(cwd)
        .env("HOME", dir.join("home"))
        .env("ATOMIC_HOME", dir.join("atomic-home"))
        .env("USER", "dev")
        .env_remove("ATOMIC_MODEL_API_KEY_ENV")
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENAI_BASE_URL")
        .env_remove("CODEX_HOME")
        .env_remove("ATOMIC_DATABASE_URL")
        // Nothing listens on the discard port, so detection never finds a
        // local Ollama and an endpoint comes only from what a test sets.
        .env("ATOMIC_DETECT_OLLAMA_ADDR", "127.0.0.1:9");
    cmd
}

/// `atomic <args>` run in `cwd` on that environment, with nothing added.
fn atomic(dir: &Path, cwd: &Path, args: &[&str]) -> Output {
    run_and_capture(&mut atomic_command(dir, cwd, args), "", "atomic")
}

fn eval_with(dir: &Path, project: &Path, url: &str, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut argv = vec!["eval"];
    argv.extend(args);
    let mut cmd = atomic_command(dir, project, &argv);
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
    // Waited for under the same bound as every other command this suite runs.
    run_and_capture(&mut cmd, "", "atomic eval")
}

fn text(out: &Output) -> String {
    format!(
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn atomic_eval_replays_a_redacted_model_answer() {
    const INPUT: &str = concat!("sk-ant-", "api03-inputabcdefghijklmnop1234567890");
    const OUTPUT: &str = concat!("sk-ant-", "api03-outputabcdefghijklmnop1234567890");
    on_codex_runtime(async {
        let dir = scratch("redacted-cassette");
        let project = project(&dir, None);
        let eval_dir = project.join("evals/note");
        std::fs::write(
            eval_dir.join("cases.jsonl"),
            json!({"input": {"text": INPUT}, "expect": ["output.ticket.startsWith('BUG-1 ')"]})
                .to_string(),
        )
        .expect("case");
        let model = FakeModel::start(vec![Reply::Text(
            json!({"ticket": format!("BUG-1 {OUTPUT}")}).to_string(),
        )])
        .await
        .expect("fake model");
        let url = model.base_url();
        let live = eval_async(&dir, &project, &url, &["note", "--live"]).await;
        assert!(live.status.success(), "{}", text(&live));
        assert_eq!(model.calls(), 1);
        let files: Vec<_> = std::fs::read_dir(eval_dir.join("cassettes"))
            .expect("cassettes")
            .map(|entry| entry.expect("cassette entry").path())
            .collect();
        assert_eq!(files.len(), 1);
        let fixture = std::fs::read_to_string(&files[0]).expect("cassette file");
        assert!(
            !fixture.contains(INPUT),
            "request secret persisted in cassette"
        );
        assert!(
            !fixture.contains(OUTPUT),
            "response secret persisted in cassette"
        );

        let replay = eval_async(&dir, &project, &url, &["note", "--json"]).await;
        assert!(replay.status.success(), "{}", text(&replay));
        let report: Value = serde_json::from_slice(&replay.stdout).expect("one JSON report");
        assert_eq!(report["cassettes"][0]["replayed"], 1);
        assert_eq!(report["cassettes"][0]["corrupt"], 0);
        assert_eq!(
            report["reports"][0]["cases"][0]["output"]["ticket"],
            "BUG-1 [REDACTED]"
        );
        assert_eq!(model.calls(), 1, "the replay contacted the model");
    });
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

/// What the tool may spend of M39a's 15 minutes for `atomic init` to a
/// passing eval, leaving the rest to the person.
const INIT_TO_EVAL_BOUND: Duration = Duration::from_secs(120);

/// From `atomic init` to a passing eval, on what init wrote and nothing else:
/// the sample eval case init ships records against the fake model, replays
/// offline to a passing report, and those commands together stay inside a
/// bound on the wall clock.
///
/// The clause's 15 minutes are a person's clock — reading what init wrote,
/// deciding what to change, reading a report — and no test measures a person.
/// What this bounds is the tool's share of that clock: the commands a new user
/// types, which must leave the other 13 minutes and more room for the person.
#[test]
fn from_init_to_a_passing_eval_records_and_replays_the_sample_case_within_the_bound() {
    on_codex_runtime(async {
        let dir = scratch("init-eval");
        // What init's sample case asks for is a non-empty `summary` of at
        // most 160 characters (its output schema), so any summary passes it.
        let model = FakeModel::respond(|_| {
            Reply::Text(
                json!({"summary": "Export crashes when a file name holds an accent."}).to_string(),
            )
        })
        .await
        .expect("fake model");
        let url = model.base_url();
        let started = Instant::now();

        // A new user's first command, in an empty directory of their own.
        let fresh = dir.join("note-tools");
        std::fs::create_dir_all(&fresh).expect("dir");
        let init = atomic(&dir, &fresh, &["init"]);
        assert!(init.status.success(), "{}", text(&init));

        // Nothing is hand edited: the case recorded below is the one init
        // wrote, with the expectation and the input it ships with.
        let case = std::fs::read_to_string(fresh.join("evals/summarize-note/export-crash.yaml"))
            .expect("the sample case init wrote");
        assert!(
            case.contains("output.summary.size() > 0"),
            "the case is init's own, not a test's: {case}"
        );
        let note = case
            .lines()
            .find_map(|l| l.trim().strip_prefix("text: "))
            .unwrap_or_else(|| panic!("the case gives its own input: {case}"))
            .to_owned();

        // Recorded live against the fake model: every atom with cases runs,
        // which is the one atom init wrote, in one model call that asks with
        // the note from init's own case.
        let live = eval_async(&dir, &fresh, &url, &["--live"]).await;
        assert!(live.status.success(), "{}", text(&live));
        assert_eq!(model.calls(), 1, "recording the case is one model call");
        let asked = model.requests()[0].to_string();
        assert!(
            asked.contains(&note),
            "the live eval sent the input from the case init wrote ({note}): {asked}"
        );
        let out = String::from_utf8_lossy(&live.stdout);
        assert!(
            out.contains("PASS summarize-note: 1 of 1 cases passed"),
            "the recorded eval passes: {out}"
        );

        // The recording is in the project, beside the case it answers, which
        // is what makes the next replay need nothing but the project: one
        // case, one recording.
        let cassettes = fresh.join("evals/summarize-note/cassettes");
        let recordings: Vec<PathBuf> = std::fs::read_dir(&cassettes)
            .unwrap_or_else(|e| panic!("{}: {e}", cassettes.display()))
            .map(|entry| entry.expect("entry").path())
            .collect();
        assert_eq!(
            recordings.len(),
            1,
            "the one case leaves one recording: {recordings:?}"
        );

        // Replayed: no key, no model call, the answer read off that
        // recording. It names the endpoint it was made against, so the replay
        // is given that same endpoint and the fake model counts whatever it
        // is asked for — which, offline, is nothing at all.
        let sent = model.every_request();
        let replayed = eval_async(&dir, &fresh, &url, &["--json"]).await;
        assert!(replayed.status.success(), "{}", text(&replayed));
        assert_eq!(model.calls(), 1, "the replay made no model call");
        assert_eq!(
            model.every_request(),
            sent,
            "offline, the replay sent the endpoint no request of any kind"
        );
        let report: Value =
            serde_json::from_slice(&replayed.stdout).expect("one JSON report from --json");
        assert_eq!(report["passed"], true, "the replayed report passes");
        assert_eq!(report["source"], "recorded");
        assert_eq!(report["refused"], json!([]), "no atom was set aside");
        assert_eq!(report["reports"][0]["atom"], "summarize-note");
        assert_eq!(report["reports"][0]["passed"], true);
        assert_eq!(report["reports"][0]["cases"][0]["passed"], true);
        assert_eq!(report["cassettes"][0]["replayed"], 1, "{report}");
        assert_eq!(report["cassettes"][0]["missed"], 0, "{report}");

        // The tool's whole share of the clause's clock, on this machine.
        let took = started.elapsed();
        assert!(
            took < INIT_TO_EVAL_BOUND,
            "`atomic init` to a passing eval took {:.1} s, over the {} s bound",
            took.as_secs_f64(),
            INIT_TO_EVAL_BOUND.as_secs(),
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
        // The live eval sent one request per case, and nothing else.
        let sent = model.every_request();
        assert_eq!(
            sent.len(),
            2,
            "two cases, two requests, no probe beside them: {sent:?}"
        );
        let (d, p, u) = (dir.clone(), project.clone(), url.clone());
        let replay = tokio::task::spawn_blocking(move || {
            eval_with(&d, &p, &u, &["note", "--json"], &secret)
        })
        .await
        .expect("eval");
        assert!(replay.status.success(), "{}", text(&replay));
        assert_eq!(model.calls(), 2, "the replay made no model call");
        assert_eq!(
            model.every_request(),
            sent,
            "the replay sent the endpoint no request of any kind"
        );
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
        // A live eval metered in a shared ledger refuses the sign-in. A
        // sign-in is kept on Linux only, so elsewhere there is none to refuse.
        if cfg!(target_os = "linux") {
            env.push((
                "ATOMIC_DATABASE_URL".into(),
                "postgres://atomic:atomic@127.0.0.1:9/atomic".into(),
            ));
            env.push(("ATOMIC_EXECUTOR_ID".into(), "host-1".into()));
            let (d, p, e) = (dir.clone(), project.clone(), env.clone());
            let shared = tokio::task::spawn_blocking(move || {
                let e: Vec<(&str, &str)> =
                    e.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
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
        }
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

/// What `atomic eval` reuses and what it throws away: `note`'s recordings
/// are recorded once; a `validate` rule added to the atom changes nothing that
/// is sent, so each case's first turn is answered from them, the replay says
/// it is a historical check, and the new rule is checked against the recorded
/// answers; another model is a request never recorded, so every case is left
/// without a recording and says to record it again. No model is called after
/// the recording.
#[test]
fn a_validate_edit_reuses_the_recordings_and_another_model_throws_them_away() {
    on_codex_runtime(async {
        let dir = scratch("validate-model");
        let project = project(&dir, None);
        let model = FakeModel::respond(answer).await.expect("fake model");
        let url = model.base_url();
        let out = eval_async(&dir, &project, &url, &["note", "--live"]).await;
        assert!(out.status.success(), "{}", text(&out));
        let recorded = model.calls();
        assert_eq!(recorded, 2, "one call per case");

        // A rule both recorded answers keep: replayed whole, and labelled.
        let atom = project.join("atoms/note.atom.astro");
        let original = std::fs::read_to_string(&atom).expect("the atom");
        let with_rule = |rule: &str| {
            let edited = original.replace(
                "kind: 'lite'",
                &format!("kind: 'lite', validate: [({{ output }}) => {rule}]"),
            );
            assert_ne!(edited, original, "the rule was not added");
            std::fs::write(&atom, edited).expect("the edited atom");
        };
        with_rule("output.ticket.length > 0");
        let out = eval_async(&dir, &project, &url, &["note"]).await;
        assert!(out.status.success(), "{}", text(&out));
        let said = text(&out);
        for part in [
            "a historical check",
            "PASS note: 2 of 2 cases passed",
            "2 replayed, 0 recorded, 0 without a recording",
        ] {
            assert!(said.contains(part), "{part}: {said}");
        }
        assert_eq!(model.calls(), recorded, "a validate edit called the model");

        // A rule the second recorded answer, FEAT-2, breaks: both first
        // turns are still answered from the recordings, and the rule is
        // checked against what they recorded. BUG-1 keeps it; FEAT-2 does
        // not, so its repair turn asks again, a request never recorded, which
        // misses and calls no model.
        with_rule("output.ticket.length < 6");
        let out = eval_async(&dir, &project, &url, &["note", "--json"]).await;
        assert!(!out.status.success(), "{}", text(&out));
        let report: Value = serde_json::from_slice(&out.stdout).expect("one JSON report");
        assert_eq!(report["source"], "recorded", "{report}");
        assert_eq!(report["cassettes"][0]["replayed"], 2, "{report}");
        assert_eq!(
            report["cassettes"][0]["missed"], 1,
            "the repair turn: {report}"
        );
        let cases = report["reports"][0]["cases"].as_array().expect("two cases");
        assert_eq!(cases.len(), 2, "{report}");
        assert_eq!(cases[0]["passed"], true, "BUG-1 keeps the rule: {report}");
        assert_eq!(cases[0]["output"], json!({"ticket": "BUG-1"}), "{report}");
        assert_eq!(cases[1]["passed"], false, "FEAT-2 breaks it: {report}");
        let failures = cases[1]["failures"].as_array().expect("FEAT-2's failures");
        assert_eq!(failures.len(), 1, "{report}");
        assert!(
            failures[0]
                .as_str()
                .is_some_and(|f| f.contains("no recording answers this request")),
            "the repair turn missed: {report}"
        );
        assert_eq!(model.calls(), recorded, "a validate edit called the model");
        std::fs::write(&atom, &original).expect("the atom back");

        // Another model: no recording answers, and each case says so.
        let other = [("ATOMIC_MODEL", "another-model")];
        let (d, p, u) = (dir.clone(), project.clone(), url.clone());
        let out = tokio::task::spawn_blocking(move || eval_with(&d, &p, &u, &["note"], &other))
            .await
            .expect("eval");
        assert!(
            !out.status.success(),
            "another model was answered: {}",
            text(&out)
        );
        let said = text(&out);
        for part in [
            "no recording answers this request",
            "run the eval with --live to record it",
            "2 model call(s) had no usable recording (2 missing, 0 corrupt): \
             record them with --live",
        ] {
            assert!(said.contains(part), "{part}: {said}");
        }
        // The control: the recorded model is still answered.
        let out = eval_async(&dir, &project, &url, &["note"]).await;
        assert!(out.status.success(), "{}", text(&out));
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

/// What the command below writes to its error stream: more than any pipe
/// holds, so it cannot finish writing until someone is reading. The shell
/// doubles this from one character, so what it writes is exactly it.
const STREAM_FILLED: usize = 256 * 1024;

/// What the same command writes to its output stream, which the test looks
/// for in what it read: a reader that took the streams in the wrong order
/// would lose the command's end, and its own exit code, without noticing.
const STREAM_SAID: usize = 4096;

/// How long that command may take, which is less than [`COMMAND_BOUND`] so
/// that a stream nobody reads fails this test on its own rather than waiting
/// out the bound meant for a command that has hung.
const STREAM_BOUND: Duration = Duration::from_secs(60);

/// Every stream of a command is read while it runs, not one after the other:
/// a command that fills its error stream while its output is still open is
/// read, waited for and reported. The command is the shell and what it writes
/// is this test's own text, so it reaches no endpoint and needs no model.
#[test]
fn a_command_that_fills_one_stream_is_read_while_it_writes_the_other() {
    let dir = scratch("streams");
    let script = dir.join("both-streams.sh");
    // Error stream first, and more of it than a pipe holds, with its output
    // still open; then its output, then an exit code of its own.
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nfilled=e\nwhile [ ${{#filled}} -lt {STREAM_FILLED} ]; do \
             filled=$filled$filled; done\nprintf '%s' \"$filled\" >&2\n\
             printf '%s' \"$STREAM_SAID\"\nexit 3\n"
        ),
    )
    .expect("script");
    let said = "s".repeat(STREAM_SAID);
    let for_command = said.clone();
    let (came, answer) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut command = Command::new("/bin/sh");
        command.arg(&script).env("STREAM_SAID", &for_command);
        let _ = came.send(run_and_capture(&mut command, "", "the two-stream command"));
    });
    let out = match answer.recv_timeout(STREAM_BOUND) {
        Ok(out) => out,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            panic!("the thread running the two-stream command ended with no answer for it")
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!(
            "the two-stream command was not read within {} s: the stream nobody reads is \
             full, and the command cannot reach its own end",
            STREAM_BOUND.as_secs()
        ),
    };
    assert_eq!(
        out.status.code(),
        Some(3),
        "it ended by itself, uncut: {}",
        text(&out)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "e".repeat(STREAM_FILLED),
        "all of its error stream, which filled the pipe"
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        said,
        "all of its output stream, which was still open while its error stream filled"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The `atomic-create-atom` skill as this binary ships it: `atomic setup
/// codex` run in a directory of this test's own, and the `SKILL.md` it wrote
/// read back from there. The worked example is run as the file an agent that
/// was set up reads, not as a copy of it kept beside a test.
fn installed_skill(dir: &Path) -> String {
    let root = dir.join("agent");
    std::fs::create_dir_all(&root).expect("agent directory");
    let where_ = root.display().to_string();
    let out = atomic(dir, &root, &["setup", "codex", "--path", &where_]);
    assert!(out.status.success(), "{}", text(&out));
    let skill = root.join(".agents/skills/atomic-create-atom/SKILL.md");
    let text = std::fs::read_to_string(&skill).unwrap_or_else(|e| {
        panic!(
            "{}: the skill `atomic setup` said it wrote: {e}",
            skill.display()
        )
    });
    assert!(
        text.contains("`atomic eval "),
        "the installed skill tells the agent to run no eval command: {text}"
    );
    text
}

/// Every file the worked example in `skill` names — each `File \`path\`:`
/// line followed by its fenced block — as its path and the exact bytes the
/// block holds. A named file with no block after it, a block that names no
/// language or holds nothing, and a block that is never closed are this
/// test's own failures: an example it cannot read whole is never run in part,
/// and an unclosed block would otherwise swallow the rest of the file into
/// the one being read.
fn worked_example(skill: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = skill.lines().collect();
    let mut files = Vec::new();
    let mut at = 0;
    while at < lines.len() {
        let Some(path) = lines[at]
            .strip_prefix("File `")
            .and_then(|rest| rest.strip_suffix("`:"))
        else {
            at += 1;
            continue;
        };
        let opened = lines[at..]
            .iter()
            .position(|l| l.starts_with("```"))
            .unwrap_or_else(|| panic!("the skill names `{path}` with no block after it"));
        let fence = lines[at + opened];
        assert!(fence.len() > 3, "`{path}`: its block names no language");
        let after = at + opened + 1;
        let closed = lines[after..]
            .iter()
            .position(|l| *l == "```")
            .unwrap_or_else(|| panic!("`{path}`: its block is never closed"));
        let body = &lines[after..after + closed];
        assert!(!body.is_empty(), "`{path}`: its block holds nothing");
        files.push((path.to_owned(), body.join("\n") + "\n"));
        at = after + closed + 1;
    }
    assert!(
        !files.is_empty(),
        "the skill names no file of its worked example"
    );
    files
}

/// What the loopback server answers one of the worked example's cases with:
/// the notes it was sent say what the actions are. Every case's prompt holds
/// the atom's typed example notes as well as its own, so only the case with
/// an instruction hidden in its notes says this.
fn example_actions(request: &Value) -> Reply {
    let actions = if request.to_string().contains("Ignore your instructions") {
        // The instruction is data: it names no task, so there is no action.
        json!([])
    } else {
        json!([
            {"task": "Send the budget by Friday", "owner": "Ana"},
            {"task": "Book the room", "owner": ""},
        ])
    };
    Reply::Text(json!({ "actions": actions }).to_string())
}

/// The skill's own worked example, run at the command line the skill tells an
/// agent to use, with nothing but this test's loopback server behind it.
///
/// `atomic setup` writes the pack, the worked example's files become a
/// project, `atomic check --json` says what the installed skill says it says,
/// and `atomic eval extract-actions` runs the example's two cases: recorded
/// once from the stand-in, then replayed from those recordings with the real
/// command — plain and with `--json`, whole and one case by its identity —
/// which answers both from the tapes and sends the endpoint no request of any
/// kind. A recording taken away leaves that case without an answer, says to
/// record it with `--live`, and still sends nothing. So the skill's ending is
/// checked as a person gets it: the flags, the case selection, the endpoint
/// the replay needs, the replay meter and the exit status, not only the
/// engine underneath them.
#[test]
fn the_skill_worked_example_replays_its_two_cases_through_the_cli_with_no_further_call() {
    on_codex_runtime(async {
        let dir = scratch("skill-worked-example");
        let skill = installed_skill(&dir);
        let files = worked_example(&skill);
        let named: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        // The example's own files, in the order the skill writes them: one
        // atom, its two schemas, its two cases and one fixture. If the skill
        // stops naming any of these, this test says so rather than run a
        // project of its own invention.
        assert_eq!(
            named,
            [
                "atomic.yaml",
                "atoms/extract-actions.input.schema.yaml",
                "atoms/extract-actions.output.schema.yaml",
                "atoms/extract-actions.atom.astro",
                "evals/extract-actions/two-actions.yaml",
                "evals/extract-actions/instruction-in-notes.yaml",
                "fixtures/extract-actions/two-actions.input.json",
            ],
            "the worked example's files, as the installed skill names them: {named:?}"
        );
        let project = dir.join("project");
        for (path, contents) in &files {
            let target = project.join(path);
            std::fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
            std::fs::write(&target, contents)
                .unwrap_or_else(|e| panic!("{}: {e}", target.display()));
        }

        // Step 7, at the command line. What the command printed is the very
        // line the installed skill tells the agent to expect, so the two
        // cannot drift apart unnoticed.
        let checked = atomic(&dir, &project, &["check", "--json"]);
        assert!(checked.status.success(), "{}", text(&checked));
        let printed = String::from_utf8_lossy(&checked.stdout).trim().to_owned();
        let check_json: Value = serde_json::from_str(&printed)
            .unwrap_or_else(|e| panic!("`atomic check --json` printed no JSON: {printed}: {e}"));
        assert_eq!(
            check_json,
            json!({
                "checked": {"atoms": 1, "workflows": 0, "toolset_operations": 0},
                "warnings": [],
            }),
            "the skill's own project is one atom, no workflow, no toolset operation"
        );
        assert!(
            skill.contains(&printed),
            "the installed skill does not print what `atomic check --json` answered: {printed}"
        );

        // The endpoint the recordings name: this test's own loopback server,
        // so no provider, no key and no person's decision is anywhere near
        // this. It counts every request it is sent, model call or other.
        let model = FakeModel::respond(example_actions)
            .await
            .expect("fake model");
        let url = model.base_url();

        // Recorded once, so the project has the two recordings a checked-in
        // example would ship: two cases, two calls, each answer recorded.
        let live = eval_async(&dir, &project, &url, &["extract-actions", "--live"]).await;
        assert!(live.status.success(), "{}", text(&live));
        assert_eq!(model.calls(), 2, "one model call per case");
        let said = String::from_utf8_lossy(&live.stdout).to_string();
        for part in [
            "model calls: live, each answer recorded in the cassettes",
            "PASS extract-actions: 2 of 2 cases passed",
            "cassettes: 0 replayed, 2 recorded, 0 without a recording",
        ] {
            assert!(said.contains(part), "{part}: {said}");
        }
        let cassettes = project.join("evals/extract-actions/cassettes");
        let recordings: Vec<PathBuf> = std::fs::read_dir(&cassettes)
            .unwrap_or_else(|e| panic!("{}: {e}", cassettes.display()))
            .map(|e| e.expect("entry").path())
            .collect();
        assert_eq!(
            recordings.len(),
            2,
            "one recording beside the cases per case: {recordings:?}"
        );
        for tape in &recordings {
            let kept = std::fs::symlink_metadata(tape)
                .unwrap_or_else(|e| panic!("{}: {e}", tape.display()));
            assert!(
                kept.is_file(),
                "what sits beside the cases is no file: {tape:?}"
            );
            let bytes =
                std::fs::read_to_string(tape).unwrap_or_else(|e| panic!("{}: {e}", tape.display()));
            assert!(
                !bytes.contains("Authorization"),
                "a header kept in a recording: {tape:?}"
            );
        }

        // Replayed, plainly: the same two cases off those recordings, called
        // a historical check, with the stand-in sent nothing at all.
        let sent = model.every_request();
        let replay = eval_async(&dir, &project, &url, &["extract-actions"]).await;
        assert!(replay.status.success(), "{}", text(&replay));
        assert_eq!(model.calls(), 2, "the replay made a model call");
        assert_eq!(
            model.every_request(),
            sent,
            "the replay sent the endpoint a request of any kind"
        );
        let said = String::from_utf8_lossy(&replay.stdout).to_string();
        for part in [
            "model calls: replayed from cassettes: a historical check of recorded answers, \
             not a live pass",
            "PASS extract-actions: 2 of 2 cases passed, pass rate 1.000, threshold 1.000",
            "cassettes: 2 replayed, 0 recorded, 0 without a recording",
        ] {
            assert!(said.contains(part), "{part}: {said}");
        }

        // The same replay as its JSON report: two cases, each named for the
        // file the skill wrote, each answered with what its own case checks,
        // every call replayed and none keyed by a credential.
        let replay = eval_async(&dir, &project, &url, &["extract-actions", "--json"]).await;
        assert!(replay.status.success(), "{}", text(&replay));
        assert_eq!(model.calls(), 2, "the replay made a model call");
        assert_eq!(
            model.every_request(),
            sent,
            "the replay sent the endpoint a request of any kind"
        );
        let document: Value =
            serde_json::from_slice(&replay.stdout).expect("one JSON report from --json");
        assert_eq!(document["passed"], true, "{document}");
        assert_eq!(document["source"], "recorded", "{document}");
        assert_eq!(document["refused"], json!([]), "a lite atom was set aside");
        assert_eq!(document["reports"].as_array().expect("one report").len(), 1);
        let report = &document["reports"][0];
        assert_eq!(report["atom"], "extract-actions");
        assert_eq!(report["passed"], true);
        assert_eq!(report["pass_rate"], 1.0);
        assert_eq!(report["threshold"], 1.0);
        assert_eq!(report["failures"], json!([]));
        let mut ids: Vec<&str> = report["cases"]
            .as_array()
            .expect("the example's two cases")
            .iter()
            .map(|c| c["id"].as_str().expect("a case identity"))
            .collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            ["instruction-in-notes.yaml", "two-actions.yaml"],
            "the cases the skill wrote are the cases that ran"
        );
        for case in report["cases"].as_array().expect("two cases") {
            assert_eq!(case["passed"], true, "{case}");
        }
        let actions = |id: &str| -> Value {
            report["cases"]
                .as_array()
                .expect("two cases")
                .iter()
                .find(|c| c["id"] == json!(id))
                .and_then(|c| c["output"].get("actions"))
                .cloned()
                .expect("the case has its output")
        };
        assert_eq!(
            actions("two-actions.yaml"),
            json!([
                {"task": "Send the budget by Friday", "owner": "Ana"},
                {"task": "Book the room", "owner": ""},
            ]),
            "what the ordinary case's assertions were checked against"
        );
        assert_eq!(
            actions("instruction-in-notes.yaml"),
            json!([]),
            "the hidden instruction named no task, so the case expects no action"
        );
        let tape = &document["cassettes"][0];
        assert_eq!(tape["atom"], "extract-actions", "{document}");
        assert_eq!(tape["replayed"], 2, "{document}");
        assert_eq!(tape["recorded"], 0, "the replay recorded: {document}");
        assert_eq!(tape["missed"], 0, "a case with no recording: {document}");
        assert_eq!(tape["corrupt"], 0, "{document}");
        assert_eq!(tape["keyed"], 0, "a key reached the replay: {document}");
        assert_eq!(document["usage"], json!([]), "a replay spent nothing");

        // One case by its identity, as the skill says to read a failing case:
        // it runs alone, off its own recording, and asks the endpoint nothing.
        let one = eval_async(
            &dir,
            &project,
            &url,
            &["extract-actions", "--case", "two-actions.yaml", "--json"],
        )
        .await;
        assert!(one.status.success(), "{}", text(&one));
        assert_eq!(model.calls(), 2, "one case by its identity made no call");
        assert_eq!(
            model.every_request(),
            sent,
            "one case by its identity sent the endpoint a request"
        );
        let report: Value =
            serde_json::from_slice(&one.stdout).expect("one JSON report from --json");
        assert_eq!(
            report["reports"][0]["cases"]
                .as_array()
                .expect("one case")
                .len(),
            1,
            "`--case two-actions.yaml` ran some other number of cases: {report}"
        );
        assert_eq!(
            report["reports"][0]["cases"][0]["id"], "two-actions.yaml",
            "{report}"
        );
        assert_eq!(report["reports"][0]["cases"][0]["passed"], true, "{report}");
        assert_eq!(report["cassettes"][0]["replayed"], 1, "{report}");
        assert_eq!(report["cassettes"][0]["missed"], 0, "{report}");

        // A recording taken away is the case with none: the command fails,
        // says which call had no answer and how to record it, and still sends
        // the endpoint nothing rather than asking it instead.
        std::fs::remove_file(&recordings[0])
            .unwrap_or_else(|e| panic!("{}: {e}", recordings[0].display()));
        let gone = eval_async(&dir, &project, &url, &["extract-actions", "--json"]).await;
        assert!(
            !gone.status.success(),
            "a missing recording passed: {}",
            text(&gone)
        );
        assert_eq!(
            model.calls(),
            2,
            "a case with no recording reached the model"
        );
        assert_eq!(
            model.every_request(),
            sent,
            "a case with no recording sent the endpoint a request"
        );
        let report: Value =
            serde_json::from_slice(&gone.stdout).expect("one JSON report from --json");
        assert_eq!(report["passed"], false, "{report}");
        assert_eq!(report["cassettes"][0]["replayed"], 1, "{report}");
        assert_eq!(report["cassettes"][0]["missed"], 1, "{report}");
        assert_eq!(report["cassettes"][0]["corrupt"], 0, "{report}");
        let said = text(&gone);
        for part in [
            "no recording answers this request",
            "run the eval with --live to record it",
            "1 model call(s) had no usable recording (1 missing, 0 corrupt): record them with \
             --live",
        ] {
            assert!(said.contains(part), "{part}: {said}");
        }

        // The whole run, as the endpoint saw it: the two calls that made the
        // two recordings, and nothing else across any replay, any one-case
        // run or any miss — no second call, no probe, no list, no retry.
        let every = model.every_request();
        let seen: Vec<(&str, &str)> = every
            .iter()
            .map(|(m, p)| (m.as_str(), p.as_str()))
            .collect();
        assert_eq!(
            seen.len(),
            2,
            "every request the endpoint ever saw: {seen:?}"
        );
        for (method, path) in &seen {
            assert_eq!(*method, "POST", "not a model call reached it: {seen:?}");
            assert!(
                path.ends_with("/chat/completions") || path.ends_with("/responses"),
                "a request that is no model call reached it: {seen:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    });
}
