// SPDX-License-Identifier: Apache-2.0

//! `atomic setup <agent>`: install the authoring skills for a coding agent.
//!
//! The pack goes where the agent looks for project skills: Codex reads the
//! cross-tool `.agents/skills/`, Claude Code its own `.claude/skills/`. A short
//! section between marker lines in the agent's instructions file (`AGENTS.md`
//! or `CLAUDE.md`) points the agent at the skill.
//!
//! The skill's own directory belongs to atomic, and the agent reads whatever
//! it finds there, so setup first requires that directory to hold exactly the
//! pack's files, each with the pack's content, reached down plain directories,
//! and refuses anything else in it by name. Once it does, setup rewrites the
//! pack's files and the instructions section, and touches nothing else: skills
//! installed beside atomic's, and the rest of the instructions file, stay as
//! they were.

use crate::skills;
use anyhow::{Context, bail, ensure};
use std::collections::BTreeSet;
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
///
/// Nothing is written until every skill directory atomic owns holds only what
/// this binary's pack writes there and is reached without a link, so a refusal
/// installs nothing and points no agent at a directory holding instructions
/// atomic never wrote.
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
    // The agent reads everything in the skill's directory, so nothing outside
    // the pack may sit in it and nothing may lead into it through a link.
    let skills_dir = Path::new(agent.skills_dir());
    for dir in owned_dirs(&pack)? {
        OwnedDir::new(&root, skills_dir, &dir, &pack).check()?;
    }
    let mut written = Vec::new();
    for file in pack {
        let relative = skills_dir.join(&file.path);
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

/// The directories atomic owns below the agent's skills directory: one per
/// skill in the pack. Every pack path starts with its skill's directory, so
/// nothing atomic writes lives outside them.
fn owned_dirs(pack: &[skills::SkillFile]) -> anyhow::Result<Vec<String>> {
    let mut dirs = BTreeSet::new();
    for file in pack {
        let dir = file
            .path
            .split('/')
            .next()
            .expect("a path has at least one piece");
        ensure!(
            !dir.is_empty(),
            "the pack writes {}, loose in the skills directory, not inside a skill",
            file.path
        );
        dirs.insert(dir);
    }
    Ok(dirs.into_iter().map(str::to_owned).collect())
}

/// What a skill directory holds that the pack does not write there, as the
/// clauses of one refusal: a pack file holding other content first, then what
/// atomic does not write, then links.
#[derive(Default)]
struct Foreign {
    /// Pack paths holding something other than the pack's contents.
    changed: Vec<String>,
    /// Paths atomic does not write, directories included.
    extra: Vec<String>,
    /// Paths that are symbolic links, which are never followed.
    links: Vec<String>,
}

impl Foreign {
    fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.extra.is_empty() && self.links.is_empty()
    }

    /// The refusal's clauses, in order.
    fn clauses(&self) -> impl Iterator<Item = &str> {
        self.changed
            .iter()
            .chain(self.extra.iter())
            .chain(self.links.iter())
            .map(String::as_str)
    }
}

/// One skill's directory below the repository root, and what the pack writes
/// in it. The directory is atomic's to install into, so [`OwnedDir::check`]
/// requires it to hold nothing else and to be reached without a link.
struct OwnedDir<'a> {
    /// The repository being set up.
    root: &'a Path,
    /// The directory itself.
    path: PathBuf,
    /// Where it sits below `root`, which is how a problem in it is named.
    at: PathBuf,
    /// `{skill}/`, which every pack path of this skill starts with.
    prefix: String,
    /// The pack's paths below the skill directory, which are the files allowed
    /// there.
    files: BTreeSet<&'a str>,
    /// The directories those paths pass through, below the skill directory.
    dirs: BTreeSet<&'a str>,
    /// The pack, for the content each allowed file must hold.
    pack: &'a [skills::SkillFile],
}

