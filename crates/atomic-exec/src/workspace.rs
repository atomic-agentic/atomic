// SPDX-License-Identifier: Apache-2.0

//! Workspaces: where one attempt of one atom run does its work.
//!
//! Every atom run opens a workspace on its backend before it calls a model or
//! runs a command, and closes it when it ends. The request ([`Open`]) names
//! the attempt (its run, its step and the attempt itself) and says what the
//! atom needs: the directory its work starts from, the sandbox a model
//! turn's shell and file work runs in, the directories such a turn may also
//! write, and the environment the atom declares for its commands. The
//! backend answers with a [`Workspace`]: where that directory is on the
//! runner, a scratch directory of the attempt's own, and where a turn's shell
//! and file work runs ([`Shell`]).
//!
//! A model turn's agent loop stays in the control plane; only its shell and
//! file work runs where the workspace says. Nothing here names a Codex type:
//! a backend on another machine honours the same request by giving its own
//! paths and shell.
//!
//! On this machine ([`crate::LocalBackend`]), scratch directories live under
//! one private directory (mode 0700), normally in the project's state
//! directory, which is outside every directory a sandboxed turn may write.
//! Each attempt's is named by its run and step, so opening an attempt removes
//! what an earlier attempt of the same step left, and connecting removes what
//! attempts of processes that no longer run left.

use crate::ExecError;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The sandbox a model turn's shell and file work runs in. The network is
/// restricted either way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sandbox {
    /// Read the workspace, write nothing.
    #[default]
    ReadOnly,
    /// Write only inside the workspace and its writable roots.
    WorkspaceWrite,
}

/// What an atom run asks a backend for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Open {
    /// What the workspace is for, such as the atom's name; it appears in
    /// errors.
    pub label: String,
    /// The run the attempt belongs to.
    pub run: String,
    /// The step within the run. With [`run`](Self::run) it names what a
    /// later attempt of the same step replaces.
    pub step: String,
    /// This attempt, unique to it: an attempt that runs again opens with a
    /// new one.
    pub attempt: String,
    /// The directory, on the control plane's machine, that the atom's work
    /// starts from: the project, or a step's worktree. `None` for an atom
    /// that reads no files and runs no commands, such as a lite atom.
    pub source: Option<PathBuf>,
    /// The sandbox a model turn's shell and file work runs in. An exec
    /// atom's command is not a model turn and is not sandboxed by it.
    pub sandbox: Sandbox,
    /// Directories a `workspace-write` turn may write besides the
    /// workspace, such as the project's shared caches.
    pub writable_roots: Vec<PathBuf>,
    /// The environment the atom declares for every command it runs. Nothing
    /// else of the control plane's environment is passed on.
    pub env: BTreeMap<String, String>,
}

/// Where a model turn's shell and file work runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Shell {
    /// Nowhere: the atom has no shell or file tools, and its turn only
    /// talks to the model.
    #[default]
    None,
    /// On this machine, under Codex's own sandbox.
    Local,
}

/// A workspace a backend opened: what the atom's work uses, as the runner
/// sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Workspace {
    /// The request's label.
    pub label: String,
    /// The workspace's root, where the atom's work runs, or `None` when the
    /// request had no source.
    pub root: Option<PathBuf>,
    /// An empty, private directory of this attempt's own for its commands'
    /// `HOME` and `TMPDIR`, removed when the workspace is closed; `None` when
    /// the request had no source.
    pub scratch: Option<PathBuf>,
    /// Where a model turn's shell and file work runs.
    pub shell: Shell,
    /// The sandbox it runs in.
    pub sandbox: Sandbox,
    /// The directories it may write besides the root, as the runner sees
    /// them.
    pub writable_roots: Vec<PathBuf>,
    /// The environment every command in the workspace gets beside the
    /// platform's core variables.
    pub env: BTreeMap<String, String>,
}

impl Workspace {
    /// The root, or an error naming the workspace when it has none.
    pub fn require_root(&self) -> Result<&PathBuf, ExecError> {
        self.root.as_ref().ok_or_else(|| ExecError::Workspace {
            label: self.label.clone(),
            message: "it has no root: it was opened for an atom that touches no files".into(),
        })
    }

    /// The scratch directory, or an error naming the workspace when it has
    /// none.
    pub fn require_scratch(&self) -> Result<&PathBuf, ExecError> {
        self.scratch.as_ref().ok_or_else(|| ExecError::Workspace {
            label: self.label.clone(),
            message: "it has no scratch directory: it was opened for an atom that runs no \
                      commands"
                .into(),
        })
    }
}

/// Where a step's attempts keep their scratch directories, under `root`:
/// one directory per run and step, named so that two never collide.
pub(crate) fn step_dir(root: &Path, run: &str, step: &str) -> PathBuf {
    let mut digest = Sha256::new();
    digest.update(run.as_bytes());
    digest.update([0]);
    digest.update(step.as_bytes());
    let hash: String = digest.finalize()[..12]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    root.join(format!("{}-{hash}", slug(step)))
}

