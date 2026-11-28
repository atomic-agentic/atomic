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

/// A command's writable-data limit on macOS, held by the process that waits
/// on it.
///
/// macOS refuses an `RLIMIT_DATA` or `RLIMIT_AS` below the size of a
/// process's whole address map, and an arm64 process starts with hundreds of
/// gigabytes mapped, so a command cannot be given a limit near the memory it
/// may hold. While it waits, the parent reads the physical footprint of each
/// process in the command's group, the memory in use that macOS itself
/// limits processes by, and stops the command when one holds more than its
/// limit. As on Linux the limit is each process's own, not the group's
/// total. The kernel does not hold it, so between two readings a process
/// can go past it by what it writes in that time. A process that left the
/// command's group is not read. A process in the group whose memory cannot
/// be read stops the command.
#[cfg(target_os = "macos")]
mod memory {
    /// Why the command must stop: a process in `group`, or `child` where it
    /// has none of its own, holds more than `limit` bytes or cannot be read.
    pub(super) fn over(limit: u64, child: u32, group: Option<u32>) -> Option<String> {
        let pids = match group {
            Some(group) => match members(group) {
                Ok(pids) => pids,
                Err(e) => {
                    return Some(format!(
                        "atomic stopped it: its processes could not be listed to hold its \
                         memory limit ({e})"
                    ));
                }
            },
            None => libc::pid_t::try_from(child).into_iter().collect(),
        };
        for pid in pids {
            match footprint(pid) {
                Ok(Some(held)) if held > limit => {
                    return Some(format!(
                        "atomic stopped it: process {pid} held {held} bytes, more than its \
                         memory limit of {limit}"
                    ));
                }
                Ok(_) => {}
                Err(e) => {
                    return Some(format!(
                        "atomic stopped it: the memory of process {pid} could not be read to \
                         hold its limit ({e})"
                    ));
                }
            }
        }
        None
    }

    /// The processes in process group `group`, as libproc lists them.
    fn members(group: u32) -> std::io::Result<Vec<libc::pid_t>> {
        let group = libc::pid_t::try_from(group).map_err(std::io::Error::other)?;
        let mut room = 64usize;
        loop {
            let mut pids: Vec<libc::pid_t> = vec![0; room];
            let bytes = libc::c_int::try_from(room * std::mem::size_of::<libc::pid_t>())
                .map_err(|_| std::io::Error::other("too many processes to list"))?;
            // SAFETY: the buffer holds `bytes` bytes, which is all
            // proc_listpgrppids writes.
            #[allow(unsafe_code)]
            let listed = unsafe { libc::proc_listpgrppids(group, pids.as_mut_ptr().cast(), bytes) };
            let Ok(listed) = usize::try_from(listed) else {
                return Err(std::io::Error::last_os_error());
            };
            // A full buffer may have left some out: look again with more.
            if listed < room {
                pids.truncate(listed);
                pids.retain(|p| *p > 0);
                return Ok(pids);
            }
            room *= 4;
        }
    }

    /// The physical footprint of `pid` in bytes, or `None` for a process
    /// that has ended or is ending.
    #[allow(unsafe_code)]
    fn footprint(pid: libc::pid_t) -> std::io::Result<Option<u64>> {
        let mut usage = std::mem::MaybeUninit::<libc::rusage_info_v0>::zeroed();
        // SAFETY: `usage` is writable storage with the size and alignment of
        // the structure the V0 flavor fills, and it lives until the call
        // returns.
        let result =
            unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V0, usage.as_mut_ptr().cast()) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::ESRCH) {
                Ok(None)
            } else {
                Err(error)
            };
        }
        // SAFETY: the call filled the structure, whose fields are plain
        // numbers that zeroed storage is valid for in any case.
        Ok(Some(unsafe { usage.assume_init() }.ri_phys_footprint))
    }
}

/// For a process that is about to exit, not to recover: kill every process
/// [`run`] started that has not ended, with everything it started, and
/// never report how any of them ended, so no step records an outcome its
/// own process did not reach. Returns how many were running.
///
/// On Linux it then ends every other process descending from this one, as
/// [`end_all`] does, and returns only once they have ended.
///
/// Call it, then exit, calling [`end_all`] just before. The steps that were
/// running stay unfinished, and their attempt guards decide whether their
/// commands may run again.
pub fn stop_all(report: &mut dyn FnMut(&str)) -> usize {
    // Under the lock that spawning holds, so a process is either killed
    // here or never started.
    let running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    STOPPING.store(true, Ordering::SeqCst);
    // On Linux, everything is found and stopped before any group is killed:
    // a process whose parent died first would have left this one's
    // descendants, and not be found.
    #[cfg(target_os = "linux")]
    descendants::end(&running, report);
    #[cfg(not(target_os = "linux"))]
    {
        for group in running.iter() {
            kill_group(*group);
        }
        let _ = report;
    }
    running.len()
}

/// For a process that is about to exit: on Linux, end every process
/// descending from this one that runs now or starts while this waits, found
/// through `/proc`, one in another process group or session, as a sandbox's
/// command is, included, and return once each has ended. Each found at
/// first is stopped where it is, for at most a second of looking, every
/// look included, so none starts another while the rest are found; then
/// the process groups and all found are killed, and any found while this
/// waits is killed on sight, each look again at most a second. Atomic's own
/// tools, such as its git, are waited for, never killed: git's own time
/// limit, ten minutes, bounds that wait. `report` is told what outlives its
/// kill after ten seconds and every thirty after that, and the wait goes on,
/// so the process, and any lock it holds, outlasts them all: only SIGKILL
/// ends it. A process that left this one's descendants, as one that
/// daemonizes outside a sandbox's process namespace does, is not found.
/// Elsewhere than Linux it does nothing more than [`stop_all`]'s kill.
/// Returns how many commands [`run`] had running.
pub fn end_all(report: &mut dyn FnMut(&str)) -> usize {
    stop_all(report)
}

