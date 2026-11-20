// SPDX-License-Identifier: Apache-2.0

//! A Codex atom's shell sees none of the person's secrets.
//!
//! The model chooses the commands an atom's shell runs, and the model is
//! whatever endpoint atomic was given, so whatever the shell can see, the
//! endpoint can read. `atomic run` runs a Codex atom, through atomic's own
//! entry point and under Codex's Linux sandbox, whose scripted model runs
//! `env`, with canary values in atomic's environment for the variables that
//! carry credentials: model and cloud keys, tokens, atomic's and Codex's own
//! settings and database connections, and its shell profiles exporting a
//! variable no pattern names. None reaches the command, and the platform's
//! core variables do. The command cannot read the person's own
//! configuration file either.
//!
//! This covers what the shell inherits and its configuration; files a
//! command can read elsewhere in the home directory are another matter.
//!
//! Needs bubblewrap and unprivileged user namespaces, so it runs with the
//! `sandbox-tests` feature (and `ATOMIC_DEV_SANDBOX=on` in the development
//! container); with the feature on, a sandbox that cannot start fails it.

#![cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#![allow(clippy::unwrap_used)]

mod support;

use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use serde_json::json;
use std::path::Path;
use std::process::{Command, Stdio};

/// Variables that carry credentials, each set to a value holding `canary`.
const CANARIES: &[&str] = &[
    "OPENAI_API_KEY",
    "OPENAI_ORGANIZATION",
    "ANTHROPIC_API_KEY",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "GITHUB_TOKEN",
    "MY_SECRET",
    "PGPASSWORD",
    "DATABASE_URL",
    "CODEX_CANARY",
    "ATOMIC_CANARY",
    "UNRELATED_CANARY",
];

fn write_project(project: &Path) {
    std::fs::create_dir_all(project.join("atoms")).unwrap();
    std::fs::write(project.join("atomic.yaml"), "atomic: 1\nname: env-check\n").unwrap();
    std::fs::write(
        project.join("atoms/look.atom.astro"),
        "---\nimport type { Props, Input, Output } from '../.atomic/types/look';\n\
         import type { AtomConfig } from '../.atomic/types/authoring';\n\
         const { input }: Props = Astro.props;\n\
         export const config: AtomConfig<Input, Output> = { description: 'Look around.', \
         kind: 'codex', sandbox: 'read-only' };\n\
         ---\nLook around: {input.text}\n",
    )
    .unwrap();
    for (role, schema) in [
        (
            "input",
            json!({"type": "object", "additionalProperties": false, "required": ["text"],
                   "properties": {"text": {"type": "string"}}}),
        ),
        (
            "output",
            json!({"type": "object", "additionalProperties": false, "required": ["done"],
                   "properties": {"done": {"type": "boolean"}}}),
        ),
    ] {
        std::fs::write(
            project.join(format!("atoms/look.{role}.schema.json")),
            schema.to_string(),
        )
        .unwrap();
    }
}

/// The person's shell profiles, each exporting a variable whose name no
/// exclusion pattern matches.
const PROFILES: &[&str] = &[".profile", ".bash_profile", ".bashrc", ".zshenv"];

#[test]
fn an_atoms_shell_sees_none_of_the_persons_secrets() {
    support::require_sandbox();
    on_codex_runtime(async {
        // The home and the project outside any checkout, since a personal
        // configuration inside the checkout holding the project is refused;
        // atomic's state outside the system temporary directory, where a
        // sandboxed turn may write, and found as a person's is, without
        // ATOMIC_HOME, which brings denials of its own.
        let dir = std::env::temp_dir().join(format!("atomic-shell-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let atomic_home = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("atomic-shell-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&atomic_home);
        let home = dir.join("home");
        std::fs::create_dir_all(home.join(".config/atomic")).unwrap();
        for profile in PROFILES {
            std::fs::write(
                home.join(profile),
                format!("export PROFILE_CANARY_PASSWORD=canary-profile-{profile}\n"),
            )
            .unwrap();
        }
        // The person's own configuration, which only they may read.
        let config = home.join(".config/atomic/config.yaml");
        std::fs::write(&config, "# canary-config\nrunner: local\n").unwrap();
        let command = format!("env; cat {}; echo looked", config.display());
        let model = FakeModel::start(vec![
            Reply::Tool {
                name: "exec_command".into(),
                arguments: json!({"cmd": command, "yield_time_ms": 20000}),
            },
            // A login shell, asked for outright.
            Reply::Tool {
                name: "exec_command".into(),
                arguments: json!({"cmd": "echo logged-in", "login": true, "yield_time_ms": 20000}),
            },
            Reply::Text(r#"{"done": true}"#.into()),
        ])
        .await
        .unwrap();
        let project = dir.join("project");
        write_project(&project);
        let mut command = Command::new(env!("CARGO_BIN_EXE_atomic"));
        command
            .args(["run", "look", "--data", r#"{"text": "the note"}"#])
            .current_dir(&project)
            .env("HOME", dir.join("home"))
            .env("XDG_DATA_HOME", &atomic_home)
            .env("XDG_CONFIG_HOME", dir.join("home/.config"))
            .env("ATOMIC_DETECT_OLLAMA_ADDR", "127.0.0.1:9")
            .env("ATOMIC_MODEL_BASE_URL", model.base_url())
            .env("ATOMIC_MODEL", "synthetic")
            .stdin(Stdio::null());
        for name in [
            "CODEX_HOME",
            "ATOMIC_HOME",
            "ATOMIC_DATABASE_URL",
            "ATOMIC_EXECUTOR_ID",
            "ATOMIC_MODEL_API_KEY_ENV",
        ] {
            command.env_remove(name);
        }
        for name in CANARIES {
            command.env(name, format!("canary-{}", name.to_lowercase()));
        }
        let out = tokio::task::spawn_blocking(move || command.output().unwrap())
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let requests = model.requests();
        let output_of = |n: usize| -> String {
            requests.get(n).expect("Codex answered the tool call")["input"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| item["type"] == "function_call_output")
                .map(|item| item["output"].as_str().unwrap_or_default().to_owned())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let printed = output_of(1);
        // The next request carries both outputs; the login shell is refused.
        let login = output_of(2).replace(&printed, "");
        assert!(
            !login.contains("logged-in") && login.contains("login shell is disabled"),
            "a login shell ran: {login}"
        );
        // The command ran, and saw the core variables.
        assert!(printed.contains("PATH="), "{printed}");
        // A home of its own, never the person's.
        assert!(printed.contains("HOME="), "{printed}");
        assert!(
            !printed.contains(&format!("HOME={}\n", home.display())),
            "{printed}"
        );
        assert!(printed.contains("looked"), "{printed}");
        assert!(
            !printed.contains("PROFILE_CANARY"),
            "a shell profile ran: {printed}"
        );
        assert!(
            !printed.contains("canary-config"),
            "the command read the person's configuration: {printed}"
        );
        for name in CANARIES {
            assert!(
                !printed.contains(&format!("{name}=")) && !printed.contains("canary"),
                "{name} reached the atom's shell: {printed}"
            );
        }
    });
}
