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
/// Call it, then exit. The steps that were running stay unfinished, and
/// their attempt guards decide whether their commands may run again.
pub fn stop_all() -> usize {
    // Under the lock that spawning holds, so a process is either killed
    // here or never started.
    let running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    STOPPING.store(true, Ordering::SeqCst);
    for group in running.iter() {
        kill_group(*group);
    }
    running.len()
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

/// Ignore the stop signals in this process, as a runner does: a command in
/// its process group may signal the whole group, and the runner must live
/// to report how the command ended. The commands it starts get them back,
/// and SIGKILL, which stopping a runner uses, cannot be ignored.
pub fn ignore_stop_signals() {
    #[cfg(unix)]
    for signal in STOP_SIGNALS {
        // SAFETY: setting a signal's disposition to ignore has no
        // handler to run.
        #[allow(unsafe_code)]
        unsafe {
            libc::signal(signal, libc::SIG_IGN);
        }
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
                for signal in STOP_SIGNALS {
                    libc::signal(signal, libc::SIG_DFL);
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
    let writer = std::thread::spawn(move || input.write_all(&request));
    let stdout_limit = limits.stdout as u64 + 1;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(stdout_limit)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let stderr_limit = limits.stderr as u64;
    let errors = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut stderr = stderr;
        (&mut stderr).take(stderr_limit).read_to_end(&mut bytes)?;
        // Drain the rest so the process never blocks on a full pipe.
        std::io::copy(&mut stderr, &mut std::io::sink())?;
        Ok::<_, std::io::Error>(bytes)
    });
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
    let stdout = reader
        .join()
        .map_err(|_| io(std::io::Error::other("stdout reader panicked")))?
        .map_err(io)?;
    let stderr = errors
        .join()
        .map_err(|_| io(std::io::Error::other("stderr reader panicked")))?
        .map_err(io)?;
    let written = writer
        .join()
        .map_err(|_| io(std::io::Error::other("stdin writer panicked")))?;
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