/// Whether this process is stopping ([`stop_all`]): the commands it ran
/// may have been killed, so no step whose work may have depended on one
/// should report, or record, how it ended.
pub fn stopping() -> bool {
    STOPPING.load(Ordering::SeqCst)
}

/// The processes of atomic's own tools, such as its git, which stopping
/// waits for and never kills: a git killed mid-commit could leave its
/// repository locked.
/// Each by its process id and start time (see `descendants`).
static OWN_TOOLS: Mutex<Vec<(u32, u64)>> = Mutex::new(Vec::new());

/// A process [`spawn_own_tool`] started, counted as atomic's own until
/// this is dropped.
#[derive(Debug)]
pub struct OwnTool(u32);

impl Drop for OwnTool {
    fn drop(&mut self) {
        OWN_TOOLS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(pid, _)| *pid != self.0);
    }
}

/// Start `command` as one of atomic's own tools: a process that stopping
/// waits for and never kills (see [`end_all`]), in a process group of its
/// own, which [`kill_own_tool`] kills. Started and counted under the lock
/// stopping takes, so it is never found before it is counted.
pub fn spawn_own_tool(
    command: &mut std::process::Command,
) -> std::io::Result<(std::process::Child, OwnTool)> {
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command, 0);
    let mut own = OWN_TOOLS.lock().unwrap_or_else(|e| e.into_inner());
    let child = command.spawn()?;
    let pid = child.id();
    #[cfg(target_os = "linux")]
    let started = descendants::started(pid).unwrap_or(0);
    #[cfg(not(target_os = "linux"))]
    let started = 0;
    own.push((pid, started));
    Ok((child, OwnTool(pid)))
}

