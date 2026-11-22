// SPDX-License-Identifier: Apache-2.0

//! Running `git` where an atom may have written: the only place atomic runs
//! git.
//!
//! A worktree an atom has written to is untrusted input. It can hold a planted
//! `.git` directory, nested repositories, attributes naming drivers, and files
//! shaped like options. So every git command here is built the same way:
//!
//! - **Nothing in the worktree decides where the repository is.** The git
//!   directory and the work tree are always given explicitly (`GIT_DIR`,
//!   `GIT_WORK_TREE`), so a `.git` file or directory the atom planted is never
//!   read; repository discovery happens only in the project's own directory,
//!   before any atom runs.
//! - **Nothing in the repository or the environment is run.** The environment
//!   is cleared, system and global config are off, and every option that could
//!   run a program (hooks, fsmonitor, pagers, editors, external diff and
//!   text-conversion drivers, signing, credential helpers, ssh) is set on the
//!   command line, which beats any config file the repository holds, and is
//!   passed on to any git that git itself starts. Network transports are
//!   refused and lazy fetches are off.
//! - **Arguments are checked before git sees them.** Commits are hex, refs
//!   are plain names checked here, and anything that could look like an
//!   option comes after `--end-of-options`.
//! - **Each command has a time limit and a cap on its output,** and blocks
//!   the calling thread: async callers use `tokio::task::spawn_blocking`.
//!
//! git runs as a separate program, never linked, and is found only at fixed
//! paths, never through `PATH`.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

/// Where git may be, in order. The first that exists is used.
const GIT_PROGRAMS: &[&str] = &[
    "/usr/bin/git",
    "/usr/local/bin/git",
    "/opt/homebrew/bin/git",
];

/// How long one git command may take, by default. A checkout of a large
/// repository is the slowest thing asked of it. An assumption.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// The most output a command may give before it is stopped.
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

/// How much of git's error output is kept for an error message.
const STDERR_BYTES: usize = 4096;

/// Settings given on every command line. The command line beats every config
/// file, the repository's own included, and git passes these on to any git it
/// starts (a submodule's, say).
const HARDENING: &[&str] = &[
    // Programs git would run.
    "core.hooksPath=/dev/null",
    "core.fsmonitor=false",
    "core.pager=cat",
    "core.editor=/bin/false",
    "sequence.editor=/bin/false",
    "core.askPass=/bin/false",
    "core.sshCommand=/bin/false",
    "credential.helper=",
    "gpg.program=/bin/false",
    "gpg.ssh.program=/bin/false",
    "gpg.x509.program=/bin/false",
    "commit.gpgSign=false",
    "tag.gpgSign=false",
    "log.showSignature=false",
    "core.attributesFile=/dev/null",
    "core.excludesFile=/dev/null",
    // No network, and nothing that reaches into other repositories.
    "protocol.allow=never",
    "submodule.recurse=false",
    "fetch.recurseSubmodules=false",
    "diff.ignoreSubmodules=all",
    "status.submoduleSummary=false",
    // No background work and no surprises in what is read back.
    "gc.auto=0",
    "maintenance.auto=false",
    "core.untrackedCache=false",
    "core.splitIndex=false",
    "core.quotePath=false",
    "color.ui=never",
    "advice.detachedHead=false",
    // A worktree's record names it by absolute path, which atomic checks.
    "worktree.useRelativePaths=false",
    "i18n.logOutputEncoding=UTF-8",
];

/// Read a git's stderr on a thread of its own: the start of it is kept for
/// an error message, and the rest drained so git never blocks on a full pipe.
fn keep_errors(mut stderr: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = (&mut stderr)
            .take(STDERR_BYTES as u64)
            .read_to_end(&mut bytes);
        let _ = std::io::copy(&mut stderr, &mut std::io::sink());
        bytes
    })
}

