// SPDX-License-Identifier: Apache-2.0

//! Named failpoints, where a crash test kills the process.
//!
//! A failpoint is a named place in atomic's code. With the `failpoints`
//! feature, three environment variables drive them:
//!
//! - `ATOMIC_FAILPOINT=<name>`: the process kills itself with SIGKILL at the
//!   first failpoint of that name. Nothing is flushed, no destructor runs, the
//!   engine does not shut down and no core is dumped.
//! - `ATOMIC_FAILPOINT_PANIC=<name>`: the code that reaches the first
//!   failpoint of that name panics there, as a bug in atomic would; the
//!   process goes on as it does after any panic.
//! - `ATOMIC_FAILPOINT_LOG=<file>`: every failpoint the process reaches is
//!   appended to `<file>`, one name per line, before it is checked. A log
//!   that cannot be written kills the process too, saying why, so a test
//!   never reads a partial log as a whole one. A crash
//!   test runs a workflow once with the log on, reads which failpoints it
//!   passed and in what order, and then kills a fresh run at each in turn.
//!
//! Without the feature, [`failpoint`] does nothing and [`armed`] is false.
//!
//! Names are built from a site's template: `checkpoint:before:{op}` reached
//! by the step `read.turn` is `checkpoint:before:read.turn`. [`SITES`] lists
//! every site in atomic, whichever crate it is in; [`site_of`] finds the site
//! a name comes from. atomic-core, which does not depend on this crate, has
//! its own `failpoint` with the same variables and the same behaviour.

/// One failpoint site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Site {
    /// The name, or a template whose one `{...}` placeholder, always last,
    /// stands for the operation, step or request it is reached for.
    pub name: &'static str,
    /// Where the process is when it stops here.
    pub what: &'static str,
}

impl Site {
    /// Whether `name` is a failpoint of this site.
    pub fn matches(&self, name: &str) -> bool {
        match self.name.split_once('{') {
            Some((prefix, _)) => name.len() > prefix.len() && name.starts_with(prefix),
            None => name == self.name,
        }
    }
}

/// Every failpoint site in atomic, for the v0.1 crash suite.
pub const SITES: &[Site] = &[
    Site {
        name: "checkpoint:before:{op}",
        what: "a durable step's body, or a transaction's writes, finished; its checkpoint is \
               not committed",
    },
    Site {
        name: "checkpoint:after:{op}",
        what: "the operation's checkpoint is committed by the process that ran it (a replay \
               does not reach it)",
    },
    Site {
        name: "atom:after-call:{request}",
        what: "mid-atom: one model call was made and its usage settled; the atom's turn goes on",
    },
    Site {
        name: "atom:after-admit:{request}",
        what: "mid-atom: a model call is admitted and its bound reserved; it is not dispatched",
    },
    Site {
        name: "usage:after-progress:{request}",
        what: "mid-turn: a Codex turn's reported usage so far is recorded, and the turn goes on; \
               its name is the request and the record's number, `<request>:<n>`",
    },
    Site {
        name: "usage:before-settle:{request}",
        what: "mid-atom: the provider answered the call and its usage is not yet recorded",
    },
    Site {
        name: "recv:after:{topic}",
        what: "a message wait's outcome, the message taken or the timeout, is recorded (a replay \
               reaches it too)",
    },
    Site {
        name: "worktree:after-prepare:{step}",
        what: "a code-changing step's worktree is reset to its base and the atom has not started",
    },
    Site {
        name: "cleanup:after-refs:{run}",
        what: "a finished run's unaccepted attempt refs are deleted and its worktrees are not yet \
               removed",
    },
    Site {
        name: "usage:after-park",
        what: "a budget left no room for the next model call and the park is recorded",
    },
    Site {
        name: "pipeline:ran:{step}",
        what: "an atom step's work is done, inside its turn, before the turn's checkpoint",
    },
    Site {
        name: "pipeline:after:{step}",
        what: "a step is published, or a gate decided, and the next step has not started",
    },
    Site {
        name: "commit:after-stage",
        what: "mid-commit: the worktree is staged and written as a tree; no commit exists",
    },
    Site {
        name: "commit:before-ref:{step}",
        what: "mid-commit: the commit object exists and no attempt ref names it",
    },
    Site {
        name: "pipeline:after-commit",
        what: "atomic committed a worktree step's work and kept its attempt ref; the turn's \
               checkpoint is not written",
    },
    Site {
        name: "gate:after-open",
        what: "the gate's row is committed and the run has not started waiting",
    },
    Site {
        name: "gate:after-wait",
        what: "the run woke, or its deadline passed, and it has not read the decision",
    },
    Site {
        name: "gate:before-wake",
        what: "the decision is committed and the waiting run has not been woken",
    },
    Site {
        name: "change-request:after-draft",
        what: "the change-request workflow published its draft and has not started the review",
    },
];