/// `text` as a short name that is safe in a path.
fn slug(text: &str) -> String {
    let slug: String = text
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(40)
        .collect();
    if slug.is_empty() { "step".into() } else { slug }
}

/// Open `request` on this machine, keeping scratch directories under
/// `scratch_root`: the source directory is the root, as it is; a fresh
/// scratch directory replaces whatever an earlier attempt of the same run
/// and step left; a turn's shell runs here, under Codex's own sandbox.
pub(crate) fn open_local(scratch_root: &Path, request: &Open) -> Result<Workspace, ExecError> {
    let fail = |message: String| ExecError::Workspace {
        label: request.label.clone(),
        message,
    };
    let Some(source) = &request.source else {
        return Ok(Workspace {
            label: request.label.clone(),
            sandbox: request.sandbox,
            env: request.env.clone(),
            ..Workspace::default()
        });
    };
    if !source.is_dir() {
        return Err(fail(format!("{} is not a directory", source.display())));
    }
    let failed = |what: &str, e: std::io::Error| {
        let message = format!("{what}: {e}");
        if transient(&e) {
            ExecError::Unavailable {
                label: request.label.clone(),
                message,
            }
        } else {
            fail(message)
        }
    };
    private_dir(scratch_root).map_err(|e| failed("create the scratch directories' parent", e))?;
    let step = step_dir(scratch_root, &request.run, &request.step);
    // An earlier attempt of this step, finished or not, is replaced.
    match std::fs::remove_dir_all(&step) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(failed("remove an earlier attempt's scratch directory", e)),
    }
    private_dir(&step).map_err(|e| failed("create the step's scratch directory", e))?;
    let scratch = step.join(format!("{}-{}", std::process::id(), slug(&request.attempt)));
    private_dir(&scratch).map_err(|e| failed("create its scratch directory", e))?;
    Ok(Workspace {
        label: request.label.clone(),
        root: Some(source.clone()),
        scratch: Some(scratch),
        shell: Shell::Local,
        sandbox: request.sandbox,
        writable_roots: request.writable_roots.clone(),
        env: request.env.clone(),
    })
}

/// Close a workspace [`open_local`] opened: its scratch directory goes, and
/// its root, which the control plane owns, stays.
pub(crate) fn close_local(workspace: &Workspace) -> Result<(), ExecError> {
    let Some(scratch) = &workspace.scratch else {
        return Ok(());
    };
    match std::fs::remove_dir_all(scratch) {
        Ok(()) => {}
        // Closed already, or replaced by a later attempt.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(ExecError::Workspace {
                label: workspace.label.clone(),
                message: format!("remove its scratch directory: {e}"),
            });
        }
    }
    // The step's directory goes once its last attempt has.
    if let Some(step) = scratch.parent() {
        let _ = std::fs::remove_dir(step);
    }
    Ok(())
}

/// Remove what attempts of processes that no longer run left under
/// `scratch_root`, and return how many attempts' directories went. A
/// directory whose owner may still run is kept.
pub(crate) fn sweep_local(scratch_root: &Path) -> std::io::Result<usize> {
    let mut removed = 0;
    let steps = match std::fs::read_dir(scratch_root) {
        Ok(steps) => steps,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    for step in steps {
        let step = step?.path();
        if !step.is_dir() {
            continue;
        }
        for attempt in std::fs::read_dir(&step)? {
            let attempt = attempt?.path();
            let owner = attempt
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.split_once('-'))
                .and_then(|(pid, _)| pid.parse::<u32>().ok());
            // A name that does not say its owner is not ours to judge.
            let Some(owner) = owner else { continue };
            if owner != std::process::id() && !alive(owner) {
                std::fs::remove_dir_all(&attempt)?;
                removed += 1;
            }
        }
        let _ = std::fs::remove_dir(&step);
    }
    Ok(removed)
}

/// Whether process `pid` may still run. Where that cannot be told, it may.
fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return true;
        };
        // SAFETY: signal 0 only checks that the process exists and may be
        // signalled; nothing is sent.
        #[allow(unsafe_code)]
        let result = unsafe { libc::kill(pid, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// Create `dir`, and its parents, readable and writable by this user only.
fn private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        // One that existed already, perhaps with a wider mode, is narrowed.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Whether `e` may pass if the same thing is tried again later: the disk or
/// a quota is full, too many files are open, or the system is busy.
pub(crate) fn transient(e: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    let kind = matches!(
        e.kind(),
        ErrorKind::StorageFull
            | ErrorKind::QuotaExceeded
            | ErrorKind::ResourceBusy
            | ErrorKind::WouldBlock
            | ErrorKind::Interrupted
            | ErrorKind::TimedOut
    );
    #[cfg(unix)]
    let raw = matches!(e.raw_os_error(), Some(libc::EMFILE | libc::ENFILE));
    #[cfg(not(unix))]
    let raw = false;
    kind || raw
}