/// Why a git command failed or was never run.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// No git program at any of the fixed paths.
    #[error("no git program found at {}", GIT_PROGRAMS.join(", "))]
    NoGit,
    /// git ran and said no.
    #[error("git {what} failed (exit code {code:?}): {stderr}")]
    Failed {
        /// The subcommand.
        what: String,
        /// Its exit code, if it exited rather than being killed.
        code: Option<i32>,
        /// The start of what it printed on stderr, with control characters
        /// replaced. Untrusted text.
        stderr: String,
    },
    /// git took longer than its limit and was stopped.
    #[error("git took longer than {0:?}, so it was stopped")]
    Timeout(Duration),
    /// git printed more than the cap and was stopped.
    #[error("git's output passed {0} bytes, so it was stopped")]
    TooLarge(usize),
    /// git could not be started.
    #[error("could not run git: {0}")]
    Spawn(#[from] std::io::Error),
    /// An argument failed its check, so git was never run.
    #[error("{0}")]
    Invalid(&'static str),
    /// git's output was not in the form asked for.
    #[error("git's output could not be read")]
    Unreadable,
}

impl GitError {
    /// Whether this is git running and saying no, as distinct from git, or
    /// the file system under it, failing before it answered at all.
    ///
    /// Asked about a directory, git says no both when it holds no repository
    /// and when it holds one git cannot read, and it says little about which.
    /// atomic leaves that difference unproven rather than pick the news it
    /// likes, and refuses either way.
    pub fn refused(&self) -> bool {
        matches!(self, GitError::Failed { .. })
    }
}

/// What a directory is to git (see [`Git::checkout_at`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkout {
    /// The checkout's own git directory.
    pub git_dir: PathBuf,
    /// The repository's common git directory, which holds its objects.
    pub common_dir: PathBuf,
    /// The top of the work tree, `None` for a bare repository.
    pub top: Option<PathBuf>,
}

/// One entry of a commit's tree (see [`Repo::entries`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    /// Its mode, such as `100644`.
    pub mode: String,
    /// `blob`, `tree` or `commit` (a submodule).
    pub kind: String,
    /// Its object ID.
    pub id: String,
    /// Its path from the tree's top.
    pub path: String,
}