impl<'a> OwnedDir<'a> {
    fn new(
        root: &'a Path,
        skills_dir: &Path,
        skill: &'a str,
        pack: &'a [skills::SkillFile],
    ) -> Self {
        let prefix = format!("{skill}/");
        let files: BTreeSet<&'a str> = pack
            .iter()
            .map(|f| f.path.as_str())
            .filter(|p| p.starts_with(prefix.as_str()))
            .map(|p| &p[prefix.len()..])
            .collect();
        let mut dirs: BTreeSet<&'a str> = BTreeSet::new();
        for path in files.iter().copied() {
            let mut rest = path;
            while let Some(cut) = rest.find('/') {
                dirs.insert(&path[..path.len() - rest.len() + cut]);
                rest = &rest[cut + 1..];
            }
        }
        let at = skills_dir.join(skill);
        Self {
            root,
            path: root.join(&at),
            at,
            prefix,
            files,
            dirs,
            pack,
        }
    }

    /// Refuse, naming each, whatever this directory holds that the pack does
    /// not write there, and whatever link stands between the repository root
    /// and it. An absent directory is what a first install meets.
    fn check(&self) -> anyhow::Result<()> {
        let mut foreign = Foreign::default();
        if let Some(link) = self.chain_link() {
            // Read through an alias, so this is not the directory atomic was
            // asked to install into, however plain what it leads to looks.
            foreign
                .links
                .push(format!("{} is a symbolic link", link.display()));
        } else if is_link(&self.path) {
            // The whole directory atomic owns is an alias for elsewhere.
            foreign
                .links
                .push(format!("{} is a symbolic link", self.shown("")));
        } else if self.path.exists() {
            self.inspect("", &mut foreign)?;
        }
        if foreign.is_empty() {
            return Ok(());
        }
        bail!(
            "{} holds what atomic did not write: {}; remove what is listed, or the whole \
             directory, and run `atomic setup` again",
            self.shown(""),
            foreign.clauses().collect::<Vec<_>>().join("; "),
        )
    }

    /// The first link standing between the repository root and this directory,
    /// named as it sits below the root, if any: one there points what follows
    /// at another place, so the directory is only inspected once the whole
    /// chain is plain directories.
    fn chain_link(&self) -> Option<PathBuf> {
        let mut dir = self.root.to_owned();
        let mut shown = PathBuf::new();
        for part in self.at.parent()?.components() {
            dir.push(part);
            shown.push(part.as_os_str());
            if is_link(&dir) {
                return Some(shown);
            }
        }
        None
    }

    /// How this directory, or a path below it, is named in a refusal: as it
    /// sits below the repository root.
    fn shown(&self, relative: &str) -> String {
        let path = if relative.is_empty() {
            self.at.clone()
        } else {
            self.at.join(relative)
        };
        match path.to_str() {
            Some(text) => text.to_owned(),
            None => self.at.display().to_string(),
        }
    }

