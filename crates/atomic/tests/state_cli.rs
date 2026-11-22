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
//!   all, rather than running on a spelling it has not compared — and so is a
//!   *configuration* directory atomic cannot place, which leaves no home
//!   comparable with it. `atomic run` and `atomic serve` are what is run here;
//!   every command that starts a project's runs makes the same check before it
//!   creates anything, because all of them come through one function;
//! - a home beside it, `~/.config/atomic-state`, is accepted: the comparison
//!   is by path components, not by string;
//! - a *relative* `HOME` is no configuration directory for the credential
//!   store either: like settings and the denial, it gives way to the home the
//!   password database gives (or is refused when there is none), so
//!   `atomic model status` never reads the store at a spelling a working
//!   directory would complete, and creates nothing under one;
//! - a `HOME` that is not absolute gives a state home that is not absolute
//!   either — the platform's data directory under it — and such a home is
//!   placed from the directory the command was run in before it is compared
//!   with anything;
//! - the configuration directory itself may be one whose name the operating
//!   system gives as bytes that are not text: `atomic --json config --explain`
//!   says a setting comes from the `config.yaml` in that directory, which is
//!   the one directory this check, the denial of it to atoms' commands and the
//!   settings a person reads are all made from.

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

/// The person's own configuration directory: a throwaway directory that is not
/// inside any repository checkout, and that goes — with everything below it, a
/// case that ends in a failed assertion included — when the case that asked for
/// it ends.
///
/// The throwaway directories above are under the target directory, which is
/// inside this checkout, and atomic refuses to read a person's own
/// configuration that sits inside the checkout holding the project it is
/// resolving for. A stand-in for that file has to live somewhere else.
fn person_config(tag: &str) -> PersonConfig {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "atomic-state-cli-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the person's directory");
    let person = PersonConfig(std::fs::canonicalize(&dir).expect("the person's directory placed"));
    assert!(
        person.ancestors().all(|d| !d.join(".git").exists()),
        "{} is inside a repository checkout, where atomic would refuse the \
         person's own configuration",
        person.display()
    );
    person
}

/// See [`person_config`].
struct PersonConfig(PathBuf);

impl std::ops::Deref for PersonConfig {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for PersonConfig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `atomic args...` in `cwd`, as the operator `dev`, with the throwaway home
/// under `dir` and nothing of the machine's own configuration or model
/// endpoint.
fn command(dir: &Path, cwd: &Path, args: &[&str], env: &[(&str, String)]) -> Command {
    command_os(
        dir,
        cwd,
        args,
        &env.iter()
            .map(|(name, value)| (*name, OsString::from(value)))
            .collect::<Vec<_>>(),
    )
}

/// [`command`], for an environment value that is not text: a path the operating
/// system gives as bytes that are not valid UTF-8 is a path all the same, and
/// `String` cannot carry one.
fn command_os(dir: &Path, cwd: &Path, args: &[&str], env: &[(&str, OsString)]) -> Command {
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
    // A home directory that is a link elsewhere, as many people have one: the
    // configuration directory under it is the one the person's files really
    // live in, so a state directory inside it must be refused even though the
    // home atomic was given, read by string, says nothing of links. `HOME` is
    // given to the command and names the link, which is what a person's shell
    // gives it, so the state's directory and the configuration directory are
    // placed by walking through that same link to the home it leads to — and
    // meet there, inside the denial.
    //
    // What `HOME` is not given to decide is which home the password database
    // gives: this command always has a `HOME`, so the case where the *database*
    // is what names the home is the unit test's, in `state.rs`:
    // `a_process_with_no_home_in_its_environment_is_placed_by_the_password_home`.
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

/// Which of the two directories a run could not place. The words atomic is to
/// say for each: one names the directory in the refusal, another says what to
/// change for it — the state's own home, which `ATOMIC_HOME` names, or the
/// configuration directory, which `HOME` and `XDG_CONFIG_HOME` name. Both
/// spelled out here, so that an action which stopped saying which of the two
/// failed its test here instead of agreeing with whatever came back.
enum Side {
    State,
    Config,
}

impl Side {
    fn words(&self) -> &'static str {
        match self {
            Side::State => "atomic's state directory",
            Side::Config => "your configuration directory",
        }
    }

    fn fix(&self) -> &'static str {
        match self {
            Side::State => {
                "set ATOMIC_HOME to a directory atomic can place, with a directory above it \
                 that exists and can be read; until then atomic cannot tell whether its state \
                 would be inside your own configuration directory"
            }
            Side::Config => {
                "fix HOME, or XDG_CONFIG_HOME, so that your configuration directory can be \
                 placed: it, or a directory above it, cannot be entered, passes through \
                 something that is not a directory, is a link that leads nowhere, or cannot \
                 otherwise be looked up; until then atomic cannot tell whether its state \
                 would be inside that directory"
            }
        }
    }
}

