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
use std::path::{Path, PathBuf};
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
    write_codex_atom(project, "peek", "read-only");
    write_codex_atom(project, "tidy", "workspace-write");
    // `tidy` may write, so it runs as a worktree step at a base commit;
    // `peek` runs as one too, and may not write; `tidy-twice` runs `tidy`
    // in two worktrees, the second on the first's commit.
    std::fs::create_dir_all(project.join("workflows")).unwrap();
    for (name, atom) in [("tidy-up", "tidy"), ("peek-in", "peek")] {
        std::fs::write(
            project.join(format!("workflows/{name}.workflow.yaml")),
            format!(
                "atomic: 1\nname: {name}\n\
                 input: {{ $ref: ../atoms/{atom}.input.schema.json }}\n\
                 output: {{ $ref: ../atoms/{atom}.output.schema.json }}\n\
                 steps:\n  step:\n    atom: {atom}\n    with: {{ base: input.base, text: input.text }}\n    \
                 worktree: true\n\
                 returns: steps.step.output\n"
            ),
        )
        .unwrap();
    }
    std::fs::write(
        project.join("workflows/tidy-twice.workflow.yaml"),
        "atomic: 1\nname: tidy-twice\n\
         input: { $ref: ../atoms/tidy.input.schema.json }\n\
         output: { $ref: ../atoms/tidy.output.schema.json }\n\
         steps:\n  first:\n    atom: tidy\n    with: { base: input.base, text: input.text }\n    \
         worktree: true\n\
         \x20 second:\n    after: [first]\n    atom: tidy\n    \
         with: { base: steps.first.metadata.commit, text: input.text }\n    worktree: true\n\
         returns: steps.second.output\n",
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
    let input = if name != "look" {
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

/// Where [`run_steps`] keeps the atomic home for `target`.
fn atomic_home_for(project: &Path, target: &str) -> PathBuf {
    // Tests run at once, so each project's run has its own.
    let test = project
        .parent()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "atomic-shell-env-{}-{target}-{test}",
        std::process::id()
    ))
}

/// Run `target` of `project` through `atomic run` as a person with home
/// `home` and the variables `env` would, on a model whose turns each run
/// one of `commands`, and return what each printed.
async fn run_steps(
    home: &Path,
    project: &Path,
    target: &str,
    data: &str,
    commands: &[String],
    env: &[(&str, &str)],
) -> Vec<String> {
    run_ending(home, project, target, data, commands, env, true).await
}

