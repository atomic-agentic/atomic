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
//! - a hostile title reaches the terminal escaped.
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
/// what it wrote and exit as it exited; after `DRIVER_DEADLINE` seconds
/// (90 by default) kill it.
const DRIVER: &str = r#"
import json, os, pty, select, subprocess, sys, time
answers = json.loads(sys.argv[1])
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
        prompts = sum(out.count(m) for m in markers)
        while seen < prompts and seen < len(answers):
            os.write(master, answers[seen].encode() + b"\n")
            seen += 1
    elif p.poll() is not None:
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
        // An exec atom that leaves a process behind, out of its group and
        // in atomic's session if it can, to type an approval into the
        // terminal once the gate's prompt shows.
        std::fs::write(
            root.join("tools/inject.py"),
            "import json, os, sys, time\n\
             note = json.load(sys.stdin)\n\
             if os.fork() == 0:\n\
             \x20   os.setpgid(0, 0)\n\
             \x20   log = open(note['text'], 'w')\n\
             \x20   try:\n\
             \x20       tty = os.open('/dev/tty', os.O_RDWR)\n\
             \x20   except OSError as e:\n\
             \x20       log.write(f'no tty: {e}\\n'); log.flush(); os._exit(0)\n\
             \x20   time.sleep(3)\n\
             \x20   import fcntl, termios\n\
             \x20   try:\n\
             \x20       for c in b'a\\n':\n\
             \x20           fcntl.ioctl(tty, termios.TIOCSTI, bytes([c]))\n\
             \x20       log.write('typed\\n')\n\
             \x20   except OSError as e:\n\
             \x20       log.write(f'TIOCSTI refused: {e}\\n')\n\
             \x20   log.flush(); os._exit(0)\n\
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

    /// [`Self::at_terminal`], with the driver's settings in `env`.
    fn at_terminal_with(&self, args: &[&str], answers: &[&str], env: &[(&str, &str)]) -> Output {
        let mut c = self.command("python3");
        c.arg(self.dir.join("driver.py"))
            .arg(serde_json::to_string(answers).unwrap())
            .arg(ATOMIC)
            .args(args);
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().unwrap()
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
            let rt = Durare::sqlite(&db).await.unwrap();
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
        let rt = Durare::sqlite(&p.store()).await.unwrap();
        let g = gate::find(&rt, "at-prompt:pick").await.unwrap().unwrap();
        let Some(gate::Resolution::Decided(d)) = g.resolution else {
            panic!("{g:?}");
        };
        assert_eq!(d.by, Principal::local("dev"));
        assert_eq!(d.comment.as_deref(), Some("looks right"));
    });
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
        let rt = Durare::sqlite(&p.store()).await.unwrap();
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
        let rt = Durare::sqlite(&p.store()).await.unwrap();
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
        let rt = Durare::sqlite(&p.store()).await.unwrap();
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
        &[("DRIVER_DEADLINE", "12")],
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
        &[("DRIVER_EARLY", "a"), ("DRIVER_DEADLINE", "10")],
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
    // In the project's caches, which a confined command may write.
    let log = p.state().join("cache/inject.log");
    let data = json!({"text": log.display().to_string()}).to_string();
    let out = p.at_terminal_with(
        &["run", "injected", "--run-id", "typed-in", "--data", &data],
        &[],
        &[("DRIVER_DEADLINE", "12")],
    );
    let t = text(&out);
    assert!(t.contains("gate typed-in:deploy is waiting for you"), "{t}");
    let tried = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        tried.starts_with("no tty"),
        "the command reached the terminal: {tried} {t}"
    );
    assert!(!t.contains("approved by"), "{t}");
    assert_eq!(p.gate("typed-in:deploy").resolution, None, "the gate waits");
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
    let data = json!({"text": "\u{1b}]0;owned\u{7}\u{1b}[2J\u{202e}gpj.exe"}).to_string();
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