/// Failpoint sites in atomic's commands rather than in the workflows the
/// crash suite runs, such as `atomic runs resume` and a park's release:
/// their own tests drive them.
pub const COMMAND_SITES: &[Site] = &[
    Site {
        name: "resume:queued",
        what: "runs resume put the run on its resume queue and its engine has not started \
               dispatching it",
    },
    Site {
        name: "release:before-wake",
        what: "a park's release is committed and its run has not been woken",
    },
    Site {
        name: "release:redeliver",
        what: "a process is about to wake the runs of released parks whose wake-up may have \
               been lost",
    },
];

/// The site `name` comes from, if it is one of [`SITES`] or
/// [`COMMAND_SITES`].
pub fn site_of(name: &str) -> Option<&'static Site> {
    SITES.iter().chain(COMMAND_SITES).find(|s| s.matches(name))
}

/// Whether failpoints are compiled in. Callers that build a name check this
/// first, so a build without the feature does not format one.
pub const fn armed() -> bool {
    cfg!(feature = "failpoints")
}

#[cfg(feature = "failpoints")]
static CLI_FAILPOINT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Arm one named crash point from a development CLI command. The process
/// starts only one command, so a second selection is a configuration error.
/// The name must be a known site; callers further restrict it to a step in
/// the run being started.
#[cfg(feature = "failpoints")]
pub fn arm_cli(name: &str) -> Result<(), &'static str> {
    if site_of(name).is_none() {
        return Err("unknown failpoint site");
    }
    CLI_FAILPOINT
        .set(name.to_owned())
        .map_err(|_| "a CLI failpoint is already armed")
}

/// A named point where a crash test can kill the process: see the module
/// documentation. Without the `failpoints` feature it does nothing.
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
        let cli = CLI_FAILPOINT.get();
        if cli.is_some_and(|selected| selected == name)
            || (cli.is_none() && std::env::var("ATOMIC_FAILPOINT").is_ok_and(|f| f == name))
        {
            eprintln!("failpoint {name}: killing the process");
            kill_self();
        }
        // A panic in atomic's own code at this point, for a test of how a
        // panicked run is reported.
        if std::env::var("ATOMIC_FAILPOINT_PANIC").is_ok_and(|f| f == name) {
            panic!("failpoint {name}: atomic's own code panicked here, as a test asked");
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_finds_its_site() {
        let site = |n: &str| site_of(n).map(|s| s.name);
        assert_eq!(
            site("checkpoint:before:read.turn"),
            Some("checkpoint:before:{op}")
        );
        assert_eq!(site("pipeline:after:rank"), Some("pipeline:after:{step}"));
        assert_eq!(site("pipeline:after-commit"), Some("pipeline:after-commit"));
        assert_eq!(site("gate:after-open"), Some("gate:after-open"));
        // A template needs something in its placeholder.
        assert_eq!(site("pipeline:after:"), None);
        assert_eq!(site("gate:after-open:x"), None);
        assert_eq!(site("nowhere"), None);
    }

    #[test]
    fn templates_do_not_overlap() {
        for a in SITES {
            let example = a.name.replace("{", "").replace("}", "");
            let found: Vec<_> = SITES.iter().filter(|s| s.matches(&example)).collect();
            assert_eq!(found, [a], "{example} matches more than its own site");
        }
    }
}