/// [`run_steps`], with each turn answering as it should when `succeeds`,
/// or with what its atom's output schema refuses, so the run fails.
async fn run_ending(
    home: &Path,
    project: &Path,
    target: &str,
    data: &str,
    commands: &[String],
    env: &[(&str, &str)],
    succeeds: bool,
) -> Vec<String> {
    let answer = if succeeds {
        r#"{"done": true}"#
    } else {
        r#"{"done": "no"}"#
    };
    let replies = commands
        .iter()
        .flat_map(|command| {
            [
                Reply::Tool {
                    name: "exec_command".into(),
                    arguments: json!({"cmd": command, "yield_time_ms": 20000}),
                },
                Reply::Text(answer.into()),
            ]
        })
        .chain(
            (!succeeds)
                .then(|| (0..8).map(|_| Reply::Text(answer.into())))
                .into_iter()
                .flatten(),
        )
        .collect();
    let model = FakeModel::start(replies).await.unwrap();
    let atomic_home = atomic_home_for(project, target);
    let _ = std::fs::remove_dir_all(&atomic_home);
    let mut run = Command::new(env!("CARGO_BIN_EXE_atomic"));
    // Every workflow here runs a worktree step, so it is bound to the
    // project's own repository as its fixture.
    run.args(["run", target, "--data", data])
        .arg("--fixture")
        .arg(project)
        .args(["--fixture-rev", "main"])
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
        "RUSTUP_HOME",
    ] {
        run.env_remove(name);
    }
    for (name, value) in env {
        run.env(name, value);
    }
    let at = atomic_home.with_file_name(format!(
        "{}-run",
        atomic_home.file_name().unwrap().to_string_lossy()
    ));
    let out = tokio::task::spawn_blocking(move || bounded(run, &at))
        .await
        .unwrap();
    assert_eq!(
        out.status.success(),
        succeeds,
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let requests = model.requests();
    assert!(
        requests.len() >= 2 * commands.len(),
        "{} model calls; stderr: {}",
        requests.len(),
        String::from_utf8_lossy(&out.stderr)
    );
    (0..commands.len())
        .map(|i| {
            requests[2 * i + 1]["input"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| item["type"] == "function_call_output")
                .map(|item| item["output"].as_str().unwrap_or_default().to_owned())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect()
}

/// [`run_steps`] for a workflow of one step.
async fn run_once(
    home: &Path,
    project: &Path,
    target: &str,
    data: &str,
    command: &str,
    env: &[(&str, &str)],
) -> String {
    run_steps(home, project, target, data, &[command.to_owned()], env)
        .await
        .remove(0)
}

/// The value of `name=` that a command printed on a line of its own.
fn printed_value(output: &str, name: &str) -> String {
    output
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{name}=")))
        .unwrap_or_default()
        .to_owned()
}

/// The directory of the one run's build caches, under `target`'s atomic
/// home, from a path inside it that a command printed.
fn run_caches(project: &Path, target: &str, inside: &str) -> PathBuf {
    let path = Path::new(inside);
    let at = path
        .ancestors()
        .find(|a| {
            a.parent()
                .and_then(Path::file_name)
                .is_some_and(|n| n == "cache")
        })
        .unwrap_or_else(|| panic!("{inside} is in no run's caches"))
        .to_owned();
    assert!(at.starts_with(atomic_home_for(project, target)), "{inside}");
    at
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
        std::fs::create_dir_all(home.join(".rustup")).unwrap();
        let project = dir.join("project");
        write_project(&project);
        let base = git(&project);
        let command = format!(
            "ls -A \"$HOME\"; echo listed; find \"$HOME\" -type l; echo linked; \
             for d in {PROTECTED}; do ls -A \"$HOME/$d\" 2>/dev/null; done; echo protected; \
             mkdir -p \"$HOME/.cache\" && echo cached > \"$HOME/.cache/x\" && cat \"$HOME/.cache/x\"; \
             cat {0}/.ssh/id_ed25519 {0}/.profile; echo \"home=$HOME\"; \
             echo \"cargo=$CARGO_HOME\"; echo \"rustup=$RUSTUP_HOME\"; echo \"gomod=$GOMODCACHE\"; \
             echo \"target=$CARGO_TARGET_DIR\"; \
             mkdir -p \"$XDG_CACHE_HOME\" && touch \"$XDG_CACHE_HOME/w\" && echo cache-written; \
             touch \"$RUSTUP_HOME/w\" 2>/dev/null && echo rustup-written; \
             mkdir -p \"$GOMODCACHE/m@v1\" && touch \"$GOMODCACHE/m@v1/go.mod\" && \
             chmod 0500 \"$GOMODCACHE/m@v1\" \"$GOMODCACHE\"; \
             echo looked",
            home.display(),
            PROTECTED = PROTECTED.join(" ")
        );
        let data = json!({"base": base, "text": "the note"}).to_string();
        let printed = run_once(&home, &project, "tidy-up", &data, &command, &[]).await;
        let output = printed
            .split_once("Output:\n")
            .map_or(printed.as_str(), |(_, o)| o);
        // It starts empty but for the sandbox's own read-only placeholders,
        // which are empty too, and the turn's own temporary directory, with
        // no link anywhere in it.
        let (listing, rest) = output.split_once("listed\n").expect("listed");
        assert!(
            listing
                .lines()
                .all(|name| PROTECTED.contains(&name) || name == "tmp"),
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
        // A worktree step's tools find its run's own caches, which it may
        // write and which go when the run ends; cargo's home is its own
        // home's; and the toolchains rustup has, which it may only read.
        let value = |name: &str| printed_value(rest, name);
        assert_eq!(
            value("cargo"),
            format!("{}/.cargo", value("home")),
            "{printed}"
        );
        let caches = run_caches(&project, "tidy-up", &value("gomod"));
        assert_eq!(Path::new(&value("gomod")), caches.join("go-mod"));
        assert!(
            Path::new(&value("target")).starts_with(caches.join("cargo-target")),
            "{printed}"
        );
        // Gone, read-only directories and all.
        assert!(!caches.exists(), "the run's caches outlived it");
        assert_eq!(
            value("rustup"),
            home.join(".rustup").display().to_string(),
            "{printed}"
        );
        assert!(rest.contains("cache-written"), "{printed}");
        assert!(!rest.contains("rustup-written"), "{printed}");
        assert!(rest.contains("looked"), "{printed}");
        // RUSTUP_HOME, where the person sets it.
        let toolchains = dir.join("toolchains");
        std::fs::create_dir_all(&toolchains).unwrap();
        let toolchains = toolchains.display().to_string();
        let printed = run_once(
            &home,
            &project,
            "tidy-up",
            &data,
            "echo \"rustup=$RUSTUP_HOME\"; touch \"$RUSTUP_HOME/w\" 2>/dev/null && echo rustup-written",
            &[("RUSTUP_HOME", &toolchains)],
        )
        .await;
        assert_eq!(printed_value(&printed, "rustup"), toolchains, "{printed}");
        assert!(!printed.contains("rustup-written"), "{printed}");
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn a_read_only_worktree_step_finds_its_caches_and_writes_nothing_there() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = std::env::temp_dir().join(format!("atomic-peek-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let project = dir.join("project");
        write_project(&project);
        let base = git(&project);
        let data = json!({"base": base, "text": "the note"}).to_string();
        let command = "echo \"xdg=$XDG_CACHE_HOME\"; \
                       mkdir -p \"$XDG_CACHE_HOME/x\" 2>/dev/null && echo cache-written; \
                       mkdir -p \"$CARGO_TARGET_DIR\" 2>/dev/null && echo target-written; echo looked";
        let printed = run_once(&home, &project, "peek-in", &data, command, &[]).await;
        run_caches(&project, "peek-in", &printed_value(&printed, "xdg"));
        assert!(printed.contains("looked"), "{printed}");
        for wrote in ["cache-written", "target-written"] {
            assert!(!printed.contains(wrote), "{wrote}: {printed}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn a_run_that_fails_leaves_no_build_caches() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = std::env::temp_dir().join(format!("atomic-failed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let project = dir.join("project");
        write_project(&project);
        let base = git(&project);
        let data = json!({"base": base, "text": "the note"}).to_string();
        let build = "mkdir -p \"$XDG_CACHE_HOME\" && echo built > \"$XDG_CACHE_HOME/x\"; \
                     echo \"xdg=$XDG_CACHE_HOME\"";
        let printed = run_ending(
            &home,
            &project,
            "tidy-up",
            &data,
            &[build.to_owned()],
            &[],
            false,
        )
        .await;
        let caches = run_caches(&project, "tidy-up", &printed_value(&printed[0], "xdg"));
        assert!(!caches.exists(), "a failed run's caches stayed: {caches:?}");
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn each_worktree_builds_in_a_target_directory_of_its_own() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = std::env::temp_dir().join(format!("atomic-twice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let project = dir.join("project");
        write_project(&project);
        let base = git(&project);
        let data = json!({"base": base, "text": "the note"}).to_string();
        let build = "mkdir -p \"$CARGO_TARGET_DIR\" && ls -A \"$CARGO_TARGET_DIR\"; echo listed; \
                     echo built > \"$CARGO_TARGET_DIR/built-here\"; echo \"target=$CARGO_TARGET_DIR\"";
        let printed = run_steps(
            &home,
            &project,
            "tidy-twice",
            &data,
            &[build.to_owned(), build.to_owned()],
            &[],
        )
        .await;
        let (first, second) = (&printed[0], &printed[1]);
        let output = |p: &str| p.split_once("Output:\n").map_or(p, |(_, o)| o).to_owned();
        let (listing, _) = output(second)
            .split_once("listed\n")
            .map(|(l, r)| (l.to_owned(), r.to_owned()))
            .expect("listed");
        assert_eq!(
            listing.trim(),
            "",
            "the second worktree found the first's build: {second}"
        );
        let (one, two) = (
            printed_value(first, "target"),
            printed_value(second, "target"),
        );
        assert_ne!(one, two, "{first}\n{second}");
        assert_eq!(
            run_caches(&project, "tidy-twice", &one),
            run_caches(&project, "tidy-twice", &two)
        );
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
        let at = atomic_home.with_file_name(format!(
            "{}-run",
            atomic_home.file_name().unwrap().to_string_lossy()
        ));
        let out = tokio::task::spawn_blocking(move || bounded(command, &at))
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

/// How long one `atomic run` here may take before the test fails instead of
/// waiting on it: the longest, a workflow of several worktree steps, takes
/// under a minute.
const RUN_LIMIT: std::time::Duration = std::time::Duration::from_secs(300);

/// Run `run` to its end and return how it ended and what it printed, kept
/// in `<at>.stdout` and `<at>.stderr` rather than pipes, so nothing it
/// leaves running can hold the test. A run that has not ended within
/// [`RUN_LIMIT`] is sent SIGKILL, with the process group it leads, and
/// fails the test with what it printed; a command it started in a session
/// of its own is not in that group.
fn bounded(mut run: Command, at: &Path) -> std::process::Output {
    let file = |ext: &str| PathBuf::from(format!("{}.{ext}", at.display()));
    let (stdout, stderr) = (file("stdout"), file("stderr"));
    run.stdout(std::fs::File::create(&stdout).unwrap())
        .stderr(std::fs::File::create(&stderr).unwrap());
    std::os::unix::process::CommandExt::process_group(&mut run, 0);
    let mut child = run.spawn().unwrap();
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if started.elapsed() > RUN_LIMIT {
            // The child leads its own group, so the group's id is its pid.
            // Signalled before it is reaped, so the id is still its own.
            if let Ok(group) = libc::pid_t::try_from(child.id()) {
                // SAFETY: killpg only sends a signal; a group that is gone
                // is ESRCH.
                #[allow(unsafe_code)]
                unsafe {
                    libc::killpg(group, libc::SIGKILL);
                }
            }
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    let (stdout, stderr) = (
        std::fs::read(&stdout).unwrap_or_default(),
        std::fs::read(&stderr).unwrap_or_default(),
    );
    let Some(status) = status else {
        panic!(
            "the run had not ended after {RUN_LIMIT:?}, so it was killed: stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
    };
    std::process::Output {
        status,
        stdout,
        stderr,
    }
}

/// Run `look` once through `atomic run` with home `home`, its command
/// `command`, and return what the command printed, failing the test if the
/// run does not end in time ([`bounded`]). Its atomic home is the test's
/// own, named after `dir`:
/// the tests that call this run at once, and each removes its home first.
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
    let atomic_home = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "atomic-shell-env-{}",
        dir.file_name().unwrap().to_string_lossy()
    ));
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
    let at = dir.join("look");
    let out = tokio::task::spawn_blocking(move || bounded(run, &at))
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&atomic_home);
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

/// A credential place kept with the person's dotfiles on a file system
/// that marks every file executable stays denied: it is no program.
#[test]
fn a_linked_secret_marked_executable_stays_denied() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = std::env::temp_dir().join(format!("atomic-marked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home");
        std::fs::create_dir_all(home.join("dotfiles")).unwrap();
        std::fs::write(
            home.join("dotfiles/netrc"),
            "machine example.com login dev password canary-marked\n",
        )
        .unwrap();
        std::fs::set_permissions(
            home.join("dotfiles/netrc"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink(home.join("dotfiles/netrc"), home.join(".netrc")).unwrap();
        let command = format!(
            "cat {h}/.netrc {h}/dotfiles/netrc; echo looked",
            h = home.display()
        );
        let printed = look_once(&dir, &home, &command, &[]).await;
        assert!(printed.contains("looked"), "{printed}");
        assert!(
            !printed.contains("canary"),
            "the marked secret was read: {printed}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn a_secret_the_system_keeps_or_a_pipe_stays_denied_and_the_persons_programs_run() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = std::env::temp_dir().join(format!("atomic-kept-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home");
        std::fs::create_dir_all(home.join(".kube")).unwrap();
        std::fs::create_dir_all(home.join(".config/fish/bin")).unwrap();
        // A cluster's credentials kept by the system, root's own, outside
        // the system's program directories: here /etc/hostname stands in.
        let kept = Path::new("/etc/hostname");
        let secret = std::fs::read_to_string(kept).expect("a file the system keeps");
        assert!(!secret.trim().is_empty());
        std::os::unix::fs::symlink(kept, home.join(".kube/config")).unwrap();
        // A credential served through a pipe, which nothing writes to.
        let fifo =
            std::ffi::CString::new(home.join(".netrc").into_os_string().into_encoded_bytes())
                .unwrap();
        // SAFETY: mkfifo reads a NUL-terminated path and makes a pipe there.
        #[allow(unsafe_code)]
        let made = unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) };
        assert_eq!(made, 0, "mkfifo: {}", std::io::Error::last_os_error());
        // A program of the person's own, linked from inside a denied
        // directory, as a virtual environment's python is.
        std::fs::create_dir_all(dir.join("tools")).unwrap();
        std::fs::write(dir.join("tools/python3"), "#!/bin/sh\necho program-ran\n").unwrap();
        std::fs::set_permissions(
            dir.join("tools/python3"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            dir.join("tools/python3"),
            home.join(".config/fish/bin/python3"),
        )
        .unwrap();
        let command = format!(
            "cat {h}/.kube/config /etc/hostname; timeout 3 cat {h}/.netrc; echo \"pipe=$?\"; \
             {d}/tools/python3; echo looked",
            h = home.display(),
            d = dir.display()
        );
        let printed = look_once(&dir, &home, &command, &[]).await;
        assert!(printed.contains("looked"), "{printed}");
        assert!(
            !printed.contains(secret.trim()),
            "the kept secret was read: {printed}"
        );
        // Refused at once (1), not waited on until the time ran out (124).
        assert!(
            printed.lines().any(|l| l.trim() == "pipe=1"),
            "the pipe was opened: {printed}"
        );
        assert!(printed.contains("program-ran"), "{printed}");
        let _ = std::fs::remove_dir_all(&dir);
    });
}

/// Credential places named through a directory that is an absolute link, as
/// `/var/run` links to `/run`, stay denied under both spellings, and every
/// command still starts: a projected volume reached by a link to its token's
/// link, the version it rotates in next, an item nested in one of its
/// directories, and a moved key directory.
#[test]
fn credentials_behind_an_absolutely_linked_directory_stay_denied() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = std::env::temp_dir().join(format!("atomic-aws-linked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let (real, linked) = (dir.join("real"), dir.join("linked"));
        std::fs::create_dir_all(real.join("gnupg")).unwrap();
        std::fs::write(real.join("gnupg/key"), "canary-gnupg\n").unwrap();
        std::os::unix::fs::symlink(&real, &linked).unwrap();
        let now = "..2026_01_01_00_00_00.1";
        let next = "..2026_01_01_01_00_00.2";
        for volume in ["sa", "nested"] {
            for version in [now, next] {
                let at = real.join(volume).join(version);
                std::fs::create_dir_all(at.join("aws")).unwrap();
                std::fs::write(at.join("token"), format!("canary-{volume}{version}\n")).unwrap();
                std::fs::write(at.join("aws/token"), format!("canary-{volume}-aws\n")).unwrap();
            }
            std::os::unix::fs::symlink(now, real.join(volume).join("..data")).unwrap();
            std::os::unix::fs::symlink("..data/token", real.join(volume).join("token")).unwrap();
            std::os::unix::fs::symlink("..data/aws", real.join(volume).join("aws")).unwrap();
        }
        std::fs::create_dir_all(dir.join("etc")).unwrap();
        std::os::unix::fs::symlink(linked.join("sa/token"), dir.join("etc/token")).unwrap();
        let mut reads = vec![dir.join("etc/token")];
        for root in [&real, &linked] {
            reads.push(root.join("gnupg/key"));
            for volume in ["sa", "nested"] {
                for item in ["token", "aws/token"] {
                    reads.push(root.join(volume).join(item));
                    for version in [now, next] {
                        reads.push(root.join(volume).join(version).join(item));
                    }
                }
            }
        }
        let reads: Vec<String> = reads.iter().map(|p| p.display().to_string()).collect();
        let command = format!("cat {} 2>/dev/null; echo looked", reads.join(" "));
        let gnupg = linked.join("gnupg").display().to_string();
        let web = dir.join("etc/token").display().to_string();
        let nested = linked.join("nested/aws/token").display().to_string();
        let printed = look_once(
            &dir,
            &home,
            &command,
            &[
                ("GNUPGHOME", gnupg.as_str()),
                ("AWS_WEB_IDENTITY_TOKEN_FILE", web.as_str()),
                ("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE", nested.as_str()),
            ],
        )
        .await;
        assert!(printed.contains("looked"), "the command ran: {printed}");
        assert!(
            !printed.contains("canary"),
            "a credential was read: {printed}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    });
}

/// AWS's workload token files stay denied to a turn's commands, whether a
/// variable in atomic's environment names an ordinary file or a token in a
/// projected volume, as Kubernetes mounts one: the token there now, the
/// file behind its link, and the next version of it, which Kubernetes
/// rotates in, are all out of reach; the rest of the machine is not.
#[test]
fn aws_workload_token_files_stay_denied_whether_plain_or_mounted() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = std::env::temp_dir().join(format!("atomic-aws-token-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        // An ordinary token file.
        std::fs::create_dir_all(dir.join("tokens")).unwrap();
        std::fs::write(dir.join("tokens/web-identity"), "canary-web-identity\n").unwrap();
        // A projected volume: the token a link through `..data` to the
        // current version, and the version the next rotation brings.
        let volume = dir.join("serviceaccount");
        let now = "..2026_01_01_00_00_00.1";
        let next = "..2026_01_01_01_00_00.2";
        for (version, canary) in [(now, "canary-pod-identity"), (next, "canary-next-token")] {
            std::fs::create_dir_all(volume.join(version)).unwrap();
            std::fs::write(volume.join(version).join("token"), format!("{canary}\n")).unwrap();
        }
        std::os::unix::fs::symlink(now, volume.join("..data")).unwrap();
        std::os::unix::fs::symlink("..data/token", volume.join("token")).unwrap();
        std::fs::write(dir.join("tokens/readme"), "readable-beside\n").unwrap();
        let command = format!(
            "cat {d}/tokens/web-identity {v}/token {v}/{now}/token {v}/{next}/token; \
             cat {d}/tokens/readme; echo looked",
            d = dir.display(),
            v = volume.display()
        );
        let web = dir.join("tokens/web-identity").display().to_string();
        let pod = volume.join("token").display().to_string();
        let printed = look_once(
            &dir,
            &home,
            &command,
            &[
                ("AWS_WEB_IDENTITY_TOKEN_FILE", web.as_str()),
                ("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE", pod.as_str()),
            ],
        )
        .await;
        assert!(printed.contains("looked"), "{printed}");
        assert!(
            printed.contains("readable-beside"),
            "a file beside an ordinary token stays readable: {printed}"
        );
        for canary in [
            "canary-web-identity",
            "canary-pod-identity",
            "canary-next-token",
        ] {
            assert!(!printed.contains(canary), "{canary} was read: {printed}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    });
}

/// Codex's file helper, which applies a turn's patches under the turn's
/// sandbox, keeps that sandbox's registry of mount points where the turn's
/// commands keep theirs: in the backend's shared directory in atomic's
/// state, not in the `TMPDIR` atomic was given.
#[test]
fn a_turns_patches_keep_their_sandbox_registry_with_its_commands() {
    support::require_sandbox();
    on_codex_runtime(patch_keeps_its_registry("registry", false));
}

/// The same with `ATOMIC_HOME` given relative to the directory atomic
/// starts in: the file helper, which Codex starts elsewhere, still finds
/// the shared directory, and the patch lands.
#[test]
fn a_relative_atomic_home_keeps_patches_and_their_registry_working() {
    support::require_sandbox();
    on_codex_runtime(patch_keeps_its_registry("relative-home", true));
}

/// Apply a patch in a worktree step through `atomic run`, given another
/// `TMPDIR`, and with its state under an `ATOMIC_HOME` relative to the
/// project when `relative`: it lands, and the one registry is in the
/// backend's shared directory in the state.
async fn patch_keeps_its_registry(tag: &str, relative: bool) {
    let dir = std::env::temp_dir().join(format!("atomic-patch-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let home = dir.join("home");
    let elsewhere = dir.join("elsewhere");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    let project = dir.join("project");
    write_project(&project);
    let base = git(&project);
    let data = json!({"base": base, "text": "the note"}).to_string();
    let patch = "apply_patch <<'EOF'\n*** Begin Patch\n*** Add File: patched.txt\n\
                 +patched\n*** End Patch\nEOF\n";
    let tmpdir = elsewhere.to_string_lossy().into_owned();
    let state = if relative {
        Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("atomic-patch-{tag}-{}", std::process::id()))
    } else {
        atomic_home_for(&project, "tidy-up")
    };
    let _ = std::fs::remove_dir_all(&state);
    // The state's path from the project's directory, where atomic starts.
    let up = "../".repeat(project.components().count() - 1);
    let from_project = format!("{up}{}", state.strip_prefix("/").unwrap().display());
    let mut env = vec![("TMPDIR", tmpdir.as_str())];
    if relative {
        env.push(("ATOMIC_HOME", from_project.as_str()));
    }
    let printed = run_once(&home, &project, "tidy-up", &data, patch, &env).await;
    assert!(
        printed.contains("Success. Updated the following files") && printed.contains("patched.txt"),
        "{printed}"
    );
    let uid = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(&home).unwrap());
    let registry = format!("codex-bwrap-synthetic-mount-targets-{uid}");
    assert!(
        !elsewhere.join(&registry).exists(),
        "the file helper kept its registry in the TMPDIR atomic was given"
    );
    let shared: Vec<PathBuf> = found(&state, &registry)
        .into_iter()
        .map(|r| r.parent().unwrap().to_owned())
        .collect();
    assert!(
        shared.len() == 1 && shared[0].ends_with("scratch/.tmp"),
        "registries: {shared:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&state);
}

/// Every directory named `name` under `dir`.
fn found(dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            if entry.file_name() == name {
                found.push(path);
            } else {
                found.extend(self::found(&path, name));
            }
        }
    }
    found
}
