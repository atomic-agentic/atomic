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

// On Linux, atomic runs exec atoms' commands only under Codex's sandbox,
// and stops a run that needs it where it cannot start: the tests that run
// commands need it (they are marked `needs_sandbox` below), and with the
// `sandbox-tests` feature on, a sandbox that cannot start fails them. The
// others run everywhere.
#![cfg_attr(
    all(target_os = "linux", not(feature = "sandbox-tests")),
    allow(dead_code, unused_imports)
)]
#![recursion_limit = "512"]

use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use atomic_durable::gate::{self, DecisionCommand, GateFilter, Principal, Verdict};
use atomic_durable::{DurableRuntime, Durare, RunStatus};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

#[cfg(all(target_os = "linux", feature = "sandbox-tests"))]
mod support;

const ATOMIC: &str = env!("CARGO_BIN_EXE_atomic");

const NOTE: &str = r#"{"text": "Exporting a file named café.txt crashes the app."}"#;
const RANK: &str = r#"{"priority": 1}"#;
const SUMMARY: &str = r#"{"line": "P1: Export crashes on accents"}"#;

fn scratch(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    // Not under /tmp: atomic refuses scratch directories where sandboxed
    // turns may write, and its state, scratch included, lives under HOME.
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
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
    // It keeps its own tally of its runs in its run's caches, the only
    // place besides its scratch directory it may write, and returns it
    // (see [`TALLY`], [`count_log`]).
    let _ = dir;
    std::fs::write(
        project.join("tools/count.py"),
        format!(
            "import json, os, sys\n{TALLY}\
             note = json.load(sys.stdin)\n\
             json.dump({{'title': note['text'][:25], 'words': len(note['text'].split()), \
             'ran': tally('count-note')}}, sys.stdout)\n"
        ),
    )
    .expect("script");
    write_atom(
        &project,
        "count-note",
        "description: 'Count words.', kind: 'exec', exec: { command: ['python3', 'tools/count.py'] }",
        "",
        object(json!({"text": {"type": "string"}})),
        object(
            json!({"title": {"type": "string"}, "words": {"type": "integer"}, "ran": {"type": "integer"}}),
        ),
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

/// A tool's lines that keep its own tally, in its run's caches: each run of
/// the command appends a line to `<atom>.tally` and returns how many lines
/// the file has, as `ran` in its output. A command run again in the same
/// run, by this execution or another, killed or not, is counted, since the
/// run's caches last until the run ends.
const TALLY: &str = "def tally(atom):\n\
    \x20   at = os.path.join(os.environ['ATOMIC_CACHE_DIR'], atom + '.tally')\n\
    \x20   open(at, 'a').write('ran\\n')\n\
    \x20   return len(open(at).read().splitlines())\n";

/// How many times the counting atom's command ran, in the store `rt` (see
/// [`ran_in`]).
#[cfg(feature = "postgres-tests")]
async fn counted_in(rt: &Durare) -> usize {
    ran_in(rt, None, "count-note").await
}

/// How many times exec atom `atom`'s command ran, by the tally it keeps
/// ([`TALLY`]), summed over the runs in the store `rt` whose workflow has a
/// step of `atom`: for a run that still has its caches (in `state`), the
/// tally there, which counts every run of the command so far; for one that
/// has ended, the largest tally its step's recorded output gives, which
/// counts every run up to the one whose result the run kept, or, where the
/// run kept no result of it, how many of its executions began the step.
async fn ran_in(rt: &Durare, state: Option<&Path>, atom: &str) -> usize {
    let mut ran = 0;
    let runs = rt.runs_after(None, 100_000).await.expect("runs");
    for (key, _) in runs {
        let Some(record) = rt.run_record(&key.id).await.expect("record") else {
            continue;
        };
        if record.workflow != atomic_app::pipeline::PIPELINE {
            continue;
        }
        let counting: Vec<String> = record.input["pipeline"]["steps"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| s["atom"] == atom)
            .filter_map(|s| s["id"].as_str().map(str::to_owned))
            .collect();
        if counting.is_empty() {
            continue;
        }
        if let Some(tally) = state.and_then(|state| live_tally(state, &key.id, atom)) {
            ran += tally;
            continue;
        }
        let steps = rt.steps(&key.id).await.expect("steps");
        let recorded = steps
            .iter()
            .filter(|s| {
                counting
                    .iter()
                    .any(|id| s.name == *id || s.name.starts_with(&format!("{id}.")))
            })
            .filter_map(|s| s.output.as_ref().and_then(tally_in))
            .max();
        ran += match recorded {
            Some(tally) => tally,
            None => began(rt, &key.id, &steps, &counting).await,
        };
    }
    ran
}

/// How many executions of run `run` began one of the steps `counting`, by
/// the store: an execution that ran a command records its attempt, or its
/// turn when the step has none.
async fn began(
    rt: &Durare,
    run: &str,
    steps: &[atomic_durable::StepRecord],
    counting: &[String],
) -> usize {
    let ran_it: Vec<_> = steps
        .iter()
        .filter(|s| {
            counting.iter().any(|id| {
                s.name == format!("{id}.attempt")
                    || (s.name == format!("{id}.turn") && s.error.is_none())
            })
        })
        .collect();
    let executions = atomic_durable::history::executions(rt, run)
        .await
        .unwrap_or_default();
    let mut by: Vec<i64> = ran_it
        .iter()
        .filter_map(|s| atomic_durable::history::recorded_by(s, &executions))
        .collect();
    by.sort_unstable();
    by.dedup();
    if by.is_empty() {
        usize::from(!ran_it.is_empty())
    } else {
        by.len()
    }
}

/// The lines of `atom`'s tally in run `run`'s caches under `state`, if the
/// run still has them.
fn live_tally(state: &Path, run: &str, atom: &str) -> Option<usize> {
    let prefix = format!("{run}-");
    std::fs::read_dir(state.join("cache"))
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        .find_map(|e| std::fs::read_to_string(e.path().join(format!("{atom}.tally"))).ok())
        .map(|text| text.lines().count())
}

/// The largest `ran` in a recorded value, wherever the record nests it.
fn tally_in(value: &Value) -> Option<usize> {
    match value {
        Value::Object(map) => map
            .get("ran")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .into_iter()
            .chain(map.values().filter_map(tally_in))
            .max(),
        Value::Array(items) => items.iter().filter_map(tally_in).max(),
        Value::String(text) => serde_json::from_str::<Value>(text)
            .ok()
            .filter(|v| v.is_object() || v.is_array())
            .as_ref()
            .and_then(tally_in),
        _ => None,
    }
}

/// [`ran`] for the counting atom.
fn count_log(dir: &Path) -> usize {
    ran(dir, "count-note")
}

/// The stores of every atomic home in `dir`, with each project's state
/// directory.
fn stores(dir: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mut stores = Vec::new();
    for home in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        for project in std::fs::read_dir(home.path().join("private"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let db = project.path().join("atomic.db");
            if db.is_file() {
                stores.push((db, home.path().join("projects").join(project.file_name())));
            }
        }
    }
    stores
}

/// What run `id` returned, once it has finished, in any store in `dir`.
fn finished_output(dir: &Path, id: &str) -> Option<Value> {
    let (stores, id) = (stores(dir), id.to_owned());
    // A runtime of its own, so it may be asked from inside the tests' own.
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(async move {
                for (db, _) in stores {
                    let rt = Durare::sqlite(&db).await.expect("store");
                    if rt.status(&id).await.expect("status") == Some(RunStatus::Succeeded) {
                        return rt.result::<Value>(&id).await.ok();
                    }
                }
                None
            })
    })
    .join()
    .expect("result")
}

/// [`ran_in`] summed over the stores of every atomic home in `dir`.
fn ran(dir: &Path, atom: &str) -> usize {
    let (atom, stores) = (atom.to_owned(), stores(dir));
    // A runtime of its own, so it may be asked from inside the tests' own.
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(async move {
                let mut ran = 0;
                for (db, state) in stores {
                    let rt = Durare::sqlite(&db).await.expect("store");
                    ran += ran_in(&rt, Some(&state), &atom).await;
                }
                ran
            })
    })
    .join()
    .expect("count")
}

/// The file named `name` that an exec atom's confined command wrote into
/// its run's caches, `<state>/cache/<run>/<name>`, under any of the atomic
/// homes in `dir`, while the run has them: a command may write only those
/// and its scratch directory, and the caches go when the run ends. A path
/// that does not exist when none has.
fn run_file(dir: &Path, name: &str) -> PathBuf {
    run_files(dir, name)
        .into_iter()
        .next()
        .unwrap_or_else(|| dir.join("no-run-has").join(name))
}

/// Every file named `name` in the caches of the runs that still have them,
/// under any of the atomic homes in `dir` (see [`run_file`]).
fn run_files(dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for home in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        for state in std::fs::read_dir(home.path().join("projects"))
            .into_iter()
            .flatten()
            .flatten()
        {
            for run in std::fs::read_dir(state.path().join("cache"))
                .into_iter()
                .flatten()
                .flatten()
            {
                let file = run.path().join(name);
                if file.exists() {
                    found.push(file);
                }
            }
        }
    }
    found
}

/// That the counting atom's command ran exactly once in each of `runs`
/// runs that still have their caches, by the tally it keeps there: one
/// tally file a run, of one line. A later execution that ran it again
/// would add a line, or, on another executor, a file of its own.
fn ran_once_live(dir: &Path, runs: usize) {
    let tallies = run_files(dir, "count-note.tally");
    assert_eq!(tallies.len(), runs, "live tallies: {tallies:?}");
    for tally in &tallies {
        let lines = std::fs::read_to_string(tally)
            .expect("tally")
            .lines()
            .count();
        assert_eq!(lines, 1, "{} ran {lines} times", tally.display());
    }
}

/// What an exec atom's command names on its command line for a file of its
/// run's caches it writes (see [`run_file`]): `dir/<name>`, unique to the
/// test, which [`pids_with`] finds its processes by; the command writes the
/// file of that name in `$ATOMIC_CACHE_DIR`.
fn command_log(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}

/// The processes, as this process numbers them, whose command line has
/// `marker`: a confined command runs in a process namespace of its own, so
/// the ids it sees for itself are not these.
fn pids_with(marker: &str) -> Vec<String> {
    let mut pids = Vec::new();
    for entry in std::fs::read_dir("/proc").expect("/proc").flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.bytes().all(|b| b.is_ascii_digit()) || name == std::process::id().to_string() {
            continue;
        }
        let cmdline = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
        if String::from_utf8_lossy(&cmdline).contains(marker) && alive(&name) {
            pids.push(name);
        }
    }
    pids
}

/// The session of process `pid`, from field 6 of `/proc/<pid>/stat`.
fn session_of(pid: &str) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let session = stat.rsplit(')').next()?.split_whitespace().nth(3)?;
    Some(session.to_owned())
}

/// The parent of process `pid`, from field 4 of `/proc/<pid>/stat`.
fn parent_of(pid: &str) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let parent = stat.rsplit(')').next()?.split_whitespace().nth(1)?;
    Some(parent.to_owned())
}

/// The `atomic runner` process that started a process whose command line
/// has `marker`, if one did.
fn runner_of(marker: &str) -> Option<String> {
    for pid in pids_with(marker) {
        let mut at = pid;
        while let Some(parent) = parent_of(&at).filter(|p| p != "0" && p != "1") {
            let cmdline = std::fs::read(format!("/proc/{parent}/cmdline")).unwrap_or_default();
            let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
            if args.get(1) == Some(&b"runner".as_slice()) {
                return Some(parent);
            }
            at = parent;
        }
    }
    None
}

/// Acts, from outside the sandbox, for exec commands that ask: a command
/// with `<log>` on its command line, `<log>` being one of `logs` (see
/// [`command_log`]), writes the file of `<log>`'s name with `.ask` into its
/// run's caches, and this finds the `atomic runner` that started it, calls
/// `act` on it and writes `.done` beside the `.ask`. A confined command
/// cannot reach its runner itself. It counts how often each asked
/// ([`acted`](Self::acted)), and the most lines `<log>` had in the run's
/// caches when it did ([`ran`](Self::ran)), the command's own tally of its
/// runs. Stops when dropped.
struct RunnerWatch {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    acted: std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<PathBuf, (usize, usize)>>>,
}

impl RunnerWatch {
    fn new(logs: Vec<PathBuf>, act: impl Fn(&str) + Send + 'static) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopping = stop.clone();
        let acted = std::sync::Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new()));
        let counts = acted.clone();
        let thread = std::thread::spawn(move || {
            while !stopping.load(std::sync::atomic::Ordering::SeqCst) {
                for log in &logs {
                    let (Some(dir), Some(name)) = (log.parent(), log.file_name()) else {
                        continue;
                    };
                    let name = Path::new(name).with_extension("ask");
                    let ask = run_file(dir, &name.display().to_string());
                    if !ask.exists() {
                        continue;
                    }
                    if let Some(runner) = runner_of(&log.display().to_string()) {
                        let _ = std::fs::remove_file(&ask);
                        let tally = std::fs::read_to_string(ask.with_extension("log"))
                            .unwrap_or_default()
                            .lines()
                            .count();
                        act(&runner);
                        let mut counts = counts.lock().expect("counts");
                        let count = counts.entry(log.clone()).or_insert((0, 0));
                        count.0 += 1;
                        count.1 = count.1.max(tally);
                        drop(counts);
                        std::fs::write(ask.with_extension("done"), "").expect("done");
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        Self {
            stop,
            thread: Some(thread),
            acted,
        }
    }

    /// How many times the command with `log` on its command line asked.
    fn acted(&self, log: &Path) -> usize {
        self.counts(log).0
    }

    /// How many times, by its own tally, the command with `log` on its
    /// command line had run when it last asked.
    fn ran(&self, log: &Path) -> usize {
        self.counts(log).1
    }

    fn counts(&self, log: &Path) -> (usize, usize) {
        self.acted
            .lock()
            .expect("counts")
            .get(log)
            .copied()
            .unwrap_or((0, 0))
    }

    /// Kill the asking command's runner.
    fn killing(logs: Vec<PathBuf>) -> Self {
        Self::new(logs, |runner| {
            let _ = Command::new("/bin/sh")
                .args(["-c", &format!("kill -KILL {runner}")])
                .status();
        })
    }
}

impl Drop for RunnerWatch {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
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
        // The person's own configuration is under the throwaway home.
        "XDG_CONFIG_HOME",
        "ATOMIC_DATABASE_URL",
        "ATOMIC_DATABASE_SCHEMA",
        "ATOMIC_EXECUTOR_ID",
        "DBOS__VMID",
        "DBOS__APPVERSION",
        "ATOMIC_FAILPOINT",
        "ATOMIC_FAILPOINT_PANIC",
        "ATOMIC_MODEL_BASE_URL",
        "ATOMIC_MODEL",
        "ATOMIC_MODEL_API_KEY_ENV",
        "ATOMIC_RUNNER",
        "ATOMIC_BACKEND_WAIT",
        "OPENAI_API_KEY",
        "DATABASE_URL",
    ] {
        cmd.env_remove(name);
    }
    // No Ollama, whatever runs on this machine: a test that wants one points
    // this at a fake.
    cmd.env("ATOMIC_DETECT_OLLAMA_ADDR", "127.0.0.1:9");
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

/// The database of the project whose state directory is `state`
/// (`<home>/projects/<id>`): `<home>/private/<id>/atomic.db`.
fn database(state: &Path) -> PathBuf {
    let id = state.file_name().expect("a project id");
    state
        .parent()
        .and_then(Path::parent)
        .expect("the atomic home")
        .join("private")
        .join(id)
        .join("atomic.db")
}

/// Wait until what `atomic` wrote to `log` (stderr) or `out` (stdout) holds
/// `says`, and fail with everything it printed when it never does: a run
/// names what it pinned before anything of it runs, and what it refuses with
/// arrives on stdout as JSON.
fn wait_for_line(log: &Path, out: &Path, says: &str, what: &str) {
    let started = Instant::now();
    loop {
        let shown = format!(
            "{}{}",
            std::fs::read_to_string(log).unwrap_or_default(),
            std::fs::read_to_string(out).unwrap_or_default()
        );
        if shown.contains(says) {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "{what}: never said {says:?}; it printed:\n{shown}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
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

/// [`open_gate`] while reading the run's status: a run that ends on the way
/// says so at once, with the status it ended on, rather than the wait running
/// out for a gate that could never open.
async fn await_gate(rt: &Durare, run: &str) -> gate::Gate {
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
        // No row yet is a run still being admitted: its admission line is
        // printed before the row is written.
        let status = rt.status(run).await.expect("status");
        assert!(
            matches!(status, None | Some(RunStatus::Pending)),
            "{run} ended before it opened a gate: {status:?}"
        );
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
            answer: None,
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
        if started.elapsed() >= Duration::from_secs(120) {
            // Not left running after the test.
            let _ = child.kill();
            let _ = child.wait();
            panic!("{what} did not exit");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_name_a_path_and_the_project_flag_run_the_same_atom_on_the_same_input() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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
    // Each run's command ran once, by the tally it keeps in its run's caches.
    let expected = json!({"title": "Exporting a file named ca", "words": 8, "ran": 1});
    for (cwd, args, stdin) in &runs {
        let out = atomic(&dir, cwd, args, &[], stdin);
        assert!(out.status.success(), "{args:?}: {}", text(&out));
        assert_eq!(stdout_json(&out), expected, "{args:?}");
        // The durable run id comes first, on stderr.
        assert!(run_id(&String::from_utf8_lossy(&out.stderr)).starts_with("run-"));
    }
    assert_eq!(count_log(&dir), runs.len(), "one run each");

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
        count_log(&dir),
        runs.len() + 1,
        "a finished run is not run again"
    );
}

#[test]
fn a_misspelled_atom_fails_check_with_the_fix() {
    let dir = scratch("misspelled");
    let project = project(&dir);
    std::fs::write(
        project.join("workflows/triage.workflow.yaml"),
        TRIAGE.replace("atom: rank-note", "atom: rank-notes"),
    )
    .expect("workflow");
    let out = atomic(&dir, &project, &["--json", "check"], &[], "");
    assert!(!out.status.success(), "{}", text(&out));
    let error = &stdout_json(&out)["error"];
    assert_eq!(error["kind"], "workflow.invalid", "{error}");
    assert!(
        error["description"]
            .as_str()
            .is_some_and(|d| d.contains("no atom named `rank-notes`")),
        "{error}"
    );
    assert_eq!(
        error["action"], "change `atom: rank-notes` to `atom: rank-note`",
        "{error}"
    );
    // And running it by a misspelled name says which was meant.
    let out = atomic(
        &dir,
        &project,
        &["--json", "run", "rank-notes", "--data", NOTE],
        &[],
        "",
    );
    let error = &stdout_json(&out)["error"];
    assert_eq!(error["kind"], "target.unknown", "{error}");
    assert!(
        error["action"]
            .as_str()
            .is_some_and(|a| a.starts_with("did you mean `rank-note`?")),
        "{error}"
    );
}

/// Where `model` listens, as an address: its base URL without `/v1`.
fn address_of(model: &FakeModel) -> String {
    model
        .base_url()
        .trim_start_matches("http://")
        .trim_end_matches("/v1")
        .to_owned()
}

/// The person's own configuration, at `~/.config/atomic/config.yaml`, caps a
/// run made with `atomic run`: the project's larger cap, and its `null` for
/// their dollar cap, leave their caps the ones the run's plan records, with
/// their ceilings beside them.
#[test]
fn your_own_configuration_caps_what_atomic_run_records() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(RANK.into())])
            .await
            .expect("fake model");
        let dir = scratch("person-caps");
        let project = project(&dir);
        // Their home outside the checkout that holds the project, as a
        // configuration inside it could have come with the project.
        let home = std::env::temp_dir().join(format!("atomic-person-caps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let config = home.join(".config/atomic");
        std::fs::create_dir_all(&config).expect("config dir");
        std::fs::write(
            config.join("config.yaml"),
            "budget:\n  atom:\n    tokens: 150000\n    cost_usd: 1\n  workflow:\n    \
             tokens: 300000\n  pricing:\n    synthetic:\n      input_usd_per_mtok: 1\n      \
             output_usd_per_mtok: 1\n",
        )
        .expect("their configuration");
        std::fs::write(
            project.join("atomic.yaml"),
            "atomic: 1\nname: notes\nbudget:\n  workflow:\n    tokens: 900000\n  atom:\n    \
             cost_usd: null\n",
        )
        .expect("manifest");
        let env = model_env(&model.base_url());
        let (d, p, h) = (dir.clone(), project.clone(), home.display().to_string());
        let out = tokio::task::spawn_blocking(move || {
            atomic(
                &d,
                &p,
                &[
                    "run",
                    "rank-note",
                    "--run-id",
                    "capped-1",
                    "--data",
                    r#"{"title": "Export crashes", "words": 2}"#,
                ],
                &with(&env, &[("HOME", h.as_str())]),
                "",
            )
        })
        .await
        .expect("child");
        assert!(out.status.success(), "{}", text(&out));
        let state = state_dir(&dir, &project);
        let rt = Durare::sqlite(&database(&state)).await.expect("store");
        // The plan is what the run's first step, `pin`, recorded.
        let steps = rt.steps("capped-1").await.expect("steps");
        let pin = steps
            .iter()
            .find(|s| s.name == "pin")
            .and_then(|s| s.output.clone())
            .expect("the run pinned its plan");
        let plan = &pin["budgets"];
        assert_eq!(plan["run"]["tokens"], 300_000, "{plan}");
        assert_eq!(plan["atoms"]["rank-note"]["tokens"], 150_000, "{plan}");
        assert_eq!(plan["atoms"]["rank-note"]["micro_usd"], 1_000_000, "{plan}");
        assert_eq!(plan["ceilings"]["workflow"]["tokens"], 300_000, "{plan}");
        assert_eq!(plan["ceilings"]["atom"]["cost_usd"], 1.0, "{plan}");
        let _ = std::fs::remove_dir_all(&home);
    });
}

/// Under the person's own dollar cap, a run that calls a model only the
/// project prices says so on stderr at its start.
#[test]
fn a_run_says_which_models_only_the_project_prices_under_your_dollar_cap() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(RANK.into())])
            .await
            .expect("fake model");
        let dir = scratch("project-priced");
        let project = project(&dir);
        let home =
            std::env::temp_dir().join(format!("atomic-project-priced-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let config = home.join(".config/atomic");
        std::fs::create_dir_all(&config).expect("config dir");
        std::fs::write(
            config.join("config.yaml"),
            "budget:\n  atom:\n    cost_usd: 1\n",
        )
        .expect("their configuration");
        std::fs::write(
            project.join("atomic.yaml"),
            "atomic: 1\nname: notes\nbudget:\n  pricing:\n    synthetic:\n      \
             input_usd_per_mtok: 0.000001\n      output_usd_per_mtok: 0.000001\n",
        )
        .expect("manifest");
        let env = model_env(&model.base_url());
        let (d, p, h) = (dir.clone(), project.clone(), home.display().to_string());
        let out = tokio::task::spawn_blocking(move || {
            atomic(
                &d,
                &p,
                &[
                    "run",
                    "rank-note",
                    "--data",
                    r#"{"title": "Export crashes", "words": 2}"#,
                ],
                &with(&env, &[("HOME", h.as_str())]),
                "",
            )
        })
        .await
        .expect("child");
        assert!(out.status.success(), "{}", text(&out));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(
                "under your own dollar cap, the run calls synthetic at prices only a layer \
                 above your own configuration sets"
            ),
            "{stderr}"
        );
        let _ = std::fs::remove_dir_all(&home);
    });
}

