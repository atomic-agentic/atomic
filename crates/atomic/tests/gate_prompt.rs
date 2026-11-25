// SPDX-License-Identifier: Apache-2.0

//! Gates at the terminal: `atomic run` prompts for each gate its run opens,
//! and question gates.
//!
//! - an approval gate reached in `atomic run` is approved at the prompt, as
//!   the enrolled operator, and the workflow continues;
//! - with no terminal, the run says how to decide from another terminal and
//!   waits, and `atomic gates approve` there lets it go on;
//! - a question gate answered at the prompt resumes its step with the typed
//!   answer, and writes the feedback entry when it asks to remember; an
//!   answer outside the enum is refused, there and by `atomic gates answer`;
//! - a question nobody answers in time resumes with its declared default,
//!   which the store records and the feedback log remembers;
//! - replies typed ahead, or before anything is shown, decide no gate, and
//!   an exec command cannot type into the prompt (it has no terminal);
//! - a gate store that stops answering while a gate is shown is told about
//!   once, not on every pass of the read that looks for a decision made
//!   elsewhere;
//! - a hostile title reaches the terminal escaped, in `atomic gates list`
//!   and at the prompt.
//!
//! The project's atoms are exec atoms, so no model is called. The prompt
//! runs on a pseudo-terminal a small driver opens; the credential is in the
//! opt-in file store, since the test machine has no OS keyring. Linux only,
//! where credentials are kept, and, since exec atoms' commands run under
//! Codex's sandbox, with the `sandbox-tests` feature; with the feature on, a
//! sandbox that cannot start fails the tests.

#![cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#![allow(clippy::unwrap_used)]

mod support;

use atomic_core::testing::on_codex_runtime;
use atomic_durable::Durare;
use atomic_durable::gate::{self, Principal};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const ATOMIC: &str = env!("CARGO_BIN_EXE_atomic");

/// Run a command on a pseudo-terminal, its controlling terminal, and type
/// `answers` (a JSON list) into it, one each time it shows a prompt; with
/// `DRIVER_EARLY`, type that line first, before anything is shown. Print
/// what it wrote and exit as it exited.
///
/// When it stops: a test says what it waits for with `DRIVER_STOP_AFTER`
/// (that many prompts answered) or `DRIVER_STOP_MARKER` (that written), and
/// the driver stops once it has happened and the terminal has written
/// nothing for `DRIVER_QUIET` seconds (one by default, so a stop is never
/// taken at the instant its condition is met), so the run's answer to it has
/// been written and read too. A test that waits for a run to *stay* waiting
/// is then bounded by what the run does, not by a deadline a busy machine
/// could miss. With neither, it stops when the command ends.
/// `DRIVER_SEEN_MARKER` and `DRIVER_SEEN_FILE`, named together, do something
/// else: when that marker has been written, the driver creates that file, so
/// that a test which has to act while the run sits waiting can see when to.
/// `DRIVER_DEADLINE` seconds (90 by default) is the backstop that catches a
/// run which never gets that far. A driver that ends there has not seen what
/// its test waited for, and [`Project::at_terminal_with`] fails the test.
const DRIVER: &str = r#"
import json, os, pty, select, subprocess, sys, time
answers = json.loads(sys.argv[1])
# Before anything is started: a half-set handshake would leave a test
# waiting for a file no one is going to write.
if bool(os.environ.get("DRIVER_SEEN_MARKER")) != bool(os.environ.get("DRIVER_SEEN_FILE")):
    sys.exit("DRIVER_SEEN_MARKER and DRIVER_SEEN_FILE are set together or not at all")
master, slave = pty.openpty()
def own_terminal():
    os.setsid()
    import fcntl, termios
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)
# With DRIVER_STDERR, standard error goes to that file, not the terminal.
err = open(os.environ["DRIVER_STDERR"], "w") if os.environ.get("DRIVER_STDERR") else slave
# With DRIVER_BACKGROUND, the command runs in a background process group of
# the terminal's session, as `command &` from a shell would.
BACKGROUND = """
import os, sys, fcntl, termios
os.setsid()
fcntl.ioctl(0, termios.TIOCSCTTY, 0)
pid = os.fork()
if pid == 0:
    os.setpgid(0, 0)
    os.execv(sys.argv[1], sys.argv[1:])
_, status = os.waitpid(pid, 0)
sys.exit(os.waitstatus_to_exitcode(status))
"""
if os.environ.get("DRIVER_BACKGROUND"):
    p = subprocess.Popen([sys.executable, "-c", BACKGROUND] + sys.argv[2:], stdin=slave,
                         stdout=slave, stderr=err)
else:
    p = subprocess.Popen(sys.argv[2:], stdin=slave, stdout=slave, stderr=err,
                         preexec_fn=own_terminal)
os.close(slave)
if os.environ.get("DRIVER_EARLY"):
    os.write(master, os.environ["DRIVER_EARLY"].encode() + b"\n")
