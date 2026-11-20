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

/// The project: `look`, a read-only codex atom, and `tidy`, one that may
/// write its workspace.
fn write_project(project: &Path) {
    std::fs::create_dir_all(project.join("atoms")).unwrap();
    std::fs::write(project.join("atomic.yaml"), "atomic: 1\nname: env-check\n").unwrap();
    write_codex_atom(project, "look", "read-only");
    write_codex_atom(project, "tidy", "workspace-write");
    // `tidy` may write, so it runs as a worktree step at a base commit.
    std::fs::create_dir_all(project.join("workflows")).unwrap();
    std::fs::write(
        project.join("workflows/tidy-up.workflow.yaml"),
        "atomic: 1\nname: tidy-up\n\
         input: { $ref: ../atoms/tidy.input.schema.json }\n\
         output: { $ref: ../atoms/tidy.output.schema.json }\n\
         steps:\n  tidy:\n    atom: tidy\n    with: { base: input.base, text: input.text }\n    \
         worktree: true\n\
         returns: steps.tidy.output\n",
    )
    .unwrap();
}

fn write_codex_atom(project: &Path, name: &str, sandbox: &str) {
    std::fs::write(
        project.join(format!("atoms/{name}.atom.astro")),
        format!(
            "---\nimport type {{ Props, Input, Output }} from '../.atomic/types/{name}';\n\
             import type {{ AtomConfig }} from '../.atomic/types/authoring';\n\
             const {{ input }}: Props = Astro.props;\n\
             export const config: AtomConfig<Input, Output> = {{ description: 'Look around.', \
             kind: 'codex', sandbox: '{sandbox}' }};\n\
             ---\nLook around: {{input.text}}\n"
        ),
    )
    .unwrap();
    let input = if name == "tidy" {
        json!({"type": "object", "additionalProperties": false, "required": ["base", "text"],
               "properties": {"base": {"type": "string", "format": "git-commit"},
                              "text": {"type": "string"}}})
    } else {
        json!({"type": "object", "additionalProperties": false, "required": ["text"],
               "properties": {"text": {"type": "string"}}})
    };
    for (role, schema) in [
        ("input", input),
        (
            "output",
            json!({"type": "object", "additionalProperties": false, "required": ["done"],
                   "properties": {"done": {"type": "boolean"}}}),
        ),
    ] {
        std::fs::write(
            project.join(format!("atoms/{name}.{role}.schema.json")),
            schema.to_string(),
        )
        .unwrap();
    }
}

/// Run atom `atom` of `project` through `atomic run` as a person with home
/// `home` would, on `model`, and return what its one command printed.
async fn run_once(home: &Path, project: &Path, target: &str, data: &str, command: &str) -> String {
    let model = FakeModel::start(vec![
        Reply::Tool {
            name: "exec_command".into(),
            arguments: json!({"cmd": command, "yield_time_ms": 20000}),
        },
        Reply::Text(r#"{"done": true}"#.into()),
    ])
    .await
    .unwrap();
    let atomic_home = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-shell-env-{}-{target}", std::process::id()));
    let _ = std::fs::remove_dir_all(&atomic_home);
    let mut run = Command::new(env!("CARGO_BIN_EXE_atomic"));
    run.args(["run", target, "--data", data])
        .current_dir(project)
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
        "ATOMIC_MODEL_API_KEY_ENV",
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
fn a_turn_writes_a_home_of_its_own_and_still_reads_nothing_of_the_persons() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = std::env::temp_dir().join(format!("atomic-turn-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh/id_ed25519"), "canary-key\n").unwrap();
        std::fs::write(home.join(".profile"), "export TOKEN=canary-profile\n").unwrap();
        let project = dir.join("project");
        write_project(&project);
        let base = git(&project);
        let command = format!(
            "ls -A \"$HOME\"; echo listed; find \"$HOME\" -type l; echo linked; \
             for d in {PROTECTED}; do ls -A \"$HOME/$d\" 2>/dev/null; done; echo protected; \
             mkdir -p \"$HOME/.cache\" && echo cached > \"$HOME/.cache/x\" && cat \"$HOME/.cache/x\"; \
             cat {0}/.ssh/id_ed25519 {0}/.profile; echo \"home=$HOME\"; \
             echo \"cargo=$CARGO_HOME\"; echo \"rustup=$RUSTUP_HOME\"; echo \"gomod=$GOMODCACHE\"; \
             mkdir -p \"$CARGO_HOME\" && touch \"$CARGO_HOME/w\" && echo cache-written; \
             echo looked",
            home.display(),
            PROTECTED = PROTECTED.join(" ")
        );
        let data = json!({"base": base, "text": "the note"}).to_string();
        let printed = run_once(&home, &project, "tidy-up", &data, &command).await;
        let output = printed
            .split_once("Output:\n")
            .map_or(printed.as_str(), |(_, o)| o);
        // It starts empty but for the sandbox's own read-only placeholders,
        // which are empty too, with no link anywhere in it.
        let (listing, rest) = output.split_once("listed\n").expect("listed");
        assert!(
            listing.lines().all(|name| PROTECTED.contains(&name)),
            "the turn's home started with: {listing}"
        );
        let (links, rest) = rest.split_once("linked\n").expect("linked");
        assert_eq!(links.trim(), "", "links in the turn's home: {links}");
        let (inside, rest) = rest.split_once("protected\n").expect("protected");
        assert_eq!(inside.trim(), "", "the placeholders hold: {inside}");
        // The turn may write it.
        assert!(rest.contains("cached\n"), "{printed}");
        // And reads nothing of the person's.
        assert!(!rest.contains("canary"), "{printed}");
        assert!(
            !rest.contains(&format!("home={}\n", home.display())),
            "{printed}"
        );
        // A worktree step's tools find the project's shared caches, which it
        // may write, and the toolchains rustup has.
        let value = |name: &str| {
            rest.lines()
                .find_map(|l| l.strip_prefix(&format!("{name}=")))
                .unwrap_or_default()
                .to_owned()
        };
        for name in ["cargo", "gomod"] {
            assert!(value(name).contains("/cache/"), "{name}: {printed}");
        }
        assert!(!value("rustup").is_empty(), "{printed}");
        assert!(rest.contains("cache-written"), "{printed}");
        assert!(rest.contains("looked"), "{printed}");
        let _ = std::fs::remove_dir_all(&dir);
    });
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

