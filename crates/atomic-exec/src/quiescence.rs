// SPDX-License-Identifier: Apache-2.0

//! Whether a runner's sandbox is safe to suspend.
//!
//! A sandbox backend that snapshots and suspends its sandboxes, such as Agent
//! Substrate, does so only when asked and has no notion of whether the work in
//! a sandbox is finished, so atomic decides. Codex's own event log is not
//! enough to decide on: after a snapshot that keeps only volumes, it still
//! shows a turn in progress that no longer exists. The runner therefore
//! reports quiescence as the conjunction of everything that can still be in
//! flight, and the control plane suspends only on that report:
//!
//! - Codex's JSON event log for the attempt has no open turn and no open item;
//! - no process is alive but the runner's own;
//! - no exec-server session is connected;
//! - the worktree is committed, and pushed when that is required;
//! - `CODEX_HOME` is on the sandbox's durable directory.
//!
//! A turn the log calls open, with no Codex process alive, is
//! [`State::Lost`]: the attempt is over and must be failed or resumed, not
//! waited for. Any check that cannot be made counts against quiescence.
//!
//! [`check`] takes one snapshot; [`Idle`] turns successive snapshots into how
//! long the sandbox has been quiescent, so an idle timer counts from
//! quiescence. Its clock is the sandbox's own monotonic clock, which stops
//! while the sandbox is suspended.
//!
//! Checks read `/proc` and run `git`, and must not run concurrently in one
//! process: a check's own `git` child would be seen by another check's scan.

use crate::process::{self, Limits};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What to check, as the runner knows it for the current attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inputs {
    /// Codex's JSON event log for the attempt (`codex exec --json`); `None`
    /// when the attempt runs no Codex turn in this sandbox. A path that does
    /// not exist yet means no turn has started.
    pub events: Option<PathBuf>,
    /// The attempt's git worktree, if it has one.
    pub worktree: Option<PathBuf>,
    /// Whether the worktree's commits must also be on its upstream.
    pub require_pushed: bool,
    /// The sandbox's durable directory.
    pub durable: PathBuf,
    /// Codex's state directory, when Codex runs in this sandbox. It must
    /// exist, and its resolved path must lie on the durable directory.
    pub codex_home: Option<PathBuf>,
    /// The port exec-server listens on, when the runner serves one.
    pub exec_server_port: Option<u16>,
    /// The runner's own processes, besides this one and PID 1, which never
    /// count against quiescence.
    pub runner_pids: BTreeSet<u32>,
    /// The program name that identifies Codex and its helpers: a process
    /// whose program's file name starts with it is a Codex process.
    pub codex_program: String,
    /// Where `/proc` is mounted.
    pub proc_root: PathBuf,
    /// The `git` program, found on `PATH` when it has no `/`.
    pub git: String,
}

impl Inputs {
    /// Checks for an attempt in a sandbox whose durable directory is
    /// `durable`, with nothing else configured yet: no Codex log, no
    /// worktree, no exec-server, pushes required, `/proc` and `git` as found.
    pub fn new(durable: impl Into<PathBuf>) -> Self {
        Self {
            events: None,
            worktree: None,
            require_pushed: true,
            durable: durable.into(),
            codex_home: None,
            exec_server_port: None,
            runner_pids: BTreeSet::new(),
            codex_program: "codex".into(),
            proc_root: PathBuf::from("/proc"),
            git: "git".into(),
        }
    }
}

/// The sandbox's state, for the control plane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Safe to suspend: nothing is in flight and the attempt's outputs are saved.
    Quiescent,
    /// Something is in flight or unsaved, or could not be checked.
    Busy,
    /// Codex's log shows a turn in progress, but no Codex process is alive:
    /// the turn was lost (for example to an earlier suspend that kept only
    /// volumes). The attempt must be failed or resumed.
    Lost,
}

