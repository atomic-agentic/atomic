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
    /// The description and the action as they were put together, names
    /// apart from text, for a failure made with [`Failure::named`].
    #[serde(skip)]
    said: Option<Box<[Said; 2]>>,
}

impl Failure {
    /// A failure of `kind`.
    pub fn new(kind: &str, description: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            kind: kind.to_owned(),
            description: description.into(),
            action: action.into(),
            said: None,
        }
    }

    /// A failure of `kind` whose description and action name something a
    /// person or a workflow chose, such as a run's id: reported, the names
    /// are written as they are, and only the text around them is redacted.
    pub fn named(kind: &str, description: Said, action: Said) -> Self {
        Self {
            kind: kind.to_owned(),
            description: description.joined(),
            action: action.joined(),
            said: Some(Box::new([description, action])),
        }
    }
}

/// A failure's description or action put together from names, written as
/// they are, and text, redacted when it is reported. Text next to text is
/// redacted together, so a secret's name and its value in it stay read as
/// one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Said(Vec<Part>);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Part {
    Text(String),
    Name(String),
}

impl Said {
    /// Nothing yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// atomic's words, or what an error or a model said: redacted.
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.0.push(Part::Text(text.into()));
        self
    }

    /// A name a person or a workflow chose, such as a run's id: written as
    /// it is.
    pub fn name(mut self, name: &str) -> Self {
        self.0.push(Part::Name(name.to_owned()));
        self
    }

    /// The whole of it, as text.
    fn joined(&self) -> String {
        self.0
            .iter()
            .map(|p| match p {
                Part::Text(t) | Part::Name(t) => t.as_str(),
            })
            .collect()
    }

    /// The whole of it as it is reported: each stretch of text between names
    /// redacted on its own, and the names as they are.
    fn redacted(&self) -> String {
        let mut out = String::new();
        let mut text = String::new();
        for part in &self.0 {
            match part {
                Part::Text(t) => text.push_str(t),
                Part::Name(name) => {
                    out.push_str(&atomic_core::redact::text(&text));
                    text.clear();
                    out.push_str(name);
                }
            }
        }
        let rest = atomic_core::redact::text(&text);
        out.push_str(&rest);
        out
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
                // What to change, in the refusal's own words: a state
                // directory inside the configuration directory is set wrong,
                // while a directory that cannot be *placed* is fixed on the
                // side that could not be placed — the state's home, or
                // `HOME`/`XDG_CONFIG_HOME`, which is not the same advice.
                state::StateError::InsideConfig(outside) => outside.fix(),
                state::StateError::Resolve { side, .. } => side.fix(),
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
    // An error can quote what a model or a command wrote: escaped for the
    // terminal too, once it is redacted, since redaction reads a terminal
    // escape sequence around a token as it is and not as it is shown. An
    // error keeps its own line breaks.
    if json {
        println!(
            "{}",
            crate::shown::compact(&document(failure)).unwrap_or_default()
        );
    } else {
        use atomic_core::redact::Line;
        let [description, action] = shown(failure);
        Line::new().plain("error: ").shown(&description).log();
        Line::new().plain("  to fix: ").shown(&action).log();
    }
}

/// `failure` as `--json` reports it: `{"error": {kind, description,
/// action}}`, redacted.
fn document(failure: &Failure) -> serde_json::Value {
    match &failure.said {
        // Redacted part by part already: redacting the whole again would take
        // a name for a secret.
        Some(said) => serde_json::json!({ "error": {
            "kind": failure.kind,
            "description": said[0].redacted(),
            "action": said[1].redacted(),
        }}),
        None => {
            let mut doc = serde_json::json!({ "error": failure });
            atomic_core::redact::value(&mut doc);
            doc
        }
    }
}