seen, out = 0, b""
# A run that never prompts fails rather than hangs.
deadline = time.time() + float(os.environ.get("DRIVER_DEADLINE", "90"))
# What the test waits for, once the run is up: this many prompts answered,
# or this written. 0 / empty waits for the command to end instead.
stop_after = int(os.environ.get("DRIVER_STOP_AFTER", "0"))
stop_marker = os.environ.get("DRIVER_STOP_MARKER", "").encode()
# What the test waits to have happened, for a test that acts from outside
# while the run waits rather than one that stops: when this has been written,
# create that file, once. It does not say when to stop.
seen_marker = os.environ.get("DRIVER_SEEN_MARKER", "").encode()
seen_file = os.environ.get("DRIVER_SEEN_FILE", "")
# Stop once that has happened and the terminal has written nothing for this
# many seconds: whatever the run was going to write in answer has been
# written, and read, so a test that waits for the run to still be waiting
# waits for the thing, not for the clock. One second by default, so a stop is
# never taken at the instant its condition is met; a test waiting for
# something slower says so.
quiet = float(os.environ.get("DRIVER_QUIET", "1"))
last = time.time()
told = False
markers = (b"comment if you like > ", b"(/reject to refuse) > ")
# Each prompt counts once, by its count of markers so far.
while True:
    r, _, _ = select.select([master], [], [], 0.2)
    if r:
        try:
            data = os.read(master, 4096)
        except OSError:
            data = b""
        if not data:
            break
        out += data
        last = time.time()
        if seen_marker and not told and seen_marker in out:
            open(seen_file, "w").close()
            told = True
        prompts = sum(out.count(m) for m in markers)
        while seen < prompts and seen < len(answers):
            os.write(master, answers[seen].encode() + b"\n")
            seen += 1
    elif p.poll() is not None:
        break
    seen_enough = bool(stop_after) and seen >= stop_after
    if stop_marker and stop_marker in out:
        seen_enough = True
    if seen_enough and time.time() - last >= quiet:
        p.kill()
        out += b"\n[driver: stopped]\n"
        break
    if time.time() > deadline:
        p.kill()
        out += b"\n[driver: deadline]\n"
        break
sys.stdout.buffer.write(out)
sys.exit(p.wait())
"#;

struct Project {
    dir: PathBuf,
    root: PathBuf,
}

