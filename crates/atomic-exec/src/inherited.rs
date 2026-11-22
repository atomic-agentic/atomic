// SPDX-License-Identifier: Apache-2.0

//! Descriptors a process was started with, kept from the programs it starts.
//!
//! Whatever started atomic may have left descriptors open past the standard
//! three: a CI runner's pipes, a shell's or an editor's files and sockets.
//! A descriptor without close-on-exec passes to every program a process
//! starts, through every sandbox that does not close it, so a command could
//! read or write what atomic's parent holds. [`keep_from_children`] marks
//! every one close-on-exec, but those handed on purpose, as atomic's
//! entry point and its sandbox helper do first thing: the process keeps
//! using what it holds, and no program it starts after inherits any of it.
//! A descriptor this process opens later without close-on-exec is not
//! covered; Rust's standard library and tokio open every one with it.

use std::os::fd::RawFd;

/// Mark every descriptor this process holds above the standard three
/// close-on-exec, but those in `keep`, whose flags are left as they are.
/// With `close_range` where the kernel has it (Linux 5.11 and later), and
/// otherwise one descriptor at a time, as this process's descriptor
/// directory lists them. An error means some may still pass to a program
/// this process starts: the caller refuses to start any.
pub fn keep_from_children(keep: &[RawFd]) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    if close_range_except(keep).is_ok() {
        return Ok(());
    }
    keep_listed_from_children(std::path::Path::new(DESCRIPTORS), keep)
}

/// Where this process's open descriptors are listed.
#[cfg(target_os = "linux")]
const DESCRIPTORS: &str = "/proc/self/fd";
#[cfg(not(target_os = "linux"))]
const DESCRIPTORS: &str = "/dev/fd";

/// The descriptor of the socket this process was handed its secret
/// variables on, as [`crate::SECRET_ENV_FD`] names it, if it names one
/// above the standard three: the one descriptor atomic hands its sandbox
/// helper and its runner on purpose.
pub fn handed_secret_fd() -> Option<RawFd> {
    std::env::var(crate::SECRET_ENV_FD)
        .ok()?
        .parse::<RawFd>()
        .ok()
        .filter(crate::process::above_stdio)
}

/// [`keep_from_children`] with `close_range`, around each descriptor in
/// `keep`. A descriptor opened for the purpose without close-on-exec must
/// come out marked: a `close_range` that reports success and marks nothing,
/// as a seccomp filter answering it with no error would, is an error.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn close_range_except(keep: &[RawFd]) -> std::io::Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    // SAFETY: open reads one NUL-terminated path; the descriptor it returns
    // is owned here alone.
    let probe = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) };
    if probe < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: as above.
    let probe = unsafe { OwnedFd::from_raw_fd(probe) };
    let mut kept: Vec<u32> = keep
        .iter()
        .filter_map(|fd| u32::try_from(*fd).ok())
        .collect();
    kept.sort_unstable();
    let mut first = 3u32;
    for fd in kept.into_iter().filter(|fd| *fd >= 3).chain([u32::MAX]) {
        if fd > first {
            let last = if fd == u32::MAX { u32::MAX } else { fd - 1 };
            // SAFETY: close_range takes integers, touches no memory, and with
            // CLOSE_RANGE_CLOEXEC changes only the descriptors' flags.
            let marked = unsafe {
                libc::syscall(
                    libc::SYS_close_range,
                    first,
                    last,
                    libc::CLOSE_RANGE_CLOEXEC,
                )
            };
            if marked != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        first = fd.saturating_add(1);
    }
    // SAFETY: F_GETFD only reads the descriptor's flags.
    let flags = unsafe { libc::fcntl(probe.as_raw_fd(), libc::F_GETFD) };
    // Flags that cannot be read are no proof of a mark.
    let unread = flags == -1;
    let inheritable = flags & libc::FD_CLOEXEC == 0;
    if unread || inheritable {
        return Err(std::io::Error::other(
            "close_range reported success but left a descriptor inheritable",
        ));
    }
    Ok(())
}