/// Why a sandbox is not quiescent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reason {
    /// Codex's log shows a turn, or items in it, still in progress.
    TurnOpen {
        /// The types of the items started and not completed.
        items: Vec<String>,
    },
    /// Processes other than the runner's are alive.
    Processes {
        /// How many.
        count: usize,
    },
    /// A control plane has exec-server sessions open.
    ExecSessions {
        /// How many connections.
        count: usize,
    },
    /// The worktree has uncommitted changes, untracked files included.
    Uncommitted {
        /// How many paths.
        changes: usize,
    },
    /// The worktree's branch has commits its upstream does not.
    Unpushed {
        /// How many.
        ahead: u32,
    },
    /// Pushes are required, but the worktree's branch has no existing upstream.
    NoUpstream,
    /// `CODEX_HOME` is not on the durable directory, so a snapshot that keeps
    /// only volumes would lose Codex's state.
    CodexHomeNotDurable {
        /// Where it is.
        codex_home: PathBuf,
    },
    /// A check could not be made.
    CheckFailed {
        /// Which check.
        check: String,
        /// Why.
        message: String,
    },
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TurnOpen { items } if items.is_empty() => write!(f, "a Codex turn is open"),
            Self::TurnOpen { items } => {
                write!(f, "a Codex turn is open ({} in progress)", items.join(", "))
            }
            Self::Processes { count } => write!(f, "{count} attempt process(es) alive"),
            Self::ExecSessions { count } => write!(f, "{count} exec-server session(s) open"),
            Self::Uncommitted { changes } => {
                write!(f, "the worktree has {changes} uncommitted change(s)")
            }
            Self::Unpushed { ahead } => write!(f, "the worktree has {ahead} unpushed commit(s)"),
            Self::NoUpstream => write!(f, "the worktree's branch has no upstream to push to"),
            Self::CodexHomeNotDurable { codex_home } => write!(
                f,
                "CODEX_HOME ({}) is not on the durable directory",
                codex_home.display()
            ),
            Self::CheckFailed { check, message } => write!(f, "cannot check {check}: {message}"),
        }
    }
}

/// A Codex turn, as its event log tells it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    /// A `turn.started` with no `turn.completed` or `turn.failed` after it.
    pub open: bool,
    /// The types of the items started and not completed.
    pub open_items: Vec<String>,
}

impl Turn {
    /// Whether anything is still in progress.
    pub fn in_progress(&self) -> bool {
        self.open || !self.open_items.is_empty()
    }
}

/// A live process that is not the runner's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Process {
    /// Its PID.
    pub pid: u32,
    /// Its command line.
    pub argv: Vec<String>,
}

/// One quiescence check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// The verdict.
    pub state: State,
    /// Why it is not quiescent; empty when it is.
    pub reasons: Vec<Reason>,
    /// The Codex turn, when there is a log and it could be read.
    pub turn: Option<Turn>,
    /// The processes that count against quiescence.
    pub processes: Vec<Process>,
    /// Connections to exec-server.
    pub exec_sessions: usize,
    /// How long the sandbox has been quiescent, in milliseconds, as
    /// [`Idle::stamp`] last recorded it; 0 until then.
    pub quiescent_for_ms: u64,
}

impl Report {
    /// Whether the sandbox is safe to suspend.
    pub fn quiescent(&self) -> bool {
        self.state == State::Quiescent
    }
}

