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
//! what earlier attempts of the same step left, except the scratch of any
//! attempt this process still has open, and connecting removes what
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
    /// Directories a `workspace-write` turn, or an exec command, may write
    /// besides the workspace and its scratch directory, such as its run's
    /// build caches.
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
    /// `HOME` (and an exec atom's `TMPDIR`), removed when the workspace is
    /// closed; `None` when the request had no source.
    pub scratch: Option<PathBuf>,
    /// A private temporary directory every model turn on this backend
    /// shares, and none may write: the `TMPDIR` of Codex's sandbox helper,
    /// which keeps its registry there of the mount points it protects in
    /// writable roots, so turns that may share a writable root share it too.
    /// A turn's commands get a `TMPDIR` of their own, inside `scratch`.
    /// `None` when the request had no source.
    pub tmp: Option<PathBuf>,
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
    /// Directories of the backend's own that the workspace's commands may
    /// neither read nor write, `scratch` excepted: the scratch directories of
    /// every other attempt, live or not, its backend's and those of the
    /// other backends it was told to deny, such as every project's in the
    /// atomic home, so no turn and no confined command reads another's `HOME`
    /// or `TMPDIR`.
    pub denied: Vec<PathBuf>,
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

/// Refuse a scratch root inside any of `roots`: a sandboxed turn may write
/// there, and a scratch directory is where an unsandboxed command reads its
/// startup files.
pub(crate) fn check_outside(scratch_root: &Path, roots: &[PathBuf]) -> Result<(), String> {
    let root = resolved(scratch_root);
    for other in roots {
        let other = resolved(other);
        if root.starts_with(&other) {
            return Err(format!(
                "scratch directories under {} would be inside {}, where a sandboxed turn may \
                 write; keep them in the atomic home's scratch/, outside the project, /tmp and \
                 $TMPDIR",
                scratch_root.display(),
                other.display()
            ));
        }
    }
    Ok(())
}

/// Refuse a writable root at or inside `denied`, a directory a workspace's
/// commands are denied: Codex's Linux sandbox reopens a writable directory
/// inside a denied one, so the root would be reachable after all.
pub(crate) fn check_not_within(denied: &Path, roots: &[PathBuf]) -> Result<(), String> {
    let denied_real = resolved(denied);
    for root in roots {
        if resolved(root).starts_with(&denied_real) {
            return Err(format!(
                "{} lies inside {}, which a workspace's commands are denied, and cannot be \
                 one of its writable directories",
                root.display(),
                denied.display()
            ));
        }
    }
    Ok(())
}

/// The directories of `denied` that lie inside no other of them: the ones a
/// sandbox denies. Codex's Linux sandbox cannot open a writable directory
/// again inside a denied one that lies inside another, so a backend's own
/// scratch root inside a directory it is also told to deny, such as the
/// atomic home's `scratch/`, is left to that one. Inside means as the paths
/// resolve: one that is a link to somewhere else, such as another disk, is
/// not covered by the directory its link sits in, and is denied itself.
pub(crate) fn outermost(denied: &[PathBuf]) -> Vec<PathBuf> {
    let real: Vec<PathBuf> = denied.iter().map(PathBuf::as_path).map(resolved).collect();
    denied
        .iter()
        .zip(&real)
        .filter(|(d, r)| {
            !denied
                .iter()
                .zip(&real)
                .any(|(o, ro)| o != *d && ro != *r && r.starts_with(ro))
        })
        .map(|(d, _)| d.clone())
        .collect()
}

/// Make each of `dirs`, other backends' scratch roots a workspace's commands
/// are denied, if missing: one that appeared later, while a command ran,
/// would not be denied to it.
fn make_denied(
    dirs: &[PathBuf],
    failed: &dyn Fn(&str, std::io::Error) -> ExecError,
) -> Result<(), ExecError> {
    for dir in dirs {
        private_dir(dir).map_err(|e| {
            failed(
                &format!("create {}, which its commands are denied", dir.display()),
                e,
            )
        })?;
    }
    Ok(())
}

