// SPDX-License-Identifier: Apache-2.0

//! Stop a watched worker when the process holding its memory budget exits.

#![allow(unsafe_code)]

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Command;

const WORKER_SUPERVISOR_FD: &str = "ATOMIC_WORKER_SUPERVISOR_FD";
const WORKER_SUPERVISOR_PID: &str = "ATOMIC_WORKER_SUPERVISOR_PID";
const PARENT_CHECK_MS: libc::c_int = 100;

/// Holds the supervisor's end of the worker's private lifetime socket.
pub(crate) struct Supervisor {
    _socket: UnixStream,
    worker: Option<UnixStream>,
}

impl Supervisor {
    /// Arranges for the worker to inherit just its end of the socket.
    pub(crate) fn prepare(command: &mut Command) -> io::Result<Self> {
        let (parent, worker) = UnixStream::pair()?;
        let parent = above_stdio(parent)?;
        let worker = above_stdio(worker)?;
        let fd = worker.as_raw_fd();
        command.env(WORKER_SUPERVISOR_FD, fd.to_string());
        command.env(WORKER_SUPERVISOR_PID, std::process::id().to_string());
        // SAFETY: fcntl is async-signal-safe and the closure allocates
        // nothing. The caller holds both ends through spawn.
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(Self {
            _socket: parent,
            worker: Some(worker),
        })
    }

    /// The worker has its own copy now. Close the supervisor's extra copy.
    pub(crate) fn child_spawned(&mut self) {
        self.worker = None;
    }
}

fn above_stdio(socket: UnixStream) -> io::Result<UnixStream> {
    if socket.as_raw_fd() >= 3 {
        return Ok(socket);
    }
    // SAFETY: the socket is open. A successful duplicate is owned here.
    let fd = unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the new descriptor is an owned duplicate of the socket.
    Ok(unsafe { UnixStream::from_raw_fd(fd) })
}

/// Starts the lifetime check before the worker reads or executes its request.
pub(crate) fn watch_supervisor() -> io::Result<()> {
    let fd = std::env::var(WORKER_SUPERVISOR_FD)
        .ok()
        .and_then(|value| value.parse::<RawFd>().ok())
        .filter(|fd| *fd >= 3)
        .ok_or_else(|| io::Error::other("missing worker supervisor socket"))?;
    let parent = std::env::var(WORKER_SUPERVISOR_PID)
        .ok()
        .and_then(|value| value.parse::<libc::pid_t>().ok())
        .filter(|pid| *pid > 0)
        .ok_or_else(|| io::Error::other("missing worker supervisor PID"))?;
    // SAFETY: this only changes the descriptor's close-on-exec flag.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // A peer that has already closed must stop startup synchronously.
    match receive(fd, libc::MSG_DONTWAIT | libc::MSG_PEEK) {
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
        Err(error) => return Err(error),
        Ok(()) => {
            return Err(io::Error::other(
                "worker supervisor socket closed or invalid",
            ));
        }
    }
    if !has_parent(parent) {
        return Err(io::Error::other(
            "worker supervisor is no longer its parent",
        ));
    }
    std::thread::Builder::new()
        .name("atomic-worker-supervisor".into())
        .spawn(move || {
            // The supervisor never writes. EOF, a byte, or a failed read
            // all mean this worker no longer has a usable supervisor.
            let mut socket = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            while has_parent(parent) {
                // A concurrent fork could briefly inherit an endpoint
                // while Darwin is setting its close-on-exec flag. Check
                // the direct parent even if another copy keeps EOF away.
                // SAFETY: poll writes to one live pollfd on this stack.
                let ready = unsafe { libc::poll(&mut socket, 1, PARENT_CHECK_MS) };
                if ready == 0
                    || (ready < 0
                        && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted)
                {
                    continue;
                }
                break;
            }
            // SAFETY: _exit ends this process from any thread without
            // waiting for the compiler or helper to cooperate.
            unsafe { libc::_exit(1) };
        })?;
    Ok(())
}

fn has_parent(expected: libc::pid_t) -> bool {
    // SAFETY: getppid takes no arguments and only reads the process identity.
    (unsafe { libc::getppid() }) == expected
}

