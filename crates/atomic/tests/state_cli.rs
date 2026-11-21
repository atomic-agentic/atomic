// SPDX-License-Identifier: Apache-2.0

//! Where `ATOMIC_HOME` may not be: the person's own configuration directory,
//! or a directory inside it.
//!
//! Every atom's commands are denied `$XDG_CONFIG_HOME/atomic`, by default
//! `~/.config/atomic`, whole, and a denied path inside another denied path is
//! dropped, so a state directory there is denied with it: the worktrees and
//! caches its commands must reach fall out of their reach, and the commands
//! fail saying nothing of where the cause is. atomic refuses such a home at
//! start instead, before it creates anything in it.
//!
//! - `atomic run` and `atomic serve` refuse a home that *is* that directory,
//!   whether `HOME` gives it or `XDG_CONFIG_HOME` moves it, and one inside it,
//!   naming both directories and what to move, in text and with `--json`;
//! - a home that is a link pointing into it is refused too, since the denial
//!   follows links;
//! - a home beside it, `~/.config/atomic-state`, is accepted: the comparison
//!   is by path components, not by string.

use atomic_core::state::StateError;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const ATOMIC: &str = env!("CARGO_BIN_EXE_atomic");

/// A throwaway directory under the target directory rather than under `/tmp`,
/// where atomic's own state and the sandbox both dislike what they find (see
/// `scratch` in run_cli.rs). Its `home` is the operator's home directory, and
/// `project` a project: only its manifest, because a command refuses or
/// creates a project's state before it reads anything else in the project.
///
/// `XDG_CONFIG_HOME` is left unset, so the configuration directory every
/// atom's commands are denied is the default one, `<dir>/home/.config/atomic`.
fn scratch(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "atomic-state-cli-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["home", "project/atoms"] {
        std::fs::create_dir_all(dir.join(sub)).expect("scratch");
    }
    std::fs::write(dir.join("project/atomic.yaml"), "atomic: 1\nname: notes\n").expect("manifest");
    // atomic compares real paths, so the paths a test compares against are
    // real too.
    let dir = std::fs::canonicalize(&dir).expect("the scratch directory");
    assert!(
        !dir.join("home/.config").exists(),
        "the operator was given a configuration directory"
    );
    dir
}

/// `atomic args...` in `cwd`, as the operator `dev`, with the throwaway home
/// under `dir` and nothing of the machine's own configuration or model
/// endpoint.
fn command(dir: &Path, cwd: &Path, args: &[&str], env: &[(&str, String)]) -> Command {
    let mut c = Command::new(ATOMIC);
    c.args(args)
        .current_dir(cwd)
        .env("HOME", dir.join("home"))
        .env("USER", "dev")
        .env_remove("XDG_CONFIG_HOME")
        .env("ATOMIC_CREDENTIAL_STORE", "file")
        // No model endpoint from this machine: no Ollama, no key.
        .env("ATOMIC_DETECT_OLLAMA_ADDR", "127.0.0.1:9")
        .stdin(Stdio::null());
    for name in [
        "CODEX_HOME",
        "XDG_DATA_HOME",
        "ATOMIC_MODEL_BASE_URL",
        "ATOMIC_MODEL",
        "ATOMIC_MODEL_API_KEY_ENV",
        "ATOMIC_DATABASE_URL",
        "ATOMIC_EXECUTOR_ID",
        "ATOMIC_RUNNER",
        "ATOMIC_FAILPOINT",
        "ATOMIC_FAILPOINT_PANIC",
        "OPENAI_API_KEY",
        "CODEX_API_KEY",
        "DATABASE_URL",
    ] {
        c.env_remove(name);
    }
    for (key, value) in env {
        c.env(key, value);
    }
    c
}

fn atomic(dir: &Path, cwd: &Path, args: &[&str], env: &[(&str, String)]) -> Output {
    command(dir, cwd, args, env).output().expect("start atomic")
}

