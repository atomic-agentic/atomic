// SPDX-License-Identifier: Apache-2.0

//! The crash suite: every v0.1 failpoint, triggered in turn, against real
//! workflows and the scripted fake model.
//!
//! Each scenario is a workflow atomic runs. One uninterrupted run, in a child
//! process with `ATOMIC_FAILPOINT_LOG` set, records every failpoint the
//! workflow passes. Then, for each of those failpoints, a fresh run is
//! started in a child process that is killed there, and a second process
//! recovers it. Each must end with:
//!
//! - the workflow complete, with the result an uninterrupted run returns (a
//!   run that parked on its budget parked again, once);
//! - no completed step repeated: walking both processes' failpoints in
//!   order, no durable operation runs again once its checkpoint has
//!   committed, none commits twice, and none runs twice within one process;
//!   each atom makes, by the fake model's count, one run's worth of model
//!   calls at most before the kill and, after it, none if its turn had
//!   committed and one run's worth if not; an exec command whose step
//!   committed does not run again, as its own log shows; and the ledger and
//!   the gate store agree;
//! - one accepted durable candidate commit per code-changing step: the
//!   step's only attempt ref, numbered as the kill allows, on the step's
//!   base, changing exactly what an uninterrupted run's commit changes, with
//!   no worktree left.
//!
//! Across the scenarios the failpoints reached cover every site in the
//! inventory (`atomic_durable::failpoints::SITES`), and none is outside it.
//! `ATOMIC_TEST_CRASH_ONLY=<text>` kills only at the failpoints whose names
//! contain `<text>`, for working on one.
//!
//! The scenarios:
//!
//! - `pipeline`: a workflow file run by `run_pipeline`: a lite atom, a codex
//!   atom in a worktree whose first answer does not fit its contract and is
//!   repaired, an exec atom declared idempotent, a gate the local operator
//!   approves, and a lite atom after it;
//! - `change-request`: the change-request workflow written in Rust, a lite
//!   atom and then a codex atom;
//! - `parked`: a workflow file whose budget has room for one model call:
//!   its first step's call settles, and its second finds no room, so it
//!   parks.
//!
//! The codex atom's command runs under Codex's Linux sandbox, as in
//! production, so the suite needs bubblewrap and unprivileged user
//! namespaces and runs with the `sandbox-tests` feature; with the feature on,
//! a sandbox that cannot start fails it.

#![cfg(all(target_os = "linux", feature = "sandbox-tests"))]
// Codex's turn futures nest deeply inside the workflow's; proving them Send
// needs more than the default query depth.
#![recursion_limit = "512"]

mod support;

use atomic_app::pipeline::{PIPELINE, Pipeline, PipelineRun, Services, prepare, register_pipeline};
use atomic_app::workflows::{CHANGE_REQUEST, ChangeRequestRun, register_change_request};
use atomic_core::artifact::LocalStore;
use atomic_core::budget::BudgetSettings;
use atomic_core::codex::{CodexPaths, ModelEndpoint, StateGuard};
use atomic_core::git::Git;
use atomic_core::lite::LiteRuntime;
use atomic_core::runtime::{Artifacts, CodexRuntime, Runtimes};
use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use atomic_core::worktree::Worktrees;
use atomic_core::{AtomConfig, atom, workflow};
use atomic_durable::failpoints::{SITES, site_of};
use atomic_durable::gate::{self, DecisionCommand, Deployment, GateFilter, Principal, Verdict};
use atomic_durable::{DurableRuntime, Durare, RunStatus, parks};
use atomic_schema::registry::Registry;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ENV_SCENARIO: &str = "ATOMIC_CRASH_SCENARIO";
const ENV_ROLE: &str = "ATOMIC_CRASH_ROLE";
const ENV_DIR: &str = "ATOMIC_CRASH_DIR";
const ENV_URL: &str = "ATOMIC_CRASH_URL";
/// Every scenario's run id.
const RUN: &str = "crash-1";
/// How long one child process may take before the suite gives up on it.
const CHILD_LIMIT: Duration = Duration::from_secs(180);

// ---------------------------------------------------------------------------
// The scenarios' projects.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scenario {
    Pipeline,
    ChangeRequest,
    Parked,
}

impl Scenario {
    const ALL: [Self; 3] = [Self::Pipeline, Self::ChangeRequest, Self::Parked];

