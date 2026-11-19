// SPDX-License-Identifier: Apache-2.0

//! Named failpoints, where a crash test kills the process.
//!
//! A failpoint is a named place in atomic's code. With the `failpoints`
//! feature, two environment variables drive them:
//!
//! - `ATOMIC_FAILPOINT=<name>`: the process aborts at the first failpoint of
//!   that name, as if killed. Nothing is flushed, no destructor runs and the
//!   engine does not shut down.
//! - `ATOMIC_FAILPOINT_LOG=<file>`: every failpoint the process reaches is
//!   appended to `<file>`, one name per line, before it is checked. A crash
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
        name: "usage:before-settle",
        what: "mid-atom: the provider was called and its usage is not yet recorded",
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

/// The site `name` comes from, if it is one of [`SITES`].
pub fn site_of(name: &str) -> Option<&'static Site> {
    SITES.iter().find(|s| s.matches(name))
}

/// Whether failpoints are compiled in. Callers that build a name check this
/// first, so a build without the feature does not format one.
pub const fn armed() -> bool {
    cfg!(feature = "failpoints")
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
