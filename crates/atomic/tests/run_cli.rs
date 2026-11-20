// SPDX-License-Identifier: Apache-2.0

//! `atomic run`, `atomic runs resume`, `atomic serve` and `atomic runner`,
//! driven as a person or a script would: through the `atomic` executable,
//! on a project written while the test runs.
//!
//! - a name inside the project, a path from an unrelated directory and
//!   `--project` naming the manifest or its directory run the same atom or
//!   workflow, and input from a file, stdin and `--data` is the same input;
//! - an error says what went wrong and what to do, and with `--json` the
//!   action is a field of its own;
//! - a run killed after a step is resumed by its id, and nothing that
//!   finished runs again; a run killed at a gate keeps the gate and its
//!   deadline, and a second resume is refused while one runs;
//! - a resumed run keeps the budget it already spent, and an exec command
//!   whose outcome is unknown stays parked however often it is resumed;
//! - `atomic serve` started after a kill recovers the run and finishes it;
//!   stopped mid-command, it holds the lock until it exits and kills the
//!   command, at once on a second signal;
//! - a run started by one build resumes and recovers under a byte-different
//!   one, and one of another durable layout is reported, not resumed;
//! - a run id that exists is refused, and so is resuming a cancelled run;
//! - exec commands run in an `atomic runner` process when asked to, and one
//!   that kills its runner never runs again unless it says it may;
//! - the executor identity comes from atomic's own setting, is required on
//!   PostgreSQL, a restarted server recovers only its own runs, and a run
//!   another executor owns is resumed only once it has stopped.
//!
//! Model atoms call the scripted fake model, which counts calls; the exec
//! atom logs each time its command runs.

#![recursion_limit = "512"]

use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use atomic_durable::gate::{self, DecisionCommand, GateFilter, Principal, Verdict};
use atomic_durable::{DurableRuntime, Durare, RunStatus};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const ATOMIC: &str = env!("CARGO_BIN_EXE_atomic");

const NOTE: &str = r#"{"text": "Exporting a file named café.txt crashes the app."}"#;
const RANK: &str = r#"{"priority": 1}"#;
const SUMMARY: &str = r#"{"line": "P1: Export crashes on accents"}"#;

fn scratch(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "atomic-run-cli-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["home", "elsewhere"] {
        std::fs::create_dir_all(dir.join(sub)).expect("scratch");
    }
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

fn write_atom(project: &Path, name: &str, config: &str, body: &str, input: Value, output: Value) {
    let atoms = project.join("atoms");
    std::fs::write(
        atoms.join(format!("{name}.atom.astro")),
        format!(
            "---\nimport type {{ Props, Input, Output }} from '../.atomic/types/{name}';\n\
             import type {{ AtomConfig }} from '../.atomic/types/authoring';\n\
             const {{ input }}: Props = Astro.props;\n\
             export const config: AtomConfig<Input, Output> = {{ {config} }};\n\
             ---\n{body}\n"
        ),
    )
    .expect("atom");
    for (role, schema) in [("input", input), ("output", output)] {
        std::fs::write(
            atoms.join(format!("{name}.{role}.schema.json")),
            schema.to_string(),
        )
        .expect("schema");
    }
}

const TRIAGE: &str = "\
atomic: 1
name: triage
input: { $ref: ../atoms/count-note.input.schema.json }
output: { $ref: ../atoms/summarize-note.output.schema.json }
steps:
  count:
    atom: count-note
    with: { text: input.text }
  rank:
    after: [count]
    atom: rank-note
    with: { title: steps.count.output.title, words: steps.count.output.words }
  summarize:
    after: [rank]
    atom: summarize-note
    with: { title: steps.count.output.title, priority: steps.rank.output.priority }
returns: steps.summarize.output
";

/// The project: an exec atom that counts a note's words and logs each run,
/// two model atoms, and workflows over them.
fn project(dir: &Path) -> PathBuf {
    let project = dir.join("project");
    for sub in ["atoms", "workflows", "tools"] {
        std::fs::create_dir_all(project.join(sub)).expect("project");
    }
    std::fs::write(project.join("atomic.yaml"), "atomic: 1\nname: notes\n").expect("manifest");
    // Each run logs the command line of the process that started it.
    std::fs::write(
        project.join("tools/count.py"),
        "import json, os, sys\n\
         note = json.load(sys.stdin)\n\
         parent = open(f'/proc/{os.getppid()}/cmdline').read().replace('\\0', ' ')\n\
         open(os.environ['COUNT_LOG'], 'a').write(parent.strip() + '\\n')\n\
         json.dump({'title': note['text'][:25], 'words': len(note['text'].split())}, sys.stdout)\n",
    )
    .expect("script");
    let log = dir.join("count.log");
    write_atom(
        &project,
        "count-note",
        &format!(
            "description: 'Count words.', kind: 'exec', exec: {{ command: ['python3', 'tools/count.py'] }}, env: {{ COUNT_LOG: '{}' }}",
            log.display()
        ),
        "",
        object(json!({"text": {"type": "string"}})),
        object(json!({"title": {"type": "string"}, "words": {"type": "integer"}})),
    );
    write_atom(
        &project,
        "rank-note",
        "description: 'rank-note'",
        "Rank the note from 1 (urgent) to 3.\nTitle: {input.title}\nWords: {input.words}",
        object(json!({"title": {"type": "string"}, "words": {"type": "integer"}})),
        object(json!({"priority": {"type": "integer", "minimum": 1, "maximum": 3}})),
    );
    write_atom(
        &project,
        "summarize-note",
        "description: 'summarize-note'",
        "Write one line for the triage board.\nTitle: {input.title}\nPriority: {input.priority}",
        object(json!({"title": {"type": "string"}, "priority": {"type": "integer"}})),
        object(json!({"line": {"type": "string"}})),
    );
    let workflows = project.join("workflows");
    std::fs::write(workflows.join("triage.workflow.yaml"), TRIAGE).expect("workflow");
    std::fs::write(
        workflows.join("gated.workflow.yaml"),
        TRIAGE.replace("name: triage", "name: gated").replace(
            "  summarize:\n    after: [rank]\n",
            "  check:\n    after: [rank]\n    gate:\n      title: \"'Check the rank of ' + steps.count.output.title\"\n      show: steps.rank.output\n      timeout: 24h\n  summarize:\n    after: [check]\n",
        ),
    )
    .expect("workflow");
    std::fs::write(
        workflows.join("count-only.workflow.yaml"),
        "atomic: 1\nname: count-only\n\
         input: { $ref: ../atoms/count-note.input.schema.json }\n\
         output: { $ref: ../atoms/count-note.output.schema.json }\n\
         steps:\n  count:\n    atom: count-note\n    with: { text: input.text }\n\
         returns: steps.count.output\n",
    )
    .expect("workflow");
    project
}

