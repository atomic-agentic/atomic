// SPDX-License-Identifier: Apache-2.0

//! Actionable errors: every error the CLI reports says what went wrong and
//! what to do about it.
//!
//! A [`Failure`] carries the two separately: a description and an action,
//! with a stable `kind` for scripts. Code that knows the remedy where it
//! fails returns a `Failure` directly. Anything else is turned into one by
//! [`analyze`], which, like a failure analyzer, recognises the errors
//! atomic's crates return and supplies the action from what they carry.
//!
//! With `--json` the failure is printed as one JSON object on stdout, with
//! the action as a field of its own; otherwise as text on stderr.

use atomic_core::{atom, collections, project, state, workflow};
use serde::Serialize;

/// What went wrong and what to do about it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, thiserror::Error)]
#[error("{description}")]
pub struct Failure {
    /// A stable name for the kind of failure, such as `workflow.invalid`.
    pub kind: String,
    /// What went wrong.
    pub description: String,
    /// What to change or do to fix it.
    pub action: String,
}

impl Failure {
    /// A failure of `kind`.
    pub fn new(kind: &str, description: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            kind: kind.to_owned(),
            description: description.into(),
            action: action.into(),
        }
    }
}

/// The failure an error amounts to: the first error in its chain that atomic
/// knows the remedy for, described by the whole chain.
pub fn analyze(error: &anyhow::Error) -> Failure {
    for cause in error.chain() {
        if let Some(f) = cause.downcast_ref::<Failure>() {
            return f.clone();
        }
        if let Some(f) = known(cause) {
            return Failure {
                description: chain(error),
                ..f
            };
        }
    }
    Failure::new(
        "error",
        chain(error),
        "atomic has no specific advice for this error; the description says what failed, \
         and RUST_BACKTRACE=1 shows where",
    )
}

/// Every message in the chain, outermost first, without repeats.
fn chain(error: &anyhow::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    for cause in error.chain() {
        let text = cause.to_string();
        if !parts.iter().any(|p| p.contains(&text)) {
            parts.push(text);
        }
    }
    parts.join(": ")
}

