// SPDX-License-Identifier: Apache-2.0

//! A bounded child process: no shell, a cleared environment, a time limit,
//! and capped output. [`crate::LocalBackend`] runs commands with it, and so
//! does atomic's own trusted tooling, such as the Astro compiler, which is
//! not a runner and does not go through the backend trait.
//!
//! On Unix each process starts in a process group of its own, so stopping
//! it stops everything it started, and a terminal's signals reach atomic
//! alone. On Linux it also gets a parent-death signal, best effort: if the
//! thread that started it dies with atomic, so does the process. A process
//! that is stopping calls [`stop_all`] before it exits.

use crate::{Completed, ExecError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The process groups of the processes running now.
static RUNNING: Mutex<Vec<u32>> = Mutex::new(Vec::new());
/// Set by [`stop_all`]: this process is about to exit.
static STOPPING: AtomicBool = AtomicBool::new(false);

/// Kill the process group `group` started as.
fn kill_group(group: u32) {
    #[cfg(unix)]
    if let Ok(group) = libc::pid_t::try_from(group) {
        // SAFETY: killpg only sends a signal; a group that is gone is ESRCH.
        #[allow(unsafe_code)]
        unsafe {
            libc::killpg(group, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = group;
}

/// For a process that is about to exit, not to recover: kill every process
/// [`run`] started that has not ended, with everything it started, and
/// never report how any of them ended, so no step records an outcome its
/// own process did not reach. Returns how many were running.
///
/// On Linux it returns only once every process this one started, directly
/// or not, has ended: one in another process group or session, as a
/// sandbox's command is, included. Each is stopped where it is, so none
/// starts another while the rest are found, then killed, and waited for.
/// While some outlive their kill, `report` is given their process ids
/// after ten seconds and every thirty after that, and the wait goes on:
/// the process, and any lock it holds, outlasts every command it stopped.
///
/// Call it, then exit. The steps that were running stay unfinished, and
/// their attempt guards decide whether their commands may run again.
pub fn stop_all(report: &mut dyn FnMut(&[u32])) -> usize {
    // Under the lock that spawning holds, so a process is either killed
    // here or never started.
    let running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    STOPPING.store(true, Ordering::SeqCst);
    #[cfg(target_os = "linux")]
    let stopped = descendants::stop();
    for group in running.iter() {
        kill_group(*group);
    }
    #[cfg(target_os = "linux")]
    descendants::kill_and_wait(&stopped, report);
    #[cfg(not(target_os = "linux"))]
    let _ = report;
    running.len()
}

/// The processes this one started, directly or not, found through
/// `/proc`: each known by its process id and when it started, which tells
/// it from a later process given the same id.
#[cfg(target_os = "linux")]
mod descendants {
    use std::collections::{HashMap, HashSet};
    use std::time::{Duration, Instant};

    /// A process, by its id and its start time in clock ticks after boot.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub(super) struct Process {
        pid: libc::pid_t,
        started: u64,
    }

    /// Its state, parent and start time, from `/proc/<pid>/stat`.
    fn stat(pid: libc::pid_t) -> Option<(char, libc::pid_t, u64)> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // After the command name, which may hold spaces and parentheses.
        let fields: Vec<&str> = text.rsplit_once(')')?.1.split_whitespace().collect();
        let state = fields.first()?.chars().next()?;
        let parent = fields.get(1)?.parse().ok()?;
        // Field 22 of the whole line; the 20th after the name.
        let started = fields.get(19)?.parse().ok()?;
        Some((state, parent, started))
    }

    /// Whether `process` still runs: there, the same process, and neither
    /// a zombie nor dead.
    fn alive(process: &Process) -> bool {
        stat(process.pid).is_some_and(|(state, _, started)| {
            started == process.started && !matches!(state, 'Z' | 'X' | 'x')
        })
    }

    /// Every process now running whose parents lead to this one.
    fn found() -> Vec<Process> {
        let mut children: HashMap<libc::pid_t, Vec<Process>> = HashMap::new();
        for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else {
                continue;
            };
            if let Some((state, parent, started)) = stat(pid)
                && !matches!(state, 'Z' | 'X' | 'x')
            {
                children
                    .entry(parent)
                    .or_default()
                    .push(Process { pid, started });
            }
        }
        let mut out = Vec::new();
        let mut next = vec![libc::pid_t::try_from(std::process::id()).unwrap_or(0)];
        while let Some(parent) = next.pop() {
            for child in children.remove(&parent).unwrap_or_default() {
                next.push(child.pid);
                out.push(child);
            }
        }
        out
    }

    /// Send `signal` to `process` if it is still the same process.
    fn signal(process: &Process, signal: libc::c_int) {
        if stat(process.pid).is_some_and(|(_, _, started)| started == process.started) {
            // SAFETY: kill only sends a signal.
            #[allow(unsafe_code)]
            unsafe {
                libc::kill(process.pid, signal);
            }
        }
    }

    /// Stop every process this one started, directly or not, where it is,
    /// looking again until no new one appears: a stopped process starts
    /// nothing more.
    pub(super) fn stop() -> Vec<Process> {
        let mut stopped: HashSet<Process> = HashSet::new();
        loop {
            let new: Vec<Process> = found()
                .into_iter()
                .filter(|p| !stopped.contains(p))
                .collect();
            if new.is_empty() {
                break;
            }
            for process in new {
                signal(&process, libc::SIGSTOP);
                stopped.insert(process);
            }
        }
        stopped.into_iter().collect()
    }

    /// Kill each of `stopped` and wait until none runs, giving `report` the
    /// ids of those still running after ten seconds and every thirty after.
    pub(super) fn kill_and_wait(stopped: &[Process], report: &mut dyn FnMut(&[u32])) {
        for process in stopped {
            signal(process, libc::SIGKILL);
        }
        let begun = Instant::now();
        let mut next = Duration::from_secs(10);
        loop {
            let left: Vec<u32> = stopped
                .iter()
                .filter(|p| alive(p))
                .filter_map(|p| u32::try_from(p.pid).ok())
                .collect();
            if left.is_empty() {
                return;
            }
            if begun.elapsed() >= next {
                report(&left);
                next += Duration::from_secs(30);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Which process group a process [`run_in`] starts runs in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    /// One of its own, which its time limit and [`stop_all`] kill, with
    /// everything it started; whatever is left in it when the process ends
    /// is killed then, so nothing it left behind outlives it or holds its
    /// output open.
    Own,
    /// This process's own: for the command of a runner, so that stopping
    /// the runner's group stops the command and everything it started. Its
    /// time limit kills the command alone; the rest goes with the runner's
    /// group.
    Inherit,
}

/// The signals that stop a process from a terminal or a supervisor.
#[cfg(unix)]
const STOP_SIGNALS: [libc::c_int; 4] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT];

/// The signals that stop a process from a terminal or a supervisor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopSignal {
    /// SIGTERM.
    Term,
    /// SIGINT.
    Int,
    /// SIGHUP.
    Hup,
    /// SIGQUIT.
    Quit,
}

impl StopSignal {
    #[cfg(unix)]
    fn number(self) -> libc::c_int {
        match self {
            Self::Term => libc::SIGTERM,
            Self::Int => libc::SIGINT,
            Self::Hup => libc::SIGHUP,
            Self::Quit => libc::SIGQUIT,
        }
    }
}

/// Whether this process ignores `signal`, as `nohup`, or a shell starting a
/// background job, makes it. A process that handles signals keeps an
/// inherited ignore, so check before registering a handler.
pub fn ignored(signal: StopSignal) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: with a null new action, sigaction only reads the current
        // one into `old`, which an all-zero value initialises.
        #[allow(unsafe_code)]
        unsafe {
            let mut old: libc::sigaction = std::mem::zeroed();
            libc::sigaction(signal.number(), std::ptr::null(), &mut old) == 0
                && old.sa_sigaction == libc::SIG_IGN
        }
    }
    #[cfg(not(unix))]
    {
        let _ = signal;
        false
    }
}