/// Check everything [`Inputs`] names, once.
pub fn check(inputs: &Inputs) -> Report {
    let mut reasons = Vec::new();
    let failed = |check: &str, message: String| Reason::CheckFailed {
        check: check.into(),
        message,
    };

    let turn = match &inputs.events {
        None => None,
        Some(path) => match std::fs::read_to_string(path) {
            Ok(log) => match codex_turn(&log) {
                Ok(turn) => Some(turn),
                Err(e) => {
                    reasons.push(failed("Codex's event log", e));
                    None
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(Turn::default()),
            Err(e) => {
                reasons.push(failed("Codex's event log", e.to_string()));
                None
            }
        },
    };
    if let Some(turn) = turn.as_ref().filter(|t| t.in_progress()) {
        reasons.push(Reason::TurnOpen {
            items: turn.open_items.clone(),
        });
    }

    let mut exclude = inputs.runner_pids.clone();
    exclude.insert(std::process::id());
    let processes = match attempt_processes(&inputs.proc_root, &exclude) {
        Ok(processes) => Some(processes),
        Err(e) => {
            reasons.push(failed("processes", e));
            None
        }
    };
    if let Some(count) = processes.as_ref().map(Vec::len).filter(|n| *n > 0) {
        reasons.push(Reason::Processes { count });
    }

    let mut exec_sessions = 0;
    if let Some(port) = inputs.exec_server_port {
        match exec_server_connections(&inputs.proc_root, port) {
            Ok(0) => {}
            Ok(count) => {
                exec_sessions = count;
                reasons.push(Reason::ExecSessions { count });
            }
            Err(e) => reasons.push(failed("exec-server sessions", e)),
        }
    }

    if let Some(path) = &inputs.worktree {
        match worktree(&inputs.git, path) {
            Ok(w) => {
                if w.changes > 0 {
                    reasons.push(Reason::Uncommitted { changes: w.changes });
                }
                if inputs.require_pushed && !w.upstream {
                    reasons.push(Reason::NoUpstream);
                }
                if inputs.require_pushed && w.ahead > 0 {
                    reasons.push(Reason::Unpushed { ahead: w.ahead });
                }
            }
            Err(e) => reasons.push(failed("the worktree", e)),
        }
    }

    if let Some(home) = &inputs.codex_home {
        match durable_home(home, &inputs.durable) {
            Ok(true) => {}
            Ok(false) => reasons.push(Reason::CodexHomeNotDurable {
                codex_home: home.clone(),
            }),
            Err(e) => reasons.push(failed("CODEX_HOME durability", e)),
        }
    }

    // Lost needs both facts: the log says a turn is running, and the process
    // scan, which must have succeeded, finds no Codex.
    let codex_alive = processes
        .iter()
        .flatten()
        .any(|p| is_program(p, &inputs.codex_program));
    let lost = turn.as_ref().is_some_and(Turn::in_progress) && processes.is_some() && !codex_alive;
    let state = if lost {
        State::Lost
    } else if reasons.is_empty() {
        State::Quiescent
    } else {
        State::Busy
    };
    Report {
        state,
        reasons,
        turn,
        processes: processes.unwrap_or_default(),
        exec_sessions,
        quiescent_for_ms: 0,
    }
}

fn durable_home(home: &Path, durable: &Path) -> Result<bool, String> {
    let resolve = |path: &Path| {
        let resolved = std::fs::canonicalize(path)
            .map_err(|e| format!("resolving {}: {e}", path.display()))?;
        if !resolved.is_dir() {
            return Err(format!("{} is not a directory", path.display()));
        }
        Ok(resolved)
    };
    Ok(resolve(home)?.starts_with(resolve(durable)?))
}

/// How long a sandbox has been quiescent, from successive reports.
#[derive(Clone, Debug, Default)]
pub struct Idle {
    since: Option<Instant>,
}

impl Idle {
    /// Record a check made at `now`, and return how long the sandbox has been
    /// quiescent without a break; zero when it is not.
    pub fn observe(&mut self, quiescent: bool, now: Instant) -> Duration {
        if !quiescent {
            self.since = None;
            return Duration::ZERO;
        }
        now.saturating_duration_since(*self.since.get_or_insert(now))
    }

    /// [`Idle::observe`] `report`, and record the result in it.
    pub fn stamp(&mut self, report: &mut Report, now: Instant) {
        let quiet = self.observe(report.quiescent(), now);
        report.quiescent_for_ms = u64::try_from(quiet.as_millis()).unwrap_or(u64::MAX);
    }
}

/// The turn in a Codex JSON event log (`codex exec --json`).
///
/// A final line without a newline is a write still in flight, or cut short
/// when Codex was killed, and is ignored; any other line that is not JSON is
/// an error.
pub fn codex_turn(log: &str) -> Result<Turn, String> {
    let complete = match log.rfind('\n') {
        Some(end) => &log[..end],
        None => "",
    };
    let mut open = false;
    let mut started: Vec<(String, String)> = Vec::new();
    let mut completed: BTreeSet<String> = BTreeSet::new();
    for (n, line) in complete.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: serde_json::Value =
            serde_json::from_str(line).map_err(|e| format!("line {}: {e}", n + 1))?;
        let item_id = || {
            event
                .pointer("/item/id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        match event.get("type").and_then(serde_json::Value::as_str) {
            Some("turn.started") => open = true,
            Some("turn.completed" | "turn.failed") => open = false,
            Some("item.started") => {
                if let Some(id) = item_id() {
                    let kind = event
                        .pointer("/item/type")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("item");
                    started.push((id, kind.to_owned()));
                }
            }
            Some("item.completed") => {
                if let Some(id) = item_id() {
                    completed.insert(id);
                }
            }
            _ => {}
        }
    }
    let open_items = started
        .into_iter()
        .filter(|(id, _)| !completed.contains(id))
        .map(|(_, kind)| kind)
        .collect();
    Ok(Turn { open, open_items })
}

/// Every live process under `proc_root` except PID 1, the PIDs in
/// `exclude`, zombies and kernel threads.
///
/// Processes are not filtered by parent: an attempt's orphaned background
/// processes are reparented to PID 1, which may be the runner itself.
pub fn attempt_processes(
    proc_root: &Path,
    exclude: &BTreeSet<u32>,
) -> Result<Vec<Process>, String> {
    let entries = std::fs::read_dir(proc_root)
        .map_err(|e| format!("reading {}: {e}", proc_root.display()))?;
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("listing {}: {e}", proc_root.display()))?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == 1 || exclude.contains(&pid) {
            continue;
        }
        // Only disappearance proves the process ended. Permission and I/O
        // errors leave its liveness unknown and must fail the whole scan.
        let cmdline = match std::fs::read(entry.path().join("cmdline")) {
            Ok(cmdline) => cmdline,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("reading PID {pid} cmdline: {e}")),
        };
        let stat = match std::fs::read_to_string(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("reading PID {pid} stat: {e}")),
        };
        if cmdline.is_empty() || process_state(&stat) == Some('Z') {
            continue;
        }
        let argv = cmdline
            .split(|b| *b == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect();
        out.push(Process { pid, argv });
    }
    out.sort_by_key(|p| p.pid);
    Ok(out)
}

