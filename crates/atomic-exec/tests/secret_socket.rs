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
    let read = |fd: &str| read_secret_socket(std::ffi::OsStr::new(fd));
    // Still dumpable: refused before the socket is touched.
    let fd = socket_holding(b"K=v\0");
    let refused = read(&fd).expect_err("a dumpable process reads no secret");
    assert!(refused.to_string().contains("dumpable"), "{refused}");
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
    let fd = std::os::fd::OwnedFd::from(pipe).into_raw_fd().to_string();
    let refused = read(&fd).expect_err("a pipe is refused");
    assert!(refused.to_string().contains("not a socket"), "{refused}");
    for fd in ["2", "-1", "x"] {
        let refused = read(fd).expect_err(fd);
        assert!(refused.to_string().contains("above 2"), "{fd}: {refused}");
    }
}