fn known(cause: &(dyn std::error::Error + 'static)) -> Option<Failure> {
    let f = |kind: &str, action: String| Some(Failure::new(kind, "", action));
    if let Some(e) = cause.downcast_ref::<workflow::WorkflowError>() {
        let action = e.action.clone().unwrap_or_else(|| {
            format!(
                "fix {} at line {} ({}), then run `atomic check` to check it again",
                e.file.display(),
                e.line,
                if e.path.is_empty() { "/" } else { &e.path }
            )
        });
        return f("workflow.invalid", action);
    }
    if let Some(e) = cause.downcast_ref::<atom::AtomError>() {
        return f(
            "atom.invalid",
            format!(
                "fix {} at line {}, then run `atomic check {}` to check it again",
                e.file.display(),
                e.line,
                e.file.display()
            ),
        );
    }
    if let Some(e) = cause.downcast_ref::<project::ProjectError>() {
        return match e {
            project::ProjectError::NotFound(_) => f(
                "project.not_found",
                "run atomic inside a project (a directory with atomic.yaml or \
                 .atomic/atomic.yaml, or below one), pass --project with the manifest or its \
                 directory, or give the path of an .atom.astro or .workflow.yaml file"
                    .into(),
            ),
            project::ProjectError::BothManifests(dir) => f(
                "project.ambiguous",
                format!(
                    "delete one of {0}/atomic.yaml and {0}/.atomic/atomic.yaml",
                    dir.display()
                ),
            ),
            project::ProjectError::Read { path, .. } => f(
                "project.unreadable",
                format!("check that {} exists and is readable", path.display()),
            ),
            project::ProjectError::Yaml(_) | project::ProjectError::Invalid { .. } => f(
                "project.invalid",
                "fix the manifest where the description points, then run `atomic check`".into(),
            ),
        };
    }
    if let Some(e) = cause.downcast_ref::<collections::CollectError>() {
        return f(
            "project.collection",
            match e {
                collections::CollectError::DuplicateName { .. } => {
                    "rename one of the two files, or set a different `name` in one of them".into()
                }
                _ => "fix the project's collections where the description points".into(),
            },
        );
    }
    if let Some(e) = cause.downcast_ref::<state::LockError>() {
        return match e {
            state::LockError::Held { owner, .. } => f(
                "runtime.busy",
                format!(
                    "wait for that process to finish, or stop it ({owner}); a run it leaves \
                     unfinished can be continued with `atomic runs resume <run-id>` or by \
                     `atomic serve`"
                ),
            ),
            state::LockError::Io { path, .. } => f(
                "runtime.lock",
                format!(
                    "make {} writable by this user, or set ATOMIC_HOME to a directory that is",
                    path.parent().unwrap_or(path).display()
                ),
            ),
        };
    }
    if let Some(e) = cause.downcast_ref::<state::StateError>() {
        return f(
            "state.unavailable",
            match e {
                state::StateError::NoDataDir => {
                    "set ATOMIC_HOME to a directory for atomic's state".into()
                }
                state::StateError::InUse { path, .. } => format!(
                    "stop the other atomic process using {} (an older atomic's `atomic serve`, \
                     `run` or `runs resume`), then run atomic again; its state moves then",
                    path.display()
                ),
                state::StateError::Both { old, current } => format!(
                    "keep the copy that holds this project's runs at {}, move {} out of the \
                     state directory, then run atomic again",
                    current.display(),
                    old.display()
                ),
                state::StateError::InsideConfig { config, .. } => format!(
                    "move ATOMIC_HOME outside {}, or point XDG_CONFIG_HOME, or HOME, \
                     elsewhere; a worktree or a cache inside your own configuration \
                     directory is out of atoms' commands' reach",
                    config.display()
                ),
                _ => "set ATOMIC_HOME to a writable directory for atomic's state, or fix the \
                      permissions the description names"
                    .into(),
            },
        );
    }
    if let Some(e) = cause.downcast_ref::<atomic_durable::IdentityError>() {
        return f(
            match e {
                atomic_durable::IdentityError::VersionVariable { .. } => "config.engine_version",
                _ => "config.executor_id",
            },
            match e {
                atomic_durable::IdentityError::EngineVariable { variable, value } => format!(
                    "unset {variable} and set ATOMIC_EXECUTOR_ID={value} (or pass \
                     --executor-id) instead"
                ),
                atomic_durable::IdentityError::VersionVariable { variable, .. } => format!(
                    "unset {variable}: atomic stamps each run with its own durable step layout, \
                     so any atomic of that layout continues the run"
                ),
                atomic_durable::IdentityError::Invalid(_) => {
                    "set ATOMIC_EXECUTOR_ID to a name of letters, digits, `.`, `_`, `:` and \
                     `-`, unique to this process and the same across its restarts"
                        .into()
                }
            },
        );
    }
    if let Some(e) = cause.downcast_ref::<atomic_app::pipeline::PrepareError>() {
        return f(
            "workflow.unsupported",
            format!(
                "change workflow `{}` so this version of atomic can run it, as the \
                 description says",
                e.workflow
            ),
        );
    }
    None
}

/// Print `failure` for a person or, with `json`, for a script.
/// Report `failure`, with any secret its text carries redacted: an error
/// can quote what a provider or a command said.
pub fn report(failure: &Failure, json: bool) {
    if json {
        let mut doc = serde_json::json!({ "error": failure });
        atomic_core::redact::value(&mut doc);
        println!("{doc}");
    } else {
        atomic_core::log_line!("error: {}", failure.description);
        atomic_core::log_line!("  to fix: {}", failure.action);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_passes_through_with_its_own_action() {
        let error =
            anyhow::Error::new(Failure::new("x.y", "it broke", "mend it")).context("while running");
        let f = analyze(&error);
        assert_eq!(f.kind, "x.y");
        assert_eq!(f.action, "mend it");
    }

    #[test]
    fn a_known_error_gets_its_action_and_keeps_the_whole_description() {
        let error = anyhow::Error::new(project::ProjectError::NotFound("/tmp/x".into()))
            .context("find the project");
        let f = analyze(&error);
        assert_eq!(f.kind, "project.not_found");
        assert!(
            f.description
                .starts_with("find the project: no atomic.yaml")
        );
        assert!(f.action.contains("--project"), "{}", f.action);
    }

    #[test]
    fn an_unknown_error_still_has_an_action() {
        let f = analyze(&anyhow::anyhow!("something odd"));
        assert_eq!(f.kind, "error");
        assert_eq!(f.description, "something odd");
        assert!(!f.action.is_empty());
    }
}