/// The state letter in a `/proc/<pid>/stat` line, after the parenthesised
/// command name, which may itself contain spaces and parentheses.
fn process_state(stat: &str) -> Option<char> {
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .and_then(|s| s.chars().next())
}

fn is_program(process: &Process, name: &str) -> bool {
    process
        .argv
        .first()
        .and_then(|arg0| Path::new(arg0).file_name())
        .and_then(|f| f.to_str())
        .is_some_and(|f| f.starts_with(name))
}

/// Established TCP connections whose local port is `port`, in a
/// `/proc/net/tcp` or `/proc/net/tcp6` table.
pub fn established_connections(table: &str, port: u16) -> usize {
    table
        .lines()
        .skip(1)
        .filter(|row| {
            let mut cols = row.split_whitespace().skip(1);
            let local = cols.next().and_then(|addr| addr.rsplit_once(':'));
            let state = cols.nth(1);
            state == Some("01")
                && local.and_then(|(_, p)| u16::from_str_radix(p, 16).ok()) == Some(port)
        })
        .count()
}

fn exec_server_connections(proc_root: &Path, port: u16) -> Result<usize, String> {
    let mut found = None;
    for table in ["net/tcp", "net/tcp6"] {
        let path = proc_root.join(table);
        match std::fs::read_to_string(&path) {
            Ok(text) => *found.get_or_insert(0) += established_connections(&text, port),
            // Kernels without an address family omit its table entirely.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("reading {}: {e}", path.display())),
        }
    }
    found.ok_or_else(|| format!("no TCP table under {}", proc_root.display()))
}

/// A worktree's status.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Worktree {
    /// Paths with uncommitted changes, untracked files included.
    pub changes: usize,
    /// Whether the branch's upstream ref exists.
    pub upstream: bool,
    /// Commits the upstream does not have.
    pub ahead: u32,
}

/// The status of the worktree at `path`, from `git status`.
///
/// The worktree is the attempt's output and untrusted, so nothing in it may
/// run: system and global configuration are not read, and the repository's
/// own settings that could execute a program (fsmonitor, hooks, pager and
/// filters) are overridden. Filter discovery includes conditional config and
/// initialized submodules, whose status inherits those overrides. `git`
/// refuses a repository another user owns, which counts as a failed check.
/// Files that need a content filter to match the index are conservatively
/// reported as changed: the check cannot safely run that filter.
pub fn worktree(git: &str, path: &Path) -> Result<Worktree, String> {
    let program = process::resolve(git)?;
    let mut filters = BTreeSet::new();
    collect_filters(&program, path, &mut filters, 0)?;
    let done = git_command(
        &program,
        path,
        &filters,
        &[
            "status",
            "--porcelain=v1",
            "--branch",
            "--untracked-files=normal",
        ],
    )?;
    let out = git_output(done)?;
    Ok(parse_status(&String::from_utf8_lossy(&out)))
}

