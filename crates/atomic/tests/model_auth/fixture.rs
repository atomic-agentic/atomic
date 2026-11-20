// SPDX-License-Identifier: Apache-2.0

//! The project, sign-in and `atomic` invocation the model authentication
//! suites share: a sign-in is a file store with a token no real backend
//! would take, under the test's own `ATOMIC_HOME`.

#![allow(dead_code)]

use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

pub const ATOMIC: &str = env!("CARGO_BIN_EXE_atomic");

pub struct Fixture {
    pub dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Fixture {
    pub fn new(tag: &str) -> Self {
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
    pub fn sign_in(&self) {
        self.sign_in_with("synthetic-access-token", "synthetic-refresh-token");
    }

    /// A sign-in with no token to revoke, so logging out calls no issuer.
    pub fn sign_in_unrevocable(&self) {
        self.sign_in_with("", "");
    }

    pub fn sign_in_with(&self, access: &str, refresh: &str) {
        let auth = self.dir.join("atomic-home/credentials/codex-auth");
        std::fs::create_dir_all(&auth).unwrap();
        let id_token = atomic_core::testing::fake_id_token("dev@example.invalid", "account-1");
        // Its issuer is the discard port on loopback: a refresh, on a stale
        // sign-in or a 401, fails here and reaches no one.
        std::fs::write(
            auth.join("auth.json"),
            json!({"auth_mode": "chatgpt", "OPENAI_API_KEY": null, "tokens": {
                "id_token": id_token, "access_token": access,
                "refresh_token": refresh, "account_id": "account-1",
                "oauth_provenance": {"issuer": "http://127.0.0.1:9",
                                     "client_id": "app_EMoamEEZ73f0CkXaXp7hrann"}},
                "last_refresh": "2099-01-01T00:00:00Z"})
            .to_string(),
        )
        .unwrap();
    }

    pub fn atomic(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        self.command(args, env).output().unwrap()
    }

    /// `atomic args...`, stopped if it has not finished within `secs`
    /// seconds; `None` then.
    pub fn atomic_within(&self, args: &[&str], env: &[(&str, &str)], secs: u64) -> Option<Output> {
        let mut child = self
            .command(args, env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                return Some(child.wait_with_output().unwrap());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = child.kill();
        let _ = child.wait();
        None
    }

    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
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
        c
    }
}

pub fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}