/// A run refused because the directory `side` could not be placed at
/// `directory`, which is where the home `home` leads; `cause` is what the
/// filesystem said, and `absent` what the refusal left uncreated. Both the
/// person's text and a script's `--json` carry the same refusal and the same
/// action.
fn cannot_place_refused(
    dir: &Path,
    project: &Path,
    home: &Path,
    side: Side,
    directory: &Path,
    cause: &str,
    absent: &Path,
) {
    let (what, fix) = (side.words(), side.fix());
    let words = format!("cannot resolve {what} {}: ", directory.display());
    let env = home_env(home);
    let run = |json: bool| {
        let mut args: Vec<&str> = Vec::new();
        if json {
            args.push("--json");
        }
        args.extend(["run", "count-note", "--data", r#"{"text": "a note"}"#]);
        atomic(dir, project, &args, &env)
    };

    let out = run(false);
    let t = text(&out);
    assert!(!out.status.success(), "the run was not refused: {t}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!("error: {words}")),
        "not the refusal of {what}: {t}"
    );
    assert!(
        stderr.contains(cause),
        "the refusal of {what} does not say what stood in the way: {t}"
    );
    assert!(
        stderr.contains(&format!("  to fix: {fix}")),
        "the action does not say what to change for {what}: {t}"
    );

    let out = run(true);
    let t = text(&out);
    let error = stdout_json(&out)["error"].clone();
    assert_eq!(error["kind"], "state.unavailable", "{t}");
    assert_eq!(error["action"], fix, "the action of {what}: {t}");
    let description = error["description"]
        .as_str()
        .unwrap_or_else(|| panic!("no description: {t}"));
    assert!(
        description.starts_with(&words) && description.contains(cause),
        "the description is not the refusal of {what}: {t}"
    );
    assert!(!absent.exists(), "the refused home was created anyway: {t}");
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
    cannot_place_refused(
        &dir,
        &project,
        &far,
        Side::State,
        &state_home,
        &format!(
            "{} cannot be read: No such file or directory",
            far.display()
        ),
        &dir.join("far/projects"),
    );
    // The configuration directory, which then no spelling of a home can be
    // compared with: `~/.config/atomic` is a link to nowhere, so where it is
    // cannot be said. A directory nobody may look in stops the same walk as
    // surely, a step earlier and saying something different about it, but not
    // in this file: the configuration directory here is always under the home
    // the command is given, and closing that is not a home a run could be
    // started with. The unit test of the same check has that shape:
    // `state.rs`'s `a_home_behind_a_directory_nobody_may_look_in_is_refused`.
    let home = dir.join("state");
    std::fs::create_dir_all(config.parent().expect(".config")).expect("the config directory");
    std::os::unix::fs::symlink(dir.join("nowhere"), &config).expect("the dangling link");
    cannot_place_refused(
        &dir,
        &project,
        &home,
        Side::Config,
        &config,
        "cannot be read: No such file or directory",
        &home,
    );
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
fn a_relative_home_keeps_credentials_at_the_one_configuration_directory() {
    let dir = scratch("relative-home");
    let project = dir.join("project");
    // The person's `HOME` is a relative spelling. Settings, the denial, the
    // credential store and the key directory are all made of the same
    // configuration directory, and a relative `HOME` is no configuration
    // directory: it gives way to the home the password database gives.
    // `atomic model status` looks for the sign-in in the credential store,
    // creating nothing, so a command run this way says which of the two it
    // ends up with: signed out, where the password database names an
    // absolute home, and that home being unknown where it does not. What
    // it must never do is place the store at the relative spelling and
    // read it from the working directory — which it used to, and the file
    // store then refused a place that was not an absolute path; that
    // sentence is named here as the one that must not come back. Nothing
    // may be created under the relative spelling either: the store is
    // found, not made, by a command that signs no one in.
    let refusal = "the credential file store: the home directory is unknown (HOME is unset \
                   or not an absolute path, and the user has no home in the password \
                   database), so there is no configuration directory for credentials; set \
                   HOME, or set ATOMIC_HOME to keep them under it";
    let signed_out = "no one is signed in with ChatGPT";
    let env = [("HOME", "home".to_string())];

    let out = atomic(&dir, &project, &["model", "status"], &env);
    let t = text(&out);
    assert!(
        !out.status.success(),
        "`atomic model status` claimed a sign-in it was never given: {t}"
    );
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        !said.contains("is not an absolute path"),
        "the credential store was placed at the relative spelling of `HOME`, and the file \
         store refused itself there: {t}"
    );
    if said.contains("the home directory is unknown") {
        // Neither the environment nor the password database names an
        // absolute home: refused, saying which home is missing and what
        // to set.
        assert!(said.contains(refusal), "not the whole refusal: {t}");
    } else {
        // The password database names an absolute home: the sign-in is
        // looked for in its `credentials` — the same directory settings
        // and the denial are made of — and there is none.
        assert!(
            said.contains(signed_out),
            "not the signed-out answer of the one configuration directory: {t}"
        );
    }
    assert!(
        !dir.join("home/.config").exists() && !dir.join("project/home").exists(),
        "the relative spelling of `HOME` was made into a configuration directory: {t}"
    );

    // The same answer for a script, in its own form.
    let out = atomic(&dir, &project, &["--json", "model", "status"], &env);
    let t = text(&out);
    assert!(
        !out.status.success(),
        "`atomic model status` claimed a sign-in it was never given: {t}"
    );
    let error = stdout_json(&out)["error"].clone();
    let kind = error["kind"].as_str().unwrap_or("");
    let description = error["description"].as_str().unwrap_or("");
    // Which home the password database gives is this machine's; the unit
    // tests of the credential and key places pin that choice with a home
    // they make up.
    if description.contains("the home directory is unknown") {
        assert_eq!(kind, "error", "{t}");
        assert_eq!(description, refusal, "the refusal of the unknown home: {t}");
    } else {
        assert_eq!(kind, "model.signed_out", "{t}");
        assert_eq!(description, signed_out, "{t}");
    }
    assert!(
        !dir.join("home/.config").exists() && !dir.join("project/home").exists(),
        "the relative spelling of `HOME` was made into a configuration directory: {t}"
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

/// A `HOME` that is not absolute still gives the platform's data directory, so
/// it gives a state home that is not absolute either — and a run places it from
/// the directory the command was run in before it compares that home with
/// anything. `crates/atomic-core/src/state.rs`'s
/// `a_relative_state_directory_is_placed_from_the_working_directory` asks the
/// same check directly, which is what `scripts/controls/state-home-placed.row`
/// switches off and runs; this is the same control on the path a person takes,
/// which `scripts/controls/state-home-placed-cli.row` switches off and runs —
/// where the relative home is not spelled out but is what `dirs::data_dir`
/// makes of a relative `HOME`, with no `ATOMIC_HOME` and no `XDG_DATA_HOME` set.
#[test]
fn a_relative_home_gives_a_state_directory_placed_from_the_working_directory() {
    let dir = scratch("relative-home");
    let project = dir.join("project");
    // The configuration directory the state home is compared with: outside this
    // checkout, and outside the state home. It has to be named by an absolute
    // path for this test to ask anything of the placing at all — the password
    // database is the only other thing that can name one, and a test may not
    // depend on what it says about whoever runs it. With no configuration
    // directory there is nothing to compare the state home with and the check
    // never places it.
    let person = person_config("relative-home");
    let settings = person.join("settings");
    std::fs::create_dir_all(settings.join("atomic")).expect("the configuration directory");
    // The home is `home`, relative, and the command runs in `project`, so what
    // the platform's data directory rule gives is `home/.local/share/atomic`
    // read from there. Left unset: `ATOMIC_HOME`, which would be made absolute
    // before the check ever saw it, and `XDG_DATA_HOME`, which that rule takes
    // only as an absolute path and which the command's environment carries none
    // of.
    let base = project.join("home/.local/share/atomic");
    let id = project_id(&dir, &project);
    let (state, private) = state_and_private(&base, &id);
    let out = command_os(
        &dir,
        &project,
        &["debug", "state-dir"],
        &[
            ("HOME", OsString::from("home")),
            ("XDG_CONFIG_HOME", settings.into_os_string()),
        ],
    )
    .output()
    .expect("start atomic");
    let t = text(&out);
    assert!(
        out.status.success(),
        "a run whose state home is placed from the working directory was refused: {t}"
    );
    // The state is there, below the directory the command was run in: that is
    // the placed reading of the home the platform's rule gave, and it is the
    // check that compares the home with the configuration directory that asks
    // for it. A check handed the unresolved spelling refuses the run and creates
    // nothing, which is what these two assertions then say. `debug state-dir`
    // prints the spelling the platform's rule made, not a placed one, so the
    // directories are what is asked here and not stdout.
    assert!(state.is_dir(), "the state directory is missing: {t}");
    assert!(private.is_dir(), "the private directory is missing: {t}");
}

/// `XDG_CONFIG_HOME` as the operating system gives it, and not as text.
///
/// A directory whose name is bytes that are not UTF-8 is a directory all the
/// same, and the `config.yaml` in it is the file a person's settings come from.
/// Settings, the denial of that directory to every atom's commands and the
/// refusal of a state directory inside it are made from one resolution, which
/// reads the variables as the operating system gives them; reading the variable
/// as text sees nothing here, and would take the settings from
/// `~/.config/atomic/config.yaml` — a directory no atom is denied — while the
/// denial and the state check covered this one. What is pinned beside it, in
/// `crates/atomic/src/settings/tests.rs`, is only the wiring — that a person's
/// run asks the state's own resolution; here it is the command a person runs,
/// whose output carries a path that is not text.
#[cfg(target_os = "linux")]
#[test]
fn a_setting_in_a_configuration_directory_of_bytes_that_are_not_text_is_sourced_from_it() {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let dir = scratch("odd-bytes");
    let project = dir.join("project");
    // The person's own configuration directory cannot be under the throwaway
    // directory, because that is inside this checkout and atomic refuses a
    // person's own configuration inside the checkout holding the project. So it
    // is a directory of its own, outside any repository, below a name ending in
    // a byte that is not valid UTF-8: no text reading of `XDG_CONFIG_HOME` can
    // name it, and the plain name it is built from is never made.
    let person = person_config("odd-bytes");
    let mut raw = person.join("xdg").into_os_string().into_vec();
    raw.push(0xc3);
    let xdg = PathBuf::from(OsStr::from_bytes(&raw));
    let config = xdg.join("atomic");
    std::fs::create_dir_all(&config).expect("the configuration directory of odd bytes");
    std::fs::write(
        config.join("config.yaml"),
        "model:\n  name: from-a-directory-of-odd-bytes\n",
    )
    .expect("the person's own configuration");

    let out = command_os(
        &dir,
        &project,
        &["--json", "config", "--explain"],
        &[("XDG_CONFIG_HOME", OsString::from(OsStr::from_bytes(&raw)))],
    )
    .output()
    .expect("start atomic");
    let t = text(&out);
    assert!(out.status.success(), "the configuration was not read: {t}");
    let doc = stdout_json(&out);
    let row = doc["settings"]
        .as_array()
        .expect("the settings atomic printed")
        .iter()
        .find(|setting| setting["setting"] == "model.name")
        .unwrap_or_else(|| panic!("atomic printed no model.name: {t}"));
    // The person's own file, word for word — with the byte that is not text
    // shown as U+FFFD, which is how atomic shows a path it cannot show as
    // text, and what the JSON of it carries. A setting that came from
    // anywhere else, a default above all, is the two readings parting.
    let shown = format!("{}:2", config.join("config.yaml").display());
    assert_eq!(
        row["source"].as_str(),
        Some(shown.as_str()),
        "model.name did not come from the configuration directory of odd bytes: {row}\n{t}"
    );
    assert_eq!(
        row["value"].as_str(),
        Some("from-a-directory-of-odd-bytes"),
        "model.name is not what that file holds: {row}\n{t}"
    );
}