/// How many times the exec atom's command ran, and the command line of the
/// process that started each run.
fn count_log(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("count.log"))
        .map(|s| s.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// `atomic`, from the executable at `program`, with a throwaway home and
/// state, as the local operator `dev`.
fn command_as(
    program: &Path,
    dir: &Path,
    cwd: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .env("HOME", dir.join("home"))
        .env("ATOMIC_HOME", dir.join("atomic-home"))
        .env("USER", "dev");
    for name in [
        "CODEX_HOME",
        "XDG_DATA_HOME",
        "ATOMIC_DATABASE_URL",
        "ATOMIC_DATABASE_SCHEMA",
        "ATOMIC_EXECUTOR_ID",
        "DBOS__VMID",
        "DBOS__APPVERSION",
        "ATOMIC_FAILPOINT",
        "ATOMIC_MODEL_BASE_URL",
        "ATOMIC_MODEL",
        "ATOMIC_MODEL_API_KEY_ENV",
        "ATOMIC_RUNNER",
    ] {
        cmd.env_remove(name);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd
}

fn atomic(dir: &Path, cwd: &Path, args: &[&str], env: &[(&str, &str)], stdin: &str) -> Output {
    atomic_as(Path::new(ATOMIC), dir, cwd, args, env, stdin)
}

/// [`atomic`], from the executable at `program`.
fn atomic_as(
    program: &Path,
    dir: &Path,
    cwd: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    stdin: &str,
) -> Output {
    let mut child = command_as(program, dir, cwd, args, env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start atomic");
    std::io::Write::write_all(&mut child.stdin.take().expect("stdin"), stdin.as_bytes())
        .expect("write stdin");
    // A command that should finish and does not fails the test, never hangs it.
    let pid = child.id();
    let (send, done) = std::sync::mpsc::channel();
    std::thread::spawn(move || send.send(child.wait_with_output()));
    match done.recv_timeout(Duration::from_secs(120)) {
        Ok(out) => out.expect("atomic ran"),
        Err(_) => {
            let _ = Command::new("/bin/sh")
                .args(["-c", &format!("kill -KILL {pid}")])
                .status();
            panic!("atomic {args:?} did not finish within two minutes");
        }
    }
}

/// `atomic` in the background, its stderr in `log`.
fn spawn(dir: &Path, cwd: &Path, args: &[&str], env: &[(&str, &str)], log: &Path) -> Child {
    spawn_as(Path::new(ATOMIC), dir, cwd, args, env, log)
}

/// [`spawn`], from the executable at `program`.
fn spawn_as(
    program: &Path,
    dir: &Path,
    cwd: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    log: &Path,
) -> Child {
    command_as(program, dir, cwd, args, env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(log).expect("log"))
        .spawn()
        .expect("start atomic")
}

fn text(out: &Output) -> String {
    format!(
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn stdout_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", text(out)))
}

/// The run id `atomic run` printed on stderr.
fn run_id(stderr: &str) -> String {
    stderr
        .lines()
        .find_map(|l| l.strip_prefix("run "))
        .and_then(|l| l.split(':').next())
        .unwrap_or_else(|| panic!("no run id in {stderr}"))
        .to_owned()
}

fn model_env(url: &str) -> [(&'static str, String); 2] {
    [
        ("ATOMIC_MODEL_BASE_URL", url.to_owned()),
        ("ATOMIC_MODEL", "synthetic".to_owned()),
    ]
}

fn with<'a>(a: &'a [(&'static str, String)], b: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    a.iter()
        .map(|(k, v)| (*k, v.as_str()))
        .chain(b.iter().copied())
        .collect()
}

/// The project's state directory, as atomic resolves it.
fn state_dir(dir: &Path, project: &Path) -> PathBuf {
    let out = atomic(dir, project, &["debug", "state-dir"], &[], "");
    assert!(out.status.success(), "{}", text(&out));
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

async fn open_gate(rt: &Durare, run: &str) -> gate::Gate {
    let started = Instant::now();
    loop {
        let open = gate::gates(
            rt,
            GateFilter {
                open_only: true,
                run: Some(run.to_owned()),
            },
        )
        .await
        .expect("gates");
        if let Some(g) = open.into_iter().next() {
            return g;
        }
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "{run} never opened a gate"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The local operator approves `g`, from outside the run's process: a
/// second terminal.
async fn approve(rt: &Durare, g: &gate::Gate) {
    // The operator's enrolled credential; enrolling again is a no-op.
    let credential = gate::HumanCredential::new(Principal::local("dev"), "the operator's secret");
    let _ = gate::enroll(rt, &credential).await;
    gate::decide(
        rt,
        &credential,
        DecisionCommand {
            command_id: format!("{}-approve", g.id),
            gate: g.id.clone(),
            verdict: Verdict::Approve,
            comment: None,
            proposal: g.proposal.clone(),
        },
    )
    .await
    .expect("decided");
}

fn terminate(child: &Child) {
    // The shell's own `kill`: the test image need not have the program.
    let status = Command::new("/bin/sh")
        .args(["-c", &format!("kill -TERM {}", child.id())])
        .status()
        .expect("kill");
    assert!(status.success());
}

fn wait_child(child: &mut Child, what: &str) -> std::process::ExitStatus {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            return status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "{what} did not exit"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_name_a_path_and_the_project_flag_run_the_same_atom_on_the_same_input() {
    let dir = scratch("same");
    let project = project(&dir);
    let elsewhere = dir.join("elsewhere");
    std::fs::write(elsewhere.join("note.json"), NOTE).expect("input file");
    let atom_file = project.join("atoms/count-note.atom.astro");
    let manifest = project.join("atomic.yaml");
    let (atom_file, project_dir, manifest) = (
        atom_file.to_string_lossy().into_owned(),
        project.to_string_lossy().into_owned(),
        manifest.to_string_lossy().into_owned(),
    );
    let runs: Vec<(&Path, Vec<&str>, &str)> = vec![
        (&project, vec!["run", "count-note", "--data", NOTE], ""),
        (&elsewhere, vec!["run", &atom_file, "--data", NOTE], ""),
        (
            &elsewhere,
            vec![
                "--project",
                &project_dir,
                "run",
                "count-note",
                "--input",
                "note.json",
            ],
            "",
        ),
        (
            &elsewhere,
            vec!["--project", &manifest, "run", "count-note", "--input", "-"],
            NOTE,
        ),
        (&project, vec!["run", "count-note", "--input", "-"], NOTE),
    ];
    let expected = json!({"title": "Exporting a file named ca", "words": 8});
    for (cwd, args, stdin) in &runs {
        let out = atomic(&dir, cwd, args, &[], stdin);
        assert!(out.status.success(), "{args:?}: {}", text(&out));
        assert_eq!(stdout_json(&out), expected, "{args:?}");
        // The durable run id comes first, on stderr.
        assert!(run_id(&String::from_utf8_lossy(&out.stderr)).starts_with("run-"));
    }
    assert_eq!(count_log(&dir).len(), runs.len(), "one run each");

    // With --json the result names the durable run, which the store keeps.
    let out = atomic(
        &dir,
        &project,
        &["--json", "run", "count-note", "--data", NOTE],
        &[],
        "",
    );
    let doc = stdout_json(&out);
    assert_eq!(doc["output"], expected);
    let id = doc["run"].as_str().expect("run id").to_owned();
    let out = atomic(
        &dir,
        &project,
        &["--json", "runs", "resume", &id],
        &[
            ("ATOMIC_MODEL_BASE_URL", "http://127.0.0.1:9/v1"),
            ("ATOMIC_MODEL", "unused"),
        ],
        "",
    );
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        stdout_json(&out),
        json!({"run": id, "status": "succeeded", "output": expected})
    );
    assert_eq!(
        count_log(&dir).len(),
        runs.len() + 1,
        "a finished run is not run again"
    );
}

#[test]
fn a_workflow_runs_by_name_and_by_file() {
    let dir = scratch("workflow");
    let project = project(&dir);
    let file = project.join("workflows/count-only.workflow.yaml");
    let file = file.to_string_lossy().into_owned();
    let by_name = atomic(
        &dir,
        &project,
        &["run", "count-only", "--data", NOTE],
        &[],
        "",
    );
    let by_file = atomic(
        &dir,
        &dir.join("elsewhere"),
        &["run", &file, "--data", NOTE],
        &[],
        "",
    );
    for out in [&by_name, &by_file] {
        assert!(out.status.success(), "{}", text(out));
    }
    assert_eq!(stdout_json(&by_name), stdout_json(&by_file));
    assert_eq!(
        stdout_json(&by_name),
        json!({"title": "Exporting a file named ca", "words": 8})
    );
}

#[test]
fn errors_say_what_to_do_and_json_carries_the_action_apart() {
    let dir = scratch("errors");
    let project = project(&dir);
    // A mapping from one step's output that cannot fit the next step's input.
    std::fs::write(
        project.join("workflows/broken.workflow.yaml"),
        TRIAGE.replace("name: triage", "name: broken").replace(
            "title: steps.count.output.title, words",
            "title: steps.count.output, words",
        ),
    )
    .expect("workflow");
    let out = atomic(&dir, &project, &["--json", "check"], &[], "");
    assert!(!out.status.success());
    let error = &stdout_json(&out)["error"];
    assert_eq!(error["kind"], "workflow.invalid", "{error}");
    let description = error["description"].as_str().expect("description");
    assert!(
        description.contains("step `rank`") && description.contains("step `count`"),
        "both steps are named: {description}"
    );
    let action = error["action"].as_str().expect("an action of its own");
    assert!(
        action.contains("`with.title` in step `rank`")
            && action.contains("atom `count-note` (step `count`)"),
        "{action}"
    );
    assert!(
        !description.contains(action),
        "the action is not in the description"
    );

    // Without --json the same failure is text, with the action on its own line.
    let out = atomic(&dir, &project, &["check"], &[], "");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("error: ") && stderr.contains("\n  to fix: make the two agree"),
        "{stderr}"
    );

    // An unknown name lists what there is.
    std::fs::remove_file(project.join("workflows/broken.workflow.yaml")).expect("remove");
    let out = atomic(
        &dir,
        &project,
        &["--json", "run", "count-notes", "--data", NOTE],
        &[],
        "",
    );
    let error = &stdout_json(&out)["error"];
    assert_eq!(error["kind"], "target.unknown");
    assert!(
        error["action"]
            .as_str()
            .is_some_and(|a| a.contains("count-note")),
        "{error}"
    );

    // Input that breaks the contract starts nothing.
    let out = atomic(
        &dir,
        &project,
        &["--json", "run", "count-note", "--data", r#"{"text": 3}"#],
        &[],
        "",
    );
    let error = &stdout_json(&out)["error"];
    assert_eq!(error["kind"], "input.invalid", "{error}");
    assert!(!String::from_utf8_lossy(&out.stderr).contains("run run-"));
    assert!(count_log(&dir).is_empty());
}

#[test]
fn a_run_killed_after_a_step_resumes_by_its_id_and_reruns_nothing_finished() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())])
            .await
            .expect("fake model");
        let dir = scratch("resume");
        let project = project(&dir);
        let env = model_env(&model.base_url());
        let (d, p, e) = (dir.clone(), project.clone(), env.clone());
        let out = tokio::task::spawn_blocking(move || {
            atomic(
                &d,
                &p,
                &["run", "triage", "--data", NOTE],
                &with(&e, &[("ATOMIC_FAILPOINT", "pipeline:after:rank")]),
                "",
            )
        })
        .await
        .expect("child");
        assert!(
            !out.status.success(),
            "killed at the failpoint: {}",
            text(&out)
        );
        let id = run_id(&String::from_utf8_lossy(&out.stderr));
        assert_eq!(model.calls(), 1, "rank ran");
        assert_eq!(count_log(&dir).len(), 1, "count ran");

        let (d, p, e, i) = (dir.clone(), project.clone(), env.clone(), id.clone());
        let out = tokio::task::spawn_blocking(move || {
            atomic(
                &d,
                &p,
                &["--json", "runs", "resume", &i],
                &with(&e, &[]),
                "",
            )
        })
        .await
        .expect("child");
        assert!(out.status.success(), "{}", text(&out));
        assert_eq!(
            stdout_json(&out),
            json!({"run": id, "status": "succeeded", "output": {"line": "P1: Export crashes on accents"}})
        );
        assert_eq!(model.calls(), 2, "only summarize ran again");
        assert_eq!(count_log(&dir).len(), 1, "count did not run again");
    });
}

#[test]
fn a_resumed_run_keeps_the_budget_it_already_spent() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())])
            .await
            .expect("fake model");
        let dir = scratch("budget");
        let project = project(&dir);
        let state = state_dir(&dir, &project);
        // One call's whole bound (80,000 tokens by default) and one token
        // more: rank fits, and summarize fits only if rank's spend is forgotten.
        std::fs::write(
            project.join("workflows/budgeted.workflow.yaml"),
            format!(
                "{}budget:\n  tokens: 80001\n",
                TRIAGE.replace("name: triage", "name: budgeted")
            ),
        )
        .expect("workflow");
        let env = model_env(&model.base_url());
        let (d, p, e) = (dir.clone(), project.clone(), env.clone());
        let out = tokio::task::spawn_blocking(move || {
            atomic(
                &d,
                &p,
                &["run", "budgeted", "--run-id", "budget-1", "--data", NOTE],
                &with(&e, &[("ATOMIC_FAILPOINT", "pipeline:after:rank")]),
                "",
            )
        })
        .await
        .expect("child");
        assert!(!out.status.success(), "killed: {}", text(&out));
        assert_eq!(model.calls(), 1, "rank ran");

        let mut resumed = spawn(
            &dir,
            &project,
            &["runs", "resume", "budget-1"],
            &with(&env, &[]),
            &dir.join("resumed.log"),
        );
        let rt = Durare::sqlite(&state.join("private/atomic.db"))
            .await
            .expect("store");
        let started = Instant::now();
        let park = loop {
            if let Some(p) = atomic_durable::parks(&rt, "budget-1")
                .await
                .expect("parks")
                .into_iter()
                .next()
            {
                break p;
            }
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "the resumed run did not park on its budget: {} model calls; {}",
                model.calls(),
                std::fs::read_to_string(dir.join("resumed.log")).unwrap_or_default()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert!(park.reason.contains("run:budget-1"), "{}", park.reason);
        assert_eq!(model.calls(), 1, "summarize was not called");
        resumed.kill().expect("stop the resume");
        resumed.wait().expect("reap");
    });
}

