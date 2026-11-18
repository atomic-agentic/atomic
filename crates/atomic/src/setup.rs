// SPDX-License-Identifier: Apache-2.0

//! `atomic setup <agent>`: install the authoring skills for a coding agent.
//!
//! The pack goes where the agent looks for project skills: Codex reads the
//! cross-tool `.agents/skills/`, Claude Code its own `.claude/skills/`. A short
//! section between marker lines in the agent's instructions file (`AGENTS.md`
//! or `CLAUDE.md`) points the agent at the skill; running setup again
//! replaces that section and the pack's files, and leaves everything else in
//! the directory as it was.

use crate::skills;
use anyhow::{Context, bail, ensure};
use std::path::{Component, Path, PathBuf};

/// A coding agent atomic can set up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Agent {
    /// Claude Code.
    Claude,
    /// OpenAI Codex, including its editor extensions.
    Codex,
}

impl Agent {
    /// The agent's product name.
    pub fn title(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex",
        }
    }

    /// Where the agent reads project skills, relative to the project.
    pub fn skills_dir(self) -> &'static str {
        match self {
            Agent::Claude => ".claude/skills",
            Agent::Codex => ".agents/skills",
        }
    }

    /// The instructions file the agent reads, relative to the project.
    pub fn instructions_file(self) -> &'static str {
        match self {
            Agent::Claude => "CLAUDE.md",
            Agent::Codex => "AGENTS.md",
        }
    }
}

/// Arguments of `atomic setup`.
#[derive(Debug, clap::Args)]
pub struct SetupArgs {
    /// The coding agent to set up.
    #[arg(value_enum)]
    pub agent: Agent,
    /// The repository to set up; defaults to the current directory.
    #[arg(long)]
    pub path: Option<PathBuf>,
}

const BEGIN: &str =
    "<!-- BEGIN atomic: written by `atomic setup`; edits between these lines are replaced -->";
const END: &str = "<!-- END atomic -->";
/// How a section written by any version of atomic starts.
const BEGIN_PREFIX: &str = "<!-- BEGIN atomic";

/// The section added to the agent's instructions file.
fn section() -> String {
    format!(
        "{BEGIN}\n## atomic\n\n\
         This repository defines typed, single-purpose agents (atoms) with atomic. To \
         create or change an atom, use the `{skill}` skill. `atomic check` is the \
         authority on whether atoms are valid: run it after every change and fix what it \
         reports.\n{END}\n",
        skill = skills::CREATE_ATOM,
    )
}

/// Run `atomic setup`.
pub fn run(args: SetupArgs) -> anyhow::Result<()> {
    let root = match args.path {
        Some(path) => path,
        None => std::env::current_dir().context("read the working directory")?,
    };
    let written = install(args.agent, &root)?;
    println!(
        "set up atomic's authoring skills for {} in {}",
        args.agent.title(),
        root.display()
    );
    for path in written {
        println!("  {}", path.display());
    }
    Ok(())
}

/// Install the pack for `agent` in the repository at `root`, and return the
/// files written, relative to `root`.
pub fn install(agent: Agent, root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let root =
        std::fs::canonicalize(root).with_context(|| format!("cannot open {}", root.display()))?;
    ensure!(root.is_dir(), "{} is not a directory", root.display());
    let pack = skills::pack().map_err(anyhow::Error::msg)?;
    for file in &pack {
        if let Some(dir) = file.path.strip_suffix("/SKILL.md") {
            let problems = skills::validate_skill(dir, &file.contents);
            ensure!(
                problems.is_empty(),
                "skill {dir} does not meet the Agent Skills specification: {}",
                problems.join("; ")
            );
        }
    }
    let mut written = Vec::new();
    for file in pack {
        let relative = Path::new(agent.skills_dir()).join(&file.path);
        write_inside(&root, &relative, file.contents.as_bytes())?;
        written.push(relative);
    }
    let instructions = Path::new(agent.instructions_file());
    let current = match std::fs::read_to_string(root.join(instructions)) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("read {}", instructions.display())),
    };
    let updated = with_section(current.as_deref(), &section())
        .with_context(|| format!("update {}", instructions.display()))?;
    if current.as_deref() != Some(updated.as_str()) {
        write_inside(&root, instructions, updated.as_bytes())?;
        written.push(instructions.to_owned());
    }
    Ok(written)
}