    /// Add what the directory `at` below this one holds that the pack does not
    /// write, where `at` is empty for this directory itself. Entries are taken
    /// in name order, so a refusal does not depend on the order a filesystem
    /// happens to give. A directory is walked into only when the pack writes a
    /// file under it, so what atomic never writes is refused with the directory
    /// that holds it and nothing here descends past the pack's own nesting.
    fn inspect(&self, at: &str, foreign: &mut Foreign) -> anyhow::Result<()> {
        let shown_at = self.shown(at);
        let read = std::fs::read_dir(self.path.join(at))
            .with_context(|| format!("cannot inspect {shown_at}"))?;
        let mut names: Vec<std::ffi::OsString> = read
            .map(|entry| {
                entry.map(|entry| entry.file_name()).map_err(|e| {
                    anyhow::Error::new(e).context(format!("cannot inspect {shown_at}"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        names.sort();
        for name in names {
            let relative = if at.is_empty() {
                name.to_string_lossy().into_owned()
            } else {
                format!("{at}/{}", name.to_string_lossy())
            };
            let shown = self.shown(&relative);
            // What an entry is, not what it points at: a symbolic link is
            // named as one and never followed, read from or written through.
            let meta = std::fs::symlink_metadata(self.path.join(&relative))
                .with_context(|| format!("cannot inspect {shown}"))?;
            let kind = meta.file_type();
            if kind.is_symlink() {
                foreign.links.push(format!("{shown} is a symbolic link"));
                continue;
            }
            if kind.is_dir() {
                if !self.dirs.contains(relative.as_str()) {
                    foreign
                        .extra
                        .push(format!("{shown} is not a directory atomic writes"));
                    continue;
                }
                self.inspect(&relative, foreign)?;
                continue;
            }
            if !self.files.contains(relative.as_str()) {
                foreign
                    .extra
                    .push(format!("{shown} is not a file atomic writes"));
                continue;
            }
            // The pack has this path, so its own bytes belong here: a leftover
            // from another version, an edited file or anything not a plain
            // file at all is refused rather than taken in.
            let file = self
                .pack
                .iter()
                .find(|f| f.path == self.pack_path(&relative))
                .expect("the pack has its own path");
            // Sizes first, so a large leftover costs no reading.
            if meta.len() != file.contents.len() as u64 {
                foreign
                    .changed
                    .push(format!("{shown} does not hold what atomic writes there"));
                continue;
            }
            let on_disk = std::fs::read(self.path.join(&relative))
                .with_context(|| format!("cannot read {shown}"))?;
            if on_disk != file.contents.as_bytes() {
                foreign
                    .changed
                    .push(format!("{shown} does not hold what atomic writes there"));
            }
        }
        Ok(())
    }

    /// The pack path of a file at `relative` below this directory.
    fn pack_path(&self, relative: &str) -> String {
        format!("{}{relative}", self.prefix)
    }
}

/// Whether `path` is itself a symbolic link.
fn is_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
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

    /// Re-running setup is what an up-to-date install meets, and the check
    /// must not stand in its way: the same pack installs again.
    #[test]
    fn reinstalling_the_same_pack_needs_no_clean_up() {
        let root = scratch("again");
        let first = install(Agent::Codex, &root).expect("first");
        let second = install(Agent::Codex, &root).expect("second");
        assert!(first.contains(&PathBuf::from("AGENTS.md")), "{first:?}");
        assert!(
            !second.contains(&PathBuf::from("AGENTS.md")),
            "the instructions file was rewritten: {second:?}"
        );
        for file in skills::pack().expect("pack") {
            let written = Path::new(Agent::Codex.skills_dir()).join(&file.path);
            assert!(
                second.contains(&written),
                "{written:?} was not written again"
            );
        }
    }

    /// What an augmented directory costs: the refusal's exact wording, and no
    /// install at all.
    #[test]
    fn a_file_atomic_did_not_write_in_the_skill_directory_is_refused() {
        let root = scratch("extra");
        let extra = root.join(".agents/skills/atomic-create-atom/scripts/evil.sh");
        std::fs::create_dir_all(extra.parent().expect("a parent")).expect("scripts");
        std::fs::write(&extra, "#!/bin/sh\necho not atomic\n").expect("write");
        let error = install(Agent::Codex, &root).expect_err("refused");
        assert_eq!(
            format!("{error:#}"),
            ".agents/skills/atomic-create-atom holds what atomic did not write: \
             .agents/skills/atomic-create-atom/scripts is not a directory atomic writes; \
             remove what is listed, or the whole directory, and run `atomic setup` again"
        );
        assert!(
            !root
                .join(".agents/skills/atomic-create-atom/SKILL.md")
                .exists(),
            "the skill was installed despite the refusal"
        );
        assert!(
            !root.join("AGENTS.md").exists(),
            "the agent was told to use it anyway"
        );
        assert_eq!(
            std::fs::read_to_string(&extra).expect("theirs"),
            "#!/bin/sh\necho not atomic\n"
        );
    }

    /// The extra thing can be a plain file beside the pack's own, which is
    /// what a leftover from an older atomic or a hand-written note looks like.
    #[test]
    fn a_plain_extra_file_in_the_skill_directory_is_refused() {
        let root = scratch("extra-file");
        std::fs::create_dir_all(root.join(".claude/skills/atomic-create-atom"))
            .expect("skill directory");
        std::fs::write(
            root.join(".claude/skills/atomic-create-atom/extra.md"),
            "mine",
        )
        .expect("write");
        let error = install(Agent::Claude, &root).expect_err("refused");
        assert_eq!(
            format!("{error:#}"),
            ".claude/skills/atomic-create-atom holds what atomic did not write: \
             .claude/skills/atomic-create-atom/extra.md is not a file atomic writes; \
             remove what is listed, or the whole directory, and run `atomic setup` again"
        );
        assert!(!root.join("CLAUDE.md").exists());
    }

    /// A hidden file is part of the directory too, so it is named.
    #[test]
    fn a_hidden_file_in_the_skill_directory_is_refused() {
        let root = scratch("hidden");
        std::fs::create_dir_all(root.join(".agents/skills/atomic-create-atom/references"))
            .expect("references");
        std::fs::write(
            root.join(".agents/skills/atomic-create-atom/.alias"),
            "read me first",
        )
        .expect("write");
        let error = install(Agent::Codex, &root).expect_err("refused");
        assert_eq!(
            format!("{error:#}"),
            ".agents/skills/atomic-create-atom holds what atomic did not write: \
             .agents/skills/atomic-create-atom/.alias is not a file atomic writes; \
             remove what is listed, or the whole directory, and run `atomic setup` again"
        );
        assert!(!root.join("AGENTS.md").exists());
    }

    /// What a version of atomic that wrote other content, or an edited
    /// installed file, leaves behind is refused rather than taken in.
    #[test]
    fn a_pack_file_holding_other_content_is_refused() {
        let root = scratch("changed");
        std::fs::create_dir_all(root.join(".agents/skills/atomic-create-atom/references"))
            .expect("references");
        let skill = root.join(".agents/skills/atomic-create-atom/SKILL.md");
        std::fs::write(&skill, "---\nname: atomic-create-atom\n---\nmine\n").expect("write");
        let error = install(Agent::Codex, &root).expect_err("refused");
        assert_eq!(
            format!("{error:#}"),
            ".agents/skills/atomic-create-atom holds what atomic did not write: \
             .agents/skills/atomic-create-atom/SKILL.md does not hold what atomic writes there; \
             remove what is listed, or the whole directory, and run `atomic setup` again"
        );
        assert_eq!(
            std::fs::read_to_string(&skill).expect("unchanged"),
            "---\nname: atomic-create-atom\n---\nmine\n"
        );
        assert!(!root.join("AGENTS.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_inside_the_skill_directory_is_refused_not_followed() {
        let root = scratch("link-inside");
        let outside = scratch("link-target");
        std::fs::create_dir_all(root.join(".agents/skills/atomic-create-atom/references"))
            .expect("references");
        std::fs::write(outside.join("someone-elses.md"), "theirs").expect("write");
        std::os::unix::fs::symlink(
            outside.join("someone-elses.md"),
            root.join(".agents/skills/atomic-create-atom/references/fields.md"),
        )
        .expect("link");
        let error = install(Agent::Codex, &root).expect_err("refused");
        assert_eq!(
            format!("{error:#}"),
            ".agents/skills/atomic-create-atom holds what atomic did not write: \
             .agents/skills/atomic-create-atom/references/fields.md is a symbolic link; \
             remove what is listed, or the whole directory, and run `atomic setup` again"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("someone-elses.md")).expect("read"),
            "theirs"
        );
        assert!(!root.join("AGENTS.md").exists());
    }

    /// The directory behind the alias looks like a skill of its own, right
    /// down to the `name` its directory goes by, and stays exactly as it was.
    #[cfg(unix)]
    #[test]
    fn a_skill_directory_that_is_a_link_is_refused() {
        let root = scratch("dir-link");
        let other = scratch("elsewhere");
        let planted = "---\nname: atomic-create-atom\n---\nnot atomic's own\n";
        std::fs::create_dir_all(root.join(".agents/skills")).expect("skills");
        std::fs::create_dir_all(other.join("references")).expect("references");
        std::fs::write(other.join("SKILL.md"), planted).expect("write");
        std::os::unix::fs::symlink(&other, root.join(".agents/skills/atomic-create-atom"))
            .expect("link");
        let error = install(Agent::Codex, &root).expect_err("refused");
        assert_eq!(
            format!("{error:#}"),
            ".agents/skills/atomic-create-atom holds what atomic did not write: \
             .agents/skills/atomic-create-atom is a symbolic link; \
             remove what is listed, or the whole directory, and run `atomic setup` again"
        );
        assert_eq!(
            std::fs::read_to_string(other.join("SKILL.md")).expect("theirs"),
            planted
        );
        assert_eq!(
            std::fs::read_dir(&other).expect("theirs").count(),
            2,
            "something new appeared behind the link"
        );
        assert!(!root.join("AGENTS.md").exists());
    }

    /// A pack path that is a link to a device is refused as a link before
    /// anything is read from it.
    #[cfg(unix)]
    #[test]
    fn a_pack_path_linked_to_a_device_is_refused() {
        let root = scratch("device-link");
        std::fs::create_dir_all(root.join(".agents/skills/atomic-create-atom")).expect("skill");
        let path = root.join(".agents/skills/atomic-create-atom/SKILL.md");
        std::os::unix::fs::symlink("/dev/null", &path).expect("link");
        let error = install(Agent::Codex, &root).expect_err("refused");
        assert_eq!(
            format!("{error:#}"),
            ".agents/skills/atomic-create-atom holds what atomic did not write: \
             .agents/skills/atomic-create-atom/SKILL.md is a symbolic link; \
             remove what is listed, or the whole directory, and run `atomic setup` again"
        );
        assert!(!root.join("AGENTS.md").exists());
    }

    /// Refusing is about the directory atomic owns. A skill installed beside
    /// it stays, and so does anything else in the shared skills directory.
    #[test]
    fn a_skill_installed_beside_atomic_is_left_alone() {
        let root = scratch("neighbour");
        let neighbour = root.join(".agents/skills/someone-elses-skill");
        std::fs::create_dir_all(neighbour.join("scripts")).expect("neighbour");
        std::fs::write(
            neighbour.join("SKILL.md"),
            "---\nname: someone-elses-skill\n---\n",
        )
        .expect("write");
        std::fs::write(neighbour.join("scripts/run.sh"), "#!/bin/sh\n").expect("write");
        let instructions = "# My project\n";
        std::fs::write(root.join("AGENTS.md"), instructions).expect("write");
        install(Agent::Codex, &root).expect("installs");
        assert_eq!(
            std::fs::read_to_string(neighbour.join("scripts/run.sh")).expect("theirs"),
            "#!/bin/sh\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("AGENTS.md")).expect("AGENTS.md"),
            format!("{instructions}\n{}", section())
        );
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

    /// A skill directory is installed only down a chain of plain directories.
    /// A link partway down leads the agent, and atomic's own writes, somewhere
    /// else, so it is refused where it is, and what it leads to is left out.
    #[cfg(unix)]
    #[test]
    fn a_skill_directory_reached_through_a_link_is_refused() {
        let root = scratch("chain");
        let shared = root.join("shared/.claude/skills/atomic-create-atom");
        std::fs::create_dir_all(&shared).expect("shared");
        std::fs::write(shared.join("extra.md"), "mine").expect("write");
        std::os::unix::fs::symlink("shared/.claude", root.join(".claude")).expect("link");
        let error = install(Agent::Claude, &root).expect_err("refused");
        assert_eq!(
            format!("{error:#}"),
            ".claude/skills/atomic-create-atom holds what atomic did not write: \
             .claude is a symbolic link; remove what is listed, or the whole directory, \
             and run `atomic setup` again"
        );
        assert!(!root.join("CLAUDE.md").exists());
        assert_eq!(
            std::fs::read_to_string(shared.join("extra.md")).expect("theirs"),
            "mine"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_links_out_of_the_repository_are_refused() {
        let root = scratch("link");
        let outside = scratch("outside");
        std::os::unix::fs::symlink(&outside, root.join(".agents")).expect("link");
        let error = install(Agent::Codex, &root).expect_err("refused");
        assert_eq!(
            format!("{error:#}"),
            ".agents/skills/atomic-create-atom holds what atomic did not write: \
             .agents is a symbolic link; remove what is listed, or the whole directory, \
             and run `atomic setup` again"
        );
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