/// The stop signals this process ignores only because it is a runner,
/// one bit per signal in [`STOP_SIGNALS`]: the commands it starts get them
/// back. A signal it inherited as ignored stays ignored for them too.
static RESTORE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Ignore the stop signals in this process, as a runner does: a command in
/// its process group may signal the whole group, and the runner must live
/// to report how the command ended. The commands it starts get back each
/// one this process did not already ignore, and SIGKILL, which stopping a
/// runner uses, cannot be ignored.
pub fn ignore_stop_signals() {
    #[cfg(unix)]
    for (bit, signal) in [
        StopSignal::Term,
        StopSignal::Int,
        StopSignal::Hup,
        StopSignal::Quit,
    ]
    .into_iter()
    .enumerate()
    {
        if ignored(signal) {
            continue;
        }
        // SAFETY: setting a signal's disposition to ignore has no
        // handler to run.
        #[allow(unsafe_code)]
        unsafe {
            libc::signal(signal.number(), libc::SIG_IGN);
        }
        RESTORE.fetch_or(1 << bit, Ordering::SeqCst);
    }
}

/// Wait here for good if the process is stopping: its exit ends the wait.
fn unless_stopping() {
    if STOPPING.load(Ordering::SeqCst) {
        stopped();
    }
}

/// Wait for the exit of a process that is stopping.
fn stopped() -> ! {
    loop {
        std::thread::park();
    }
}