#[test]
fn with_only_a_local_ollama_a_model_atom_runs_and_explain_says_why() {
    on_codex_runtime(async {
        // The fake model stands in for Ollama: it lists one model, synthetic.
        let model = FakeModel::start(vec![Reply::Text(RANK.into())])
            .await
            .expect("fake model");
        let dir = scratch("ollama");
        let project = project(&dir);
        let addr = address_of(&model);
        let (d, p, a) = (dir.clone(), project.clone(), addr.clone());
        let out = tokio::task::spawn_blocking(move || {
            atomic(
                &d,
                &p,
                &[
                    "run",
                    "rank-note",
                    "--data",
                    r#"{"title": "Export crashes", "words": 2}"#,
                ],
                &[("ATOMIC_DETECT_OLLAMA_ADDR", a.as_str())],
                "",
            )
        })
        .await
        .expect("child");
        assert!(out.status.success(), "{}", text(&out));
        assert_eq!(stdout_json(&out), json!({"priority": 1}));
        assert_eq!(
            model.calls(),
            1,
            "the model atom called the detected Ollama"
        );
        // Said once, so a detected endpoint is never used silently.
        let stderr = String::from_utf8_lossy(&out.stderr);
        let said: Vec<&str> = stderr
            .lines()
            .filter(|l| l.starts_with("atomic: model "))
            .collect();
        assert_eq!(
            said,
            [format!(
                "atomic: model synthetic at http://{addr}/v1 (detected: Ollama answered at \
                 {addr}); `atomic config --explain` says why"
            )],
            "{stderr}"
        );

        let (d, p, a) = (dir.clone(), project.clone(), addr.clone());
        let out = tokio::task::spawn_blocking(move || {
            atomic(
                &d,
                &p,
                &["--json", "config", "--explain"],
                &[("ATOMIC_DETECT_OLLAMA_ADDR", a.as_str())],
                "",
            )
        })
        .await
        .expect("child");
        assert!(out.status.success(), "{}", text(&out));
        let doc = stdout_json(&out);
        let source = |setting: &str| -> String {
            doc["settings"]
                .as_array()
                .and_then(|all| all.iter().find(|s| s["setting"] == setting))
                .and_then(|s| s["source"].as_str())
                .unwrap_or_else(|| panic!("no {setting}: {doc}"))
                .to_owned()
        };
        assert_eq!(
            source("model.base_url"),
            format!("detected: Ollama answered at {addr}")
        );
        assert!(
            source("model.name").contains("first model it lists"),
            "{doc}"
        );
        assert_eq!(source("runner"), "default");
        // Every setting says where it came from.
        assert!(
            doc["settings"].as_array().is_some_and(|all| all.len() > 10
                && all
                    .iter()
                    .all(|s| s["source"].as_str().is_some_and(|s| !s.is_empty()))),
            "{doc}"
        );
    });
}

#[test]
fn with_only_an_api_key_explain_names_openai_and_never_the_key() {
    let dir = scratch("api-key");
    let project = project(&dir);
    let out = atomic(
        &dir,
        &project,
        &["config", "--explain"],
        &[("OPENAI_API_KEY", "sk-test-not-for-printing")],
        "",
    );
    assert!(out.status.success(), "{}", text(&out));
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(!said.contains("sk-test-not-for-printing"), "{said}");
    let line = |setting: &str| -> String {
        said.lines()
            .find(|l| l.split_whitespace().next() == Some(setting))
            .unwrap_or_else(|| panic!("no {setting} in {said}"))
            .to_owned()
    };
    assert!(
        line("model.base_url").contains("https://api.openai.com/v1")
            && line("model.base_url").contains("detected: OPENAI_API_KEY is set"),
        "{said}"
    );
    assert!(
        line("model.api_key_env").contains("OPENAI_API_KEY (set"),
        "{said}"
    );
    assert!(line("database.url").contains("default"), "{said}");
    // An explicit setting wins, and says so.
    let out = atomic(
        &dir,
        &project,
        &["config", "--explain", "--model", "my-model"],
        &[
            ("OPENAI_API_KEY", "sk-test-not-for-printing"),
            ("ATOMIC_RUNNER", "process"),
        ],
        "",
    );
    let said = String::from_utf8_lossy(&out.stdout);
    let line = |setting: &str| -> String {
        said.lines()
            .find(|l| l.split_whitespace().next() == Some(setting))
            .unwrap_or_else(|| panic!("no {setting} in {said}"))
            .to_owned()
    };
    assert!(
        line("model.name").contains("my-model") && line("model.name").contains("flag --model"),
        "{said}"
    );
    assert!(
        line("runner").contains("environment variable ATOMIC_RUNNER"),
        "{said}"
    );
}

#[test]
fn no_secret_reaches_stderr_or_json_whatever_atomic_is_given() {
    let dir = scratch("secrets");
    let project = project(&dir);
    let secrets = [
        ("OPENAI_API_KEY", "sk-leak-openai"),
        (
            "ATOMIC_DATABASE_URL",
            "postgres://atomic:leak-dbpass@127.0.0.1:9/atomic?sslpassword=leak-query",
        ),
        (
            "ATOMIC_MODEL_BASE_URL",
            "https://me:leak-token@127.0.0.1:9/v1",
        ),
        ("ATOMIC_MODEL", "m"),
        // A key put where its variable's name belongs.
        ("ATOMIC_MODEL_API_KEY_ENV", "sk-leak-in-the-name"),
        ("ATOMIC_EXECUTOR_ID", "leak-free-id"),
    ];
    let bad_budget = [("ATOMIC_BUDGET_WORKFLOW_TOKENS", "leak-not-a-number")];
    /// A command and the variables it runs with.
    type Case<'a> = (&'a [&'a str], Vec<(&'a str, &'a str)>);
    let runs: [Case; 5] = [
        (&["config", "--explain"], secrets.to_vec()),
        (&["--json", "config", "--explain"], secrets.to_vec()),
        // A setting atomic refuses, from the variables and as JSON.
        (
            &["--json", "run", "count-only", "--data", NOTE],
            [&secrets[..], &bad_budget[..]].concat(),
        ),
        (
            &["run", "count-only", "--data", NOTE],
            [&secrets[..], &bad_budget[..]].concat(),
        ),
        // A store that cannot be reached.
        (
            &["--json", "run", "count-only", "--data", NOTE],
            secrets.to_vec(),
        ),
    ];
    for (args, env) in runs {
        let out = atomic(&dir, &project, args, &env, "");
        let all = text(&out).replace("leak-free-id", "");
        assert!(!all.contains("leak"), "{args:?}: {all}");
    }
}

#[test]
fn the_projects_files_cannot_route_a_key_and_the_persons_own_configuration_can() {
    let dir = scratch("person-config");
    let project = project(&dir);
    let routing = "model:\n  base_url: https://example.com/v1\n  name: m\n  api_key_env: MY_KEY\n";
    // In the project, whether git tracks it or not.
    std::fs::write(project.join("atomic.local.yaml"), routing).expect("local");
    for tracked in [false, true] {
        if tracked {
            git_in(&project, &["init", "-q"]);
            git_in(&project, &["add", "atomic.local.yaml"]);
        }
        let out = atomic(
            &dir,
            &project,
            &["--json", "run", "count-only", "--data", NOTE],
            &[],
            "",
        );
        assert!(!out.status.success(), "{}", text(&out));
        let error = &stdout_json(&out)["error"];
        assert_eq!(error["kind"], "config.invalid", "{error}");
        let said = error["description"].as_str().unwrap_or_default();
        assert!(said.contains("atomic.local.yaml:4"), "{said}");
        assert!(said.contains(".config/atomic/config.yaml"), "{said}");
    }
    // In the person's own configuration, under their home.
    std::fs::remove_file(project.join("atomic.local.yaml")).expect("remove");
    let config = dir.join("home/.config/atomic");
    std::fs::create_dir_all(&config).expect("config dir");
    std::fs::write(config.join("config.yaml"), routing).expect("config");
    let out = atomic(&dir, &project, &["--json", "config", "--explain"], &[], "");
    assert!(out.status.success(), "{}", text(&out));
    let doc = stdout_json(&out);
    let row = doc["settings"]
        .as_array()
        .and_then(|all| all.iter().find(|s| s["setting"] == "model.api_key_env"))
        .cloned()
        .unwrap_or_default();
    assert!(
        row["source"]
            .as_str()
            .is_some_and(|s| s.contains(".config/atomic/config.yaml")),
        "{doc}"
    );
}

#[test]
fn a_key_for_an_endpoint_the_project_chose_is_withheld_and_a_model_run_refused() {
    let dir = scratch("withheld-key");
    let project = project(&dir);
    std::fs::write(
        project.join("atomic.yaml"),
        "atomic: 1\nname: notes\nmodel:\n  base_url: https://example.com/v1\n  name: m\n",
    )
    .expect("manifest");
    let key = [("ATOMIC_MODEL_API_KEY_ENV", "MY_KEY"), ("MY_KEY", "k")];
    // Commands that call no model are not refused.
    for args in [&["--json", "config"][..], &["--json", "check"][..]] {
        let out = atomic(&dir, &project, args, &key, "");
        assert!(out.status.success(), "{args:?}: {}", text(&out));
    }
    let out = atomic(
        &dir,
        &project,
        &[
            "--json",
            "run",
            "rank-note",
            "--data",
            r#"{"title": "Export crashes", "words": 2}"#,
        ],
        &key,
        "",
    );
    assert!(!out.status.success(), "{}", text(&out));
    let error = &stdout_json(&out)["error"];
    assert_eq!(error["kind"], "config.model", "{error}");
    assert!(
        error["description"]
            .as_str()
            .is_some_and(|d| d.contains("atomic.yaml:4") && d.contains("is not used")),
        "{error}"
    );
}

#[test]
fn config_runs_from_any_directory_above_the_persons_own_configuration() {
    let dir = scratch("config-from-home");
    let home = dir.join("home");
    std::fs::create_dir_all(home.join(".config/atomic")).expect("config dir");
    std::fs::write(home.join(".config/atomic/config.yaml"), "runner: process\n").expect("config");
    // The home directory and the one above it, neither in a project.
    for cwd in [home.clone(), dir.clone()] {
        let out = atomic(&dir, &cwd, &["--json", "config", "--explain"], &[], "");
        assert!(out.status.success(), "{}: {}", cwd.display(), text(&out));
        let doc = stdout_json(&out);
        assert!(
            doc["settings"].as_array().is_some_and(|all| all
                .iter()
                .any(|s| s["setting"] == "runner" && s["value"] == "process")),
            "{doc}"
        );
    }
}

#[test]
fn config_output_is_redacted_like_every_line_atomic_writes() {
    let dir = scratch("config-redacted");
    let project = project(&dir);
    // A key, put where a model's name belongs.
    let key = concat!("sk-proj-", "abcdefghijklmnop1234");
    let env = [("ATOMIC_MODEL", key)];
    for args in [
        &["config", "--explain"][..],
        &["--json", "config", "--explain"][..],
    ] {
        let out = atomic(&dir, &project, args, &env, "");
        assert!(out.status.success(), "{args:?}: {}", text(&out));
        let said = text(&out);
        assert!(!said.contains(key), "{args:?}: {said}");
        assert!(said.contains("[REDACTED]"), "{args:?}: {said}");
    }
}

#[test]
fn check_reads_the_settings_of_every_manifest() {
    let dir = scratch("check-settings");
    let project = project(&dir);
    let member = project.join("member");
    std::fs::create_dir_all(&member).expect("member");
    std::fs::write(
        project.join("atomic.yaml"),
        "atomic: 1\nname: notes\nworkspace:\n  members: [member]\n",
    )
    .expect("manifest");
    let check = || atomic(&dir, &project, &["--json", "check"], &[], "");
    // A misspelled setting in a member's manifest.
    std::fs::write(
        member.join("atomic.yaml"),
        "atomic: 1\nname: member\nmodel:\n  bse_url: http://127.0.0.1:9/v1\n",
    )
    .expect("member manifest");
    let out = check();
    assert!(!out.status.success(), "{}", text(&out));
    let error = &stdout_json(&out)["error"];
    assert_eq!(error["kind"], "config.invalid", "{error}");
    assert!(
        error["description"]
            .as_str()
            .is_some_and(|d| d.contains("member/atomic.yaml:4") && d.contains("bse_url")),
        "{error}"
    );
    // A runner that run would refuse, in the workspace's own manifest.
    std::fs::write(member.join("atomic.yaml"), "atomic: 1\nname: member\n").expect("member");
    std::fs::write(
        project.join("atomic.yaml"),
        "atomic: 1\nname: notes\nrunner: proces\nworkspace:\n  members: [member]\n",
    )
    .expect("manifest");
    let out = check();
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        stdout_json(&out)["error"]["description"]
            .as_str()
            .is_some_and(|d| d.contains("must be local or process")),
        "{}",
        text(&out)
    );
    std::fs::write(
        project.join("atomic.yaml"),
        "atomic: 1\nname: notes\nrunner: process\nworkspace:\n  members: [member]\n",
    )
    .expect("manifest");
    let out = check();
    assert!(out.status.success(), "{}", text(&out));
}

#[test]
fn init_in_an_empty_directory_makes_a_project_that_checks_and_runs() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"summary": "Export crashes on accented file names."}"#.into()),
            Reply::Text(r#"{"summary": "Export crashes on accented file names."}"#.into()),
        ])
        .await
        .expect("fake model");
        let dir = scratch("init");
        let fresh = dir.join("note-tools");
        std::fs::create_dir_all(&fresh).expect("dir");
        let out = atomic(&dir, &fresh, &["init"], &[], "");
        assert!(out.status.success(), "{}", text(&out));
        for file in [
            "atomic.yaml",
            ".gitignore",
            "atoms/summarize-note.atom.astro",
            "atoms/summarize-note.input.schema.yaml",
            "atoms/summarize-note.output.schema.yaml",
            "workflows/summarize.workflow.yaml",
            "evals/summarize-note/export-crash.yaml",
        ] {
            assert!(fresh.join(file).is_file(), "{file} was written");
        }
        let manifest = std::fs::read_to_string(fresh.join("atomic.yaml")).expect("manifest");
        assert!(manifest.contains("name: \"note-tools\""), "{manifest}");
        let out = atomic(&dir, &fresh, &["--json", "check"], &[], "");
        assert!(
            out.status.success(),
            "the new project checks: {}",
            text(&out)
        );
        assert_eq!(stdout_json(&out)["checked"]["atoms"], 1);
        assert_eq!(stdout_json(&out)["checked"]["workflows"], 1);

        // With nothing configured but a local Ollama, its atom and its
        // workflow run.
        let addr = address_of(&model);
        for target in ["summarize-note", "summarize"] {
            let (d, f, a) = (dir.clone(), fresh.clone(), addr.clone());
            let out = tokio::task::spawn_blocking(move || {
                atomic(
                    &d,
                    &f,
                    &[
                        "run",
                        target,
                        "--data",
                        r#"{"text": "Exporting café.txt crashes the app."}"#,
                    ],
                    &[("ATOMIC_DETECT_OLLAMA_ADDR", a.as_str())],
                    "",
                )
            })
            .await
            .expect("child");
            assert!(out.status.success(), "{target}: {}", text(&out));
            assert_eq!(
                stdout_json(&out),
                json!({"summary": "Export crashes on accented file names."})
            );
        }

        // A second init leaves the project as it is.
        let before = std::fs::read_to_string(fresh.join("atomic.yaml")).expect("manifest");
        let out = atomic(&dir, &fresh, &["--json", "init"], &[], "");
        assert!(!out.status.success());
        assert_eq!(stdout_json(&out)["error"]["kind"], "init.exists");
        assert_eq!(
            std::fs::read_to_string(fresh.join("atomic.yaml")).expect("manifest"),
            before
        );
    });
}

#[test]
fn init_in_a_repository_with_code_embeds_the_project_in_dot_atomic() {
    let dir = scratch("init-embedded");
    let repo = dir.join("app");
    std::fs::create_dir_all(repo.join("src")).expect("dir");
    std::fs::write(repo.join("src/main.rs"), "fn main() {}\n").expect("code");
    std::fs::write(repo.join("README.md"), "# app\n").expect("readme");
    let out = atomic(&dir, &repo, &["init"], &[], "");
    assert!(out.status.success(), "{}", text(&out));
    assert!(repo.join(".atomic/atomic.yaml").is_file());
    assert!(
        repo.join(".atomic/atoms/summarize-note.atom.astro")
            .is_file()
    );
    assert!(
        !repo.join("atomic.yaml").exists(),
        "nothing at the repository's top"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("src/main.rs")).expect("code"),
        "fn main() {}\n",
        "the code is untouched"
    );
    let out = atomic(&dir, &repo, &["--json", "check"], &[], "");
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(stdout_json(&out)["checked"]["atoms"], 1);
    // From below the repository's top too.
    let out = atomic(&dir, &repo.join("src"), &["--json", "check"], &[], "");
    assert!(out.status.success(), "{}", text(&out));
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_workflow_runs_by_name_and_by_file() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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
        json!({"title": "Exporting a file named ca", "words": 8, "ran": 1})
    );
    // The run's artifact store took its lock where no atom can open it.
    let state = state_dir(&dir, &project);
    let private = state
        .parent()
        .and_then(Path::parent)
        .expect("the atomic home")
        .join("private")
        .join(state.file_name().expect("the project's id"));
    assert!(private.join("artifacts.lock").is_file());
    assert!(!state.join("artifacts/lock").exists());
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
    assert!(count_log(&dir) == 0);
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_run_killed_after_a_step_resumes_by_its_id_and_reruns_nothing_finished() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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
        assert!(
            run_file(&dir, "count-note.tally").is_file(),
            "the killed run's caches hold count's tally"
        );
        assert_eq!(count_log(&dir), 1, "count ran");

        // Resumed, and killed again before the run ends, so its caches,
        // and the tally count keeps there, are still there to read: a
        // later execution that ran count again would show.
        let (d, p, e, i) = (dir.clone(), project.clone(), env.clone(), id.clone());
        let out = tokio::task::spawn_blocking(move || {
            atomic(
                &d,
                &p,
                &["runs", "resume", &i],
                &with(&e, &[("ATOMIC_FAILPOINT", "pipeline:after:summarize")]),
                "",
            )
        })
        .await
        .expect("child");
        assert!(!out.status.success(), "killed again: {}", text(&out));
        assert_eq!(model.calls(), 2, "only summarize ran again");
        assert!(
            run_file(&dir, "count-note.tally").is_file(),
            "the killed run still has its caches, and the tally in them"
        );
        assert_eq!(count_log(&dir), 1, "count did not run again");

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
        assert_eq!(model.calls(), 2, "nothing ran again");
        assert_eq!(count_log(&dir), 1, "count did not run again");
    });
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_run_stopped_mid_call_charges_the_call_only_the_time_it_ran() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    on_codex_runtime(async {
        // Rank's call never answers before the run is stopped.
        let model = FakeModel::start(vec![Reply::Delayed {
            after: Duration::from_secs(300),
            reply: Box::new(Reply::Text(RANK.into())),
        }])
        .await
        .expect("fake model");
        let dir = scratch("stopped");
        let project = project(&dir);
        let state = state_dir(&dir, &project);
        let env = model_env(&model.base_url());
        let mut run = spawn(
            &dir,
            &project,
            &["run", "triage", "--run-id", "stopped-1", "--data", NOTE],
            &with(&env, &[]),
            &dir.join("stopped.log"),
        );
        wait_until("rank's call reached the model", || model.calls() >= 1);
        terminate(&run);
        let status = wait_child(&mut run, "the stopped run");
        assert!(!status.success(), "stopped short of its result");
        // Its call is settled as interrupted: the tokens stay at the bound,
        // the time is what it ran, not the 10 minutes it reserved.
        let rt = Durare::sqlite(&database(&state)).await.expect("store");
        let attempts = rt.ledger().attempts("stopped-1").await.expect("attempts");
        // The exec step before rank finished, and its command is settled.
        let (work, calls): (Vec<_>, Vec<_>) = attempts.iter().partition(|a| a.model == "(command)");
        assert!(work.iter().all(|a| a.state == "settled"), "{attempts:?}");
        assert_eq!(calls.len(), 1, "{attempts:?}");
        let call = calls[0];
        assert_eq!(call.state, "reserved");
        assert_eq!(call.reserved.millis, 600_000);
        assert!(call.charged.millis < 60_000, "{call:?}");
        assert_eq!(call.charged.tokens, call.reserved.tokens);
        let log = std::fs::read_to_string(dir.join("stopped.log")).unwrap_or_default();
        assert!(log.contains("settled 1 model call"), "{log}");
    });
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_resumed_run_keeps_the_budget_it_already_spent() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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
        let rt = Durare::sqlite(&database(&state)).await.expect("store");
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

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn an_unresolved_effect_stays_parked_across_resumes() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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
    assert_eq!(count_log(&dir), 1);
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
        assert_eq!(count_log(&dir), 1, "the command did not run again");
    }
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_run_killed_at_a_gate_keeps_it_and_its_deadline_and_one_resume_runs_at_a_time() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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
        let rt = Durare::sqlite(&database(&state)).await.expect("store");
        let opened = open_gate(&rt, "gated-1").await;
        assert!(opened.deadline.is_some());
        first.kill().expect("SIGKILL the run");
        first.wait().expect("reap");

        // Killed after its last step, so the run still has its caches when
        // it is looked at below.
        let mut resumed = spawn(
            &dir,
            &project,
            &["runs", "resume", "gated-1"],
            &with(&env, &[("ATOMIC_FAILPOINT", "pipeline:after:summarize")]),
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
            !status.success(),
            "killed at the failpoint: {}",
            std::fs::read_to_string(dir.join("resumed.log")).unwrap_or_default()
        );
        // The resumed execution ran summarize and nothing it had: count's
        // tally, still in the run's caches, says count ran once.
        assert_eq!(model.calls(), 2, "rank once, summarize once");
        ran_once_live(&dir, 1);
        let (d, p, e) = (dir.clone(), project.clone(), env.clone());
        let out = tokio::task::spawn_blocking(move || {
            atomic(&d, &p, &["runs", "resume", "gated-1"], &with(&e, &[]), "")
        })
        .await
        .expect("child");
        assert!(out.status.success(), "{}", text(&out));
        assert_eq!(
            rt.status("gated-1").await.expect("status"),
            Some(RunStatus::Succeeded)
        );
        assert_eq!(model.calls(), 2, "nothing ran again");
        assert_eq!(count_log(&dir), 1);
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
    let rt = Durare::sqlite(&database(state)).await.expect("store");
    open_gate(&rt, id).await;
    run.kill().expect("SIGKILL the run");
    run.wait().expect("reap");
    decide_without_waking(state, id).await;
}