/// Kill a tool [`spawn_own_tool`] started, with everything in its process
/// group, as when it has run past its time limit, and reap it.
pub fn kill_own_tool(child: &mut std::process::Child) {
    kill_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

/// The processes descending from this one, found through `/proc`: each
/// known by its process id and when it started, which tells it from a
/// later process given the same id.
#[cfg(target_os = "linux")]
mod descendants {
    use super::OWN_TOOLS;
    use std::collections::{HashMap, HashSet};
    use std::io::ErrorKind;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    /// How long the processes found at first are stopped and looked for
    /// again before all are killed.
    const STOP_PHASE: Duration = Duration::from_secs(1);
    /// The longest one look in the wait may take, however often looks have
    /// run out of time.
    const LOOK_MAX: Duration = Duration::from_secs(60);
    /// When those still running are first reported, and how often after.
    const FIRST_REPORT: Duration = Duration::from_secs(10);
    const REPORT_EVERY: Duration = Duration::from_secs(30);

    /// A process, by its id and its start time in clock ticks after boot.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    struct Process {
        pid: libc::pid_t,
        started: u64,
    }

    /// What `/proc/<pid>/stat` says of a process.
    struct Stat {
        state: char,
        parent: libc::pid_t,
        started: u64,
    }

    /// Read `/proc/<pid>/stat`: `Ok(None)` when no such process is there.
    fn stat(pid: libc::pid_t) -> std::io::Result<Option<Stat>> {
        let text = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(text) => text,
            Err(e)
                if matches!(e.kind(), ErrorKind::NotFound)
                    || e.raw_os_error() == Some(libc::ESRCH) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let bad =
            || std::io::Error::new(ErrorKind::InvalidData, format!("/proc/{pid}/stat: {text}"));
        // After the command name, which may hold spaces and parentheses.
        let fields: Vec<&str> = text
            .rsplit_once(')')
            .ok_or_else(bad)?
            .1
            .split_whitespace()
            .collect();
        let state = fields
            .first()
            .and_then(|f| f.chars().next())
            .ok_or_else(bad)?;
        let parent = fields.get(1).and_then(|f| f.parse().ok()).ok_or_else(bad)?;
        // Field 22 of the whole line; the 20th after the name.
        let started = fields
            .get(19)
            .and_then(|f| f.parse().ok())
            .ok_or_else(bad)?;
        Ok(Some(Stat {
            state,
            parent,
            started,
        }))
    }

    fn ended(state: char) -> bool {
        matches!(state, 'Z' | 'X' | 'x')
    }

    /// When process `pid` started, if it is there.
    pub(super) fn started(pid: u32) -> Option<u64> {
        stat(libc::pid_t::try_from(pid).ok()?)
            .ok()
            .flatten()
            .map(|s| s.started)
    }

    /// Whether a process whose leader shows as a zombie has a thread that is
    /// not: one that cannot be read counts as running.
    fn threads_run(pid: libc::pid_t) -> bool {
        match std::fs::read_dir(format!("/proc/{pid}/task")) {
            Ok(tasks) => tasks.flatten().any(|task| {
                match std::fs::read_to_string(task.path().join("stat")) {
                    Ok(text) => text
                        .rsplit_once(')')
                        .and_then(|(_, rest)| rest.split_whitespace().next())
                        .and_then(|f| f.chars().next())
                        .is_none_or(|state| !ended(state)),
                    Err(e) => e.kind() != ErrorKind::NotFound,
                }
            }),
            Err(e) => e.kind() != ErrorKind::NotFound,
        }
    }

    /// Whether `process` may still run: there, the same process, and with a
    /// thread that is neither a zombie nor dead. What cannot be read counts
    /// as running.
    fn alive(process: &Process) -> bool {
        match stat(process.pid) {
            Ok(None) => false,
            Ok(Some(stat)) if stat.started != process.started => false,
            Ok(Some(stat)) if !ended(stat.state) => true,
            // A leader that has exited while its other threads run shows as
            // a zombie: look at each thread.
            Ok(Some(_)) => threads_run(process.pid),
            Err(_) => true,
        }
    }

    /// Every process now running whose parents lead to this one, each
    /// started no earlier than its parent, as far as `/proc` can be read by
    /// `deadline`: processes that keep starting others cannot hold one look
    /// past it. Says whether the look was complete; reports those that
    /// cannot be read, telling `unreadable` which have been reported
    /// already. That `proc` cannot be listed at all is reported once for a
    /// whole stop, which `listed` records: the wait looks again every few
    /// milliseconds and the reason never changes.
    fn found(
        proc: &Path,
        report: &mut dyn FnMut(&str),
        unreadable: &mut HashSet<libc::pid_t>,
        listed: &mut bool,
        deadline: Instant,
    ) -> (Vec<(Process, libc::pid_t)>, bool) {
        let me = libc::pid_t::try_from(std::process::id()).unwrap_or(0);
        let mut children: HashMap<libc::pid_t, Vec<(Process, libc::pid_t)>> = HashMap::new();
        let mut own_start = 0;
        let entries = match std::fs::read_dir(proc) {
            Ok(entries) => entries,
            Err(e) => {
                // Once for the whole stop: the wait looks again every few
                // milliseconds, and the reason never changes.
                if !*listed {
                    *listed = true;
                    report(&format!(
                        "cannot list {}, so the processes this one started cannot be found: {e}",
                        proc.display()
                    ));
                }
                return (Vec::new(), false);
            }
        };
        let mut complete = true;
        for entry in entries.flatten() {
            if Instant::now() >= deadline {
                complete = false;
                break;
            }
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<libc::pid_t>().ok())
            else {
                continue;
            };
            match stat(pid) {
                Ok(Some(stat)) if pid == me => own_start = stat.started,
                Ok(Some(stat)) if !ended(stat.state) || threads_run(pid) => {
                    children.entry(stat.parent).or_default().push((
                        Process {
                            pid,
                            started: stat.started,
                        },
                        stat.parent,
                    ))
                }
                Ok(_) => {}
                Err(e) => {
                    if unreadable.insert(pid) {
                        report(&format!("cannot read process {pid} in /proc: {e}"));
                    }
                }
            }
        }
        let mut starts: HashMap<libc::pid_t, u64> = HashMap::from([(me, own_start)]);
        let mut out = Vec::new();
        let mut next = vec![me];
        while let Some(parent) = next.pop() {
            let parent_started = starts.get(&parent).copied().unwrap_or(0);
            for (child, _) in children.remove(&parent).unwrap_or_default() {
                // A process older than its parent had a parent once given the
                // same id: not this one's.
                if child.started < parent_started {
                    continue;
                }
                starts.insert(child.pid, child.started);
                next.push(child.pid);
                out.push((child, parent));
            }
        }
        (out, complete)
    }

    /// The processes of atomic's own tools, and every process they started.
    fn own(found: &[(Process, libc::pid_t)], tools: &[(u32, u64)]) -> HashSet<Process> {
        let tools: HashSet<Process> = tools
            .iter()
            .filter_map(|(pid, started)| {
                Some(Process {
                    pid: libc::pid_t::try_from(*pid).ok()?,
                    started: *started,
                })
            })
            .collect();
        let mut own: HashSet<libc::pid_t> = HashSet::new();
        let mut out = HashSet::new();
        // `found` lists parents before their children.
        for (process, parent) in found {
            if tools.contains(process) || own.contains(parent) {
                own.insert(process.pid);
                out.insert(*process);
            }
        }
        out
    }

    /// Send `signal` to `process` if it is still the same process, through
    /// a process file descriptor where there is one, so that a process
    /// given its id since cannot get it.
    fn signal(process: &Process, signal: libc::c_int) {
        let same = || {
            stat(process.pid)
                .ok()
                .flatten()
                .is_some_and(|s| s.started == process.started)
        };
        // SAFETY: the syscalls take plain integers and a null pointer, and
        // the descriptor opened is closed here.
        #[allow(unsafe_code)]
        unsafe {
            let fd = libc::syscall(libc::SYS_pidfd_open, process.pid, 0);
            if fd >= 0 {
                if same() {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd,
                        signal,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                }
                libc::close(fd as libc::c_int);
            } else if std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) && same()
            {
                // No descriptor to be had, as where a seccomp profile refuses
                // the call or descriptors run out: by id, just checked.
                libc::kill(process.pid, signal);
            }
        }
    }

    /// Stop what `find` finds with `stop`, looking again until nothing new
    /// appears or `limit` is up, whichever is first: processes that keep
    /// starting others cannot hold the stop. Returns what was stopped, and
    /// whether nothing new appeared before the limit.
    fn stop_phase<P: Copy + Eq + std::hash::Hash>(
        find: &mut dyn FnMut(Instant) -> (Vec<P>, bool),
        stop: &mut dyn FnMut(&P),
        limit: Duration,
    ) -> (HashSet<P>, bool) {
        let deadline = Instant::now() + limit;
        let mut found = HashSet::new();
        loop {
            let (looked, complete) = find(deadline);
            let new: Vec<P> = looked.into_iter().filter(|p| !found.contains(p)).collect();
            let none = new.is_empty();
            // Everything a look found is kept, to be killed, even past the
            // deadline, which ends only the stopping and the looking.
            for process in new {
                if Instant::now() < deadline {
                    stop(&process);
                }
                found.insert(process);
            }
            if none && complete {
                return (found, Instant::now() < deadline);
            }
            if Instant::now() >= deadline {
                return (found, false);
            }
        }
    }

    /// See [`super::end_all`].
    pub(super) fn end(groups: &[u32], report: &mut dyn FnMut(&str)) {
        // Held throughout: atomic's own tools start before this or after.
        let tools = OWN_TOOLS.lock().unwrap_or_else(|e| e.into_inner());
        let me = std::process::id().to_string();
        if std::fs::read_link("/proc/self")
            .ok()
            .and_then(|p| p.to_str().map(str::to_owned))
            != Some(me)
        {
            for group in groups {
                super::kill_group(*group);
            }
            static SAID: AtomicBool = AtomicBool::new(false);
            if !SAID.swap(true, Ordering::SeqCst) {
                report(
                    "this process's /proc is not its own, so the processes it started cannot \
                     be found; only their process groups were killed",
                );
            }
            return;
        }
        // From here a process whose parent dies comes to this one, rather
        // than to the system's first process, so it is still found: the
        // first process of a sandbox's process namespace, say, once the
        // group kill below has ended what started it.
        // SAFETY: prctl with these arguments only sets a flag of this
        // process.
        #[allow(unsafe_code)]
        unsafe {
            libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
        }
        let mut unreadable = HashSet::new();
        let mut listed = false;
        let begun = Instant::now();
        let (mut watched, settled) = stop_phase(
            &mut |deadline| {
                let (all, complete) = found(
                    Path::new("/proc"),
                    report,
                    &mut unreadable,
                    &mut listed,
                    deadline,
                );
                let own = own(&all, &tools);
                let others = all
                    .into_iter()
                    .map(|(p, _)| p)
                    .filter(|p| !own.contains(p))
                    .collect();
                (others, complete)
            },
            &mut |process| signal(process, libc::SIGSTOP),
            STOP_PHASE,
        );
        if !settled {
            report(&format!(
                "the processes of stopped commands were still starting others after {}s; \
                 killing the {} found, and any that start, as they are found",
                STOP_PHASE.as_secs(),
                watched.len()
            ));
        }
        for group in groups {
            super::kill_group(*group);
        }
        for process in &watched {
            signal(process, libc::SIGKILL);
        }
        // Wait, killing whatever appears, until nothing descending from
        // this one runs.
        let report = std::cell::RefCell::new(report);
        wait_phase(
            &mut watched,
            &mut |deadline| {
                let (all, complete) = found(
                    Path::new("/proc"),
                    &mut |what| (report.borrow_mut())(what),
                    &mut unreadable,
                    &mut listed,
                    deadline,
                );
                // The wait's own: waited for, never killed, unlike the
                // others it finds.
                let waited = own(&all, &tools);
                let others = all
                    .into_iter()
                    .map(|(p, _)| p)
                    .filter(|p| !waited.contains(p))
                    .collect();
                (others, waited.into_iter().collect(), complete)
            },
            &mut |process| signal(process, libc::SIGKILL),
            &mut |process| alive(process),
            &mut |what| (report.borrow_mut())(what),
            &|process| process.pid.to_string(),
            begun,
        );
    }

    /// How long the wait's next look may take, after one that found nothing
    /// running and was incomplete: twice as long, up to [`LOOK_MAX`], if it
    /// `ran_out` of time; as long, if it ended at once, as when /proc
    /// cannot be listed.
    fn next_look(look: Duration, ran_out: bool) -> Duration {
        if ran_out {
            look.saturating_mul(2).min(LOOK_MAX)
        } else {
            look
        }
    }

    /// Wait until nothing `find` finds, nor anything in `watched`, is
    /// `alive`, killing each process found that is not yet watched, and
    /// watching it: `find` gives what runs that may be killed, the processes
    /// of atomic's own tools, which are only waited for, and whether its look
    /// was complete. Only a complete look can show that nothing runs: after
    /// one cut short, the next may take twice as long. `report` is told what
    /// is left after ten seconds from `begun`, and every thirty after.
    fn wait_phase<P: Copy + Eq + std::hash::Hash>(
        watched: &mut HashSet<P>,
        find: &mut dyn FnMut(Instant) -> (Vec<P>, Vec<P>, bool),
        kill: &mut dyn FnMut(&P),
        alive: &mut dyn FnMut(&P) -> bool,
        report: &mut dyn FnMut(&str),
        name: &dyn Fn(&P) -> String,
        begun: Instant,
    ) {
        let mut look = STOP_PHASE;
        let mut next = FIRST_REPORT;
        loop {
            let looked = Instant::now();
            let (others, own, complete) = find(looked + look);
            // Out of time, rather than cut short at once, as when /proc
            // cannot be listed at all.
            let ran_out = !complete && looked.elapsed() >= look;
            for process in others {
                if watched.insert(process) {
                    kill(&process);
                }
            }
            let left: Vec<String> = watched
                .iter()
                .chain(&own)
                .filter(|p| alive(p))
                .map(name)
                .collect();
            if left.is_empty() && complete {
                return;
            }
            if left.is_empty() {
                look = next_look(look, ran_out);
            }
            if begun.elapsed() >= next {
                if left.is_empty() {
                    report(&format!(
                        "a look through /proc for the processes of stopped commands did not \
                         finish in time; looking again, for up to {}s, and the execution lock \
                         stays held until one does",
                        look.as_secs()
                    ));
                } else {
                    report(&format!(
                        "processes [{}] of stopped commands, or of atomic's own tools, have not \
                         ended yet; the execution lock stays held until they do, and only \
                         SIGKILL ends this wait",
                        left.join(", ")
                    ));
                }
                next += REPORT_EVERY;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::atomic::AtomicUsize;
        use std::sync::{Arc, Mutex};

        /// Run `stop_phase` with `find` and `stop`, on a thread, giving it
        /// `within`.
        fn bounded(
            mut find: impl FnMut(Instant) -> (Vec<u32>, bool) + Send + 'static,
            mut stop: impl FnMut(&u32) + Send + 'static,
            limit: Duration,
            within: Duration,
        ) -> (HashSet<u32>, bool) {
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = sender.send(stop_phase(&mut find, &mut stop, limit));
            });
            receiver
                .recv_timeout(within)
                .unwrap_or_else(|_| panic!("the stop phase did not return in {within:?}"))
        }

        /// Processes that never stop starting others hold the stop no
        /// longer than its limit: whether each look finds another, a look
        /// itself never ends, or stopping each one is slow. Whatever a look
        /// found is kept, to be killed, even what there was no time left to
        /// stop. Ones that settle are all stopped.
        #[test]
        fn the_stop_phase_is_bounded_and_keeps_what_it_found() {
            let limit = Duration::from_millis(200);
            let within = Duration::from_secs(5);
            let mut next = 0u32;
            let (found, settled) = bounded(
                move |_| {
                    next += 1;
                    (vec![next], true)
                },
                |_| {},
                limit,
                within,
            );
            assert!(found.len() > 1);
            assert!(!settled, "it gave up looking");

            // A look that would chase new processes for good, but for its
            // deadline: what it found by then is kept.
            let mut next = 0u32;
            let (found, settled) = bounded(
                move |deadline| {
                    let mut out = Vec::new();
                    while Instant::now() < deadline {
                        next += 1;
                        out.push(next);
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    (out, false)
                },
                |_| {},
                limit,
                within,
            );
            assert!(!found.is_empty(), "the cut look's finds were kept");
            assert!(!settled);

            // Many found at once, each slow to stop: all kept, few stopped.
            let stops = Arc::new(AtomicUsize::new(0));
            let counted = stops.clone();
            let (found, settled) = bounded(
                |_| ((0..10_000).collect(), true),
                move |_| {
                    counted.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(5));
                },
                limit,
                within,
            );
            assert_eq!(found.len(), 10_000, "every one found is kept, to be killed");
            assert!(stops.load(Ordering::SeqCst) < 10_000);
            assert!(!settled);

            // A look cut short that found nothing new is no proof that
            // nothing runs: it looks again.
            let mut rounds = 0;
            let (found, settled) = bounded(
                move |_| {
                    rounds += 1;
                    match rounds {
                        1 => (vec![1], true),
                        2 => (vec![1], false),
                        _ => (vec![1, 2], true),
                    }
                },
                |_| {},
                Duration::from_secs(60),
                within,
            );
            assert!(found.contains(&2), "{found:?}");
            assert!(!settled || found.len() == 2);

            let mut rounds = 0;
            let (found, settled) = bounded(
                move |_| {
                    rounds += 1;
                    if rounds < 3 {
                        (vec![1, 2, rounds + 10], true)
                    } else {
                        (vec![1, 2], true)
                    }
                },
                |_| {},
                Duration::from_secs(60),
                within,
            );
            assert_eq!(found, HashSet::from([1, 2, 11, 12]));
            assert!(settled);
        }

        /// A look that ran out of time makes the next one twice as long, never
        /// past the cap however often; one that ended at once does not.
        #[test]
        fn the_next_look_grows_only_when_one_ran_out_and_is_capped() {
            let mut look = STOP_PHASE;
            for _ in 0..200 {
                look = next_look(look, true);
            }
            assert_eq!(look, LOOK_MAX);
            assert_eq!(
                next_look(Duration::from_secs(2), true),
                Duration::from_secs(4)
            );
            assert_eq!(next_look(STOP_PHASE, false), STOP_PHASE);
        }

        /// Looks that end incomplete at once, as when /proc cannot be listed,
        /// leave the wait waiting, the lock held, without growing the look:
        /// only a look that ran out of time makes the next one longer.
        #[test]
        fn a_wait_whose_looks_end_at_once_incomplete_waits() {
            let handle = std::thread::spawn(|| {
                let mut watched = HashSet::<u32>::new();
                wait_phase(
                    &mut watched,
                    &mut |_| (Vec::new(), Vec::new(), false),
                    &mut |_| {},
                    &mut |_| true,
                    &mut |_| {},
                    &|p| p.to_string(),
                    Instant::now(),
                );
            });
            std::thread::sleep(Duration::from_millis(1500));
            assert!(
                !handle.is_finished(),
                "the wait ended, or panicked, with no look complete"
            );
        }

        /// A look that cannot list the process directory fails at once, so
        /// the wait comes round to it every few milliseconds: that it cannot
        /// be listed is told once for the whole wait, not once a look.
        #[test]
        fn the_notice_that_proc_cannot_be_listed_is_told_once_per_wait() {
            // How many looks fail before one gets through, which is more
            // than the ten seconds after which the wait reports what is
            // left, so that a report of those would show in the notices.
            const LOOKS: u32 = 20;
            // A regular file is nothing to list: every look of it fails at
            // once, as one does where /proc is not mounted.
            let proc =
                std::env::temp_dir().join(format!("atomic-exec-no-proc-{}", std::process::id()));
            std::fs::write(&proc, "").expect("the stand-in for /proc is a file");
            let why = std::fs::read_dir(&proc).expect_err("a regular file cannot be listed");
            let notes = Arc::new(Mutex::new(Vec::new()));
            let (sender, receiver) = std::sync::mpsc::channel();
            let (listing, told) = (proc.clone(), notes.clone());
            let handle = std::thread::spawn(move || {
                let mut watched = HashSet::<Process>::new();
                let mut unreadable = HashSet::new();
                let mut listed = false;
                let mut looks = 0;
                wait_phase(
                    &mut watched,
                    &mut |deadline| {
                        looks += 1;
                        if looks > LOOKS {
                            // One look that gets through and finds nothing
                            // ends the wait, so that no thread is left.
                            let _ = sender.send(looks);
                            return (Vec::new(), Vec::new(), true);
                        }
                        let mut telling = |what: &str| {
                            told.lock().expect("the notices").push(what.to_owned());
                        };
                        let (all, complete) = found(
                            &listing,
                            &mut telling,
                            &mut unreadable,
                            &mut listed,
                            deadline,
                        );
                        (
                            all.into_iter().map(|(process, _)| process).collect(),
                            Vec::new(),
                            complete,
                        )
                    },
                    &mut |_| {},
                    &mut |_| false,
                    &mut |_| {},
                    &|process| process.pid.to_string(),
                    Instant::now(),
                );
            });
            let looks = receiver
                .recv_timeout(Duration::from_secs(60))
                .expect("the wait never ended once a look got through");
            handle.join().expect("the wait panicked");
            assert!(looks > LOOKS, "the wait looked only {looks} times");
            assert_eq!(
                *notes.lock().expect("the notices"),
                vec![format!(
                    "cannot list {}, so the processes this one started cannot be found: {why}",
                    proc.display()
                )],
                "the wait was told that {proc:?} cannot be listed once a look"
            );
            std::fs::remove_file(&proc).expect("the stand-in for /proc goes");
        }

        /// A look cut short that finds nothing running is no proof that
        /// nothing runs: the wait looks again, and kills what a complete
        /// look finds.
        #[test]
        fn the_wait_ends_only_after_a_complete_look() {
            let killed = Arc::new(Mutex::new(Vec::new()));
            let (sender, receiver) = std::sync::mpsc::channel();
            let marked = killed.clone();
            std::thread::spawn(move || {
                let mut rounds = 0;
                let mut watched = HashSet::new();
                let dead = marked.clone();
                wait_phase(
                    &mut watched,
                    &mut |_| {
                        rounds += 1;
                        if rounds < 3 {
                            (Vec::new(), Vec::new(), false)
                        } else {
                            (vec![7u32], Vec::new(), true)
                        }
                    },
                    &mut |p| marked.lock().expect("killed").push(*p),
                    &mut |p| !dead.lock().expect("killed").contains(p),
                    &mut |_| {},
                    &|p| p.to_string(),
                    Instant::now(),
                );
                let _ = sender.send(());
            });
            receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("the wait ended");
            assert_eq!(
                *killed.lock().expect("killed"),
                vec![7],
                "what the complete look found was killed"
            );
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
    /// Optional per-process CPU seconds, inherited by children on Unix.
    pub cpu_seconds: Option<u64>,
    /// Optional per-process writable data bytes, inherited by children on Unix.
    pub data_bytes: Option<u64>,
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
    let none = crate::SecretEnv::default();
    run_in_with_secret(group, program, args, cwd, env, &none, stdin, limits)
}

/// Convert a declared Unix cap before fork. `RLIM_INFINITY` would silently
/// turn a cap into no cap, and a narrowing cast could do the same on a platform
/// with a smaller `rlim_t`.
#[cfg(unix)]
fn checked_rlimit(
    program: &str,
    name: &str,
    value: Option<u64>,
    hard_extra: u64,
) -> Result<Option<(libc::rlim_t, libc::rlim_t)>, ExecError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let soft = libc::rlim_t::try_from(value).ok();
    let hard = value
        .checked_add(hard_extra)
        .and_then(|v| libc::rlim_t::try_from(v).ok());
    match (soft, hard) {
        (Some(soft), Some(hard)) if soft < libc::RLIM_INFINITY && hard < libc::RLIM_INFINITY => {
            Ok(Some((soft, hard)))
        }
        _ => Err(ExecError::Start {
            program: program.to_owned(),
            message: format!("{name} limit is too large to enforce on this machine"),
        }),
    }
}