/// The most stderr kept, in bytes; anything beyond it is dropped.
pub const STDERR_LIMIT: usize = 65_536;

/// How long a process may run and how much it may write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Wall-clock time before it is killed.
    pub time: Duration,
    /// The most stdout, in bytes; more is an error.
    pub stdout: usize,
    /// The most stderr kept, in bytes; more is dropped.
    pub stderr: usize,
}

/// `program` as a path: as given when it names a path, else the first match
/// on this process's `PATH`. The child's own environment never has `PATH`
/// unless the caller passes one, so the lookup happens here.
pub fn resolve(program: &str) -> Result<PathBuf, String> {
    if program.contains('/') {
        return Ok(PathBuf::from(program));
    }
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(program))
                .find(|file| file.is_file())
        })
        .ok_or_else(|| format!("{program} is not on PATH"))
}

/// Run `program` with `args`, in `cwd` (or the current directory), with
/// exactly `env`, `stdin` on its standard input, within `limits`.
///
/// A process that exits, with any code, has completed. A broken pipe on its
/// stdin only means it stopped reading; its exit code and stderr say why.
pub fn run(
    program: &Path,
    args: &[String],
    cwd: Option<&Path>,
    env: &[(String, String)],
    stdin: &[u8],
    limits: Limits,
) -> Result<Completed, ExecError> {
    run_in(Group::Own, program, args, cwd, env, stdin, limits)
}

/// [`run`], in the process group `group` says.
pub fn run_in(
    group: Group,
    program: &Path,
    args: &[String],
    cwd: Option<&Path>,
    env: &[(String, String)],
    stdin: &[u8],
    limits: Limits,
) -> Result<Completed, ExecError> {
    let name = program.display().to_string();
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    #[cfg(unix)]
    if group != Group::Inherit {
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
    }
    #[cfg(unix)]
    {
        let parent = libc::pid_t::try_from(std::process::id()).unwrap_or(0);
        // SAFETY: between fork and exec the closure makes only
        // async-signal-safe calls (signal, prctl, getppid, _exit) and
        // allocates nothing.
        #[allow(unsafe_code)]
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut command, move || {
                // A runner ignores the stop signals, which its command,
                // in its group, could send it; the command gets them back.
                let restore = RESTORE.load(Ordering::SeqCst);
                for (bit, signal) in STOP_SIGNALS.into_iter().enumerate() {
                    if restore & (1 << bit) != 0 {
                        libc::signal(signal, libc::SIG_DFL);
                    }
                }
                #[cfg(target_os = "linux")]
                {
                    // Best effort: where prctl is refused, the process
                    // group still lets atomic stop it.
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0);
                    // atomic died before the signal was set.
                    if libc::getppid() != parent {
                        libc::_exit(137);
                    }
                }
                #[cfg(not(target_os = "linux"))]
                let _ = parent;
                Ok(())
            });
        }
    }
    // Checked, started and registered under one lock, which stop_all takes
    // too: a process that is stopping starts nothing more.
    let mut running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    if STOPPING.load(Ordering::SeqCst) {
        drop(running);
        stopped();
    }
    let child = command.spawn().map_err(|e| ExecError::Start {
        program: name.clone(),
        message: e.to_string(),
    })?;
    // A process in this one's group is stopped with this process.
    let own = (group != Group::Inherit).then(|| child.id());
    running.extend(own);
    drop(running);
    let done = finish(child, &name, stdin, limits, own);
    if let Some(own) = own {
        RUNNING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|g| *g != own);
    }
    // A process that is stopping killed it: how it ended is not its result.
    unless_stopping();
    done
}

/// Whether `child` has exited: `Some(true)` if it has and is not yet
/// reaped, `Some(false)` where that cannot be told apart from reaping, and
/// `None` if it still runs.
fn exited(child: &mut std::process::Child) -> std::io::Result<Option<bool>> {
    #[cfg(target_os = "linux")]
    {
        let pid = libc::id_t::from(child.id());
        // SAFETY: an all-zero siginfo_t is valid; waitid with WNOWAIT only
        // reads the child's state and leaves it to be reaped.
        #[allow(unsafe_code)]
        let found = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let r = libc::waitid(
                libc::P_PID,
                pid,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            );
            if r != 0 {
                return Err(std::io::Error::last_os_error());
            }
            info.si_pid() != 0
        };
        Ok(found.then_some(true))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(child.try_wait()?.map(|_| false))
    }
}