/// `text` with atomic's section replaced, or appended when it has none.
fn with_section(text: Option<&str>, section: &str) -> anyhow::Result<String> {
    let Some(text) = text else {
        return Ok(section.to_owned());
    };
    match (text.find(BEGIN_PREFIX), text.find(END)) {
        (Some(begin), Some(end)) if begin < end => {
            let mut end = end + END.len();
            if text[end..].starts_with('\n') {
                end += 1;
            }
            Ok(format!("{}{section}{}", &text[..begin], &text[end..]))
        }
        (None, None) => {
            let separator = match text {
                "" => "",
                t if t.ends_with("\n\n") => "",
                t if t.ends_with('\n') => "\n",
                _ => "\n\n",
            };
            Ok(format!("{text}{separator}{section}"))
        }
        _ => bail!("its atomic section has a missing or misplaced marker line; fix or remove it"),
    }
}

/// Write `relative` under `root`, creating directories, and refuse any path
/// that leaves `root` through `..` or a symbolic link.
fn write_inside(root: &Path, relative: &Path, contents: &[u8]) -> anyhow::Result<()> {
    ensure!(
        relative
            .components()
            .all(|c| matches!(c, Component::Normal(_))),
        "{} is not a plain relative path",
        relative.display()
    );
    let outside = || {
        anyhow::anyhow!(
            "{} leads outside {} through a symbolic link",
            relative.display(),
            root.display()
        )
    };
    // Each directory is checked before anything is created below it.
    let mut dir = root.to_owned();
    for part in relative.parent().into_iter().flat_map(Path::components) {
        dir.push(part);
        match std::fs::symlink_metadata(&dir) {
            Ok(_) => {
                let resolved = std::fs::canonicalize(&dir).map_err(|_| outside())?;
                if !resolved.starts_with(root) {
                    return Err(outside());
                }
                ensure!(resolved.is_dir(), "{} is not a directory", dir.display());
                dir = resolved;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
            }
            Err(e) => return Err(e).with_context(|| format!("inspect {}", dir.display())),
        }
    }
    let file = dir.join(relative.file_name().context("a file has a name")?);
    if let Ok(meta) = std::fs::symlink_metadata(&file)
        && meta.file_type().is_symlink()
    {
        let resolved = std::fs::canonicalize(&file)
            .with_context(|| format!("{} is a broken symbolic link", relative.display()))?;
        ensure!(
            resolved.starts_with(root),
            "{} is a symbolic link outside {}",
            relative.display(),
            root.display()
        );
    }
    std::fs::write(&file, contents).with_context(|| format!("write {}", relative.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "atomic-setup-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("scratch directory");
        dir
    }

    #[test]
    fn codex_gets_the_cross_tool_skills_directory_and_agents_md() {
        let root = scratch("codex");
        let written = install(Agent::Codex, &root).expect("installs");
        let skill = root.join(".agents/skills/atomic-create-atom");
        assert!(skill.join("SKILL.md").is_file());
        assert!(skill.join("references/fields.md").is_file());
        let agents = std::fs::read_to_string(root.join("AGENTS.md")).expect("AGENTS.md");
        assert!(agents.contains("`atomic-create-atom` skill"));
        assert!(written.contains(&PathBuf::from("AGENTS.md")));
        assert!(!root.join(".claude").exists() && !root.join("CLAUDE.md").exists());
    }

    #[test]
    fn claude_gets_its_own_skills_directory_and_claude_md() {
        let root = scratch("claude");
        install(Agent::Claude, &root).expect("installs");
        assert!(
            root.join(".claude/skills/atomic-create-atom/SKILL.md")
                .is_file()
        );
        assert!(root.join("CLAUDE.md").is_file());
        assert!(!root.join(".agents").exists() && !root.join("AGENTS.md").exists());
    }

    #[test]
    fn installed_files_are_the_pack() {
        let root = scratch("pack");
        install(Agent::Codex, &root).expect("installs");
        for file in skills::pack().expect("pack") {
            let installed = std::fs::read_to_string(root.join(".agents/skills").join(&file.path))
                .expect("installed file");
            assert_eq!(installed, file.contents, "{}", file.path);
        }
    }

    #[test]
    fn the_users_instructions_are_kept_and_the_section_is_replaced() {
        let root = scratch("merge");
        let mine = "# My project\n\nRun the tests before committing.\n";
        std::fs::write(root.join("AGENTS.md"), mine).expect("write");
        install(Agent::Codex, &root).expect("first");
        let once = std::fs::read_to_string(root.join("AGENTS.md")).expect("read");
        assert!(once.starts_with(mine), "{once}");
        assert_eq!(once.matches(BEGIN).count(), 1);
        // A stale section, even one an older atomic marked differently, is
        // replaced, and text after it is kept.
        let stale = once
            .replace("is the", "was the")
            .replace(BEGIN, "<!-- BEGIN atomic, an older version -->")
            + "\nMore of mine.\n";
        std::fs::write(root.join("AGENTS.md"), &stale).expect("write");
        let written = install(Agent::Codex, &root).expect("second");
        let twice = std::fs::read_to_string(root.join("AGENTS.md")).expect("read");
        assert_eq!(twice, once + "\nMore of mine.\n");
        assert!(written.contains(&PathBuf::from("AGENTS.md")));
        // Unchanged: the file is not rewritten.
        let third = install(Agent::Codex, &root).expect("third");
        assert!(!third.contains(&PathBuf::from("AGENTS.md")));
    }

    #[test]
    fn a_broken_marker_is_refused_not_duplicated() {
        let root = scratch("broken");
        std::fs::write(root.join("CLAUDE.md"), format!("{BEGIN}\nhalf a section\n"))
            .expect("write");
        let error = install(Agent::Claude, &root).expect_err("refused");
        assert!(format!("{error:#}").contains("marker"), "{error:#}");
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_links_out_of_the_repository_are_refused() {
        let root = scratch("link");
        let outside = scratch("outside");
        std::os::unix::fs::symlink(&outside, root.join(".agents")).expect("link");
        let error = install(Agent::Codex, &root).expect_err("refused");
        assert!(format!("{error:#}").contains("symbolic link"), "{error:#}");
        assert!(
            std::fs::read_dir(&outside)
                .expect("outside")
                .next()
                .is_none()
        );

        let root = scratch("link-file");
        std::fs::write(outside.join("notes.md"), "theirs").expect("write");
        std::os::unix::fs::symlink(outside.join("notes.md"), root.join("CLAUDE.md")).expect("link");
        let error = install(Agent::Claude, &root).expect_err("refused");
        assert!(format!("{error:#}").contains("symbolic link"), "{error:#}");
        assert_eq!(
            std::fs::read_to_string(outside.join("notes.md")).expect("read"),
            "theirs"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_claude_md_linked_to_agents_md_inside_the_repository_is_followed() {
        let root = scratch("inside-link");
        std::fs::write(root.join("AGENTS.md"), "# Shared\n").expect("write");
        std::os::unix::fs::symlink("AGENTS.md", root.join("CLAUDE.md")).expect("link");
        install(Agent::Claude, &root).expect("claude");
        install(Agent::Codex, &root).expect("codex");
        let text = std::fs::read_to_string(root.join("AGENTS.md")).expect("read");
        assert!(text.starts_with("# Shared\n"));
        assert_eq!(text.matches(BEGIN).count(), 1, "{text}");
        assert!(
            std::fs::symlink_metadata(root.join("CLAUDE.md"))
                .expect("meta")
                .file_type()
                .is_symlink()
        );
    }
}