/// A connected Unix socket holding `secret`'s entries, with its other end
/// written and closed, which a child inherits; none for an empty one. A
/// socket rather than a pipe: another process cannot reopen it through
/// `/proc/<pid>/fd` before the child has protected itself.
#[cfg(unix)]
fn secret_socket(secret: &crate::SecretEnv) -> std::io::Result<Option<std::os::fd::OwnedFd>> {
    use std::io::Write;
    if secret.is_empty() {
        return Ok(None);
    }
    let bytes = secret.encode().map_err(std::io::Error::other)?;
    // A socket's buffer holds this much without a reader; the entries are a
    // token or two, far less.
    if bytes.len() > MAX_SECRET_BYTES {
        return Err(std::io::Error::other(
            "the secret variables are too long to hand over",
        ));
    }
    // Both ends are close-on-exec; the child clears it on its own copy of
    // the end it reads, which ends once the other end is shut down.
    let (read, mut write) = std::os::unix::net::UnixStream::pair()?;
    write.write_all(&bytes)?;
    write.shutdown(std::net::Shutdown::Write)?;
    drop(write);
    Ok(Some(std::os::fd::OwnedFd::from(read)))
}

/// Whether this process can still be read through /proc by another of its
/// user: on Linux, whether it is dumpable; elsewhere, never said to be.
#[cfg(unix)]
#[allow(unsafe_code)]
fn still_dumpable() -> bool {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: PR_GET_DUMPABLE takes no pointer and touches no memory.
        unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) != 0 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Whether `mode`, as fstat gives it, is a socket's.