/// A whole commit ID: 40 hex digits for SHA-1 or 64 for SHA-256, in lower
/// case.
pub fn is_sha(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Part of a commit ID, from 7 hex digits to a whole one, in lower case.
pub fn is_sha_prefix(text: &str) -> bool {
    (7..=64).contains(&text.len()) && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A path inside a repository: relative, with no `.` or `..` part, no empty
/// part, no control characters and no leading `-`.
pub fn is_git_path(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 4096
        && !text.starts_with('-')
        && !text.chars().any(char::is_control)
        && !text
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

/// A branch, tag or other ref name as `git check-ref-format` allows, more
/// strictly: ASCII letters, digits and `._/-`, no part that starts with `.`
/// or ends with `.lock`, no `..`, and no leading `-`. No revision syntax
/// (`~`, `^`, `@{`, `:`) gets through.
pub fn is_ref_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 255
        && !text.starts_with('-')
        && !text.contains("..")
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
        && text
            .split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
        && !text.ends_with('.')
}

/// The git program, and how long each command may take.
#[derive(Clone, Debug)]
pub struct Git {
    program: PathBuf,
    timeout: Duration,
}

impl Git {
    /// git at the first fixed path where it exists.
    pub fn find() -> Result<Self, GitError> {
        GIT_PROGRAMS
            .iter()
            .map(Path::new)
            .find(|p| p.is_file())
            .map(Self::at)
            .ok_or(GitError::NoGit)
    }

    /// git at `program`.
    pub fn at(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// The same git, with another time limit per command.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The program.
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// The repository whose git directory is `git_dir`, with no work tree.
    pub fn repo(&self, git_dir: impl Into<PathBuf>) -> Repo {
        Repo {
            git: self.clone(),
            git_dir: git_dir.into(),
            work_tree: None,
            overrides: Arc::default(),
        }
    }

    /// Make an empty bare repository at `path`, from no template, so no
    /// sample hook or other file comes with it.
    pub fn init_bare(&self, path: &Path) -> Result<Repo, GitError> {
        let mut command = self.base();
        command
            .args(["init", "--bare", "--quiet", "--template="])
            .arg(path);
        self.run(command, "init", None, MAX_OUTPUT_BYTES)?;
        Ok(self.repo(path))
    }

    /// The repository whose git directory is `git_dir`, checked out at
    /// `work_tree`. Commands never look in `work_tree` for a git directory.
    pub fn worktree(&self, git_dir: impl Into<PathBuf>, work_tree: impl Into<PathBuf>) -> Repo {
        Repo {
            git: self.clone(),
            git_dir: git_dir.into(),
            work_tree: Some(work_tree.into()),
            overrides: Arc::default(),
        }
    }

    /// The repository `dir` is in, found the way git finds it, as its common
    /// git directory (shared by all of its worktrees). Only for a directory
    /// atomic trusts, such as the project root, never for a worktree an atom
    /// wrote to. The same hardening applies, and every filter driver the
    /// repository's config defines is switched off for every later command
    /// on it (see [`Repo::without_filters`]).
    pub fn discover(&self, dir: &Path) -> Result<Repo, GitError> {
        self.find_repo(dir)?.without_filters()
    }

    /// The root of the checkout containing `dir`, including a linked
    /// checkout whose common git directory belongs to another worktree.
    /// Like discovery, this is for a trusted source directory.
    pub fn worktree_root(&self, dir: &Path) -> Result<PathBuf, GitError> {
        self.path_at(dir, "--show-toplevel")
    }

    /// What `dir` is to git, found the way git finds it, for a trusted
    /// directory: the checkout's own git directory (a linked worktree's is
    /// not its repository's), the repository's common one, and the top of
    /// its work tree, `None` for a bare repository.
    pub fn checkout_at(&self, dir: &Path) -> Result<Checkout, GitError> {
        let bare = {
            let mut command = self.base();
            let mut safe = OsString::from("safe.directory=");
            safe.push(dir);
            command.arg("-c").arg(safe).current_dir(dir);
            command.args(["rev-parse", "--is-bare-repository"]);
            self.run(command, "rev-parse", None, MAX_OUTPUT_BYTES)?
        };
        let top = if String::from_utf8_lossy(&bare).trim() == "true" {
            None
        } else {
            Some(self.path_at(dir, "--show-toplevel")?)
        };
        Ok(Checkout {
            git_dir: self.path_at(dir, "--git-dir")?,
            common_dir: self.path_at(dir, "--git-common-dir")?,
            top,
        })
    }

    fn find_repo(&self, dir: &Path) -> Result<Repo, GitError> {
        Ok(self.repo(self.path_at(dir, "--git-common-dir")?))
    }

    fn path_at(&self, dir: &Path, query: &str) -> Result<PathBuf, GitError> {
        let mut command = self.base();
        let mut safe = OsString::from("safe.directory=");
        safe.push(dir);
        command.arg("-c").arg(safe).current_dir(dir);
        command.args(["rev-parse", "--path-format=absolute", query]);
        let out = self.run(command, "rev-parse", None, MAX_OUTPUT_BYTES)?;
        let text = String::from_utf8(out).map_err(|_| GitError::Unreadable)?;
        let path = PathBuf::from(text.trim_end_matches('\n'));
        if !path.is_absolute() {
            return Err(GitError::Unreadable);
        }
        Ok(path)
    }

    /// git with a cleared environment and the hardening settings, before any
    /// repository is named.
    fn base(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", "/nonexistent")
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "/bin/false")
            .env("SSH_ASKPASS", "/bin/false")
            .env("GIT_SSH_COMMAND", "/bin/false")
            .env("GIT_EDITOR", "/bin/false")
            .env("GIT_SEQUENCE_EDITOR", "/bin/false")
            // A list naming no real transport, so every one is refused.
            .env("GIT_ALLOW_PROTOCOL", "none")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_LITERAL_PATHSPECS", "1")
            .env("GIT_PAGER", "cat")
            .env("GIT_ADVICE", "0")
            .arg("--no-pager")
            .arg("--no-replace-objects")
            .arg("--literal-pathspecs")
            .arg("--no-lazy-fetch");
        for setting in HARDENING {
            command.arg("-c").arg(setting);
        }
        command
    }

    /// Run `command`, giving it `input` on stdin. Output past `cap` stops git
    /// with [`GitError::TooLarge`].
    fn run(
        &self,
        mut command: Command,
        what: &str,
        input: Option<Vec<u8>>,
        cap: usize,
    ) -> Result<Vec<u8>, GitError> {
        let started = Instant::now();
        command
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Atomic's own: stopping waits for it rather than kill it mid-commit.
        let (mut child, _own) = atomic_exec::process::spawn_own_tool(&mut command)?;
        if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
            // On its own thread, so that git filling stdout cannot stall it,
            // nor it git.
            std::thread::spawn(move || {
                let _ = stdin.write_all(&input);
            });
        }
        let (Some(mut stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            stop(&mut child);
            return Err(GitError::Unreadable);
        };
        let errors = keep_errors(stderr);
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let read = (&mut stdout)
                .take(cap as u64 + 1)
                .read_to_end(&mut bytes)
                .map(|_| bytes);
            // Closing the pipe here stops a git that had more to say.
            drop(stdout);
            let _ = sender.send(read);
        });
        let bytes = match receiver.recv_timeout(self.timeout) {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(error)) => {
                stop(&mut child);
                return Err(GitError::Spawn(error));
            }
            Err(_) => {
                stop(&mut child);
                return Err(GitError::Timeout(self.timeout));
            }
        };
        if bytes.len() > cap {
            stop(&mut child);
            return Err(GitError::TooLarge(cap));
        }
        let left = self.timeout.saturating_sub(started.elapsed());
        match wait_for(&mut child, left)? {
            Some(status) if status.success() => Ok(bytes),
            Some(status) => {
                let stderr = errors.join().unwrap_or_default();
                Err(GitError::Failed {
                    what: what.to_owned(),
                    code: status.code(),
                    stderr: printable(&stderr),
                })
            }
            None => {
                stop(&mut child);
                Err(GitError::Timeout(self.timeout))
            }
        }
    }
}

