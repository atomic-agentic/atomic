// SPDX-License-Identifier: Apache-2.0

//! Model authentication for local use, through the `atomic` executable:
//!
//! - `atomic model status` says who is signed in, from atomic's own sign-in
//!   store and never with a token, and `atomic model logout` forgets the sign-in;
//! - with no endpoint and no sign-in, a run of a model atom says to sign in
//!   or configure an endpoint; signed in, with no model, it says to set one;
//! - signed in, a lite atom reports that the sign-in is for Codex atoms only
//!   and what it needs instead, before it calls anything.
//!
//! A Codex atom running on the sign-in, with no API key set, is tested in
//! process (atomic_core's runtime tests), against the scripted fake model
//! standing in for ChatGPT's Codex backend. The sign-in here is a file
//! store with a token no real backend would take; the test machine has no
//! OS keyring. Linux only, where sign-ins are kept.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const ATOMIC: &str = env!("CARGO_BIN_EXE_atomic");

struct Fixture {
    dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn b64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().fold(0u32, |n, b| (n << 8) | u32::from(*b)) << (8 * (3 - chunk.len()));
        for i in 0..=chunk.len() {
            out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
        }
    }
    out
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("atomic-model-auth-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let project = dir.join("project");
        std::fs::create_dir_all(project.join("atoms")).unwrap();
        std::fs::create_dir_all(dir.join("home")).unwrap();
        std::fs::write(project.join("atomic.yaml"), "atomic: 1\nname: auth\n").unwrap();
        // An exec atom that reports whether it can read atomic's environment
        // through /proc.
        std::fs::create_dir_all(project.join("tools")).unwrap();
        std::fs::write(
            project.join("tools/peek.py"),
            "import json, os, sys\njson.load(sys.stdin)\n\
             try:\n    seen = open(f'/proc/{os.getppid()}/environ', 'rb').read().decode(errors='replace')\n\
             except OSError as e:\n    seen = f'refused: {e}'\n\
             json.dump({'line': seen}, sys.stdout)\n",
        )
        .unwrap();
        std::fs::write(
            project.join("atoms/peek.atom.astro"),
            "---\nimport type { Props, Input, Output } from '../.atomic/types/peek';\n\
             import type { AtomConfig } from '../.atomic/types/authoring';\n\
             const { input }: Props = Astro.props;\n\
             export const config: AtomConfig<Input, Output> = { description: 'Peek.', \
             kind: 'exec', exec: { command: ['python3', 'tools/peek.py'] } };\n---\n\n",
        )
        .unwrap();
        for (role, field) in [("input", "text"), ("output", "line")] {
            std::fs::write(
                project.join(format!("atoms/peek.{role}.schema.json")),
                json!({"type": "object", "additionalProperties": false, "required": [field],
                       "properties": {field: {"type": "string"}}})
                .to_string(),
            )
            .unwrap();
        }
        for (name, kind) in [("tidy-lite", "lite"), ("tidy-codex", "codex")] {
            std::fs::write(
                project.join(format!("atoms/{name}.atom.astro")),
                format!(
                    "---\nimport type {{ Props, Input, Output }} from '../.atomic/types/{name}';\n\
                     import type {{ AtomConfig }} from '../.atomic/types/authoring';\n\
                     const {{ input }}: Props = Astro.props;\n\
                     export const config: AtomConfig<Input, Output> = {{ description: 'Tidy.', \
                     kind: '{kind}' }};\n---\nTidy this note: {{input.text}}\n"
                ),
            )
            .unwrap();
            for (role, field) in [("input", "text"), ("output", "line")] {
                std::fs::write(
                    project.join(format!("atoms/{name}.{role}.schema.json")),
                    json!({"type": "object", "additionalProperties": false, "required": [field],
                           "properties": {field: {"type": "string"}}})
                    .to_string(),
                )
                .unwrap();
            }
        }
        Self { dir }
    }

    /// Keep a ChatGPT sign-in in the file store, as `atomic model login` would.
    fn sign_in(&self) {
        self.sign_in_with("synthetic-access-token", "synthetic-refresh-token");
    }

    /// A sign-in with no token to revoke, so logging out calls no issuer.
    fn sign_in_unrevocable(&self) {
        self.sign_in_with("", "");
    }

    fn sign_in_with(&self, access: &str, refresh: &str) {
        let auth = self.dir.join("atomic-home/credentials/codex-auth");
        std::fs::create_dir_all(&auth).unwrap();
        let id_token = format!(
            "{}.{}.sig",
            b64url(br#"{"alg":"none","typ":"JWT"}"#),
            b64url(
                json!({"email": "dev@example.invalid", "https://api.openai.com/auth": {
                    "chatgpt_plan_type": "plus", "chatgpt_account_id": "account-1"}})
                .to_string()
                .as_bytes()
            )
        );
        std::fs::write(
            auth.join("auth.json"),
            json!({"auth_mode": "chatgpt", "OPENAI_API_KEY": null, "tokens": {
                "id_token": id_token, "access_token": access,
                "refresh_token": refresh, "account_id": "account-1"}})
            .to_string(),
        )
        .unwrap();
    }

    fn atomic(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut c = Command::new(ATOMIC);
        c.args(args)
            .current_dir(self.dir.join("project"))
            .env("HOME", self.dir.join("home"))
            .env("ATOMIC_HOME", self.dir.join("atomic-home"))
            .env("ATOMIC_CREDENTIAL_STORE", "file")
            .env("USER", "dev")
            .stdin(Stdio::null());
        for name in [
            "CODEX_HOME",
            "XDG_DATA_HOME",
            "ATOMIC_DATABASE_URL",
            "ATOMIC_MODEL_BASE_URL",
            "ATOMIC_MODEL",
            "ATOMIC_MODEL_API_KEY_ENV",
            "OPENAI_API_KEY",
            "CODEX_API_KEY",
        ] {
            c.env_remove(name);
        }
        for (k, v) in env {
            c.env(k, v);
        }
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
fn login_status_says_who_is_signed_in_and_logout_forgets_it() {
    let f = Fixture::new("status");
    let none = f.atomic(&["model", "status"], &[]);
    assert!(!none.status.success());
    assert!(
        text(&none).contains("no one is signed in"),
        "{}",
        text(&none)
    );
    // No token to revoke: logging out below calls no issuer.
    f.sign_in_unrevocable();
    let status = f.atomic(&["model", "status"], &[]);
    let t = text(&status);
    assert!(status.status.success(), "{t}");
    assert!(
        t.contains("signed in with chatgpt as dev@example.invalid (Plus plan)"),
        "{t}"
    );
    assert!(
        !t.contains("synthetic-access-token"),
        "a token was printed: {t}"
    );
    let json: serde_json::Value =
        serde_json::from_slice(&f.atomic(&["--json", "model", "status"], &[]).stdout)
            .expect("json");
    assert_eq!(json["signed_in"], true);
    assert_eq!(json["account"]["mode"], "chatgpt");
    assert_eq!(json["account"]["store"], "the credential file store");
    let out = f.atomic(&["model", "logout"], &[]);
    assert!(text(&out).contains("signed out"), "{}", text(&out));
    assert!(!f.atomic(&["model", "status"], &[]).status.success());
}

#[test]
fn a_model_atom_with_no_model_access_says_how_to_get_it() {
    let f = Fixture::new("none");
    let data = ["--data", r#"{"text": "a note"}"#];
    let out = f.atomic(&["run", "tidy-codex", data[0], data[1]], &[]);
    let t = text(&out);
    assert!(!out.status.success());
    assert!(t.contains("atomic model login"), "{t}");
    assert!(t.contains("ATOMIC_MODEL_BASE_URL"), "{t}");
    f.sign_in();
    let out = f.atomic(&["run", "tidy-codex", data[0], data[1]], &[]);
    assert!(!out.status.success());
    assert!(text(&out).contains("set ATOMIC_MODEL"), "{}", text(&out));
}

#[test]
fn signed_in_a_lite_atom_says_what_it_needs() {
    let f = Fixture::new("lite");
    f.sign_in();
    let out = f.atomic(
        &["run", "tidy-lite", "--data", r#"{"text": "a note"}"#],
        &[("ATOMIC_MODEL", "synthetic")],
    );
    let t = text(&out);
    assert!(!out.status.success(), "{t}");
    assert!(t.contains("serves Codex atoms only"), "{t}");
    assert!(t.contains("ATOMIC_MODEL_BASE_URL"), "{t}");
}

#[test]
fn a_command_atomic_runs_cannot_read_its_environment() {
    let f = Fixture::new("environ");
    let out = f.atomic(
        &["run", "peek", "--data", r#"{"text": "x"}"#],
        &[("ATOMIC_TEST_CANARY", "canary-in-atomics-environment")],
    );
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(
        !t.contains("canary-in-atomics-environment"),
        "read through /proc: {t}"
    );
    assert!(t.contains("refused"), "{t}");
}

/// A Codex thread through `atomic run` reads its instructions through the
/// sandbox helper, so this needs Codex's Linux sandbox.
#[cfg(feature = "sandbox-tests")]
#[test]
fn a_configured_endpoint_wins_and_gets_no_sign_in_token() {
    use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
    let f = Fixture::new("precedence");
    f.sign_in();
    on_codex_runtime(async move {
        let model = FakeModel::start(vec![Reply::Text(r#"{"line": "tidied"}"#.into())])
            .await
            .unwrap();
        let base = model.base_url();
        let f = std::sync::Arc::new(f);
        let run = f.clone();
        let out = tokio::task::spawn_blocking(move || {
            run.atomic(
                &["run", "tidy-codex", "--data", r#"{"text": "a note"}"#],
                &[
                    ("ATOMIC_MODEL_BASE_URL", base.as_str()),
                    ("ATOMIC_MODEL", "synthetic"),
                ],
            )
        })
        .await
        .unwrap();
        assert!(out.status.success(), "{}", text(&out));
        assert!(model.calls() >= 1);
        for headers in model.headers() {
            for (name, value) in headers {
                assert!(
                    !value.contains("synthetic-access-token"),
                    "the sign-in's token went to the endpoint in {name}"
                );
            }
        }
        // And an empty base URL is no endpoint: the sign-in serves, and asks
        // for a model.
        let out = f.atomic(
            &["run", "tidy-codex", "--data", r#"{"text": "a note"}"#],
            &[("ATOMIC_MODEL_BASE_URL", "")],
        );
        assert!(text(&out).contains("set ATOMIC_MODEL"), "{}", text(&out));
    });
}
