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
//! configuration file, nor the file a linked one points to, nor any of the
//! places in the home directory where credentials are kept, and none of
//! the functions or aliases the person's shell startup files define reach
//! it.
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

/// The entries of [`atomic_core::codex::HOME_SECRETS`] that are directories;
/// the rest are files.
const SECRET_DIRS: &[&str] = &[
    ".ssh",
    ".aws",
    ".codex",
    ".config/gcloud",
    ".azure",
    ".kube",
    ".gnupg",
    ".config/gh",
    ".password-store",
    ".local/share/keyrings",
    ".config/fish",
    ".local/share/fish",
    ".claude",
    ".bashrc.d",
    ".zsh_sessions",
];

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
        // Every place credentials are kept, holding a canary: a file, or a
        // directory with a file in it.
        let mut secrets = Vec::new();
        for path in atomic_core::codex::HOME_SECRETS {
            let at = home.join(path);
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            if SECRET_DIRS.contains(path) {
                std::fs::create_dir_all(&at).unwrap();
                std::fs::write(at.join("secret"), "canary-secret\n").unwrap();
            } else {
                std::fs::write(&at, "canary-secret\n").unwrap();
            }
            secrets.push(at);
        }
        for profile in PROFILES {
            std::fs::write(
                home.join(profile),
                format!(
                    "export PROFILE_CANARY_PASSWORD=canary-profile-{profile}\n\
                     canary_fn() {{ echo canary-function; }}\n\
                     alias canary_alias='echo canary-alias'\n"
                ),
            )
            .unwrap();
        }
        // The person's own configuration, which only they may read, linked
        // to a file kept elsewhere.
        std::fs::create_dir_all(dir.join("kept")).unwrap();
        let kept = dir.join("kept/config.yaml");
        std::fs::write(&kept, "# canary-config\nrunner: local\n").unwrap();
        let config = home.join(".config/atomic/config.yaml");
        std::os::unix::fs::symlink(&kept, &config).unwrap();
        let reads: String = secrets
            .iter()
            .chain([&config, &kept])
            .map(|p| format!("cat '{0}' '{0}/secret';", p.display()))
            .collect();
        let command = format!("env; {reads} type canary_fn; alias; echo looked");
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
        assert!(
            !printed.contains("canary-secret"),
            "the command read a credential in the home directory: {printed}"
        );
        assert!(
            !printed.contains("canary-function")
                && !printed.contains("canary-alias")
                && !printed.contains("canary_fn ()"),
            "the shell's startup functions or aliases reached the command: {printed}"
        );
        for name in CANARIES {
            assert!(
                !printed.contains(&format!("{name}="))
                    && !printed.contains(&format!("canary-{}", name.to_lowercase())),
                "{name} reached the atom's shell: {printed}"
            );
        }
    });
}

/// Run `look` once through `atomic run` with home `home`, its command
/// `command`, and return what the command printed.
async fn look_once(dir: &Path, home: &Path, command: &str) -> String {
    let model = FakeModel::start(vec![
        Reply::Tool {
            name: "exec_command".into(),
            arguments: json!({"cmd": command, "yield_time_ms": 20000}),
        },
        Reply::Text(r#"{"done": true}"#.into()),
    ])
    .await
    .unwrap();
    let project = dir.join("project");
    write_project(&project);
    let atomic_home = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-shell-env-linked-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&atomic_home);
    let mut run = Command::new(env!("CARGO_BIN_EXE_atomic"));
    run.args(["run", "look", "--data", r#"{"text": "the note"}"#])
        .current_dir(&project)
        .env("HOME", home)
        .env("XDG_DATA_HOME", &atomic_home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("ATOMIC_DETECT_OLLAMA_ADDR", "127.0.0.1:9")
        .env("ATOMIC_MODEL_BASE_URL", model.base_url())
        .env("ATOMIC_MODEL", "synthetic")
        .stdin(Stdio::null());
    for name in [
        "CODEX_HOME",
        "ATOMIC_HOME",
        "ATOMIC_DATABASE_URL",
        "ATOMIC_EXECUTOR_ID",
    ] {
        run.env_remove(name);
    }
    let out = tokio::task::spawn_blocking(move || run.output().unwrap())
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    model.requests()[1]["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "function_call_output")
        .map(|item| item["output"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_secret_kept_behind_a_link_stays_denied_and_commands_still_run() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = std::env::temp_dir().join(format!("atomic-linked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home");
        // A startup file kept with the person's dotfiles, a key directory
        // linked from elsewhere, and a credential two levels inside a
        // denied directory that is a link to a file outside it.
        std::fs::create_dir_all(home.join("dotfiles")).unwrap();
        std::fs::write(home.join("dotfiles/bashrc"), "canary-linked-file\n").unwrap();
        std::os::unix::fs::symlink(home.join("dotfiles/bashrc"), home.join(".bashrc")).unwrap();
        std::fs::create_dir_all(dir.join("keys")).unwrap();
        std::fs::write(dir.join("keys/id_ed25519"), "canary-linked-dir\n").unwrap();
        std::os::unix::fs::symlink(dir.join("keys"), home.join(".ssh")).unwrap();
        std::fs::create_dir_all(home.join(".aws/deep")).unwrap();
        std::fs::create_dir_all(dir.join("elsewhere")).unwrap();
        std::fs::write(dir.join("elsewhere/credentials"), "canary-deep-link\n").unwrap();
        std::os::unix::fs::symlink(
            dir.join("elsewhere/credentials"),
            home.join(".aws/deep/credentials"),
        )
        .unwrap();
        let command = format!(
            "cat {h}/.bashrc {h}/dotfiles/bashrc {h}/.ssh/id_ed25519 {d}/keys/id_ed25519 \
             {h}/.aws/deep/credentials {d}/elsewhere/credentials; echo looked",
            h = home.display(),
            d = dir.display()
        );
        let printed = look_once(&dir, &home, &command).await;
        assert!(printed.contains("looked"), "the command ran: {printed}");
        assert!(
            !printed.contains("canary"),
            "a linked secret was read: {printed}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    });
}