    fn name(self) -> &'static str {
        match self {
            Self::Pipeline => "pipeline",
            Self::ChangeRequest => "change-request",
            Self::Parked => "parked",
        }
    }

    fn from_name(name: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|s| s.name() == name)
            .unwrap_or_else(|| panic!("no scenario {name}"))
    }

    /// The steps that change code: each must end with one accepted commit.
    fn code_changing(self) -> &'static [&'static str] {
        match self {
            Self::Pipeline => &["fix"],
            Self::ChangeRequest | Self::Parked => &[],
        }
    }

    /// The atom steps whose model calls the suite counts, and the exec step
    /// whose command it counts, if any.
    fn atoms(self) -> &'static [&'static str] {
        match self {
            Self::Pipeline => &["read", "fix", "summarize"],
            Self::ChangeRequest => &["draft", "review"],
            Self::Parked => &["read"],
        }
    }

    /// Which atom step made model call `request`.
    fn caller(self, request: &Value) -> Option<&'static str> {
        let text = request.to_string();
        match self {
            Self::Pipeline | Self::Parked => {
                if text.contains("Write one line") {
                    Some("summarize")
                } else if text.contains("Fix the note") {
                    Some("fix")
                } else if text.contains("Give the note a short title") {
                    Some("read")
                } else {
                    None
                }
            }
            // The draft is a lite atom on Chat Completions; the review a
            // Codex turn on the Responses API.
            Self::ChangeRequest => Some(if request.get("messages").is_some() {
                "draft"
            } else {
                "review"
            }),
        }
    }

    /// The fake model's answer to `request`.
    fn reply(self, request: &Value) -> Reply {
        let text = request.to_string();
        Reply::Text(
            match self.caller(request) {
                Some("read") => r#"{"title": "Export crashes on accents", "words": 7}"#,
                // The fix changes code: it appends to a file in its worktree,
                // so a retry that did not start from the base would show.
                // Its first answer then does not parse, so the atom gets its
                // repair turn on the same thread, which the last call
                // answers.
                Some("fix") if text.contains(UNFIT) => r#"{"done": true}"#,
                Some("fix") if text.contains("function_call_output") => UNFIT,
                Some("fix") => {
                    return Reply::Tool {
                        name: "exec_command".into(),
                        arguments: json!({"cmd": FIX_COMMAND, "yield_time_ms": 10000}),
                    };
                }
                // The summary names the commit it was given, so the result
                // shows which commit the step after the fix saw.
                Some("summarize") => {
                    // The renderer passes input.commit as data, in an
                    // element of its own: `<data-… path="input.commit" …>`.
                    let commit = text
                        .split(r#"path=\"input.commit\""#)
                        .nth(1)
                        .and_then(|rest| rest.split_once('>'))
                        .map(|(_, value)| {
                            value
                                .chars()
                                .take_while(char::is_ascii_hexdigit)
                                .collect::<String>()
                        })
                        .unwrap_or_default();
                    return Reply::Text(format!(r#"{{"line": "P1: fixed in {commit}"}}"#));
                }
                Some("draft") => {
                    r#"{"title": "Charge for every unit in the cart", "priority": 1, "criteria": ["Two units at 125 cents total 250 cents."]}"#
                }
                Some("review") => {
                    r#"{"verdict": "approve", "summary": "The two-unit criterion holds.", "findings": []}"#
                }
                _ => "Synthetic reply.",
            }
            .into(),
        )
    }

    /// The part of the result an uninterrupted run and every recovered run
    /// agree on.
    fn comparable(self, result: &Value) -> Value {
        match self {
            // The commit's ID depends on when it was made; the summary must
            // name the accepted one.
            Self::Pipeline => {
                let commit = result["commit"].as_str().unwrap_or("no commit");
                let line = result["line"].as_str().unwrap_or_default();
                json!({"line": line.replace(commit, "<the accepted commit>")})
            }
            // The published steps carry thread IDs.
            Self::ChangeRequest => json!({"task": result["task"], "verdict": result["verdict"]}),
            Self::Parked => result.clone(),
        }
    }
}

/// The fix atom's first answer, which is not JSON.
const UNFIT: &str = "I fixed the note.";

/// What the fix runs in its worktree: it appends, so it is not idempotent,
/// and only a worktree reset to its base gives the same file every time.
const FIX_COMMAND: &str = "printf 'fixed\\n' >> fixed.txt";

fn scratch(tag: &str) -> PathBuf {
    // Outside the system temporary directory, which the workspace-write
    // sandbox leaves writable.
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-crash-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in [
        "bin",
        "home",
        "cwd",
        "project/atoms",
        "project/workflows",
        "project/tools",
    ] {
        std::fs::create_dir_all(dir.join(sub)).expect("scratch");
    }
    // Codex runs its Linux sandbox as a helper named codex-linux-sandbox:
    // atomic's binary under that name.
    std::os::unix::fs::symlink(
        env!("CARGO_BIN_EXE_atomic"),
        dir.join("bin/codex-linux-sandbox"),
    )
    .expect("the sandbox helper");
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

/// An atom with sidecar schemas, written into `project/atoms`; `config` is
/// added to its configuration.
fn write_atom(project: &Path, name: &str, body: &str, config: &str, input: Value, output: Value) {
    let atoms = project.join("atoms");
    std::fs::write(
        atoms.join(format!("{name}.atom.astro")),
        format!(
            "---\nimport type {{ Props, Input, Output }} from '../.atomic/types/{name}';\n\
             import type {{ AtomConfig }} from '../.atomic/types/authoring';\n\
             const {{ input }}: Props = Astro.props;\n\
             export const config: AtomConfig<Input, Output> = {{ description: '{name}'{config} }};\n\
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

fn plain_git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new(Git::find().expect("git").program())
        .args([
            "-c",
            "user.name=synthetic-author",
            "-c",
            "user.email=synthetic@example.invalid",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgSign=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git");
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout)
        .expect("UTF-8")
        .trim()
        .to_owned()
}

const PIPELINE_FILE: &str = "\
atomic: 1
name: crash-suite
input:
  type: object
  additionalProperties: false
  required: [text, base]
  properties:
    text: {type: string}
    base: {type: string}
output:
  type: object
  additionalProperties: false
  required: [line, commit]
  properties:
    line: {type: string}
    commit: {type: string}
steps:
  read:
    atom: read-note
    with: { text: input.text }
  fix:
    after: [read]
    atom: fix-note
    with: { title: steps.read.output.title, base: input.base }
    worktree: true
  send:
    after: [fix]
    atom: send-note
    with: { text: input.text }
  check:
    after: [send]
    gate:
      title: \"'Ship the fix for ' + steps.read.output.title\"
      show: steps.fix.metadata.commit
      timeout: 24h
  summarize:
    after: [check]
    atom: summarize-note
    with: { title: steps.read.output.title, commit: steps.fix.metadata.commit }
returns: \"{'line': steps.summarize.output.line, 'commit': steps.fix.metadata.commit}\"
";

const PARKED_FILE: &str = "\
atomic: 1
name: crash-parked
input: { $ref: ../atoms/read-note.input.schema.json }
output: { $ref: ../atoms/read-note.output.schema.json }
steps:
  read:
    atom: read-note
    with: { text: input.text }
  again:
    after: [read]
    atom: read-note
    with: { text: input.text }
returns: steps.read.output
budget:
  tokens: 150
";

const NOTE: &str = "Exporting a file named café.txt crashes the app.";

/// A project in a git repository with the pipeline scenario's atoms and
/// workflows. Returns the commit the code-changing step starts from.
fn project(dir: &Path) -> String {
    let project = dir.join("project");
    std::fs::write(project.join("atomic.yaml"), "atomic: 1\nname: crash\n").expect("manifest");
    let note = object(json!({"title": {"type": "string"}, "words": {"type": "integer"}}));
    write_atom(
        &project,
        "read-note",
        "Give the note a short title and count its words.\nNote: {input.text}",
        "",
        object(json!({"text": {"type": "string"}})),
        note.clone(),
    );
    // It changes code, so it is a codex atom that may write its workspace.
    write_atom(
        &project,
        "fix-note",
        "Fix the note.\nTitle: {input.title}\nBase: {input.base}",
        ", kind: 'codex', sandbox: 'workspace-write'",
        object(json!({"title": {"type": "string"}, "base": {"type": "string"}})),
        object(json!({"done": {"type": "boolean"}})),
    );
    // An exec atom whose command reaches outside atomic: each run appends a
    // line to `sent.log`. Repeating it is safe, and it says so.
    std::fs::write(
        project.join("tools/send.py"),
        "import json, os, sys\nnote = json.load(sys.stdin)\nwith open(os.environ['SENT_LOG'], 'a') as log:\n    log.write('sent\\n')\njson.dump({'title': 'Export crashes on accents', 'words': len(note['text'].split())}, sys.stdout)\n",
    )
    .expect("script");
    write_atom(
        &project,
        "send-note",
        "",
        &format!(
            ", kind: 'exec', exec: {{ command: ['python3', 'tools/send.py'] }}, env: {{ SENT_LOG: '{}' }}, effects: {{ external: 'idempotent' }}",
            dir.join("sent.log").display()
        ),
        object(json!({"text": {"type": "string"}})),
        note,
    );
    write_atom(
        &project,
        "summarize-note",
        "Write one line for the triage board.\nTitle: {input.title}\nCommit: {input.commit}",
        "",
        object(json!({"title": {"type": "string"}, "commit": {"type": "string"}})),
        object(json!({"line": {"type": "string"}})),
    );
    std::fs::write(project.join("workflows/crash.workflow.yaml"), PIPELINE_FILE).expect("workflow");
    std::fs::write(project.join("workflows/parked.workflow.yaml"), PARKED_FILE).expect("workflow");
    plain_git(&project, &["init", "-q"]);
    plain_git(&project, &["add", "."]);
    plain_git(&project, &["commit", "-q", "-m", "base"]);
    plain_git(&project, &["rev-parse", "HEAD"])
}

/// Load the project's atoms and one workflow file and prepare it to run.
fn load(project: &Path, file: &str) -> Pipeline {
    let file = project.join("workflows").join(file);
    let mut registry = Registry::new();
    let mut atoms = BTreeMap::new();
    for entry in std::fs::read_dir(project.join("atoms")).expect("atoms") {
        let path = entry.expect("entry").path();
        if path.to_string_lossy().ends_with(".atom.astro") {
            let atom = atom::load(&path, &mut registry).expect("atom loads");
            atoms.insert(atom.name.clone(), atom);
        }
    }
    let loaded = workflow::load(&file, &atoms, &mut registry).expect("workflow loads");
    prepare(&loaded, &atoms, &local()).expect("prepares")
}

/// One developer on one machine.
fn local() -> Deployment {
    Deployment::Local {
        operator: Principal::local("dev"),
    }
}

fn runtimes(dir: &Path, cwd: &Path, base_url: &str) -> Arc<Runtimes> {
    let endpoint = ModelEndpoint {
        base_url: base_url.to_owned(),
        model: "synthetic".into(),
        api_key_env: None,
        sign_in: None,
    };
    Arc::new(Runtimes {
        codex: CodexRuntime {
            paths: CodexPaths::with_helpers(
                PathBuf::from(env!("CARGO_BIN_EXE_atomic")),
                dir.join("bin/codex-linux-sandbox"),
            ),
            codex_home: dir.join("home"),
            // The real sandbox helper enforces the denial of atomic's state.
            guard: StateGuard::deny_state_dir(dir.join("state")).expect("guard"),
            cwd: cwd.to_owned(),
            endpoint: endpoint.clone(),
            defaults: AtomConfig::default(),
            artifacts: Artifacts::none(),
            meter: atomic_core::usage::Metering::none(),
            caches: None,
        },
        lite: LiteRuntime::new(endpoint, AtomConfig::default()),
        backend: Default::default(),
        exec_meter: Default::default(),
        clock: None,
    })
}

async fn engine(scenario: Scenario, dir: &Path, base_url: &str) -> Durare {
    let mut rt = Durare::sqlite(&dir.join("atomic.db"))
        .await
        .expect("engine");
    let project = dir.join("project");
    match scenario {
        Scenario::Pipeline | Scenario::Parked => {
            let repo = Git::find()
                .expect("git")
                .discover(&project)
                .expect("repository");
            register_pipeline(
                &mut rt,
                Services {
                    runtimes: runtimes(dir, &project, base_url),
                    store: LocalStore::open(dir.join("artifacts")).expect("artifact store"),
                    deployment: local(),
                    worktrees: Some(Arc::new(Worktrees::new(repo, dir.join("state/worktrees")))),
                    caches: None,
                    budgets: Arc::new(if scenario == Scenario::Parked {
                        // Each call reserves 150 tokens: the workflow's cap
                        // has room for one at a time.
                        let mut small = BudgetSettings::default();
                        small.call.input_tokens = 100;
                        small.call.output_tokens = 50;
                        small
                    } else {
                        BudgetSettings::default()
                    }),
                    context: None,
                },
            );
        }
        Scenario::ChangeRequest => {
            register_change_request(
                &mut rt,
                runtimes(dir, &dir.join("cwd"), base_url),
                Arc::default(),
            )
            .expect("the default budgets resolve");
        }
    }
    rt.launch().await.expect("launch");
    rt
}

// ---------------------------------------------------------------------------
// One process lifetime.

/// Approve every gate of the run as the local operator, as a person at the
/// terminal would, until the process ends.
async fn approve_gates(rt: Arc<Durare>) {
    let operator = gate::HumanCredential::new(Principal::local("dev"), "the operator's secret");
    gate::enroll(&*rt, &operator)
        .await
        .expect("enroll the operator");
    loop {
        let filter = GateFilter {
            open_only: true,
            run: Some(RUN.to_owned()),
        };
        for g in gate::gates(&*rt, filter).await.unwrap_or_default() {
            let _ = gate::decide(
                &*rt,
                &operator,
                DecisionCommand {
                    command_id: format!("{}-approve", g.id),
                    gate: g.id.clone(),
                    verdict: Verdict::Approve,
                    comment: Some("the crash suite approves".into()),
                    proposal: g.proposal.clone(),
                    answer: None,
                },
            )
            .await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Wait until the run finishes or parks, and say which; a failed run is an
/// error.
async fn outcome(rt: &Durare, scenario: Scenario) -> Result<Value, String> {
    if scenario != Scenario::Parked {
        return rt.result::<Value>(RUN).await.map_err(|e| e.to_string());
    }
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        match rt.status(RUN).await.map_err(|e| e.to_string())? {
            Some(RunStatus::Succeeded) => return Err("the run finished instead of parking".into()),
            Some(RunStatus::Failed(e)) => return Err(e),
            _ => {}
        }
        if let Some(p) = parks(rt, RUN).await.map_err(|e| e.to_string())?.first() {
            return Ok(json!({"parked": p.step}));
        }
        if Instant::now() > until {
            return Err("the run neither finished nor parked".into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Not a test on its own: the suite runs it in a child process, which starts
/// the run (role `start`) or recovers it (role `recover`), approves its
/// gates, and writes its outcome next to the store.
#[test]
#[ignore = "run by the crash suite in a child process"]
fn crash_child() {
    let (Ok(scenario), Ok(role), Ok(dir), Ok(url)) = (
        std::env::var(ENV_SCENARIO),
        std::env::var(ENV_ROLE),
        std::env::var(ENV_DIR),
        std::env::var(ENV_URL),
    ) else {
        return;
    };
    let scenario = Scenario::from_name(&scenario);
    on_codex_runtime(async move {
        let dir = PathBuf::from(dir);
        let rt = Arc::new(engine(scenario, &dir, &url).await);
        if role == "start" {
            let project = dir.join("project");
            match scenario {
                Scenario::Pipeline => {
                    let base = plain_git(&project, &["rev-parse", "HEAD"]);
                    let run = PipelineRun {
                        pipeline: load(&project, "crash.workflow.yaml"),
                        input: json!({"text": NOTE, "base": base}),
                    };
                    rt.start(PIPELINE, RUN, run).await.expect("start");
                }
                Scenario::Parked => {
                    let run = PipelineRun {
                        pipeline: load(&project, "parked.workflow.yaml"),
                        input: json!({"text": NOTE}),
                    };
                    rt.start(PIPELINE, RUN, run).await.expect("start");
                }
                Scenario::ChangeRequest => {
                    let input = ChangeRequestRun {
                        request: "Two units at 1.25 should cost 2.50.".into(),
                        base: "1".repeat(40),
                        commit: "2".repeat(40),
                    };
                    rt.start(CHANGE_REQUEST, RUN, input).await.expect("start");
                }
            }
        } else {
            rt.recover().await.expect("recover");
            // A decider that stopped after recording its decision and before
            // waking the run leaves the wake-up to whoever resumes it.
            gate::redeliver(&*rt).await.expect("redeliver");
        }
        let approver = tokio::spawn(approve_gates(rt.clone()));
        let (file, text) = match outcome(&rt, scenario).await {
            Ok(out) => ("outcome.json", out.to_string()),
            Err(e) => ("error.txt", e),
        };
        std::fs::write(dir.join(file), text).expect("write the outcome");
        approver.abort();
        rt.shutdown(Duration::from_secs(5)).await.expect("shutdown");
    });
}

/// How one child process ended.
struct Ended {
    /// Whether it exited cleanly.
    clean: bool,
    /// How, for messages.
    status: String,
    /// The failpoints it reached, in order.
    reached: Vec<String>,
    /// What it wrote to stderr.
    stderr: String,
}

/// Run one child process for `scenario` in `dir`, killed at `kill` if
/// given, logging its failpoints to `dir/<tag>.log`.
fn child(
    scenario: Scenario,
    dir: &Path,
    url: &str,
    role: &str,
    kill: Option<&str>,
    tag: &str,
) -> Ended {
    let log = dir.join(format!("{tag}.log"));
    let stderr = dir.join(format!("{tag}.stderr"));
    let mut cmd = Command::new(std::env::current_exe().expect("this test binary"));
    cmd.args(["--exact", "crash_child", "--ignored", "--nocapture"])
        .env(ENV_SCENARIO, scenario.name())
        .env(ENV_ROLE, role)
        .env(ENV_DIR, dir)
        .env(ENV_URL, url)
        .env("ATOMIC_FAILPOINT_LOG", &log)
        // No model endpoint from this machine: no Ollama, no key.
        .env("ATOMIC_DETECT_OLLAMA_ADDR", "127.0.0.1:9")
        .env_remove("OPENAI_API_KEY")
        // None of the developer's own configuration.
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&stderr).expect("stderr"));
    match kill {
        Some(name) => cmd.env("ATOMIC_FAILPOINT", name),
        None => cmd.env_remove("ATOMIC_FAILPOINT"),
    };
    let mut process = cmd.spawn().expect("run the child");
    let until = Instant::now() + CHILD_LIMIT;
    let status = loop {
        if let Some(status) = process.try_wait().expect("wait for the child") {
            break status;
        }
        if Instant::now() > until {
            let _ = process.kill();
            break process.wait().expect("wait for the killed child");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Ended {
        clean: status.success(),
        status: status.to_string(),
        reached: lines(&log),
        stderr: std::fs::read_to_string(&stderr).unwrap_or_default(),
    }
}

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The last lines of `text`, for a failure message.
fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(15)..].join("\n")
}

/// The run's outcome, as the last child wrote it.
fn written_outcome(dir: &Path) -> Result<Value, String> {
    match std::fs::read_to_string(dir.join("outcome.json")) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| e.to_string()),
        Err(_) => Err(std::fs::read_to_string(dir.join("error.txt"))
            .unwrap_or_else(|_| "no outcome was written".into())),
    }
}

/// Model calls so far, by the atom step that made them.
fn calls(scenario: Scenario, model: &FakeModel) -> BTreeMap<&'static str, usize> {
    let mut by = BTreeMap::new();
    for request in model.requests() {
        let step = scenario.caller(&request).unwrap_or("unknown");
        *by.entry(step).or_insert(0) += 1;
    }
    by
}

fn sends(dir: &Path) -> usize {
    lines(&dir.join("sent.log")).len()
}

/// The attempt refs left for step `step` of the run, with the commit each
/// names.
fn attempt_refs(dir: &Path, step: &str) -> Vec<(String, String)> {
    plain_git(
        &dir.join("project"),
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            &format!("refs/atomic/{RUN}/steps/{step}/"),
        ],
    )
    .lines()
    .filter_map(|l| l.split_once(' '))
    .map(|(r, c)| (r.to_owned(), c.to_owned()))
    .collect()
}

// ---------------------------------------------------------------------------
// The suite.

/// A fresh project for `scenario` and a fake model answering it.
async fn setup(scenario: Scenario, tag: &str) -> (PathBuf, String, FakeModel) {
    let dir = scratch(&format!("{}-{tag}", scenario.name()));
    let base = project(&dir);
    let model = FakeModel::respond(move |request| scenario.reply(request))
        .await
        .expect("fake model");
    (dir, base, model)
}

/// What an uninterrupted run of `scenario` does.
#[derive(Clone)]
struct Reference {
    /// The failpoints it reaches, in order; each is reached once.
    failpoints: Vec<String>,
    /// Its result, as far as runs agree on it.
    result: Value,
    /// Its model calls, by step.
    calls: BTreeMap<&'static str, usize>,
    /// Its ledger attempts (a Codex turn is one, however many requests it
    /// makes).
    attempts: usize,
    /// Each code-changing step's accepted commit, as the paths it changed
    /// and their contents.
    commits: BTreeMap<&'static str, BTreeMap<String, String>>,
}

/// What the accepted commit of `step` changed on `base`: each path, with
/// its content there.
fn changes(dir: &Path, base: &str, commit: &str) -> BTreeMap<String, String> {
    let project = dir.join("project");
    plain_git(&project, &["diff", "--name-only", base, commit])
        .lines()
        .map(|path| {
            let content = plain_git(&project, &["show", &format!("{commit}:{path}")]);
            (path.to_owned(), content)
        })
        .collect()
}

async fn reference(scenario: Scenario) -> Reference {
    let (dir, base, model) = setup(scenario, "reference").await;
    let url = model.base_url();
    let d = dir.clone();
    let ended =
        tokio::task::spawn_blocking(move || child(scenario, &d, &url, "start", None, "reference"))
            .await
            .expect("child");
    assert!(
        ended.clean,
        "{}: the uninterrupted run ended {}\n{}",
        scenario.name(),
        ended.status,
        tail(&ended.stderr)
    );
    let result = written_outcome(&dir)
        .unwrap_or_else(|e| panic!("{}: the uninterrupted run: {e}", scenario.name()));
    // ATOMIC_FAILPOINT stops at the first failpoint of its name, so a name
    // reached twice would leave its later occurrences untested.
    let mut failpoints: Vec<String> = Vec::new();
    for name in ended.reached {
        assert!(
            !failpoints.contains(&name),
            "{}: the uninterrupted run reaches {name} more than once; name it per occurrence",
            scenario.name()
        );
        failpoints.push(name);
    }
    if scenario == Scenario::Pipeline {
        let commit = result["commit"].as_str().unwrap_or_default();
        assert!(
            commit.len() == 40 && commit.chars().all(|c| c.is_ascii_hexdigit()),
            "the uninterrupted run returned no commit: {result}"
        );
        assert_eq!(
            result["line"],
            json!(format!("P1: fixed in {commit}")),
            "the summary did not name the accepted commit"
        );
    }
    let mut commits = BTreeMap::new();
    for step in scenario.code_changing() {
        let commit = result["commit"].as_str().expect("the accepted commit");
        let changed = changes(&dir, &base, commit);
        // What the fix's command printed, as Codex reported it to the model.
        let printed: Vec<String> = model
            .requests()
            .iter()
            .flat_map(|r| r["input"].as_array().cloned().unwrap_or_default())
            .filter(|item| item["type"] == "function_call_output")
            .map(|item| item["output"].to_string())
            .collect();
        assert_eq!(
            changed.get("fixed.txt").map(String::as_str),
            Some("fixed"),
            "{}: the uninterrupted {step} changed {changed:?}; its command printed {printed:?}",
            scenario.name()
        );
        commits.insert(*step, changed);
    }
    let calls = calls(scenario, &model);
    let rt = Durare::sqlite(&dir.join("atomic.db"))
        .await
        .expect("engine");
    let attempts = rt.ledger().attempts(RUN).await.expect("attempts");
    assert!(
        attempts.iter().all(|a| a.state == "settled"),
        "{}: an uninterrupted run left attempts unsettled: {attempts:?}",
        scenario.name()
    );
    let attempts = attempts.len();
    drop(rt);
    let _ = std::fs::remove_dir_all(&dir);
    Reference {
        failpoints,
        result: scenario.comparable(&result),
        calls,
        attempts,
        commits,
    }
}

/// Kill a fresh run of `scenario` at `failpoint`, recover it in a new
/// process, and check what the suite promises. Returns what failed.
async fn kill_and_recover(
    scenario: Scenario,
    index: usize,
    failpoint: String,
    reference: Reference,
) -> Vec<String> {
    let expected = reference.result.clone();
    let (dir, base, model) = setup(scenario, &index.to_string()).await;
    let url = model.base_url();
    let at = format!("{} at {failpoint}", scenario.name());
    let mut failed: Vec<String> = Vec::new();
    macro_rules! fail {
        ($($arg:tt)*) => { failed.push(format!("{at}: {}", format!($($arg)*))) };
    }

    let (d, u, f) = (dir.clone(), url.clone(), failpoint.clone());
    let first =
        tokio::task::spawn_blocking(move || child(scenario, &d, &u, "start", Some(&f), "first"))
            .await
            .expect("child");
    if first.clean
        || !first
            .stderr
            .contains(&format!("failpoint {failpoint}: killing the process"))
    {
        fail!(
            "the first process was not killed there: it ended {}\n{}",
            first.status,
            tail(&first.stderr)
        );
        return failed;
    }
    let before = calls(scenario, &model);
    let sent_before = sends(&dir);

    let (d, u) = (dir.clone(), url.clone());
    let second =
        tokio::task::spawn_blocking(move || child(scenario, &d, &u, "recover", None, "second"))
            .await
            .expect("child");
    if !second.clean {
        fail!(
            "the recovering process ended {}\n{}",
            second.status,
            tail(&second.stderr)
        );
        return failed;
    }

    // The workflow is complete, with the result an uninterrupted run gives.
    let result = match written_outcome(&dir) {
        Ok(result) => result,
        Err(e) => {
            fail!("the recovered run did not end well: {e}");
            return failed;
        }
    };
    // A budget keeps a call a kill left unsettled charged at its bound, so
    // a parked run killed before its call settled may park sooner than the
    // uninterrupted one: at the step whose re-run finds no room.
    let parked_sooner = scenario == Scenario::Parked && result.get("parked").is_some();
    if scenario.comparable(&result) != expected && !parked_sooner {
        fail!("the recovered run returned {result}, not {expected}");
    }

    // No operation ran again once its checkpoint had committed, in either
    // process: walking both logs in order, an operation's
    // checkpoint:before after its checkpoint:after, or a second
    // checkpoint:after, is a repeat.
    let mut done: BTreeSet<&str> = BTreeSet::new();
    // And within one process, no operation of a name runs twice: these
    // workflows run each operation once. An operation reaches its
    // checkpoint:before failpoint once however often the engine retries its
    // body, so a second one is a second operation, which is a repeat.
    for (process, log) in [("killed", &first.reached), ("recovering", &second.reached)] {
        let mut ran = BTreeSet::new();
        for op in log
            .iter()
            .filter_map(|n| n.strip_prefix("checkpoint:before:"))
        {
            if !ran.insert(op) {
                fail!(
                    "{op} ran twice in the {process} process; its failpoints, in order:\n  {}",
                    log.join("\n  ")
                );
            }
        }
    }
    for name in first.reached.iter().chain(&second.reached) {
        if let Some(op) = name.strip_prefix("checkpoint:before:")
            && done.contains(op)
        {
            fail!("{op} ran again after its checkpoint committed");
        }
        if let Some(op) = name.strip_prefix("checkpoint:after:")
            && !done.insert(op)
        {
            fail!("{op} committed twice");
        }
    }
    let committed: BTreeSet<&str> = first
        .reached
        .iter()
        .filter_map(|n| n.strip_prefix("checkpoint:after:"))
        .collect();

    // Each atom's model calls, by the fake model's count: the killed process
    // made at most one run's worth, and the recovering one none if the turn
    // had committed, and one run's worth otherwise.
    let after = calls(scenario, &model);
    if after.contains_key("unknown") {
        fail!("a model call came from no step: {after:?}");
    }
    for step in scenario.atoms() {
        let once = reference.calls.get(step).copied().unwrap_or(0);
        let killed = before.get(step).copied().unwrap_or(0);
        let recovered = after.get(step).copied().unwrap_or(0) - killed;
        let expected = if committed.contains(format!("{step}.turn").as_str()) {
            0
        } else {
            once
        };
        // A parked run's re-run may find no room for the call a kill left
        // charged, and make none.
        let short = scenario == Scenario::Parked && recovered < expected;
        if killed > once || (recovered != expected && !short) {
            fail!(
                "{step} called the model {killed} times before the kill and {recovered} after; \
                 one run calls it {once} times"
            );
        }
    }

    // The exec command ran once if its step committed, and at most once
    // more otherwise.
    if scenario == Scenario::Pipeline {
        let sent = sends(&dir);
        let expected = if committed.contains("send.turn") {
            1
        } else {
            sent_before + 1
        };
        if sent != expected {
            fail!("the exec command ran {sent} times ({sent_before} before the kill)");
        }
    }

    // One accepted candidate commit per code-changing step, on its base,
    // changing what an uninterrupted run changes; its worktree is gone.
    for step in scenario.code_changing() {
        let refs = attempt_refs(&dir, step);
        let accepted = result["commit"].as_str().unwrap_or_default();
        if refs.len() != 1 || refs[0].1 != accepted {
            fail!("{step} left attempt refs {refs:?}, not one for {accepted}");
            continue;
        }
        // The first attempt's commit, unless the killed process had already
        // recorded one and its turn had not committed.
        let attempt = if first.reached.iter().any(|n| n == "pipeline:after-commit")
            && !committed.contains(format!("{step}.turn").as_str())
        {
            2
        } else {
            1
        };
        let wanted = format!("refs/atomic/{RUN}/steps/{step}/{attempt}");
        if refs[0].0 != wanted {
            fail!("{step}'s accepted commit is {}, not {wanted}", refs[0].0);
        }
        let changed = changes(&dir, &base, accepted);
        if Some(&changed) != reference.commits.get(step) {
            fail!(
                "{step}'s accepted commit changed {changed:?}, not {:?}",
                reference.commits.get(step)
            );
        }
        let parent = plain_git(
            &dir.join("project"),
            &["rev-parse", &format!("{accepted}^")],
        );
        if parent != base {
            fail!("{step}'s commit {accepted} is on {parent}, not its base {base}");
        }
        let left: Vec<_> = std::fs::read_dir(dir.join("state/worktrees"))
            .map(|d| d.filter_map(|e| e.ok().map(|e| e.file_name())).collect())
            .unwrap_or_default();
        if !left.is_empty() {
            fail!("the finished run left worktrees {left:?}");
        }
    }

    // The gate was opened once and approved once. Every metered call the
    // uninterrupted run made was settled, and those of a step that ran again
    // at most twice; only the one the kill caught between admission and
    // settlement may be left unsettled.
    let rt = Durare::sqlite(&dir.join("atomic.db"))
        .await
        .expect("engine");
    if scenario == Scenario::Pipeline {
        let filter = GateFilter {
            open_only: false,
            run: Some(RUN.to_owned()),
        };
        let gates = gate::gates(&rt, filter).await.expect("gates");
        let approved = gates
            .iter()
            .filter(|g| g.resolution.as_ref().is_some_and(|r| r.approved()))
            .count();
        if gates.len() != 1 || approved != 1 {
            fail!(
                "{} gates, {approved} approved, not one of each",
                gates.len()
            );
        }
    }
    let attempts = rt.ledger().attempts(RUN).await.expect("attempts");
    let unsettled = attempts.iter().filter(|a| a.state != "settled").count();
    let settled = attempts.len() - unsettled;
    let once = reference.attempts;
    // A parked run has room for one call. Killed after that call and before
    // it settled, the call stays charged at its bound, which leaves no room
    // for its re-run: the run parks with nothing settled, as a budget must.
    let short = settled < once && scenario != Scenario::Parked;
    if unsettled > 1 || short || attempts.len() > 2 * once {
        fail!(
            "{} ledger attempts, {unsettled} unsettled; an uninterrupted run settles {once}",
            attempts.len()
        );
    }

    // A parked run parked once, with at most its one call made, and once
    // more if a kill left it unsettled.
    if scenario == Scenario::Parked {
        let n = parks(&rt, RUN).await.expect("parks").len();
        if n != 1 {
            fail!("{n} parks recorded, not one");
        }
        if model.calls() > 2 {
            fail!("{} model calls, with room for one", model.calls());
        }
    }
    if failed.is_empty() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    failed
}

#[test]
fn every_failpoint_triggered_in_turn_ends_with_the_workflow_complete() {
    support::require_sandbox();
    on_codex_runtime(async {
        let mut cases = Vec::new();
        let mut reached = BTreeSet::new();
        for scenario in Scenario::ALL {
            let r = reference(scenario).await;
            for name in &r.failpoints {
                reached.insert(name.clone());
                cases.push((scenario, name.clone(), r.clone()));
            }
        }

        // Every site in the inventory is reached, and nothing outside it.
        let outside: Vec<&String> = reached.iter().filter(|n| site_of(n).is_none()).collect();
        assert!(
            outside.is_empty(),
            "failpoints outside the inventory: {outside:?}"
        );
        let missed: Vec<&str> = SITES
            .iter()
            .filter(|s| !reached.iter().any(|n| s.matches(n)))
            .map(|s| s.name)
            .collect();
        assert!(missed.is_empty(), "sites no scenario reaches: {missed:?}");

        // While working on one failpoint, ATOMIC_TEST_CRASH_ONLY=<part of its
        // name> kills only at the failpoints whose names contain it.
        if let Ok(only) = std::env::var("ATOMIC_TEST_CRASH_ONLY") {
            cases.retain(|(_, name, _)| name.contains(&only));
            eprintln!("crash suite: only the failpoints containing {only:?}");
        }

        // Kill a fresh run at each failpoint in turn, a few at a time.
        let width = std::thread::available_parallelism().map_or(2, |n| n.get().clamp(1, 4));
        let started = Instant::now();
        let total = cases.len();
        let mut pending = cases.into_iter().enumerate();
        let mut running = tokio::task::JoinSet::new();
        let mut failures = Vec::new();
        loop {
            while running.len() < width {
                let Some((i, (scenario, name, reference))) = pending.next() else {
                    break;
                };
                running.spawn(kill_and_recover(scenario, i, name, reference));
            }
            match running.join_next().await {
                Some(done) => failures.extend(done.expect("a case")),
                None => break,
            }
        }
        eprintln!(
            "crash suite: {total} failpoints in {:.0?}, {} failures",
            started.elapsed(),
            failures.len()
        );
        assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    });
}