#[cfg(unix)]
pub(crate) fn is_socket_mode(mode: libc::mode_t) -> bool {
    mode & libc::S_IFMT == libc::S_IFSOCK
}

/// Whether `fd` is past the standard three, as a handed socket must be.
#[cfg(unix)]
pub(crate) fn above_stdio(fd: &std::os::fd::RawFd) -> bool {
    *fd > 2
}

/// The most a secret socket holds: what the sender hands at most, and what
/// the reader takes at most.
#[cfg(unix)]
const MAX_SECRET_BYTES: usize = 4096;

/// Keep the open descriptor `fd` from the programs this process starts,
/// when it is refused rather than read.
#[cfg(unix)]
#[allow(unsafe_code)]
fn keep_from_children(fd: std::os::fd::RawFd) {
    // SAFETY: fcntl with F_SETFD takes an integer and touches no memory;
    // on a descriptor that is not open it only fails.
    unsafe {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }
}

/// The [`crate::SecretEnv`] this process was handed on the socket whose
/// descriptor `fd` names, as [`run_in_with_secret`] hands it: read to its
/// end, and closed. Refused when `fd` is not a number above 2 or not a
/// socket, or the socket holds nothing, more than the sender ever hands, or
/// what is not well formed, and on Linux while this process is still
/// dumpable: it must first be hardened so that no other process of its user
/// can read what it is about to hold. A descriptor it refuses is kept from
/// the programs this process starts.
///
/// # Safety
///
/// `fd` must name a descriptor handed to this process for this alone: one
/// nothing else in this process holds, uses or can be handed while this
/// runs, since an open descriptor is taken as this process's own and closed.
#[cfg(unix)]
#[allow(unsafe_code)]
pub unsafe fn read_secret_socket(fd: &std::ffi::OsStr) -> std::io::Result<crate::SecretEnv> {
    use std::io::Read;
    use std::os::fd::FromRawFd;
    let invalid = |why: String| std::io::Error::new(std::io::ErrorKind::InvalidData, why);
    let fd: std::os::fd::RawFd = fd
        .to_str()
        .and_then(|fd| fd.parse().ok())
        .filter(above_stdio)
        .ok_or_else(|| invalid("its descriptor is not a number above 2".to_owned()))?;
    if still_dumpable() {
        keep_from_children(fd); // a process still dumpable
        return Err(invalid(
            "this process is still dumpable; it reads secret variables only once hardened"
                .to_owned(),
        ));
    }
    // Looked at before it is taken as this process's own: a descriptor that
    // is not open must not be owned, or closing it would close another's.
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat writes one stat into `stat`, and only reads `fd`.
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fstat succeeded, so it wrote the whole stat.
    let mode = unsafe { stat.assume_init() }.st_mode;
    if !is_socket_mode(mode) {
        keep_from_children(fd); // a descriptor that is not a socket
        return Err(invalid("its descriptor is not a socket".to_owned()));
    }
    // SAFETY: the descriptor is open, and the caller handed it for this
    // alone; it is read and closed here, once.
    let socket = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut held = Vec::new();
    socket
        .take(MAX_SECRET_BYTES as u64 + 1)
        .read_to_end(&mut held)?;
    if held.len() > MAX_SECRET_BYTES {
        return Err(invalid(format!(
            "its socket holds more than the {MAX_SECRET_BYTES} bytes ever handed"
        )));
    }
    // The control plane never hands an empty socket: one is no secret.
    if held.is_empty() {
        return Err(invalid("its socket holds nothing".to_owned()));
    }
    crate::SecretEnv::decode(&held).map_err(invalid)
}