/// A repository, by its git directory and, for a worktree, its work tree.
#[derive(Clone, Debug)]
pub struct Repo {
    git: Git,
    git_dir: PathBuf,
    work_tree: Option<PathBuf>,
    /// Settings that switch off what the repository's own config names, such
    /// as its filter drivers, given to git as command-line config.
    overrides: Arc<Vec<(String, String)>>,
}

/// A ref and the object it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ref {
    /// The whole name, such as `refs/heads/main`.
    pub name: String,
    /// The object, a commit ID.
    pub sha: String,
}

/// Who a commit is by: the author and committer recorded on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    /// The name.
    pub name: String,
    /// The email address.
    pub email: String,
}

impl Default for Signature {
    fn default() -> Self {
        Self {
            name: "atomic".into(),
            email: "atomic@localhost".into(),
        }
    }
}

impl Repo {
    /// The git directory.
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// The work tree, for a worktree.
    pub fn work_tree(&self) -> Option<&Path> {
        self.work_tree.as_deref()
    }

    /// git, hardened, with the git directory and work tree given explicitly
    /// and the current directory outside the work tree's control.
    pub(crate) fn command(&self) -> Command {
        let mut command = self.git.base();
        command
            .env("GIT_DIR", &self.git_dir)
            .env("GIT_CEILING_DIRECTORIES", "/");
        let mut safe = OsString::from("safe.directory=");
        safe.push(&self.git_dir);
        command.arg("-c").arg(safe);
        match &self.work_tree {
            Some(tree) => {
                command.env("GIT_WORK_TREE", tree).current_dir(tree);
                let mut safe = OsString::from("safe.directory=");
                safe.push(tree);
                command.arg("-c").arg(safe);
            }
            None => {
                command.current_dir(&self.git_dir);
            }
        }
        if !self.overrides.is_empty() {
            command.env("GIT_CONFIG_COUNT", self.overrides.len().to_string());
            for (i, (key, value)) in self.overrides.iter().enumerate() {
                command
                    .env(format!("GIT_CONFIG_KEY_{i}"), key)
                    .env(format!("GIT_CONFIG_VALUE_{i}"), value);
            }
        }
        command
    }