/// Approve run `id`'s gate from a decider that stops after recording its
/// decision and before waking the run.
async fn decide_without_waking(state: &Path, id: &str) {
    let rt = Durare::sqlite(&database(state)).await.expect("store");
    let decider = Command::new(std::env::current_exe().expect("this test binary"))
        .args(["--exact", "decider_child", "--ignored", "--nocapture"])
        .env(DECIDER_STORE, database(state))
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

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_gate_decided_while_its_run_was_down_wakes_it_on_resume() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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
            let rt = Durare::sqlite(&database(&state)).await.expect("store");
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
        let rt = Durare::sqlite(&database(&state)).await.expect("store");
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

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_gate_recovered_by_serve_keeps_its_first_deadline_and_is_approved_from_another_terminal() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())])
            .await
            .expect("fake model");
        let dir = scratch("serve-gate");
        let project = project(&dir);
        let state = state_dir(&dir, &project);
        let env = model_env(&model.base_url());
        // Killed at its gate.
        let mut first = spawn(
            &dir,
            &project,
            &["run", "gated", "--run-id", "served-gate", "--data", NOTE],
            &with(&env, &[]),
            &dir.join("first.log"),
        );
        let rt = Durare::sqlite(&database(&state)).await.expect("store");
        let opened = open_gate(&rt, "served-gate").await;
        assert!(opened.deadline.is_some(), "the gate has a deadline");
        first.kill().expect("SIGKILL the run");
        first.wait().expect("reap");

        // The operator's credential, enrolled and kept in the file store, as
        // `atomic gates enroll` leaves it.
        let secret = "the operator's secret";
        let store = atomic_core::credential::FileSecrets::open(
            &dir.join("atomic-home/credentials"),
            &project,
        )
        .expect("the file store");
        let account = format!(
            "operator:dev@{}",
            state.file_name().expect("a project id").to_string_lossy()
        );
        atomic_core::credential::SecretStore::save(&store, &account, secret).expect("kept");
        gate::enroll(
            &rt,
            &gate::HumanCredential::new(Principal::local("dev"), secret),
        )
        .await
        .expect("enrolled");

        // A local serve recovers it.
        let mut serve = spawn(
            &dir,
            &project,
            &["serve"],
            &with(&env, &[]),
            &dir.join("serve.log"),
        );
        wait_until("serve to start", || {
            std::fs::read_to_string(dir.join("serve.log"))
                .is_ok_and(|l| l.contains("recovered 1 unfinished run(s); serving until stopped"))
        });
        let again = open_gate(&rt, "served-gate").await;
        assert_eq!(
            (again.id.clone(), again.opened_at, again.deadline),
            (opened.id.clone(), opened.opened_at, opened.deadline),
            "serve's recovery changed the gate"
        );

        // Another terminal approves it, with the operator's credential.
        let (d, p, e, id) = (dir.clone(), project.clone(), env.clone(), opened.id.clone());
        let out = tokio::task::spawn_blocking(move || {
            atomic(
                &d,
                &p,
                &["gates", "approve", &id],
                &with(&e, &[("ATOMIC_CREDENTIAL_STORE", "file")]),
                "",
            )
        })
        .await
        .expect("child");
        assert!(out.status.success(), "{}", text(&out));
        let started = Instant::now();
        while rt.status("served-gate").await.expect("status") != Some(RunStatus::Succeeded) {
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "serve did not finish the approved run: {}",
                std::fs::read_to_string(dir.join("serve.log")).unwrap_or_default()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // Decided as it was first opened, and by the operator.
        let decided = gate::find(&rt, &opened.id)
            .await
            .expect("gate")
            .expect("exists");
        assert_eq!(
            (decided.opened_at, decided.deadline),
            (opened.opened_at, opened.deadline)
        );
        let Some(gate::Resolution::Decided(decision)) = &decided.resolution else {
            panic!("{:?}", decided.resolution);
        };
        assert_eq!(decision.verdict, Verdict::Approve);
        assert_eq!(decision.by, Principal::local("dev"));
        assert_eq!(model.calls(), 2, "rank once, summarize once");
        terminate(&serve);
        let status = tokio::task::spawn_blocking(move || wait_child(&mut serve, "serve"))
            .await
            .expect("child");
        assert!(status.success());
    });
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn serve_started_after_a_kill_recovers_the_run_and_finishes_it() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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

        // Killed after its last step, so the run still has its caches: the
        // recovering execution ran summarize and nothing the killed one
        // had, by count's tally there.
        let mut killed = spawn(
            &dir,
            &project,
            &["serve"],
            &with(&env, &[("ATOMIC_FAILPOINT", "pipeline:after:summarize")]),
            &dir.join("serve-killed.log"),
        );
        let status = tokio::task::spawn_blocking(move || wait_child(&mut killed, "serve"))
            .await
            .expect("child");
        assert!(
            !status.success(),
            "killed at the failpoint: {}",
            std::fs::read_to_string(dir.join("serve-killed.log")).unwrap_or_default()
        );
        assert_eq!(model.calls(), 2, "rank once, summarize once");
        ran_once_live(&dir, 1);

        let serve = spawn(
            &dir,
            &project,
            &["serve"],
            &with(&env, &[]),
            &dir.join("serve.log"),
        );
        let rt = Durare::sqlite(&database(&state)).await.expect("store");
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
        assert_eq!(count_log(&dir), 1);
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

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_run_started_by_one_build_resumes_and_recovers_under_another() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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
        // First killed after its last step, so the run still has its caches
        // and count's tally there: the other build ran nothing it had.
        let (d, p, e, o) = (dir.clone(), project.clone(), env.clone(), other.clone());
        let out = tokio::task::spawn_blocking(move || {
            atomic_as(
                &o,
                &d,
                &p,
                &["runs", "resume", "built-1"],
                &with(&e, &[("ATOMIC_FAILPOINT", "pipeline:after:summarize")]),
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
        assert_eq!(model.calls(), 2, "rank once, summarize once");
        ran_once_live(&dir, 1);
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
        // Killed after its last step first, so built-2 still has its caches
        // and count's tally there: the other build's server ran nothing
        // the killed run had.
        let mut killed = spawn_as(
            &other,
            &dir,
            &project,
            &["serve"],
            &with(&env, &[("ATOMIC_FAILPOINT", "pipeline:after:summarize")]),
            &dir.join("serve-killed.log"),
        );
        let status = tokio::task::spawn_blocking(move || wait_child(&mut killed, "serve"))
            .await
            .expect("child");
        assert!(
            !status.success(),
            "killed at the failpoint: {}",
            std::fs::read_to_string(dir.join("serve-killed.log")).unwrap_or_default()
        );
        assert_eq!(model.calls(), 4, "each run's rank and summarize once");
        ran_once_live(&dir, 1);
        let mut serve = spawn_as(
            &other,
            &dir,
            &project,
            &["serve"],
            &with(&env, &[]),
            &dir.join("serve.log"),
        );
        let rt = Durare::sqlite(&database(&state)).await.expect("store");
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
        assert_eq!(count_log(&dir), 2, "each run counted once");
        terminate(&serve);
        let status = tokio::task::spawn_blocking(move || wait_child(&mut serve, "serve"))
            .await
            .expect("child");
        assert!(status.success());
    });
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_run_of_another_durable_layout_is_reported_not_resumed() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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
        let db = sqlx::SqlitePool::connect(&format!("sqlite://{}", database(&state).display()))
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
        // Never resumed, the run keeps its caches, and count's tally there.
        ran_once_live(&dir, 1);
        assert_eq!(count_log(&dir), 1);
    });
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_resumed_run_left_queued_is_taken_by_no_other_command() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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
        let rt = Durare::sqlite(&database(&state)).await.expect("store");
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

        // serve takes it from the queue, and is killed after its last step,
        // so the run still has its caches: by count's tally there, nothing
        // the killed run had ran again. Then serve finishes it.
        let mut killed = spawn(
            &dir,
            &project,
            &["serve"],
            &with(&env, &[("ATOMIC_FAILPOINT", "pipeline:after:summarize")]),
            &dir.join("serve-killed.log"),
        );
        let status = tokio::task::spawn_blocking(move || wait_child(&mut killed, "serve"))
            .await
            .expect("child");
        assert!(
            !status.success(),
            "killed at the failpoint: {}",
            std::fs::read_to_string(dir.join("serve-killed.log")).unwrap_or_default()
        );
        assert_eq!(model.calls(), 4);
        ran_once_live(&dir, 1);
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
            count_log(&dir),
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

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_run_id_that_already_exists_is_refused() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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
        assert_eq!(count_log(&dir), 1, "nothing ran again");

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
        assert_eq!(count_log(&dir), 2);
    });
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_cancelled_run_is_not_resumed_and_a_parked_one_is() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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
        let db_url = format!("sqlite://{}", database(&state).display());
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

/// `atomic run` confines an exec atom's command: through the engine as it
/// is wired, the command cannot read the project's database or the
/// credential store, write a gate decision, or open a socket, and can still
/// read the project.
#[cfg(target_os = "linux")]
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn atomic_run_confines_an_exec_atoms_command() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    let dir = scratch("confined");
    let project = project(&dir);
    let state = state_dir(&dir, &project);
    let db = database(&state);
    let credentials = dir.join("atomic-home/credentials");
    std::fs::create_dir_all(&credentials).expect("credentials");
    let credential = credentials.join("local-dev");
    std::fs::write(&credential, "the operator's secret").expect("credential");
    std::fs::write(
        project.join("tools/probe.py"),
        "import json, os, socket, sqlite3, sys\n\
         json.load(sys.stdin)\n\
         def attempt(f):\n    try:\n        f()\n        return 'ok'\n    except Exception as e:\n        return type(e).__name__ + ': ' + str(e)\n\
         def read(p):\n    return lambda: open(p, 'rb').read(1)\n\
         def gate():\n    c = sqlite3.connect(os.environ['DB'])\n    c.execute(\"INSERT INTO atomic_gate_commands VALUES ('f', 'g', '{}', '{}', 0)\")\n    c.commit()\n\
         def tcp():\n    socket.create_connection(('127.0.0.1', 9), timeout=5)\n\
         json.dump({'db': attempt(read(os.environ['DB'])), 'gate': attempt(gate), \
         'credential': attempt(read(os.environ['CREDENTIAL'])), 'tcp': attempt(tcp), \
         'project': attempt(read('atomic.yaml'))}, sys.stdout)\n",
    )
    .expect("script");
    let field = json!({"type": "string"});
    write_atom(
        &project,
        "probe-private",
        &format!(
            "description: 'Probe.', kind: 'exec', exec: {{ command: ['python3', 'tools/probe.py'] }}, \
             env: {{ DB: '{}', CREDENTIAL: '{}' }}",
            db.display(),
            credential.display()
        ),
        "",
        object(json!({})),
        object(
            json!({"db": field, "gate": field, "credential": field, "tcp": field, "project": field}),
        ),
    );
    let denied = |p: &Path| {
        format!(
            "PermissionError: [Errno 13] Permission denied: '{}'",
            p.display()
        )
    };
    // As the engine wires it, on this process's backend and through a
    // runner process.
    for runner in ["local", "process"] {
        let out = atomic(
            &dir,
            &project,
            &["run", "probe-private", "--runner", runner, "--data", "{}"],
            &[],
            "",
        );
        assert!(out.status.success(), "{runner}: {}", text(&out));
        assert_eq!(
            stdout_json(&out),
            json!({
                "db": denied(&db),
                "gate": "OperationalError: unable to open database file",
                "credential": denied(&credential),
                "tcp": "PermissionError: [Errno 1] Operation not permitted",
                "project": "ok",
            }),
            "{runner}"
        );
    }
}

/// An exec atom's confined command has no controlling terminal, even when
/// atomic has one: it cannot open `/dev/tty`, so it cannot type into
/// atomic's terminal (TIOCSTI) or answer a prompt there. Nor can it reach
/// that terminal by its path in `/dev/pts`, or through another process's
/// descriptors in `/proc`, to draw on the screen: its `/dev` and `/proc` are
/// its own. atomic runs here on a pseudo-terminal of its own, which a
/// command run directly there opens, by `/dev/tty` and by its path.
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[cfg(target_os = "linux")]
#[test]
fn a_confined_command_has_no_controlling_terminal() {
    support::require_sandbox();
    let dir = scratch("tty");
    let project = project(&dir);
    std::fs::write(
        project.join("tools/tty.py"),
        format!(
            "import fcntl, json, os, sys, termios\n\
             json.load(sys.stdin)\n\
             def attempt(f):\n    try:\n        f()\n        return 'ok'\n    except Exception as e:\n        return type(e).__name__ + ': ' + str(e)\n\
             def tty():\n    os.close(os.open('/dev/tty', os.O_RDWR))\n\
             def sti():\n    fd = os.open('/dev/tty', os.O_RDWR)\n    fcntl.ioctl(fd, termios.TIOCSTI, b'x')\n\
             def by_path():\n    os.close(os.open(open('{pty}').read().strip(), os.O_WRONLY))\n\
             def through_proc():\n\
             \x20   for pid in os.listdir('/proc'):\n\
             \x20       if not pid.isdigit(): continue\n\
             \x20       try: fds = os.listdir(f'/proc/{{pid}}/fd')\n\
             \x20       except OSError: continue\n\
             \x20       for fd in fds:\n\
             \x20           try: target = os.readlink(f'/proc/{{pid}}/fd/{{fd}}')\n\
             \x20           except OSError: continue\n\
             \x20           if target.startswith('/dev/pts/'):\n\
             \x20               os.close(os.open(f'/proc/{{pid}}/fd/{{fd}}', os.O_WRONLY))\n\
             \x20               return\n\
             \x20   raise OSError('no terminal among the processes it sees')\n\
             json.dump({{'tty': attempt(tty), 'sti': attempt(sti), 'by_path': attempt(by_path), \
             'through_proc': attempt(through_proc)}}, sys.stdout)\n",
            pty = dir.join("pty-path").display()
        ),
    )
    .expect("script");
    let field = json!({"type": "string"});
    write_atom(
        &project,
        "tty-probe",
        "description: 'Probe the terminal.', kind: 'exec', exec: { command: ['python3', 'tools/tty.py'] }",
        "",
        object(json!({})),
        object(json!({"tty": field, "sti": field, "by_path": field, "through_proc": field})),
    );
    // `pty.spawn` runs its arguments with a new pseudo-terminal as their
    // controlling terminal; `tty` first writes that terminal's path where
    // the probe reads it.
    let record = format!(
        "tty > '{}' && exec \"$0\" \"$@\"",
        dir.join("pty-path").display()
    );
    let run = |program: &str, args: &[&str]| -> String {
        let out = command_as(Path::new("/usr/bin/env"), &dir, &project, &[], &[])
            .args(["python3", "-c", "import pty, sys; pty.spawn(sys.argv[1:])"])
            .args(["sh", "-c", record.as_str(), program])
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("run on a pseudo-terminal");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    // Directly on the terminal, the same checks open it.
    let direct = run(
        "python3",
        &[
            "-c",
            "import os; os.close(os.open('/dev/tty', os.O_RDWR)); \
             os.close(os.open(os.ttyname(0), os.O_WRONLY)); print('opened')",
        ],
    );
    assert!(
        direct.contains("opened"),
        "the test has a terminal: {direct}"
    );
    for runner in ["local", "process"] {
        let out = run(
            ATOMIC,
            &["run", "tty-probe", "--runner", runner, "--data", "{}"],
        );
        let json = out
            .find('{')
            .and_then(|start| out[start..].rfind('}').map(|end| &out[start..=start + end]))
            .unwrap_or_else(|| panic!("{runner}: no result in {out}"));
        let result: Value = serde_json::from_str(json).unwrap_or_else(|e| panic!("{e}: {out}"));
        // Each fails, as OSError or one of its kinds; opening the terminal
        // by its path fails on that path, not on a slip in the probe.
        for probe in ["tty", "sti", "by_path", "through_proc"] {
            assert!(
                result[probe]
                    .as_str()
                    .is_some_and(|r| r != "ok" && r.contains("Error")),
                "{runner}: {probe}: {result}"
            );
        }
        assert!(
            result["by_path"]
                .as_str()
                .is_some_and(|r| r.contains("'/dev/pts/")),
            "{runner}: by_path: {result}"
        );
    }
}

/// An exec atom's command has no network unless its atom declares hosts,
/// and then reaches only those, on ports 80 and 443, through atomic's proxy.
/// No test reaches the outside world: the declared host is under the
/// reserved `.invalid` domain, which never resolves. The proxy refuses a host
/// the atom did not declare (`not_allowed`) and a declared one on another
/// port (`port_not_allowed`) before any lookup. A declared host on port 80
/// or 443 passes both and is refused only by the next check, which resolves
/// it to make sure it is not on a private network and fails closed when it
/// does not resolve (`not_allowed_local`). A proxy named in atomic's own
/// environment changes none of this, and is never used. No connection leaves
/// the command except through the proxy.
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[cfg(target_os = "linux")]
#[test]
fn an_exec_atom_reaches_only_the_hosts_it_declares() {
    support::require_sandbox();
    let dir = scratch("network");
    let project = project(&dir);
    std::fs::write(
        project.join("tools/net.py"),
        "import json, os, socket, sys, urllib.request\n\
         json.load(sys.stdin)\n\
         proxy = os.environ.get('HTTP_PROXY')\n\
         opener = urllib.request.build_opener(urllib.request.ProxyHandler({'http': proxy} if proxy else {}))\n\
         def reason(body):\n\
         \x20   try: return json.loads(body).get('reason', '')\n\
         \x20   except Exception: return body.decode(errors='replace').strip()\n\
         def attempt(f):\n\
         \x20   try:\n\
         \x20       return f()\n\
         \x20   except urllib.error.HTTPError as e:\n\
         \x20       return ('HTTP ' + str(e.code) + ' ' + reason(e.read())).strip()\n\
         \x20   except Exception as e:\n\
         \x20       return type(e).__name__\n\
         def fetch(url):\n\
         \x20   def get():\n\
         \x20       opener.open(url, timeout=20).read()\n\
         \x20       return 'ok'\n\
         \x20   return get\n\
         def tunnel(target):\n\
         \x20   def connect():\n\
         \x20       host, port = proxy.split('//')[1].rstrip('/').rsplit(':', 1)\n\
         \x20       s = socket.create_connection((host, int(port)), timeout=20)\n\
         \x20       s.sendall(('CONNECT ' + target + ' HTTP/1.1\\r\\nHost: ' + target + '\\r\\n\\r\\n').encode())\n\
         \x20       data = b''\n\
         \x20       while b'\\r\\n\\r\\n' not in data:\n\
         \x20           chunk = s.recv(4096)\n\
         \x20           if not chunk: break\n\
         \x20           data += chunk\n\
         \x20       head, _, body = data.partition(b'\\r\\n\\r\\n')\n\
         \x20       length = [int(l.split(b':')[1]) for l in head.split(b'\\r\\n') if l.lower().startswith(b'content-length:')]\n\
         \x20       while length and len(body) < length[0]:\n\
         \x20           chunk = s.recv(4096)\n\
         \x20           if not chunk: break\n\
         \x20           body += chunk\n\
         \x20       code = head.split(b' ')[1].decode()\n\
         \x20       return ('HTTP ' + code + ' ' + reason(body)).strip() if code != '200' else 'tunnel'\n\
         \x20   return connect\n\
         def direct():\n\
         \x20   socket.create_connection(('192.0.2.1', 80), timeout=5).close()\n\
         \x20   return 'ok'\n\
         if proxy:\n\
         \x20   ports = {'tunnel_443': attempt(tunnel('allowed.invalid:443')), 'tunnel_22': attempt(tunnel('allowed.invalid:22'))}\n\
         else:\n\
         \x20   ports = {'tunnel_443': 'no proxy', 'tunnel_22': 'no proxy'}\n\
         json.dump({'proxied': proxy is not None, 'declared': attempt(fetch('http://allowed.invalid/')), \
         'undeclared': attempt(fetch('http://blocked.invalid/')), 'direct': attempt(direct)} | ports, sys.stdout)\n",
    )
    .expect("script");
    let schema = object(json!({
        "proxied": {"type": "boolean"},
        "declared": {"type": "string"},
        "undeclared": {"type": "string"},
        "direct": {"type": "string"},
        "tunnel_443": {"type": "string"},
        "tunnel_22": {"type": "string"},
    }));
    write_atom(
        &project,
        "declares-network",
        "description: 'Reach a declared host.', kind: 'exec', exec: { command: ['python3', 'tools/net.py'] }, \
         network: { allow: ['allowed.invalid'] }",
        "",
        object(json!({})),
        schema.clone(),
    );
    write_atom(
        &project,
        "no-network",
        "description: 'Declare no network.', kind: 'exec', exec: { command: ['python3', 'tools/net.py'] }",
        "",
        object(json!({})),
        schema,
    );
    // A proxy in atomic's own environment, which answers anything it gets:
    // nothing may go through it.
    let upstream = std::net::TcpListener::bind("127.0.0.1:0").expect("upstream");
    let upstream_url = format!("http://{}", upstream.local_addr().expect("address"));
    upstream.set_nonblocking(true).expect("nonblocking");
    let upstream_env = [
        ("HTTP_PROXY", upstream_url.as_str()),
        ("HTTPS_PROXY", upstream_url.as_str()),
        ("ALL_PROXY", upstream_url.as_str()),
        ("http_proxy", upstream_url.as_str()),
        ("https_proxy", upstream_url.as_str()),
        ("all_proxy", upstream_url.as_str()),
    ];
    for (runner, env) in [
        ("local", &[][..]),
        ("process", &[][..]),
        ("local", &upstream_env[..]),
    ] {
        let case = format!(
            "{runner}{}",
            if env.is_empty() {
                ""
            } else {
                " with a proxy in atomic's environment"
            }
        );
        let run = |atom: &str| {
            let out = atomic(
                &dir,
                &project,
                &["run", atom, "--runner", runner, "--data", "{}"],
                env,
                "",
            );
            assert!(out.status.success(), "{case} {atom}: {}", text(&out));
            stdout_json(&out)
        };
        let declared = run("declares-network");
        assert_eq!(
            declared,
            json!({
                "proxied": true,
                "declared": "HTTP 403 not_allowed_local",
                "undeclared": "HTTP 403 not_allowed",
                "direct": declared["direct"],
                // A tunnel's refusal is text: the name passed the list on
                // 443 and failed only its address check, and on 22 it did
                // not pass the list.
                "tunnel_443": "HTTP 403 Sandbox policy blocks local/private network addresses.",
                "tunnel_22": "HTTP 403 Request blocked by network policy.",
            }),
            "{case}"
        );
        assert_ne!(declared["direct"], "ok", "{case}: {declared}");

        let none = run("no-network");
        assert_eq!(none["proxied"], false, "{case}: {none}");
        for probe in ["declared", "undeclared", "direct"] {
            assert!(
                none[probe]
                    .as_str()
                    .is_some_and(|r| !r.starts_with("HTTP") && r != "ok"),
                "{case}: {probe}: {none}"
            );
        }
    }
    // Offline this cannot fail: every request above is refused before the
    // proxy dials anything, through an upstream proxy or not. The control
    // for the upstream proxy is the unit test that reads the proxy's
    // configuration, network::tests::a_proxy_uses_no_upstream_proxy_and_no_socks.
    assert!(
        upstream.accept().is_err(),
        "a request went through the proxy in atomic's environment"
    );
}

/// A `bwrap` in front of the real one, in `dir/bwrap-bin`: while
/// `dir/bwrap-full` exists it fails as bubblewrap does on a full disk, and
/// while `dir/bwrap-refuse` exists as it does where user namespaces are not
/// allowed. Returns the `PATH` to run atomic with.
fn bwrap_in_front(dir: &Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var("PATH").unwrap_or_default();
    let real = path
        .split(':')
        .map(|d| Path::new(d).join("bwrap"))
        .find(|p| p.is_file())
        .expect("bubblewrap on PATH");
    let bin = dir.join("bwrap-bin");
    std::fs::create_dir_all(&bin).expect("bin");
    let script = format!(
        "#!/bin/sh\n\
         if [ \"$1\" != --help ]; then\n\
         \x20 if [ -e '{full}' ]; then\n\
         \x20   echo 'bwrap: Cannot make directory /tmp/x: No space left on device' >&2; exit 1\n\
         \x20 fi\n\
         \x20 if [ -e '{refuse}' ]; then\n\
         \x20   echo 'bwrap: No permissions to create a new namespace, likely because the kernel \
         does not allow non-privileged user namespaces.' >&2; exit 1\n\
         \x20 fi\n\
         fi\n\
         exec '{real}' \"$@\"\n",
        full = dir.join("bwrap-full").display(),
        refuse = dir.join("bwrap-refuse").display(),
        real = real.display(),
    );
    let wrapper = bin.join("bwrap");
    std::fs::write(&wrapper, script).expect("wrapper");
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    format!("{}:{path}", bin.display())
}

/// A project that ran before has its backend's temporary directory, so a
/// full disk shows first in the sandbox's trial before the next run's first
/// command: the run waits, records nothing, and finishes once there is room.
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_sandbox_trial_failing_for_want_of_room_makes_the_run_wait_then_finish() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    let dir = scratch("sandbox-full");
    let project = project(&dir);
    let path = bwrap_in_front(&dir);
    let env = [("PATH", path.as_str())];
    let out = atomic(
        &dir,
        &project,
        &["run", "count-only", "--data", NOTE],
        &env,
        "",
    );
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(count_log(&dir), 1);

    let full = dir.join("bwrap-full");
    std::fs::write(&full, "").expect("the disk fills");
    let log = dir.join("full.log");
    let mut run = spawn(
        &dir,
        &project,
        &["run", "count-only", "--data", NOTE],
        &env,
        &log,
    );
    wait_until("the run to wait", || {
        std::fs::read_to_string(&log)
            .is_ok_and(|l| l.contains("is waiting for the execution backend"))
    });
    let waiting = std::fs::read_to_string(&log).expect("log");
    assert!(waiting.contains("No space left on device"), "{waiting}");
    assert!(!waiting.contains("cannot start here"), "{waiting}");
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        run.try_wait().expect("wait").is_none(),
        "it waits rather than stopping: {waiting}"
    );
    assert_eq!(count_log(&dir), 1, "nothing ran meanwhile");

    std::fs::remove_file(&full).expect("room again");
    let status = wait_child(&mut run, "the waiting run");
    let text = std::fs::read_to_string(&log).expect("log");
    assert!(status.success(), "{text}");
    assert!(text.contains("can open its workspace again"), "{text}");
    assert_eq!(count_log(&dir), 2, "its command ran once");
    let _ = std::fs::remove_dir_all(&dir);
}

