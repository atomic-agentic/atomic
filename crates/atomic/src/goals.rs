// SPDX-License-Identifier: Apache-2.0
//! Human goal commands over pinned work-graph items.

use anyhow::{Context, bail};
use atomic_core::state::ProjectState;
use atomic_durable::Database;
use atomic_graph::goal::{GoalDefinition, GoalFeedbackKind, GoalMeasure};
use atomic_graph::sqlite::SqliteStore;
use clap::{Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Subcommand)]
pub enum GoalsCommand {
    /// Add goals/<name>.goal.yaml as a pinned graph item.
    Add { name: String },
    /// List the project's registered goals.
    List,
    /// Show one goal's measure, budget, wakes, children and feedback.
    Show { name: String },
    /// Stop admitting new wakes for a goal; its children stay ready.
    Pause { name: String },
    /// Admit new wakes for a paused goal.
    Resume { name: String },
    /// Append feedback from the enrolled operator.
    Feedback {
        name: String,
        text: String,
        /// Note, gate-rejection, answer or steering.
        #[arg(long, value_enum, default_value_t = FeedbackKindArg::Note)]
        kind: FeedbackKindArg,
        /// Stable idempotency key for a retry of the same feedback.
        #[arg(long)]
        request_id: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum FeedbackKindArg {
    Note,
    GateRejection,
    Answer,
    Steering,
}

impl From<FeedbackKindArg> for GoalFeedbackKind {
    fn from(value: FeedbackKindArg) -> Self {
        match value {
            FeedbackKindArg::Note => Self::Note,
            FeedbackKindArg::GateRejection => Self::GateRejection,
            FeedbackKindArg::Answer => Self::Answer,
            FeedbackKindArg::Steering => Self::Steering,
        }
    }
}

fn goal_id(name: &str) -> anyhow::Result<String> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("goal name must be a single 1..=64 byte component of letters, numbers, - or _");
    }
    Ok(format!("goal-{name}"))
}

#[cfg(unix)]
fn read_goal_file(root: &Path, name: &str) -> anyhow::Result<(PathBuf, String)> {
    use std::ffi::CString;
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd};

    let _ = goal_id(name)?;
    let directory = root.join("goals");
    let path = directory.join(format!("{name}.goal.yaml"));
    let root = std::fs::File::open(root).context("open the project directory")?;
    let goals = CString::new("goals")?;
    // SAFETY: the root descriptor is live and the fixed single-component name
    // is NUL-terminated. The returned descriptor has one File owner below.
    #[allow(unsafe_code)]
    let dir_fd = unsafe {
        libc::openat(
            root.as_raw_fd(),
            goals.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if dir_fd < 0 {
        return Err(std::io::Error::last_os_error())
            .context("open goals directory without following a link");
    }
    // SAFETY: openat returned this owned descriptor and no other File owns it.
    #[allow(unsafe_code)]
    let dir = unsafe { std::fs::File::from_raw_fd(dir_fd) };
    let filename = CString::new(format!("{name}.goal.yaml"))?;
    // SAFETY: `dir` owns a live descriptor and `filename` is one checked
    // component. A successful descriptor is taken into exactly one File.
    #[allow(unsafe_code)]
    let file_fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            filename.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if file_fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("open {} without following a link", path.display()));
    }
    // SAFETY: openat returned this owned descriptor and no other File owns it.
    #[allow(unsafe_code)]
    let file = unsafe { std::fs::File::from_raw_fd(file_fd) };
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > 65_536 {
        bail!(
            "{} must be a regular file of at most 65536 bytes",
            path.display()
        );
    }
    let mut text = String::new();
    file.take(65_537).read_to_string(&mut text)?;
    if text.len() > 65_536 {
        bail!("{} exceeds 65536 bytes", path.display());
    }
    Ok((path, text))
}

#[cfg(not(unix))]
fn read_goal_file(_root: &Path, _name: &str) -> anyhow::Result<(PathBuf, String)> {
    bail!("goal file loading needs no-follow file opening on this platform")
}

fn load_goal(root: &Path, name: &str) -> anyhow::Result<GoalDefinition> {
    let (path, text) = read_goal_file(root, name)?;
    let doc = atomic_schema::yaml::load(&path, &text)?;
    let definition: GoalDefinition = serde_json::from_value(doc.value)
        .with_context(|| format!("parse goal definition {}", path.display()))?;
    definition.validate().map_err(anyhow::Error::msg)?;
    Ok(definition)
}