#[test]
fn an_unresolved_effect_stays_parked_across_resumes() {
    let dir = scratch("parked-effect");
    let project = project(&dir);
    // Killed after its command ran and before its step's checkpoint.
    let out = atomic(
        &dir,
        &project,
        &["run", "count-only", "--run-id", "effect-1", "--data", NOTE],
        &[("ATOMIC_FAILPOINT", "pipeline:ran:count")],
        "",
    );
    assert!(!out.status.success(), "killed: {}", text(&out));
    assert_eq!(count_log(&dir).len(), 1);
    // The command declares no effects, so resuming parks it instead of
    // running it again, and later resumes leave it parked.
    for expected in ["run.failed", "run.failed"] {
        let out = atomic(
            &dir,
            &project,
            &["--json", "runs", "resume", "effect-1"],
            &UNUSED_MODEL,
            "",
        );
        assert!(!out.status.success(), "{}", text(&out));
        let error = &stdout_json(&out)["error"];
        assert_eq!(error["kind"], expected, "{error}");
        assert!(
            error["description"]
                .as_str()
                .is_some_and(|d| d.contains("outcome_unknown")),
            "{error}"
        );
        assert_eq!(count_log(&dir).len(), 1, "the command did not run again");
    }
}

#[test]
fn a_run_killed_at_a_gate_keeps_it_and_its_deadline_and_one_resume_runs_at_a_time() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())])
            .await
            .expect("fake model");
        let dir = scratch("gate");
        let project = project(&dir);
        let state = state_dir(&dir, &project);
        let env = model_env(&model.base_url());
        let mut first = spawn(
            &dir,
            &project,
            &["run", "gated", "--run-id", "gated-1", "--data", NOTE],
            &with(&env, &[]),
            &dir.join("first.log"),
        );
        let rt = Durare::sqlite(&state.join("private/atomic.db"))
            .await
            .expect("store");
        let opened = open_gate(&rt, "gated-1").await;
        assert!(opened.deadline.is_some());
        first.kill().expect("SIGKILL the run");
        first.wait().expect("reap");

        let mut resumed = spawn(
            &dir,
            &project,
            &["runs", "resume", "gated-1"],
            &with(&env, &[]),
            &dir.join("resumed.log"),
        );
        wait_until("the resume to start", || {
            std::fs::read_to_string(dir.join("resumed.log")).is_ok_and(|l| l.contains("resuming"))
        });
        // A second resume is refused while the first runs, and says who runs it.
        let (d, p, e) = (dir.clone(), project.clone(), env.clone());
        let out = tokio::task::spawn_blocking(move || {
            atomic(
                &d,
                &p,
                &["--json", "runs", "resume", "gated-1"],
                &with(&e, &[]),
                "",
            )
        })
        .await
        .expect("child");
        assert!(!out.status.success());
        let error = &stdout_json(&out)["error"];
        assert_eq!(error["kind"], "runtime.busy", "{error}");
        assert!(
            error["description"]
                .as_str()
                .is_some_and(|d| d.contains(&format!("pid {}", resumed.id()))
                    && d.contains("atomic runs resume")),
            "{error}"
        );

        // The gate is the one the killed run opened, with its deadline.
        let again = open_gate(&rt, "gated-1").await;
        assert_eq!(
            (again.id.clone(), again.opened_at, again.deadline),
            (opened.id.clone(), opened.opened_at, opened.deadline)
        );
        approve(&rt, &again).await;
        let status = tokio::task::spawn_blocking(move || wait_child(&mut resumed, "resume"))
            .await
            .expect("child");
        assert!(
            status.success(),
            "{}",
            std::fs::read_to_string(dir.join("resumed.log")).unwrap_or_default()
        );
        assert_eq!(
            rt.status("gated-1").await.expect("status"),
            Some(RunStatus::Succeeded)
        );
        assert_eq!(model.calls(), 2, "rank once, summarize once");
        assert_eq!(count_log(&dir).len(), 1);
    });
}

const DECIDER_STORE: &str = "ATOMIC_TEST_DECIDER_STORE";
const DECIDER_RUN: &str = "ATOMIC_TEST_DECIDER_RUN";

/// A second terminal approving a run's open gate: run by
/// [`decided_while_down`] in a process of its own, so a failpoint can stop
/// it after the decision is recorded and before the run is woken.
#[test]
#[ignore = "run by a_gate_decided_while_its_run_was_down_wakes_it_on_resume in a child process"]
fn decider_child() {
    let store = std::env::var(DECIDER_STORE).expect("store");
    let run = std::env::var(DECIDER_RUN).expect("run");
    on_codex_runtime(async move {
        let rt = Durare::sqlite(Path::new(&store)).await.expect("store");
        let g = open_gate(&rt, &run).await;
        approve(&rt, &g).await;
    });
}

/// Start `gated` as `id`, kill it at its gate, then approve the gate from a
/// decider that stops after recording its decision and before waking the
/// run.
async fn decided_while_down(
    dir: &Path,
    project: &Path,
    state: &Path,
    env: &[(&'static str, String)],
    id: &str,
) {
    let mut run = spawn(
        dir,
        project,
        &["run", "gated", "--run-id", id, "--data", NOTE],
        &with(env, &[]),
        &dir.join(format!("{id}.log")),
    );
    let rt = Durare::sqlite(&state.join("private/atomic.db"))
        .await
        .expect("store");
    open_gate(&rt, id).await;
    run.kill().expect("SIGKILL the run");
    run.wait().expect("reap");
    decide_without_waking(state, id).await;
}

/// Approve run `id`'s gate from a decider that stops after recording its
/// decision and before waking the run.
async fn decide_without_waking(state: &Path, id: &str) {
    let rt = Durare::sqlite(&state.join("private/atomic.db"))
        .await
        .expect("store");
    let decider = Command::new(std::env::current_exe().expect("this test binary"))
        .args(["--exact", "decider_child", "--ignored", "--nocapture"])
        .env(DECIDER_STORE, state.join("private/atomic.db"))
        .env(DECIDER_RUN, id)
        .env("ATOMIC_FAILPOINT", "gate:before-wake")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("decider");
    assert!(!decider.success(), "the decider stopped before the wake-up");
    let g = gate::find(&rt, &open_gate_id(&rt, id).await)
        .await
        .expect("gate")
        .expect("exists");
    assert!(g.resolution.is_some(), "the decision was recorded");
}

/// The id of run `run`'s gate, open or not.
async fn open_gate_id(rt: &Durare, run: &str) -> String {
    gate::gates(
        rt,
        GateFilter {
            open_only: false,
            run: Some(run.to_owned()),
        },
    )
    .await
    .expect("gates")
    .into_iter()
    .next()
    .expect("a gate")
    .id
}

#[test]
fn a_gate_decided_while_its_run_was_down_wakes_it_on_resume() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![
            Reply::Text(RANK.into()),
            Reply::Text(SUMMARY.into()),
            Reply::Text(RANK.into()),
            Reply::Text(RANK.into()),
            Reply::Text(SUMMARY.into()),
            Reply::Text(SUMMARY.into()),
        ])
        .await
        .expect("fake model");
        let dir = scratch("redeliver");
        let project = project(&dir);
        let state = state_dir(&dir, &project);
        let env = model_env(&model.base_url());

        // Resumed: it goes on without waiting for the gate's day-long deadline.
        decided_while_down(&dir, &project, &state, &env, "decided-1").await;
        let (d, p, e) = (dir.clone(), project.clone(), env.clone());
        let out = tokio::task::spawn_blocking(move || {
            atomic(
                &d,
                &p,
                &["--json", "runs", "resume", "decided-1"],
                &with(&e, &[]),
                "",
            )
        })
        .await
        .expect("child");
        assert!(out.status.success(), "{}", text(&out));
        assert_eq!(
            stdout_json(&out)["output"],
            json!({"line": "P1: Export crashes on accents"})
        );

        // Recovered by serve: the same. A third run waits at its gate, not
        // yet decided, when serve starts.
        decided_while_down(&dir, &project, &state, &env, "decided-2").await;
        let mut third = spawn(
            &dir,
            &project,
            &["run", "gated", "--run-id", "decided-3", "--data", NOTE],
            &with(&env, &[]),
            &dir.join("decided-3.log"),
        );
        {
            let rt = Durare::sqlite(&state.join("private/atomic.db"))
                .await
                .expect("store");
            open_gate(&rt, "decided-3").await;
        }
        third.kill().expect("SIGKILL the run");
        third.wait().expect("reap");
        let mut serve = spawn(
            &dir,
            &project,
            &["serve"],
            &with(&env, &[]),
            &dir.join("serve.log"),
        );
        let rt = Durare::sqlite(&state.join("private/atomic.db"))
            .await
            .expect("store");
        let started = Instant::now();
        while rt.status("decided-2").await.expect("status") != Some(RunStatus::Succeeded) {
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "serve left the run waiting at its decided gate: {}",
                std::fs::read_to_string(dir.join("serve.log")).unwrap_or_default()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // Decided while serve runs, by a decider that never woke it: serve
        // catches up on its own.
        wait_until("serve to start", || {
            std::fs::read_to_string(dir.join("serve.log"))
                .is_ok_and(|l| l.contains("serving until stopped"))
        });
        decide_without_waking(&state, "decided-3").await;
        let started = Instant::now();
        while rt.status("decided-3").await.expect("status") != Some(RunStatus::Succeeded) {
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "serve never woke the run decided while it ran"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(model.calls(), 6, "each run ranked and summarized once");
        terminate(&serve);
        let status = tokio::task::spawn_blocking(move || wait_child(&mut serve, "serve"))
            .await
            .expect("child");
        assert!(status.success());
    });
}

#[test]
fn serve_started_after_a_kill_recovers_the_run_and_finishes_it() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())])
            .await
            .expect("fake model");
        let dir = scratch("serve");
        let project = project(&dir);
        let state = state_dir(&dir, &project);
        let env = model_env(&model.base_url());
        let (d, p, e) = (dir.clone(), project.clone(), env.clone());
        let out = tokio::task::spawn_blocking(move || {
            atomic(
                &d,
                &p,
                &["run", "triage", "--run-id", "served-1", "--data", NOTE],
                &with(&e, &[("ATOMIC_FAILPOINT", "pipeline:after:rank")]),
                "",
            )
        })
        .await
        .expect("child");
        assert!(
            !out.status.success(),
            "killed at the failpoint: {}",
            text(&out)
        );

        let serve = spawn(
            &dir,
            &project,
            &["serve"],
            &with(&env, &[]),
            &dir.join("serve.log"),
        );
        let rt = Durare::sqlite(&state.join("private/atomic.db"))
            .await
            .expect("store");
        let started = Instant::now();
        while rt.status("served-1").await.expect("status") != Some(RunStatus::Succeeded) {
            assert!(
                started.elapsed() < Duration::from_secs(120),
                "serve never finished the run"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let out: Value = rt.result("served-1").await.expect("result");
        assert_eq!(out, json!({"line": "P1: Export crashes on accents"}));
        assert_eq!(model.calls(), 2, "rank once, summarize once");
        assert_eq!(count_log(&dir).len(), 1);
        let log = std::fs::read_to_string(dir.join("serve.log")).expect("log");
        assert!(log.contains("recovered 1 unfinished run(s)"), "{log}");

        // While serve runs, no other process executes the project's runs.
        let (d, p) = (dir.clone(), project.clone());
        let out = tokio::task::spawn_blocking(move || {
            atomic(
                &d,
                &p,
                &["--json", "run", "count-note", "--data", NOTE],
                &[],
                "",
            )
        })
        .await
        .expect("child");
        assert_eq!(stdout_json(&out)["error"]["kind"], "runtime.busy");

        terminate(&serve);
        let mut serve = serve;
        let status = tokio::task::spawn_blocking(move || wait_child(&mut serve, "serve"))
            .await
            .expect("child");
        assert!(status.success(), "serve stops cleanly on SIGTERM: {status}");
    });
}