/// `path` with its symlinks resolved as far as it exists.
fn resolved(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut existing = path;
    loop {
        if let Ok(real) = std::fs::canonicalize(existing) {
            return rest.iter().rev().fold(real, |p, part| p.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => return path.to_owned(),
        }
    }
}

/// Directories no scratch root may be in, whatever the project: the system's
/// temporary directories, which `workspace-write` turns may write.
pub(crate) fn temporary_roots() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/tmp"), std::env::temp_dir()];
    if let Some(tmp) = std::env::var_os("TMPDIR").filter(|t| !t.is_empty()) {
        roots.push(PathBuf::from(tmp));
    }
    roots
}

/// This process, as its scratch directories name it: its id and, where the
/// system tells it, when it started, so a later process given the same id
/// is not taken for it.
fn owner() -> &'static str {
    static OWNER: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    OWNER.get_or_init(|| {
        let pid = std::process::id();
        format!("{pid}.{}", started(pid).unwrap_or(0))
    })
}

/// When process `pid` started, in the system's own units, where it says:
/// clock ticks after boot on Linux, microseconds since the epoch on macOS.
fn started(pid: u32) -> Option<u64> {
    crate::process::started(pid)
}

/// The same scratch exclusions apply to commands run in an atom workspace
/// and to executable criteria run in a host-prepared candidate directory.
pub(crate) fn command_exclusions(
    scratch_root: &Path,
    denying: &[PathBuf],
    writable: &[PathBuf],
    fail: &dyn Fn(String) -> ExecError,
    failed: &dyn Fn(&str, std::io::Error) -> ExecError,
) -> Result<Vec<PathBuf>, ExecError> {
    let mut denied = vec![scratch_root.to_owned()];
    for dir in denying {
        if !denied.contains(dir) {
            denied.push(dir.clone());
        }
    }
    for root in &denied {
        check_outside(root, writable).map_err(fail)?;
        check_not_within(root, writable).map_err(fail)?;
    }
    make_denied(&denied[1..], failed)?;
    Ok(outermost(&denied))
}

/// Open `request` on this machine, keeping scratch directories under
/// `scratch_root` and sharing `tmp` with every other turn of the backend:
/// the source directory is the root, as it is; a fresh
/// scratch directory replaces whatever an earlier attempt of the same run
/// and step left, except an attempt this process still has open; a turn's
/// shell runs here, under Codex's own sandbox.
pub(crate) fn open_local(
    scratch_root: &Path,
    tmp: &Path,
    denying: &[PathBuf],
    request: &Open,
) -> Result<Workspace, ExecError> {
    let fail = |message: String| ExecError::Workspace {
        label: request.label.clone(),
        message,
    };
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
    let Some(source) = &request.source else {
        let denied = command_exclusions(scratch_root, denying, &[], &fail, &failed)?;
        return Ok(Workspace {
            label: request.label.clone(),
            sandbox: request.sandbox,
            env: request.env.clone(),
            denied,
            ..Workspace::default()
        });
    };
    if !source.is_dir() {
        return Err(fail(format!("{} is not a directory", source.display())));
    }
    let writable: Vec<PathBuf> = std::iter::once(source.clone())
        .chain(request.writable_roots.iter().cloned())
        .collect();
    let denied = command_exclusions(scratch_root, denying, &writable, &fail, &failed)?;
    private_dir(scratch_root).map_err(|e| failed("create the scratch directories' parent", e))?;
    let tmp = tmp.to_owned();
    private_dir(&tmp).map_err(|e| failed("create the shared temporary directory", e))?;
    let step = step_dir(scratch_root, &request.run, &request.step);
    let scratch = step.join(format!("{}-{}", owner(), slug(&request.attempt)));
    // What earlier attempts of this step left is replaced, but never an
    // attempt this process still has open: one whose run was cancelled
    // while it opened can finish opening after its step's next attempt,
    // and must not take that one's directory from under it. Under the lock
    // of the open attempts, so none opens or closes meanwhile, what goes is
    // only moved aside; it is removed after.
    let (aside, opened) = {
        let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
        let aside = set_aside(scratch_root, &step, &open);
        let opened = (|| {
            private_dir(&step).map_err(|e| failed("create the step's scratch directory", e))?;
            if scratch.exists() {
                return Err(fail(format!(
                    "{} exists already: an attempt is opened only once",
                    scratch.display()
                )));
            }
            private_dir(&scratch).map_err(|e| failed("create its scratch directory", e))?;
            open.insert(scratch.clone());
            Ok(())
        })();
        (aside, opened)
    };
    // What was set aside goes whether or not this attempt opened: on a full
    // disk, that space is what lets the next try succeed. What cannot be
    // removed is left, with a warning.
    for left in aside {
        if let Err(e) = remove_entry(&left) {
            warn(&format!(
                "could not remove an earlier attempt's scratch {}: {e}",
                left.display()
            ));
        }
    }
    opened?;
    Ok(Workspace {
        label: request.label.clone(),
        root: Some(source.clone()),
        scratch: Some(scratch),
        tmp: Some(tmp),
        shell: Shell::Local,
        sandbox: request.sandbox,
        writable_roots: request.writable_roots.clone(),
        env: request.env.clone(),
        denied,
    })
}

