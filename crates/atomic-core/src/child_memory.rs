// SPDX-License-Identifier: Apache-2.0

//! The memory limit on a compiler or helper child, where its parent has to
//! enforce it.
//!
//! On Linux a child limits its own writable memory with `RLIMIT_DATA`, and
//! the kernel refuses what goes past it. macOS has no limit a process can
//! set on itself that does the same. There `RLIMIT_DATA` and `RLIMIT_AS`
//! both bound the size of the whole address map, and the kernel refuses a
//! limit below the map's present size; an arm64 process starts with
//! hundreds of gigabytes mapped that nothing can use, so no limit near the
//! memory a child may hold can be set at all. The limits macOS keeps on
//! memory in use take a privilege atomic does not have.
//!
//! So on macOS the parent measures. Each time it wakes while it waits on a
//! child it reads the child's physical footprint, the count of memory in use
//! that macOS itself limits processes by, and it kills a child found past
//! its limit. A footprint counts what a process has written to, compressed
//! or swapped out or not, and nothing it has only reserved.
//!
//! The kernel does not hold this limit, so it is not exact: between two
//! readings a child can go past it by what it can write in that time, and is
//! killed if a later reading still finds it over budget. A failed reading
//! stops the child, with a short grace period when the OS reports the child
//! absent so the parent can collect its exit status.

#![allow(unsafe_code)]

use std::time::{Duration, Instant};

/// How long a parent sleeps between two readings of a watched child.
pub(crate) const INTERVAL: Duration = Duration::from_millis(2);

/// How long a child may be gone to a reading and still not be collected by
/// its parent: the moment a process takes to end. One gone for longer has
/// not ended, and is stopped as one that cannot be read.
const ENDING: Duration = Duration::from_secs(1);

/// Why a watched child is stopped.
#[derive(Debug)]
pub(crate) enum Breach {
    /// It held more than its limit.
    Exceeded,
    /// It is running and its memory cannot be read.
    Unread(std::io::Error),
}

/// The limit on one child's memory, held by the parent that waits on it.
pub(crate) struct Watch {
    limit: u64,
    /// The memory a process has in use, or `None` for one that is ending.
    read: fn(u32) -> std::io::Result<Option<u64>>,
    /// When the child was first gone to a reading, while it stays so.
    gone: Option<Instant>,
}

#[cfg(test)]
thread_local! {
    /// The limit a test has put in place of each child's own (see
    /// [`with_limit`]).
    static FORCED: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Runs `body` with every child this thread starts watched at `limit` bytes
/// in place of its own limit, on any platform: how a test reaches the watch
/// where the platform needs none, at a limit a test machine can spare.
#[cfg(test)]
pub(crate) fn with_limit<T>(limit: u64, body: impl FnOnce() -> T) -> T {
    let before = FORCED.replace(Some(limit));
    let result = body();
    FORCED.set(before);
    result
}

impl Watch {
    /// The watch a child limited to `limit` bytes needs on this platform:
    /// one on macOS, and none where the child's own `RLIMIT_DATA` holds.
    pub(crate) fn for_child(limit: u64) -> Option<Self> {
        #[cfg(test)]
        if let Some(limit) = FORCED.get() {
            return Some(Self::new(limit));
        }
        cfg!(target_os = "macos").then(|| Self::new(limit))
    }

    fn new(limit: u64) -> Self {
        Self {
            limit,
            read: footprint,
            gone: None,
        }
    }

    /// Reads `child`'s memory. An error says why the caller must stop it.
    pub(crate) fn check(&mut self, child: &mut std::process::Child) -> Result<(), Breach> {
        // A child that has ended holds nothing, and its process id may be
        // another process's by now. Its status is kept for the caller.
        if matches!(child.try_wait(), Ok(Some(_))) {
            return Ok(());
        }
        match (self.read)(child.id()) {
            Ok(Some(held)) if held > self.limit => Err(Breach::Exceeded),
            Ok(Some(_)) => {
                self.gone = None;
                Ok(())
            }
            Ok(None) => {
                let since = *self.gone.get_or_insert_with(Instant::now);
                if since.elapsed() < ENDING {
                    Ok(())
                } else {
                    Err(Breach::Unread(std::io::Error::other(
                        "the process is running and reports no memory",
                    )))
                }
            }
            Err(error) => Err(Breach::Unread(error)),
        }
    }
}

/// The physical footprint of process `pid`, in bytes.
#[cfg(target_os = "macos")]
fn footprint(pid: u32) -> std::io::Result<Option<u64>> {
    let pid = libc::pid_t::try_from(pid).map_err(std::io::Error::other)?;
    let mut usage = std::mem::MaybeUninit::<libc::rusage_info_v0>::zeroed();
    // SAFETY: `usage` is writable storage with the size and alignment of the
    // structure the V0 flavor fills, and it lives until the call returns.
    let result =
        unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V0, usage.as_mut_ptr().cast()) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        // A process that is ending, or has ended and is not collected yet,
        // is not found.
        return if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(None)
        } else {
            Err(error)
        };
    }
    // SAFETY: the call filled the structure, whose fields are plain numbers
    // that zeroed storage is valid for in any case.
    Ok(Some(unsafe { usage.assume_init() }.ri_phys_footprint))
}