/// A copy of the atomic executable that differs from it by one byte, as a
/// rebuild of the same source would.
fn rebuilt(dir: &Path) -> PathBuf {
    let copy = dir.join("bin/atomic");
    std::fs::create_dir_all(dir.join("bin")).expect("bin");
    let mut bytes = std::fs::read(ATOMIC).expect("read atomic");
    bytes.push(0);
    std::fs::write(&copy, bytes).expect("copy atomic");
    let mut permissions = std::fs::metadata(&copy).expect("copy").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&copy, permissions).expect("make the copy executable");
    copy
}

/// Run the triage workflow as `id` until the failpoint after `rank` kills it.
fn killed_after_rank(dir: &Path, project: &Path, env: &[(&'static str, String)], id: &str) {
    let out = atomic(
        dir,
        project,
        &["run", "triage", "--run-id", id, "--data", NOTE],
        &with(env, &[("ATOMIC_FAILPOINT", "pipeline:after:rank")]),
        "",
    );
    assert!(
        !out.status.success(),
        "killed at the failpoint: {}",
        text(&out)
    );
}

#[test]
fn a_run_started_by_one_build_resumes_and_recovers_under_another() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![
            Reply::Text(RANK.into()),
            Reply::Text(SUMMARY.into()),
            Reply::Text(RANK.into()),
            Reply::Text(SUMMARY.into()),
        ])
        .await
        .expect("fake model");
        let dir = scratch("rebuilt");
        let project = project(&dir);
        let state = state_dir(&dir, &project);
        let env = model_env(&model.base_url());
        let other = rebuilt(&dir);

        // Resumed by the other build.
        let (d, p, e) = (dir.clone(), project.clone(), env.clone());
        tokio::task::spawn_blocking(move || killed_after_rank(&d, &p, &e, "built-1"))
            .await
            .expect("child");
        let (d, p, e, o) = (dir.clone(), project.clone(), env.clone(), other.clone());
        let out = tokio::task::spawn_blocking(move || {
            atomic_as(
                &o,
                &d,
                &p,
                &["--json", "runs", "resume", "built-1"],
                &with(&e, &[]),
                "",
            )
        })
        .await
        .expect("child");
        assert!(out.status.success(), "{}", text(&out));
        assert_eq!(
            stdout_json(&out)["output"],
            json!({"line": "P1: Export crashes on accents"})
        );
        assert_eq!(model.calls(), 2, "only summarize ran again");

        // Recovered by the other build's server.
        let (d, p, e) = (dir.clone(), project.clone(), env.clone());
        tokio::task::spawn_blocking(move || killed_after_rank(&d, &p, &e, "built-2"))
            .await
            .expect("child");
        let mut serve = spawn_as(
            &other,
            &dir,
            &project,
            &["serve"],
            &with(&env, &[]),
            &dir.join("serve.log"),
        );
        let rt = Durare::sqlite(&state.join("private/atomic.db"))
            .await
            .expect("store");
        let started = Instant::now();
        while rt.status("built-2").await.expect("status") != Some(RunStatus::Succeeded) {
            assert!(
                started.elapsed() < Duration::from_secs(120),
                "the other build's server never finished the run: {}",
                std::fs::read_to_string(dir.join("serve.log")).unwrap_or_default()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(model.calls(), 4);
        assert_eq!(count_log(&dir).len(), 2, "each run counted once");
        terminate(&serve);
        let status = tokio::task::spawn_blocking(move || wait_child(&mut serve, "serve"))
            .await
            .expect("child");
        assert!(status.success());
    });
}

#[test]
fn a_run_of_another_durable_layout_is_reported_not_resumed() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(RANK.into())])
            .await
            .expect("fake model");
        let dir = scratch("layout");
        let project = project(&dir);
        let state = state_dir(&dir, &project);
        let env = model_env(&model.base_url());
        let (d, p, e) = (dir.clone(), project.clone(), env.clone());
        tokio::task::spawn_blocking(move || killed_after_rank(&d, &p, &e, "old-1"))
            .await
            .expect("child");
        // As if an atomic of an earlier layout had started it.
        let db = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            state.join("private/atomic.db").display()
        ))
        .await
        .expect("store");
        sqlx::query(
            "UPDATE workflow_status SET application_version = 'atomic.pipeline.0' \
             WHERE workflow_uuid = 'old-1'",
        )
        .execute(&db)
        .await
        .expect("restamp");

        let (d, p, e) = (dir.clone(), project.clone(), env.clone());
        let out = tokio::task::spawn_blocking(move || {
            atomic(
                &d,
                &p,
                &["--json", "runs", "resume", "old-1"],
                &with(&e, &[]),
                "",
            )
        })
        .await
        .expect("child");
        assert!(!out.status.success(), "{}", text(&out));
        let error = &stdout_json(&out)["error"];
        assert_eq!(error["kind"], "run.other_layout", "{error}");
        assert!(
            error["description"]
                .as_str()
                .is_some_and(|d| d.contains("atomic.pipeline.0")),
            "{error}"
        );

        let mut serve = spawn(
            &dir,
            &project,
            &["serve"],
            &with(&env, &[]),
            &dir.join("serve.log"),
        );
        wait_until("serve to recover", || {
            std::fs::read_to_string(dir.join("serve.log")).is_ok_and(|l| l.contains("recovered"))
        });
        let log = std::fs::read_to_string(dir.join("serve.log")).expect("log");
        assert!(
            log.contains("old-1 (layout atomic.pipeline.0)")
                && log.contains("recovered 0 unfinished run(s)"),
            "{log}"
        );
        terminate(&serve);
        let status = tokio::task::spawn_blocking(move || wait_child(&mut serve, "serve"))
            .await
            .expect("child");
        assert!(status.success());
        assert_eq!(model.calls(), 1, "nothing ran again");
        assert_eq!(count_log(&dir).len(), 1);
    });
}

#[test]
fn a_resumed_run_left_queued_is_taken_by_no_other_command() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![
            Reply::Text(RANK.into()),
            Reply::Text(RANK.into()),
            Reply::Text(SUMMARY.into()),
            Reply::Text(SUMMARY.into()),
        ])
        .await
        .expect("fake model");
        let dir = scratch("left-queued");
        let project = project(&dir);
        let state = state_dir(&dir, &project);
        let env = model_env(&model.base_url());
        let (d, p, e) = (dir.clone(), project.clone(), env.clone());
        tokio::task::spawn_blocking(move || killed_after_rank(&d, &p, &e, "left-1"))
            .await
            .expect("child");
        let (d, p, e) = (dir.clone(), project.clone(), env.clone());
        tokio::task::spawn_blocking(move || killed_after_rank(&d, &p, &e, "other-1"))
            .await
            .expect("child");
        // The resume puts the run on its queue and dies before running it.
        let (d, p, e) = (dir.clone(), project.clone(), env.clone());
        let out = tokio::task::spawn_blocking(move || {
            atomic(
                &d,
                &p,
                &["runs", "resume", "left-1"],
                &with(&e, &[("ATOMIC_FAILPOINT", "resume:queued")]),
                "",
            )
        })
        .await
        .expect("child");
        assert!(!out.status.success(), "killed: {}", text(&out));

        // An unrelated run starts and finishes, and leaves it alone.
        let (d, p, e) = (dir.clone(), project.clone(), env.clone());
        let out = tokio::task::spawn_blocking(move || {
            atomic(
                &d,
                &p,
                &["run", "count-note", "--data", NOTE],
                &with(&e, &[]),
                "",
            )
        })
        .await
        .expect("child");
        assert!(out.status.success(), "{}", text(&out));
        let rt = Durare::sqlite(&state.join("private/atomic.db"))
            .await
            .expect("store");
        assert_eq!(
            rt.status("left-1").await.expect("status"),
            Some(RunStatus::Pending)
        );
        assert_eq!(model.calls(), 2, "atomic run did not run it");

        // Resuming another run of the same executor leaves it alone too.
        let (d, p, e) = (dir.clone(), project.clone(), env.clone());
        let out = tokio::task::spawn_blocking(move || {
            atomic(&d, &p, &["runs", "resume", "other-1"], &with(&e, &[]), "")
        })
        .await
        .expect("child");
        assert!(out.status.success(), "{}", text(&out));
        assert_eq!(
            rt.status("left-1").await.expect("status"),
            Some(RunStatus::Pending)
        );
        assert_eq!(model.calls(), 3, "only other-1's summary");

        // serve takes it from the queue and finishes it.
        let mut serve = spawn(
            &dir,
            &project,
            &["serve"],
            &with(&env, &[]),
            &dir.join("serve.log"),
        );
        let started = Instant::now();
        while rt.status("left-1").await.expect("status") != Some(RunStatus::Succeeded) {
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "serve never finished the run: {}",
                std::fs::read_to_string(dir.join("serve.log")).unwrap_or_default()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(model.calls(), 4);
        assert_eq!(
            count_log(&dir).len(),
            3,
            "each triage's count once, count-note once"
        );
        terminate(&serve);
        let status = tokio::task::spawn_blocking(move || wait_child(&mut serve, "serve"))
            .await
            .expect("child");
        assert!(status.success());
    });
}