/// The scratch directories of this process's attempts that are open: an
/// attempt opening never removes one of them.
static OPEN: std::sync::Mutex<std::collections::BTreeSet<PathBuf>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// Move what is in `step` aside, under `scratch_root`, except the scratch
/// directories in `open`, and return where each went. One that cannot be
/// moved is left, with a warning.
fn set_aside(
    scratch_root: &Path,
    step: &Path,
    open: &std::collections::BTreeSet<PathBuf>,
) -> Vec<PathBuf> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let Ok(entries) = std::fs::read_dir(step) else {
        return Vec::new();
    };
    let mut aside = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if open.contains(&path) {
            continue;
        }
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let to = scratch_root.join(format!("{REMOVING}{}-{n}", owner()));
        // One that cannot be moved, such as a directory nobody may write,
        // is removed where it is.
        match std::fs::rename(&path, &to) {
            Ok(()) => aside.push(to),
            Err(_) => {
                if let Err(e) = remove_entry(&path) {
                    warn(&format!(
                        "could not remove an earlier attempt's scratch {}: {e}",
                        path.display()
                    ));
                }
            }
        }
    }
    aside
}

/// Remove `path`, a directory and everything under it, or a file.
fn remove_entry(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => remove_tree(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// How a scratch directory moved aside to be removed is named, then its
/// owner: hidden, so it is never taken for a step's.
const REMOVING: &str = ".removing-";

/// Close a workspace [`open_local`] opened: its scratch directory goes, and
/// its root, which the control plane owns, stays.
pub(crate) fn close_local(workspace: &Workspace) -> Result<(), ExecError> {
    let Some(scratch) = &workspace.scratch else {
        return Ok(());
    };
    let removed = remove_tree(scratch).map_err(|e| ExecError::Workspace {
        label: workspace.label.clone(),
        message: format!("remove its scratch directory: {e}"),
    });
    // No longer open, and the step's directory goes once its last attempt
    // has, both under the lock an opening attempt holds.
    let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
    open.remove(scratch);
    if removed.is_ok()
        && let Some(step) = scratch.parent()
    {
        let _ = std::fs::remove_dir(step);
    }
    drop(open);
    removed
}

/// Remove what attempts of processes that no longer run left under
/// `scratch_root`, and return how many attempts' directories went. A
/// directory whose owner may still run is kept, and one that cannot be
/// removed is left with a warning: nothing here fails a connect.
pub(crate) fn sweep_local(scratch_root: &Path) -> usize {
    let mut removed = 0;
    let Ok(steps) = std::fs::read_dir(scratch_root) else {
        return 0;
    };
    for step in steps.flatten() {
        let step = step.path();
        let name = step
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        // What a process moved aside to remove and did not, as it was killed.
        if let Some(tag) = name
            .strip_prefix(REMOVING)
            .and_then(|rest| rest.rsplit_once('-'))
            .map(|(tag, _)| tag)
            && tag != owner()
            && !owner_may_run(tag)
        {
            match remove_entry(&step) {
                Ok(()) => removed += 1,
                Err(e) => warn(&format!(
                    "could not remove a dead attempt's scratch {}: {e}",
                    step.display()
                )),
            }
            continue;
        }
        if name.starts_with('.') || !step.is_dir() {
            continue;
        }
        let Ok(attempts) = std::fs::read_dir(&step) else {
            continue;
        };
        for attempt in attempts.flatten() {
            let attempt = attempt.path();
            let Some(name) = attempt.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            // A name that does not say its owner is not ours to judge.
            let Some((tag, _)) = name.split_once('-') else {
                continue;
            };
            if tag == owner() || owner_may_run(tag) {
                continue;
            }
            match remove_tree(&attempt) {
                Ok(()) => removed += 1,
                Err(e) => warn(&format!(
                    "could not remove a dead attempt's scratch {}: {e}",
                    attempt.display()
                )),
            }
        }
        let _ = std::fs::remove_dir(&step);
    }
    removed
}

/// Retire `scratch_root`, a backend's scratch root no longer used: remove
/// what attempts of processes that no longer run left there, as a connect
/// does, and the root itself once nothing else is in it but its dot-entries,
/// such as its shared temporary directory. Returns whether the root is gone.
/// An attempt whose owner may still run, or an entry whose name does not
/// say whose it is, keeps the root, so a backend still denies it.
pub fn retire_scratch_root(scratch_root: &Path) -> std::io::Result<bool> {
    let entries = match std::fs::read_dir(scratch_root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(e),
    };
    drop(entries);
    sweep_local(scratch_root);
    for entry in std::fs::read_dir(scratch_root)? {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with('.') {
            return Ok(false);
        }
    }
    match remove_tree(scratch_root) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e),
    }
}