    /// The same repository's worktree: git directory `git_dir`, checked out
    /// at `work_tree`, with this repository's overrides.
    pub fn linked(&self, git_dir: impl Into<PathBuf>, work_tree: impl Into<PathBuf>) -> Repo {
        Repo {
            work_tree: Some(work_tree.into()),
            git_dir: git_dir.into(),
            ..self.clone()
        }
    }

    /// This repository with every filter driver its config defines switched
    /// off. A filter's `clean` and `smudge` programs run on `add` and on
    /// checkout, for any file whose attributes name the filter, and an atom
    /// controls both the attributes and the content of a worktree it wrote
    /// to; the command line cannot name every driver in advance, so they are
    /// listed here once and each is emptied. Files go into commits as they
    /// are in the worktree.
    pub fn without_filters(self) -> Result<Repo, GitError> {
        let out = match self.run(
            &[
                "config",
                "--includes",
                "--null",
                "--name-only",
                "--get-regexp",
                "^filter\\.",
            ],
            None,
        ) {
            Ok(out) => out,
            // Exit code 1: no such settings.
            Err(GitError::Failed { code: Some(1), .. }) => Vec::new(),
            Err(e) => return Err(e),
        };
        let mut names: Vec<String> = out
            .split(|b| *b == 0)
            .filter_map(|key| std::str::from_utf8(key).ok())
            .filter_map(|key| key.trim_matches('\n').strip_prefix("filter."))
            .filter_map(|rest| rest.rsplit_once('.').map(|(name, _)| name.to_owned()))
            .collect();
        names.sort();
        names.dedup();
        let mut overrides = (*self.overrides).clone();
        for name in names {
            for (key, value) in [
                ("clean", ""),
                ("smudge", ""),
                ("process", ""),
                ("required", "false"),
            ] {
                overrides.push((format!("filter.{name}.{key}"), value.to_owned()));
            }
        }
        Ok(Repo {
            overrides: Arc::new(overrides),
            ..self
        })
    }

    /// Run git with `args` after the fixed options.
    pub(crate) fn run(&self, args: &[&str], input: Option<Vec<u8>>) -> Result<Vec<u8>, GitError> {
        self.run_with(args, input, &[])
    }

    /// Run git with `args` and extra environment `env`.
    pub(crate) fn run_with(
        &self,
        args: &[&str],
        input: Option<Vec<u8>>,
        env: &[(&str, &str)],
    ) -> Result<Vec<u8>, GitError> {
        let mut command = self.command();
        for (k, v) in env {
            command.env(k, v);
        }
        command.args(args);
        let what = args.first().copied().unwrap_or("");
        self.git.run(command, what, input, MAX_OUTPUT_BYTES)
    }

    fn text(&self, args: &[&str]) -> Result<String, GitError> {
        String::from_utf8(self.run(args, None)?).map_err(|_| GitError::Unreadable)
    }

    /// The whole ID of the commit `rev` names: a commit ID or a unique prefix
    /// of one, or a ref name (`main`, `v1.0`, `refs/remotes/origin/main`),
    /// looked up the way git does. Anything else, such as `HEAD~1` or
    /// `@{-1}`, is refused before git runs.
    pub fn resolve_commit(&self, rev: &str) -> Result<String, GitError> {
        if !is_sha_prefix(rev) && !is_ref_name(rev) {
            return Err(GitError::Invalid(
                "a revision is a commit ID or a plain ref name",
            ));
        }
        let revision = format!("{rev}^{{commit}}");
        let sha = self.text(&[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &revision,
        ])?;
        let sha = sha.trim_end();
        if !is_sha(sha) {
            return Err(GitError::Unreadable);
        }
        Ok(sha.to_string())
    }