pub async fn run(command: GoalsCommand, state: &ProjectState, json: bool) -> anyhow::Result<()> {
    let database = crate::engine::database(state)?;
    if matches!(database, Database::Postgres { .. }) {
        bail!("goal commands currently require a SQLite project store");
    }
    let privileged = matches!(
        command,
        GoalsCommand::Add { .. }
            | GoalsCommand::Pause { .. }
            | GoalsCommand::Resume { .. }
            | GoalsCommand::Feedback { .. }
    );
    let store = if privileged {
        SqliteStore::open_authorized(
            &state.database(),
            SystemTime::now,
            crate::gates::operator_credential(state)?,
        )
    } else {
        SqliteStore::open(&state.database(), SystemTime::now)
    }?;
    match command {
        GoalsCommand::Add { name } => {
            let id = goal_id(&name)?;
            let definition = load_goal(state.root(), &name)?;
            store.add_goal(&id, definition).await?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({"id":id,"status":"pinned","active":true})
                );
            } else {
                println!("added {id}");
            }
        }
        GoalsCommand::List => {
            let goals = store.list_goals().await?;
            if json {
                println!("{}", serde_json::to_string(&goals)?);
            } else {
                for goal in goals {
                    println!(
                        "{}  {}  {}",
                        goal.id,
                        if goal.active { "active" } else { "paused" },
                        goal.title
                    );
                }
            }
        }
        GoalsCommand::Show { name } => {
            let brief = store.goal_brief(&goal_id(&name)?).await?;
            if json {
                println!("{}", serde_json::to_string(&brief)?);
            } else {
                println!(
                    "{}: {} ({})",
                    brief.id,
                    brief.definition.title,
                    if brief.active { "active" } else { "paused" }
                );
                println!(
                    "budget remaining: {} tokens and {} runs per {:?}",
                    brief.tokens_remaining, brief.runs_remaining, brief.definition.budget.period
                );
                match &brief.definition.measure {
                    Some(GoalMeasure::Tools { names }) => {
                        println!("measure: tools {}", names.join(", "));
                    }
                    Some(GoalMeasure::Cel { expr }) => println!("measure: CEL {expr}"),
                    None => println!("measure: none"),
                }
                if let Some(sample) = &brief.measure {
                    println!("last measure: {}", sample.value);
                }
                for wake in brief.wakes {
                    println!(
                        "wake {}: {} — {}",
                        wake.wake_id,
                        wake.outcome.as_str(),
                        wake.reason
                    );
                }
                for child in brief.open_children {
                    println!("open child: {} ({})", child.id, child.status);
                }
                for child in brief.closed_children {
                    println!("closed child: {} ({})", child.id, child.status);
                }
                if brief.children_truncated {
                    println!(
                        "showing first 200 of {} children (open first)",
                        brief.total_children
                    );
                }
                for feedback in brief.feedback {
                    println!("feedback {}: {}", feedback.kind.as_str(), feedback.text);
                }
            }
        }
        GoalsCommand::Pause { name } => {
            let id = goal_id(&name)?;
            store.set_goal_active(&id, false).await?;
            if json {
                println!("{}", serde_json::json!({"id":id,"active":false}));
            } else {
                println!("paused {id}");
            }
        }
        GoalsCommand::Resume { name } => {
            let id = goal_id(&name)?;
            store.set_goal_active(&id, true).await?;
            if json {
                println!("{}", serde_json::json!({"id":id,"active":true}));
            } else {
                println!("resumed {id}");
            }
        }
        GoalsCommand::Feedback {
            name,
            text,
            kind,
            request_id,
        } => {
            let id = goal_id(&name)?;
            let request = request_id.unwrap_or(format!(
                "goal-cli-{}",
                atomic_core::credential::fresh_secret()?
            ));
            store
                .add_goal_feedback(&id, &request, kind.into(), &text)
                .await?;
            if json {
                println!("{}", serde_json::json!({"id":id,"request_id":request}));
            } else {
                println!("feedback recorded for {id}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{goal_id, load_goal};
    use std::path::Path;

    #[test]
    fn goal_name_is_one_component() {
        assert_eq!(
            goal_id("weekly-review").expect("goal test fixture"),
            "goal-weekly-review"
        );
        for name in ["", "../secret", "a/b", ".hidden", "bad.yaml"] {
            assert!(goal_id(name).is_err(), "{name}");
        }
    }

    #[test]
    fn malformed_goal_yaml_is_refused() {
        let root = std::env::temp_dir().join(format!("atomic-goal-file-{}", std::process::id()));
        let goals = root.join("goals");
        std::fs::create_dir_all(&goals).expect("goal test fixture");
        std::fs::write(
            goals.join("malformed.goal.yaml"),
            "title: Missing budget\nunknown: x\n",
        )
        .expect("goal test fixture");
        assert!(load_goal(&root, "malformed").is_err());
        std::fs::write(
            goals.join("valid.goal.yaml"),
            "title: Read failures\nbudget:\n  period: day\n  tokens: 100\n  runs: 10\n",
        )
        .expect("goal test fixture");
        assert_eq!(
            load_goal(&root, "valid").expect("goal test fixture").title,
            "Read failures"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn goal_file_loader_refuses_symlinked_file_and_directory() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("atomic-goal-link-{}", std::process::id()));
        let goals = root.join("goals");
        std::fs::create_dir_all(&goals).expect("goal test fixture");
        let outside = root.join("outside.yaml");
        std::fs::write(
            &outside,
            "title: Outside\nbudget: {period: day, tokens: 1}\n",
        )
        .expect("goal test fixture");
        symlink(&outside, goals.join("linked.goal.yaml")).expect("goal test fixture");
        assert!(load_goal(&root, "linked").is_err());
        let second = root.join("second");
        std::fs::create_dir_all(&second).expect("goal test fixture");
        symlink(&outside, second.join("through-dir.goal.yaml")).expect("goal test fixture");
        std::fs::remove_dir_all(&goals).expect("goal test fixture");
        symlink(&second, &goals).expect("goal test fixture");
        assert!(load_goal(Path::new(&root), "through-dir").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