#[test]
fn a_run_id_that_already_exists_is_refused() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(RANK.into())])
            .await
            .expect("fake model");
        let dir = scratch("reused");
        let project = project(&dir);
        let env = model_env(&model.base_url());
        let refused = |args: Vec<String>, env: Vec<(&'static str, String)>| {
            let (d, p) = (dir.clone(), project.clone());
            tokio::task::spawn_blocking(move || {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                atomic(&d, &p, &args, &with(&env, &[]), "")
            })
        };
        // A finished run's id, for the same atom or another workflow.
        let out = atomic(
            &dir,
            &project,
            &["run", "count-note", "--run-id", "done-1", "--data", NOTE],
            &[],
            "",
        );
        assert!(out.status.success(), "{}", text(&out));
        for target in ["count-note", "count-only"] {
            let args = [
                "--json", "run", target, "--run-id", "done-1", "--data", NOTE,
            ];
            let out = refused(args.map(str::to_owned).to_vec(), env.to_vec())
                .await
                .expect("child");
            assert!(!out.status.success(), "{}", text(&out));
            let error = &stdout_json(&out)["error"];
            assert_eq!(error["kind"], "run.exists", "{target}: {error}");
            assert!(
                error["action"]
                    .as_str()
                    .is_some_and(|a| a.contains("atomic runs resume done-1")),
                "{error}"
            );
        }
        assert_eq!(count_log(&dir).len(), 1, "nothing ran again");

        // An unfinished one's: refused, not waited on.
        let (d, p, e) = (dir.clone(), project.clone(), env.clone());
        tokio::task::spawn_blocking(move || killed_after_rank(&d, &p, &e, "pending-1"))
            .await
            .expect("child");
        let args = [
            "--json",
            "run",
            "triage",
            "--run-id",
            "pending-1",
            "--data",
            NOTE,
        ];
        let out = refused(args.map(str::to_owned).to_vec(), env.to_vec())
            .await
            .expect("child");
        let error = &stdout_json(&out)["error"];
        assert_eq!(error["kind"], "run.exists", "{error}");
        assert!(
            error["description"]
                .as_str()
                .is_some_and(|d| d.contains("pending-1") && d.contains("unfinished")),
            "{error}"
        );
        assert_eq!(model.calls(), 1, "nothing ran again");
        assert_eq!(count_log(&dir).len(), 2);
    });
}

#[test]
fn a_cancelled_run_is_not_resumed_and_a_parked_one_is() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![
            Reply::Text(RANK.into()),
            Reply::Text(RANK.into()),
            Reply::Text(SUMMARY.into()),
        ])
        .await
        .expect("fake model");
        let dir = scratch("cancelled");
        let project = project(&dir);
        let state = state_dir(&dir, &project);
        let env = model_env(&model.base_url());
        let db_url = format!("sqlite://{}", state.join("private/atomic.db").display());
        for (id, status) in [
            ("cancelled-1", "CANCELLED"),
            ("parked-1", "MAX_RECOVERY_ATTEMPTS_EXCEEDED"),
        ] {
            let (d, p, e) = (dir.clone(), project.clone(), env.clone());
            tokio::task::spawn_blocking(move || killed_after_rank(&d, &p, &e, id))
                .await
                .expect("child");
            let db = sqlx::SqlitePool::connect(&db_url).await.expect("store");
            sqlx::query("UPDATE workflow_status SET status = ? WHERE workflow_uuid = ?")
                .bind(status)
                .bind(id)
                .execute(&db)
                .await
                .expect("set the status");
        }
        let resume = |id: &'static str| {
            let (d, p, e) = (dir.clone(), project.clone(), env.clone());
            tokio::task::spawn_blocking(move || {
                atomic(
                    &d,
                    &p,
                    &["--json", "runs", "resume", id],
                    &with(&e, &[]),
                    "",
                )
            })
        };
        let out = resume("cancelled-1").await.expect("child");
        assert!(!out.status.success(), "{}", text(&out));
        let error = &stdout_json(&out)["error"];
        assert_eq!(error["kind"], "run.not_resumable", "{error}");
        assert!(
            error["description"]
                .as_str()
                .is_some_and(|d| d.contains("CANCELLED")),
            "{error}"
        );
        assert_eq!(model.calls(), 2, "nothing ran again");
        // A run the engine parked for its recovery count goes on.
        let out = resume("parked-1").await.expect("child");
        assert!(out.status.success(), "{}", text(&out));
        assert_eq!(
            stdout_json(&out)["output"],
            json!({"line": "P1: Export crashes on accents"})
        );
        assert_eq!(model.calls(), 3);
    });
}

#[test]
fn exec_commands_run_in_a_runner_process_when_asked() {
    let dir = scratch("runner");
    let project = project(&dir);
    let local = atomic(
        &dir,
        &project,
        &["run", "count-note", "--data", NOTE],
        &[],
        "",
    );
    let split = atomic(
        &dir,
        &project,
        &["run", "count-note", "--runner", "process", "--data", NOTE],
        &[],
        "",
    );
    for out in [&local, &split] {
        assert!(out.status.success(), "{}", text(out));
    }
    assert_eq!(stdout_json(&local), stdout_json(&split));
    let log = count_log(&dir);
    assert_eq!(log.len(), 2);
    assert!(log[0].contains(" run count-note"), "{}", log[0]);
    assert!(
        log[1].ends_with(" runner"),
        "the runner started it: {}",
        log[1]
    );

    // The runner process itself has only PATH from atomic's environment,
    // which has HOME, USER and ATOMIC_HOME too.
    std::fs::write(
        project.join("tools/parent-env.py"),
        "import json, os, sys\n\
         json.load(sys.stdin)\n\
         names = [e.split(b'=')[0].decode() for e in open(f'/proc/{os.getppid()}/environ', 'rb').read().split(b'\\0') if e]\n\
         json.dump({'names': sorted(names)}, sys.stdout)\n",
    )
    .expect("script");
    write_atom(
        &project,
        "parent-env",
        "description: 'Name the parent environment.', kind: 'exec', exec: { command: ['python3', 'tools/parent-env.py'] }",
        "",
        object(json!({"text": {"type": "string"}})),
        object(json!({"names": {"type": "array", "items": {"type": "string"}}})),
    );
    let out = atomic(
        &dir,
        &project,
        &["run", "parent-env", "--runner", "process", "--data", NOTE],
        &[],
        "",
    );
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(stdout_json(&out), json!({"names": ["PATH"]}));
}

/// An exec atom whose command logs that it ran, then, the first time or
/// `always`, SIGKILLs the process that started it: with `--runner process`,
/// its runner.
fn write_runner_killer(project: &Path, dir: &Path, name: &str, effects: &str, always: bool) {
    std::fs::write(
        project.join("tools/kill-runner.py"),
        "import json, os, signal, sys\n\
         note = json.load(sys.stdin)\n\
         log = os.environ['KILL_LOG']\n\
         open(log, 'a').write('ran\\n')\n\
         if os.environ.get('KILL_ALWAYS') or not os.path.exists(log + '.killed'):\n\
         \x20   open(log + '.killed', 'w').close()\n\
         \x20   os.kill(os.getppid(), signal.SIGKILL)\n\
         json.dump({'title': note['text'][:25], 'words': len(note['text'].split())}, sys.stdout)\n",
    )
    .expect("script");
    write_atom(
        project,
        name,
        &format!(
            "description: 'Kill the runner.', kind: 'exec', exec: {{ command: ['python3', 'tools/kill-runner.py'] }}, env: {{ KILL_LOG: '{}'{} }}{effects}",
            dir.join(format!("{name}.log")).display(),
            if always { ", KILL_ALWAYS: '1'" } else { "" }
        ),
        "",
        object(json!({"text": {"type": "string"}})),
        object(json!({"title": {"type": "string"}, "words": {"type": "integer"}})),
    );
}

fn lines(file: &Path) -> usize {
    std::fs::read_to_string(file)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

#[test]
fn a_command_that_kills_its_runner_is_never_run_again_unless_it_may_be() {
    let dir = scratch("runner-killed");
    let project = project(&dir);
    write_runner_killer(&project, &dir, "undeclared", "", false);
    write_runner_killer(
        &project,
        &dir,
        "idempotent",
        ", effects: { external: 'idempotent' }",
        false,
    );
    // Undeclared: it may have done what it does, so it parks after one run.
    let out = atomic(
        &dir,
        &project,
        &[
            "--json",
            "run",
            "undeclared",
            "--runner",
            "process",
            "--data",
            NOTE,
        ],
        &[],
        "",
    );
    assert!(!out.status.success(), "{}", text(&out));
    let error = &stdout_json(&out)["error"];
    assert_eq!(error["kind"], "run.failed", "{error}");
    assert!(
        error["description"]
            .as_str()
            .is_some_and(|d| d.contains("outcome_unknown")),
        "{error}"
    );
    assert_eq!(lines(&dir.join("undeclared.log")), 1, "it ran once");

    // Declared idempotent: the step is retried, and the second run finishes.
    let out = atomic(
        &dir,
        &project,
        &["run", "idempotent", "--runner", "process", "--data", NOTE],
        &[],
        "",
    );
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        stdout_json(&out),
        json!({"title": "Exporting a file named ca", "words": 8})
    );
    assert_eq!(
        lines(&dir.join("idempotent.log")),
        2,
        "once more, as declared"
    );
}

/// An idempotent exec atom whose command starts a child that sleeps, logs
/// `start <pid> <child pid>`, then waits a minute before it would answer.
fn write_slow(project: &Path, dir: &Path) {
    std::fs::write(
        project.join("tools/slow.py"),
        "import json, os, subprocess, sys, time\n\
         note = json.load(sys.stdin)\n\
         child = subprocess.Popen(['sleep', '60'])\n\
         open(os.environ['SLOW_LOG'], 'a').write(f'start {os.getpid()} {child.pid}\\n')\n\
         time.sleep(60)\n\
         open(os.environ['SLOW_LOG'], 'a').write('end\\n')\n\
         json.dump({'title': note['text'][:25], 'words': len(note['text'].split())}, sys.stdout)\n",
    )
    .expect("script");
    write_atom(
        project,
        "slow",
        &format!(
            "description: 'Take a minute.', kind: 'exec', exec: {{ command: ['python3', 'tools/slow.py'] }}, env: {{ SLOW_LOG: '{}' }}, effects: {{ external: 'idempotent' }}",
            dir.join("slow.log").display()
        ),
        "",
        object(json!({"text": {"type": "string"}})),
        object(json!({"title": {"type": "string"}, "words": {"type": "integer"}})),
    );
}

const UNUSED_MODEL: [(&str, &str); 2] = [
    ("ATOMIC_MODEL_BASE_URL", "http://127.0.0.1:9/v1"),
    ("ATOMIC_MODEL", "unused"),
];

/// For each start of the slow command, its pid and its child's.
fn slow_starts(dir: &Path) -> Vec<[String; 2]> {
    std::fs::read_to_string(dir.join("slow.log"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (pid, child) = l.strip_prefix("start ")?.split_once(' ')?;
            Some([pid.to_owned(), child.to_owned()])
        })
        .collect()
}

/// Whether serve has said it stopped or failed.
fn serve_ended(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("serve.log"))
        .is_ok_and(|l| l.contains("error") || l.contains("stopping"))
}