/// The [`crate::SecretEnv`] this process was handed, if its environment
/// names a socket in [`crate::SECRET_ENV_FD`] (see [`read_secret_socket`]);
/// none if it names none. The variable is gone from its environment after.
/// Only `atomic runner` calls it, before it starts any thread, and
/// the descriptor the variable names is the one the control plane handed
/// it: the contract [`read_secret_socket`] asks of its caller holds there,
/// and nowhere this could be called from outside the crate.
#[cfg(unix)]
#[allow(unsafe_code)]
pub(crate) fn take_inherited_secret_env() -> std::io::Result<crate::SecretEnv> {
    let Some(fd) = std::env::var_os(crate::SECRET_ENV_FD) else {
        return Ok(crate::SecretEnv::default());
    };
    // SAFETY: called before this process starts any thread, so nothing
    // reads the environment concurrently; the descriptor the variable names
    // was handed to this process for this alone.
    unsafe {
        std::env::remove_var(crate::SECRET_ENV_FD);
        read_secret_socket(&fd)
    }
}

/// [`run_in`], with `secret` on a socket the child inherits, whose
/// descriptor it finds in [`crate::SECRET_ENV_FD`]; the variables are never
/// in its environment.
#[allow(clippy::too_many_arguments)]
pub fn run_in_with_secret(
    group: Group,
    program: &Path,
    args: &[String],
    cwd: Option<&Path>,
    env: &[(String, String)],
    secret: &crate::SecretEnv,
    stdin: &[u8],
    limits: Limits,
) -> Result<Completed, ExecError> {
    let name = program.display().to_string();
    #[cfg(unix)]
    let cpu = checked_rlimit(&name, "CPU", limits.cpu_seconds, 1)?;
    #[cfg(unix)]
    let data = checked_rlimit(&name, "writable data", limits.data_bytes, 0)?;
    // macOS refuses an `RLIMIT_DATA` below the size of the whole address map,
    // which starts far larger than any limit a command is given, so there
    // the parent holds the limit while it waits (see [`memory`]).
    #[cfg(target_os = "macos")]
    let data: Option<(libc::rlim_t, libc::rlim_t)> = data.and(None);
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
    let secret_end = {
        use std::os::fd::AsRawFd;
        let socket = secret_socket(secret).map_err(|e| ExecError::Start {
            program: name.clone(),
            message: format!("hand it its secret variables: {e}"),
        })?;
        if let Some(socket) = &socket {
            let fd = socket.as_raw_fd();
            command.env(crate::SECRET_ENV_FD, fd.to_string());
            // SAFETY: between fork and exec the closure makes one
            // async-signal-safe call, fcntl, and allocates nothing.
            #[allow(unsafe_code)]
            unsafe {
                std::os::unix::process::CommandExt::pre_exec(&mut command, move || {
                    // The child keeps its end across exec; this process
                    // closes its own once the child has started.
                    if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        socket
    };
    #[cfg(not(unix))]
    if !secret.is_empty() {
        return Err(ExecError::Start {
            program: name,
            message: "secret variables are handed on a socket only on Unix".into(),
        });
    }
    #[cfg(not(unix))]
    if limits.cpu_seconds.is_some() || limits.data_bytes.is_some() {
        return Err(ExecError::Start {
            program: name,
            message: "CPU and writable-data limits require Unix".into(),
        });
    }
    #[cfg(unix)]
    {
        let parent = libc::pid_t::try_from(std::process::id()).unwrap_or(0);
        let own = group != Group::Inherit;
        // SAFETY: between fork and exec the closure makes only
        // async-signal-safe calls (setsid, signal, prctl, getppid, _exit)
        // and allocates nothing.
        #[allow(unsafe_code)]
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut command, move || {
                // A session of its own, which is also a process group of its
                // own (its id is the child's pid, which `killpg` stops): with
                // no controlling terminal, the command cannot open atomic's
                // `/dev/tty` and type into a gate's prompt.
                if own && libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
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
                if let Some((soft, hard)) = cpu {
                    let limit = libc::rlimit {
                        rlim_cur: soft,
                        rlim_max: hard,
                    };
                    if libc::setrlimit(libc::RLIMIT_CPU, &limit) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                if let Some((soft, hard)) = data {
                    let limit = libc::rlimit {
                        rlim_cur: soft,
                        rlim_max: hard,
                    };
                    if libc::setrlimit(libc::RLIMIT_DATA, &limit) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
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
    // The child has its own copy of the secret socket.
    #[cfg(unix)]
    drop(secret_end);
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
    // Why the parent stopped it, where it holds the memory limit itself.
    #[cfg(target_os = "macos")]
    let mut breach: Option<String> = None;
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
            None => {
                #[cfg(target_os = "macos")]
                if breach.is_none()
                    && let Some(limit) = limits.data_bytes
                    && let Some(why) = memory::over(limit, child.id(), group)
                {
                    // Everything it started; the next look collects it.
                    if let Some(group) = group {
                        kill_group(group);
                    }
                    let _ = child.kill();
                    breach = Some(why);
                    continue;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };
    // Its output ends when every process holding it has let go. One that
    // left its group, or shares a runner's, may hold it for good: what came
    // within the grace after the exit is the output.
    let deadline = Instant::now() + OUTPUT_GRACE;
    let stdout = stdout.until(deadline).map_err(io)?;
    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut stderr = stderr.until(deadline).map_err(io)?;
    // Killed as the kernel would have refused it: how is said on stderr.
    #[cfg(target_os = "macos")]
    if let Some(why) = breach {
        stderr.extend_from_slice(format!("\n{why}\n").as_bytes());
    }
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