fn receive(fd: RawFd, flags: libc::c_int) -> io::Result<()> {
    let mut byte = 0u8;
    loop {
        // SAFETY: recv takes a writable byte, and never reads more than
        // one byte into it. EBADF and ENOTSOCK are handled as errors.
        if unsafe { libc::recv(fd, (&mut byte as *mut u8).cast(), 1, flags) } >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::{Child, ExitStatus, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    #[ignore = "worker subprocess for the lifetime test"]
    fn worker_entry() {
        if let Err(error) = watch_supervisor() {
            eprintln!("{error}");
            std::process::exit(2);
        }
        let mut stdout = io::stdout().lock();
        stdout.write_all(b"worker watching\n").expect("announce");
        stdout.flush().expect("flush");
        loop {
            std::thread::park();
        }
    }

    fn worker_command() -> Command {
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args([
                "--exact",
                "worker_lifetime::tests::worker_entry",
                "--ignored",
                "--nocapture",
            ])
            .stdout(Stdio::piped());
        command
    }

    #[test]
    fn closing_the_supervisor_stops_an_active_worker() {
        let mut command = worker_command();
        let mut supervisor = Supervisor::prepare(&mut command).expect("socket");
        let mut child = command.spawn().expect("worker");
        supervisor.child_spawned();

        let stdout = child.stdout.take().expect("worker stdout");
        let (announce, ready) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if line
                    .as_deref()
                    .is_ok_and(|line| line.ends_with("worker watching"))
                {
                    let _ = announce.send(());
                }
            }
        });
        let announced = ready.recv_timeout(Duration::from_secs(10)).is_ok();
        let was_running = matches!(child.try_wait(), Ok(None));
        drop(supervisor);
        let status = wait_for_exit(&mut child);
        reader.join().expect("output reader");
        assert!(announced && was_running, "worker did not start watching");
        assert_eq!(status.and_then(|status| status.code()), Some(1));
    }

    #[test]
    fn a_live_socket_from_the_wrong_parent_does_not_start_a_worker() {
        let mut command = worker_command();
        let mut supervisor = Supervisor::prepare(&mut command).expect("socket");
        command
            .env(WORKER_SUPERVISOR_PID, (std::process::id() + 1).to_string())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("worker");
        supervisor.child_spawned();
        // Keep the peer open while waiting. The startup check must refuse
        // the wrong parent even when the channel has no EOF.
        assert_eq!(
            wait_for_exit(&mut child).and_then(|status| status.code()),
            Some(2)
        );
        let mut error = String::new();
        child
            .stderr
            .take()
            .expect("worker stderr")
            .read_to_string(&mut error)
            .expect("read rejection");
        assert!(
            error.contains("worker supervisor is no longer its parent"),
            "{error}"
        );
    }

    #[test]
    #[ignore = "intermediate supervisor for the reparenting test"]
    fn supervisor_entry() {
        // Pass the open worker endpoint through. Only our own parent
        // holds the other end, so our exit will not cause socket EOF.
        let _worker = worker_command()
            .env(WORKER_SUPERVISOR_PID, std::process::id().to_string())
            .stdout(Stdio::inherit())
            .spawn()
            .expect("worker");
        loop {
            std::thread::park();
        }
    }

    #[test]
    fn losing_the_direct_parent_stops_a_worker_with_a_live_socket() {
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args([
                "--exact",
                "worker_lifetime::tests::supervisor_entry",
                "--ignored",
                "--nocapture",
            ])
            .process_group(0)
            .stdout(Stdio::piped());
        let mut socket = Supervisor::prepare(&mut command).expect("socket");
        let mut supervisor = command.spawn().expect("supervisor");
        socket.child_spawned();
        let group = libc::pid_t::try_from(supervisor.id()).expect("process group ID");
        let stdout = supervisor.stdout.take().expect("worker stdout");
        let (event, events) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line.ends_with("worker watching") {
                    let _ = event.send(());
                }
            }
            let _ = event.send(());
        });
        let timeout = Duration::from_secs(5);
        let ready = events.recv_timeout(timeout).is_ok();
        let active = matches!(supervisor.try_wait(), Ok(None));
        let stopped = supervisor.kill().and_then(|()| supervisor.wait()).is_ok();
        // The outer test deliberately keeps the peer open until the
        // orphan has either closed its stdout or reached the deadline.
        let exited = events.recv_timeout(timeout).is_ok();
        drop(socket);
        if !exited && events.recv_timeout(timeout).is_err() {
            // SAFETY: this dedicated process group was created for the
            // fixture above and its descendant still holds stdout.
            let _ = unsafe { libc::kill(-group, libc::SIGKILL) };
        }
        reader.join().expect("output reader");
        assert!(
            ready && active && stopped,
            "supervisor did not start and stop"
        );
        assert!(
            exited,
            "worker survived loss of its parent with the socket open"
        );
    }

    fn wait_for_exit(child: &mut Child) -> Option<ExitStatus> {
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) if started.elapsed() < Duration::from_secs(5) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
    }
}