/// Whether process `pid` still runs: `kill -0` reaches it, and on Linux it
/// is not a zombie waiting to be reaped.
fn alive(pid: &str) -> bool {
    let reached = Command::new("/bin/sh")
        .args(["-c", &format!("kill -0 {pid} 2>/dev/null")])
        .status()
        .expect("kill")
        .success();
    let zombie = || {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit(')')
                .next()
                .unwrap_or("")
                .trim_start()
                .starts_with('Z')
        })
    };
    reached && !(cfg!(target_os = "linux") && zombie())
}

/// Wait until none of `pids` runs.
fn all_gone(pids: &[String], what: &str) {
    let started = Instant::now();
    while let Some(pid) = pids.iter().find(|pid| alive(pid)) {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{what}: process {pid} survived"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Whether another process holds the project's execution lock.
fn lock_held(state: &Path) -> bool {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(state.join("private/runtime.lock"))
        .expect("the lock file");
    matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

/// Send `signal` (such as `TERM`) to `child`.
fn signal(child: &Child, signal: &str) {
    let status = Command::new("/bin/sh")
        .args(["-c", &format!("kill -{signal} {}", child.id())])
        .status()
        .expect("kill");
    assert!(status.success());
}

/// Start the slow atom as run `id` and SIGKILL `atomic run` while its
/// command runs. Nothing can catch SIGKILL: on Linux the command's
/// parent-death signal stops it, but not its child, which this kills.
/// Returns the project's state.
fn slow_run_killed_mid_command(dir: &Path, project: &Path, id: &str) -> PathBuf {
    write_slow(project, dir);
    let state = state_dir(dir, project);
    let mut run = spawn(
        dir,
        project,
        &["run", "slow", "--run-id", id, "--data", NOTE],
        &[],
        &dir.join("run.log"),
    );
    wait_until("the command to start", || slow_starts(dir).len() == 1);
    run.kill().expect("SIGKILL atomic run");
    run.wait().expect("reap");
    let [pid, child] = slow_starts(dir)[0].clone();
    if cfg!(target_os = "linux") {
        // Well before it would end by itself.
        all_gone(std::slice::from_ref(&pid), "the command outlived atomic");
    }
    let _ = Command::new("/bin/sh")
        .args(["-c", &format!("kill -KILL {pid} {child} 2>/dev/null")])
        .status();
    all_gone(&[pid, child], "cleaning up");
    state
}

/// Stop `child` with `sig` and then again, and check it stops at once with
/// `code`, after saying it is stopping in `log`.
fn stop_twice(child: &mut Child, sig: &str, log: &Path, code: i32) -> String {
    signal(child, sig);
    wait_until("it to begin stopping", || {
        std::fs::read_to_string(log).is_ok_and(|l| l.contains("stopping;"))
    });
    let again = Instant::now();
    signal(child, sig);
    let status = wait_child(child, "the stopped process");
    let text = std::fs::read_to_string(log).expect("log");
    assert!(
        again.elapsed() < Duration::from_secs(5),
        "the second signal stops it at once: {text}"
    );
    assert_eq!(status.code(), Some(code), "{text}");
    assert!(text.contains("stopping now"), "{text}");
    text
}

#[test]
fn serve_stopped_mid_command_holds_the_lock_until_it_exits_and_runs_nothing_twice() {
    let dir = scratch("serve-stop");
    let project = project(&dir);
    let state = slow_run_killed_mid_command(&dir, &project, "slow-1");
    // serve needs a model endpoint, though nothing here calls one.
    let mut serve = spawn(
        &dir,
        &project,
        &["serve"],
        &UNUSED_MODEL,
        &dir.join("serve.log"),
    );
    wait_until("serve to start the command again", || {
        slow_starts(&dir).len() == 2 || serve_ended(&dir)
    });
    assert_eq!(
        slow_starts(&dir).len(),
        2,
        "{}",
        std::fs::read_to_string(dir.join("serve.log")).unwrap_or_default()
    );
    let pids = slow_starts(&dir)[1].clone();
    terminate(&serve);
    // Until serve exits, the lock stays held and the command stays its own.
    let stopping = Instant::now();
    let status = loop {
        let held = lock_held(&state);
        if let Some(status) = serve.try_wait().expect("wait") {
            break status;
        }
        assert!(held, "the lock was free while serve still ran");
        assert!(
            stopping.elapsed() < Duration::from_secs(30),
            "serve did not stop"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let log = std::fs::read_to_string(dir.join("serve.log")).expect("log");
    assert!(
        stopping.elapsed() >= Duration::from_secs(9),
        "running steps had their time to checkpoint: {log}"
    );
    assert!(!status.success(), "a command was stopped: {log}");
    assert!(log.contains("stopped 1 command(s)"), "{log}");
    all_gone(&pids, "the stopped command and its child");
    assert!(!lock_held(&state), "the lock went with the process");
    let ends = std::fs::read_to_string(dir.join("slow.log")).expect("log");
    assert!(!ends.contains("end"), "no command ran to its end: {ends}");
    assert_eq!(slow_starts(&dir).len(), 2, "started twice, never at once");
}

#[test]
fn a_second_signal_stops_serve_at_once_with_everything_its_command_started() {
    // With the command a child of serve, and in a runner process of its own.
    for runner in ["local", "process"] {
        let dir = scratch("serve-twice");
        let project = project(&dir);
        let state = slow_run_killed_mid_command(&dir, &project, "slow-2");
        let log = dir.join("serve.log");
        let mut serve = spawn(
            &dir,
            &project,
            &["serve", "--runner", runner],
            &UNUSED_MODEL,
            &log,
        );
        wait_until("serve to start the command again", || {
            slow_starts(&dir).len() == 2 || serve_ended(&dir)
        });
        assert_eq!(
            slow_starts(&dir).len(),
            2,
            "{runner}: {}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        let pids = slow_starts(&dir)[1].clone();
        if runner == "process" && cfg!(target_os = "linux") {
            // Field 4 of /proc/<pid>/stat: the parent.
            let stat = std::fs::read_to_string(format!("/proc/{}/stat", pids[0])).expect("stat");
            let parent = stat
                .rsplit(')')
                .next()
                .unwrap_or("")
                .split_whitespace()
                .nth(1);
            assert_ne!(
                parent,
                Some(serve.id().to_string().as_str()),
                "a runner started it"
            );
        }
        terminate(&serve);
        wait_until("serve to begin stopping", || {
            std::fs::read_to_string(&log).is_ok_and(|l| l.contains("stopping;"))
        });
        assert!(lock_held(&state));
        let text = stop_twice(&mut serve, "TERM", &log, 1);
        assert!(text.contains("stopped 1 command(s)"), "{runner}: {text}");
        all_gone(&pids, runner);
        assert!(!lock_held(&state));
        assert_eq!(slow_starts(&dir).len(), 2);
    }
}

#[test]
fn a_signal_stops_run_and_resume_with_everything_their_command_started() {
    for (command, sig) in [
        ("run", "INT"),
        ("run", "TERM"),
        ("resume", "INT"),
        ("resume", "TERM"),
    ] {
        let dir = scratch("stopped");
        let project = project(&dir);
        let log = dir.join("stopped.log");
        let (mut child, starts) = if command == "run" {
            write_slow(&project, &dir);
            let child = spawn(
                &dir,
                &project,
                &["run", "slow", "--run-id", "slow-3", "--data", NOTE],
                &[],
                &log,
            );
            (child, 1)
        } else {
            slow_run_killed_mid_command(&dir, &project, "slow-3");
            let child = spawn(
                &dir,
                &project,
                &["runs", "resume", "slow-3"],
                &UNUSED_MODEL,
                &log,
            );
            (child, 2)
        };
        wait_until("the command to start", || slow_starts(&dir).len() == starts);
        let pids = slow_starts(&dir)[starts - 1].clone();
        let text = stop_twice(&mut child, sig, &log, 130);
        assert!(
            text.contains("atomic runs resume slow-3"),
            "{command} {sig}: {text}"
        );
        all_gone(&pids, &format!("{command} {sig}"));
    }
}

#[test]
fn a_run_that_keeps_stopping_is_dead_lettered_after_a_few_tries() {
    let dir = scratch("dead-letter");
    let project = project(&dir);
    // Idempotent, so each recovery may run it again, and its runner always
    // dies under it.
    write_runner_killer(
        &project,
        &dir,
        "doomed",
        ", effects: { external: 'idempotent' }",
        true,
    );
    let out = atomic(
        &dir,
        &project,
        &[
            "--json", "run", "doomed", "--runner", "process", "--data", NOTE,
        ],
        &[],
        "",
    );
    assert!(!out.status.success(), "{}", text(&out));
    let error = &stdout_json(&out)["error"];
    assert_eq!(error["kind"], "run.dead_lettered", "{error}");
    let action = error["action"].as_str().unwrap_or_default();
    assert!(!action.contains("runs resume"), "{error}");
    let ran = lines(&dir.join("doomed.log"));
    assert_eq!(
        ran,
        1 + atomic_app::pipeline::RECOVERY_LIMIT as usize,
        "once, then once per recovery"
    );
}

/// A workflow of two undeclared exec steps: `first` logs `first start`,
/// takes three seconds and logs `first end`; `second` logs `second`.
fn write_two_steps(project: &Path, dir: &Path) {
    let log = dir.join("steps.log");
    for (name, body) in [
        (
            "first",
            "log('first start')\ntime.sleep(3)\nlog('first end')\n",
        ),
        ("second", "log('second')\n"),
    ] {
        std::fs::write(
            project.join(format!("tools/{name}.py")),
            format!(
                "import json, os, sys, time\n\
                 def log(line): open(os.environ['STEPS_LOG'], 'a').write(line + '\\n')\n\
                 note = json.load(sys.stdin)\n\
                 {body}\
                 json.dump({{'title': note['text'][:25], 'words': len(note['text'].split())}}, sys.stdout)\n"
            ),
        )
        .expect("script");
        write_atom(
            project,
            name,
            &format!(
                "description: '{name}', kind: 'exec', exec: {{ command: ['python3', 'tools/{name}.py'] }}, env: {{ STEPS_LOG: '{}' }}",
                log.display()
            ),
            "",
            object(json!({"text": {"type": "string"}})),
            object(json!({"title": {"type": "string"}, "words": {"type": "integer"}})),
        );
    }
    std::fs::write(
        project.join("workflows/two-steps.workflow.yaml"),
        "atomic: 1\nname: two-steps\n\
         input: { $ref: ../atoms/first.input.schema.json }\n\
         output: { $ref: ../atoms/second.output.schema.json }\n\
         steps:\n  first:\n    atom: first\n    with: { text: input.text }\n\
         \x20 second:\n    after: [first]\n    atom: second\n    with: { text: steps.first.output.title }\n\
         returns: steps.second.output\n",
    )
    .expect("workflow");
}

fn steps_log(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("steps.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn no_step_starts_after_the_first_signal() {
    let dir = scratch("no-new-steps");
    let project = project(&dir);
    write_two_steps(&project, &dir);
    let log = dir.join("run.log");
    let mut run = spawn(
        &dir,
        &project,
        &["run", "two-steps", "--run-id", "two-1", "--data", NOTE],
        &[],
        &log,
    );
    wait_until("the first step to start", || {
        steps_log(&dir).contains(&"first start".to_owned())
    });
    signal(&run, "TERM");
    let status = wait_child(&mut run, "atomic run");
    let said = std::fs::read_to_string(&log).expect("log");
    assert_eq!(status.code(), Some(130), "{said}");
    // The running step finished and was recorded; nothing began after it.
    assert_eq!(steps_log(&dir), ["first start", "first end"], "{said}");
    assert!(!said.contains("stopped 1 command"), "{said}");

    // The run goes on from the step after it.
    let out = atomic(
        &dir,
        &project,
        &["runs", "resume", "two-1"],
        &UNUSED_MODEL,
        "",
    );
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(steps_log(&dir), ["first start", "first end", "second"]);
}

#[test]
fn serve_restarts_do_not_park_a_healthy_run() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())])
            .await
            .expect("fake model");
        let dir = scratch("restarts");
        let project = project(&dir);
        let state = state_dir(&dir, &project);
        let env = model_env(&model.base_url());
        let mut first = spawn(
            &dir,
            &project,
            &["run", "gated", "--run-id", "waiting-1", "--data", NOTE],
            &with(&env, &[]),
            &dir.join("first.log"),
        );
        let rt = Durare::sqlite(&state.join("private/atomic.db"))
            .await
            .expect("store");
        let opened = open_gate(&rt, "waiting-1").await;
        first.kill().expect("SIGKILL the run");
        first.wait().expect("reap");
        // More restarts than one process may recover a run.
        for n in 0..(atomic_app::pipeline::RECOVERY_LIMIT + 2) {
            let log = dir.join(format!("serve-{n}.log"));
            let mut serve = spawn(&dir, &project, &["serve"], &with(&env, &[]), &log);
            wait_until("serve to recover", || {
                std::fs::read_to_string(&log).is_ok_and(|l| l.contains("serving until stopped"))
            });
            terminate(&serve);
            let (d, l) = (dir.clone(), log.clone());
            let status = tokio::task::spawn_blocking(move || wait_child(&mut serve, "serve"))
                .await
                .expect("child");
            let _ = d;
            let text = std::fs::read_to_string(&l).unwrap_or_default();
            assert!(status.success(), "{text}");
            assert!(!text.contains("parked"), "restart {n}: {text}");
        }
        assert_eq!(
            rt.status("waiting-1").await.expect("status"),
            Some(RunStatus::Pending),
            "still waiting at its gate"
        );
        approve(&rt, &open_gate(&rt, "waiting-1").await).await;
        let mut serve = spawn(
            &dir,
            &project,
            &["serve"],
            &with(&env, &[]),
            &dir.join("serve-last.log"),
        );
        let started = Instant::now();
        while rt.status("waiting-1").await.expect("status") != Some(RunStatus::Succeeded) {
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "the approved run never finished"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let _ = opened;
        assert_eq!(model.calls(), 2, "rank once, summarize once");
        terminate(&serve);
        let status = tokio::task::spawn_blocking(move || wait_child(&mut serve, "serve"))
            .await
            .expect("child");
        assert!(status.success());
    });
}

#[test]
fn the_runner_role_keeps_its_exit_code_contract() {
    let dir = scratch("runner-codes");
    // A request it cannot read is a usage error, and nothing is written.
    let out = atomic(&dir, &dir, &["runner"], &[], "not a request");
    assert_eq!(out.status.code(), Some(64), "{}", text(&out));
    assert!(out.stdout.is_empty());
    let out = atomic(&dir, &dir, &["runner", "--bogus"], &[], "");
    assert_eq!(out.status.code(), Some(64), "{}", text(&out));
    // A request it can read runs, and the answer says how the command ended.
    let request = json!({
        "protocol": 1, "program": "/bin/sh", "args": ["-c", "cat; exit 3"],
        "cwd": "/", "env": [], "stdin": "6869", "time_limit_ms": 10000, "output_limit": 1024
    });
    let out = atomic(&dir, &dir, &["runner"], &[], &request.to_string());
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(
        stdout_json(&out),
        json!({"outcome": "completed", "code": 3, "stdout": "6869", "stderr": ""})
    );
    // Nor after a global flag.
    let out = atomic(&dir, &dir, &["--json", "runner"], &[], &request.to_string());
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(stdout_json(&out)["code"], 3);
    // The runner never makes a state directory.
    assert!(!dir.join("atomic-home").exists());
}

#[test]
fn the_executor_identity_is_atomics_own_and_required_on_postgres() {
    let dir = scratch("identity");
    let project = project(&dir);
    let model = [
        ("ATOMIC_MODEL_BASE_URL", "http://127.0.0.1:9/v1"),
        ("ATOMIC_MODEL", "unused"),
    ];
    let mut env = model.to_vec();
    env.push(("ATOMIC_DATABASE_URL", "postgres://atomic@127.0.0.1:9/none"));
    let out = atomic(&dir, &project, &["--json", "serve"], &env, "");
    assert!(!out.status.success());
    let error = &stdout_json(&out)["error"];
    assert_eq!(error["kind"], "config.executor_id", "{error}");
    assert!(
        error["action"]
            .as_str()
            .is_some_and(|a| a.contains("ATOMIC_EXECUTOR_ID")),
        "{error}"
    );

    // The engine's own variable is refused, with the setting to use instead.
    let mut env = model.to_vec();
    env.push(("DBOS__VMID", "worker-7"));
    let out = atomic(&dir, &project, &["--json", "serve"], &env, "");
    let error = &stdout_json(&out)["error"];
    assert_eq!(error["kind"], "config.executor_id", "{error}");
    assert!(
        error["action"].as_str().is_some_and(
            |a| a.contains("unset DBOS__VMID") && a.contains("ATOMIC_EXECUTOR_ID=worker-7")
        ),
        "{error}"
    );
    // So is its version variable: atomic stamps runs with its own layout.
    let mut env = model.to_vec();
    env.push(("DBOS__APPVERSION", "v9"));
    let out = atomic(&dir, &project, &["--json", "serve"], &env, "");
    let error = &stdout_json(&out)["error"];
    assert_eq!(error["kind"], "config.engine_version", "{error}");
    assert!(
        error["action"]
            .as_str()
            .is_some_and(|a| a.contains("unset DBOS__APPVERSION")),
        "{error}"
    );
}

/// Two processes on one PostgreSQL store, each with its own identity, each
/// with a run waiting at a gate. One is killed; `atomic serve` restarted
/// under its identity recovers its run and not the live one's.
#[cfg(feature = "postgres-tests")]
#[test]
fn a_restarted_server_recovers_only_its_own_runs_on_postgres() {
    let url = std::env::var("ATOMIC_TEST_DATABASE_URL")
        .expect("postgres-tests needs ATOMIC_TEST_DATABASE_URL");
    on_codex_runtime(async move {
        let model = FakeModel::start(vec![
            Reply::Text(RANK.into()),
            Reply::Text(RANK.into()),
            Reply::Text(SUMMARY.into()),
            Reply::Text(SUMMARY.into()),
        ])
        .await
        .expect("fake model");
        let dir = scratch("postgres");
        let project = project(&dir);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let schema = format!("atomic_cli_{}_{nanos}", std::process::id());
        let model_env = model_env(&model.base_url());
        // Each process has a state directory of its own, as on its own host.
        let a_home = dir.join("a-home").to_string_lossy().into_owned();
        let b_home = dir.join("b-home").to_string_lossy().into_owned();
        let env_a = [
            ("ATOMIC_DATABASE_URL", url.as_str()),
            ("ATOMIC_DATABASE_SCHEMA", schema.as_str()),
            ("ATOMIC_EXECUTOR_ID", "worker-a"),
            ("ATOMIC_HOME", a_home.as_str()),
        ];
        let env_b = [
            ("ATOMIC_DATABASE_URL", url.as_str()),
            ("ATOMIC_DATABASE_SCHEMA", schema.as_str()),
            ("ATOMIC_EXECUTOR_ID", "worker-b"),
            ("ATOMIC_HOME", b_home.as_str()),
        ];
        let mut a = spawn(
            &dir,
            &project,
            &["run", "gated", "--run-id", "run-a", "--data", NOTE],
            &with(&model_env, &env_a),
            &dir.join("a.log"),
        );
        let rt = Durare::postgres(&url, &schema).await.expect("store");
        let gate_a = open_gate(&rt, "run-a").await;
        let mut b = spawn(
            &dir,
            &project,
            &["run", "gated", "--run-id", "run-b", "--data", NOTE],
            &with(&model_env, &env_b),
            &dir.join("b.log"),
        );
        let gate_b = open_gate(&rt, "run-b").await;
        a.kill().expect("SIGKILL a");
        a.wait().expect("reap a");

        let mut serve = spawn(
            &dir,
            &project,
            &["serve"],
            &with(&model_env, &env_a),
            &dir.join("serve.log"),
        );
        wait_until("serve to recover", || {
            std::fs::read_to_string(dir.join("serve.log")).is_ok_and(|l| l.contains("recovered"))
        });
        let log = std::fs::read_to_string(dir.join("serve.log")).expect("log");
        assert!(log.contains("executor worker-a"), "{log}");
        assert!(
            log.contains("recovered 1 unfinished run(s)"),
            "only its own: {log}"
        );

        approve(&rt, &gate_a).await;
        approve(&rt, &gate_b).await;
        for run in ["run-a", "run-b"] {
            let started = Instant::now();
            while rt.status(run).await.expect("status") != Some(RunStatus::Succeeded) {
                assert!(
                    started.elapsed() < Duration::from_secs(120),
                    "{run} never finished"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        let status = tokio::task::spawn_blocking(move || wait_child(&mut b, "b"))
            .await
            .expect("child");
        assert!(status.success(), "b finished its own run");
        assert_eq!(model.calls(), 4, "each run ranked and summarized once");
        terminate(&serve);
        let status = tokio::task::spawn_blocking(move || wait_child(&mut serve, "serve"))
            .await
            .expect("child");
        assert!(status.success());
    });
}

/// The environment of executor `name` on schema `schema`, with a state
/// directory of its own, as on a host of its own.
#[cfg(feature = "postgres-tests")]
fn executor_env(dir: &Path, url: &str, schema: &str, name: &str) -> Vec<(&'static str, String)> {
    vec![
        ("ATOMIC_DATABASE_URL", url.to_owned()),
        ("ATOMIC_DATABASE_SCHEMA", schema.to_owned()),
        ("ATOMIC_EXECUTOR_ID", name.to_owned()),
        (
            "ATOMIC_HOME",
            dir.join(format!("{name}-home"))
                .to_string_lossy()
                .into_owned(),
        ),
    ]
}

/// On PostgreSQL the execution lock covers one host only, so resuming a
/// run another executor owns is refused unless the operator says that
/// executor has stopped.
#[cfg(feature = "postgres-tests")]
#[test]
fn resuming_a_run_another_executor_owns_needs_its_owner_stopped() {
    let url = std::env::var("ATOMIC_TEST_DATABASE_URL")
        .expect("postgres-tests needs ATOMIC_TEST_DATABASE_URL");
    on_codex_runtime(async move {
        let model = FakeModel::start(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())])
            .await
            .expect("fake model");
        let dir = scratch("owned");
        let project = project(&dir);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let schema = format!("atomic_owned_{}_{nanos}", std::process::id());
        let mut a_env = model_env(&model.base_url()).to_vec();
        a_env.extend(executor_env(&dir, &url, &schema, "worker-a"));
        let mut b_env = model_env(&model.base_url()).to_vec();
        b_env.extend(executor_env(&dir, &url, &schema, "worker-b"));
        let mut a = spawn(
            &dir,
            &project,
            &["run", "gated", "--run-id", "owned-1", "--data", NOTE],
            &with(&a_env, &[]),
            &dir.join("a.log"),
        );
        let rt = Durare::postgres(&url, &schema).await.expect("store");
        let opened = open_gate(&rt, "owned-1").await;

        // worker-a is alive and owns the run: worker-b may not take it.
        let resume = |extra: &'static [&'static str]| {
            let (d, p, e) = (dir.clone(), project.clone(), b_env.clone());
            tokio::task::spawn_blocking(move || {
                let mut args = vec!["--json", "runs", "resume", "owned-1"];
                args.extend(extra);
                atomic(&d, &p, &args, &with(&e, &[]), "")
            })
        };
        let out = resume(&[]).await.expect("child");
        assert!(!out.status.success(), "{}", text(&out));
        let error = &stdout_json(&out)["error"];
        assert_eq!(error["kind"], "run.owned", "{error}");
        assert!(
            error["description"]
                .as_str()
                .is_some_and(|d| d.contains("worker-a")),
            "{error}"
        );
        assert!(
            error["action"]
                .as_str()
                .is_some_and(|a| a.contains("--owner-stopped")),
            "{error}"
        );
        assert!(
            a.try_wait().expect("wait").is_none(),
            "the owner kept its run"
        );

        // Once it has stopped, the operator says so, and worker-b continues
        // the run at the gate it opened.
        a.kill().expect("SIGKILL worker-a");
        a.wait().expect("reap");
        let mut b = spawn(
            &dir,
            &project,
            &["runs", "resume", "owned-1", "--owner-stopped"],
            &with(&b_env, &[]),
            &dir.join("b.log"),
        );
        wait_until("worker-b to resume", || {
            std::fs::read_to_string(dir.join("b.log")).is_ok_and(|l| l.contains("resuming"))
        });
        let again = open_gate(&rt, "owned-1").await;
        assert_eq!(again.id, opened.id);
        approve(&rt, &again).await;
        let status = tokio::task::spawn_blocking(move || wait_child(&mut b, "worker-b"))
            .await
            .expect("child");
        assert!(
            status.success(),
            "{}",
            std::fs::read_to_string(dir.join("b.log")).unwrap_or_default()
        );
        assert_eq!(model.calls(), 2, "rank once, summarize once");
        assert_eq!(count_log(&dir).len(), 1, "count once");
    });
}

/// A run resumed by one executor goes on that executor's own queue: an
/// unrelated `atomic run` on another executor never takes it.
#[cfg(feature = "postgres-tests")]
#[test]
fn a_resumed_run_is_taken_only_by_its_resumer_on_postgres() {
    let url = std::env::var("ATOMIC_TEST_DATABASE_URL")
        .expect("postgres-tests needs ATOMIC_TEST_DATABASE_URL");
    on_codex_runtime(async move {
        let model = FakeModel::start(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())])
            .await
            .expect("fake model");
        let dir = scratch("private-queue");
        let project = project(&dir);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let schema = format!("atomic_queue_{}_{nanos}", std::process::id());
        let env_of = |name: &str| {
            let mut env = model_env(&model.base_url()).to_vec();
            env.extend(executor_env(&dir, &url, &schema, name));
            env
        };
        let (a, b, c) = (env_of("worker-a"), env_of("worker-b"), env_of("worker-c"));
        let (d, p) = (dir.clone(), project.clone());
        tokio::task::spawn_blocking(move || killed_after_rank(&d, &p, &a, "queued-1"))
            .await
            .expect("child");
        let call = |env: Vec<(&'static str, String)>, args: &'static [&'static str]| {
            let (d, p) = (dir.clone(), project.clone());
            tokio::task::spawn_blocking(move || atomic(&d, &p, args, &with(&env, &[]), ""))
        };
        // worker-b takes the run over and dies before running it.
        let mut killed = b.clone();
        killed.push(("ATOMIC_FAILPOINT", "resume:queued".into()));
        let out = call(killed, &["runs", "resume", "queued-1", "--owner-stopped"])
            .await
            .expect("child");
        assert!(!out.status.success(), "killed: {}", text(&out));

        // worker-c's unrelated run leaves it alone.
        let out = call(c, &["run", "count-note", "--data", NOTE])
            .await
            .expect("child");
        assert!(out.status.success(), "{}", text(&out));
        let rt = Durare::postgres(&url, &schema).await.expect("store");
        let record = rt
            .run_record("queued-1")
            .await
            .expect("record")
            .expect("run");
        assert_eq!(record.status, RunStatus::Pending);
        assert_eq!(record.owner(), Some("worker-b"), "{record:?}");
        assert_eq!(model.calls(), 1, "worker-c did not run it");

        // worker-b again, which owns it now, finishes it from its queue.
        let out = call(b, &["--json", "runs", "resume", "queued-1"])
            .await
            .expect("child");
        assert!(out.status.success(), "{}", text(&out));
        assert_eq!(
            stdout_json(&out)["output"],
            json!({"line": "P1: Export crashes on accents"})
        );
        assert_eq!(model.calls(), 2, "summarize ran once: {}", text(&out));
        assert_eq!(count_log(&dir).len(), 2);
    });
}

/// `atomic gates` on PostgreSQL: a second terminal enrolls, lists the gate
/// a live run waits at, and approves it, all in the runs' own store.
#[cfg(all(feature = "postgres-tests", target_os = "linux"))]
#[test]
fn gates_are_listed_and_decided_in_the_postgres_store() {
    let url = std::env::var("ATOMIC_TEST_DATABASE_URL")
        .expect("postgres-tests needs ATOMIC_TEST_DATABASE_URL");
    on_codex_runtime(async move {
        let model = FakeModel::start(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())])
            .await
            .expect("fake model");
        let dir = scratch("pg-gates");
        let project = project(&dir);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let schema = format!("atomic_gates_{}_{nanos}", std::process::id());
        let mut env = model_env(&model.base_url()).to_vec();
        env.extend(executor_env(&dir, &url, &schema, "worker-a"));
        env.push(("ATOMIC_CREDENTIAL_STORE", "file".into()));
        let mut run = spawn(
            &dir,
            &project,
            &["run", "gated", "--run-id", "pg-gate-1", "--data", NOTE],
            &with(&env, &[]),
            &dir.join("run.log"),
        );
        let rt = Durare::postgres(&url, &schema).await.expect("store");
        let g = open_gate(&rt, "pg-gate-1").await;
        let gates = |args: &'static [&'static str]| {
            let (d, p, e) = (dir.clone(), project.clone(), env.clone());
            tokio::task::spawn_blocking(move || atomic(&d, &p, args, &with(&e, &[]), ""))
        };
        // Enrolling is a person's command, so it runs on a pseudo-terminal.
        let (d, p, e) = (dir.clone(), project.clone(), env.clone());
        let enrolled = tokio::task::spawn_blocking(move || {
            let mut script = command_as(Path::new("/usr/bin/script"), &d, &p, &[], &with(&e, &[]));
            script
                .args(["-qec", &format!("{ATOMIC} gates enroll"), "/dev/null"])
                .output()
                .expect("script")
        })
        .await
        .expect("child");
        assert!(enrolled.status.success(), "{}", text(&enrolled));
        assert!(
            text(&enrolled).contains("enrolled local:dev"),
            "{}",
            text(&enrolled)
        );

        let listed = gates(&["gates", "list", "--json"]).await.expect("child");
        assert!(listed.status.success(), "{}", text(&listed));
        let listed = stdout_json(&listed);
        assert_eq!(listed[0]["id"], json!(g.id), "{listed}");

        let id = g.id.clone();
        let (d, p, e) = (dir.clone(), project.clone(), env.clone());
        let approved = tokio::task::spawn_blocking(move || {
            atomic(&d, &p, &["gates", "approve", &id], &with(&e, &[]), "")
        })
        .await
        .expect("child");
        assert!(approved.status.success(), "{}", text(&approved));
        let status = tokio::task::spawn_blocking(move || wait_child(&mut run, "the run"))
            .await
            .expect("child");
        assert!(
            status.success(),
            "{}",
            std::fs::read_to_string(dir.join("run.log")).unwrap_or_default()
        );
        assert_eq!(model.calls(), 2);
    });
}