    /// The commit ref `name` names, or `None` if there is no such ref.
    pub fn ref_target(&self, name: &str) -> Result<Option<String>, GitError> {
        check_ref(name)?;
        let found = self.refs(name)?;
        Ok(found.into_iter().find(|r| r.name == name).map(|r| r.sha))
    }

    /// Every ref under `prefix` (such as `refs/atomic/`) that names a commit,
    /// sorted by name.
    pub fn refs(&self, prefix: &str) -> Result<Vec<Ref>, GitError> {
        check_ref(prefix.trim_end_matches('/'))?;
        let out = self.run(
            &[
                "for-each-ref",
                "--format=%(objectname)%00%(objecttype)%00%(refname)",
                "--end-of-options",
                prefix,
            ],
            None,
        )?;
        let mut refs = Vec::new();
        for line in out.split(|b| *b == b'\n') {
            let fields: Vec<&[u8]> = line.split(|b| *b == 0).collect();
            let [object, b"commit", name] = fields[..] else {
                continue;
            };
            let (Ok(name), Ok(sha)) = (std::str::from_utf8(name), std::str::from_utf8(object))
            else {
                continue;
            };
            if is_sha(sha) && is_ref_name(name) {
                refs.push(Ref {
                    name: name.to_owned(),
                    sha: sha.to_owned(),
                });
            }
        }
        refs.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(refs)
    }

    /// Create ref `name` at commit `sha`. Fails if the ref already exists,
    /// so two writers can never both believe they created it.
    pub fn create_ref(&self, name: &str, sha: &str) -> Result<(), GitError> {
        check_ref(name)?;
        check_sha(sha)?;
        let zero = "0".repeat(sha.len());
        self.run(&["update-ref", "--no-deref", name, sha, &zero], None)?;
        Ok(())
    }

    /// Delete ref `name` if it still names `sha`.
    pub fn delete_ref(&self, name: &str, sha: &str) -> Result<(), GitError> {
        check_ref(name)?;
        check_sha(sha)?;
        self.run(&["update-ref", "--no-deref", "-d", name, sha], None)?;
        Ok(())
    }

    /// The parents of commit `sha`.
    pub fn parents(&self, sha: &str) -> Result<Vec<String>, GitError> {
        check_sha(sha)?;
        let text = self.text(&["rev-list", "--parents", "-n", "1", sha])?;
        let mut ids = text.split_whitespace().map(str::to_owned);
        if ids.next().as_deref() != Some(sha) {
            return Err(GitError::Unreadable);
        }
        ids.map(|p| {
            if is_sha(&p) {
                Ok(p)
            } else {
                Err(GitError::Unreadable)
            }
        })
        .collect()
    }