/// The names Codex's sandbox keeps read-only, and empty, in a directory a
/// turn may write, such as its home.
const PROTECTED: &[&str] = &[".agents", ".aws", ".codex", ".git"];

/// Make `project` a repository with one commit, and return it.
fn git(project: &Path) -> String {
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(["-c", "user.name=dev", "-c", "user.email=dev@example.com"])
            .args(["-c", "init.defaultBranch=main"])
            .args(args)
            .current_dir(project)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "project"]);
    git(&["rev-parse", "HEAD"])
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
async fn look_once(dir: &Path, home: &Path, command: &str, env: &[(&str, &str)]) -> String {
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
    for (name, value) in env {
        run.env(name, value);
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
        // The person's own Codex home, moved, which atomic replaces with
        // its own for the turn.
        std::fs::create_dir_all(dir.join("their-codex")).unwrap();
        std::fs::write(dir.join("their-codex/auth.json"), "canary-codex-auth\n").unwrap();
        let command = format!(
            "cat {h}/.bashrc {h}/dotfiles/bashrc {h}/.ssh/id_ed25519 {d}/keys/id_ed25519 \
             {h}/.aws/deep/credentials {d}/elsewhere/credentials {d}/their-codex/auth.json; \
             echo looked",
            h = home.display(),
            d = dir.display()
        );
        let theirs = dir.join("their-codex").display().to_string();
        let printed = look_once(&dir, &home, &command, &[("CODEX_HOME", &theirs)]).await;
        assert!(printed.contains("looked"), "the command ran: {printed}");
        assert!(
            !printed.contains("canary"),
            "a linked secret was read: {printed}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn a_secret_place_linked_to_a_device_or_a_system_program_leaves_both_usable() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = std::env::temp_dir().join(format!("atomic-unlinked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(home.join(".config/fish/bin")).unwrap();
        // History kept nowhere, hosts thrown away, a variable naming
        // /dev/null, and a tool in a denied directory linked to the
        // system's python.
        let null = Path::new("/dev/null");
        std::os::unix::fs::symlink(null, home.join(".bash_history")).unwrap();
        std::os::unix::fs::symlink(null, home.join(".ssh/known_hosts")).unwrap();
        let python = std::fs::canonicalize("/usr/bin/python3").unwrap();
        std::os::unix::fs::symlink(&python, home.join(".config/fish/bin/python3")).unwrap();
        let command = "echo x > /dev/null && echo null-written; cat /dev/null && echo null-read; \
                       /usr/bin/python3 -c 'print(\"python-ran\")'; echo looked";
        let printed = look_once(&dir, &home, command, &[("HISTFILE", "/dev/null")]).await;
        for ran in ["null-written", "null-read", "python-ran", "looked"] {
            assert!(printed.contains(ran), "{ran}: {printed}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    });
}