#[test]
fn help_and_the_version_create_nothing_even_from_a_deleted_directory() {
    let dir = scratch("quiet");
    // The shell leaves a directory it then removes, and runs atomic there.
    let script = format!(
        "mkdir gone && cd gone && rmdir ../gone && '{ATOMIC}' --version && '{ATOMIC}' --help"
    );
    let out = Command::new("/bin/sh")
        .args(["-c", &script])
        .current_dir(&dir)
        .env("HOME", dir.join("home"))
        .env("ATOMIC_HOME", dir.join("atomic-home"))
        .env_remove("XDG_DATA_HOME")
        .output()
        .expect("run");
    assert!(out.status.success(), "{}", text(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("atomic "));
    assert!(out.stderr.is_empty(), "no warnings: {}", text(&out));
    assert!(!dir.join("atomic-home").exists(), "nothing was created");

    // The same under another name.
    let renamed = dir.join("bin/my-tool");
    std::fs::create_dir_all(dir.join("bin")).expect("bin");
    std::fs::copy(ATOMIC, &renamed).expect("copy");
    let out = Command::new(&renamed)
        .arg("--version")
        .current_dir(&dir)
        .env("HOME", dir.join("home"))
        .env("ATOMIC_HOME", dir.join("atomic-home"))
        .output()
        .expect("run");
    assert!(out.status.success(), "{}", text(&out));
    assert!(out.stderr.is_empty(), "no warnings: {}", text(&out));
    assert!(!dir.join("atomic-home").exists(), "nothing was created");
}
