// SPDX-License-Identifier: Apache-2.0

//! `atomic setup <agent>`: install the authoring skills for a coding agent.
//!
//! The pack goes where the agent looks for project skills: Codex reads the
//! cross-tool `.agents/skills/`, Claude Code its own `.claude/skills/`. A short
//! section between marker lines in the agent's instructions file (`AGENTS.md`
//! or `CLAUDE.md`) points the agent at the skill.
//!
//! The skill's own directory belongs to atomic, and the agent reads whatever it
//! finds there, so setup first requires that directory to hold only what atomic
//! wrote there, each file still holding the bytes atomic wrote, reached down
//! plain directories. What atomic wrote is what its last run recorded in the
//! directory's own manifest, `.atomic-pack.json`. A directory written by a
//! setup that wrote no manifest counts too, but only while it holds exactly
//! this pack's files with this pack's bytes, since nothing can have been added
//! to it or edited in it without showing. Setup refuses anything else, naming
//! what it refuses, or the first of them and how many more there were when one
//! message cannot hold them all. Once the directory passes, setup rewrites the
//! pack's files and the instructions section, records what it wrote in the
//! manifest, and touches nothing else: skills installed beside atomic's, and the
//! rest of the instructions file, stay as they were.

use crate::shown::line;
use crate::skills;
use anyhow::{Context, bail, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
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

/// The record setup leaves in each skill directory it owns: every file it wrote
/// there, and the sha256 of what it wrote. It is how a later setup tells its own
/// files from what someone else put there, so a directory an older atomic wrote
/// can be upgraded, and one someone added to cannot.
const MANIFEST: &str = ".atomic-pack.json";
/// The manifest format this atomic reads and writes.
const MANIFEST_FORMAT: u64 = 1;
/// How big a manifest may be. One is a line per file of the pack, so anything
/// much larger is not a manifest atomic wrote, and reading it would be the
/// point.
const MANIFEST_BYTES_MAX: u64 = 16 * 1024;
/// How much of a refusal's list of names is written before the rest is counted
/// instead. The remedy that follows is never cut for it.
const NAMED_BYTES_MAX: usize = 2048;

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
/// atomic wrote there and is reached without a link, so a refusal installs
/// nothing and points no agent at a directory holding instructions atomic never
/// wrote. What passes may be rewritten; the only thing removed is a file the
/// manifest says atomic wrote, still holding what it recorded, that this pack
/// no longer ships. The manifest goes down after the files it records.
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
    // The agent reads everything in the skill's directory, so nothing atomic
    // did not write may sit in it and nothing may lead into it through a link.
    let skills_dir = Path::new(agent.skills_dir());
    // Checked first, all of them, before anything is written or removed: a
    // refusal leaves every directory as it was found. Each check comes back
    // with the files its manifest says are atomic's, still unmodified, that
    // this pack no longer ships.
    let skills = owned_dirs(&pack)?;
    let mut owned = Vec::new();
    for skill in &skills {
        let dir = OwnedDir::new(&root, skills_dir, skill, &pack);
        let stale = dir.check()?;
        owned.push((dir, stale));
    }
    let instructions = Path::new(agent.instructions_file());
    // The instructions file is the one path atomic writes that no skill
    // directory's check covers, so its own link is refused here: before anything
    // is written, and before it is read, since a link is not something atomic
    // reads through either. A link atomic may not write through leaves the skill
    // not installed at all, rather than installed and unreferenced. The only
    // other file a link standing there may lead to is the other agent's
    // instructions file, which is how one file serves both agents. A link that
    // leads into a skill would write the instructions over one of its files, so
    // it is refused like any other.
    let mut instructions_files: Vec<PathBuf> = Vec::new();
    for other in [Agent::Claude, Agent::Codex] {
        instructions_files.push(PathBuf::from(other.instructions_file()));
    }
    link_to_own_file(
        &root,
        instructions,
        &root.join(instructions),
        &instructions_files,
    )?;
    // What the instructions file is to hold, read and put together before
    // anything is written: an instructions file atomic cannot read, or one whose
    // atomic section is broken, is refused here, so a refusal leaves the skill
    // not partly installed for an agent to find.
    let current = match std::fs::read_to_string(root.join(instructions)) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("read {}", instructions.display())),
    };
    let updated = with_section(current.as_deref(), &section())
        .with_context(|| format!("update {}", instructions.display()))?;
    for (dir, stale) in &owned {
        for gone in stale {
            std::fs::remove_file(dir.path.join(gone))
                .with_context(|| format!("remove {}", dir.shown(gone)))?;
        }
    }
    let mut written = Vec::new();
    for file in &pack {
        let relative = skills_dir.join(&file.path);
        write_inside(&root, &relative, file.contents.as_bytes())?;
        written.push(relative);
    }
    // After the files, so a setup that stopped halfway leaves the manifest of
    // the run before it, which then refuses the directory rather than vouching
    // for bytes it never saw.
    for (dir, _) in &owned {
        let relative = skills_dir.join(dir.skill).join(MANIFEST);
        write_inside(&root, &relative, dir.manifest().as_bytes())?;
        written.push(relative);
    }
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
            line(&file.path)
        );
        dirs.insert(dir);
    }
    Ok(dirs.into_iter().map(str::to_owned).collect())
}

