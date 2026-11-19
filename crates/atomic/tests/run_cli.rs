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
//! - `atomic serve` started after a kill recovers the run and finishes it;
//! - exec commands run in an `atomic runner` process when asked to;
//! - the executor identity comes from atomic's own setting, is required on
//!   PostgreSQL, and a restarted server recovers only its own runs.
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
    gate::decide(
        rt,
        &Principal::local("dev"),
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
        let rt = Durare::sqlite(&state.join("atomic.db"))
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
        let rt = Durare::sqlite(&state.join("atomic.db"))
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
        let rt = Durare::sqlite(&state.join("atomic.db"))
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
        let db =
            sqlx::SqlitePool::connect(&format!("sqlite://{}", state.join("atomic.db").display()))
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
}

/// An exec atom whose command logs that it ran, then, the first time,
/// SIGKILLs the process that started it: with `--runner process`, its runner.
fn write_runner_killer(project: &Path, dir: &Path, name: &str, effects: &str) {
    std::fs::write(
        project.join("tools/kill-runner.py"),
        "import json, os, signal, sys\n\
         note = json.load(sys.stdin)\n\
         log = os.environ['KILL_LOG']\n\
         open(log, 'a').write('ran\\n')\n\
         if not os.path.exists(log + '.killed'):\n\
         \x20   open(log + '.killed', 'w').close()\n\
         \x20   os.kill(os.getppid(), signal.SIGKILL)\n\
         json.dump({'title': note['text'][:25], 'words': len(note['text'].split())}, sys.stdout)\n",
    )
    .expect("script");
    write_atom(
        project,
        name,
        &format!(
            "description: 'Kill the runner.', kind: 'exec', exec: {{ command: ['python3', 'tools/kill-runner.py'] }}, env: {{ KILL_LOG: '{}' }}{effects}",
            dir.join(format!("{name}.log")).display()
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
    write_runner_killer(&project, &dir, "undeclared", "");
    write_runner_killer(
        &project,
        &dir,
        "idempotent",
        ", effects: { external: 'idempotent' }",
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

/// An idempotent exec atom whose command logs `start <pid>`, then sleeps a
/// minute before it would answer.
fn write_slow(project: &Path, dir: &Path) {
    std::fs::write(
        project.join("tools/slow.py"),
        "import json, os, sys, time\n\
         note = json.load(sys.stdin)\n\
         open(os.environ['SLOW_LOG'], 'a').write(f'start {os.getpid()}\\n')\n\
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

/// The pid of each start of the slow command.
fn slow_starts(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("slow.log"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.strip_prefix("start ").map(str::to_owned))
        .collect()
}

/// Whether serve has said it stopped or failed.
fn serve_ended(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("serve.log"))
        .is_ok_and(|l| l.contains("error") || l.contains("stopping"))
}

/// Whether process `pid` still runs: a zombie waiting to be reaped does not.
fn alive(pid: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        !stat
            .rsplit(')')
            .next()
            .unwrap_or("")
            .trim_start()
            .starts_with('Z')
    })
}

/// Whether another process holds the project's execution lock.
fn lock_held(state: &Path) -> bool {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(state.join("runtime.lock"))
        .expect("the lock file");
    matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

/// Start the slow atom as run `id`, SIGKILL `atomic run` while its command
/// runs, and check the command died with it. Returns the project's state.
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
    // Well before it would end by itself.
    let pid = slow_starts(dir)[0].clone();
    let killed = Instant::now();
    while alive(&pid) {
        assert!(
            killed.elapsed() < Duration::from_secs(10),
            "the command outlived atomic"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    state
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
    let pid = slow_starts(&dir)[1].clone();
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
    wait_until("the stopped command to be gone", || !alive(&pid));
    assert!(!lock_held(&state), "the lock went with the process");
    let ends = std::fs::read_to_string(dir.join("slow.log")).expect("log");
    assert!(!ends.contains("end"), "no command ran to its end: {ends}");
    assert_eq!(slow_starts(&dir).len(), 2, "started twice, never at once");
}

#[test]
fn a_second_signal_stops_serve_at_once() {
    let dir = scratch("serve-twice");
    let project = project(&dir);
    let state = slow_run_killed_mid_command(&dir, &project, "slow-2");
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
    let pid = slow_starts(&dir)[1].clone();
    terminate(&serve);
    wait_until("serve to begin stopping", || {
        std::fs::read_to_string(dir.join("serve.log")).is_ok_and(|l| l.contains("stopping;"))
    });
    assert!(lock_held(&state));
    let again = Instant::now();
    terminate(&serve);
    let status = wait_child(&mut serve, "serve");
    let log = std::fs::read_to_string(dir.join("serve.log")).expect("log");
    assert!(
        again.elapsed() < Duration::from_secs(5),
        "the second signal stops it at once: {log}"
    );
    assert!(!status.success(), "{log}");
    assert!(log.contains("stopping now"), "{log}");
    wait_until("the stopped command to be gone", || !alive(&pid));
    assert!(!lock_held(&state));
    assert_eq!(slow_starts(&dir).len(), 2);
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
#[test]
fn a_restarted_server_recovers_only_its_own_runs_on_postgres() {
    if !atomic_durable::POSTGRES_TESTS {
        return;
    }
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
    assert!(!dir.join("atomic-home").exists(), "nothing was created");
}
