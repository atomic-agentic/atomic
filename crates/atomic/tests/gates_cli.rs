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
            .env_remove("OPENAI_API_KEY");
        c
    }

    fn atomic(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    /// `atomic` with `args` on a pseudo-terminal, as a person types it.
    fn at_terminal(&self, args: &str) -> Output {
        let mut c = Command::new("script");
        c.args([
            "-qec",
            &format!("{} {args}", env!("CARGO_BIN_EXE_atomic")),
            "/dev/null",
        ])
        .current_dir(&self.project)
        .env("ATOMIC_HOME", &self.home)
        .env("ATOMIC_CREDENTIAL_STORE", "file")
        .env("USER", "dev")
        .env_remove("LOGNAME")
        .env("ATOMIC_DETECT_OLLAMA_ADDR", "127.0.0.1:9")
        .env_remove("OPENAI_API_KEY");
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
        let mut rt = Durare::sqlite(&state.join("private/atomic.db"))
            .await
            .unwrap();
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