/// What one skill directory holds that atomic may not write over, as the
/// clauses of one refusal, in this order: a file the manifest says atomic wrote
/// that no longer holds those bytes, then a pack path holding other content,
/// then what atomic does not write at all, then links.
#[derive(Default)]
struct Report {
    /// Listed paths no longer holding the bytes the manifest records, which is
    /// what it looks like when a person edits a file atomic wrote.
    edited: Vec<String>,
    /// Pack paths holding something other than the pack's contents, or
    /// something that is not a plain file. Only a directory with no manifest
    /// leaves a pack path in this clause; with one, the manifest says who wrote
    /// it, and the `edited` clause or the `extra` one takes it.
    changed: Vec<String>,
    /// Paths atomic does not write, directories included.
    extra: Vec<String>,
    /// Paths that are symbolic links, which are never followed.
    links: Vec<String>,
    /// Listed paths found holding the bytes they are recorded with. These are
    /// atomic's to write over, and to remove when this pack no longer ships
    /// them.
    unmodified: BTreeSet<String>,
}

impl Report {
    fn is_empty(&self) -> bool {
        self.edited.is_empty()
            && self.changed.is_empty()
            && self.extra.is_empty()
            && self.links.is_empty()
    }

    /// The refusal's clauses, in order.
    fn clauses(&self) -> impl Iterator<Item = &str> {
        self.edited
            .iter()
            .chain(self.changed.iter())
            .chain(self.extra.iter())
            .chain(self.links.iter())
            .map(String::as_str)
    }
}

/// What setup wrote in one skill directory: every file it made there, by its
/// path below that directory, with the sha256 of the bytes it wrote. A later
/// setup reads it before it writes, so that it may replace its own work and
/// refuse what anyone else left there.
#[derive(serde::Deserialize, serde::Serialize)]
struct Manifest {
    /// The manifest's format version. A file written to a format this atomic
    /// does not know is refused, not guessed at.
    format: u64,
    /// The files atomic wrote, by path below the skill directory, each with the
    /// hex sha256 of what it wrote there.
    files: BTreeMap<String, String>,
}

impl Manifest {
    /// A manifest for what `pack` writes inside one skill.
    fn for_pack(pack: &[skills::SkillFile], skill: &str) -> Self {
        let prefix = format!("{skill}/");
        Self {
            format: MANIFEST_FORMAT,
            files: pack
                .iter()
                .filter(|file| file.path.starts_with(prefix.as_str()))
                .map(|file| (file.path[prefix.len()..].to_owned(), sha256(&file.contents)))
                .collect(),
        }
    }

    /// The bytes to write the manifest as.
    fn text(&self) -> String {
        // From a structure this crate owns, so serializing it cannot fail.
        let body = serde_json::to_string_pretty(self).expect("a manifest is JSON");
        format!("{body}\n")
    }

    /// The manifest in `text`, or the reason it is not one atomic wrote. Every
    /// part of it is checked, because a path listed here is one atomic may
    /// write to or remove, and a hash listed here is what a file is trusted
    /// against.
    fn read(at: &str, text: &str) -> anyhow::Result<Self> {
        let here = line(at);
        let read: Self = serde_json::from_str(text).map_err(|e| {
            anyhow::anyhow!(
                "{here} is not a manifest atomic can read: {}",
                line(&e.to_string())
            )
        })?;
        ensure!(
            read.format == MANIFEST_FORMAT,
            "{here} is manifest format {}, which this atomic cannot read; it was written by \
             a different version of atomic",
            read.format
        );
        for (path, hash) in &read.files {
            ensure!(
                is_plain_relative(path)
                    && hash.len() == 64
                    && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                "{here} lists {path} as {hash}, which is not how atomic records a file"
            );
        }
        Ok(read)
    }

    /// Whether the file at `relative` holds the bytes this manifest recorded
    /// for it. A path that is not listed proves nothing about its contents, so
    /// it is never `unmodified` in this structure's sense.
    fn holds(&self, relative: &str, bytes: &[u8]) -> bool {
        self.files
            .get(relative)
            .is_some_and(|recorded| *recorded == sha256(bytes))
    }
}

/// The hex sha256 of `bytes`, which is how a manifest records a file.
fn sha256(bytes: impl AsRef<[u8]>) -> String {
    use sha2::Digest as _;
    let mut hash = sha2::Sha256::new();
    hash.update(bytes.as_ref());
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether `manifest` is a manifest atomic wrote that records exactly these
/// bytes at `relative`. This is the whole of what makes a listed file atomic's to
/// write over: a manifest that vouches for other bytes was written before someone
/// edited the file, and their work is not atomic's to throw away. It is one call
/// so that the control of it has one site, which
/// `scripts/controls/skill-dir-manifest-hash.row` switches off.
fn manifest_vouches_for(manifest: Option<&Manifest>, relative: &str, bytes: &[u8]) -> bool {
    manifest.is_some_and(|m| m.holds(relative, bytes))
}

/// Whether `path` is a plain relative path of the kind atomic writes below a
/// skill directory: `/`-separated, with no leading `/` and no piece that is
/// empty, `.` or `..`. A manifest listing anything else names somewhere atomic
/// would write outside the directory it records, so it is refused.
fn is_plain_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && path
            .split('/')
            .all(|piece| !piece.is_empty() && piece != "." && piece != "..")
}