/// The anonymous memory process `pid` has in use, resident or swapped out,
/// in bytes: the nearest Linux has to a footprint. No Linux child is
/// limited by it (see [`Watch::for_child`]); the tests of the watch are.
#[cfg(target_os = "linux")]
fn footprint(pid: u32) -> std::io::Result<Option<u64>> {
    let status = match std::fs::read_to_string(format!("/proc/{pid}/status")) {
        Ok(status) => status,
        // A process that is ending is not found.
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ESRCH) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let kilobytes = |name: &str| {
        status.lines().find_map(|line| {
            line.strip_prefix(name)?
                .trim()
                .strip_suffix("kB")?
                .trim()
                .parse::<u64>()
                .ok()
        })
    };
    // A process that has ended and is not collected yet keeps its status
    // file and loses these lines.
    Ok(kilobytes("RssAnon:")
        .zip(kilobytes("VmSwap:"))
        .map(|(resident, swapped)| (resident + swapped) * 1024))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn footprint(_: u32) -> std::io::Result<Option<u64>> {
    Err(std::io::ErrorKind::Unsupported.into())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};

    fn sleeper() -> Child {
        Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .spawn()
            .expect("sleep")
    }

    fn stop(mut child: Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    fn watch(limit: u64, read: fn(u32) -> std::io::Result<Option<u64>>) -> Watch {
        Watch {
            limit,
            read,
            gone: None,
        }
    }

    // The parent holds the limit exactly where the child cannot: at the
    // child's own limit on macOS, and not at all where the kernel holds it.
    #[test]
    fn a_child_is_watched_at_its_limit_only_on_macos() {
        let limit = Watch::for_child(4096).map(|watch| watch.limit);
        assert_eq!(limit, cfg!(target_os = "macos").then_some(4096));
        // A test's limit takes the place of the child's on its own thread.
        let forced = with_limit(1, || Watch::for_child(4096).map(|watch| watch.limit));
        assert_eq!(forced, Some(1));
        let other = with_limit(1, || {
            std::thread::spawn(|| Watch::for_child(4096).map(|watch| watch.limit))
                .join()
                .expect("thread")
        });
        assert_eq!(other, limit);
    }

    #[test]
    fn a_child_at_its_limit_stays_and_one_past_it_is_a_breach() {
        let mut child = sleeper();
        assert!(watch(4096, |_| Ok(Some(4096))).check(&mut child).is_ok());
        assert!(matches!(
            watch(4096, |_| Ok(Some(4097))).check(&mut child),
            Err(Breach::Exceeded)
        ));
        stop(child);
    }

    // The real reading, of a real child: a sleeping process holds some
    // memory, and far less than a gigabyte.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_running_childs_memory_is_read() {
        let mut child = sleeper();
        // A child just started may not have a reading yet.
        let started = Instant::now();
        let held = loop {
            match footprint(child.id()) {
                Ok(Some(held)) if held > 0 => break held,
                _ if started.elapsed() > Duration::from_secs(5) => panic!("no reading"),
                _ => std::thread::sleep(INTERVAL),
            }
        };
        assert!(held < 1 << 30, "{held}");
        assert!(Watch::new(1 << 30).check(&mut child).is_ok());
        assert!(matches!(
            Watch::new(0).check(&mut child),
            Err(Breach::Exceeded)
        ));
        stop(child);
    }

    // No reading, no limit: a running child that cannot be read is stopped
    // at the first reading that fails.
    #[test]
    fn a_running_child_that_cannot_be_read_is_a_breach() {
        let mut child = sleeper();
        let mut denied = watch(u64::MAX, |_| {
            Err(std::io::ErrorKind::PermissionDenied.into())
        });
        assert!(matches!(
            denied.check(&mut child),
            Err(Breach::Unread(error)) if error.kind() == std::io::ErrorKind::PermissionDenied
        ));
        stop(child);
    }

    // A child gone to a reading is ending, and its parent collects it. One
    // that stays gone and uncollected has not ended.
    #[test]
    fn a_child_gone_to_a_reading_has_a_moment_to_end() {
        let mut child = sleeper();
        let mut gone = watch(u64::MAX, |_| Ok(None));
        assert!(gone.check(&mut child).is_ok());
        gone.gone = Some(Instant::now() - ENDING);
        assert!(matches!(gone.check(&mut child), Err(Breach::Unread(_))));
        // A reading in between starts the moment again.
        let mut back = watch(u64::MAX, |_| Ok(Some(0)));
        back.gone = Some(Instant::now() - ENDING);
        assert!(back.check(&mut child).is_ok());
        assert!(back.gone.is_none());
        stop(child);
    }

    #[test]
    fn a_child_that_has_ended_is_not_read() {
        let mut child = Command::new("true").spawn().expect("true");
        let started = Instant::now();
        while child.try_wait().expect("wait").is_none() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "true never ended"
            );
            std::thread::sleep(INTERVAL);
        }
        // Its process id may belong to another process: nothing is read,
        // whatever the reading would say.
        let mut over = watch(0, |_| panic!("read after the child ended"));
        assert!(over.check(&mut child).is_ok());
        // The caller still gets its status.
        assert!(child.wait().expect("status").success());
    }
}
