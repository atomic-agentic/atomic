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
//!   naming `ATOMIC_HOME` as it was set, both directories as they really are
//!   and what to set instead, in text and with `--json`;
//! - a home that is a link pointing into it is refused too, since the denial
//!   follows links, and so is one that `..` leads into it, whether the names
//!   it steps over are there yet or not, and one that is relative and lands in
//!   it once read from the directory the command was run in;
//! - so is a home where no `ATOMIC_HOME` points, because the default data
//!   directory is the configuration directory, and one atomic cannot place at
//!   all, rather than running on a spelling it has not compared;
//! - a home beside it, `~/.config/atomic-state`, is accepted: the comparison
//!   is by path components, not by string.

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
        "ATOMIC_HOME",
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

/// A home, inside the configuration directory `config`: every command that
/// starts a project's runs refuses it, and creates nothing in it.
///
/// `home` is what `ATOMIC_HOME` is set to, or `None` to leave it unset and let
/// the default data directory give the home; `real` is where that home really
/// is, once links are followed and `..` folded. `extra` is the environment the
/// person set besides `ATOMIC_HOME`.
fn refused_at_start(
    dir: &Path,
    project: &Path,
    home: Option<&Path>,
    real: &Path,
    config: &Path,
    extra: &[(&str, String)],
) {
    let id = project_id(dir, project);
    let (state, private) = state_and_private(real, &id);
    // The whole sentence, spelled out here rather than asked of atomic's own
    // `Display`, so a message that loses a directory, the reason or the fix is
    // seen here and not waved through by agreeing with itself.
    let why = match home {
        Some(home) => format!(
            "ATOMIC_HOME is set to {home}, which puts atomic's state directory {state} inside \
             {config}, your own configuration directory, which every atom's commands are denied",
            home = home.display(),
            state = state.display(),
            config = config.display(),
        ),
        None => format!(
            "ATOMIC_HOME is not set to a directory, so atomic's state directory {state} is \
             inside {config}, your own configuration directory, which every atom's commands \
             are denied",
            state = state.display(),
            config = config.display(),
        ),
    };
    let fix = format!(
        "set ATOMIC_HOME to a directory outside {}, or point XDG_CONFIG_HOME, or HOME, \
         elsewhere; a worktree or a cache inside your own configuration directory is out of \
         atoms' commands' reach",
        config.display()
    );
    let mut env: Vec<(&str, String)> = home
        .map(|home| vec![("ATOMIC_HOME", home.display().to_string())])
        .unwrap_or_default();
    env.extend_from_slice(extra);

    // `atomic run`, as a person types it: the home as they set it and as it
    // really is, the configuration directory, and the fix.
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
        stderr.contains(&format!("error: {why}\n  to fix: {fix}")),
        "not the whole refusal for `atomic run`: {t}"
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
    assert_eq!(action, fix, "the action is not the fix, word for word: {t}");

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
    refused_at_start(&dir, &project, Some(&config), &config, &config, &[]);
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
        Some(&config),
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
    refused_at_start(&dir, &project, Some(&home), &home, &config, &[]);
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
    refused_at_start(&dir, &project, Some(&home), &config, &config, &[]);
}

#[test]
fn a_home_the_configuration_directory_steps_over_is_refused() {
    let dir = scratch("dotdot-missing");
    let project = dir.join("project");
    // `..` is folded against the directories that really are there before the
    // two are compared, so a home spelled beside the configuration directory
    // that walks back into it is refused like the home it lands on. Here the
    // name it walks over, `atomic-state`, is nothing at all, and `atomic` is
    // the denied directory: resolved as a string it says `atomic-state`, which
    // is beside the denial, and the run would go on with its state inside it.
    let config = dir.join("home/.config/atomic");
    std::fs::create_dir_all(&config).expect("the configuration directory");
    let home = dir.join("home/.config/atomic-state/../atomic");
    refused_at_start(&dir, &project, Some(&home), &config, &config, &[]);
    assert!(
        !dir.join("home/.config/atomic-state").exists(),
        "atomic created the directory the refusal walked over"
    );
}