/// One skill's directory below the repository root, and what the pack writes in
/// it. The directory is atomic's to install into, so [`OwnedDir::check`]
/// requires it to hold only what atomic wrote there, as its manifest says, and
/// to be reached without a link.
struct OwnedDir<'a> {
    /// The repository being set up.
    root: &'a Path,
    /// The directory itself.
    path: PathBuf,
    /// Where it sits below `root`, which is how a problem in it is named.
    at: PathBuf,
    /// The skill's name, which is this directory's own last component.
    skill: &'a str,
    /// `{skill}/`, which every pack path of this skill starts with.
    prefix: String,
    /// The pack's paths below the skill directory, which are the files this
    /// version of atomic writes there.
    files: BTreeSet<&'a str>,
    /// The directories those paths pass through, below the skill directory.
    dirs: BTreeSet<&'a str>,
    /// The pack, for the content each allowed file must hold where no manifest
    /// says who wrote it.
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
            skill,
            prefix,
            files,
            dirs,
            pack,
        }
    }

    /// Refuse, naming each of them, whatever this directory holds that atomic
    /// did not write, and whatever link stands between the repository root and
    /// it. An absent directory is what a first install meets.
    ///
    /// What the directory's manifest lists is what atomic wrote there. A listed
    /// file still holding the bytes it records may be written over, and comes
    /// back from here for removal when this pack no longer ships it; a listed
    /// file holding other bytes was edited since and is refused. With no
    /// manifest to go by, only a directory holding exactly this pack's files,
    /// with this pack's bytes, passes.
    fn check(&self) -> anyhow::Result<Vec<String>> {
        let mut report = Report::default();
        if let Some(link) = self.chain_link() {
            // Read through an alias, so this is not the directory atomic was
            // asked to install into, however plain what it leads to looks.
            report.links.push(link_named(&link));
        } else if is_link(&self.path) {
            // The whole directory atomic owns is an alias for elsewhere.
            report.links.push(link_named(&self.shown("")));
        } else if self.path.exists() {
            let manifest = self.read_manifest()?;
            self.inspect("", manifest.as_ref(), &mut report)?;
            if report.is_empty() {
                return Ok(self.leftovers(&manifest, &report));
            }
            bail!("{}", self.refusal(&report));
        }
        ensure!(report.is_empty(), "{}", self.refusal(&report));
        Ok(Vec::new())
    }

    /// The manifest this directory carries, if it has one: what the last setup
    /// wrote here. A file that is not a manifest atomic wrote is refused here,
    /// rather than read as no manifest at all, because that would let whatever
    /// is in the directory through on the strength of a file someone else left.
    fn read_manifest(&self) -> anyhow::Result<Option<Manifest>> {
        let shown = self.shown(MANIFEST);
        let at = self.path.join(MANIFEST);
        // Plain file or nothing, before it is opened: what is asked of this
        // path is one small read, and a fifo or a device would wait or stream.
        let meta = match std::fs::symlink_metadata(&at) {
            Ok(meta) if meta.is_file() => meta,
            Ok(_) => {
                bail!("{shown} is not a plain file, and atomic writes a plain file there")
            }
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("cannot inspect {shown}")),
        };
        if meta.len() > MANIFEST_BYTES_MAX {
            bail!(
                "{shown} is {}, which is larger than any manifest atomic writes; replace it \
                 and run `atomic setup` again",
                meta.len()
            );
        }
        let text = std::fs::read(&at)
            .with_context(|| format!("cannot read {shown}"))
            .and_then(|bytes| {
                String::from_utf8(bytes)
                    .map_err(|_| anyhow::anyhow!("{shown} is not a manifest atomic can read"))
            })?;
        Ok(Some(Manifest::read(&shown, &text)?))
    }

    /// What the manifest lists that this pack no longer writes, each one
    /// confirmed by the walk to still hold the bytes it records. Only these may
    /// be removed, and only these are: a file atomic did not write, or one
    /// someone changed, stays where it is, and it was refused above anyway.
    fn leftovers(&self, manifest: &Option<Manifest>, report: &Report) -> Vec<String> {
        let Some(manifest) = manifest else {
            return Vec::new();
        };
        manifest
            .files
            .keys()
            .filter(|path| !self.files.contains(path.as_str()))
            .filter(|path| report.unmodified.contains(path.as_str()))
            .cloned()
            .collect()
    }

    /// The refusal for what the walk found: the directory, what is wrong in it,
    /// and what to do about it. When there is too much to name, the list is cut
    /// and counted; the remedy is never cut, and no name is cut in half.
    fn refusal(&self, report: &Report) -> String {
        let clauses: Vec<&str> = report.clauses().collect();
        // At least one clause is always named, however long it is: a name cut
        // in half is worse advice than a long line, and the line is bounded
        // where atomic writes it.
        let mut named = String::new();
        let mut cut = 0;
        for clause in &clauses {
            if !named.is_empty() && named.len() + 2 + clause.len() > NAMED_BYTES_MAX {
                break;
            }
            if !named.is_empty() {
                named.push_str("; ");
            }
            named.push_str(clause);
            cut += 1;
        }
        if cut < clauses.len() {
            named.push_str(&format!("; and {} more", clauses.len() - cut));
        }
        let older = if report.links.len() == clauses.len() {
            // Nothing but links: atomic wrote none of them, and no version of
            // atomic writes a link, so that is not what is being suggested.
            ""
        } else {
            " (an older version of atomic may be the cause)"
        };
        format!(
            "{} holds what atomic did not write{older}: {named}; remove what is named and run \
             `atomic setup` again",
            self.shown(""),
        )
    }

    /// The first link standing between the repository root and this directory,
    /// named as it sits below the root, if any: one there points what follows
    /// at another place, so the directory is only inspected once the whole
    /// chain is plain directories.
    fn chain_link(&self) -> Option<String> {
        let mut dir = self.root.to_owned();
        let mut shown = String::new();
        for part in self.at.parent()?.components() {
            dir.push(part);
            if !shown.is_empty() {
                shown.push(std::path::MAIN_SEPARATOR);
            }
            shown.push_str(&part.as_os_str().to_string_lossy());
            if is_link(&dir) {
                return Some(line(&shown));
            }
        }
        None
    }

    /// How this directory, or a path below it, is named in a refusal: as it
    /// sits below the repository root, and safe for a terminal. Only what is
    /// below it is escaped, so the part atomic itself chose stays as it is.
    fn shown(&self, relative: &str) -> String {
        let at = self.at.display().to_string();
        if relative.is_empty() {
            at
        } else {
            format!("{at}/{}", line(relative))
        }
    }

    /// Add to `report` what the directory `at` below this one holds that atomic
    /// may not write over, where `at` is empty for this directory itself, and
    /// record there what `manifest` vouches for. With no manifest, the only
    /// content atomic accepts at one of its paths is this pack's own.
    ///
    /// Entries are taken in name order, so a refusal does not depend on the
    /// order a filesystem happens to give. A directory is walked into only when
    /// the pack writes a file under it or the manifest lists one there, so what
    /// atomic never wrote is refused with the directory holding it and nothing
    /// here descends past what atomic itself nested.
    fn inspect(
        &self,
        at: &str,
        manifest: Option<&Manifest>,
        report: &mut Report,
    ) -> anyhow::Result<()> {
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
                report.links.push(link_named(&shown));
                continue;
            }
            // The manifest, which atomic writes last and reads first, is as
            // much atomic's own as any file it lists.
            if relative == MANIFEST {
                continue;
            }
            let listed = manifest.is_some_and(|m| m.files.contains_key(relative.as_str()));
            if kind.is_dir() {
                let holds_one = manifest.is_some_and(|m| {
                    let under = format!("{relative}/");
                    m.files.keys().any(|p| p.starts_with(under.as_str()))
                });
                if !holds_one && !self.dirs.contains(relative.as_str()) {
                    report
                        .extra
                        .push(format!("{shown} is not a directory atomic writes"));
                    continue;
                }
                self.inspect(&relative, manifest, report)?;
                continue;
            }
            if !listed && !self.files.contains(relative.as_str()) {
                report
                    .extra
                    .push(format!("{shown} is not a file atomic writes"));
                continue;
            }
            // Past this point the path is atomic's to write, so only a plain
            // file may stand at it: a device, a fifo or a socket is refused,
            // not read from.
            if !meta.is_file() {
                report.changed.push(format!(
                    "{shown} is not a plain file, and atomic writes a plain file there"
                ));
                continue;
            }
            if listed {
                // The manifest says atomic wrote these bytes here. If they are
                // still here, the file is atomic's to replace; if not, someone
                // edited it, and their work is not atomic's to throw away.
                let bytes = self.bytes(&relative, &shown)?;
                let holds = manifest_vouches_for(manifest, &relative, &bytes);
                if holds {
                    report.unmodified.insert(relative);
                } else {
                    report
                        .edited
                        .push(format!("{shown} no longer holds what atomic wrote there"));
                }
                continue;
            }
            // No manifest vouches for this path, so nothing but what this very
            // pack would write there counts as atomic's: a leftover from
            // another version, or an edit, is refused instead of taken in.
            let file = self
                .pack
                .iter()
                .find(|f| f.path == self.pack_path(&relative))
                .expect("the pack has its own path");
            // Sizes first, so a large leftover costs no reading.
            let other = meta.len() != file.contents.len() as u64
                || self.bytes(&relative, &shown)? != file.contents.as_bytes();
            if other {
                report.changed.push(format!(
                    "{shown} holds other content than the pack's, which atomic wrote there and \
                     can replace"
                ));
            }
        }
        Ok(())
    }

    /// The bytes of the plain file at `relative`, which the walk has seen to be
    /// one.
    fn bytes(&self, relative: &str, shown: &str) -> anyhow::Result<Vec<u8>> {
        std::fs::read(self.path.join(relative)).with_context(|| format!("cannot read {shown}"))
    }

    /// The pack path of a file at `relative` below this directory.
    fn pack_path(&self, relative: &str) -> String {
        format!("{}{relative}", self.prefix)
    }

    /// The manifest to write here now: every file of the pack that belongs to
    /// this skill, with the bytes this atomic writes for it.
    fn manifest(&self) -> String {
        Manifest::for_pack(self.pack, self.skill).text()
    }
}