    /// Copy every object reachable from commit `sha` into `to`, with no
    /// transport: this repository writes them as one pack, and `to` indexes
    /// it. Both run within this repository's time limit, and a failure of
    /// either stops the other. A shallow or grafted repository is refused by
    /// the caller first: its history would be cut where git believes it
    /// ends, so check the copy with [`Repo::has_everything`].
    pub fn copy_reachable(&self, sha: &str, to: &Repo) -> Result<(), GitError> {
        check_sha(sha)?;
        let deadline = Instant::now() + self.git.timeout;
        let mut pack = self.command();
        pack.args(["pack-objects", "--revs", "--stdout", "--quiet"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut packer = pack.spawn()?;
        let fail = |packer: &mut Child, e: GitError| {
            stop(packer);
            e
        };
        let written = packer
            .stdin
            .take()
            .ok_or(GitError::Unreadable)
            .and_then(|mut stdin| Ok(stdin.write_all(format!("{sha}\n").as_bytes())?));
        if let Err(e) = written {
            return Err(fail(&mut packer, e));
        }
        let (Some(stream), Some(pack_errors)) = (packer.stdout.take(), packer.stderr.take()) else {
            return Err(fail(&mut packer, GitError::Unreadable));
        };
        let pack_errors = keep_errors(pack_errors);
        let mut index = to.command();
        index
            .args(["index-pack", "--stdin"])
            .stdin(Stdio::from(stream))
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut indexer = match index.spawn() {
            Ok(indexer) => indexer,
            Err(e) => return Err(fail(&mut packer, e.into())),
        };
        let Some(index_errors) = indexer.stderr.take() else {
            stop(&mut indexer);
            return Err(fail(&mut packer, GitError::Unreadable));
        };
        let index_errors = keep_errors(index_errors);
        let left = || deadline.saturating_duration_since(Instant::now());
        let indexed = wait_for(&mut indexer, left())?;
        let packed = match indexed {
            Some(_) => wait_for(&mut packer, left())?,
            None => None,
        };
        let (Some(indexed), Some(packed)) = (indexed, packed) else {
            stop(&mut indexer);
            stop(&mut packer);
            return Err(GitError::Timeout(self.git.timeout));
        };
        let (pack_errors, index_errors) = (
            pack_errors.join().unwrap_or_default(),
            index_errors.join().unwrap_or_default(),
        );
        if !packed.success() {
            return Err(GitError::Failed {
                what: "pack-objects".into(),
                code: packed.code(),
                stderr: printable(&pack_errors),
            });
        }
        if !indexed.success() {
            return Err(GitError::Failed {
                what: "index-pack".into(),
                code: indexed.code(),
                stderr: printable(&index_errors),
            });
        }
        Ok(())
    }

    /// Whether this repository holds commit `sha` and every object it
    /// reaches, as `git rev-list --objects` walks them.
    pub fn has_everything(&self, sha: &str) -> Result<bool, GitError> {
        check_sha(sha)?;
        match self.run(&["rev-list", "--objects", "--quiet", sha, "--"], None) {
            Ok(_) => Ok(true),
            // A missing object is fatal to the walk; any other failure is
            // not an answer.
            Err(GitError::Failed {
                code: Some(128), ..
            }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Whether commit `ancestor` is `descendant` or one of its ancestors.
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool, GitError> {
        check_sha(ancestor)?;
        check_sha(descendant)?;
        match self.run(&["merge-base", "--is-ancestor", ancestor, descendant], None) {
            Ok(_) => Ok(true),
            Err(GitError::Failed { code: Some(1), .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// The hash this repository names its objects with: `sha1` or `sha256`.
    pub fn object_format(&self) -> Result<String, GitError> {
        Ok(self
            .text(&["rev-parse", "--show-object-format"])?
            .trim()
            .to_owned())
    }

    /// Whether this repository is shallow: its history is cut short.
    pub fn is_shallow(&self) -> Result<bool, GitError> {
        Ok(self.text(&["rev-parse", "--is-shallow-repository"])?.trim() == "true")
    }

    /// Every entry of commit `sha`'s tree, recursively: its mode, type
    /// (`blob`, `tree` or `commit`, a submodule), object ID and path.
    pub fn entries(&self, sha: &str) -> Result<Vec<TreeEntry>, GitError> {
        check_sha(sha)?;
        let out = self.run(&["ls-tree", "-r", "-z", "--full-tree", sha], None)?;
        out.split(|b| *b == 0)
            .filter(|e| !e.is_empty())
            .map(|entry| {
                let text = std::str::from_utf8(entry).map_err(|_| GitError::Unreadable)?;
                let (head, path) = text.split_once('\t').ok_or(GitError::Unreadable)?;
                let mut parts = head.split(' ');
                let (Some(mode), Some(kind), Some(id)) = (parts.next(), parts.next(), parts.next())
                else {
                    return Err(GitError::Unreadable);
                };
                if !is_sha(id) {
                    return Err(GitError::Unreadable);
                }
                Ok(TreeEntry {
                    mode: mode.to_owned(),
                    kind: kind.to_owned(),
                    id: id.to_owned(),
                    path: path.to_owned(),
                })
            })
            .collect()
    }

    /// The contents of blob `id`, read by its object ID, so any path it has
    /// in a tree is read the same.
    pub fn blob_by_id(&self, id: &str) -> Result<Vec<u8>, GitError> {
        check_sha(id)?;
        self.run(&["cat-file", "blob", id], None)
    }

    /// The tree of commit `sha`.
    pub fn tree_of(&self, sha: &str) -> Result<String, GitError> {
        check_sha(sha)?;
        let revision = format!("{sha}^{{tree}}");
        let tree = self.text(&["rev-parse", "--verify", "--end-of-options", &revision])?;
        let tree = tree.trim_end();
        if !is_sha(tree) {
            return Err(GitError::Unreadable);
        }
        Ok(tree.to_owned())
    }

    /// The paths that differ between the trees of `from` and `to`, sorted.
    /// External diff and text-conversion drivers are off.
    pub fn changed_paths(&self, from: &str, to: &str) -> Result<Vec<String>, GitError> {
        check_sha(from)?;
        check_sha(to)?;
        let out = self.run(
            &[
                "diff-tree",
                "-r",
                "-z",
                "--name-only",
                "--no-renames",
                "--no-ext-diff",
                "--no-textconv",
                from,
                to,
            ],
            None,
        )?;
        let mut paths: Vec<String> = out
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        paths.sort();
        Ok(paths)
    }

    /// Stage everything in the work tree, as `git add --all` sees it, write
    /// the index as a tree and commit it with parent `parent`, as `who`.
    /// Returns the new commit's ID. No ref moves, and no hook can run: the
    /// commit is made with plumbing, which has none, on top of the hardening.
    pub fn commit_all(
        &self,
        parent: &str,
        message: &str,
        who: &Signature,
    ) -> Result<String, GitError> {
        if self.work_tree.is_none() {
            return Err(GitError::Invalid("only a worktree can be committed"));
        }
        check_sha(parent)?;
        let bad = ['\0', '<', '>', '\n'];
        if message.contains('\0') || who.name.contains(bad) || who.email.contains(bad) {
            return Err(GitError::Invalid(
                "a commit message or signature holds a character git cannot record",
            ));
        }
        // Ignored files stay out, as the work tree's ignore rules say.
        self.run(&["add", "--all"], None)?;
        let tree = self.text(&["write-tree"])?;
        let tree = tree.trim_end();
        if !is_sha(tree) {
            return Err(GitError::Unreadable);
        }
        crate::failpoint::failpoint("commit:after-stage");
        let env = [
            ("GIT_AUTHOR_NAME", who.name.as_str()),
            ("GIT_AUTHOR_EMAIL", who.email.as_str()),
            ("GIT_COMMITTER_NAME", who.name.as_str()),
            ("GIT_COMMITTER_EMAIL", who.email.as_str()),
        ];
        let out = self.run_with(
            &[
                "commit-tree",
                "--no-gpg-sign",
                "-p",
                parent,
                "-F",
                "-",
                tree,
            ],
            Some(message.as_bytes().to_vec()),
            &env,
        )?;
        let sha = String::from_utf8(out).map_err(|_| GitError::Unreadable)?;
        let sha = sha.trim_end();
        if !is_sha(sha) {
            return Err(GitError::Unreadable);
        }
        Ok(sha.to_owned())
    }
}

fn check_sha(sha: &str) -> Result<(), GitError> {
    if !is_sha(sha) {
        return Err(GitError::Invalid("a commit is 40 or 64 hex digits"));
    }
    Ok(())
}

fn check_ref(name: &str) -> Result<(), GitError> {
    if !is_ref_name(name) || !name.starts_with("refs/") {
        return Err(GitError::Invalid("not a plain ref name under refs/"));
    }
    Ok(())
}

/// Bytes git printed, as text safe to show: control characters but newline
/// replaced.
fn printable(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_end()
        .chars()
        .map(|c| if c.is_control() && c != '\n' { '?' } else { c })
        .collect()
}

/// Stop git, with everything it started: it runs in a process group of
/// its own (see `atomic_exec::process::spawn_own_tool`).
fn stop(child: &mut Child) {
    atomic_exec::process::kill_own_tool(child);
}

/// The child's exit, if it comes within `limit`.
fn wait_for(child: &mut Child, limit: Duration) -> Result<Option<ExitStatus>, GitError> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(test)]
pub(crate) mod tests;