#[test]
fn a_home_that_walks_back_into_the_configuration_directory_is_refused() {
    let dir = scratch("dotdot");
    let project = dir.join("project");
    // The same fold with every name it steps over already there:
    // `~/.config/atomic/../atomic/state` is `~/.config/atomic/state`, inside
    // the denial, whatever the string says.
    let config = dir.join("home/.config/atomic");
    std::fs::create_dir_all(&config).expect("the configuration directory");
    let real = config.join("state");
    let home = dir.join("home/.config/atomic/../atomic/state");
    refused_at_start(&dir, &project, Some(&home), &real, &config, &[]);
}

#[test]
fn a_relative_home_inside_the_configuration_directory_is_refused() {
    let dir = scratch("relative");
    // What a relative `ATOMIC_HOME` names is read from the directory the
    // person ran atomic in, so the project sits inside the configuration
    // directory to put the command there: run from it, the home `state` is a
    // directory inside the denial, and the first run is where that shows.
    let config = dir.join("home/.config/atomic");
    std::fs::create_dir_all(config.join("proj/atoms")).expect("a project in the configuration");
    std::fs::write(config.join("proj/atomic.yaml"), "atomic: 1\nname: notes\n").expect("manifest");
    // The project as atomic resolves it, which is how the refusal says where
    // the state would go. On this machine a path may be a link, and the
    // scratch directory's own spelling would not be what is compared.
    let config = std::fs::canonicalize(config).expect("the configuration directory");
    let project = config.join("proj");
    refused_at_start(
        &dir,
        &project,
        Some(Path::new("state")),
        &project.join("state"),
        &config,
        &[],
    );
}

#[test]
fn a_home_replacing_a_link_named_home_is_refused() {
    let dir = scratch("passwd-home");
    let project = dir.join("project");
    // A home directory that is a link elsewhere, as many people have one. With
    // `HOME` unset atomic still denies the configuration directory under the
    // home the password database gives, so a state directory inside it is
    // denied with it and is refused here too. The link stands where the
    // password database says this user's home is, which is why `HOME` is not
    // given to the command at all; where the database says something else, the
    // run cannot get that far, so the refusal is checked with `HOME` naming the
    // same directory, which is the same walk of `..` and links.
    let target = dir.join("real-home");
    std::fs::create_dir_all(target.join(".config/atomic")).expect("the configuration directory");
    let link = dir.join("home");
    std::fs::remove_dir(&link).expect("the empty home the scratch made");
    std::os::unix::fs::symlink(&target, &link).expect("the link for a home");
    let config = target.join(".config/atomic");
    refused_at_start(&dir, &project, Some(&config), &config, &config, &[]);
}

#[test]
fn a_home_inside_a_linked_configuration_directory_is_refused() {
    let dir = scratch("config-link");
    let project = dir.join("project");
    // The person keeps their configuration where they please and links
    // `~/.config/atomic` to it. The denial is made of the directory the link
    // leads to, so the state there is denied too, and the refusal says where
    // it really is rather than the link's name.
    let settings = dir.join("settings/atomic");
    std::fs::create_dir_all(&settings).expect("the settings directory");
    let config = dir.join("home/.config/atomic");
    std::fs::create_dir_all(config.parent().expect(".config")).expect("the config directory");
    std::os::unix::fs::symlink(&settings, &config).expect("the link");
    let home = settings.join("state");
    refused_at_start(&dir, &project, Some(&home), &home, &settings, &[]);
}

#[test]
fn a_home_the_default_data_directory_leads_to_is_refused() {
    let dir = scratch("default-home");
    let project = dir.join("project");
    // No `ATOMIC_HOME` at all: the state goes to the platform's data
    // directory, and a person who has pointed that at where their
    // configuration lives has their state inside the denial without ever
    // setting atomic's home. The refusal has no `ATOMIC_HOME` to blame, so it
    // says that nothing set it and tells the person to set one.
    let settings = dir.join("settings");
    std::fs::create_dir_all(&settings).expect("the settings directory");
    let config = settings.join("atomic");
    refused_at_start(
        &dir,
        &project,
        None,
        &config,
        &config,
        &[
            ("XDG_CONFIG_HOME", settings.display().to_string()),
            ("XDG_DATA_HOME", settings.display().to_string()),
        ],
    );
}

