// SPDX-License-Identifier: Apache-2.0

//! `atomic gates` from a second terminal: while a run waits at its gate in
//! this process (the first terminal), the operator enrolls their credential
//! with `atomic gates enroll`, sees the gate in `atomic gates list` and
//! approves it with `atomic gates approve`, each a separate `atomic`
//! process; the waiting run then passes the gate.
//!
//! - enrolling is refused without a terminal, and works in one (`script`
//!   gives the command a pseudo-terminal);
//! - a decision without an enrolled credential is refused;
//! - a second, different decision on the decided gate gets the conflict.
//!
//! The credential goes to the opt-in file store, since the test machine has
//! no OS keyring. Linux only, where credentials are kept.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use atomic_core::testing::on_codex_runtime;
use atomic_durable::gate::{
    self, Deployment, GateSpec, OnTimeout, Principal, Resolution, Tier, Verdict,
};
use atomic_durable::{DurableRuntime, Durare, DurareContext};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

struct Terminal {
    project: PathBuf,
    home: PathBuf,
}

impl Terminal {
    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_atomic"));
        c.current_dir(&self.project)
            .env("ATOMIC_HOME", &self.home)
            .env("ATOMIC_CREDENTIAL_STORE", "file")
            .env("USER", "dev")
            .env_remove("LOGNAME")
            // No model endpoint from this machine: no Ollama, no key.
            .env("ATOMIC_DETECT_OLLAMA_ADDR", "127.0.0.1:9")
            .env_remove("OPENAI_API_KEY")
            // None of the person's own configuration.
            .env("XDG_CONFIG_HOME", self.home.join("config"));
        c
    }

    fn atomic(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    /// [`Self::atomic`], as the account `user`.
    fn atomic_as(&self, user: &str, args: &[&str]) -> Output {
        self.command()
            .env("USER", user)
            .args(args)
            .output()
            .unwrap()
    }

    /// `atomic` with `args` on a pseudo-terminal, as a person types it.
    fn at_terminal(&self, args: &str) -> Output {
        self.at_terminal_as("dev", args)
    }

    /// [`Self::at_terminal`], as the account `user`.
    fn at_terminal_as(&self, user: &str, args: &str) -> Output {
        let mut c = Command::new("script");
        c.args([
            "-qec",
            &format!("{} {args}", env!("CARGO_BIN_EXE_atomic")),
            "/dev/null",
        ])
        .current_dir(&self.project)
        .env("ATOMIC_HOME", &self.home)
        .env("ATOMIC_CREDENTIAL_STORE", "file")
        .env("USER", user)
        .env_remove("LOGNAME")
        .env("ATOMIC_DETECT_OLLAMA_ADDR", "127.0.0.1:9")
        .env_remove("OPENAI_API_KEY")
        .env("XDG_CONFIG_HOME", self.home.join("config"));
        c.output().unwrap()
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
fn a_gate_is_approved_from_a_second_terminal_while_the_run_waits() {
    on_codex_runtime(async {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("atomic-gates-cli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["project", "home"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let dir = std::fs::canonicalize(dir).unwrap();
        let t = Terminal {
            project: dir.join("project"),
            home: dir.join("home"),
        };
        let out = t.atomic(&["debug", "state-dir"]);
        assert!(out.status.success(), "{}", text(&out));
        let state = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());

        // The first terminal: a run waiting at its gate.
        // Every project's private state is in the home's `private/`.
        let db = t
            .home
            .join("private")
            .join(state.file_name().unwrap())
            .join("atomic.db");
        let mut rt = Durare::sqlite(&db).await.unwrap();
        rt.register("gated", |ctx: DurareContext, (): ()| async move {
            let spec = GateSpec {
                workflow: "gated".into(),
                title: "Ship it".into(),
                show: json!({"version": "1.2.3"}),
                evidence: Vec::new(),
                approvers: gate::resolve(
                    "ship",
                    None,
                    &Deployment::Local {
                        operator: Principal::local("dev"),
                    },
                )
                .map_err(|e| atomic_durable::DurableError::Failed(e.to_string()))?,
                tier: Tier::High,
                timeout: None,
                on_timeout: OnTimeout::Reject,
                question: None,
            };
            gate::gate(&ctx, "ship", spec).await
        });
        rt.launch().await.unwrap();
        let run = format!("cli-{}", std::process::id());
        rt.start("gated", &run, ()).await.unwrap();
        let id = gate::gate_id(&run, "ship");
        while gate::find(&rt, &id).await.unwrap().is_none() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // The second terminal.
        let refused = t.atomic(&["gates", "approve", &id]);
        assert!(!refused.status.success());
        assert!(
            text(&refused).contains("atomic gates enroll"),
            "{}",
            text(&refused)
        );
        let refused = t.atomic(&["gates", "enroll"]);
        assert!(!refused.status.success());
        assert!(
            text(&refused).contains("from a terminal"),
            "{}",
            text(&refused)
        );

        let enrolled = t.at_terminal("gates enroll");
        assert!(enrolled.status.success(), "{}", text(&enrolled));
        assert!(
            text(&enrolled).contains("enrolled local:dev"),
            "{}",
            text(&enrolled)
        );
        let again = t.at_terminal("gates enroll");
        assert!(
            text(&again).contains("already enrolled"),
            "{}",
            text(&again)
        );
        assert!(gate::enrolled(&rt, &Principal::local("dev")).await.unwrap());

        let listed = t.atomic(&["gates", "list"]);
        assert!(listed.status.success(), "{}", text(&listed));
        assert!(text(&listed).contains(&id), "{}", text(&listed));
        assert!(text(&listed).contains("Ship it"), "{}", text(&listed));

        let approved = t.atomic(&["gates", "approve", &id, "--comment", "go"]);
        assert!(approved.status.success(), "{}", text(&approved));
        let resolution: Resolution = tokio::time::timeout(Duration::from_secs(30), rt.result(&run))
            .await
            .unwrap()
            .unwrap();
        let Resolution::Decided(decision) = &resolution else {
            panic!("{resolution:?}");
        };
        assert_eq!(decision.verdict, Verdict::Approve);
        assert_eq!(decision.by, Principal::local("dev"));
        assert_eq!(decision.comment.as_deref(), Some("go"));

        // A different decision now gets the conflict.
        let late = t.atomic(&["gates", "reject", &id]);
        assert!(!late.status.success());
        assert!(text(&late).contains("already resolved"), "{}", text(&late));
        rt.shutdown(Duration::from_secs(5)).await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    });
}

/// An account name is the operator's principal, and it reaches the terminal
/// escaped: a control sequence in it cannot set the window title.
#[test]
fn a_hostile_account_name_is_shown_escaped() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-gates-cli-user-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["project", "home"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    let dir = std::fs::canonicalize(dir).unwrap();
    let t = Terminal {
        project: dir.join("project"),
        home: dir.join("home"),
    };
    let enrolled = t.at_terminal_as("dev\u{1b}]0;owned\u{7}", "gates enroll");
    assert!(enrolled.status.success(), "{:?}", text(&enrolled));
    let shown = text(&enrolled);
    assert!(
        !shown.contains('\u{1b}') && !shown.contains('\u{7}'),
        "a control reached the terminal: {shown:?}"
    );
    assert!(
        shown.contains("enrolled local:dev\\u{001b}]0;owned\\u{0007}"),
        "{shown:?}"
    );
}

/// A fresh pair of terminals in a directory of their own.
fn terminals(name: &str) -> (PathBuf, Terminal) {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-gates-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["project", "home"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    let dir = std::fs::canonicalize(dir).unwrap();
    let t = Terminal {
        project: dir.join("project"),
        home: dir.join("home"),
    };
    (dir, t)
}

/// A gate id that quotes a coloured token is redacted before it is
/// escaped, in the error that says there is no such gate.
#[test]
fn a_coloured_token_in_a_gate_id_is_redacted() {
    let (dir, t) = terminals("token-id");
    let token = concat!("ghp_", "SAMPLE0123456789", "abcdefghijklmnopqrstuv");
    let id = format!("\u{1b}[1;33m{token}\u{1b}[0m");
    for json in [false, true] {
        let mut args = vec!["gates", "approve", id.as_str()];
        if json {
            args.insert(0, "--json");
        }
        let out = t.atomic(&args);
        assert!(!out.status.success());
        let shown = text(&out);
        assert!(
            !shown.contains("SAMPLE0123456789"),
            "json {json}: the token showed"
        );
        assert!(
            !shown.contains('\u{1b}'),
            "json {json}: an escape reached the terminal"
        );
        assert!(shown.contains("[REDACTED]"), "json {json}: {shown:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The principal who decided and the command id they sent reach the
/// terminal escaped, in `gates approve` and in `gates list`.
#[test]
fn a_hostile_principal_and_command_are_shown_escaped() {
    on_codex_runtime(async {
        let (dir, t) = terminals("hostile-decision");
        let user = "dev\u{1b}]0;owned\u{7}";
        let out = t.atomic(&["debug", "state-dir"]);
        assert!(out.status.success(), "{}", text(&out));
        let state = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        let db = t
            .home
            .join("private")
            .join(state.file_name().unwrap())
            .join("atomic.db");
        let mut rt = Durare::sqlite(&db).await.unwrap();
        rt.register("gated", move |ctx: DurareContext, (): ()| async move {
            let spec = GateSpec {
                workflow: "gated".into(),
                title: "Ship it".into(),
                show: json!({}),
                evidence: Vec::new(),
                approvers: gate::resolve(
                    "ship",
                    None,
                    &Deployment::Local {
                        operator: Principal::local("dev\u{1b}]0;owned\u{7}"),
                    },
                )
                .map_err(|e| atomic_durable::DurableError::Failed(e.to_string()))?,
                tier: Tier::High,
                timeout: None,
                on_timeout: OnTimeout::Reject,
                question: None,
            };
            gate::gate(&ctx, "ship", spec).await
        });
        rt.launch().await.unwrap();
        let run = format!("hostile-{}", std::process::id());
        rt.start("gated", &run, ()).await.unwrap();
        let id = gate::gate_id(&run, "ship");
        while gate::find(&rt, &id).await.unwrap().is_none() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let enrolled = t.at_terminal_as(user, "gates enroll");
        assert!(enrolled.status.success(), "{:?}", text(&enrolled));
        let approved = t.atomic_as(
            user,
            &["gates", "approve", &id, "--command-id", "cmd\u{1b}[2J"],
        );
        assert!(approved.status.success(), "{:?}", text(&approved));
        let listed = t.atomic_as(user, &["gates", "list", "--all"]);
        assert!(listed.status.success(), "{:?}", text(&listed));
        for (what, out) in [("approve", &approved), ("list", &listed)] {
            let shown = text(out);
            assert!(
                !shown.contains('\u{1b}') && !shown.contains('\u{7}'),
                "{what}: a control reached the terminal: {shown:?}"
            );
            assert!(
                shown.contains("by local:dev\\u{001b}]0;owned\\u{0007}"),
                "{what}: {shown:?}"
            );
        }
        assert!(
            text(&approved).contains("(cmd\\u{001b}[2J)"),
            "{:?}",
            text(&approved)
        );
        rt.shutdown(Duration::from_secs(5)).await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    });
}