/// Whether the process an attempt's directory names (`<pid>.<started>`, or
/// an older `<pid>`) may still run: a process with that id runs, and, where
/// both are known, it started when the owner did.
fn owner_may_run(tag: &str) -> bool {
    let (pid, start) = match tag.split_once('.') {
        Some((pid, start)) => (pid, start.parse::<u64>().ok()),
        None => (tag, None),
    };
    let Ok(pid) = pid.parse::<u32>() else {
        return true;
    };
    if !alive(pid) {
        return false;
    }
    match (start, started(pid)) {
        (Some(recorded), Some(now)) if recorded != 0 => recorded == now,
        _ => true,
    }
}

/// Remove what is in `dir`, keeping `dir`; what cannot be removed is left
/// with a warning.
pub(crate) fn empty_dir(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let removed = if entry.file_type().is_ok_and(|t| t.is_dir()) {
            remove_tree(&path)
        } else {
            match std::fs::remove_file(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            }
        };
        if let Err(e) = removed {
            warn(&format!("could not remove {}: {e}", path.display()));
        }
    }
}

/// Remove `path` and everything under it. A directory that is not writable,
/// such as a Go module cache's, is made writable and the removal tried
/// again. Symlinks are removed, never followed.
pub(crate) fn remove_tree(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            make_writable(path);
            match std::fs::remove_dir_all(path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            }
        }
        Err(e) => Err(e),
    }
}

/// Give this user write access to every directory under `path`, never
/// following a symlink: each directory is opened without following one, and
/// changed through that descriptor, so swapping it for a link changes
/// nothing outside.
fn make_writable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let Ok(dir) = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(path)
        else {
            return;
        };
        let Ok(meta) = dir.metadata() else {
            return;
        };
        let mode = meta.permissions().mode() | 0o700;
        let _ = dir.set_permissions(std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    {
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return;
        };
        if !meta.is_dir() {
            return;
        }
        let mut permissions = meta.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        let _ = std::fs::set_permissions(path, permissions);
    }
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                make_writable(&entry.path());
            }
        }
    }
}

/// Say on stderr what the backend could not do and is carrying on without,
/// through the log the binary installs (see [`crate::set_log`]).
fn warn(message: &str) {
    crate::log(&format!("atomic: {message}"));
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

/// [`private_dir`], for the backend.
pub(crate) fn ensure_private(dir: &Path) -> std::io::Result<()> {
    private_dir(dir)
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
