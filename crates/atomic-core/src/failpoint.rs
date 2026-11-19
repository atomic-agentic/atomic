// SPDX-License-Identifier: Apache-2.0

//! Named failpoints in atomic-core, for crash tests.
//!
//! The same mechanism as atomic-durable's `failpoints` module, which lists
//! every site in atomic, these included; this crate does not depend on that
//! one, so it carries its own copy of the switch. With the `failpoints`
//! feature, `ATOMIC_FAILPOINT=<name>` aborts the process at the first
//! failpoint of that name, as if killed, and `ATOMIC_FAILPOINT_LOG=<file>`
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
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log)
            {
                let _ = file.write_all(format!("{name}\n").as_bytes());
            }
        }
        if std::env::var("ATOMIC_FAILPOINT").is_ok_and(|f| f == name) {
            eprintln!("failpoint {name}: aborting");
            std::process::abort();
        }
    }
    #[cfg(not(feature = "failpoints"))]
    let _ = name;
}
