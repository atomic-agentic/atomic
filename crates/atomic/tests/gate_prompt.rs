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
//! - a question nobody answers in time resumes with its declared default.
//!
//! The project's atoms are exec atoms, so no model is called. The prompt
//! runs on a pseudo-terminal a small driver opens; the credential is in the
//! opt-in file store, since the test machine has no OS keyring. Linux only,
//! where credentials are kept.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use atomic_core::testing::on_codex_runtime;
use atomic_durable::Durare;
use atomic_durable::gate::{self, Principal};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const ATOMIC: &str = env!("CARGO_BIN_EXE_atomic");

/// Run a command on a pseudo-terminal and type `answers` into it, one each
/// time it shows a prompt; print what it wrote and exit as it exited.
const DRIVER: &str = r#"
import os, pty, select, subprocess, sys, time
answers = sys.argv[1].split("\n") if sys.argv[1] else []
master, slave = pty.openpty()
p = subprocess.Popen(sys.argv[2:], stdin=slave, stdout=slave, stderr=slave)
os.close(slave)
seen, out = 0, b""
# A run that never prompts fails rather than hangs.
deadline = time.time() + 90
markers = (b"comment if you like > ", b"(/reject to refuse) > ")
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
        out += b"\n[driver: no prompt within 90s]\n"
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
                 timeout: 2s\n      on_timeout: { default: stable }\n",
                "steps.pick.decision.answer",
            ),
        ] {
            std::fs::write(
                root.join(format!("workflows/{name}.workflow.yaml")),
                workflow(name, gate, then),
            )
            .unwrap();
        }
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
            "ATOMIC_DATABASE_URL",
            "ATOMIC_DATABASE_SCHEMA",
            "ATOMIC_EXECUTOR_ID",
            "ATOMIC_FAILPOINT",
            "ATOMIC_MODEL_BASE_URL",
            "ATOMIC_MODEL",
            "ATOMIC_RUNNER",
        ] {
            c.env_remove(name);
        }
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
        self.command("python3")
            .arg(self.dir.join("driver.py"))
            .arg(answers.join("\n"))
            .arg(ATOMIC)
            .args(args)
            .output()
            .unwrap()
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
        let rt = Durare::sqlite(&p.state().join("private/atomic.db"))
            .await
            .unwrap();
        let g = gate::find(&rt, "at-prompt:pick").await.unwrap().unwrap();
        let Some(gate::Resolution::Decided(d)) = g.resolution else {
            panic!("{g:?}");
        };
        assert_eq!(d.by, Principal::local("dev"));
        assert_eq!(d.comment.as_deref(), Some("looks right"));
    });
}

#[test]
fn without_a_terminal_the_run_waits_for_a_decision_from_another() {
    use std::io::BufRead;
    let p = Project::new("no-tty");
    p.enroll();
    let mut run = p
        .command(ATOMIC)
        .args([
            "run",
            "approval",
            "--run-id",
            "elsewhere",
            "--data",
            r#"{"text": "v2"}"#,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let stderr = run.stderr.take().unwrap();
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
    assert!(run.try_wait().unwrap().is_none(), "the run waits");
    let approved = p.atomic(&["gates", "approve", "elsewhere:pick"]);
    assert!(approved.status.success(), "{}", text(&approved));
    let out = run.wait_with_output().unwrap();
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(t.contains("got got v2"), "{t}");
}

#[test]
fn a_question_gate_answered_at_the_prompt_resumes_with_the_typed_answer_and_remembers_it() {
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
        &["nightly", "2"],
    );
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(t.contains("question: Which channel for got v3?"), "{t}");
    assert!(t.contains("1) stable") && t.contains("2) beta"), "{t}");
    assert!(
        t.contains("\"nightly\" is not an answer to this question"),
        "an answer outside the enum is refused: {t}"
    );
    assert!(t.contains("answered \"beta\" by local:dev"), "{t}");
    assert!(
        t.contains("got beta"),
        "the step after it got the typed answer: {t}"
    );
    on_codex_runtime(async move {
        let rt = Durare::sqlite(&p.state().join("private/atomic.db"))
            .await
            .unwrap();
        let log = gate::feedback(&rt).await.unwrap();
        assert_eq!(log.len(), 1, "{log:?}");
        assert_eq!(log[0].gate, "asked:pick");
        assert_eq!(log[0].ask, "Which channel for got v3?");
        assert_eq!(log[0].answer, json!("beta"));
        assert_eq!(log[0].by, Principal::local("dev"));
        assert!(log[0].at > 0);
    });
}

#[test]
fn an_answer_outside_the_enum_is_refused_from_another_terminal_too() {
    let p = Project::new("refused");
    p.enroll();
    let run = p
        .command(ATOMIC)
        .args([
            "run",
            "question",
            "--run-id",
            "typed",
            "--data",
            r#"{"text": "v4"}"#,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
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
    let out = run.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("got stable"), "{}", text(&out));
    on_codex_runtime(async move {
        let rt = Durare::sqlite(&p.state().join("private/atomic.db"))
            .await
            .unwrap();
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
fn a_question_nobody_answers_in_time_resumes_with_its_default() {
    let p = Project::new("timed");
    let out = p.atomic(&[
        "run",
        "timed",
        "--run-id",
        "late",
        "--data",
        r#"{"text": "v5"}"#,
    ]);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(t.contains("got stable"), "the declared default: {t}");
    on_codex_runtime(async move {
        let rt = Durare::sqlite(&p.state().join("private/atomic.db"))
            .await
            .unwrap();
        // Nobody answered, and it asks nothing remembered.
        assert!(gate::feedback(&rt).await.unwrap().is_empty());
        let g = gate::find(&rt, "late:pick").await.unwrap().unwrap();
        assert!(
            matches!(g.resolution, Some(gate::Resolution::Expired { .. })),
            "{g:?}"
        );
    });
}