/// With `--backend-wait`, a run whose step finds no room on the execution
/// backend for that long stops, unfinished and with nothing recorded, with
/// the exit code for a failure to try again; `atomic runs resume` continues
/// it once there is room, and its command runs once.
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_run_that_waits_too_long_for_room_stops_unfinished_and_resumes() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    let dir = scratch("backend-wait");
    let project = project(&dir);
    let path = bwrap_in_front(&dir);
    let env = [("PATH", path.as_str())];
    let out = atomic(
        &dir,
        &project,
        &["run", "count-only", "--data", NOTE],
        &env,
        "",
    );
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(count_log(&dir), 1);

    let full = dir.join("bwrap-full");
    std::fs::write(&full, "").expect("the disk fills");
    let log = dir.join("wait.log");
    let started = Instant::now();
    let mut run = spawn(
        &dir,
        &project,
        &[
            "run",
            "count-only",
            "--run-id",
            "room-1",
            "--backend-wait",
            "2",
            "--data",
            NOTE,
        ],
        &env,
        &log,
    );
    let status = wait_child(&mut run, "the waiting run");
    let waited = started.elapsed();
    let text_log = std::fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(
        status.code(),
        Some(atomic_app::workflows::NO_ROOM_EXIT),
        "{text_log}"
    );
    assert!(
        waited >= Duration::from_secs(2),
        "it waited its limit: {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(30),
        "and no longer: {waited:?}"
    );
    assert!(
        text_log.contains("the most this process waits"),
        "{text_log}"
    );
    assert!(text_log.contains("atomic runs resume"), "{text_log}");
    assert_eq!(count_log(&dir), 1, "nothing ran");

    std::fs::remove_file(&full).expect("room again");
    let out = atomic(
        &dir,
        &project,
        &["runs", "resume", "room-1"],
        &with_unused(&env),
        "",
    );
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(count_log(&dir), 2, "its command ran once");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A signal stops a run waiting for room on the execution backend at once,
/// not after the time a running step has to reach its checkpoint: the
/// waiting step gives up its wait, records nothing, and `atomic runs
/// resume` continues it.
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_signal_ends_a_wait_for_room_at_once() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    let dir = scratch("wait-stop");
    let project = project(&dir);
    let path = bwrap_in_front(&dir);
    let env = [("PATH", path.as_str())];
    let out = atomic(
        &dir,
        &project,
        &["run", "count-only", "--data", NOTE],
        &env,
        "",
    );
    assert!(out.status.success(), "{}", text(&out));
    let full = dir.join("bwrap-full");
    std::fs::write(&full, "").expect("the disk fills");
    let log = dir.join("wait.log");
    let mut run = spawn(
        &dir,
        &project,
        &["run", "count-only", "--run-id", "room-2", "--data", NOTE],
        &env,
        &log,
    );
    wait_until("the run to wait", || {
        std::fs::read_to_string(&log)
            .is_ok_and(|l| l.contains("is waiting for the execution backend"))
    });
    // Into its pauses, which grow towards half a minute.
    std::thread::sleep(Duration::from_secs(4));
    let stopping = Instant::now();
    signal(&run, "TERM");
    let status = wait_child(&mut run, "the stopped run");
    let took = stopping.elapsed();
    let text_log = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        took < Duration::from_secs(3),
        "it stopped in {took:?}: {text_log}"
    );
    assert!(!status.success(), "{text_log}");
    assert_eq!(count_log(&dir), 1, "nothing ran");

    std::fs::remove_file(&full).expect("room again");
    let out = atomic(
        &dir,
        &project,
        &["runs", "resume", "room-2"],
        &with_unused(&env),
        "",
    );
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(count_log(&dir), 2, "its command ran once");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Under `atomic serve`, a step that found no room within the wait is set
/// aside, not given up: serve says so, keeps trying at most half a minute
/// apart and says so each time, stops at once when asked meanwhile, and runs
/// the step once there is room. The wait comes from `ATOMIC_BACKEND_WAIT`.
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn serve_keeps_trying_a_step_that_found_no_room_in_time() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    let dir = scratch("serve-room");
    let project = project(&dir);
    write_two_steps(&project, &dir);
    let path = bwrap_in_front(&dir);
    // A run stopped after its first command, with a second to run.
    let out = atomic(
        &dir,
        &project,
        &["run", "two-steps", "--run-id", "two-1", "--data", NOTE],
        &[
            ("PATH", path.as_str()),
            ("ATOMIC_FAILPOINT", "pipeline:after:first"),
        ],
        "",
    );
    assert!(!out.status.success(), "{}", text(&out));
    let full = dir.join("bwrap-full");
    std::fs::write(&full, "").expect("the disk fills");
    let env = with_unused(&[("PATH", path.as_str()), ("ATOMIC_BACKEND_WAIT", "1")]);
    let log = dir.join("serve.log");
    let mut serve = spawn(&dir, &project, &["serve"], &env, &log);
    wait_until("serve to set the step aside", || {
        std::fs::read_to_string(&log).is_ok_and(|l| l.contains("is set aside"))
    });
    // A stop while a step is set aside does not wait for it.
    let stopping = Instant::now();
    terminate(&serve);
    let status = wait_child(&mut serve, "serve");
    let text_log = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(status.success(), "{text_log}");
    assert!(
        stopping.elapsed() < Duration::from_secs(5),
        "a set-aside step is not one a stop waits for: {text_log}"
    );
    assert_eq!(
        steps_log(&dir),
        ["first start", "first end"],
        "second has not run"
    );

    // Started again, serve sets the step aside again, keeps trying, and
    // runs it, once, when there is room: the run's output carries the log
    // its caches kept, which go with it.
    let log = dir.join("serve-again.log");
    let mut serve = spawn(&dir, &project, &["serve"], &env, &log);
    wait_until("serve to try again", || {
        std::fs::read_to_string(&log)
            .is_ok_and(|l| l.contains("is set aside") && l.contains("still no room"))
    });
    assert!(serve.try_wait().expect("wait").is_none(), "serve goes on");
    std::fs::remove_file(&full).expect("room again");
    wait_until("serve to finish the run", || {
        finished_output(&dir, "two-1").is_some()
    });
    terminate(&serve);
    let status = wait_child(&mut serve, "serve");
    let text_log = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(status.success(), "{text_log}");
    assert_eq!(
        finished_output(&dir, "two-1").expect("finished")["steps"],
        json!(["first start", "first end", "second"])
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--backend-wait` is a whole number of seconds, at least 1, and so is
/// the `backend_wait` setting's variable, `ATOMIC_BACKEND_WAIT`, whose empty
/// value means none was given. Each is checked before anything runs; with no sandbox here the empty case may
/// stop for that, never for the wait.
#[test]
fn the_backend_wait_is_at_least_a_second() {
    let dir = scratch("backend-wait-zero");
    let project = project(&dir);
    let out = atomic(
        &dir,
        &project,
        &["run", "no-such-atom", "--backend-wait", "0", "--data", "{}"],
        &[],
        "",
    );
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("--backend-wait"), "{}", text(&out));
    let out = atomic(
        &dir,
        &project,
        &["--json", "run", "count-only", "--data", NOTE],
        &[("ATOMIC_BACKEND_WAIT", "")],
        "",
    );
    assert_ne!(
        out.status.code(),
        Some(2),
        "an empty value is no value: {}",
        text(&out)
    );
    // Run or stopped for want of a sandbox, but not refused: no
    // configuration error, and nothing said of the wait.
    assert!(
        !text(&out).contains("config.invalid")
            && !text(&out).contains("backend_wait")
            && !text(&out).contains("ATOMIC_BACKEND_WAIT"),
        "an empty value is no value: {}",
        text(&out)
    );
    let out = atomic(
        &dir,
        &project,
        &["--json", "run", "count-only", "--data", NOTE],
        &[("ATOMIC_BACKEND_WAIT", "0")],
        "",
    );
    assert_eq!(
        stdout_json(&out)["error"]["kind"],
        "config.invalid",
        "{}",
        text(&out)
    );
    assert!(
        stdout_json(&out)["error"]["description"]
            .as_str()
            .is_some_and(
                |d| d.contains("backend_wait, from environment variable ATOMIC_BACKEND_WAIT")
            ),
        "{}",
        text(&out)
    );
    // Written in the manifest, the wait is read like any setting: the
    // project loads, a run runs, and `config --explain` says where it came
    // from.
    let manifest = project.join("atomic.yaml");
    let written = std::fs::read_to_string(&manifest).expect("manifest");
    std::fs::write(&manifest, format!("{written}backend_wait: 300\n")).expect("manifest");
    let out = atomic(
        &dir,
        &project,
        &["--json", "run", "count-only", "--data", NOTE],
        &[],
        "",
    );
    assert!(
        !text(&out).contains("project.invalid")
            && !text(&out).contains("config.invalid")
            && !text(&out).contains("unknown manifest key"),
        "{}",
        text(&out)
    );
    let out = atomic(&dir, &project, &["config", "--explain"], &[], "");
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out)
            .lines()
            .any(|l| l.contains("backend_wait") && l.contains("300") && l.contains("atomic.yaml")),
        "{}",
        text(&out)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `env` with an unused model endpoint, as `atomic runs resume` needs one.
fn with_unused<'a>(env: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    UNUSED_MODEL
        .iter()
        .copied()
        .chain(env.iter().copied())
        .collect()
}

/// Under `atomic serve`, a run whose commands cannot be confined is set
/// aside, unfinished and with nothing recorded, and serve goes on with the
/// others and stops at once when asked. It tries the sandbox again now and
/// then, so once the machine is fixed serve itself finishes the run, its
/// second step once. `atomic run` stops with its exit code instead.
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn serve_sets_aside_a_run_whose_commands_cannot_be_confined_and_serves_the_rest() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())])
            .await
            .expect("fake model");
        let dir = scratch("serve-unconfinable");
        let project = project(&dir);
        write_two_steps(&project, &dir);
        let state = state_dir(&dir, &project);
        let path = bwrap_in_front(&dir);
        let env = model_env(&model.base_url());
        let with_path = with(&env, &[("PATH", path.as_str())]);
        // A run at its gate, with no command left to run after it.
        let mut gated = spawn(
            &dir,
            &project,
            &["run", "gated", "--run-id", "gated-1", "--data", NOTE],
            &with_path,
            &dir.join("gated.log"),
        );
        let rt = Durare::sqlite(&database(&state)).await.expect("store");
        let opened = open_gate(&rt, "gated-1").await;
        gated.kill().expect("SIGKILL the run");
        gated.wait().expect("reap");
        // A run stopped after its first command, with a second to run.
        let (d, p, e) = (dir.clone(), project.clone(), path.clone());
        let out = tokio::task::spawn_blocking(move || {
            atomic(
                &d,
                &p,
                &["run", "two-steps", "--run-id", "two-1", "--data", NOTE],
                &[
                    ("PATH", e.as_str()),
                    ("ATOMIC_FAILPOINT", "pipeline:after:first"),
                ],
                "",
            )
        })
        .await
        .expect("child");
        assert!(!out.status.success(), "{}", text(&out));

        // The sandbox cannot start now.
        std::fs::write(dir.join("bwrap-refuse"), "").expect("refuse");
        let (d, p, e) = (dir.clone(), project.clone(), path.clone());
        let out = tokio::task::spawn_blocking(move || {
            let mut env = UNUSED_MODEL.to_vec();
            env.push(("PATH", e.as_str()));
            atomic(&d, &p, &["runs", "resume", "two-1"], &env, "")
        })
        .await
        .expect("child");
        assert_eq!(
            out.status.code(),
            Some(atomic_app::workflows::UNCONFINABLE_EXIT),
            "{}",
            text(&out)
        );
        let log = dir.join("serve.log");
        let mut serve = spawn(&dir, &project, &["serve"], &with_path, &log);
        wait_until("serve to set the run aside", || {
            std::fs::read_to_string(&log).is_ok_and(|l| l.contains("until the sandbox can start"))
        });
        approve(&rt, &opened).await;
        let started = Instant::now();
        while rt.status("gated-1").await.expect("status") != Some(RunStatus::Succeeded) {
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "serve never finished the other run: {}",
                std::fs::read_to_string(&log).unwrap_or_default()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(serve.try_wait().expect("wait").is_none(), "serve goes on");
        assert_eq!(
            rt.status("two-1").await.expect("status"),
            Some(RunStatus::Pending),
            "set aside, with nothing recorded"
        );
        let stopping = Instant::now();
        terminate(&serve);
        let status = tokio::task::spawn_blocking(move || wait_child(&mut serve, "serve"))
            .await
            .expect("child");
        let text_log = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(status.success(), "{text_log}");
        assert!(
            stopping.elapsed() < Duration::from_secs(8),
            "a set-aside run is not one a stop waits for: {text_log}"
        );
        assert_eq!(
            steps_log(&dir),
            ["first start", "first end"],
            "second never ran"
        );

        // A serve started while the sandbox still cannot start sets the run
        // aside again, and once the machine is fixed it finishes the run
        // itself, its second step once.
        let log = dir.join("serve-2.log");
        let mut serve = spawn(&dir, &project, &["serve"], &with_path, &log);
        wait_until("serve to set the run aside again", || {
            std::fs::read_to_string(&log).is_ok_and(|l| l.contains("until the sandbox can start"))
        });
        std::fs::remove_file(dir.join("bwrap-refuse")).expect("fixed");
        let started = Instant::now();
        while rt.status("two-1").await.expect("status") != Some(RunStatus::Succeeded) {
            assert!(
                started.elapsed() < Duration::from_secs(90),
                "serve never tried the sandbox again: {}",
                std::fs::read_to_string(&log).unwrap_or_default()
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let text_log = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(text_log.contains("the sandbox starts again"), "{text_log}");
        terminate(&serve);
        let status = tokio::task::spawn_blocking(move || wait_child(&mut serve, "serve"))
            .await
            .expect("child");
        assert!(
            status.success(),
            "{}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        // Each step's command ran once, in order, by the log the run kept.
        let out: Value = rt.result("two-1").await.expect("finished");
        assert_eq!(out["steps"], json!(["first start", "first end", "second"]));
    });
}

/// State under /tmp would put atom commands' scratch directories where
/// sandboxed turns may write, so atomic refuses to run there.
#[test]
fn state_under_the_temporary_directory_is_refused() {
    let dir = scratch("tmp-state");
    let project = project(&dir);
    let home =
        std::env::temp_dir().join(format!("atomic-run-cli-tmp-state-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let home_text = home.to_string_lossy().into_owned();
    let out = atomic(
        &dir,
        &project,
        &["run", "count-note", "--data", NOTE],
        &[("ATOMIC_HOME", &home_text)],
        "",
    );
    let _ = std::fs::remove_dir_all(&home);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("where a sandboxed turn may write"),
        "{}",
        text(&out)
    );
    assert!(count_log(&dir) == 0, "nothing ran");
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn exec_commands_run_in_a_runner_process_when_asked() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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
    assert_eq!(count_log(&dir), 2);

    // A runner process starts the command. The command cannot see what
    // started it, so it asks from here. The runner is non-dumpable, so its
    // environment cannot be read from here; that it gets only PATH from
    // atomic's is tested where its environment is set (atomic-exec's runner
    // tests).
    let log = command_log(&dir, "parent-env.log");
    std::fs::write(
        project.join("tools/parent-env.py"),
        "import json, os, sys, time\n\
         json.load(sys.stdin)\n\
         log = os.path.join(os.environ['ATOMIC_CACHE_DIR'], os.path.basename(sys.argv[1]))\n\
         open(log.removesuffix('.log') + '.ask', 'w').close()\n\
         done = log.removesuffix('.log') + '.done'\n\
         for _ in range(500):\n\
         \x20   if os.path.exists(done): break\n\
         \x20   time.sleep(0.02)\n\
         json.dump({'seen': os.path.exists(done)}, sys.stdout)\n",
    )
    .expect("script");
    write_atom(
        &project,
        "parent-env",
        &format!(
            "description: 'Ask for the runner.', kind: 'exec', exec: {{ command: ['python3', 'tools/parent-env.py', '{}'] }}",
            log.display()
        ),
        "",
        object(json!({"text": {"type": "string"}})),
        object(json!({"seen": {"type": "boolean"}})),
    );
    let names = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = names.clone();
    let watch = RunnerWatch::new(vec![log], move |runner| {
        // Its command line stays readable, and says what it is.
        let cmdline = std::fs::read(format!("/proc/{runner}/cmdline")).expect("cmdline");
        let found: Vec<String> = cmdline
            .split(|b| *b == 0)
            .skip(1)
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .filter(|a| !a.is_empty())
            .collect();
        *seen.lock().expect("names") = found;
    });
    let out = atomic(
        &dir,
        &project,
        &["run", "parent-env", "--runner", "process", "--data", NOTE],
        &[],
        "",
    );
    drop(watch);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        stdout_json(&out),
        json!({"seen": true}),
        "a runner started it"
    );
    assert_eq!(*names.lock().expect("names"), ["runner"]);
}

/// An exec atom whose command logs that it ran, then, the first time or
/// `always`, has the process that started it, with `--runner process` its
/// runner, SIGKILLed by a [`RunnerWatch::killing`] on its log, which this
/// returns.
fn write_runner_killer(
    project: &Path,
    dir: &Path,
    name: &str,
    effects: &str,
    always: bool,
) -> PathBuf {
    let log = command_log(dir, &format!("{name}.log"));
    std::fs::write(
        project.join("tools/kill-runner.py"),
        "import json, os, sys, time\n\
         note = json.load(sys.stdin)\n\
         log = os.path.join(os.environ['ATOMIC_CACHE_DIR'], os.path.basename(sys.argv[1]))\n\
         open(log, 'a').write('ran\\n')\n\
         if os.environ.get('KILL_ALWAYS') or not os.path.exists(log + '.killed'):\n\
         \x20   open(log + '.killed', 'w').close()\n\
         \x20   open(log.removesuffix('.log') + '.ask', 'w').close()\n\
         \x20   time.sleep(30)\n\
         \x20   sys.exit(1)\n\
         json.dump({'title': note['text'][:25], 'words': len(note['text'].split()), \
         'ran': len(open(log).read().splitlines())}, sys.stdout)\n",
    )
    .expect("script");
    write_atom(
        project,
        name,
        &format!(
            "description: 'Kill the runner.', kind: 'exec', exec: {{ command: ['python3', 'tools/kill-runner.py', '{}'] }}{}{effects}",
            log.display(),
            if always {
                ", env: { KILL_ALWAYS: '1' }"
            } else {
                ""
            }
        ),
        "",
        object(json!({"text": {"type": "string"}})),
        object(
            json!({"title": {"type": "string"}, "words": {"type": "integer"}, "ran": {"type": "integer"}}),
        ),
    );
    log
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_command_that_kills_its_runner_is_never_run_again_unless_it_may_be() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    let dir = scratch("runner-killed");
    let project = project(&dir);
    let undeclared = write_runner_killer(&project, &dir, "undeclared", "", false);
    let idempotent = write_runner_killer(
        &project,
        &dir,
        "idempotent",
        ", effects: { external: 'idempotent' }",
        false,
    );
    let watch = RunnerWatch::killing(vec![undeclared.clone(), idempotent.clone()]);
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
    // Its runner was killed under it once, and it never ran again: run
    // again, it would have finished, since it kills only the first time.
    assert_eq!(watch.acted(&undeclared), 1, "killed once");
    assert_eq!(watch.ran(&undeclared), 1, "it ran once");

    // Declared idempotent: the step is retried, and the second run finishes.
    let out = atomic(
        &dir,
        &project,
        &["run", "idempotent", "--runner", "process", "--data", NOTE],
        &[],
        "",
    );
    assert!(out.status.success(), "{}", text(&out));
    // Killed once, then run once more to its end, as declared: its own
    // tally says it ran twice.
    assert_eq!(
        stdout_json(&out),
        json!({"title": "Exporting a file named ca", "words": 8, "ran": 2})
    );
    assert_eq!(watch.acted(&idempotent), 1, "killed once");
}

/// An idempotent exec atom whose command starts a child that sleeps, logs
/// `start`, then waits a minute before it would answer. The command and its
/// child have the log's path on their command lines, for [`slow_pids`].
fn write_slow(project: &Path, dir: &Path) {
    let log = command_log(dir, "slow.log");
    std::fs::write(
        project.join("tools/slow.py"),
        "import json, os, subprocess, sys, time\n\
         note = json.load(sys.stdin)\n\
         log = os.path.join(os.environ['ATOMIC_CACHE_DIR'], os.path.basename(sys.argv[1]))\n\
         child = subprocess.Popen(['python3', '-c', 'import time; time.sleep(60)', sys.argv[1]])\n\
         open(log, 'a').write('start\\n')\n\
         time.sleep(60)\n\
         open(log, 'a').write('end\\n')\n\
         json.dump({'title': note['text'][:25], 'words': len(note['text'].split())}, sys.stdout)\n",
    )
    .expect("script");
    write_atom(
        project,
        "slow",
        &format!(
            "description: 'Take a minute.', kind: 'exec', exec: {{ command: ['python3', 'tools/slow.py', '{}'] }}, effects: {{ external: 'idempotent' }}",
            log.display()
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

/// How many times the slow command has started.
fn slow_starts(dir: &Path) -> usize {
    std::fs::read_to_string(run_file(dir, "slow.log"))
        .unwrap_or_default()
        .lines()
        .filter(|l| *l == "start")
        .count()
}

/// The processes of the slow command that run now, with everything that
/// started it inside its sandbox, and its child.
fn slow_pids(dir: &Path) -> Vec<String> {
    pids_with(&command_log(dir, "slow.log").display().to_string())
}

/// Whether serve has said it stopped or failed.
fn serve_ended(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("serve.log"))
        .is_ok_and(|l| l.contains("error") || l.contains("stopping"))
}

/// Whether process `pid` still runs: `kill -0` reaches it, and on Linux it
/// is not a zombie waiting to be reaped. On Linux a process whose stat can
/// no longer be read has been reaped since `kill -0` reached it, so it is
/// gone too.
fn alive(pid: &str) -> bool {
    let reached = Command::new("/bin/sh")
        .args(["-c", &format!("kill -0 {pid} 2>/dev/null")])
        .status()
        .expect("kill")
        .success();
    let gone = || match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .unwrap_or("")
            .trim_start()
            .starts_with('Z'),
        Err(_) => true,
    };
    reached && !(cfg!(target_os = "linux") && gone())
}

/// Wait until no process with `marker` on its command line runs.
fn all_gone_with(marker: &str, what: &str) {
    let started = Instant::now();
    while let Some(pid) = pids_with(marker).first() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{what}: process {pid} survived"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
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
        .open(database(state).with_file_name("runtime.lock"))
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
/// parent-death signal stops it, and the sandbox it runs in with it; this
/// kills whatever is left. Returns the project's state.
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
    wait_until("the command to start", || slow_starts(dir) == 1);
    let pids = slow_pids(dir);
    run.kill().expect("SIGKILL atomic run");
    run.wait().expect("reap");
    if cfg!(target_os = "linux") {
        // Well before it would end by itself.
        all_gone(&pids, "the command outlived atomic");
    }
    let pids = slow_pids(dir);
    if !pids.is_empty() {
        let _ = Command::new("/bin/sh")
            .args(["-c", &format!("kill -KILL {} 2>/dev/null", pids.join(" "))])
            .status();
    }
    all_gone(&pids, "cleaning up");
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

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn serve_stopped_mid_command_holds_the_lock_until_it_exits_and_runs_nothing_twice() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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
        slow_starts(&dir) == 2 || serve_ended(&dir)
    });
    assert_eq!(
        slow_starts(&dir),
        2,
        "{}",
        std::fs::read_to_string(dir.join("serve.log")).unwrap_or_default()
    );
    let pids = slow_pids(&dir);
    assert!(
        !pids.is_empty(),
        "the slow command's processes were found before the stop"
    );
    terminate(&serve);
    // While any command runs, the lock stays held, and it goes only with
    // serve. The lock is probed first and the commands checked after: a
    // command alive after a probe that found the lock free was alive during
    // it, so the order is checked with no window to time.
    let stopping = Instant::now();
    let status = loop {
        let held = lock_held(&state);
        let running: Vec<&String> = pids.iter().filter(|pid| alive(pid)).collect();
        assert!(
            held || running.is_empty(),
            "the lock was free while command {running:?} still ran"
        );
        if let Some(status) = serve.try_wait().expect("wait") {
            break status;
        }
        if !held {
            // Only serve's exit frees it: the kernel releases the lock as the
            // process ends, a moment before it can be reaped.
            let freed = Instant::now();
            break loop {
                if let Some(status) = serve.try_wait().expect("wait") {
                    break status;
                }
                assert!(
                    freed.elapsed() < Duration::from_secs(5),
                    "the lock was free while serve still ran"
                );
                std::thread::sleep(Duration::from_millis(20));
            };
        }
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
    let ends = std::fs::read_to_string(run_file(&dir, "slow.log")).expect("log");
    assert!(!ends.contains("end"), "no command ran to its end: {ends}");
    assert_eq!(slow_starts(&dir), 2, "started twice, never at once");
}

/// The slow command made stubborn: it and the grandchild it starts ignore
/// SIGTERM, and the grandchild leaves its process group and session and
/// holds a gigabyte it has written, which takes the kernel a while to free
/// once it is killed, so a process that exits before it has ended is seen.
const STUBBORN: &str = "import json, os, signal, subprocess, sys, time\n\
    signal.signal(signal.SIGTERM, signal.SIG_IGN)\n\
    note = json.load(sys.stdin)\n\
    log = os.path.join(os.environ['ATOMIC_CACHE_DIR'], os.path.basename(sys.argv[1]))\n\
    child = subprocess.Popen(['python3', '-c', 'import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); held = bytearray(1 << 30); held[::4096] = b\\'x\\' * (1 << 18); time.sleep(60)', sys.argv[1]], start_new_session=True)\n\
    open(log, 'a').write('start\\n')\n\
    time.sleep(60)\n\
    open(log, 'a').write('end\\n')\n\
    json.dump({'title': note['text'][:25], 'words': len(note['text'].split())}, sys.stdout)\n";

/// A stopped serve holds the execution lock until every process of the
/// command it stopped has ended, under either runner: one that ignores
/// SIGTERM and one that left the command's process group and session
/// included. The lock is probed first and the processes checked after, so
/// a process alive after a probe that found the lock free was alive
/// during it.
// needs_sandbox
#[cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#[test]
fn serve_holds_the_lock_until_every_process_of_a_stopped_command_has_ended() {
    support::require_sandbox();
    for runner in ["local", "process"] {
        let dir = scratch(&format!("serve-stubborn-{runner}"));
        let project = project(&dir);
        let state = slow_run_killed_mid_command(&dir, &project, "stubborn-1");
        std::fs::write(project.join("tools/slow.py"), STUBBORN).expect("script");
        let log = dir.join("serve.log");
        let mut serve = spawn(
            &dir,
            &project,
            &["serve", "--runner", runner],
            &UNUSED_MODEL,
            &log,
        );
        wait_until("serve to start the command again", || {
            slow_starts(&dir) == 2 || serve_ended(&dir)
        });
        let text = || std::fs::read_to_string(&log).unwrap_or_default();
        assert_eq!(slow_starts(&dir), 2, "{runner}: {}", text());
        // The grandchild is in a session of its own.
        let started = Instant::now();
        let pids = loop {
            let pids = slow_pids(&dir);
            let sessions: std::collections::BTreeSet<String> =
                pids.iter().filter_map(|pid| session_of(pid)).collect();
            if sessions.len() >= 2 {
                break pids;
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "{runner}: no process of the command in a session of its own: {pids:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        terminate(&serve);
        let stopping = Instant::now();
        let status = loop {
            let held = lock_held(&state);
            let running: Vec<&String> = pids.iter().filter(|pid| alive(pid)).collect();
            assert!(
                held || running.is_empty(),
                "{runner}: the lock was free while {running:?} still ran: {}",
                text()
            );
            if let Some(status) = serve.try_wait().expect("wait") {
                break status;
            }
            assert!(
                stopping.elapsed() < Duration::from_secs(30),
                "{runner}: serve did not stop: {}",
                text()
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(
            !status.success(),
            "{runner}: a command was stopped: {}",
            text()
        );
        assert!(
            text().contains("stopped 1 command(s)"),
            "{runner}: {}",
            text()
        );
        all_gone(&pids, "the stopped command and its grandchild");
        assert!(
            !text().contains("have not ended yet"),
            "{runner}: every process ended at its kill: {}",
            text()
        );
        let ends = std::fs::read_to_string(run_file(&dir, "slow.log")).expect("log");
        assert!(
            !ends.contains("end"),
            "{runner}: no command ran to its end: {ends}"
        );
    }
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_second_signal_stops_serve_at_once_with_everything_its_command_started() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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
            slow_starts(&dir) == 2 || serve_ended(&dir)
        });
        assert_eq!(
            slow_starts(&dir),
            2,
            "{runner}: {}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        let pids = slow_pids(&dir);
        if runner == "process" && cfg!(target_os = "linux") {
            let log = command_log(&dir, "slow.log");
            let started_by = runner_of(&log.display().to_string()).expect("a runner started it");
            assert_eq!(
                parent_of(&started_by),
                Some(serve.id().to_string()),
                "serve's own runner"
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
        assert_eq!(slow_starts(&dir), 2);
    }
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_signal_stops_run_and_resume_with_everything_their_command_started() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    // Each stop signal, run and resume, both runners, and both paths: a
    // second signal at once, or one signal and the ten seconds the running
    // step has to reach its checkpoint.
    for (command, sig, runner, twice) in [
        ("run", "INT", "local", true),
        ("run", "TERM", "process", true),
        ("run", "HUP", "local", true),
        ("run", "QUIT", "process", false),
        ("resume", "INT", "process", true),
        ("resume", "TERM", "local", false),
    ] {
        let case = format!("{command} {sig} {runner}");
        let dir = scratch("stopped");
        let project = project(&dir);
        let log = dir.join("stopped.log");
        let (mut child, starts) = if command == "run" {
            write_slow(&project, &dir);
            let child = spawn(
                &dir,
                &project,
                &[
                    "run", "slow", "--run-id", "slow-3", "--runner", runner, "--data", NOTE,
                ],
                &[],
                &log,
            );
            (child, 1)
        } else {
            slow_run_killed_mid_command(&dir, &project, "slow-3");
            let child = spawn(
                &dir,
                &project,
                &["runs", "resume", "slow-3", "--runner", runner],
                &UNUSED_MODEL,
                &log,
            );
            (child, 2)
        };
        wait_until("the command to start", || slow_starts(&dir) == starts);
        let pids = slow_pids(&dir);
        let text = if twice {
            stop_twice(&mut child, sig, &log, 130)
        } else {
            let stopping = Instant::now();
            signal(&child, sig);
            let status = wait_child(&mut child, &case);
            let text = std::fs::read_to_string(&log).expect("log");
            assert!(
                stopping.elapsed() >= Duration::from_secs(9),
                "{case}: the running step had its time: {text}"
            );
            assert_eq!(status.code(), Some(130), "{case}: {text}");
            assert!(text.contains("stopped 1 command(s)"), "{case}: {text}");
            text
        };
        assert!(text.contains("atomic runs resume slow-3"), "{case}: {text}");
        all_gone(&pids, &case);
    }
}

#[test]
fn atomic_run_reading_its_input_stops_on_a_signal() {
    let dir = scratch("reading");
    let project = project(&dir);
    for sig in ["INT", "TERM"] {
        let mut child = command_as(
            Path::new(ATOMIC),
            &dir,
            &project,
            &["run", "count-note", "--input", "-"],
            &[],
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start atomic");
        // Its standard input stays open: it is still reading.
        std::thread::sleep(Duration::from_millis(500));
        signal(&child, sig);
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().expect("wait") {
                break status;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{sig}: atomic kept reading"
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(!status.success(), "{sig}");
        assert!(count_log(&dir) == 0, "{sig}: nothing ran");
    }
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_run_that_keeps_stopping_is_dead_lettered_after_a_few_tries() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    let dir = scratch("dead-letter");
    let project = project(&dir);
    // Idempotent, so each recovery may run it again, and its runner always
    // dies under it.
    let doomed = write_runner_killer(
        &project,
        &dir,
        "doomed",
        ", effects: { external: 'idempotent' }",
        true,
    );
    let watch = RunnerWatch::killing(vec![doomed.clone()]);
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
    // Each try's runner was killed under it, and by its own tally it ran
    // once, then once per recovery.
    let tries = 1 + atomic_app::pipeline::RECOVERY_LIMIT as usize;
    assert_eq!(watch.acted(&doomed), tries, "killed each try");
    assert_eq!(watch.ran(&doomed), tries, "once, then once per recovery");
}

/// A workflow of two undeclared exec steps: `first` logs `first start`,
/// takes three seconds and logs `first end`; `second` logs `second`. The
/// log is in the run's caches, and each returns it as `steps`, so the
/// workflow's output says what ran in the run, and in what order.
fn write_two_steps(project: &Path, dir: &Path) {
    let log = command_log(dir, "steps.log");
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
                 def log(line): open(os.path.join(os.environ['ATOMIC_CACHE_DIR'], os.path.basename(os.environ['STEPS_LOG'])), 'a').write(line + '\\n')\n\
                 note = json.load(sys.stdin)\n\
                 {body}\
                 steps = open(os.path.join(os.environ['ATOMIC_CACHE_DIR'], os.path.basename(os.environ['STEPS_LOG']))).read().splitlines()\n\
                 json.dump({{'title': note['text'][:25], 'words': len(note['text'].split()), 'steps': steps}}, sys.stdout)\n"
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
            object(
                json!({"title": {"type": "string"}, "words": {"type": "integer"}, "steps": {"type": "array", "items": {"type": "string"}}}),
            ),
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
    std::fs::read_to_string(run_file(dir, "steps.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn no_step_starts_after_the_first_signal() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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
        &["--json", "runs", "resume", "two-1"],
        &UNUSED_MODEL,
        "",
    );
    assert!(out.status.success(), "{}", text(&out));
    // Each step's command ran once, in order, by the log the run kept.
    assert_eq!(
        stdout_json(&out)["output"]["steps"],
        json!(["first start", "first end", "second"])
    );
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn serve_restarts_do_not_park_a_healthy_run() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
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
        let rt = Durare::sqlite(&database(&state)).await.expect("store");
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

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_command_that_signals_its_own_group_does_not_kill_its_runner() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    let dir = scratch("group-signal");
    let project = project(&dir);
    // The command shares its runner's process group, and sends the whole
    // group SIGTERM, which it ignores itself.
    write_atom(
        &project,
        "group-signal",
        r#"description: 'Signal the group.', kind: 'exec', exec: { command: ['sh', '-c', "trap '' TERM; kill -TERM 0; cat > /dev/null; printf '{\"title\": \"t\", \"words\": 1}'"] }"#,
        "",
        object(json!({"text": {"type": "string"}})),
        object(json!({"title": {"type": "string"}, "words": {"type": "integer"}})),
    );
    let out = atomic(
        &dir,
        &project,
        &["run", "group-signal", "--runner", "process", "--data", NOTE],
        &[],
        "",
    );
    assert!(
        out.status.success(),
        "the runner reported it: {}",
        text(&out)
    );
    assert_eq!(stdout_json(&out), json!({"title": "t", "words": 1}));
}

/// A workflow of two exec steps: `mark`, and then `tally`, idempotent,
/// which logs the run it ran for in the run's caches and returns the log as
/// `tallied`.
fn write_tally(project: &Path, dir: &Path) {
    let log = command_log(dir, "tally.log");
    for (name, body, effects) in [
        ("mark", "out['tallied'] = []\n", ""),
        (
            "tally",
            "at = os.path.join(os.environ['ATOMIC_CACHE_DIR'], os.path.basename(os.environ['TALLY_LOG']))\n\
             open(at, 'a').write(os.environ['ATOMIC_RUN_ID'] + '\\n')\n\
             out['tallied'] = open(at).read().splitlines()\n",
            ", effects: { external: 'idempotent' }",
        ),
    ] {
        std::fs::write(
            project.join(format!("tools/{name}.py")),
            format!(
                "import json, os, sys\n\
                 note = json.load(sys.stdin)\n\
                 out = {{'title': note['text'][:25], 'words': len(note['text'].split())}}\n\
                 {body}\
                 json.dump(out, sys.stdout)\n"
            ),
        )
        .expect("script");
        write_atom(
            project,
            name,
            &format!(
                "description: '{name}', kind: 'exec', exec: {{ command: ['python3', 'tools/{name}.py'] }}, env: {{ TALLY_LOG: '{}' }}{effects}",
                log.display()
            ),
            "",
            object(json!({"text": {"type": "string"}})),
            object(
                json!({"title": {"type": "string"}, "words": {"type": "integer"}, "tallied": {"type": "array", "items": {"type": "string"}}}),
            ),
        );
    }
    std::fs::write(
        project.join("workflows/tallied.workflow.yaml"),
        "atomic: 1\nname: tallied\n\
         input: { $ref: ../atoms/mark.input.schema.json }\n\
         output: { $ref: ../atoms/tally.output.schema.json }\n\
         steps:\n  mark:\n    atom: mark\n    with: { text: input.text }\n\
         \x20 tally:\n    after: [mark]\n    atom: tally\n    with: { text: steps.mark.output.title }\n\
         returns: steps.tally.output\n",
    )
    .expect("workflow");
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn serve_started_on_many_queued_runs_runs_each_once() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("many-queued");
        let project = project(&dir);
        write_tally(&project, &dir);
        let state = state_dir(&dir, &project);
        let ids: Vec<String> = (0..12).map(|n| format!("tally-{n}")).collect();
        for id in &ids {
            // Killed after `mark`, then resumed onto its queue and killed
            // again before anything takes it from there.
            let out = atomic(
                &dir,
                &project,
                &["run", "tallied", "--run-id", id, "--data", NOTE],
                &[("ATOMIC_FAILPOINT", "pipeline:after:mark")],
                "",
            );
            assert!(!out.status.success(), "{}", text(&out));
            let out = atomic(
                &dir,
                &project,
                &["runs", "resume", id],
                &with(
                    &UNUSED_MODEL.map(|(k, v)| (k, v.to_owned())),
                    &[("ATOMIC_FAILPOINT", "resume:queued")],
                ),
                "",
            );
            assert!(!out.status.success(), "{}", text(&out));
        }
        let mut serve = spawn(
            &dir,
            &project,
            &["serve"],
            &UNUSED_MODEL,
            &dir.join("serve.log"),
        );
        let rt = Durare::sqlite(&database(&state)).await.expect("store");
        let started = Instant::now();
        for id in &ids {
            while rt.status(id).await.expect("status") != Some(RunStatus::Succeeded) {
                assert!(
                    started.elapsed() < Duration::from_secs(90),
                    "{id} never finished: {}",
                    std::fs::read_to_string(dir.join("serve.log")).unwrap_or_default()
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        // Give a second execution, if there were one, its time to show.
        tokio::time::sleep(Duration::from_secs(2)).await;
        terminate(&serve);
        let status = tokio::task::spawn_blocking(move || wait_child(&mut serve, "serve"))
            .await
            .expect("child");
        assert!(status.success());
        // Each run's command ran once, for that run, by the log the run
        // kept.
        for id in &ids {
            let out: Value = rt.result(id).await.expect("result");
            assert_eq!(out["tallied"], json!([id]), "{id}'s command ran once");
        }
    });
}

#[test]
fn signals_ignored_when_serve_starts_stay_ignored() {
    let dir = scratch("nohup");
    let project = project(&dir);
    let log = dir.join("serve.log");
    // As `nohup`, and a shell starting a background job, leave them.
    let script = format!("trap '' HUP INT QUIT; exec '{ATOMIC}' serve");
    let mut serve = command_as(
        Path::new("/bin/sh"),
        &dir,
        &project,
        &["-c", &script],
        &UNUSED_MODEL,
    )
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(std::fs::File::create(&log).expect("log"))
    .spawn()
    .expect("start serve");
    wait_until("serve to start", || {
        std::fs::read_to_string(&log).is_ok_and(|l| l.contains("serving until stopped"))
    });
    for sig in ["HUP", "INT", "QUIT"] {
        signal(&serve, sig);
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            serve.try_wait().expect("wait").is_none(),
            "SIG{sig} stopped it: {}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
    }
    // What it does not ignore still stops it.
    terminate(&serve);
    let status = wait_child(&mut serve, "serve");
    assert!(
        status.success(),
        "{}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_leftover_holding_a_commands_output_does_not_hold_its_step_under_the_runner() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    let dir = scratch("runner-leftover");
    let project = project(&dir);
    // The command leaves a child that keeps its stdout, in the runner's
    // group, and is found by the marker on its command line.
    let marker = dir.join("leftover-marker").display().to_string();
    write_atom(
        &project,
        "leaves-a-child",
        &format!(
            r#"description: 'Leave a child.', kind: 'exec', exec: {{ command: ['sh', '-c', "python3 -c 'import time; time.sleep(30)' '{marker}' & cat > /dev/null; printf '{{\"title\": \"t\", \"words\": 1}}'"] }}"#
        ),
        "",
        object(json!({"text": {"type": "string"}})),
        object(json!({"title": {"type": "string"}, "words": {"type": "integer"}})),
    );
    let started = Instant::now();
    let out = atomic(
        &dir,
        &project,
        &[
            "run",
            "leaves-a-child",
            "--runner",
            "process",
            "--data",
            NOTE,
        ],
        &[],
        "",
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "the step waited {:?} for the child",
        started.elapsed()
    );
    all_gone_with(&marker, "the child went with its runner's group");
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_run_that_gets_further_each_time_is_not_dead_lettered() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    let dir = scratch("progress");
    let project = project(&dir);
    // More steps than one process may recover a run at the same step, each
    // of whose runners dies once under it.
    let n = atomic_app::pipeline::RECOVERY_LIMIT as usize + 2;
    let mut workflow = String::from(
        "atomic: 1\nname: bumpy\n\
         input: { $ref: ../atoms/bump-0.input.schema.json }\n\
         output: { $ref: ../atoms/bump-0.output.schema.json }\nsteps:\n",
    );
    let mut logs = Vec::new();
    for i in 0..n {
        logs.push(write_runner_killer(
            &project,
            &dir,
            &format!("bump-{i}"),
            ", effects: { external: 'idempotent' }",
            false,
        ));
        let text = if i == 0 {
            "input.text".to_owned()
        } else {
            format!("steps.s{}.output.title", i - 1)
        };
        let after = if i == 0 {
            String::new()
        } else {
            format!("    after: [s{}]\n", i - 1)
        };
        workflow.push_str(&format!(
            "  s{i}:\n{after}    atom: bump-{i}\n    with: {{ text: {text} }}\n"
        ));
    }
    workflow.push_str(&format!("returns: steps.s{}.output\n", n - 1));
    std::fs::write(project.join("workflows/bumpy.workflow.yaml"), workflow).expect("workflow");
    let watch = RunnerWatch::killing(logs.clone());
    let out = atomic(
        &dir,
        &project,
        &["run", "bumpy", "--runner", "process", "--data", NOTE],
        &[],
        "",
    );
    assert!(out.status.success(), "{}", text(&out));
    for (i, log) in logs.iter().enumerate() {
        // Killed once, then run again to its end: its recorded output keeps
        // the tally it ran with.
        assert_eq!(watch.acted(log), 1, "step {i}");
        assert_eq!(ran(&dir, &format!("bump-{i}")), 2, "step {i}");
    }
}

/// git in `dir`, as a person runs it, with an identity of its own.
fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=dev",
            "-c",
            "user.email=dev@example.com",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {}", text(&out));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A commit of `file` with `text` in the repository at `dir`, returning it.
fn commit_file(dir: &Path, file: &str, text: &str) -> String {
    std::fs::write(dir.join(file), text).expect("file");
    git_in(dir, &["add", file]);
    git_in(dir, &["commit", "-q", "-m", text]);
    git_in(dir, &["rev-parse", "HEAD"])
}

/// Everything git records about a repository's refs and worktrees.
fn git_state(dir: &Path) -> String {
    format!(
        "{}\n{}",
        git_in(dir, &["for-each-ref"]),
        git_in(dir, &["worktree", "list", "--porcelain"])
    )
}

/// What `git status` sees in the repository at `dir`: every change and every
/// untracked file, which [`git_state`]'s refs and worktree list do not cover.
fn git_status(dir: &Path) -> String {
    git_in(dir, &["status", "--porcelain", "--untracked-files=all"])
}

/// The coding example in the catalog, as written.
fn example() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/02-change-request")
}

/// The JSON in `path`.
fn read_json(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).expect("read");
    serde_json::from_str(&text).expect("JSON")
}

/// `source`'s tree under `destination`, as a person copies a project: git's
/// own metadata stays behind, and so do atomic's generated declarations,
/// which `atomic check` writes afresh.
fn copy_tree(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).expect("directory");
    for entry in std::fs::read_dir(source).expect("source") {
        let entry = entry.expect("entry");
        let name = entry.file_name();
        if [".git", ".atomic"].contains(&name.to_string_lossy().as_ref()) {
            continue;
        }
        let target = destination.join(name);
        if entry.file_type().expect("type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("copy");
        }
    }
}

/// A directory of the test's own outside every git repository, under the
/// system's temporary directory: the target directory sits inside the
/// checkout atomic is built from. Checked, so the test fails rather than run
/// inside a repository if that ever changes.
fn outside_repository(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("atomic-run-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("directory");
    let dir = std::fs::canonicalize(&dir).expect("canonical");
    let inside = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(&dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git");
    assert!(
        !inside.status.success(),
        "{} is inside a git work tree: {}",
        dir.display(),
        text(&inside)
    );
    dir
}

/// [`spawn`], with the run's stdout in `out` beside its stderr in `log`.
fn spawn_logged(
    dir: &Path,
    cwd: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    log: &Path,
    out: &Path,
) -> Child {
    command_as(Path::new(ATOMIC), dir, cwd, args, env)
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(out).expect("stdout"))
        .stderr(std::fs::File::create(log).expect("stderr"))
        .spawn()
        .expect("start atomic")
}

/// A workflow whose second step changes code in a worktree, after a gate.
fn write_coding(project: &Path) {
    write_atom(
        project,
        "fix-code",
        "description: 'Fix the code.', kind: 'codex', sandbox: 'workspace-write'",
        "Fix this: {input.task}",
        object(
            json!({"base": {"type": "string", "format": "git-commit"}, "task": {"type": "string"}}),
        ),
        object(json!({"done": {"type": "boolean"}})),
    );
    std::fs::write(
        project.join("workflows/coding.workflow.yaml"),
        "atomic: 1\nname: coding\n\
         input: { $ref: ../atoms/fix-code.input.schema.json }\n\
         output: { $ref: ../atoms/fix-code.output.schema.json }\n\
         steps:\n  check:\n    gate:\n      title: \"'Start on ' + input.task\"\n      timeout: 24h\n\
         \x20 fix:\n    after: [check]\n    atom: fix-code\n    with: { base: input.base, task: input.task }\n    worktree: true\n\
         returns: steps.fix.output\n",
    )
    .expect("workflow");
}

#[test]
fn a_coding_run_is_bound_to_its_fixture_and_never_touches_a_checkout() {
    on_codex_runtime(async {
        let dir = scratch("fixture");
        let project = project(&dir);
        write_coding(&project);
        // The developer's checkout holds the project.
        git_in(&project, &["init", "-q"]);
        git_in(&project, &["add", "."]);
        git_in(&project, &["commit", "-q", "-m", "project"]);
        // The fixture the operator prepared.
        let fixture = dir.join("fixture");
        std::fs::create_dir_all(&fixture).expect("fixture");
        git_in(&fixture, &["init", "-q"]);
        let first = commit_file(&fixture, "price.txt", "125");
        let second = commit_file(&fixture, "price.txt", "250");
        let (checkout, source) = (git_state(&project), git_state(&fixture));
        let state = state_dir(&dir, &project);

        let data = |base: &str| json!({"base": base, "task": "multiply by quantity"}).to_string();
        let fixture_arg = fixture.to_string_lossy().into_owned();
        // Each of these fails before anything runs.
        let refused = |args: Vec<String>, kind: &str| {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let out = atomic(&dir, &project, &args, &UNUSED_MODEL, "");
            assert!(!out.status.success(), "{args:?}: {}", text(&out));
            let error = stdout_json(&out)["error"].clone();
            assert_eq!(error["kind"], kind, "{args:?}: {error}");
            assert!(
                !String::from_utf8_lossy(&out.stderr).contains("run run-"),
                "{args:?}: no run was started"
            );
            error
        };
        let args = |rest: &[&str]| -> Vec<String> {
            ["--json", "run", "coding"]
                .iter()
                .chain(rest)
                .map(|s| (*s).to_owned())
                .collect()
        };
        let good = data(&second);
        refused(args(&["--data", &good]), "fixture.required");
        refused(
            args(&[
                "--data",
                &good,
                "--fixture",
                "/no/such/repo",
                "--fixture-rev",
                "main",
            ]),
            "fixture.path",
        );
        refused(
            args(&[
                "--data",
                &good,
                "--fixture",
                &fixture_arg,
                "--fixture-rev",
                "no-such-branch",
            ]),
            "fixture.revision",
        );
        let placeholder = data("1111111111111111111111111111111111111111");
        let error = refused(
            args(&[
                "--data",
                &placeholder,
                "--fixture",
                &fixture_arg,
                "--fixture-rev",
                "main",
            ]),
            "fixture.input",
        );
        assert!(
            error["description"]
                .as_str()
                .is_some_and(|d| d.contains("placeholder")),
            "{error}"
        );
        let stale = data(&first);
        let error = refused(
            args(&[
                "--data",
                &stale,
                "--fixture",
                &fixture_arg,
                "--fixture-rev",
                "main",
            ]),
            "fixture.input",
        );
        assert!(
            error["description"]
                .as_str()
                .is_some_and(|d| d.contains("is not the fixture's revision")),
            "{error}"
        );

        // Admitted from the project, and from an unrelated directory with
        // --project and a relative fixture path: the same pinned commit.
        let project_arg = project.to_string_lossy().into_owned();
        let runs = [
            (
                "from-project",
                project.clone(),
                vec![
                    "run",
                    "coding",
                    "--run-id",
                    "fix-1",
                    "--data",
                    &good,
                    "--fixture",
                    &fixture_arg,
                    "--fixture-rev",
                    "main",
                ],
            ),
            (
                "from-elsewhere",
                dir.join("elsewhere"),
                vec![
                    "--project",
                    &project_arg,
                    "run",
                    "coding",
                    "--run-id",
                    "fix-2",
                    "--data",
                    &good,
                    "--fixture",
                    "../fixture",
                    "--fixture-rev",
                    "main",
                ],
            ),
        ];
        let rt = Durare::sqlite(&database(&state)).await.expect("store");
        for (tag, cwd, args) in &runs {
            let id = args[args.iter().position(|a| *a == "--run-id").expect("id") + 1];
            let mut child = spawn(
                &dir,
                cwd,
                args,
                &UNUSED_MODEL,
                &dir.join(format!("{tag}.log")),
            );
            open_gate(&rt, id).await;
            child.kill().expect("stop the run at its gate");
            child.wait().expect("reap");
            let record = rt.run_record(id).await.expect("record").expect("run");
            let bound = &record.input["pipeline"]["fixture"];
            assert_eq!(bound["commit"], second, "{tag}: {bound}");
            assert_eq!(bound["rev"], "main", "{tag}");
            assert_eq!(
                bound["source"].as_str().map(PathBuf::from),
                Some(std::fs::canonicalize(&fixture).expect("fixture")),
                "{tag}"
            );
            let snapshot = PathBuf::from(bound["snapshot"].as_str().expect("snapshot"));
            assert!(
                snapshot.starts_with(&state),
                "{tag}: the snapshot is atomic's own"
            );
        }
        // Neither the developer's checkout nor the fixture gained a ref or a
        // worktree.
        assert_eq!(git_state(&project), checkout, "the checkout is as it was");
        assert_eq!(git_state(&fixture), source, "the fixture is as it was");
    });
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn the_coding_example_run_from_outside_the_project_pins_its_base_and_leaves_the_fixture_as_it_was()
{
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("example-outside");
        let outside = outside_repository("example-outside");
        // The example as written, kept as the project, and separately the
        // code it works on, in a repository of its own: the fixture an
        // operator prepared, with the baseline the example names and a
        // deliberate commit after it.
        let project = outside.join("project");
        copy_tree(&example(), &project);
        let fixture = outside.join("fixture");
        copy_tree(&example().join("fixtures/repo"), &fixture);
        git_in(&fixture, &["init", "-q"]);
        git_in(&fixture, &["add", "."]);
        git_in(
            &fixture,
            &[
                "commit",
                "-q",
                "-m",
                "Curated baseline with the quantity bug",
            ],
        );
        let base = git_in(&fixture, &["rev-parse", "HEAD"]);
        git_in(&fixture, &["tag", "baseline", &base]);
        let seed = std::fs::read_to_string(fixture.join("pricing.py")).expect("pricing.py");
        let incomplete = seed.replace(
            "return unit_cents",
            "return unit_cents  # Quantity handling is still incomplete.",
        );
        assert_ne!(incomplete, seed, "the seed holds the quantity bug");
        std::fs::write(fixture.join("pricing.py"), &incomplete).expect("pricing.py");
        git_in(&fixture, &["add", "pricing.py"]);
        git_in(
            &fixture,
            &["commit", "-q", "-m", "Deliberately incomplete candidate"],
        );
        let candidate = git_in(&fixture, &["rev-parse", "HEAD"]);
        git_in(&fixture, &["tag", "candidate"]);
        git_in(&fixture, &["checkout", "-q", "--detach", &base]);
        assert_ne!(candidate, base, "the fixture's tip is not its baseline");
        let source = format!("{}\n{}", git_state(&fixture), git_status(&fixture));
        // The example checks as written, from a directory outside it, which
        // is also where atomic writes the declarations its atoms import.
        let project_arg = project.to_string_lossy().into_owned();
        let checked = atomic(&dir, &outside, &["check", &project_arg], &[], "");
        assert!(checked.status.success(), "{}", text(&checked));
        let state = state_dir(&dir, &project);
        let rt = Durare::sqlite(&database(&state)).await.expect("store");
        // The example's own input, with the fixture's real baseline for the
        // placeholder, and the answers each step is scripted to give.
        let mut task =
            read_json(&example().join("fixtures/plan-task/quantity.input.json"))["task"].clone();
        task["base"] = json!(base);
        let task = task.to_string();
        let plan = read_json(&example().join("fixtures/plan-task/quantity.output.json"));
        let implemented =
            read_json(&example().join("fixtures/implement-step/quantity.output.json"));
        let review = json!({
            "verdict": "approve",
            "summary": "Two units now total 250 cents; one unit is unchanged.",
            "findings": []
        });
        // Started from directories that are neither the project nor the
        // fixture, each naming the fixture by a path of its own.
        let runs = [
            ("first", "elsewhere", "../fixture"),
            ("second", "somewhere-else/again", "../../fixture"),
        ];
        let mut pinned = Vec::new();
        for (tag, cwd, fixture_arg) in runs {
            let cwd = outside.join(cwd);
            std::fs::create_dir_all(&cwd).expect("current directory");
            let model = FakeModel::start(vec![
                Reply::Text(plan.to_string()),
                // The implementing model fixes the bug with a command in its
                // working directory, the step's worktree of the fixture.
                Reply::Tool {
                    name: "exec_command".into(),
                    arguments: json!({
                        "cmd": "sed 's/return unit_cents$/return unit_cents * quantity/' pricing.py > pricing.py.new && mv pricing.py.new pricing.py",
                        "yield_time_ms": 10000
                    }),
                },
                Reply::Text(implemented.to_string()),
                Reply::Text(review.to_string()),
            ])
            .await
            .expect("fake model");
            let env = model_env(&model.base_url());
            let id = format!("change-{tag}");
            let (log, out) = (
                dir.join(format!("{tag}.log")),
                dir.join(format!("{tag}.out")),
            );
            let mut child = spawn_logged(
                &dir,
                &cwd,
                &[
                    "--json",
                    "--project",
                    &project_arg,
                    "run",
                    "change-request",
                    "--run-id",
                    &id,
                    "--data",
                    task.as_str(),
                    "--fixture",
                    fixture_arg,
                    "--fixture-rev",
                    "baseline",
                ],
                &with(&env, &[]),
                &log,
                &out,
            );
            // Admitted from a directory that is neither the project nor the
            // fixture: the fixture found from there by a relative path, at
            // the revision it was given, pinned to its commit — which the
            // run names before anything of it runs.
            let top = std::fs::canonicalize(&fixture).expect("fixture");
            wait_for_line(
                &log,
                &out,
                &format!(
                    "run {id}: fixture {} at baseline pinned to commit {base}",
                    top.display()
                ),
                &format!("{tag}: admitted the fixture named from outside the project"),
            );
            // Finding the fixture from another directory and copying its
            // baseline into a snapshot of atomic's own leave the repository
            // as they found it.
            assert_eq!(
                format!("{}\n{}", git_state(&fixture), git_status(&fixture)),
                source,
                "{tag}: binding the fixture leaves it as it was found"
            );
            // Planned, then stopped at the first gate, which shows the plan.
            let g = await_gate(&rt, &id).await;
            assert_eq!(g.step, "approve-plan", "{tag}");
            assert_eq!(g.title, "Approve bounded plan", "{tag}");
            assert_eq!(g.show, plan, "{tag}");
            approve(&rt, &g).await;
            // Implemented in a worktree of the fixture's snapshot and
            // reviewed, then stopped at the second gate.
            let g = await_gate(&rt, &id).await;
            assert_eq!(g.step, "accept-change", "{tag}");
            assert_eq!(
                model.calls(),
                4,
                "{tag}: plan, one command, its report, review"
            );
            let commit = g.show["commit"]
                .as_str()
                .expect("the candidate commit")
                .to_owned();
            assert_eq!(g.show["review"], review, "{tag}");
            approve(&rt, &g).await;
            let status = tokio::task::spawn_blocking(move || wait_child(&mut child, "the run"))
                .await
                .expect("child");
            let shown = std::fs::read_to_string(&log).unwrap_or_default();
            let printed = std::fs::read_to_string(&out).unwrap_or_default();
            assert!(status.success(), "{tag}: {shown}\n{printed}");
            // The run finished, returned the review, and called the model no
            // more than the four times its four steps needed.
            let doc = read_json(&out);
            assert_eq!(doc["run"], id, "{tag}");
            assert_eq!(doc["status"], "succeeded", "{tag}: {doc}");
            assert_eq!(doc["output"], review, "{tag}: {doc}");
            assert_eq!(model.calls(), 4, "{tag}: nothing ran after the last gate");
            let record = rt.run_record(&id).await.expect("record").expect("run");
            let bound = &record.input["pipeline"]["fixture"];
            assert_eq!(bound["rev"], "baseline", "{tag}: {bound}");
            assert_eq!(bound["commit"], base, "{tag}: the baseline is pinned");
            assert_eq!(
                bound["source"].as_str().map(PathBuf::from),
                Some(top),
                "{tag}: the fixture named from the other directory"
            );
            let snapshot = PathBuf::from(bound["snapshot"].as_str().expect("snapshot"));
            assert!(
                snapshot.starts_with(&state),
                "{tag}: the snapshot is atomic's own"
            );
            // The candidate is atomic's commit on the pinned base, and it
            // holds the edit the model ran; the reviewer was given both.
            assert_eq!(
                git_in(&snapshot, &["rev-parse", &format!("{commit}^")]),
                base,
                "{tag}"
            );
            let fixed = git_in(&snapshot, &["show", &format!("{commit}:pricing.py")]);
            assert!(
                fixed.contains("return unit_cents * quantity"),
                "{tag}: the commit holds the model's edit: {fixed}"
            );
            let asked = model.requests()[3].to_string();
            assert!(
                asked.contains(&commit) && asked.contains(&base),
                "{tag}: the reviewer was given {commit} on {base}: {asked}"
            );
            pinned.push(bound["commit"].as_str().expect("commit").to_owned());
            // The fixture repository is exactly as it was: its refs, its
            // worktrees and its working tree, every untracked file included.
            assert_eq!(
                format!("{}\n{}", git_state(&fixture), git_status(&fixture)),
                source,
                "{tag}: the fixture repository is as it was"
            );
        }
        // Started from two unrelated directories, both runs pinned the same
        // commit of the same fixture: the revision they were given, and not
        // the repository's tip.
        assert_eq!(pinned.len(), runs.len());
        assert!(
            pinned.iter().all(|c| c == &pinned[0]),
            "{pinned:?}: the same base commit every time"
        );
        assert_eq!(pinned[0], base, "{pinned:?}");
        assert!(
            pinned.iter().all(|c| c != &candidate),
            "{pinned:?}: the pin is the named revision, not the tip"
        );
        let _ = std::fs::remove_dir_all(&outside);
    });
}

#[test]
fn a_fixture_atomic_cannot_copy_whole_or_that_is_not_its_own_repository_is_refused() {
    on_codex_runtime(async {
        let dir = scratch("fixture-refused");
        let project = project(&dir);
        write_coding(&project);
        let repo = |name: &str, extra: &[&str]| -> (PathBuf, String) {
            let at = dir.join(name);
            std::fs::create_dir_all(&at).expect("repo");
            let mut init = vec!["init", "-q"];
            init.extend_from_slice(extra);
            git_in(&at, &init);
            let commit = commit_file(&at, "price.txt", "125");
            (at, commit)
        };
        let (fixture, first) = repo("fixture", &[]);
        let second = commit_file(&fixture, "price.txt", "250");
        let refused = |fixture: &Path, base: &str, kind: &str, says: &str| {
            let data = json!({"base": base, "task": "multiply"}).to_string();
            let fixture = fixture.to_string_lossy().into_owned();
            let out = atomic(
                &dir,
                &project,
                &[
                    "--json",
                    "run",
                    "coding",
                    "--data",
                    &data,
                    "--fixture",
                    &fixture,
                    "--fixture-rev",
                    "HEAD",
                ],
                &UNUSED_MODEL,
                "",
            );
            assert!(!out.status.success(), "{fixture}: {}", text(&out));
            let error = stdout_json(&out)["error"].clone();
            assert_eq!(error["kind"], kind, "{fixture}: {error}");
            assert!(
                error["description"]
                    .as_str()
                    .is_some_and(|d| d.contains(says)),
                "{fixture}: {error}"
            );
        };
        // Not a repository, or a directory inside one. The plain directory
        // is made under the system's temporary directory, outside every
        // repository: the test's own scratch directory may sit inside the
        // checkout atomic is built from.
        let plain = std::env::temp_dir().join(format!(
            "atomic-run-cli-plain-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&plain).expect("plain");
        refused(&plain, &first, "fixture.path", "not a git repository");
        std::fs::remove_dir_all(&plain).expect("remove the plain directory");
        std::fs::create_dir_all(fixture.join("sub")).expect("sub");
        refused(
            &fixture.join("sub"),
            &second,
            "fixture.path",
            "inside the git repository",
        );
        // A repository atomic cannot read at all. git is never run on it, so
        // nothing was learned about it, and the refusal says what stopped git
        // from being asked instead of saying the repository is not there —
        // news atomic does not have, and the opposite of what the person can
        // act on. The mode goes back before anything is asserted, so a
        // failing assert leaves nothing unreadable behind.
        let blind = dir.join("blind");
        std::fs::create_dir_all(&blind).expect("blind");
        git_in(&blind, &["init", "-q"]);
        let blind_base = commit_file(&blind, "price.txt", "125");
        std::fs::set_permissions(&blind, std::os::unix::fs::PermissionsExt::from_mode(0o000))
            .expect("unreadable");
        let data = json!({"base": blind_base, "task": "multiply"}).to_string();
        let blind_arg = blind.to_string_lossy().into_owned();
        let out = atomic(
            &dir,
            &project,
            &[
                "--json",
                "run",
                "coding",
                "--data",
                &data,
                "--fixture",
                &blind_arg,
                "--fixture-rev",
                "HEAD",
            ],
            &UNUSED_MODEL,
            "",
        );
        std::fs::set_permissions(&blind, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("readable again");
        assert!(!out.status.success(), "{blind_arg}: {}", text(&out));
        let error = stdout_json(&out)["error"].clone();
        assert_eq!(error["kind"], "fixture.unreadable", "{blind_arg}: {error}");
        let description = error["description"].as_str().expect("a description");
        assert!(
            description.contains("atomic cannot tell whether it is a git repository"),
            "{description}"
        );
        assert!(
            description.contains("Permission denied"),
            "what stopped git is kept: {description}"
        );
        assert!(
            !description.contains("not a git repository"),
            "atomic never reports what it did not learn: {description}"
        );
        assert!(
            error["action"]
                .as_str()
                .is_some_and(|a| a.contains("readable")),
            "the action names what to change: {error}"
        );
        // What atomic does not copy whole.
        let (submodule, _) = repo("submodule", &[]);
        git_in(
            &submodule,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{first},vendor"),
            ],
        );
        git_in(&submodule, &["commit", "-q", "-m", "vendor"]);
        let head = git_in(&submodule, &["rev-parse", "HEAD"]);
        refused(&submodule, &head, "fixture.unsupported", "submodule");
        let (lfs, _) = repo("lfs", &[]);
        let head = commit_file(&lfs, ".gitattributes", "*.bin filter=lfs diff=lfs -text\n");
        refused(&lfs, &head, "fixture.unsupported", "Git LFS");
        git_in(&dir, &["clone", "-q", "--shared", "fixture", "borrowing"]);
        refused(
            &dir.join("borrowing"),
            &second,
            "fixture.unsupported",
            "borrows objects",
        );
        git_in(
            &dir,
            &[
                "clone",
                "-q",
                "--depth",
                "1",
                &format!("file://{}", fixture.display()),
                "shallow",
            ],
        );
        refused(
            &dir.join("shallow"),
            &second,
            "fixture.unsupported",
            "cut-short",
        );
        let (grafted, head) = repo("grafted", &[]);
        std::fs::create_dir_all(grafted.join(".git/info")).expect("info");
        std::fs::write(grafted.join(".git/info/grafts"), format!("{head}\n")).expect("grafts");
        refused(&grafted, &head, "fixture.unsupported", "cut-short");
        let (sha256, _) = repo("sha256", &["--object-format=sha256"]);
        refused(&sha256, &first, "fixture.unsupported", "SHA-1");
        // A base that is not in the fixture's history.
        git_in(&fixture, &["checkout", "-q", "-b", "elsewhere", &first]);
        let elsewhere = commit_file(&fixture, "price.txt", "999");
        git_in(&fixture, &["checkout", "-q", "main"]);
        refused(&fixture, &elsewhere, "fixture.input", "not in the history");

        // A linked worktree is bound at its own HEAD, not its repository's.
        git_in(
            &fixture,
            &["worktree", "add", "-q", "-b", "linked", "../linked", &first],
        );
        let linked = dir.join("linked");
        let own = commit_file(&linked, "price.txt", "500");
        let state = state_dir(&dir, &project);
        let rt = Durare::sqlite(&database(&state)).await.expect("store");
        let data = json!({"base": own, "task": "multiply"}).to_string();
        let linked_arg = linked.to_string_lossy().into_owned();
        let mut child = spawn(
            &dir,
            &project,
            &[
                "run",
                "coding",
                "--run-id",
                "linked-1",
                "--data",
                &data,
                "--fixture",
                &linked_arg,
                "--fixture-rev",
                "HEAD",
            ],
            &UNUSED_MODEL,
            &dir.join("linked.log"),
        );
        open_gate(&rt, "linked-1").await;
        child.kill().expect("stop the run at its gate");
        child.wait().expect("reap");
        let record = rt
            .run_record("linked-1")
            .await
            .expect("record")
            .expect("run");
        assert_eq!(record.input["pipeline"]["fixture"]["commit"], own);
    });
}

/// What was found about a project's repository is said where a worktree is
/// asked for and nowhere else. A run that changes code and names no fixture
/// is refused, and the refusal carries what git said about the project, since
/// its worktree step is asking why there is none; a run that asks nothing of
/// git about the project is not refused for what git said, so a project
/// outside every repository runs its other steps.
#[test]
fn a_project_outside_any_repository_is_refused_only_where_a_worktree_is_asked() {
    on_codex_runtime(async {
        // Where atomic keeps its own state is its usual place, since it
        // refuses a scratch directory under the temporary directory; where
        // the project is is outside every repository.
        let dir = scratch("no-repository");
        let outside = outside_repository("no-repository");
        let project = project(&outside);
        write_coding(&project);
        // The same project's work, with nothing asked of a worktree: one
        // durable step, a gate a person decides, and the run's input back.
        std::fs::write(
            project.join("workflows/ask.workflow.yaml"),
            "atomic: 1\nname: ask\n\
             input: { $ref: ../atoms/rank-note.input.schema.json }\n\
             output: { $ref: ../atoms/rank-note.input.schema.json }\n\
             steps:\n  check:\n    gate:\n      title: \"'Check the note ' + input.title\"\n\
             \x20     show: input\n      timeout: 24h\n\
             returns: input\n",
        )
        .expect("workflow");
        let checked = atomic(&dir, &project, &["--json", "check"], &[], "");
        assert!(checked.status.success(), "{}", text(&checked));
        let base = "1111111111111111111111111111111111111111";
        let data = json!({"base": base, "task": "multiply"}).to_string();

        // Refused, with git's news about the project in the refusal: no
        // repository there, which the person can act on, and not that atomic
        // could not tell — which would be the other news, and false here.
        let out = atomic(
            &dir,
            &project,
            &["--json", "run", "coding", "--data", &data],
            &UNUSED_MODEL,
            "",
        );
        assert!(!out.status.success(), "{}", text(&out));
        let error = stdout_json(&out)["error"].clone();
        assert_eq!(error["kind"], "fixture.required", "{error}");
        let description = error["description"].as_str().expect("a description");
        assert!(
            description.contains("git finds no repository in the project's directory"),
            "{description}"
        );
        assert!(
            description.contains("not a git repository"),
            "git's own words are kept: {description}"
        );
        assert!(
            !description.contains("cannot tell"),
            "nothing about this project was unreadable: {description}"
        );
        assert!(
            !String::from_utf8_lossy(&out.stderr).contains("run run-"),
            "no run started: {}",
            text(&out)
        );

        // The workflow that needs no worktree runs on the same project: the
        // gate's own mapping ran, the decision came from a human credential,
        // and the run's result is its input. Nothing was asked of git about
        // the project, which is in no repository.
        let state = state_dir(&dir, &project);
        let rt = Durare::sqlite(&database(&state)).await.expect("store");
        let log = dir.join("ask.log");
        let out_file = dir.join("ask.json");
        let said = |what: &str| {
            format!(
                "{what}\nstderr: {}\nstdout: {}",
                std::fs::read_to_string(&log).unwrap_or_default(),
                std::fs::read_to_string(&out_file).unwrap_or_default()
            )
        };
        let mut child = spawn_logged(
            &dir,
            &project,
            &[
                "--json", "run", "ask", "--run-id", "ask-1", "--data", RANK_INPUT,
            ],
            &UNUSED_MODEL,
            &log,
            &out_file,
        );
        let g = match tokio::time::timeout(Duration::from_secs(60), await_gate(&rt, "ask-1")).await
        {
            Ok(g) => g,
            Err(_) => panic!("{}", said("ask-1 never opened its gate")),
        };
        assert_eq!(
            g.title, "Check the note Export crashes on accents",
            "the gate step ran, and mapped its title from the input: {g:?}"
        );
        // A person decides, from another terminal than the run's; the run's
        // own command is stopped at its gate, as any run may be, and a second
        // command carries the decided run to its end.
        approve(&rt, &g).await;
        terminate(&child);
        wait_child(&mut child, "the run stopped at its gate");
        let out = atomic(
            &dir,
            &project,
            &["--json", "runs", "resume", "ask-1"],
            &UNUSED_MODEL,
            "",
        );
        assert!(out.status.success(), "{}", text(&out));
        let given: Value = serde_json::from_str(RANK_INPUT).expect("input");
        assert_eq!(stdout_json(&out)["output"], given, "{}", text(&out));
        assert!(
            std::fs::read_to_string(&out_file)
                .expect("stdout")
                .is_empty(),
            "the stopped command wrote no result"
        );
        let _ = std::fs::remove_dir_all(&dir);
    });
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn runs_show_says_which_steps_came_from_checkpoints_and_which_attempt_ran_them() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())])
            .await
            .expect("fake model");
        let dir = scratch("show");
        let project = project(&dir);
        let env = model_env(&model.base_url());
        let (d, p, e) = (dir.clone(), project.clone(), env.clone());
        tokio::task::spawn_blocking(move || killed_after_rank(&d, &p, &e, "shown-1"))
            .await
            .expect("child");
        let (d, p, e) = (dir.clone(), project.clone(), env.clone());
        let out = tokio::task::spawn_blocking(move || {
            atomic(&d, &p, &["runs", "resume", "shown-1"], &with(&e, &[]), "")
        })
        .await
        .expect("child");
        assert!(out.status.success(), "{}", text(&out));

        // Read from the store alone: no model, no lock.
        let out = atomic(
            &dir,
            &project,
            &["--json", "runs", "show", "shown-1"],
            &[],
            "",
        );
        assert!(out.status.success(), "{}", text(&out));
        let doc = stdout_json(&out);
        assert_eq!(doc["run"], "shown-1", "{doc}");
        assert_eq!(doc["workflow"], "triage");
        assert_eq!(doc["status"], "succeeded");
        assert_eq!(
            doc["input"],
            serde_json::from_str::<Value>(NOTE).expect("note")
        );
        assert_eq!(
            doc["output"],
            json!({"line": "P1: Export crashes on accents"})
        );
        let attempts = doc["attempts"].as_array().expect("attempts");
        assert_eq!(attempts.len(), 2, "the killed run and its resume: {doc}");
        let steps = doc["steps"].as_array().expect("steps");
        let attempt_of = |prefix: &str| -> Vec<i64> {
            steps
                .iter()
                .filter(|s| s["name"].as_str().is_some_and(|n| n.starts_with(prefix)))
                .map(|s| s["attempt"].as_i64().expect("attempt"))
                .collect()
        };
        // What finished before the kill came from checkpoints on resume.
        for prefix in ["pin", "count.", "rank."] {
            let seen = attempt_of(prefix);
            assert!(
                !seen.is_empty() && seen.iter().all(|a| *a == 1),
                "{prefix}: {seen:?}"
            );
        }
        // What the resume ran itself.
        let seen = attempt_of("summarize.");
        assert!(
            !seen.is_empty() && seen.iter().all(|a| *a == 2),
            "summarize: {seen:?}"
        );
        assert_eq!(
            attempts[0]["ran"],
            json!(attempt_of("").iter().filter(|a| **a == 1).count())
        );
        assert_eq!(attempts[1]["replayed"], attempts[0]["ran"]);
        assert!(attempts[1]["ran"].as_u64().is_some_and(|n| n > 0), "{doc}");
        assert_eq!(doc["usage"]["calls"], 2, "rank and summarize: {doc}");

        // The same for a person.
        let out = atomic(&dir, &project, &["runs", "show", "shown-1"], &[], "");
        assert!(out.status.success(), "{}", text(&out));
        let said = String::from_utf8_lossy(&out.stdout);
        assert!(said.contains("2 attempts"), "{said}");
        assert!(said.contains("from checkpoints"), "{said}");
        let summarize = said
            .lines()
            .find(|l| l.contains("summarize.publish"))
            .unwrap_or_else(|| panic!("{said}"));
        assert!(summarize.contains("attempt 2"), "{summarize}");
        let rank = said
            .lines()
            .find(|l| l.contains("rank.publish"))
            .unwrap_or_else(|| panic!("{said}"));
        assert!(rank.contains("attempt 1"), "{rank}");

        // An id the store does not know.
        let out = atomic(
            &dir,
            &project,
            &["--json", "runs", "show", "no-such-run"],
            &[],
            "",
        );
        assert!(!out.status.success());
        assert_eq!(stdout_json(&out)["error"]["kind"], "run.not_found");
    });
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_budget_in_the_projects_configuration_reaches_its_runs() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())])
            .await
            .expect("fake model");
        let dir = scratch("configured-budget");
        let project = project(&dir);
        // One call's whole bound (80,000 tokens by default) and one token
        // more, for every workflow run: rank fits, and summarize does not.
        std::fs::write(
            project.join("atomic.yaml"),
            "atomic: 1\nname: notes\nbudget:\n  workflow:\n    tokens: 80001\n",
        )
        .expect("manifest");
        let state = state_dir(&dir, &project);
        let env = model_env(&model.base_url());
        let mut run = spawn(
            &dir,
            &project,
            &["run", "triage", "--run-id", "configured-1", "--data", NOTE],
            &with(&env, &[]),
            &dir.join("run.log"),
        );
        let rt = Durare::sqlite(&database(&state)).await.expect("store");
        let started = Instant::now();
        let park = loop {
            if let Some(p) = atomic_durable::parks(&rt, "configured-1")
                .await
                .expect("parks")
                .into_iter()
                .next()
            {
                break p;
            }
            let ended = run.try_wait().expect("the run");
            assert!(
                ended.is_none() && started.elapsed() < Duration::from_secs(60),
                "the run did not park on the configured budget ({ended:?}): {} model calls; {}",
                model.calls(),
                std::fs::read_to_string(dir.join("run.log")).unwrap_or_default()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert!(park.reason.contains("run:configured-1"), "{}", park.reason);
        assert_eq!(model.calls(), 1, "summarize was not called");
        run.kill().expect("stop the run");
        run.wait().expect("reap");
    });
}

/// Run `body` on a runtime of its own, for a test that is not async.
fn block_on<T>(body: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(body)
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_run_whose_execution_cannot_be_recorded_still_runs() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    let dir = scratch("unrecorded");
    let project = project(&dir);
    let state = state_dir(&dir, &project);
    let out = atomic(
        &dir,
        &project,
        &["run", "count-only", "--data", NOTE],
        &UNUSED_MODEL,
        "",
    );
    assert!(out.status.success(), "{}", text(&out));
    // The record of executions is gone: only `runs show` needs it.
    block_on(async {
        use sqlx::Connection;
        let url = format!("sqlite://{}", database(&state).display());
        let mut db = sqlx::SqliteConnection::connect(&url).await.expect("store");
        sqlx::query("DROP TABLE atomic_executions")
            .execute(&mut db)
            .await
            .expect("drop");
        db.close().await.expect("close");
    });
    let out = atomic(
        &dir,
        &project,
        &["run", "count-only", "--data", NOTE],
        &UNUSED_MODEL,
        "",
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("could not be recorded"),
        "{}",
        text(&out)
    );
    assert_eq!(count_log(&dir), 2);
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn runs_show_answers_while_a_run_holds_the_lock_and_a_writer_the_store() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    let dir = scratch("show-locked");
    let project = project(&dir);
    write_slow(&project, &dir);
    let state = state_dir(&dir, &project);
    let mut run = spawn(
        &dir,
        &project,
        &["run", "slow", "--run-id", "held-1", "--data", NOTE],
        &[],
        &dir.join("run.log"),
    );
    wait_until("the command to start", || slow_starts(&dir) == 1);
    assert!(lock_held(&state), "the run holds the runtime lock");
    let out = block_on(async {
        use sqlx::Connection;
        let url = format!("sqlite://{}", database(&state).display());
        let mut db = sqlx::SqliteConnection::connect(&url).await.expect("store");
        // Another process in the middle of a write.
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut db)
            .await
            .expect("write lock");
        let started = Instant::now();
        let out = atomic(
            &dir,
            &project,
            &["--json", "runs", "show", "held-1"],
            &[],
            "",
        );
        let took = started.elapsed();
        sqlx::query("ROLLBACK")
            .execute(&mut db)
            .await
            .expect("rollback");
        db.close().await.expect("close");
        (out, took)
    });
    let (out, took) = out;
    run.kill().expect("SIGKILL atomic run");
    run.wait().expect("reap");
    let pids = slow_pids(&dir);
    if !pids.is_empty() {
        let _ = Command::new("/bin/sh")
            .args(["-c", &format!("kill -KILL {} 2>/dev/null", pids.join(" "))])
            .status();
    }
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        took < Duration::from_secs(3),
        "runs show waited for the writer: {took:?}"
    );
    let doc = stdout_json(&out);
    assert_eq!(doc["status"], "unfinished", "{doc}");
    assert_eq!(doc["attempts"].as_array().map(Vec::len), Some(1), "{doc}");
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
fn the_executor_identity_is_atomics_own_and_required_for_a_shared_store() {
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
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[cfg(feature = "postgres-tests")]
#[test]
fn a_restarted_server_recovers_only_its_own_runs_on_postgres() {
    on_codex_runtime(async move {
        // Declared before the store and the processes on it, so it is
        // dropped after them.
        let guard = atomic_durable::testing::schema("cli").await;
        let (url, schema) = (guard.url().to_owned(), guard.name().to_owned());
        #[cfg(target_os = "linux")]
        support::require_sandbox();
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
        let mut a = Reaped(spawn(
            &dir,
            &project,
            &["run", "gated", "--run-id", "run-a", "--data", NOTE],
            &with(&model_env, &env_a),
            &dir.join("a.log"),
        ));
        let rt = Durare::postgres(&url, &schema).await.expect("store");
        let gate_a = open_gate(&rt, "run-a").await;
        let mut b = Reaped(spawn(
            &dir,
            &project,
            &["run", "gated", "--run-id", "run-b", "--data", NOTE],
            &with(&model_env, &env_b),
            &dir.join("b.log"),
        ));
        let gate_b = open_gate(&rt, "run-b").await;
        a.kill().expect("SIGKILL a");
        a.wait().expect("reap a");

        let mut serve = Reaped(spawn(
            &dir,
            &project,
            &["serve"],
            &with(&model_env, &env_a),
            &dir.join("serve.log"),
        ));
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

/// A spawned `atomic` process that is killed and reaped when this is
/// dropped, if it has not ended by then: a test that fails part-way leaves
/// nothing running on its schema once the schema's guard drops it.
#[cfg(feature = "postgres-tests")]
struct Reaped(Child);

#[cfg(feature = "postgres-tests")]
impl std::ops::Deref for Reaped {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}

#[cfg(feature = "postgres-tests")]
impl std::ops::DerefMut for Reaped {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

#[cfg(feature = "postgres-tests")]
impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
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
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn resuming_a_run_another_executor_owns_on_postgres_needs_its_owner_stopped() {
    on_codex_runtime(async move {
        // Declared before the store and the processes on it, so it is
        // dropped after them.
        let guard = atomic_durable::testing::schema("owned").await;
        let (url, schema) = (guard.url().to_owned(), guard.name().to_owned());
        #[cfg(target_os = "linux")]
        support::require_sandbox();
        let model = FakeModel::start(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())])
            .await
            .expect("fake model");
        let dir = scratch("owned");
        let project = project(&dir);
        let mut a_env = model_env(&model.base_url()).to_vec();
        a_env.extend(executor_env(&dir, &url, &schema, "worker-a"));
        let mut b_env = model_env(&model.base_url()).to_vec();
        b_env.extend(executor_env(&dir, &url, &schema, "worker-b"));
        let mut a = Reaped(spawn(
            &dir,
            &project,
            &["run", "gated", "--run-id", "owned-1", "--data", NOTE],
            &with(&a_env, &[]),
            &dir.join("a.log"),
        ));
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
        // Killed after its last step, so the run still has its caches.
        let mut b = Reaped(spawn(
            &dir,
            &project,
            &["runs", "resume", "owned-1", "--owner-stopped"],
            &with(&b_env, &[("ATOMIC_FAILPOINT", "pipeline:after:summarize")]),
            &dir.join("b.log"),
        ));
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
            !status.success(),
            "killed at the failpoint: {}",
            std::fs::read_to_string(dir.join("b.log")).unwrap_or_default()
        );
        assert_eq!(model.calls(), 2, "rank once, summarize once");
        // Count ran once, in worker-a's caches: worker-b, whose caches are
        // in its own home, ran it nowhere.
        ran_once_live(&dir, 1);
        let out = resume(&[]).await.expect("child");
        assert!(out.status.success(), "{}", text(&out));
        assert_eq!(model.calls(), 2, "nothing ran again");
        assert_eq!(counted_in(&rt).await, 1, "count once");
    });
}

/// A run resumed by one executor goes on that executor's own queue: an
/// unrelated `atomic run` on another executor never takes it.
#[cfg(feature = "postgres-tests")]
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_resumed_run_is_taken_only_by_its_resumer_on_postgres() {
    on_codex_runtime(async move {
        // Declared before the store and the processes on it, so it is
        // dropped after them.
        let guard = atomic_durable::testing::schema("queue").await;
        let (url, schema) = (guard.url().to_owned(), guard.name().to_owned());
        #[cfg(target_os = "linux")]
        support::require_sandbox();
        let model = FakeModel::start(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())])
            .await
            .expect("fake model");
        let dir = scratch("private-queue");
        let project = project(&dir);
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

        // worker-b again, which owns it now, takes it from its queue, and is
        // killed after its last step, so the run still has its caches:
        // count ran once, in worker-a's, and nowhere since. Then worker-b
        // finishes it.
        let mut killed = b.clone();
        killed.push(("ATOMIC_FAILPOINT", "pipeline:after:summarize".into()));
        let out = call(killed, &["runs", "resume", "queued-1"])
            .await
            .expect("child");
        assert!(!out.status.success(), "killed: {}", text(&out));
        assert_eq!(model.calls(), 2, "rank once, summarize once");
        ran_once_live(&dir, 1);
        let out = call(b, &["--json", "runs", "resume", "queued-1"])
            .await
            .expect("child");
        assert!(out.status.success(), "{}", text(&out));
        assert_eq!(
            stdout_json(&out)["output"],
            json!({"line": "P1: Export crashes on accents"})
        );
        assert_eq!(model.calls(), 2, "summarize ran once: {}", text(&out));
        assert_eq!(counted_in(&rt).await, 2);
    });
}

/// `atomic gates` on PostgreSQL: a second terminal enrolls, lists the gate
/// a live run waits at, and approves it, all in the runs' own store.
#[cfg(all(feature = "postgres-tests", target_os = "linux"))]
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn gates_are_listed_and_decided_in_the_postgres_store() {
    on_codex_runtime(async move {
        // Declared before the store and the processes on it, so it is
        // dropped after them.
        let guard = atomic_durable::testing::schema("gates_cli").await;
        let (url, schema) = (guard.url().to_owned(), guard.name().to_owned());
        #[cfg(target_os = "linux")]
        support::require_sandbox();
        let model = FakeModel::start(vec![Reply::Text(RANK.into()), Reply::Text(SUMMARY.into())])
            .await
            .expect("fake model");
        let dir = scratch("pg-gates");
        let project = project(&dir);
        let mut env = model_env(&model.base_url()).to_vec();
        env.extend(executor_env(&dir, &url, &schema, "worker-a"));
        env.push(("ATOMIC_CREDENTIAL_STORE", "file".into()));
        let mut run = Reaped(spawn(
            &dir,
            &project,
            &["run", "gated", "--run-id", "pg-gate-1", "--data", NOTE],
            &with(&env, &[]),
            &dir.join("run.log"),
        ));
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

/// A panic in atomic's own workflow code (made at a step's failpoint, for
/// the test) is named as the cause, with its message, by `atomic run`,
/// `atomic runs resume` and `atomic serve`, and kept in the project's
/// private state; the resume hint says the panic will likely happen again.
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_run_whose_workflow_code_panics_is_reported_as_a_panic() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    on_codex_runtime(a_run_whose_workflow_code_panics());
}

async fn a_run_whose_workflow_code_panics() {
    let dir = scratch("panicked");
    let project = project(&dir);
    // An exec atom only: the endpoint is never called, but resuming and
    // serving need one configured.
    let panic = [
        ("ATOMIC_FAILPOINT_PANIC", "pipeline:after:count"),
        ("ATOMIC_MODEL_BASE_URL", "http://127.0.0.1:9/v1"),
        ("ATOMIC_MODEL", "synthetic"),
    ];
    let said = |out: &Output| {
        let t = text(out);
        assert!(!out.status.success(), "{t}");
        assert!(t.contains("atomic's own workflow code panicked"), "{t}");
        assert!(
            t.contains("failpoint pipeline:after:count"),
            "the panic's message: {t}"
        );
        assert!(t.contains("will likely happen again"), "{t}");
        assert!(!t.contains("more times than the engine allows"), "{t}");
        assert!(t.contains("so the run is parked, not retried"), "{t}");
        t
    };
    let out = atomic(
        &dir,
        &project,
        &["run", "count-only", "--run-id", "panicky", "--data", NOTE],
        &panic,
        "",
    );
    said(&out);
    // Parked by the engine before `atomic run` stopped, not left pending for
    // a server to run again.
    let state = state_dir(&dir, &project);
    {
        let rt = Durare::sqlite(&database(&state)).await.expect("store");
        assert_eq!(
            rt.status("panicky").await.expect("status"),
            Some(RunStatus::Other("MAX_RECOVERY_ATTEMPTS_EXCEEDED".into()))
        );
    }
    assert_eq!(count_log(&dir), 1, "the count ran once");
    // Kept where a later process, or the run's inspection, finds it.
    let private = state
        .parent()
        .and_then(Path::parent)
        .expect("the atomic home")
        .join("private")
        .join(state.file_name().expect("the project's id"));
    let kept = atomic_app::panics::kept(&private.join("panics"), "panicky").expect("kept");
    assert!(kept.contains("failpoint pipeline:after:count"), "{kept}");

    // Resumed as it is, it panics again, and says so before and after.
    let out = atomic(&dir, &project, &["runs", "resume", "panicky"], &panic, "");
    let t = said(&out);
    assert!(
        t.contains("was parked because atomic's own workflow code panicked"),
        "{t}"
    );

    // A server started later names it, from what was kept.
    let log = dir.join("serve.log");
    let mut serve = spawn(&dir, &project, &["serve"], &panic, &log);
    wait_until("serve to name the panicked run", || {
        std::fs::read_to_string(&log).is_ok_and(|l| {
            l.contains("serving until stopped")
                && l.contains("run panicky stopped: atomic's own workflow code panicked")
        })
    });
    terminate(&serve);
    let _ = wait_child(&mut serve, "serve");
    // The server recovered nothing: the run was parked, not left pending.
    let served = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        served.contains("recovered 0 unfinished run(s)"),
        "serve recovered the panicked run: {served}"
    );

    // One that panics while serving: killed where it would panic, then
    // recovered by a server, which says so as it happens.
    let killed = [
        ("ATOMIC_FAILPOINT", "pipeline:after:count"),
        ("ATOMIC_MODEL_BASE_URL", "http://127.0.0.1:9/v1"),
        ("ATOMIC_MODEL", "synthetic"),
    ];
    let out = atomic(
        &dir,
        &project,
        &[
            "run",
            "count-only",
            "--run-id",
            "panics-later",
            "--data",
            NOTE,
        ],
        &killed,
        "",
    );
    assert!(!out.status.success(), "{}", text(&out));
    let log = dir.join("serve-2.log");
    let mut serve = spawn(&dir, &project, &["serve"], &panic, &log);
    wait_until(
        "serve to name the run that panicked while it served",
        || {
            std::fs::read_to_string(&log).is_ok_and(|l| {
                l.contains(
                    "atomic serve: run panics-later stopped: atomic's own workflow code panicked",
                ) && l.contains("failpoint pipeline:after:count")
            })
        },
    );
    terminate(&serve);
    let _ = wait_child(&mut serve, "serve");
}

/// A panic noted by an earlier process is not the cause of a later run's
/// own failure under the same id: it fails as any run does.
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_note_kept_from_before_is_not_a_later_failures_cause() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    let dir = scratch("stale-note");
    let project = project(&dir);
    let state = state_dir(&dir, &project);
    let panics = state
        .parent()
        .and_then(Path::parent)
        .expect("the atomic home")
        .join("private")
        .join(state.file_name().expect("the project's id"))
        .join("panics");
    std::fs::create_dir_all(&panics).expect("panics");
    let name = {
        use sha2::Digest;
        format!("{:x}.json", sha2::Sha256::digest(b"reused"))
    };
    std::fs::write(
        panics.join(name),
        json!({"run": "reused", "message": "a panic long ago", "at": 1}).to_string(),
    )
    .expect("an old note");
    // A command that fails: an ordinary failure of the run.
    std::fs::write(project.join("tools/count.py"), "import sys\nsys.exit(3)\n")
        .expect("a failing command");
    let out = atomic(
        &dir,
        &project,
        &["run", "count-only", "--run-id", "reused", "--data", NOTE],
        &[],
        "",
    );
    let t = text(&out);
    assert!(!out.status.success(), "{t}");
    assert!(
        !t.contains("panicked"),
        "an old note was taken for the cause: {t}"
    );
    assert!(t.contains("run reused failed"), "{t}");
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn the_debug_prompt_says_its_turn_is_not_metered() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text("Hello.".into())])
            .await
            .expect("fake model");
        let dir = scratch("debug-unmetered");
        let project = project(&dir);
        let url = model.base_url();
        let out = tokio::task::spawn_blocking(move || {
            atomic(
                &dir,
                &project,
                &[
                    "debug",
                    "codex-prompt",
                    "--base-url",
                    &url,
                    "--model",
                    "synthetic",
                    "Hi.",
                ],
                &[],
                "",
            )
        })
        .await
        .expect("atomic");
        assert!(out.status.success(), "{out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "Hello.");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("this turn is not metered"),
            "the unmetered turn was silent: {stderr}"
        );
        assert_eq!(model.calls(), 1);
    });
}

/// A copy of `rank-note` named `name`, declaring `trust` if one is given.
fn ranking_atom(project: &Path, name: &str, trust: Option<&str>) {
    let config = match trust {
        Some(trust) => format!("description: '{name}', trust: '{trust}'"),
        None => format!("description: '{name}'"),
    };
    write_atom(
        project,
        name,
        &config,
        "Rank the note from 1 (urgent) to 3.\nTitle: {input.title}\nWords: {input.words}",
        object(json!({"title": {"type": "string"}, "words": {"type": "integer"}})),
        object(json!({"priority": {"type": "integer", "minimum": 1, "maximum": 3}})),
    );
}

const RANK_INPUT: &str = r#"{"title": "Export crashes on accents", "words": 7}"#;

#[test]
fn a_direct_run_refuses_input_its_atom_does_not_trust_before_anything_runs() {
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(RANK.into())])
            .await
            .expect("fake model");
        let dir = scratch("untrusted-input");
        let project = project(&dir);
        ranking_atom(&project, "rank-for-team", Some("team"));
        ranking_atom(&project, "rank-for-agents", Some("agent"));
        let env = model_env(&model.base_url());
        for (atom, sources) in [
            (
                "rank-for-team",
                "a gate a person decides, or a literal in the workflow file",
            ),
            (
                "rank-for-agents",
                "a model step's output, a gate a person decides, or a literal in the workflow \
                 file",
            ),
        ] {
            let (dir, project, env) = (dir.clone(), project.clone(), env.clone());
            let out = tokio::task::spawn_blocking(move || {
                atomic(
                    &dir,
                    &project,
                    &["--json", "run", atom, "--data", RANK_INPUT],
                    &with(&env, &[]),
                    "",
                )
            })
            .await
            .expect("atomic");
            assert!(!out.status.success(), "{atom}: {}", text(&out));
            let error = &stdout_json(&out)["error"];
            assert_eq!(error["kind"], "input.untrusted", "{atom}: {}", text(&out));
            let message = error["description"].as_str().unwrap_or_default();
            assert!(message.contains("external data"), "{atom}: {message}");
            assert!(
                message.contains(&format!("atom `{atom}`")),
                "{atom}: {message}"
            );
            // The action names what the atom takes instead.
            let action = error["action"].as_str().unwrap_or_default();
            assert!(
                action.contains(&format!(
                    "run it from a workflow that binds the field to {sources}"
                )) && action.contains("declare a lower `trust` on the atom"),
                "{atom}: the action names what can take the field's place"
            );
            // Refused as the run was loaded: no durable run was started,
            // which would name itself first on stderr.
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                !stderr.contains("run run-"),
                "{atom}: a run started: {stderr}"
            );
        }
        assert_eq!(model.calls(), 0, "a model was called");
    });
}

// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_direct_run_takes_external_input_into_an_atom_that_accepts_it() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(RANK.into()), Reply::Text(RANK.into())])
            .await
            .expect("fake model");
        let dir = scratch("external-input");
        let project = project(&dir);
        ranking_atom(&project, "rank-anything", Some("external"));
        let env = model_env(&model.base_url());
        // No trust declared, and external declared, both take the run's input.
        for atom in ["rank-note", "rank-anything"] {
            let (dir, project, env) = (dir.clone(), project.clone(), env.clone());
            let out = tokio::task::spawn_blocking(move || {
                atomic(
                    &dir,
                    &project,
                    &["run", atom, "--data", RANK_INPUT],
                    &with(&env, &[]),
                    "",
                )
            })
            .await
            .expect("atomic");
            assert!(out.status.success(), "{atom}: {}", text(&out));
            assert_eq!(stdout_json(&out), json!({"priority": 1}), "{atom}");
        }
        assert_eq!(model.calls(), 2);
    });
}

/// What a run's input and output carry, as a model's or a command's text
/// can, reaches no terminal raw: not in the run's result, plain or
/// `--json`, nor in `runs show`, plain or `--json`, nor in an error; each
/// `--json` form still reads back as the same text.
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn a_runs_hostile_text_reaches_no_terminal_raw() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    let raw = |out: &std::process::Output| {
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    };
    let hostile = ['\u{1b}', '\u{7f}', '\u{9b}', '\u{202e}', '\u{E0041}'];
    let reached = |text: &str| {
        hostile
            .iter()
            .copied()
            .filter(|c| text.contains(*c))
            .collect::<Vec<_>>()
    };
    let dir = scratch("hostile-text");
    let project = project(&dir);
    let note = "Ship\u{1b}[2J\u{7f}\u{9b}31m\u{202e}gpj.exe\u{E0041} now";
    let data = json!({ "text": note }).to_string();
    // The result, plain and as JSON.
    let plain = atomic(
        &dir,
        &project,
        &["run", "count-note", "--data", &data],
        &[],
        "",
    );
    assert!(plain.status.success(), "{}", text(&plain));
    assert!(
        reached(&raw(&plain)).is_empty(),
        "the result: {:?}",
        reached(&raw(&plain))
    );
    let out = atomic(
        &dir,
        &project,
        &["--json", "run", "count-note", "--data", &data],
        &[],
        "",
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        reached(&raw(&out)).is_empty(),
        "the JSON result: {:?}",
        reached(&raw(&out))
    );
    let doc = stdout_json(&out);
    let id = doc["run"].as_str().expect("run id").to_owned();
    assert!(
        doc["output"]["title"]
            .as_str()
            .is_some_and(|t| t.contains('\u{1b}')),
        "the output read back whole: {doc}"
    );
    // runs show, plain and as JSON.
    let shown = atomic(&dir, &project, &["runs", "show", &id], &[], "");
    assert!(shown.status.success(), "{}", text(&shown));
    assert!(
        reached(&raw(&shown)).is_empty(),
        "runs show: {:?}",
        reached(&raw(&shown))
    );
    let shown = atomic(&dir, &project, &["--json", "runs", "show", &id], &[], "");
    assert!(shown.status.success(), "{}", text(&shown));
    assert!(
        reached(&raw(&shown)).is_empty(),
        "runs show --json: {:?}",
        reached(&raw(&shown))
    );
    let doc = stdout_json(&shown);
    assert_eq!(doc["input"]["text"], note, "the input read back whole");
    // An error that quotes it, plain and as JSON.
    let hostile_id = format!("run-{note}");
    for json in [false, true] {
        let mut args = vec!["runs", "show", hostile_id.as_str()];
        if json {
            args.insert(0, "--json");
        }
        let failed = atomic(&dir, &project, &args, &[], "");
        assert!(!failed.status.success(), "{}", text(&failed));
        assert!(
            reached(&raw(&failed)).is_empty(),
            "the error (json {json}): {:?}",
            reached(&raw(&failed))
        );
    }
}

/// A model's answer to `debug codex-prompt` reaches no terminal raw.
// needs_sandbox
#[cfg(any(not(target_os = "linux"), feature = "sandbox-tests"))]
#[test]
fn the_debug_prompt_escapes_its_answer() {
    #[cfg(target_os = "linux")]
    support::require_sandbox();
    on_codex_runtime(async {
        let model = FakeModel::start(vec![Reply::Text(
            "Hello\u{1b}[2J\u{9b}31m\u{202e}\u{E0041}.\nSecond line.".into(),
        )])
        .await
        .expect("fake model");
        let dir = scratch("debug-hostile");
        let project = project(&dir);
        let url = model.base_url();
        let out = tokio::task::spawn_blocking(move || {
            atomic(
                &dir,
                &project,
                &[
                    "debug",
                    "codex-prompt",
                    "--base-url",
                    &url,
                    "--model",
                    "synthetic",
                    "Hi.",
                ],
                &[],
                "",
            )
        })
        .await
        .expect("atomic");
        assert!(out.status.success(), "{out:?}");
        let said = String::from_utf8_lossy(&out.stdout);
        for c in ['\u{1b}', '\u{9b}', '\u{202e}', '\u{E0041}'] {
            assert!(
                !said.contains(c),
                "U+{:04X} reached the terminal",
                u32::from(c)
            );
        }
        assert!(said.contains("Second line."), "{said}");
    });
}