impl Project {
    fn new(tag: &str) -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("atomic-gate-prompt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("project");
        for sub in ["atoms", "workflows", "tools"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        std::fs::create_dir_all(dir.join("home")).unwrap();
        std::fs::write(dir.join("driver.py"), DRIVER).unwrap();
        std::fs::write(root.join("atomic.yaml"), "atomic: 1\nname: gated\n").unwrap();
        std::fs::write(
            root.join("tools/echo.py"),
            "import json, sys\nnote = json.load(sys.stdin)\n\
             json.dump({'line': 'got ' + note['text']}, sys.stdout)\n",
        )
        .unwrap();
        let object = |p: Value| {
            let required: Vec<&String> = p.as_object().unwrap().keys().collect();
            json!({"type": "object", "additionalProperties": false, "required": required, "properties": p})
        };
        std::fs::write(
            root.join("atoms/echo-note.atom.astro"),
            "---\nimport type { Props, Input, Output } from '../.atomic/types/echo-note';\n\
             import type { AtomConfig } from '../.atomic/types/authoring';\n\
             const { input }: Props = Astro.props;\n\
             export const config: AtomConfig<Input, Output> = { description: 'Echo.', \
             kind: 'exec', exec: { command: ['python3', 'tools/echo.py'] } };\n---\n\n",
        )
        .unwrap();
        for (role, schema) in [
            ("input", object(json!({"text": {"type": "string"}}))),
            ("output", object(json!({"line": {"type": "string"}}))),
        ] {
            std::fs::write(
                root.join(format!("atoms/echo-note.{role}.schema.json")),
                schema.to_string(),
            )
            .unwrap();
        }
        let workflow = |name: &str, gate: &str, then: &str| {
            format!(
                "atomic: 1\nname: {name}\n\
                 input: {{ $ref: ../atoms/echo-note.input.schema.json }}\n\
                 output: {{ $ref: ../atoms/echo-note.output.schema.json }}\n\
                 steps:\n  first:\n    atom: echo-note\n    with: {{ text: input.text }}\n\
                 \x20 pick:\n    after: [first]\n    gate:\n{gate}\
                 \x20 last:\n    after: [pick]\n    atom: echo-note\n    with: {{ text: {then} }}\n\
                 returns: steps.last.output\n"
            )
        };
        // Two gates in a row, the second the one that matters.
        std::fs::write(
            root.join("workflows/twice.workflow.yaml"),
            "atomic: 1\nname: twice\n\
             input: { $ref: ../atoms/echo-note.input.schema.json }\n\
             output: { $ref: ../atoms/echo-note.output.schema.json }\n\
             steps:\n  first:\n    atom: echo-note\n    with: { text: input.text }\n\
             \x20 look:\n    after: [first]\n    gate:\n      title: \"'Look at it'\"\n\
             \x20 deploy:\n    after: [look]\n    gate:\n      title: \"'DEPLOY TO PROD'\"\n\
             returns: steps.first.output\n",
        )
        .unwrap();
        // A child that leaves its command's process group still must not
        // be able to open atomic's controlling terminal.
        std::fs::write(
            root.join("tools/inject.py"),
            "import json, os, sys\n\
             note = json.load(sys.stdin)\n\
             child = os.fork()\n\
             if child == 0:\n\
             \x20   os.setpgid(0, 0)\n\
             \x20   try:\n\
             \x20       tty = os.open('/dev/tty', os.O_RDWR)\n\
             \x20   except OSError as e:\n\
             \x20       result = f'no tty: {e}\\n'\n\
             \x20   else:\n\
             \x20       os.close(tty)\n\
             \x20       result = 'tty available\\n'\n\
             \x20   with open(os.path.join(os.environ['ATOMIC_CACHE_DIR'], note['text']), 'w') as log:\n\
             \x20       log.write(result)\n\
             \x20   os._exit(0)\n\
             if os.waitpid(child, 0)[1] != 0:\n\
             \x20   sys.exit('terminal probe did not finish')\n\
             json.dump({'line': 'ok'}, sys.stdout)\n",
        )
        .unwrap();
        std::fs::write(
            root.join("atoms/inject-note.atom.astro"),
            "---\nimport type { Props, Input, Output } from '../.atomic/types/inject-note';\n\
             import type { AtomConfig } from '../.atomic/types/authoring';\n\
             const { input }: Props = Astro.props;\n\
             export const config: AtomConfig<Input, Output> = { description: 'Inject.', \
             kind: 'exec', exec: { command: ['python3', 'tools/inject.py'] } };\n---\n\n",
        )
        .unwrap();
        for role in ["input", "output"] {
            std::fs::copy(
                root.join(format!("atoms/echo-note.{role}.schema.json")),
                root.join(format!("atoms/inject-note.{role}.schema.json")),
            )
            .unwrap();
        }
        std::fs::write(
            root.join("workflows/injected.workflow.yaml"),
            "atomic: 1\nname: injected\n\
             input: { $ref: ../atoms/echo-note.input.schema.json }\n\
             output: { $ref: ../atoms/echo-note.output.schema.json }\n\
             steps:\n  first:\n    atom: inject-note\n    with: { text: input.text }\n\
             \x20 deploy:\n    after: [first]\n    gate:\n      title: \"'DEPLOY TO PROD'\"\n\
             returns: steps.first.output\n",
        )
        .unwrap();
        for (name, gate, then) in [
            (
                "approval",
                "      title: \"'Ship ' + steps.first.output.line\"\n      show: steps.first.output\n",
                "steps.first.output.line",
            ),
            (
                "question",
                "      ask: \"'Which channel for ' + steps.first.output.line + '?'\"\n      \
                 answer: [stable, beta]\n      remember: true\n",
                "steps.pick.decision.answer",
            ),
            (
                "timed",
                "      ask: \"'Which channel?'\"\n      answer: [stable, beta]\n      \
                 remember: true\n      timeout: 2s\n      on_timeout: { default: beta }\n",
                "steps.pick.decision.answer",
            ),
        ] {
            std::fs::write(
                root.join(format!("workflows/{name}.workflow.yaml")),
                workflow(name, gate, then),
            )
            .unwrap();
        }
        // A gate that shows two keys a model chose, each the start of a
        // private key, so both redact to the same text.
        std::fs::write(
            root.join("workflows/collide.workflow.yaml"),
            workflow(
                "collide",
                &format!(
                    "      title: \"'Look at both'\"\n      show: \"{{'{KEY_START}1': 'harmless', \
                     '{KEY_START}2': 'rm -rf /'}}\"\n"
                ),
                "steps.first.output.line",
            ),
        )
        .unwrap();
        Self { dir, root }
    }

    fn command(&self, program: &str) -> Command {
        let mut c = Command::new(program);
        c.current_dir(&self.root)
            .env("HOME", self.dir.join("home"))
            .env("ATOMIC_HOME", self.dir.join("atomic-home"))
            .env("ATOMIC_CREDENTIAL_STORE", "file")
            .env("USER", "dev")
            .env_remove("LOGNAME");
        for name in [
            "CODEX_HOME",
            "XDG_DATA_HOME",
            // The person's own configuration is under the throwaway home.
            "XDG_CONFIG_HOME",
            "ATOMIC_DATABASE_URL",
            "ATOMIC_DATABASE_SCHEMA",
            "ATOMIC_EXECUTOR_ID",
            "ATOMIC_FAILPOINT",
            "ATOMIC_MODEL_BASE_URL",
            "ATOMIC_MODEL",
            "ATOMIC_MODEL_API_KEY_ENV",
            "ATOMIC_RUNNER",
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "DATABASE_URL",
        ] {
            c.env_remove(name);
        }
        // No Ollama, whatever runs on this machine.
        c.env("ATOMIC_DETECT_OLLAMA_ADDR", "127.0.0.1:9");
        c
    }

    /// `atomic args` with no terminal.
    fn atomic(&self, args: &[&str]) -> Output {
        self.command(ATOMIC)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// `atomic args` on a terminal, typing `answers` at its prompts.
    fn at_terminal(&self, args: &[&str], answers: &[&str]) -> Output {
        self.at_terminal_with(args, answers, &[])
    }

    /// [`Self::at_terminal`], with the driver's settings in `env`. Fails the
    /// test if the driver ran out its backstop deadline instead of getting
    /// what the test waited for: a test that waits for a run to *stay*
    /// waiting must be bounded by what the run does, and a deadline a busy
    /// machine misses is not that.
    fn at_terminal_with(&self, args: &[&str], answers: &[&str], env: &[(&str, &str)]) -> Output {
        let mut c = self.command("python3");
        c.arg(self.dir.join("driver.py"))
            .arg(serde_json::to_string(answers).unwrap())
            .arg(ATOMIC)
            .args(args);
        for (k, v) in env {
            c.env(k, v);
        }
        let out = c.output().unwrap();
        let t = text(&out);
        assert!(
            !t.contains("[driver: deadline]"),
            "the driver waited out its backstop without seeing what this test waits for: {t}"
        );
        out
    }

    /// `atomic args` with no terminal, killed if the test fails first.
    fn spawn(&self, args: &[&str]) -> Running {
        Running(Some(
            self.command(ATOMIC)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        ))
    }

    /// Gate `id` as the store has it.
    fn gate(&self, id: &str) -> gate::Gate {
        let (db, id) = (self.store(), id.to_owned());
        let found = std::sync::Arc::new(std::sync::Mutex::new(None));
        let into = found.clone();
        on_codex_runtime(async move {
            let rt = project_store(&db).await;
            *into.lock().unwrap() = gate::find(&rt, &id).await.unwrap();
        });
        found.lock().unwrap().take().expect("the gate")
    }

    fn enroll(&self) {
        let out = self.at_terminal(&["gates", "enroll"], &[]);
        assert!(text(&out).contains("enrolled local:dev"), "{}", text(&out));
    }

    fn state(&self) -> PathBuf {
        let out = self.atomic(&["debug", "state-dir"]);
        assert!(out.status.success(), "{}", text(&out));
        PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
    }

    /// The project's store, in its private state beside its state directory.
    fn store(&self) -> PathBuf {
        let state = self.state();
        state
            .parent()
            .and_then(Path::parent)
            .expect("the atomic home")
            .join("private")
            .join(state.file_name().expect("the project's id"))
            .join("atomic.db")
    }
}

/// The `gated` project's store at `db`, `<home>/private/<id>/atomic.db`,
/// opened as atomic opens it, for that project: what it lists of gates and
/// runs is the project's.
async fn project_store(db: &Path) -> Durare {
    let id = db
        .parent()
        .and_then(Path::file_name)
        .expect("a project id")
        .to_string_lossy()
        .into_owned();
    Durare::open(
        &atomic_durable::Database::Sqlite(db.to_owned()),
        None,
        atomic_app::pipeline::LAYOUT_VERSION,
        &format!("gated/{id}"),
    )
    .await
    .unwrap()
}

/// A run in the background, killed when dropped unless waited for.
struct Running(Option<std::process::Child>);

impl Running {
    fn wait(mut self) -> Output {
        self.0.take().unwrap().wait_with_output().unwrap()
    }

    fn still(&mut self) -> bool {
        self.0.as_mut().unwrap().try_wait().unwrap().is_none()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn an_approval_gate_is_approved_at_the_prompt_and_the_workflow_continues() {
    support::require_sandbox();
    let p = Project::new("approve");
    p.enroll();
    let out = p.at_terminal(
        &[
            "run",
            "approval",
            "--run-id",
            "at-prompt",
            "--data",
            r#"{"text": "v1"}"#,
        ],
        &["maybe", "a looks right"],
    );
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(t.contains("gate at-prompt:pick is waiting for you"), "{t}");
    assert!(t.contains("Ship got v1"), "{t}");
    assert!(
        t.contains("type `a` to approve"),
        "an unclear reply is asked again: {t}"
    );
    assert!(t.contains("approved by local:dev"), "{t}");
    assert!(t.contains("got got v1"), "the workflow continued: {t}");
    on_codex_runtime(async move {
        let rt = project_store(&p.store()).await;
        let g = gate::find(&rt, "at-prompt:pick").await.unwrap().unwrap();
        let Some(gate::Resolution::Decided(d)) = g.resolution else {
            panic!("{g:?}");
        };
        assert_eq!(d.by, Principal::local("dev"));
        assert_eq!(d.comment.as_deref(), Some("looks right"));
    });
}

#[test]
fn a_service_without_user_can_enroll_and_decide_its_gate() {
    support::require_sandbox();
    let p = Project::new("service-operator");
    for (login, expected) in [("service", "service"), ("", "operator")] {
        let env = [("USER", ""), ("LOGNAME", login)];
        let enrolled = p.at_terminal_with(&["gates", "enroll"], &[], &env);
        assert!(enrolled.status.success(), "{}", text(&enrolled));
        let approved = p.at_terminal_with(
            &[
                "run",
                "approval",
                "--run-id",
                expected,
                "--data",
                r#"{"text":"v1"}"#,
            ],
            &["a"],
            &env,
        );
        let transcript = text(&approved);
        assert!(approved.status.success(), "{transcript}");
        assert!(
            transcript.contains(&format!("approved by local:{expected}")),
            "{transcript}"
        );
    }
}

#[test]
fn a_reply_is_taken_however_late_the_prompt_reads_on_after_showing() {
    support::require_sandbox();
    let p = Project::new("late");
    p.enroll();
    // atomic held up between writing the prompt and reading on, as a busy
    // machine may hold it: the reply typed at the prompt arrives meanwhile,
    // and is still the reply to it.
    let out = p.at_terminal_with(
        &[
            "run",
            "approval",
            "--run-id",
            "late",
            "--data",
            r#"{"text": "v8"}"#,
        ],
        &["a"],
        &[
            ("ATOMIC_FAILPOINT_PAUSE", "prompt:after-ask"),
            ("DRIVER_DEADLINE", "30"),
        ],
    );
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(t.contains("gate late:pick: approved by local:dev"), "{t}");
}

#[test]
fn without_a_terminal_the_run_waits_for_a_decision_from_another() {
    support::require_sandbox();
    use std::io::BufRead;
    let p = Project::new("no-tty");
    p.enroll();
    let mut run = p.spawn(&[
        "run",
        "approval",
        "--run-id",
        "elsewhere",
        "--data",
        r#"{"text": "v2"}"#,
    ]);
    let (tx, rx) = std::sync::mpsc::channel();
    let stderr = run.0.as_mut().unwrap().stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stderr)
            .lines()
            .map_while(Result::ok)
        {
            let _ = tx.send(line);
        }
    });
    // The run says how to decide, and waits.
    let told = rx
        .iter()
        .find(|l| l.contains("decide it from a terminal with"))
        .expect("the run said how to decide its gate");
    assert!(
        told.contains("`atomic gates approve elsewhere:pick`"),
        "{told}"
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(run.still(), "the run waits");
    let approved = p.atomic(&["gates", "approve", "elsewhere:pick"]);
    assert!(approved.status.success(), "{}", text(&approved));
    let out = run.wait();
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(t.contains("got got v2"), "{t}");
}

#[test]
fn a_question_gate_answered_at_the_prompt_resumes_with_the_typed_answer_and_remembers_it() {
    support::require_sandbox();
    let p = Project::new("question");
    p.enroll();
    let out = p.at_terminal(
        &[
            "run",
            "question",
            "--run-id",
            "asked",
            "--data",
            r#"{"text": "v3"}"#,
        ],
        &["nightly", "b"],
    );
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(t.contains("question: Which channel for got v3?"), "{t}");
    assert!(t.contains("a) stable") && t.contains("b) beta"), "{t}");
    assert!(
        t.contains("nightly is not an answer to this question"),
        "an answer outside the enum is refused: {t}"
    );
    assert!(t.contains("answered beta by local:dev"), "{t}");
    assert!(
        t.contains("got beta"),
        "the step after it got the typed answer: {t}"
    );
    on_codex_runtime(async move {
        let rt = project_store(&p.store()).await;
        let log = gate::feedback(&rt).await.unwrap();
        assert_eq!(log.len(), 1, "{log:?}");
        assert_eq!(log[0].gate, "asked:pick");
        assert_eq!(log[0].ask, "Which channel for got v3?");
        assert_eq!(log[0].answer, json!("beta"));
        assert_eq!(log[0].by, Some(Principal::local("dev")));
        assert!(log[0].at > 0);
    });
}

#[test]
fn an_answer_outside_the_enum_is_refused_from_another_terminal_too() {
    support::require_sandbox();
    let p = Project::new("refused");
    p.enroll();
    let run = p.spawn(&[
        "run",
        "question",
        "--run-id",
        "typed",
        "--data",
        r#"{"text": "v4"}"#,
    ]);
    let until = Instant::now() + Duration::from_secs(60);
    while !text(&p.atomic(&["gates", "list"])).contains("typed:pick") {
        assert!(Instant::now() < until, "the gate never opened");
        std::thread::sleep(Duration::from_millis(200));
    }
    let refused = p.atomic(&["gates", "answer", "typed:pick", "nightly"]);
    assert!(!refused.status.success());
    assert!(
        text(&refused).contains("does not answer it"),
        "{}",
        text(&refused)
    );
    let refused = p.atomic(&["gates", "approve", "typed:pick"]);
    assert!(
        !refused.status.success(),
        "approving a question needs an answer"
    );
    let answered = p.atomic(&["gates", "answer", "typed:pick", "stable"]);
    assert!(answered.status.success(), "{}", text(&answered));
    let out = run.wait();
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("got stable"), "{}", text(&out));
    on_codex_runtime(async move {
        let rt = project_store(&p.store()).await;
        let log = gate::feedback(&rt).await.unwrap();
        assert_eq!(
            log.len(),
            1,
            "only the accepted answer is remembered: {log:?}"
        );
        assert_eq!(log[0].answer, json!("stable"));
    });
}

#[test]
fn a_question_nobody_answers_in_time_resumes_with_its_default_and_remembers_it() {
    support::require_sandbox();
    let p = Project::new("timed");
    p.enroll();
    let out = p.at_terminal(
        &[
            "run",
            "timed",
            "--run-id",
            "late",
            "--data",
            r#"{"text": "v5"}"#,
        ],
        &[],
    );
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(
        t.contains("silence selects beta"),
        "the prompt says what silence does: {t}"
    );
    assert!(t.contains("got beta"), "the declared default: {t}");
    let g = p.gate("late:pick");
    assert!(
        matches!(&g.resolution, Some(gate::Resolution::Defaulted { answer, .. }) if answer == &json!("beta")),
        "recorded as resumed with the default: {g:?}"
    );
    let listed = text(&p.atomic(&["gates", "list", "--all"]));
    assert!(listed.contains("silence selected beta"), "{listed}");
    on_codex_runtime(async move {
        let rt = project_store(&p.store()).await;
        let log = gate::feedback(&rt).await.unwrap();
        assert_eq!(log.len(), 1, "the remembered default: {log:?}");
        assert_eq!(log[0].answer, json!("beta"));
        assert_eq!(log[0].by, None, "nobody answered");
    });
}

#[test]
fn replies_typed_ahead_decide_no_gate_they_were_not_typed_at() {
    support::require_sandbox();
    let p = Project::new("ahead");
    p.enroll();
    // Two replies at once, at the first gate: the second must not decide
    // the second gate, which nobody has seen yet.
    let out = p.at_terminal_with(
        &[
            "run",
            "twice",
            "--run-id",
            "ahead",
            "--data",
            r#"{"text": "v6"}"#,
        ],
        &["a looked\na deployed"],
        // Both replies go to the first prompt, so the driver stops once that
        // one is answered and the terminal has been quiet long enough for
        // the second to decide the second gate, were it going to.
        &[
            ("DRIVER_STOP_AFTER", "1"),
            ("DRIVER_QUIET", "3"),
            ("DRIVER_DEADLINE", "60"),
        ],
    );
    let t = text(&out);
    assert!(t.contains("gate ahead:look: approved by local:dev"), "{t}");
    assert!(t.contains("gate ahead:deploy is waiting for you"), "{t}");
    assert!(!t.contains("gate ahead:deploy: approved"), "{t}");
    assert_eq!(
        p.gate("ahead:deploy").resolution,
        None,
        "the second gate waits"
    );

    // A line typed before anything is shown decides nothing either.
    let out = p.at_terminal_with(
        &[
            "run",
            "approval",
            "--run-id",
            "early",
            "--data",
            r#"{"text": "v7"}"#,
        ],
        &[],
        // Nothing is typed at this prompt: what was typed before the run
        // started was thrown away before the gate was shown. So the driver
        // stops once the prompt has shown and the terminal has been quiet
        // long enough for that line to decide the gate, were it going to.
        &[
            ("DRIVER_EARLY", "a"),
            ("DRIVER_STOP_MARKER", "is waiting for you"),
            ("DRIVER_QUIET", "3"),
            ("DRIVER_DEADLINE", "60"),
        ],
    );
    let t = text(&out);
    assert!(t.contains("gate early:pick is waiting for you"), "{t}");
    assert!(!t.contains("approved by"), "{t}");
    assert_eq!(p.gate("early:pick").resolution, None, "the gate waits");
}

#[test]
fn an_exec_command_cannot_type_into_the_prompt() {
    support::require_sandbox();
    let p = Project::new("inject");
    p.enroll();
    // In the run's caches, which a confined command may write, and which
    // stay while the run waits at its gate.
    let data = json!({"text": "inject.log"}).to_string();
    let out = p.at_terminal_with(
        &["run", "injected", "--run-id", "typed-in", "--data", &data],
        &[],
        // The exec step has finished its probe before the gate opens.
        &[
            ("DRIVER_STOP_MARKER", "is waiting for you"),
            ("DRIVER_QUIET", "4"),
            ("DRIVER_DEADLINE", "60"),
        ],
    );
    let t = text(&out);
    assert!(t.contains("gate typed-in:deploy is waiting for you"), "{t}");
    let tried = std::fs::read_dir(p.state().join("cache"))
        .into_iter()
        .flatten()
        .flatten()
        .find_map(|run| std::fs::read_to_string(run.path().join("shared/inject.log")).ok())
        .unwrap_or_else(|| panic!("the terminal probe did not report its result: {t}"));
    assert!(
        tried.starts_with("no tty"),
        "the command reached the terminal: {tried} {t}"
    );
    assert!(!t.contains("approved by"), "{t}");
    assert_eq!(p.gate("typed-in:deploy").resolution, None, "the gate waits");
}

/// Make the gate store stop answering the prompt's read of gate `id`, and
/// keep failing the same way every pass: its state becomes one the store
/// cannot read back. `shown` says the prompt has been written; nothing else
/// reads a gate while its run waits at it, so the run goes on waiting and
/// only the prompt's own read fails.
fn stop_the_store_answering(store: &Path, id: &str, shown: &Path) {
    let (store, id, shown) = (store.to_path_buf(), id.to_owned(), shown.to_path_buf());
    on_codex_runtime(async move {
        let until = Instant::now() + Duration::from_secs(60);
        while !shown.exists() {
            assert!(Instant::now() < until, "the prompt never showed");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        use sqlx::Connection;
        let url = format!("sqlite://{}", store.display());
        let mut db = sqlx::SqliteConnection::connect(&url)
            .await
            .expect("the gate store");
        // The run's engine writes between its reads, so a write that finds
        // the store busy is worth trying again for a while.
        let till = Instant::now() + Duration::from_secs(10);
        let changed = loop {
            let tried =
                sqlx::query("UPDATE atomic_gates SET state = 'unreadable' WHERE gate_id = ?")
                    .bind(id.clone())
                    .execute(&mut db)
                    .await;
            match tried {
                Ok(r) => break r.rows_affected(),
                Err(e) => {
                    assert!(Instant::now() < till, "cannot reach the gate store: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        };
        db.close().await.expect("the store closes");
        assert_eq!(changed, 1, "gate {id} is not in the store to break");
    });
}

/// A gate store that stops answering while a gate is shown is told about
/// once, not on every pass of the read the prompt makes to see whether the
/// gate was decided elsewhere.
#[test]
fn a_gate_store_that_stops_answering_while_a_gate_is_shown_is_told_once() {
    support::require_sandbox();
    let p = Project::new("store");
    p.enroll();
    let shown = p.dir.join("prompt-shown");
    let (shown_file, store) = (shown.display().to_string(), p.store());
    let out = std::thread::scope(|scope| {
        let running = scope.spawn(|| {
            p.at_terminal_with(
                &[
                    "run",
                    "approval",
                    "--run-id",
                    "failing",
                    "--data",
                    r#"{"text":"v9"}"#,
                ],
                &[],
                // The driver tells the test when the prompt has shown, so
                // that it can break the store while the run sits waiting, and
                // stops once the fault has been told and the terminal has
                // gone quiet for three seconds. A prompt told on every pass
                // never goes quiet, and ends on the deadline instead.
                &[
                    ("DRIVER_SEEN_MARKER", "is waiting for you"),
                    ("DRIVER_SEEN_FILE", shown_file.as_str()),
                    ("DRIVER_STOP_MARKER", "the gate store"),
                    ("DRIVER_QUIET", "3"),
                    ("DRIVER_DEADLINE", "60"),
                ],
            )
        });
        stop_the_store_answering(&store, "failing:pick", &shown);
        running.join().unwrap()
    });
    let t = text(&out);
    assert!(
        t.contains("gate failing:pick is waiting for you"),
        "the prompt did not show: {t}"
    );
    // The fault, named by the reason the store gives for it, which is the
    // same on every pass.
    let told: Vec<&str> = t
        .lines()
        .filter(|l| l.contains("unknown gate state"))
        .collect();
    assert_eq!(
        told.len(),
        1,
        "a store that failed on every pass was told {} times: {t}",
        told.len()
    );
    assert!(
        told[0].contains("the gate store:"),
        "not the prompt's own fault line: {:?}",
        told[0]
    );
    assert!(
        !t.contains("cannot read the gate store"),
        "the read that looks for a newly opened gate faulted too, so this says \
         nothing about the prompt's own tell: {t}"
    );
}

/// What starts a private key block, which a model can put in a gate's
/// title or in what it shows.
const KEY_START: &str = "-----BEGIN PRIVATE KEY-----";

#[test]
fn the_start_of_a_private_key_in_a_gate_hides_nothing_else_it_shows() {
    support::require_sandbox();
    let p = Project::new("keyed");
    p.enroll();
    let data = json!({ "text": KEY_START }).to_string();
    // A run id with a secret's word in it is a name, not a secret.
    let out = p.at_terminal(
        &[
            "run",
            "approval",
            "--run-id",
            "rotate-secret",
            "--data",
            &data,
        ],
        &["a"],
    );
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    // The title is redacted, and the shows: block after it is all there.
    assert!(t.contains("  Ship got [REDACTED]"), "{t}");
    assert!(t.contains("  shows: {"), "the show was hidden: {t}");
    assert!(
        t.contains("\"line\": \"got [REDACTED]\""),
        "the show was hidden: {t}"
    );
    assert!(
        t.lines().any(|l| l.trim_end() == "  }"),
        "the show was cut: {t}"
    );
    assert!(t.contains("`atomic gates list` shows it in full"), "{t}");
    // atomic writes the gate to the terminal, which the driver reads and
    // prints; the run's result, after it, is the person's own data.
    let prompt = t.split("approved by").next().unwrap_or_default();
    assert!(!prompt.contains(KEY_START), "{t}");
    // atomic's own words and the names in them are not redacted.
    assert!(
        t.contains("gate rotate-secret:pick is waiting for you"),
        "{t}"
    );
    assert!(t.contains("run rotate-secret: approval in project"), "{t}");
    assert!(
        t.contains("gate rotate-secret:pick: approved by local:dev"),
        "{t}"
    );
    // A question's choices, after a title holding the same.
    let out = p.at_terminal(
        &[
            "run",
            "question",
            "--run-id",
            "keyed-question",
            "--data",
            &data,
        ],
        &["b"],
    );
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(
        t.contains("question: Which channel for got [REDACTED]"),
        "{t}"
    );
    assert!(
        t.contains("a) stable") && t.contains("b) beta"),
        "the choices were hidden: {t}"
    );
    assert!(t.contains("answered beta by local:dev"), "{t}");
}

#[test]
fn without_a_terminal_the_start_of_a_private_key_in_a_title_hides_how_to_decide_nothing() {
    support::require_sandbox();
    use std::io::BufRead;
    let p = Project::new("keyed-no-tty");
    p.enroll();
    let data = json!({ "text": KEY_START }).to_string();
    let mut run = p.spawn(&[
        "run",
        "approval",
        "--run-id",
        "rotate-secret",
        "--data",
        &data,
    ]);
    let stderr = run.0.as_mut().unwrap().stderr.take().unwrap();
    let told = std::io::BufReader::new(stderr)
        .lines()
        .map_while(Result::ok)
        .find(|l| l.contains("waits at gate"))
        .expect("the run said it waits");
    assert!(
        told.contains(
            "`atomic gates approve rotate-secret:pick` or `atomic gates reject rotate-secret:pick`"
        ),
        "{told}"
    );
    assert!(!told.contains(KEY_START), "{told}");
}

#[test]
fn a_hostile_title_reaches_the_terminal_escaped() {
    support::require_sandbox();
    let p = Project::new("hostile");
    p.enroll();
    let text_in = "\u{1b}]0;owned\u{7}\u{1b}[2J\u{7f}\u{9b}31m\u{202e}gpj.exe";
    let data = json!({ "text": text_in }).to_string();
    let run = p.spawn(&["run", "approval", "--run-id", "hostile", "--data", &data]);
    let until = Instant::now() + Duration::from_secs(60);
    let listed = loop {
        let listed = p.atomic(&["gates", "list"]);
        if text(&listed).contains("hostile:pick") {
            break listed;
        }
        assert!(Instant::now() < until, "the gate never opened");
        std::thread::sleep(Duration::from_millis(200));
    };
    drop(run);
    let raw = String::from_utf8_lossy(&listed.stdout).into_owned();
    assert!(
        !raw.contains('\u{1b}') && !raw.contains('\u{7}') && !raw.contains('\u{202e}'),
        "{raw:?}"
    );
    assert!(raw.contains("\\u{001b}]0;owned"), "{raw}");
    // The JSON form too: every unsafe character escaped, and the title
    // still reads back whole.
    let listed = p.atomic(&["gates", "list", "--json"]);
    let raw = String::from_utf8_lossy(&listed.stdout).into_owned();
    for c in ['\u{1b}', '\u{7}', '\u{7f}', '\u{9b}', '\u{202e}'] {
        assert!(!raw.contains(c), "{c:?} reached the terminal: {raw:?}");
    }
    let gates: Value = serde_json::from_str(&raw).expect("JSON");
    let title = gates[0]["title"].as_str().unwrap_or_default();
    assert!(title.ends_with(text_in), "{title:?}");
}

/// Characters that must never reach a terminal as they are: ESC, BEL, DEL,
/// the C1 controls, and Unicode's zero-width, bidirectional and byte-order
/// controls. Written out here, and not taken from what atomic escapes, so a
/// character atomic forgets cannot pass the test by being forgotten.
fn unsafe_on_a_terminal(c: char) -> bool {
    matches!(c, '\u{1b}' | '\u{7}' | '\u{7f}' | '\u{feff}')
        || ('\u{80}'..='\u{9f}').contains(&c)
        || ('\u{200b}'..='\u{200f}').contains(&c)
        || ('\u{202a}'..='\u{202e}').contains(&c)
        || ('\u{2060}'..='\u{2069}').contains(&c)
        || ('\u{fff9}'..='\u{fffb}').contains(&c)
}

#[test]
fn a_hostile_title_at_the_prompt_reaches_the_terminal_escaped() {
    support::require_sandbox();
    let p = Project::new("hostile-prompt");
    p.enroll();
    let text_in = "\u{1b}]0;owned\u{7}\u{1b}[2J\u{7f}\u{9b}31m\u{202e}gpj.exe";
    let data = json!({ "text": text_in }).to_string();
    let out = p.at_terminal_with(
        &[
            "run",
            "approval",
            "--run-id",
            "hostile-prompt",
            "--data",
            &data,
        ],
        &["a"],
        &[("DRIVER_DEADLINE", "60")],
    );
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(
        t.contains("gate hostile-prompt:pick is waiting for you"),
        "the prompt did not show: {t}"
    );
    // The title is there, as text: the escape it holds reads as what it is.
    assert!(t.contains("\\u{001b}]0;owned"), "the title: {t}");
    assert!(
        t.contains("gate hostile-prompt:pick: approved by local:dev"),
        "the prompt did not decide the gate: {t}"
    );
    // Everything the prompt wrote, up to the decision that answers it. The
    // run's own result after it is the person's data, which atomic prints
    // as it is.
    let prompt = t
        .split("gate hostile-prompt:pick: approved")
        .next()
        .unwrap_or_default();
    let raw: Vec<char> = prompt
        .chars()
        .filter(|c| unsafe_on_a_terminal(*c))
        .collect();
    assert!(
        raw.is_empty(),
        "{raw:?} reached the terminal from the prompt: {prompt:?}"
    );
}

#[test]
fn without_standard_error_on_the_terminal_or_in_the_background_it_does_not_prompt() {
    support::require_sandbox();
    let p = Project::new("not-here");
    p.enroll();
    let errors = p.dir.join("stderr.txt");
    let errors_text = errors.display().to_string();
    for (run, setting) in [
        ("errors-elsewhere", ("DRIVER_STDERR", errors_text.as_str())),
        ("backgrounded", ("DRIVER_BACKGROUND", "1")),
    ] {
        let out = std::thread::scope(|scope| {
            let running = scope.spawn(|| {
                p.at_terminal_with(
                    &[
                        "run",
                        "approval",
                        "--run-id",
                        run,
                        "--data",
                        r#"{"text": "v8"}"#,
                    ],
                    &["a"],
                    &[setting, ("DRIVER_DEADLINE", "60")],
                )
            });
            let id = format!("{run}:pick");
            let until = Instant::now() + Duration::from_secs(50);
            while !text(&p.atomic(&["gates", "list"])).contains(&id) {
                assert!(Instant::now() < until, "the gate never opened");
                std::thread::sleep(Duration::from_millis(200));
            }
            std::thread::sleep(Duration::from_secs(1));
            assert_eq!(p.gate(&id).resolution, None, "nothing typed decided it");
            // Decided from another terminal: a backgrounded run is not frozen.
            let approved = p.atomic(&["gates", "approve", &id]);
            assert!(approved.status.success(), "{}", text(&approved));
            running.join().unwrap()
        });
        let t = text(&out);
        assert!(out.status.success(), "{run}: {t}");
        assert!(t.contains("got got v8"), "{run}: {t}");
        assert!(!t.contains("approve or reject"), "{run} prompted: {t}");
    }
    let said = std::fs::read_to_string(&errors).unwrap();
    assert!(said.contains("decide it from a terminal"), "{said}");
}

/// Two keys in what a gate shows that redact to the same text are both
/// shown, each with its own value: neither hides the other's.
#[test]
fn keys_that_redact_alike_in_what_a_gate_shows_are_both_shown() {
    support::require_sandbox();
    let p = Project::new("collide");
    p.enroll();
    let data = json!({ "text": "a note" }).to_string();
    let out = p.at_terminal(&["run", "collide", "--data", &data], &["a"]);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(t.contains("\"harmless\""), "a value was lost: {t}");
    assert!(t.contains("\"rm -rf /\""), "a value was lost: {t}");
    assert!(t.contains("[REDACTED] (2)"), "{t}");
    let prompt = t.split("approved by").next().unwrap_or_default();
    assert!(!prompt.contains(KEY_START), "{t}");
}