/// How a link is named in a refusal: where it is, what it is, and that writing
/// through it is not something atomic does. The link is what to remove, not the
/// directory it stands in.
fn link_named(at: &str) -> String {
    format!("{at} is a symbolic link, and atomic never writes through one")
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

/// Write `relative` under `root`, creating directories, and refuse any path that
/// leaves `root` through `..` or a symbolic link.
///
/// The caller has refused what stands at the path itself: see
/// [`link_to_own_file`], which `install` asks of the one path below the
/// repository root that no skill directory's check covers, before it writes
/// anything.
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
    std::fs::write(&file, contents).with_context(|| format!("write {}", relative.display()))
}

/// Refuse to write `file`, which stands at `relative` below `root`, when it is a
/// symbolic link atomic may not write through: one that is broken, leads out of
/// the repository, or leads to something other than another of the files named
/// in `may_lead_to`. A link inside the repository is not enough on its own: what
/// it leads to may be the person's own work, or one of a skill's files, which
/// hold the pack rather than the instructions. Where two agents share one
/// instructions file the link leads to the other's, and that one is followed.
fn link_to_own_file(
    root: &Path,
    relative: &Path,
    file: &Path,
    may_lead_to: &[PathBuf],
) -> anyhow::Result<()> {
    if !is_link(file) {
        return Ok(());
    }
    let resolved = std::fs::canonicalize(file)
        .with_context(|| format!("{} is a broken symbolic link", relative.display()))?;
    ensure!(
        resolved.starts_with(root),
        "{} is a symbolic link outside {}",
        relative.display(),
        root.display()
    );
    // The path being written is not its own target; only another of the files
    // named is, and that is read plain rather than through a link of its own,
    // which would be the same question one step on.
    let leads_to = |path: &Path| {
        path != relative
            && std::fs::canonicalize(root.join(path)).is_ok_and(|target| target == resolved)
    };
    ensure!(
        may_lead_to.iter().any(|path| leads_to(path)),
        "{} is a symbolic link to {}, which is not a file atomic may write through from here; \
         atomic writes this path plain, or through a link to the other agent's instructions file",
        relative.display(),
        resolved.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one skill of the pack, which every test here names.
    const SKILL: &str = skills::CREATE_ATOM;

    fn scratch(name: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "atomic-setup-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch directory");
        dir
    }

    /// Where `agent` installs the skill, below `root`.
    fn skill_dir(root: &Path, agent: Agent) -> PathBuf {
        root.join(agent.skills_dir()).join(SKILL)
    }

    /// The manifest an installed skill directory carries.
    fn manifest_of(root: &Path, agent: Agent) -> Manifest {
        let at = skill_dir(root, agent).join(MANIFEST);
        let text = std::fs::read_to_string(&at).expect("the manifest setup wrote");
        Manifest::read(&at.display().to_string(), &text).expect("a manifest atomic wrote")
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

    /// What the manifest is for: a later setup reads it to tell its own files
    /// from anyone else's, so it must list exactly the files this setup wrote,
    /// with the bytes it wrote, and nothing else.
    #[test]
    fn setup_records_in_the_skill_directory_what_it_wrote_there() {
        let root = scratch("manifest");
        let written = install(Agent::Codex, &root).expect("installs");
        let at = Path::new(Agent::Codex.skills_dir())
            .join(SKILL)
            .join(MANIFEST);
        assert!(written.contains(&at), "{written:?}");
        let manifest = manifest_of(&root, Agent::Codex);
        assert_eq!(manifest.format, MANIFEST_FORMAT);
        let under = format!("{SKILL}/");
        let want: BTreeMap<String, String> = skills::pack()
            .expect("pack")
            .iter()
            .filter(|file| file.path.starts_with(under.as_str()))
            .map(|file| (file.path[under.len()..].to_owned(), sha256(&file.contents)))
            .collect();
        assert_eq!(manifest.files, want);
        // Every file it lists really does hold what it says, which is what lets
        // a later setup write over it.
        for (path, recorded) in &manifest.files {
            let bytes = std::fs::read(skill_dir(&root, Agent::Codex).join(path)).expect("file");
            assert_eq!(sha256(&bytes), *recorded, "{path}");
        }
        // And it is the same bytes again next time, so re-running setup leaves a
        // directory that reads as it did.
        let once = std::fs::read_to_string(root.join(&at)).expect("read");
        install(Agent::Codex, &root).expect("again");
        assert_eq!(std::fs::read_to_string(root.join(&at)).expect("read"), once);
    }

    /// Re-running setup is what an up-to-date install meets, and the check must
    /// not stand in its way: the same pack installs again.
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

    /// What an upgrade meets: a directory an earlier setup wrote whose pack this
    /// one no longer matches. The manifest says those bytes are atomic's own and
    /// still unmodified, so setup writes its pack over them and drops what it no
    /// longer ships. That is what the record is for.
    #[test]
    fn an_upgrade_replaces_the_pack_an_earlier_setup_wrote() {
        let root = scratch("upgrade");
        install(Agent::Codex, &root).expect("installed");
        let skill = skill_dir(&root, Agent::Codex);
        // The state a release before this one leaves: its own bytes, and a
        // manifest recording exactly those.
        let mut manifest = manifest_of(&root, Agent::Codex);
        let older_skill = "---\nname: atomic-create-atom\n---\nan older release\n";
        let gone = "<!-- a reference this release no longer ships -->\n";
        std::fs::write(skill.join("SKILL.md"), older_skill).expect("write");
        std::fs::write(skill.join("references/gone.md"), gone).expect("write");
        manifest
            .files
            .insert("SKILL.md".to_owned(), sha256(older_skill));
        manifest
            .files
            .insert("references/gone.md".to_owned(), sha256(gone));
        std::fs::write(skill.join(MANIFEST), manifest.text()).expect("write");
        install(Agent::Codex, &root).expect("the upgrade installs");
        for file in skills::pack().expect("pack") {
            let installed = std::fs::read_to_string(root.join(".agents/skills").join(&file.path))
                .expect("installed file");
            assert_eq!(installed, file.contents, "{}", file.path);
        }
        assert!(
            !skill.join("references/gone.md").exists(),
            "the file an older pack shipped, unmodified, stayed"
        );
        let now = manifest_of(&root, Agent::Codex);
        assert!(
            !now.files.contains_key("references/gone.md"),
            "the new record still lists the file it removed"
        );
        assert_eq!(
            now.files.get("SKILL.md"),
            Some(&sha256(&skills::pack().expect("pack")[0].contents)),
            "the new record does not describe what it wrote"
        );
    }

    /// What a version of atomic that wrote no manifest leaves behind is refused
    /// rather than taken in: with nothing to say who wrote a file, only the
    /// pack's own bytes count as atomic's, and clearing the old skill is the
    /// person's one deliberate step.
    #[test]
    fn a_pack_file_holding_other_content_is_refused_when_no_manifest_vouches_for_it() {
        let root = scratch("changed");
        std::fs::create_dir_all(root.join(".agents/skills/atomic-create-atom/references"))
            .expect("references");
        let skill = root.join(".agents/skills/atomic-create-atom/SKILL.md");
        std::fs::write(&skill, "---\nname: atomic-create-atom\n---\nmine\n").expect("write");
        let error = install(Agent::Codex, &root).expect_err("refused");
        assert_eq!(
            format!("{error:#}"),
            ".agents/skills/atomic-create-atom holds what atomic did not write (an older \
             version of atomic may be the cause): .agents/skills/atomic-create-atom/SKILL.md \
             holds other content than the pack's, which atomic wrote there and can replace; \
             remove what is named and run `atomic setup` again"
        );
        assert_eq!(
            std::fs::read_to_string(&skill).expect("unchanged"),
            "---\nname: atomic-create-atom\n---\nmine\n"
        );
        assert!(!root.join("AGENTS.md").exists());
    }

    /// A file atomic wrote, listed in its own manifest, that someone has since
    /// changed is theirs, not atomic's to throw away.
    #[test]
    fn an_edited_pack_file_is_refused_and_left_as_it_is() {
        let root = scratch("edited");
        install(Agent::Codex, &root).expect("installed");
        let skill = skill_dir(&root, Agent::Codex);
        let theirs = "the person's own instructions, added to the skill\n";
        std::fs::write(skill.join("SKILL.md"), theirs).expect("write");
        let error = install(Agent::Codex, &root).expect_err("refused");
        assert_eq!(
            format!("{error:#}"),
            ".agents/skills/atomic-create-atom holds what atomic did not write (an older \
             version of atomic may be the cause): .agents/skills/atomic-create-atom/SKILL.md \
             no longer holds what atomic wrote there; remove what is named and run `atomic \
             setup` again"
        );
        assert_eq!(
            std::fs::read_to_string(skill.join("SKILL.md")).expect("theirs"),
            theirs
        );
        // The record still describes what the last setup wrote, not what is on
        // disk now: nothing was written over the refusal.
        assert_eq!(
            manifest_of(&root, Agent::Codex).files.get("SKILL.md"),
            Some(&sha256(&skills::pack().expect("pack")[0].contents)),
            "the record was rewritten over the refusal"
        );
    }

    /// The manifest decides whose a file is, so a directory that has one cannot
    /// smuggle anything in by holding the pack's bytes.
    #[test]
    fn a_file_the_manifest_does_not_list_is_refused_even_holding_the_packs_content() {
        let root = scratch("unlisted");
        install(Agent::Codex, &root).expect("installed");
        let skill = skill_dir(&root, Agent::Codex);
        std::fs::write(
            skill.join("SKILL.md.bak"),
            std::fs::read(skill.join("SKILL.md")).expect("read"),
        )
        .expect("write");
        let error = install(Agent::Codex, &root).expect_err("refused");
        assert_eq!(
            format!("{error:#}"),
            ".agents/skills/atomic-create-atom holds what atomic did not write (an older \
             version of atomic may be the cause): \
             .agents/skills/atomic-create-atom/SKILL.md.bak is not a file atomic writes; \
             remove what is named and run `atomic setup` again"
        );
    }

    /// A manifest is the whole basis for what a later setup may overwrite, so one
    /// that is not atomic's own is refused where it stands: reading it as no
    /// manifest at all would let anyone's file list vouch for the directory.
    #[test]
    fn a_manifest_atomic_did_not_write_is_refused() {
        let zeros = "0".repeat(64);
        let outside = format!("{{\"format\":1,\"files\":{{\"../elsewhere.md\":\"{zeros}\"}}}}");
        let bad_hash = "{\"format\":1,\"files\":{\"SKILL.md\":\"not a hash\"}}".to_owned();
        for (what, text, says) in [
            (
                "not-json",
                b"not a manifest at all\n".to_vec(),
                "is not a manifest atomic can read",
            ),
            (
                "not-text",
                vec![0xff, 0xfe, 0x00, 0x01],
                "is not a manifest atomic can read",
            ),
            (
                "another-format",
                b"{\"format\":2,\"files\":{}}".to_vec(),
                "is manifest format 2, which this atomic cannot read",
            ),
            (
                "foreign-path",
                outside.into_bytes(),
                "lists ../elsewhere.md as",
            ),
            (
                "foreign-hash",
                bad_hash.into_bytes(),
                "lists SKILL.md as not a hash",
            ),
        ] {
            let root = scratch("manifest");
            install(Agent::Codex, &root).expect("installed");
            std::fs::write(skill_dir(&root, Agent::Codex).join(MANIFEST), &text)
                .expect("write the impostor");
            let error = format!("{:#}", install(Agent::Codex, &root).unwrap_err());
            assert!(
                error.contains(&format!("{MANIFEST} {says}")),
                "{what}: {error}"
            );
            assert_eq!(
                std::fs::read(skill_dir(&root, Agent::Codex).join(MANIFEST)).expect("read"),
                text,
                "{what}: the impostor was written over, so the directory was taken in"
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
            ".agents/skills/atomic-create-atom holds what atomic did not write (an older \
             version of atomic may be the cause): \
             .agents/skills/atomic-create-atom/scripts is not a directory atomic writes; \
             remove what is named and run `atomic setup` again"
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

    /// The extra thing can be a plain file beside the pack's own, which is what
    /// a leftover from an older atomic or a hand-written note looks like.
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
            ".claude/skills/atomic-create-atom holds what atomic did not write (an older \
             version of atomic may be the cause): \
             .claude/skills/atomic-create-atom/extra.md is not a file atomic writes; remove \
             what is named and run `atomic setup` again"
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
            ".agents/skills/atomic-create-atom holds what atomic did not write (an older \
             version of atomic may be the cause): \
             .agents/skills/atomic-create-atom/.alias is not a file atomic writes; remove \
             what is named and run `atomic setup` again"
        );
        assert!(!root.join("AGENTS.md").exists());
    }

    /// A refusal is written where a person reads it, so a name cannot forge the
    /// lines around it: what is refused is named on one line, escaped.
    #[cfg(unix)]
    #[test]
    fn a_refusal_escapes_a_name_a_person_gave_a_file() {
        let root = scratch("escaping");
        std::fs::create_dir_all(skill_dir(&root, Agent::Codex)).expect("skill directory");
        std::os::unix::fs::symlink(
            "/etc/hosts",
            skill_dir(&root, Agent::Codex).join("evil\nrm -rf ~"),
        )
        .expect("link");
        let error = format!("{:#}", install(Agent::Codex, &root).unwrap_err());
        assert!(
            error.contains(".agents/skills/atomic-create-atom/evil\\nrm -rf ~ is a symbolic link"),
            "{error}"
        );
        assert_eq!(
            error.lines().count(),
            1,
            "a name split the refusal: {error}"
        );
        assert!(!root.join("AGENTS.md").exists());
    }

    /// Too much to name is counted, not squeezed: what is named stops, what is
    /// left is tallied, and the advice a person has to follow stays whole.
    #[test]
    fn a_long_list_of_names_is_cut_and_counted_and_the_advice_stays() {
        let root = scratch("many");
        std::fs::create_dir_all(skill_dir(&root, Agent::Codex)).expect("skill directory");
        for i in 0..60 {
            std::fs::write(
                skill_dir(&root, Agent::Codex).join(format!("extra-file-number-{i:02}.md")),
                "mine",
            )
            .expect("write");
        }
        let error = format!("{:#}", install(Agent::Codex, &root).unwrap_err());
        assert!(
            error.ends_with("remove what is named and run `atomic setup` again"),
            "the advice was cut: {error}"
        );
        // The list's own limit leaves room for far fewer than these 60, so the
        // rest is counted, and the name after the last one written is absent.
        assert!(error.contains("and 37 more"), "not counted: {error}");
        assert_eq!(
            error.matches("is not a file atomic writes").count(),
            23,
            "not the number the list's limit allows: {error}"
        );
        assert!(
            !error.contains("extra-file-number-23.md"),
            "named past the list's own limit"
        );
        assert_eq!(error.lines().count(), 1, "split over lines: {error}");
    }

    /// What an entry that is not a file at all looks like where a pack file
    /// belongs: refused as what it is, and never opened. A fifo would wait for a
    /// writer, or stream for ever, so being told it is not a plain file is not
    /// only clearer than reading it, it is what lets setup finish.
    #[test]
    #[cfg(unix)]
    fn a_pipe_where_a_pack_file_belongs_is_refused_as_not_a_plain_file() {
        let root = scratch("fifo");
        std::fs::create_dir_all(skill_dir(&root, Agent::Codex)).expect("skill directory");
        let path = skill_dir(&root, Agent::Codex).join("SKILL.md");
        let fifo = std::ffi::CString::new(path.into_os_string().into_encoded_bytes())
            .expect("a path with no inner NUL");
        // SAFETY: mkfifo reads a NUL-terminated path and makes a pipe there.
        #[allow(unsafe_code)]
        let made = unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) };
        assert_eq!(made, 0, "mkfifo: {}", std::io::Error::last_os_error());
        let error = format!("{:#}", install(Agent::Codex, &root).unwrap_err());
        assert!(
            error.contains(".agents/skills/atomic-create-atom/SKILL.md is not a plain file"),
            "{error}"
        );
        assert!(!root.join("AGENTS.md").exists(), "installed anyway");
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
             .agents/skills/atomic-create-atom/references/fields.md is a symbolic link, and \
             atomic never writes through one; remove what is named and run `atomic setup` \
             again"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("someone-elses.md")).expect("read"),
            "theirs"
        );
        assert!(!root.join("AGENTS.md").exists());
    }

    /// The directory behind the alias looks like a skill of its own, right down
    /// to the `name` its directory goes by, and stays exactly as it was.
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
             .agents/skills/atomic-create-atom is a symbolic link, and atomic never writes \
             through one; remove what is named and run `atomic setup` again"
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
             .agents/skills/atomic-create-atom/SKILL.md is a symbolic link, and atomic never \
             writes through one; remove what is named and run `atomic setup` again"
        );
        assert!(!root.join("AGENTS.md").exists());
    }

    /// Refusing is about the directory atomic owns. A skill installed beside it
    /// stays, and so does anything else in the shared skills directory.
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

    /// A skill directory is installed only down a chain of plain directories. A
    /// link partway down leads the agent, and atomic's own writes, somewhere
    /// else, so it is refused where it is, and what it leads to is left out. The
    /// advice is about the link: the directory it stands in is the person's own.
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
            ".claude/skills/atomic-create-atom holds what atomic did not write: .claude is a \
             symbolic link, and atomic never writes through one; remove what is named and run \
             `atomic setup` again"
        );
        assert!(
            !format!("{error:#}").contains("whole directory"),
            "the advice named the wrong thing to remove"
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
            ".agents/skills/atomic-create-atom holds what atomic did not write: .agents is a \
             symbolic link, and atomic never writes through one; remove what is named and run \
             `atomic setup` again"
        );
        assert!(
            std::fs::read_dir(&outside)
                .expect("outside")
                .next()
                .is_none(),
            "something was written where the link leads"
        );

        let root = scratch("link-file");
        std::fs::write(outside.join("notes.md"), "theirs").expect("write");
        std::os::unix::fs::symlink(outside.join("notes.md"), root.join("CLAUDE.md")).expect("link");
        let error = install(Agent::Claude, &root).expect_err("refused");
        assert!(
            format!("{error:#}").contains("CLAUDE.md is a symbolic link outside"),
            "{error:#}"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("notes.md")).expect("read"),
            "theirs"
        );
    }

    /// A link standing where the instructions file belongs is followed only when
    /// it leads to the other agent's instructions file. One that reaches a file of
    /// the person's own is refused wherever in the repository it sits, so
    /// `CLAUDE.md` cannot be an alias for a note beside the skill.
    #[cfg(unix)]
    #[test]
    fn a_link_to_a_file_atomic_does_not_write_is_refused_even_inside_the_repository() {
        let root = scratch("inside-escape");
        std::fs::create_dir_all(root.join(".claude")).expect("claude directory");
        std::fs::write(root.join(".claude/notes.md"), "the person's own note\n").expect("write");
        std::os::unix::fs::symlink(".claude/notes.md", root.join("CLAUDE.md")).expect("link");
        let error = install(Agent::Claude, &root).expect_err("refused");
        let text = format!("{error:#}");
        assert!(
            text.contains("CLAUDE.md is a symbolic link to")
                && text.contains("which is not a file atomic may write through from here"),
            "{text}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join(".claude/notes.md")).expect("theirs"),
            "the person's own note\n"
        );
        assert!(
            std::fs::symlink_metadata(root.join("CLAUDE.md"))
                .expect("meta")
                .file_type()
                .is_symlink(),
            "the link was replaced"
        );
        assert!(
            !skill_dir(&root, Agent::Claude).join("SKILL.md").is_file(),
            "the skill was installed despite the refusal"
        );
    }

    /// A link at the instructions file that leads into a skill is refused as
    /// well: writing there would put atomic's instructions over one of the pack's
    /// own files, which is no more atomic's to do than overwriting a note of the
    /// person's.
    #[cfg(unix)]
    #[test]
    fn a_link_from_the_instructions_file_into_the_skill_is_refused() {
        let root = scratch("into-skill");
        install(Agent::Codex, &root).expect("the skill is installed");
        let pack_file = skill_dir(&root, Agent::Codex).join("SKILL.md");
        std::os::unix::fs::symlink(
            Path::new(".agents/skills")
                .join(SKILL)
                .join("SKILL.md")
                .to_string_lossy()
                .as_ref(),
            root.join("CLAUDE.md"),
        )
        .expect("link");
        let before = std::fs::read(&pack_file).expect("read");
        let error = install(Agent::Claude, &root).expect_err("refused");
        let text = format!("{error:#}");
        assert!(
            text.contains("CLAUDE.md is a symbolic link to")
                && text.contains("which is not a file atomic may write through from here"),
            "{text}"
        );
        assert_eq!(
            std::fs::read(&pack_file).expect("read"),
            before,
            "the instructions were written into the skill"
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