/// [`keep_from_children`] one descriptor at a time, for each `dir` lists:
/// this process's descriptor directory. A name that is not a descriptor, or
/// any error listing it or marking one, is an error; only a descriptor gone
/// by the time it is marked, as the listing's own is, is passed over.
#[allow(unsafe_code)]
pub(crate) fn keep_listed_from_children(
    dir: &std::path::Path,
    keep: &[RawFd],
) -> std::io::Result<()> {
    let mut listed = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let name = entry?.file_name();
        let fd = name
            .to_str()
            .and_then(|n| n.parse::<RawFd>().ok())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{} lists {name:?}, not a descriptor", dir.display()),
                )
            })?;
        listed.push(fd);
    }
    for fd in listed {
        if fd <= 2 || keep.contains(&fd) {
            continue;
        }
        // SAFETY: fcntl with F_GETFD and F_SETFD takes integers and only
        // reads or sets the descriptor's flags.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags == -1 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EBADF) {
                continue;
            }
            return Err(e);
        }
        let cloexec = flags | libc::FD_CLOEXEC;
        // SAFETY: as above.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, cloexec) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    /// A descriptor this process holds that passes to the programs it
    /// starts: a copy of `fd` without close-on-exec.
    #[allow(unsafe_code)]
    fn inheritable(fd: &impl AsRawFd) -> OwnedFd {
        // SAFETY: dup takes an integer; the copy it returns is this
        // test's own.
        let copy = unsafe { libc::dup(fd.as_raw_fd()) };
        assert!(copy > 2, "dup: {}", std::io::Error::last_os_error());
        // SAFETY: as above; the descriptor is open and owned here alone.
        unsafe { OwnedFd::from_raw_fd(copy) }
    }

    #[allow(unsafe_code)]
    fn close_on_exec(fd: &impl AsRawFd) -> bool {
        // SAFETY: F_GETFD only reads the flags.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags, -1);
        flags & libc::FD_CLOEXEC != 0
    }

    /// Held by the tests that mark this process's descriptors, or that
    /// look at a descriptor no call may mark meanwhile: a test's thread
    /// shares the descriptors of every other.
    static DESCRIPTORS_HELD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Every descriptor past the standard three is kept from children, but
    /// the ones kept on purpose, both by `close_range` and by the listing.
    #[test]
    fn descriptors_are_kept_from_children_but_those_kept_on_purpose() {
        let _held = DESCRIPTORS_HELD.lock().unwrap_or_else(|e| e.into_inner());
        let file = std::fs::File::open("/dev/null").expect("file");
        for how in ["close_range", "either", "listing"] {
            let left = inheritable(&file);
            let handed = inheritable(&file);
            assert!(!close_on_exec(&left) && !close_on_exec(&handed));
            let kept = [handed.as_raw_fd()];
            match how {
                #[cfg(target_os = "linux")]
                "close_range" => close_range_except(&kept).expect("close_range"),
                #[cfg(not(target_os = "linux"))]
                "close_range" => continue,
                "either" => keep_from_children(&kept).expect("kept"),
                _ => keep_listed_from_children(std::path::Path::new(DESCRIPTORS), &kept)
                    .expect("listed"),
            }
            assert!(close_on_exec(&left), "{how}: left open is kept");
            assert!(!close_on_exec(&handed), "{how}: handed passes");
        }
    }

    /// Run `call` on a thread of its own whose seccomp filter answers
    /// `close_range` with success and does nothing, as a container's
    /// profile can, and, when `getfd_fails`, `fcntl(F_GETFD)` with `EPERM`
    /// (not `EBADF`, which a debug build's check that a descriptor it drops
    /// is open would take for a closed one). The filter is the thread's
    /// alone, so no other test sees it.
    #[cfg(target_os = "linux")]
    #[allow(unsafe_code)]
    fn where_close_range_does_nothing<T: Send + 'static>(
        getfd_fails: bool,
        call: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        std::thread::spawn(move || {
            let op = |code: u32, jt: u8, jf: u8, k: u32| libc::sock_filter {
                code: code as u16,
                jt,
                jf,
                k,
            };
            let load = libc::BPF_LD | libc::BPF_W | libc::BPF_ABS;
            let equal = libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K;
            let answer = libc::BPF_RET | libc::BPF_K;
            // The call's number, then the low word of its second argument
            // (on a little-endian machine, as atomic's Linux targets are).
            let command = std::mem::offset_of!(libc::seccomp_data, args) as u32
                + std::mem::size_of::<u64>() as u32;
            let getfd = if getfd_fails {
                libc::SYS_fcntl as u32
            } else {
                u32::MAX
            };
            let filter = [
                op(load, 0, 0, 0),
                op(equal, 0, 1, libc::SYS_close_range as u32),
                op(answer, 0, 0, libc::SECCOMP_RET_ERRNO),
                op(equal, 0, 3, getfd),
                op(load, 0, 0, command),
                op(equal, 0, 1, libc::F_GETFD as u32),
                op(answer, 0, 0, libc::SECCOMP_RET_ERRNO | libc::EPERM as u32),
                op(answer, 0, 0, libc::SECCOMP_RET_ALLOW),
            ];
            let program = libc::sock_fprog {
                len: filter.len() as u16,
                filter: filter.as_ptr().cast_mut(),
            };
            // SAFETY: prctl takes integers and, for the filter, a pointer to
            // a program on this stack that the kernel copies.
            unsafe {
                assert_eq!(
                    libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0),
                    0,
                    "no_new_privs: {}",
                    std::io::Error::last_os_error()
                );
                assert_eq!(
                    libc::prctl(
                        libc::PR_SET_SECCOMP,
                        libc::SECCOMP_MODE_FILTER,
                        &program as *const libc::sock_fprog,
                    ),
                    0,
                    "seccomp: {}",
                    std::io::Error::last_os_error()
                );
            }
            call()
        })
        .join()
        .expect("the filtered thread")
    }

    /// A `close_range` that reports success and marks nothing, as a seccomp
    /// filter answering it with no error makes it, is caught by the probe
    /// descriptor that comes out inheritable: an error, so that
    /// [`keep_from_children`] reads the listing instead, and not a sweep
    /// that left every descriptor to the programs this process starts.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_close_range_that_marks_nothing_is_an_error() {
        let _held = DESCRIPTORS_HELD.lock().unwrap_or_else(|e| e.into_inner());
        let file = std::fs::File::open("/dev/null").expect("file");
        let left = inheritable(&file);
        let refused = where_close_range_does_nothing(false, || close_range_except(&[]))
            .expect_err("a close_range that marks nothing");
        assert_eq!(
            refused.to_string(),
            "close_range reported success but left a descriptor inheritable"
        );
        // The filter took effect: close_range marked nothing.
        assert!(!close_on_exec(&left), "close_range marked a descriptor");
    }

    /// A probe whose flags cannot be read after such a `close_range` is
    /// an error too, not taken for one that came out marked.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_probe_whose_flags_cannot_be_read_is_an_error() {
        let _held = DESCRIPTORS_HELD.lock().unwrap_or_else(|e| e.into_inner());
        let refused = where_close_range_does_nothing(true, || close_range_except(&[]))
            .expect_err("a probe whose flags cannot be read");
        assert_eq!(
            refused.to_string(),
            "close_range reported success but left a descriptor inheritable"
        );
    }

    /// A listing that cannot be read, for any reason, is an error, and so is
    /// one that lists something that is not a descriptor: neither passes
    /// over the descriptors it could not see.
    #[test]
    fn a_listing_that_cannot_be_read_is_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("atomic-inherited-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let closed = dir.join("closed");
        std::fs::create_dir_all(&closed).expect("dir");
        std::fs::write(closed.join("5"), "").expect("entry");
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000)).expect("mode");
        let refused = keep_listed_from_children(&closed, &[]).expect_err("unreadable");
        assert_eq!(
            refused.kind(),
            std::io::ErrorKind::PermissionDenied,
            "{refused}"
        );
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o700)).expect("mode");
        let odd = dir.join("odd");
        std::fs::create_dir_all(&odd).expect("dir");
        std::fs::write(odd.join("not-a-descriptor"), "").expect("entry");
        let refused = keep_listed_from_children(&odd, &[]).expect_err("odd");
        assert_eq!(refused.kind(), std::io::ErrorKind::InvalidData, "{refused}");
        let looped = dir.join("looped");
        std::os::unix::fs::symlink(&looped, &looped).expect("link");
        let refused = keep_listed_from_children(&looped, &[]).expect_err("a loop");
        assert_eq!(refused.raw_os_error(), Some(libc::ELOOP), "{refused}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
