// SPDX-License-Identifier: Apache-2.0

//! Named failpoints in atomic-core, for crash tests.
//!
//! The same mechanism as atomic-durable's `failpoints` module, which lists
//! every site in atomic, these included; this crate does not depend on that
//! one, so it carries its own copy of the switch. With the `failpoints`
//! feature, `ATOMIC_FAILPOINT=<name>` kills the process (SIGKILL) at the first
//! failpoint of that name, and `ATOMIC_FAILPOINT_LOG=<file>`
//! appends every failpoint reached to `<file>`, one name per line. Without the
//! feature, [`failpoint`] does nothing and [`armed`] is false.

/// Whether failpoints are compiled in. Callers that build a name check this
/// first, so a build without the feature does not format one.
pub const fn armed() -> bool {
    cfg!(feature = "failpoints")
}

/// A named point where a crash test can kill the process.
pub fn failpoint(name: &str) {
    #[cfg(feature = "failpoints")]
    {
        if let Some(log) = std::env::var_os("ATOMIC_FAILPOINT_LOG") {
            use std::io::Write;
            // One write of the whole line, appended, so lines from threads of
            // one process do not interleave.
            let written = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log)
                .and_then(|mut file| file.write_all(format!("{name}\n").as_bytes()));
            // A test that reads the log must not read a partial one as the
            // whole truth: stop here, loudly.
            if let Err(e) = written {
                eprintln!(
                    "failpoint {name}: cannot write the failpoint log {}: {e}",
                    std::path::Path::new(&log).display()
                );
                kill_self();
            }
        }
        if std::env::var("ATOMIC_FAILPOINT").is_ok_and(|f| f == name) {
            eprintln!("failpoint {name}: killing the process");
            kill_self();
        }
    }
    #[cfg(not(feature = "failpoints"))]
    let _ = name;
}

/// A named point where a test can hold this thread up, as a busy machine's
/// scheduler might: with the `failpoints` feature,
/// `ATOMIC_FAILPOINT_PAUSE=<name>` makes it wait a second there. Without the
/// feature it does nothing. It kills nothing, so it is not a crash site.
pub fn pause(name: &str) {
    #[cfg(feature = "failpoints")]
    if std::env::var("ATOMIC_FAILPOINT_PAUSE").is_ok_and(|f| f == name) {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    #[cfg(not(feature = "failpoints"))]
    let _ = name;
}

/// End the process as a crash would: SIGKILL, which runs nothing on the way
/// out and leaves no core dump.
#[cfg(feature = "failpoints")]
#[allow(unsafe_code)]
fn kill_self() -> ! {
    #[cfg(unix)]
    // SAFETY: kill(2) on this process's own id, with a signal number from
    // libc; it takes no pointers.
    unsafe {
        libc::kill(libc::getpid(), libc::SIGKILL);
    }
    // SIGKILL cannot be caught or ignored, so this is not reached; where
    // there is no SIGKILL, abort ends the process just as abruptly.
    std::process::abort()
}
