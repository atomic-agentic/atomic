// SPDX-License-Identifier: Apache-2.0

//! Workspaces: where one attempt of one atom run does its work.
//!
//! Every atom run opens a workspace on its backend before it starts and
//! closes it when it ends. The request ([`Open`]) says what the atom needs:
//! the directory its work starts from, the sandbox a model turn's shell and
//! file work runs in, the directories such a turn may also write, and the
//! environment the atom declares for its commands. The backend answers with
//! a [`Workspace`]: where that directory is on the runner, a scratch
//! directory of the attempt's own, and where a turn's shell and file work
//! runs ([`Shell`]).
//!
//! A model turn's agent loop stays in the control plane; only its shell and
//! file work runs where the workspace says. Nothing here names a Codex type:
//! a backend on another machine honours the same request by giving its own
//! paths and shell.

use crate::ExecError;
use std::collections::BTreeMap;
use std::path::PathBuf;

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
    /// errors and in the names of what the backend creates for it.
    pub label: String,
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
    /// An empty directory of this attempt's own for its commands' `HOME`
    /// and `TMPDIR`, removed when the workspace is closed; `None` when the
    /// request had no source.
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

/// Open `request` on this machine: the source directory is the root, as it
/// is; a fresh scratch directory is made under the system's temporary
/// directory; a turn's shell runs here, under Codex's own sandbox.
pub(crate) fn open_local(request: &Open) -> Result<Workspace, ExecError> {
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
    let scratch = scratch_dir(&request.label)
        .map_err(|e| fail(format!("create its scratch directory: {e}")))?;
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
    match &workspace.scratch {
        Some(scratch) => match std::fs::remove_dir_all(scratch) {
            Ok(()) => Ok(()),
            // Closed already: closing again finds nothing to remove.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(ExecError::Workspace {
                label: workspace.label.clone(),
                message: format!("remove its scratch directory: {e}"),
            }),
        },
        None => Ok(()),
    }
}

/// A new, empty directory for one attempt, named after it.
fn scratch_dir(label: &str) -> std::io::Result<PathBuf> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tag: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(40)
        .collect();
    loop {
        let dir = std::env::temp_dir().join(format!(
            "atomic-exec-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            // Left by an earlier process with the same id: take the next name.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
}