/// Read config and the index without querying file contents or running a
/// submodule's status. Drivers can exist only in a submodule or an includeIf
/// file, so discover them in each actual repository context before status.
fn collect_filters(
    program: &Path,
    path: &Path,
    filters: &mut BTreeSet<String>,
    depth: usize,
) -> Result<(), String> {
    if depth > 32 {
        return Err("submodule nesting exceeds 32 repositories".into());
    }
    let done = git_command(
        program,
        path,
        filters,
        &[
            "config",
            "--includes",
            "--null",
            "--name-only",
            "--get-regexp",
            r"^filter\..*\.(clean|smudge|process|required)$",
        ],
    )?;
    // git config exits 1 when no key matches, rather than emitting an error.
    if done.code != Some(1) {
        let out = git_output(done)?;
        let keys = std::str::from_utf8(&out).map_err(|e| format!("filter config: {e}"))?;
        for key in keys.split('\0').filter(|key| !key.is_empty()) {
            let (driver, _) = key.rsplit_once('.').ok_or("invalid filter config key")?;
            for setting in ["clean=", "smudge=", "process=", "required=false"] {
                filters.insert(format!("{driver}.{setting}"));
            }
        }
    }
    let out = git_output(git_command(
        program,
        path,
        filters,
        &["ls-files", "--stage", "-z"],
    )?)?;
    for entry in out
        .split(|b| *b == 0)
        .filter(|entry| entry.starts_with(b"160000 "))
    {
        let tab = entry
            .iter()
            .position(|b| *b == b'\t')
            .ok_or("invalid gitlink index entry")?;
        let name = &entry[tab + 1..];
        let name = std::str::from_utf8(name).map_err(|e| format!("submodule path: {e}"))?;
        let relative = Path::new(name);
        if relative
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err("invalid submodule path".into());
        }
        let module = path.join(relative);
        if module
            .join(".git")
            .try_exists()
            .map_err(|e| format!("inspecting {}: {e}", module.display()))?
        {
            collect_filters(program, &module, filters, depth + 1)?;
        }
    }
    Ok(())
}

fn git_command(
    program: &Path,
    path: &Path,
    filters: &BTreeSet<String>,
    command: &[&str],
) -> Result<crate::Completed, String> {
    let mut args = [
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.pager=cat",
        "--no-optional-locks",
    ]
    .map(String::from)
    .to_vec();
    for setting in filters {
        args.extend(["-c".into(), setting.clone()]);
    }
    args.extend(command.iter().map(|arg| (*arg).to_owned()));
    let env = [
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_OPTIONAL_LOCKS", "0"),
        ("LC_ALL", "C"),
    ]
    .map(|(k, v)| (k.to_owned(), v.to_owned()));
    process::run(
        program,
        &args,
        Some(path),
        &env,
        b"",
        Limits {
            time: Duration::from_secs(20),
            stdout: 1 << 20,
            stderr: process::STDERR_LIMIT,
            cpu_seconds: None,
            data_bytes: None,
        },
    )
    .map_err(|e| e.to_string())
}

fn git_output(done: crate::Completed) -> Result<Vec<u8>, String> {
    if !done.success() {
        return Err(format!(
            "git exited with {:?}: {}",
            done.code,
            String::from_utf8_lossy(&done.stderr).trim()
        ));
    }
    Ok(done.stdout)
}

/// `git status --porcelain=v1 --branch` output, as a [`Worktree`].
fn parse_status(out: &str) -> Worktree {
    let mut lines = out.lines();
    let branch = lines.next().unwrap_or_default();
    let changes = lines.filter(|l| !l.is_empty()).count();
    // `## main...origin/main [ahead 2, behind 1]`, `## main`, or
    // `## No commits yet on main`.
    let upstream =
        branch.starts_with("## ") && branch.contains("...") && !branch.ends_with(" [gone]");
    let ahead = branch
        .split_once("[ahead ")
        .and_then(|(_, rest)| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    Worktree {
        changes,
        upstream,
        ahead,
    }
}

#[cfg(all(test, unix))]
mod tests;