fn text(out: &Output) -> String {
    format!(
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn stdout_json(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", text(out)))
}

/// The words atomic refuses with, as its own check makes them: a message
/// changed without the directories, or the reason, in it is seen here.
fn refusal(state: &Path, config: &Path) -> String {
    StateError::inside_config(state, config).to_string()
}

/// The id atomic keys this project's state by, learned from the one home
/// these tests give it a state directory in: the id follows the project, not
/// the home.
fn project_id(dir: &Path, project: &Path) -> OsString {
    let out = atomic(
        dir,
        project,
        &["debug", "state-dir"],
        &[("ATOMIC_HOME", dir.join("plain-home").display().to_string())],
    );
    assert!(out.status.success(), "{}", text(&out));
    let state = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    state.file_name().expect("a project id").to_owned()
}

/// What the home `home` holds for the project of id `id`: the state directory
/// itself, and the private directory beside it, which the denial does not
/// cover on its own but which sits inside the same denied home.
fn state_and_private(home: &Path, id: &OsStr) -> (PathBuf, PathBuf) {
    (
        home.join("projects").join(id),
        home.join("private").join(id),
    )
}

/// `ATOMIC_HOME` set to the home `home`.
fn home_env(home: &Path) -> [(&'static str, String); 1] {
    [("ATOMIC_HOME", home.display().to_string())]
}

/// `atomic serve`, which runs until it is stopped, so here it is only
/// expected to end by refusing at start: what it printed, or a failure that
/// it is still running.
fn serve(dir: &Path, project: &Path, env: &[(&str, String)]) -> Output {
    let mut child = command(dir, project, &["serve"], env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start atomic serve");
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        if child.try_wait().expect("atomic serve").is_some() {
            return child.wait_with_output().expect("what atomic serve printed");
        }
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            panic!("`atomic serve` was still running after a minute, not refusing and stopping");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A home at `home`, which really is `real`, inside the configuration
/// directory `config`: every command that starts a project's runs refuses it,
/// and creates nothing in it. `extra` is the environment the person set
/// besides `ATOMIC_HOME`.
fn refused_at_start(
    dir: &Path,
    project: &Path,
    home: &Path,
    real: &Path,
    config: &Path,
    extra: &[(&str, String)],
) {
    let id = project_id(dir, project);
    let (state, private) = state_and_private(real, &id);
    let why = refusal(&state, config);
    let mut env = home_env(home).to_vec();
    env.extend_from_slice(extra);

    // `atomic run`, as a person types it: both directories, in full, and the
    // fix names what to move and where from.
    let out = atomic(
        dir,
        project,
        &["run", "count-note", "--data", r#"{"text": "a note"}"#],
        &env,
    );
    let t = text(&out);
    assert!(!out.status.success(), "the run was not refused: {t}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!(
            "error: {why}\n  to fix: move ATOMIC_HOME outside {}, ",
            config.display()
        )),
        "not the whole refusal for `atomic run`: {t}"
    );
    assert!(
        stderr.contains("or point XDG_CONFIG_HOME, or HOME, elsewhere"),
        "the fix does not say where else to point the configuration: {t}"
    );

    // The same failure for a script, its action a field of its own.
    let out = atomic(
        dir,
        project,
        &[
            "--json",
            "run",
            "count-note",
            "--data",
            r#"{"text": "a note"}"#,
        ],
        &env,
    );
    let t = text(&out);
    let error = stdout_json(&out)["error"].clone();
    assert_eq!(error["kind"], "state.unavailable", "{t}");
    let description = error["description"]
        .as_str()
        .unwrap_or_else(|| panic!("no description: {t}"));
    assert_eq!(
        description, why,
        "the description is not the refusal, word for word: {t}"
    );
    let action = error["action"]
        .as_str()
        .unwrap_or_else(|| panic!("no action of its own: {t}"));
    assert!(
        action.starts_with(&format!("move ATOMIC_HOME outside {}", config.display()))
            && action.contains("out of atoms' commands' reach"),
        "{action}"
    );

    // `atomic serve`, which would otherwise run on.
    let out = serve(dir, project, &env);
    let t = text(&out);
    assert!(!out.status.success(), "serve was not refused: {t}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains(&format!("error: {why}\n")),
        "not the whole refusal for `atomic serve`: {t}"
    );

    // Refused at start: nothing was created, neither the state directory nor
    // the private directory beside it nor the store inside it.
    assert!(!state.exists(), "{state:?} was created: {t}");
    assert!(!private.exists(), "{private:?} was created: {t}");
    assert!(
        !config.join("projects").exists(),
        "state was created in the configuration directory: {t}"
    );
}

#[test]
fn a_home_that_is_the_configuration_directory_is_refused() {
    let dir = scratch("equal");
    let project = dir.join("project");
    // ATOMIC_HOME is `~/.config/atomic` itself, the very directory every
    // atom's commands are denied, and nothing of it exists beforehand.
    let config = dir.join("home/.config/atomic");
    refused_at_start(&dir, &project, &config, &config, &config, &[]);
    assert!(
        !dir.join("home/.config").exists(),
        "atomic created the operator's configuration directory"
    );
}

#[test]
fn a_home_that_is_the_configuration_directory_xdg_names_is_refused() {
    let dir = scratch("xdg");
    let project = dir.join("project");
    // The same, where the person moved their configuration directory with
    // `XDG_CONFIG_HOME`, which is what the denial is made from too.
    let config = dir.join("settings/atomic");
    std::fs::create_dir_all(config.parent().expect("settings")).expect("the configuration home");
    refused_at_start(
        &dir,
        &project,
        &config,
        &config,
        &config,
        &[(
            "XDG_CONFIG_HOME",
            dir.join("settings").display().to_string(),
        )],
    );
    assert!(
        !config.exists(),
        "atomic created the configuration directory it was refusing"
    );
}

#[test]
fn a_home_inside_the_configuration_directory_is_refused() {
    let dir = scratch("inside");
    let project = dir.join("project");
    let config = dir.join("home/.config/atomic");
    std::fs::create_dir_all(&config).expect("the configuration directory");
    // A directory of its own, still inside the denied one.
    let home = config.join("state");
    refused_at_start(&dir, &project, &home, &home, &config, &[]);
}

#[test]
fn a_home_that_links_into_the_configuration_directory_is_refused() {
    let dir = scratch("link");
    let project = dir.join("project");
    let config = dir.join("home/.config/atomic");
    std::fs::create_dir_all(&config).expect("the configuration directory");
    // A home outside it that is a link pointing into it: the denial follows
    // links when it is made, so the state is denied through this one too.
    let home = dir.join("atomic-home");
    std::os::unix::fs::symlink(&config, &home).expect("the link");
    refused_at_start(&dir, &project, &home, &config, &config, &[]);
}

#[test]
fn a_home_beside_the_configuration_directory_is_accepted() {
    let dir = scratch("sibling");
    let project = dir.join("project");
    // `~/.config/atomic-state` is beside `~/.config/atomic`, not inside it:
    // compared as strings, one is the other's prefix, and this home would be
    // refused. Compared by path components, it is not.
    let home = dir.join("home/.config/atomic-state");
    let id = project_id(&dir, &project);
    let (state, private) = state_and_private(&home, &id);
    let env = home_env(&home);

    let out = atomic(&dir, &project, &["debug", "state-dir"], &env);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert_eq!(
        PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()),
        state,
        "{t}"
    );
    assert!(state.is_dir(), "the state directory is missing: {t}");
    assert!(private.is_dir(), "the private directory is missing: {t}");

    // A command that starts runs takes the home too, and gets past its state:
    // the store opens inside it, and what it refuses next is the atom it was
    // never given, not the home.
    let out = atomic(
        &dir,
        &project,
        &[
            "--json",
            "run",
            "count-note",
            "--data",
            r#"{"text": "a note"}"#,
        ],
        &env,
    );
    let t = text(&out);
    assert_eq!(
        stdout_json(&out)["error"]["kind"],
        "target.unknown",
        "a home beside the configuration directory was refused, or its store could not open: {t}"
    );
    assert!(state.is_dir(), "{state:?} is missing: {t}");
    assert!(
        private.join("atomic.db").is_file(),
        "the store was not made in the private directory: {t}"
    );
}