#[test]
fn a_home_atomic_cannot_place_is_refused() {
    let dir = scratch("unplaced");
    let project = dir.join("project");
    let config = dir.join("home/.config/atomic");
    let id = project_id(&dir, &project);
    // Where a home leads cannot always be said: a link among the directories
    // its name is read through can point at nothing at all. Running on the
    // spelling atomic was given would run on a path atomic has not compared
    // with the denial, so atomic stops instead, and says which of its two
    // directories it could not place and what the filesystem said.
    std::fs::create_dir_all(config.parent().expect(".config")).expect("the config directory");
    let far = dir.join("far");
    std::os::unix::fs::symlink(dir.join("nowhere"), &far).expect("the link to nowhere");
    let state_home = state_and_private(&far, &id).0;
    let out = atomic(
        &dir,
        &project,
        &["run", "count-note", "--data", r#"{"text": "a note"}"#],
        &home_env(&far),
    );
    let t = text(&out);
    assert!(
        !out.status.success(),
        "a home atomic could not place ran anyway: {t}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!(
            "error: cannot resolve atomic's state directory {}: ",
            state_home.display()
        )),
        "not the refusal of an unplaceable state directory: {t}"
    );
    assert!(
        stderr.contains(&format!("{} cannot be read: ", far.display()))
            && stderr.contains("No such file or directory"),
        "the refusal does not say what stood in the way: {t}"
    );
    assert!(
        !dir.join("far/projects").exists(),
        "the unplaceable home was created anyway: {t}"
    );
    // The configuration directory, which then no spelling of a home can be
    // compared with: `~/.config/atomic` is a link to nowhere, so where it is
    // cannot be said. (A directory nobody may look in stops the same lookup a
    // step earlier; the unit test of the same check covers that shape, which
    // the person's own unreadable configuration would also stop at.)
    let home = dir.join("state");
    std::fs::create_dir_all(config.parent().expect(".config")).expect("the config directory");
    std::os::unix::fs::symlink(dir.join("nowhere"), &config).expect("the dangling link");
    let out = atomic(
        &dir,
        &project,
        &["run", "count-note", "--data", r#"{"text": "a note"}"#],
        &home_env(&home),
    );
    let t = text(&out);
    assert!(
        !out.status.success(),
        "a home beside an unplaceable configuration directory ran anyway: {t}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!(
            "error: cannot resolve your configuration directory {}: ",
            config.display()
        )),
        "not the refusal of an unplaceable configuration directory: {t}"
    );
    assert!(
        stderr.contains("cannot be read:") && stderr.contains("No such file or directory"),
        "the refusal does not say what stood in the way: {t}"
    );
    assert!(!home.exists(), "the home was created anyway: {t}");
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

#[test]
fn a_relative_xdg_config_home_gives_way_to_home() {
    let dir = scratch("relative-xdg");
    let project = dir.join("project");
    // A relative `XDG_CONFIG_HOME` names no configuration directory, as the
    // XDG rule says, and gives way to `HOME`: that is the directory the denial
    // is made of and this check made against. So a home outside the
    // configuration directory under `HOME` runs, whatever a string reading of
    // the relative variable would call the configuration directory.
    let home = dir.join("data");
    let id = project_id(&dir, &project);
    let (state, private) = state_and_private(&home, &id);
    let out = atomic(
        &dir,
        &project,
        &["debug", "state-dir"],
        &[
            ("ATOMIC_HOME", home.display().to_string()),
            ("XDG_CONFIG_HOME", "settings".to_string()),
        ],
    );
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert_eq!(
        PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()),
        state,
        "{t}"
    );
    assert!(state.is_dir(), "the state directory is missing: {t}");
    assert!(private.is_dir(), "the private directory is missing: {t}");
}
