// SPDX-License-Identifier: Apache-2.0

//! A process reads secret variables handed to it on a socket only once it
//! is hardened, and only from a socket past the standard three. It changes
//! this test process's dumpable state, so it has a binary of its own.

#![cfg(target_os = "linux")]

use atomic_exec::SecretEnv;
use atomic_exec::process::read_secret_socket;
use std::io::Write;
use std::os::fd::IntoRawFd;

/// A socket holding `bytes`, its other end shut, as a descriptor number.
fn socket_holding(bytes: &[u8]) -> String {
    let (socket, mut write) = std::os::unix::net::UnixStream::pair().expect("a socket");
    write.write_all(bytes).expect("written");
    drop(write);
    socket.into_raw_fd().to_string()
}

#[test]
#[allow(unsafe_code)]
fn a_secret_socket_is_read_only_once_hardened_and_only_from_a_socket() {
    // SAFETY: each descriptor here was made for this test alone, which runs
    // on one thread of its own binary.
    let read = |fd: &str| unsafe { read_secret_socket(std::ffi::OsStr::new(fd)) };
    // Still dumpable: refused before the socket is read, and the socket,
    // handed inheritable, is kept from the programs this process starts.
    let fd = socket_holding(b"K=v\0");
    let raw: std::os::fd::RawFd = fd.parse().expect("a number");
    // SAFETY: fcntl with F_SETFD takes an integer and touches no memory.
    assert_eq!(unsafe { libc::fcntl(raw, libc::F_SETFD, 0) }, 0);
    let refused = read(&fd).expect_err("a dumpable process reads no secret");
    assert!(refused.to_string().contains("dumpable"), "{refused}");
    // SAFETY: as above.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    assert_eq!(
        flags & libc::FD_CLOEXEC,
        libc::FD_CLOEXEC,
        "the socket refused while dumpable stays inheritable"
    );
    // Hardened, as the runner and the sandbox helper are before they read.
    // SAFETY: PR_SET_DUMPABLE takes an integer and touches no memory.
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) }, 0);
    assert_eq!(
        read(&fd).expect("read"),
        SecretEnv(vec![("K".into(), "v".into())])
    );
    // A pipe is refused, and so is a descriptor of the standard three.
    let (pipe, write) = std::io::pipe().expect("a pipe");
    drop(write);
    let raw = std::os::fd::OwnedFd::from(pipe).into_raw_fd();
    // Inheritable, as a handed descriptor is.
    // SAFETY: fcntl with F_SETFD takes an integer and touches no memory.
    assert_eq!(unsafe { libc::fcntl(raw, libc::F_SETFD, 0) }, 0);
    let refused = read(&raw.to_string()).expect_err("a pipe is refused");
    assert!(refused.to_string().contains("not a socket"), "{refused}");
    // A refused descriptor is kept from the programs this process starts.
    // SAFETY: as above.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    assert_eq!(
        flags & libc::FD_CLOEXEC,
        libc::FD_CLOEXEC,
        "the refused descriptor stays inheritable"
    );
    // A socket that holds nothing is refused: the control plane never hands
    // one, so it is no secret.
    let refused = read(&socket_holding(b"")).expect_err("an empty socket is refused");
    assert!(refused.to_string().contains("holds nothing"), "{refused}");
    // Nor more than the sender ever hands.
    let long = vec![b'x'; 5000];
    let refused = read(&socket_holding(&long)).expect_err("an over-long socket is refused");
    assert!(refused.to_string().contains("4096"), "{refused}");
    for fd in ["2", "-1", "x"] {
        let refused = read(fd).expect_err(fd);
        assert!(refused.to_string().contains("above 2"), "{fd}: {refused}");
    }
}