/// How long after a process exits its output may still arrive.
const OUTPUT_GRACE: Duration = Duration::from_secs(2);

/// One of a process's output streams, read on a thread of its own.
struct Output {
    chunks: std::sync::mpsc::Receiver<std::io::Result<Option<Vec<u8>>>>,
}

impl Output {
    /// Read up to `keep` bytes of `stream`, and with `drain` the rest
    /// unkept, so the process never blocks on a full pipe.
    fn read(stream: impl Read + Send + 'static, keep: u64, drain: bool) -> Self {
        let (send, chunks) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut stream = stream;
            let mut kept = 0u64;
            let mut buffer = vec![0u8; 65_536];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => {
                        let _ = send.send(Ok(None));
                        return;
                    }
                    Ok(n) => {
                        let room = usize::try_from(keep.saturating_sub(kept)).unwrap_or(usize::MAX);
                        let take = n.min(room);
                        kept += take as u64;
                        if take > 0 && send.send(Ok(Some(buffer[..take].to_vec()))).is_err() {
                            return;
                        }
                        if kept >= keep && !drain {
                            let _ = send.send(Ok(None));
                            return;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        let _ = send.send(Err(e));
                        return;
                    }
                }
            }
        });
        Self { chunks }
    }

    /// What arrived by its end, or by `deadline` if it has not ended.
    fn until(self, deadline: Instant) -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.chunks.recv_timeout(left) {
                Ok(Ok(Some(chunk))) => bytes.extend(chunk),
                Ok(Ok(None)) => return Ok(bytes),
                Ok(Err(e)) => return Err(e),
                // Ended without a word, or still held open.
                Err(_) => return Ok(bytes),
            }
        }
    }
}

fn finish(
    mut child: std::process::Child,
    name: &str,
    stdin: &[u8],
    limits: Limits,
    group: Option<u32>,
) -> Result<Completed, ExecError> {
    let name = name.to_owned();
    let io = |e: std::io::Error| ExecError::Io {
        program: name.clone(),
        message: e.to_string(),
    };
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| io(std::io::Error::other("no stdin")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io(std::io::Error::other("no stdout")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io(std::io::Error::other("no stderr")))?;
    let request = stdin.to_vec();
    let (wrote, written) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = wrote.send(input.write_all(&request));
    });
    let stdout = Output::read(stdout, limits.stdout as u64 + 1, false);
    let stderr = Output::read(stderr, limits.stderr as u64, true);
    let started = Instant::now();
    let status = loop {
        match exited(&mut child).map_err(io)? {
            Some(true) => {
                // Its group is killed while the process is a zombie, so the
                // group's id cannot yet belong to anyone else; then reaped.
                if let Some(group) = group {
                    kill_group(group);
                }
                break child.wait().map_err(io)?;
            }
            Some(false) => {
                // Reaped already: its group goes after it, best effort.
                if let Some(group) = group {
                    kill_group(group);
                }
                if let Some(status) = child.try_wait().map_err(io)? {
                    break status;
                }
            }
            None if started.elapsed() > limits.time => {
                // Everything it started, not only the process itself.
                if let Some(group) = group {
                    kill_group(group);
                }
                let _ = child.kill();
                let _ = child.wait();
                return Err(ExecError::TimedOut {
                    program: name,
                    seconds: limits.time.as_secs(),
                });
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    // Its output ends when every process holding it has let go. One that
    // left its group, or shares a runner's, may hold it for good: what came
    // within the grace after the exit is the output.
    let deadline = Instant::now() + OUTPUT_GRACE;
    let stdout = stdout.until(deadline).map_err(io)?;
    let stderr = stderr.until(deadline).map_err(io)?;
    // Input it never read is no failure once it has exited.
    let written = written
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .unwrap_or(Ok(()));
    if stdout.len() > limits.stdout {
        return Err(ExecError::TooLarge {
            program: name,
            limit: limits.stdout,
        });
    }
    if let Err(e) = written
        && e.kind() != std::io::ErrorKind::BrokenPipe
    {
        return Err(io(e));
    }
    Ok(Completed {
        code: status.code(),
        stdout,
        stderr,
    })
}