/// `failure`'s description and action for a person: each redacted, then
/// escaped for the terminal on as many lines as it has.
fn shown(failure: &Failure) -> [String; 2] {
    if let Some(said) = &failure.said {
        return [
            crate::shown::block(&said[0].redacted()),
            crate::shown::block(&said[1].redacted()),
        ];
    }
    let safe = |text: &str| crate::shown::block(&atomic_core::redact::text(text));
    [safe(&failure.description), safe(&failure.action)]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failure that names a run reports the run's id as it is, in text and
    /// in JSON, while the text around it is still redacted, before the name
    /// and after it. Redacted whole, `rotate-secret:2026-11` reads as a
    /// secret's name and value, and the id is lost.
    #[test]
    fn a_named_failure_keeps_its_names_and_redacts_its_text() {
        let token = concat!("ghp_", "SAMPLE0123456789", "abcdefghijklmnopqrstuv");
        let id = "rotate-secret:2026-11";
        let failure = Failure::named(
            "run.failed",
            Said::new()
                .text(format!("the push said {token}; run "))
                .name(id)
                .text(format!(" failed: {token}")),
            Said::new()
                .text("continue it with `atomic runs resume ")
                .name(id)
                .text("`"),
        );
        let [description, action] = shown(&failure);
        assert_eq!(
            description,
            format!("the push said [REDACTED]; run {id} failed: [REDACTED]")
        );
        assert_eq!(
            action,
            format!("continue it with `atomic runs resume {id}`")
        );
        let doc = document(&failure);
        assert_eq!(doc["error"]["kind"], "run.failed");
        assert_eq!(doc["error"]["description"], description.as_str());
        assert_eq!(doc["error"]["action"], action.as_str());
        // What the same text, put together with no names, still loses.
        let whole = Failure::new("run.failed", failure.description.clone(), "x");
        assert!(!shown(&whole)[0].contains(id), "{}", shown(&whole)[0]);
        assert!(!document(&whole).to_string().contains(id));
    }

    #[test]
    fn a_coloured_token_in_an_error_is_redacted_before_it_is_escaped() {
        let token = concat!("ghp_", "SAMPLE0123456789", "abcdefghijklmnopqrstuv");
        let description = format!("the push said \u{1b}[1;33m{token}\u{1b}[0m");
        let [shown, _] = shown(&Failure::new("x.y", description, "mend it"));
        assert!(!shown.contains("SAMPLE0123456789"), "the token showed");
        assert!(shown.contains("[REDACTED]"), "{shown}");
        assert!(!shown.contains('\u{1b}'), "an escape reached the terminal");
        // A control that redaction reads as white space between a secret's
        // name and its value: escaped first, it would no longer be.
        let secret = concat!("SEEDED", "q7Lm2Vx9Rk4Tz8");
        for (case, description) in [
            format!("the push sent Authorization: Bearer\u{c}{secret}"),
            format!("the command ran with --password\u{b}{secret}"),
        ]
        .into_iter()
        .enumerate()
        {
            let [written, _] = super::shown(&Failure::new("x.y", description, "mend it"));
            assert!(
                !written.contains(&secret[6..]),
                "case {case}: the secret showed"
            );
        }
    }

    #[test]
    fn a_multi_line_error_keeps_its_lines_and_tabs() {
        let f = Failure::new("x.y", "first\n\tsecond\u{1b}[2J", "one\ntwo");
        let [description, action] = shown(&f);
        assert_eq!(description, "first\n\tsecond\\u{001b}[2J");
        assert_eq!(action, "one\ntwo");
    }

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

    /// A directory atomic cannot place is named in the description, and the
    /// side to fix is the same side in the action: the state's own home, or the
    /// configuration directory under `HOME` or `XDG_CONFIG_HOME`. Setting the
    /// other is no remedy for a failure to place this one.
    #[test]
    fn an_unplaceable_directory_names_the_side_to_fix() {
        let refused = |side: state::ResolveSide| {
            analyze(&anyhow::Error::new(state::StateError::Resolve {
                side,
                path: "/home/dev/.config/atomic".into(),
                why: "it cannot be read: Permission denied (os error 13)".into(),
            }))
        };
        let state_side = refused(state::ResolveSide::State);
        assert_eq!(state_side.kind, "state.unavailable");
        assert!(
            state_side
                .description
                .contains("cannot resolve atomic's state directory"),
            "{}",
            state_side.description
        );
        assert!(
            state_side
                .action
                .starts_with("set ATOMIC_HOME to a directory atomic can place"),
            "the action of an unplaceable state directory: {}",
            state_side.action
        );
        assert!(
            !state_side.action.contains("XDG_CONFIG_HOME"),
            "the action of an unplaceable state directory sends the person to their \
             configuration directory: {}",
            state_side.action
        );
        let config_side = refused(state::ResolveSide::Config);
        assert!(
            config_side
                .description
                .contains("cannot resolve your configuration directory"),
            "{}",
            config_side.description
        );
        assert!(
            config_side
                .action
                .starts_with("fix HOME, or XDG_CONFIG_HOME,"),
            "the action of an unplaceable configuration directory: {}",
            config_side.action
        );
        assert!(
            !config_side.action.contains("set ATOMIC_HOME"),
            "the action of an unplaceable configuration directory sends the person to \
             ATOMIC_HOME: {}",
            config_side.action
        );
    }
}
