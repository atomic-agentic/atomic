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
//! rest of the instructions file, stay as they were. Each file goes down as a
//! temporary made new beside it and then renamed into place, down directories
//! opened one at a time and never through a link, so a write never passes
//! through a link and a file is never left half written.

use crate::shown::line;
use crate::skills;
use anyhow::{Context, bail, ensure};
use std::cell::RefCell;
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
/// wrote. What passes may be rewritten; the only things removed are a file the
/// manifest says atomic wrote, still holding what it recorded, that this pack no
/// longer ships, the directories that removing those files leaves empty, and an
/// empty directory atomic has no reason to write in whose record lists a path
/// under it, the shell of one an earlier setup wrote -- the last only when it
/// is all there was to refuse, and named in the refusal that comes with it. The
/// manifest goes down after the files it records.
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
    // refusal leaves every directory as it was found, but for the empty shell
    // of a directory an earlier setup wrote, which is cleared when it is all a
    // check has to refuse (see `OwnedDir::check`). Each check comes back
    // with the files its manifest says are atomic's, still unmodified, that
    // this pack no longer ships.
    let skills = owned_dirs(&pack)?;
    // What this pack writes, by pack path, so that a listed file or a pack path
    // holding exactly these bytes is known to be atomic's own work and not
    // someone's. A pack never repeats a path, so nothing here is lost.
    let mut pack_bytes: BTreeMap<&str, &[u8]> = BTreeMap::new();
    for file in &pack {
        pack_bytes.insert(file.path.as_str(), file.contents.as_bytes());
    }
    let mut owned = Vec::new();
    for skill in &skills {
        let dir = OwnedDir::new(&root, skills_dir, skill, &pack, &pack_bytes);
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
    // Where the instructions file really is, which is where it is read from
    // and written to below: the path as it was given, or, for the one link
    // `link_to_own_file` allows, the other agent's instructions file it leads
    // to. A write goes to where a file is, never through a link.
    let instructions_target = link_to_own_file(
        &root,
        instructions,
        &root.join(instructions),
        &instructions_files,
    )?;
    // What the instructions file is to hold, read and put together before
    // anything is written: an instructions file atomic cannot read, or one whose
    // atomic section is broken, is refused here, so a refusal leaves the skill
    // not partly installed for an agent to find.
    let read = at::read_plain(&root, &instructions_target, READ_BYTES_MAX)?;
    // What the write must find there again: the very file read, or nothing.
    let expect = read
        .as_ref()
        .map_or(at::Expect::Absent, |file| at::Expect::Same(file.id));
    let current = match read {
        Some(file) => Some(String::from_utf8(file.bytes).map_err(|_| {
            anyhow::anyhow!("{} is not text atomic can read", instructions.display())
        })?),
        None => None,
    };
    let updated = with_section(current.as_deref(), &section())
        .with_context(|| format!("update {}", instructions.display()))?;
    // Checked now, as the write will check it again, so a file removed or put
    // there since it was read refuses before any of the pack is written.
    at::check(&root, &instructions_target, expect)
        .with_context(|| format!("write {}", instructions.display()))?;
    for (dir, stale) in &owned {
        let mut emptied: BTreeSet<&str> = BTreeSet::new();
        for gone in stale {
            at::remove(&root, &dir.at.join(gone), false)
                .with_context(|| format!("remove {}", dir.shown(gone)))?;
            emptied.extend(parents_of(gone));
        }
        // A directory of the old pack that held only the files just removed is
        // empty now, and an empty directory the next pack has no reason to write
        // in is one the next setup would refuse. Deepest first, so emptying an
        // inner one is what can empty the one above it. One that still holds
        // anything is left: `remove_dir` refuses a directory that is not empty,
        // so nothing here can take a file away.
        let mut emptied: Vec<&str> = emptied.into_iter().collect();
        emptied.sort_by_key(|parent| std::cmp::Reverse(parent.split('/').count()));
        for parent in emptied {
            let _ = at::remove(&root, &dir.at.join(parent), true);
        }
    }
    let mut written = Vec::new();
    for file in &pack {
        let relative = skills_dir.join(&file.path);
        write_inside(&root, &relative, file.contents.as_bytes())?;
        written.push(relative);
    }
    // After the files, so a setup that stopped halfway leaves the record of the
    // run before it and never one that vouches for bytes atomic never wrote.
    // Running setup again finishes such a run: every file on disk then holds
    // either the bytes the old record still describes or the bytes this pack
    // writes, and a file the interrupted run had already removed is simply not
    // there to be accounted for.
    for (dir, _) in &owned {
        let relative = skills_dir.join(dir.skill).join(MANIFEST);
        write_inside(&root, &relative, dir.manifest().as_bytes())?;
        written.push(relative);
    }
    if current.as_deref() != Some(updated.as_str()) {
        write_expecting(&root, &instructions_target, updated.as_bytes(), expect)?;
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
        // Both halves have to be there: what is owned is the directory in front
        // of the `/`, and a path with no `/`, or with nothing on one side of it,
        // names no such directory. A pack is built here from paths this crate
        // writes, so this is a statement about the pack, not a judgement on
        // anything a person left.
        let (dir, name): (&str, &str) = file.path.split_once('/').unwrap_or_default();
        ensure!(
            !dir.is_empty() && !name.is_empty(),
            "the pack writes {}, which is not a file inside a skill directory",
            line(&file.path)
        );
        ensure!(
            is_plain_relative(&file.path),
            "the pack writes {}, which is not a plain path inside a skill directory",
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

    /// Whether every clause of this report names one of the `shells` empty
    /// directories the walk refused. That is the only case in which setup clears
    /// an empty directory at all: with anything else to complain of, every
    /// directory stays exactly where it was found, so a refusal never takes away
    /// what a person might still want. It is one call so that the control of it
    /// has one site, which `scripts/controls/skill-dir-empty-shell.row` switches
    /// off.
    fn shells_only(&self, shells: usize) -> bool {
        self.edited.is_empty()
            && self.changed.is_empty()
            && self.links.is_empty()
            && self.extra.len() == shells
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
#[serde(deny_unknown_fields)]
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
            // Both are the manifest's text, not atomic's: escaped, so that a
            // path or a hash written to split the refusal cannot do so.
            ensure!(
                is_plain_relative(path)
                    && hash.len() == 64
                    && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                "{here} lists {} as {}, which is not how atomic records a file",
                line(path),
                line(hash)
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

/// Whether `pack` writes exactly `bytes` at `path`. This is the other way a file
/// is known to be atomic's own work: with no manifest to read, or with one that
/// does not list the path, bytes that are this pack's are bytes atomic was going
/// to write there anyway. It is one call so that the control of it has one site,
/// which `scripts/controls/skill-dir-pack-bytes.row` switches off.
fn pack_holds(pack: &BTreeMap<&str, &[u8]>, path: &str, bytes: &[u8]) -> bool {
    pack.get(path).copied() == Some(bytes)
}

/// How far setup will read a file of the skill directory to find out what it
/// holds. Every file of the pack is read here, and so is every file a manifest
/// lists, so the bound has to cover the largest of them; what is larger than any
/// file atomic writes is not one of them, and is refused where it stands rather
/// than read for its hash. `pack_files_fit_their_limits` keeps this above every
/// file of the pack.
const READ_BYTES_MAX: u64 = 4 * 1024 * 1024;

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
    /// What this pack writes, by pack path, for a file on disk to be recognised
    /// as atomic's own bytes.
    pack_bytes: &'a BTreeMap<&'a str, &'a [u8]>,
    /// The directories the walk refused as ones atomic does not write which
    /// hold nothing at all, by where they sit below this one. A directory is the
    /// one thing a refusal names that setup can still clear: an empty one is
    /// often the shell an earlier pack left when its files went, and nothing a
    /// person can see is there to remove.
    empty: RefCell<BTreeSet<String>>,
}

impl<'a> OwnedDir<'a> {
    fn new(
        root: &'a Path,
        skills_dir: &Path,
        skill: &'a str,
        pack: &'a [skills::SkillFile],
        pack_bytes: &'a BTreeMap<&'a str, &'a [u8]>,
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
            pack_bytes,
            empty: RefCell::new(BTreeSet::new()),
        }
    }

    /// Refuse, naming each of them, whatever this directory holds that atomic
    /// did not write, and whatever link stands between the repository root and
    /// it. An absent directory is what a first install meets.
    ///
    /// What the directory's manifest lists is what atomic wrote there. A listed
    /// file still holding the bytes it records may be written over, and comes
    /// back from here for removal when this pack no longer ships it; a listed
    /// file holding other bytes was edited since and is refused. A file that is
    /// not listed but holds exactly what this pack writes there is atomic's own
    /// work too, which is what lets a run that stopped halfway be run again. A
    /// directory atomic does not write in is kept only while it holds a listed
    /// file on disk; empty shells of that kind are the one thing a refusal here
    /// can answer for the person, and they are removed when they are all it has
    /// to complain of. With no manifest to go by, only a directory holding
    /// exactly this pack's files, with this pack's bytes, passes.
    fn check(&self) -> anyhow::Result<Vec<String>> {
        let mut report = Report::default();
        if let Some(link) = self.chain_link() {
            // Read through an alias, so this is not the directory atomic was
            // asked to install into, however plain what it leads to looks.
            report.links.push(link_named(&link));
        } else if is_link(&self.path) {
            // The whole directory atomic owns is an alias for elsewhere.
            report.links.push(link_named(&self.shown("")));
        } else if let Some(dir) = at::open_dirs(self.root, &self.at)? {
            // Held open from here: the walk lists and reads through it, so
            // a directory swapped for a link since the chain was weighed is
            // refused, never followed.
            let manifest = self.read_manifest(&dir)?;
            self.inspect(&dir, "", manifest.as_ref(), &mut report)?;
            if report.is_empty() {
                return Ok(self.leftovers(&manifest, &report));
            }
            // An empty directory atomic has no reason to write in, whose record
            // lists a path under it, is the one thing a person is told to remove
            // that they cannot see to remove: the shell of a directory an earlier
            // pack wrote, whose files a later one stopped removing. Setup clears
            // those, and only when they are the whole of what is refused: a run
            // that has anything else to
            // complain of leaves every directory exactly as it found it, so what
            // comes out of a skill directory is only ever an empty shell, and the
            // refusal says so.
            let shells = self.empty.borrow().len();
            let cleared = if report.shells_only(shells) {
                self.clear_empty_dirs()
            } else {
                Vec::new()
            };
            let cleared = match cleared.len() {
                0 => String::new(),
                1 => format!(" ({} was empty and is removed)", cleared[0]),
                n => format!(
                    " ({n} empty directories are removed: {})",
                    cleared.join(", ")
                ),
            };
            bail!("{}{}", self.refusal(&report), cleared);
        }
        ensure!(report.is_empty(), "{}", self.refusal(&report));
        Ok(Vec::new())
    }

    /// The empty directories this walk refused, deepest first, each emptied of
    /// itself: a directory that turned out not to be empty after all is left, as
    /// is one that could not be opened. Only a directory the walk itself refused
    /// and found with nothing in it is ever tried, and `remove_dir` refuses a
    /// directory that holds anything, so nothing here can take a file away.
    fn clear_empty_dirs(&self) -> Vec<String> {
        let mut empty: Vec<String> = self.empty.borrow().iter().cloned().collect();
        empty.sort_by_key(|at| std::cmp::Reverse(at.split('/').count()));
        let mut cleared = Vec::new();
        for at in empty {
            if at::remove(self.root, &self.at.join(&at), true).is_ok_and(|removed| removed) {
                cleared.push(self.shown(&at));
            }
        }
        cleared
    }

    /// The manifest this directory carries, if it has one: what the last setup
    /// wrote here. A file that is not a manifest atomic wrote is refused here,
    /// rather than read as no manifest at all, because that would let whatever
    /// is in the directory through on the strength of a file someone else left.
    fn read_manifest(&self, dir: &at::Dir) -> anyhow::Result<Option<Manifest>> {
        let shown = self.shown(MANIFEST);
        let name = std::ffi::OsStr::new(MANIFEST);
        // Plain file or nothing, before it is opened: what is asked of this
        // path is one small read, and a fifo or a device would wait or stream.
        let meta = match at::entry(dir, name) {
            Ok(meta) if meta.kind == at::Kind::File => meta,
            Ok(_) => {
                bail!("{shown} is not a plain file, and atomic writes a plain file there")
            }
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("cannot inspect {shown}")),
        };
        let too_large = |size: u64| {
            anyhow::anyhow!(
                "{shown} is {size} bytes, which is larger than any manifest atomic writes; \
                 replace it and run `atomic setup` again"
            )
        };
        if meta.len > MANIFEST_BYTES_MAX {
            return Err(too_large(meta.len));
        }
        let text = at::read_in(dir, name, MANIFEST_BYTES_MAX)
            .with_context(|| format!("cannot read {shown}"))?
            .ok_or_else(|| too_large(MANIFEST_BYTES_MAX + 1))
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
    /// the pack writes a file under it or something standing in it is listed in
    /// the manifest, so what atomic never wrote is refused with the directory
    /// holding it and nothing here descends past what atomic itself nested.
    ///
    /// A name that is not text is refused where it stands. Naming it is only
    /// possible lossily, and two entries whose names differ only in bytes that
    /// are not text have the same lossy name: judging an entry by a name another
    /// entry shares would let one hide the other, so no entry is judged by a name
    /// it does not have. Every filesystem call is made by the entry's name in
    /// `dir`, the directory held open: a directory on the way swapped for a
    /// link after [`OwnedDir::chain_link`] weighed the chain is refused where
    /// it is opened, never listed or read through.
    fn inspect(
        &self,
        dir: &at::Dir,
        at: &str,
        manifest: Option<&Manifest>,
        report: &mut Report,
    ) -> anyhow::Result<()> {
        let shown_at = self.shown(at);
        let mut names = at::list(dir).with_context(|| format!("cannot inspect {shown_at}"))?;
        names.sort();
        for entry in names {
            // The entry's own name in the directory held open, which is what
            // every call below makes. The name is taken lossily only to refuse
            // it: names that differ only in bytes which are not text have the
            // same lossy name, so an entry judged by a name another entry
            // shares could hide that other entry.
            let name = entry.as_os_str();
            let Some(name) = name.to_str() else {
                let below = if at.is_empty() {
                    name.to_string_lossy().into_owned()
                } else {
                    format!("{at}/{}", name.to_string_lossy())
                };
                report.extra.push(format!(
                    "{} is not a name atomic can read as text",
                    self.shown(&below)
                ));
                continue;
            };
            let relative = if at.is_empty() {
                name.to_owned()
            } else {
                format!("{at}/{name}")
            };
            let shown = self.shown(&relative);
            // What an entry is, not what it points at: a symbolic link is
            // named as one and never followed, read from or written through.
            let meta = at::entry(dir, &entry).with_context(|| format!("cannot inspect {shown}"))?;
            if meta.kind == at::Kind::Link {
                report.links.push(link_named(&shown));
                continue;
            }
            // The manifest, which atomic writes last and reads first, is as
            // much atomic's own as any file it lists.
            if relative == MANIFEST {
                continue;
            }
            let listed = manifest.is_some_and(|m| m.files.contains_key(relative.as_str()));
            if meta.kind == at::Kind::Dir {
                let sub =
                    at::subdir(dir, &entry).with_context(|| format!("cannot inspect {shown}"))?;
                // What keeps a directory that is not one of the pack's is what
                // stands in it, not what a manifest once listed there: a
                // directory whose listed files are no longer on disk is the
                // shell an earlier pack left, which is exactly what atomic has
                // no reason to write in.
                if !self.dirs.contains(relative.as_str())
                    && !self.holds_own(&sub, &relative, manifest)?
                {
                    report
                        .extra
                        .push(format!("{shown} is not a directory atomic writes"));
                    // An empty one whose manifest still lists a path under it
                    // is the shell of a directory an earlier setup wrote and
                    // emptied: it is named and then cleared, because there is
                    // nothing in it for a person to go and remove. An empty one
                    // atomic cannot account for, such as one a person made, and
                    // anything that holds something, are left as they are.
                    let empty = at::list(&sub).is_ok_and(|entries| entries.is_empty());
                    if empty && lists_under(manifest, &relative) {
                        self.empty.borrow_mut().insert(relative);
                    }
                    continue;
                }
                self.inspect(&sub, &relative, manifest, report)?;
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
            if meta.kind != at::Kind::File {
                report.changed.push(format!(
                    "{shown} is not a plain file, and atomic writes a plain file there"
                ));
                continue;
            }
            if listed {
                // The manifest says atomic wrote these bytes here. If they are
                // still here, the file is atomic's to replace; so is one holding
                // exactly what this pack writes there, which is what a run that
                // stopped after the files and before its manifest leaves.
                // Anything else was edited since, and is not atomic's to throw
                // away.
                let bytes = self.bytes(dir, &entry, &shown, meta.len)?;
                let holds = manifest_vouches_for(manifest, &relative, &bytes);
                let holds =
                    holds || pack_holds(self.pack_bytes, &self.pack_path(&relative), &bytes);
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
            let path = self.pack_path(&relative);
            let file = self
                .pack
                .iter()
                .find(|f| f.path == path)
                .expect("the pack has its own path");
            // Sizes first, so a large leftover costs no reading.
            let other = meta.len != file.contents.len() as u64 || {
                let bytes = self.bytes(dir, &entry, &shown, meta.len)?;
                !pack_holds(self.pack_bytes, &path, &bytes)
            };
            if other {
                report.changed.push(format!(
                    "{shown} holds other content than the pack's, which atomic wrote there and \
                     can replace"
                ));
            }
        }
        Ok(())
    }

    /// Whether the directory `entry`, which sits at `relative` below this one,
    /// holds on disk anything the manifest lists: a file it names there, or a
    /// directory that could hold one. What the manifest lists is not enough on
    /// its own, because a directory whose listed files are no longer on disk is
    /// the shell of one an earlier pack wrote, and atomic refuses a directory it
    /// has no reason to write in, empty or not. A name that is not text is left
    /// out of this: a manifest is text, so it can name no such entry.
    fn holds_own(
        &self,
        dir: &at::Dir,
        relative: &str,
        manifest: Option<&Manifest>,
    ) -> anyhow::Result<bool> {
        let Some(manifest) = manifest else {
            return Ok(false);
        };
        let shown = self.shown(relative);
        let names = at::list(dir).with_context(|| format!("cannot inspect {shown}"))?;
        for name in names {
            let Some(name) = name.to_str() else {
                continue;
            };
            let child = format!("{relative}/{name}");
            let under = format!("{child}/");
            if manifest.files.contains_key(child.as_str())
                || manifest
                    .files
                    .keys()
                    .any(|path| path.starts_with(under.as_str()))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The bytes of a plain file the walk has seen to be one, read from `entry`,
    /// which is where the walk saw it. `size` is what its own directory entry
    /// said, and `READ_BYTES_MAX` is the most that will be read for a hash.
    fn bytes(
        &self,
        dir: &at::Dir,
        entry: &std::ffi::OsStr,
        shown: &str,
        size: u64,
    ) -> anyhow::Result<Vec<u8>> {
        let too_large = |size: u64| {
            anyhow::anyhow!(
                "{shown} is {size} bytes, more than the {READ_BYTES_MAX} bytes any file atomic \
                 writes holds; replace it and run `atomic setup` again"
            )
        };
        if size > READ_BYTES_MAX {
            return Err(too_large(size));
        }
        at::read_in(dir, entry, READ_BYTES_MAX)
            .with_context(|| format!("cannot read {shown}"))?
            .ok_or_else(|| too_large(READ_BYTES_MAX + 1))
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

/// The bytes of the plain file at `path`, opened once, never through a link
/// and never waiting, as opening a fifo would, or `None` when it holds more
/// than `max`: a file that grew since its size was seen is read no further
/// than that, and one that is no longer a plain file is refused.
#[cfg(not(unix))]
fn read_at_most(path: &Path, max: u64) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read as _;
    let mut open = std::fs::OpenOptions::new();
    open.read(true);
    let file = open.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a plain file"));
    }
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= max).then_some(bytes))
}

/// Whether `manifest` lists a path below the directory `relative`: what makes
/// an empty directory there the shell of one atomic wrote.
/// `scripts/controls/skill-dir-shell-listed.row` switches its one use off.
fn lists_under(manifest: Option<&Manifest>, relative: &str) -> bool {
    let under = format!("{relative}/");
    manifest.is_some_and(|m| m.files.keys().any(|path| path.starts_with(under.as_str())))
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

/// The directories the path `relative` passes through, deepest first: `a/b/c`
/// gives `a/b` and then `a`. Deepest first is the order to try removing them in,
/// because emptying the inner one is what can empty the one above it.
fn parents_of(relative: &str) -> Vec<&str> {
    let mut parents = Vec::new();
    let mut at = relative
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .unwrap_or("");
    while !at.is_empty() {
        parents.push(at);
        at = at.rsplit_once('/').map(|(parent, _)| parent).unwrap_or("");
    }
    parents
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

/// Write `contents` at `relative` under `root`, creating directories, and refuse
/// any path that passes through a symbolic link below `root`.
///
/// The directories are walked one at a time from `root`, each opened by its
/// name in the one before it and never through a link, and each made only once
/// it is seen not to be there: what is opened is what the next step works in,
/// so a link put in the way of a directory after it was weighed changes
/// nothing. The bytes go to a temporary of an unpredictable name, made new in
/// the file's own directory and never through a link standing at that name,
/// which is then renamed over the file. A rename replaces whatever stands
/// there, a link included, without following it, and it puts the whole file
/// down at once, so a run that stops halfway leaves the file before it or the
/// file after it, never half of either. The file keeps the permission bits of
/// the plain file it replaces, never setuid, setgid or sticky; a hard link to
/// it is severed, since what stands at the name is a new file. Through
/// [`write_expecting`], the file must still be the one read, and keeps the
/// narrower of the permissions it had then and has now.
///
/// [`link_to_own_file`] is asked of the instructions file before anything is
/// written, and `install` hands over where that link leads when there is one, so
/// what is named here is a place to write, not a link to write through.
fn write_inside(root: &Path, relative: &Path, contents: &[u8]) -> anyhow::Result<()> {
    write_expecting(root, relative, contents, at::Expect::Any)
}

/// [`write_inside`], where what stands at `relative` must still be what
/// `expect` says it was when it was read.
fn write_expecting(
    root: &Path,
    relative: &Path,
    contents: &[u8],
    expect: at::Expect,
) -> anyhow::Result<()> {
    ensure!(
        relative
            .components()
            .all(|c| matches!(c, Component::Normal(_))),
        "{} is not a plain relative path",
        line(&relative.to_string_lossy())
    );
    let name = relative.file_name().context("a file has a name")?;
    let dir = at::open_chain(root, relative)?;
    at::replace(&dir, name, contents, expect)
        .with_context(|| format!("write {}", line(&relative.to_string_lossy())))
}

/// The directory-handle calls [`write_inside`] makes: each works on a name in
/// a directory this process holds open, so nothing is resolved twice.
#[cfg(unix)]
mod at {
    use super::line;
    use anyhow::{Context, bail};
    use std::ffi::{CString, OsStr};
    use std::io::Write as _;
    use std::os::fd::AsFd as _;
    use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    /// How a directory on the way is opened besides: never through a link
    /// standing at its name. `scripts/controls/skill-dir-write-chain.row`
    /// switches this off.
    const DIR_NOT_THROUGH_A_LINK: libc::c_int = libc::O_NOFOLLOW;

    /// How a temporary is opened besides for writing: made new, and never
    /// through a link standing at its name, either of which alone refuses a
    /// link there. `scripts/controls/skill-dir-temp-new.row` switches both off.
    const NEW_NOT_THROUGH_A_LINK: [libc::c_int; 2] = [libc::O_EXCL, libc::O_NOFOLLOW];

    /// How many names a temporary is tried under before setup gives up.
    const TEMP_TRIES: usize = 16;

    /// Which file a read found, and the permissions it had: a write over it
    /// must find that same file there, and gives its contents no more than
    /// those permissions.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) struct FileId {
        dev: u64,
        ino: u64,
        /// Its permission bits, without setuid, setgid or sticky.
        mode: u32,
        /// Who owned it, which a setup run as root gives the file it writes.
        uid: libc::uid_t,
        gid: libc::gid_t,
    }

    /// A file read through [`read_plain`], and which file it was.
    pub(super) struct Read {
        pub(super) bytes: Vec<u8>,
        pub(super) id: FileId,
    }

    /// What a write over a file must find standing there.
    #[derive(Clone, Copy, Debug)]
    pub(super) enum Expect {
        /// Whatever is there, or nothing: a pack file, which a link or a file
        /// of the pack's may stand in place of.
        Any,
        /// Nothing, as there was nothing when it was read.
        Absent,
        /// The very file that was read.
        Same(FileId),
    }

    /// The permission bits of a mode, and nothing else: never setuid, setgid
    /// or sticky, which a file atomic writes never carries.
    /// `scripts/controls/skill-dir-temp-plain-mode.row` switches the mask off.
    // `st_mode` is narrower than a `u32` on some systems, such as macOS.
    #[allow(clippy::useless_conversion)]
    fn permission_bits(st: &libc::stat) -> u32 {
        u32::from(st.st_mode) & 0o777
    }

    // The fields' types differ between systems.
    #[allow(clippy::useless_conversion, clippy::unnecessary_cast)]
    fn id_of(st: &libc::stat) -> FileId {
        FileId {
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
            mode: permission_bits(st),
            uid: st.st_uid,
            gid: st.st_gid,
        }
    }

    /// The permission bits a read found.
    #[cfg(test)]
    pub(super) fn mode_of(id: FileId) -> u32 {
        id.mode
    }

    /// Whether `st` is the file `id` names.
    /// `scripts/controls/skill-dir-write-same-file.row` switches this off.
    fn same_file(st: &libc::stat, id: FileId) -> bool {
        (id_of(st).dev, id_of(st).ino) == (id.dev, id.ino)
    }

    fn c_name(name: &OsStr) -> anyhow::Result<CString> {
        CString::new(name.as_bytes()).context("a name holds a NUL byte")
    }

    fn openat(
        dir: BorrowedFd<'_>,
        name: &CString,
        flags: libc::c_int,
        mode: libc::c_uint,
    ) -> std::io::Result<OwnedFd> {
        // SAFETY: `name` is a NUL-terminated string that outlives the call,
        // and `dir` is an open descriptor; the result is owned below.
        #[allow(unsafe_code)]
        let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags, mode) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `fd` was just returned open, and nothing else owns it.
        #[allow(unsafe_code)]
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// What stands at `name` in `dir`, not following a link.
    fn stat_at(dir: BorrowedFd<'_>, name: &CString) -> std::io::Result<libc::stat> {
        // SAFETY: a zeroed stat is a valid value of a plain C struct.
        #[allow(unsafe_code)]
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `name` and `st` are valid for the call, and `dir` is open.
        #[allow(unsafe_code)]
        let r = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                name.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if r < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(st)
    }

    fn is_link(st: &libc::stat) -> bool {
        st.st_mode & libc::S_IFMT == libc::S_IFLNK
    }

    /// Open `root`, then each directory `relative` passes through, one name
    /// at a time and never through a link, making each that is not there
    /// yet: the directory the file belongs in, held open.
    pub(super) fn open_chain(root: &Path, relative: &Path) -> anyhow::Result<OwnedFd> {
        Ok(walk(root, relative, true)?.expect("every directory is made"))
    }

    /// [`open_chain`], making nothing: `None` when a directory on the way is
    /// not there, which is the one failure that means there is nothing to
    /// read or remove.
    pub(super) fn find_chain(root: &Path, relative: &Path) -> anyhow::Result<Option<OwnedFd>> {
        walk(root, relative, false)
    }

    fn walk(root: &Path, relative: &Path, make: bool) -> anyhow::Result<Option<OwnedFd>> {
        walk_through(
            root,
            relative.parent().unwrap_or(Path::new("")),
            relative,
            make,
        )
    }

    /// The directory `relative` below `root` itself, held open, reached one
    /// name at a time and never through a link: `None` when a directory on
    /// the way is not there.
    pub(super) fn open_dirs(root: &Path, relative: &Path) -> anyhow::Result<Option<OwnedFd>> {
        walk_through(root, relative, relative, false)
    }

    /// Open `root`, then each directory `dirs` names, as [`walk`] does,
    /// naming `relative` in a refusal.
    fn walk_through(
        root: &Path,
        dirs: &Path,
        relative: &Path,
        make: bool,
    ) -> anyhow::Result<Option<OwnedFd>> {
        let mut dir: OwnedFd = std::fs::File::open(root)
            .with_context(|| format!("open {}", line(&root.to_string_lossy())))?
            .into();
        let mut below = PathBuf::new();
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | DIR_NOT_THROUGH_A_LINK;
        for part in dirs.components() {
            anyhow::ensure!(
                matches!(part, std::path::Component::Normal(_)),
                "{} is not a plain relative path",
                line(&relative.to_string_lossy())
            );
            below.push(part);
            let shown = || line(&below.to_string_lossy());
            let name = c_name(part.as_os_str())?;
            let opened = match openat(dir.as_fd(), &name, flags, 0) {
                // Only a name not there is absent: any other error refuses.
                // `scripts/controls/skill-dir-walk-not-there.row` switches this off.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && !make => return Ok(None),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // SAFETY: `name` is valid for the call, and `dir` is open.
                    #[allow(unsafe_code)]
                    let r = unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o777) };
                    let made = std::io::Error::last_os_error();
                    if r < 0 && made.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(made).with_context(|| format!("create {}", shown()));
                    }
                    openat(dir.as_fd(), &name, flags, 0)
                }
                other => other,
            };
            dir = match opened {
                Ok(next) => next,
                Err(e) => {
                    // Not a directory to go on into: say what it is instead.
                    if stat_at(dir.as_fd(), &name).is_ok_and(|st| is_link(&st)) {
                        bail!(
                            "{} is reached through {}, which is a symbolic link",
                            line(&relative.to_string_lossy()),
                            shown()
                        );
                    }
                    if matches!(e.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP)) {
                        bail!("{} is not a directory", shown());
                    }
                    return Err(e).with_context(|| format!("open {}", shown()));
                }
            };
        }
        Ok(Some(dir))
    }

    /// How a file is opened to be read besides: never through a link standing
    /// at its name, and never waiting, as opening a fifo would.
    /// `scripts/controls/skill-dir-read-at.row` switches the first off, and
    /// `scripts/controls/skill-dir-read-waits.row` the second.
    const READ_NOT_THROUGH_A_LINK: libc::c_int = libc::O_NOFOLLOW;
    const READ_WITHOUT_WAITING: libc::c_int = libc::O_NONBLOCK;
    /// Both, as one set of flags, for a file opened by name in a held
    /// directory.
    pub(super) const READ_FLAGS: libc::c_int = READ_NOT_THROUGH_A_LINK | READ_WITHOUT_WAITING;

    /// The bytes of the plain file at `relative` below `root`, read through
    /// the directories held open on the way and never through a link: `None`
    /// when it is not there, and refused when it is anything but a plain file
    /// or holds more than `max` bytes.
    pub(super) fn read_plain(
        root: &Path,
        relative: &Path,
        max: u64,
    ) -> anyhow::Result<Option<Read>> {
        use std::io::Read as _;
        let shown = line(&relative.to_string_lossy());
        let Some(dir) = find_chain(root, relative)? else {
            return Ok(None);
        };
        let name = c_name(relative.file_name().context("a file has a name")?)?;
        let flags = libc::O_RDONLY | libc::O_CLOEXEC | READ_FLAGS;
        let file = match openat(dir.as_fd(), &name, flags, 0) {
            Ok(fd) => std::fs::File::from(fd),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
                bail!("{shown} is a symbolic link, and atomic never reads through one")
            }
            Err(e) => return Err(e).with_context(|| format!("read {shown}")),
        };
        let meta = file.metadata().with_context(|| format!("read {shown}"))?;
        anyhow::ensure!(meta.is_file(), "{shown} is not a plain file");
        // A file of another name, hard linked here, is not this one's to
        // read into a file the repository tracks.
        anyhow::ensure!(
            std::os::unix::fs::MetadataExt::nlink(&meta) == 1,
            "{shown} has other names, as a hard link does, and atomic reads only a file that \
             is its own"
        );
        // SAFETY: a zeroed stat is a valid value of a plain C struct.
        #[allow(unsafe_code)]
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `st` is valid for the call, and `file` is open.
        #[allow(unsafe_code)]
        if unsafe { libc::fstat(file.as_raw_fd(), &mut st) } != 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| format!("read {shown}"));
        }
        let id = id_of(&st);
        let mut bytes = Vec::new();
        file.take(max + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("read {shown}"))?;
        anyhow::ensure!(
            bytes.len() as u64 <= max,
            "{shown} is more than {max} bytes, larger than atomic reads there"
        );
        Ok(Some(Read { bytes, id }))
    }

    /// A directory held open, which the check walks through.
    pub(super) type Dir = OwnedFd;

    /// What an entry of a directory is, as [`entry`] sees it, never through
    /// a link.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Kind {
        File,
        Dir,
        Link,
        Other,
    }

    /// An entry's kind and size.
    pub(super) struct Entry {
        pub(super) kind: Kind,
        pub(super) len: u64,
    }

    /// What `name` in the directory held open as `dir` is, not following a
    /// link.
    pub(super) fn entry(dir: &OwnedFd, name: &OsStr) -> std::io::Result<Entry> {
        let name = CString::new(name.as_bytes()).map_err(std::io::Error::other)?;
        let st = stat_at(dir.as_fd(), &name)?;
        let kind = match st.st_mode & libc::S_IFMT {
            libc::S_IFREG => Kind::File,
            libc::S_IFDIR => Kind::Dir,
            libc::S_IFLNK => Kind::Link,
            _ => Kind::Other,
        };
        // The field's type differs between systems.
        #[allow(clippy::unnecessary_cast)]
        let len = st.st_size as u64;
        Ok(Entry { kind, len })
    }

    /// The directory `name` in the one held open as `dir`, held open in its
    /// turn, never through a link: a link put there since it was seen to be
    /// a directory is refused, not followed.
    pub(super) fn subdir(dir: &OwnedFd, name: &OsStr) -> anyhow::Result<OwnedFd> {
        let flags =
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | SUBDIR_NOT_THROUGH_A_LINK;
        Ok(openat(dir.as_fd(), &c_name(name)?, flags, 0)?)
    }

    /// How a directory the walk goes into is opened besides: never through
    /// a link standing at its name.
    /// `scripts/controls/skill-dir-check-at.row` switches this off.
    const SUBDIR_NOT_THROUGH_A_LINK: libc::c_int = libc::O_NOFOLLOW;

    /// The names in the directory held open as `dir`, but `.` and `..`.
    pub(super) fn list(dir: &OwnedFd) -> anyhow::Result<Vec<std::ffi::OsString>> {
        use std::os::unix::ffi::OsStringExt;
        // fdopendir takes the descriptor it is given, so it is given a copy,
        // closed on exec as the original is.
        // SAFETY: fcntl with F_DUPFD_CLOEXEC takes an open descriptor and a
        // lowest number, and touches no memory.
        #[allow(unsafe_code)]
        let copy = unsafe { libc::fcntl(dir.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if copy < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: `copy` is an open descriptor this call owns; on success the
        // stream owns it, and it is closed with the stream below.
        #[allow(unsafe_code)]
        let stream = unsafe { libc::fdopendir(copy) };
        if stream.is_null() {
            let e = std::io::Error::last_os_error();
            // SAFETY: on failure `copy` is still this call's to close.
            #[allow(unsafe_code)]
            unsafe {
                libc::close(copy);
            }
            return Err(e.into());
        }
        // From the start, as the copy shares its offset with `dir`.
        // SAFETY: `stream` is the open stream made above.
        #[allow(unsafe_code)]
        unsafe {
            libc::rewinddir(stream);
        }
        let mut names = Vec::new();
        let failed = loop {
            set_errno(0);
            // SAFETY: `stream` is open, and the entry it returns is read
            // before the next call.
            #[allow(unsafe_code)]
            let next = unsafe { libc::readdir(stream) };
            if next.is_null() {
                let e = std::io::Error::last_os_error();
                break (e.raw_os_error() != Some(0)).then_some(e);
            }
            // SAFETY: `d_name` is a NUL-terminated name inside the entry.
            #[allow(unsafe_code)]
            let name = unsafe { std::ffi::CStr::from_ptr((*next).d_name.as_ptr()) };
            let name = name.to_bytes();
            if name != b"." && name != b".." {
                names.push(std::ffi::OsString::from_vec(name.to_vec()));
            }
        };
        // SAFETY: `stream` is open, and closing it closes `copy`.
        #[allow(unsafe_code)]
        unsafe {
            libc::closedir(stream);
        }
        match failed {
            Some(e) => Err(e.into()),
            None => Ok(names),
        }
    }

    /// Set this thread's `errno`, which `readdir` leaves as it was at its end.
    fn set_errno(value: libc::c_int) {
        // SAFETY: the location is this thread's own errno.
        #[allow(unsafe_code)]
        unsafe {
            #[cfg(target_os = "linux")]
            {
                *libc::__errno_location() = value;
            }
            #[cfg(target_os = "macos")]
            {
                *libc::__error() = value;
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                let _ = value;
            }
        }
    }

    /// The bytes of the plain file `name` in the directory held open as
    /// `dir`, opened never through a link and never waiting, or `None` when
    /// it holds more than `max`. A file with other names, as a hard link
    /// has, is refused, as [`read_plain`] refuses one.
    pub(super) fn read_in(
        dir: &OwnedFd,
        name: &OsStr,
        max: u64,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        use std::io::Read as _;
        let flags = libc::O_RDONLY | libc::O_CLOEXEC | READ_FLAGS;
        let file = std::fs::File::from(openat(dir.as_fd(), &c_name(name)?, flags, 0)?);
        let meta = file.metadata()?;
        anyhow::ensure!(meta.is_file(), "not a plain file");
        // `scripts/controls/skill-dir-read-in-one-name.row` switches this off.
        let one_name = std::os::unix::fs::MetadataExt::nlink(&meta) == 1;
        anyhow::ensure!(
            one_name,
            "it has other names, as a hard link does, and atomic reads only a file that is its own"
        );
        let mut bytes = Vec::new();
        file.take(max + 1).read_to_end(&mut bytes)?;
        Ok((bytes.len() as u64 <= max).then_some(bytes))
    }

    /// Remove the file, or with `dir` the empty directory, at `relative`
    /// below `root`, by its name in the directory held open on the way, never
    /// through a link: `false` when it is not there.
    pub(super) fn remove(root: &Path, relative: &Path, dir: bool) -> anyhow::Result<bool> {
        let shown = line(&relative.to_string_lossy());
        let Some(parent) = find_chain(root, relative)? else {
            return Ok(false);
        };
        let name = c_name(relative.file_name().context("a path has a name")?)?;
        let how = if dir { libc::AT_REMOVEDIR } else { 0 };
        // SAFETY: `name` is valid for the call, and `parent` is open.
        #[allow(unsafe_code)]
        let r = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), how) };
        if r == 0 {
            return Ok(true);
        }
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::NotFound {
            return Ok(false);
        }
        Err(e).with_context(|| format!("remove {shown}"))
    }

    /// Whether what stands at `target` in `dir` is what `expect` says, and
    /// the permission bits the file written there is to have: a pack file's,
    /// those of the plain file it replaces, if one is there; the instructions
    /// file's, the narrower of those it had when it was read and has now. Only
    /// "nothing there" is taken for nothing; any other failure to look
    /// refuses.
    fn expected(dir: &OwnedFd, target: &CString, expect: Expect) -> anyhow::Result<Option<u32>> {
        let now = match stat_at(dir.as_fd(), target) {
            Ok(st) => Some(st),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).context("inspect what stands there"),
        };
        let plain = now.filter(|st| st.st_mode & libc::S_IFMT == libc::S_IFREG);
        Ok(match expect {
            Expect::Any => plain.as_ref().map(permission_bits),
            Expect::Absent => {
                anyhow::ensure!(now.is_none(), "a file was put there since it was read");
                None
            }
            Expect::Same(id) => {
                let still = plain.filter(|st| same_file(st, id));
                anyhow::ensure!(
                    still.is_some(),
                    "the file read there was removed or replaced since"
                );
                still.map(|st| narrower(id.mode, permission_bits(&st)))
            }
        })
    }

    /// The narrower of two sets of permission bits: what both allow.
    /// `scripts/controls/skill-dir-write-narrower.row` switches this off.
    fn narrower(then: u32, now: u32) -> u32 {
        then & now
    }

    /// Whether `relative` below `root` is still what `expect` says, checked
    /// as a write there would check it, without writing.
    pub(super) fn check(root: &Path, relative: &Path, expect: Expect) -> anyhow::Result<()> {
        let name = c_name(relative.file_name().context("a file has a name")?)?;
        match find_chain(root, relative)? {
            Some(dir) => expected(&dir, &name, expect).map(drop),
            None => match expect {
                Expect::Same(_) => bail!("the file read there was removed or replaced since"),
                Expect::Any | Expect::Absent => Ok(()),
            },
        }
    }

    /// Put `contents` at `name` in `dir`: written to a temporary made new
    /// beside it, under a name no one could have prepared, then renamed over
    /// it, with the permission bits [`expected`] gives and, for a setup run as
    /// root over a file it read, that file's owner. Only the temporary this
    /// call made is ever removed.
    pub(super) fn replace(
        dir: &OwnedFd,
        name: &OsStr,
        contents: &[u8],
        expect: Expect,
    ) -> anyhow::Result<()> {
        let target = c_name(name)?;
        let mode = expected(dir, &target, expect)?;
        let (temp, made) = make_temp(dir.as_fd(), name, mode)?;
        // A setup run as root leaves the file it rewrote its owner's, as it
        // was, rather than root's.
        // SAFETY: geteuid takes nothing and cannot fail.
        #[allow(unsafe_code)]
        let root_runs = unsafe { libc::geteuid() } == 0;
        if let (true, Expect::Same(id)) = (root_runs, expect) {
            // SAFETY: `made` is an open descriptor this call owns.
            #[allow(unsafe_code)]
            let r = unsafe { libc::fchown(made.as_raw_fd(), id.uid, id.gid) };
            if r != 0 {
                let e = std::io::Error::last_os_error();
                // SAFETY: `temp` is valid for the call, and `dir` is open.
                #[allow(unsafe_code)]
                unsafe {
                    libc::unlinkat(dir.as_raw_fd(), temp.as_ptr(), 0);
                }
                return Err(e).context("give the file its owner back");
            }
        }
        let unmade = |e: anyhow::Error| {
            // SAFETY: `temp` is valid for the call, and `dir` is open.
            #[allow(unsafe_code)]
            unsafe {
                libc::unlinkat(dir.as_raw_fd(), temp.as_ptr(), 0);
            }
            e
        };
        let mut file = std::fs::File::from(made);
        let written = file.write_all(contents).and_then(|()| file.sync_all());
        drop(file);
        written.map_err(|e| unmade(e.into()))?;
        // SAFETY: both names are valid for the call, and `dir` is open.
        #[allow(unsafe_code)]
        let r = unsafe {
            libc::renameat(
                dir.as_raw_fd(),
                temp.as_ptr(),
                dir.as_raw_fd(),
                target.as_ptr(),
            )
        };
        if r < 0 {
            return Err(unmade(std::io::Error::last_os_error().into()));
        }
        Ok(())
    }

    /// A temporary for `name` made new in `dir`: `.{name}.{random}.atomic-writing`,
    /// tried again under another name while one is taken. When the file it
    /// replaces is there, with `mode`, it is made readable by its owner only
    /// and given that mode before anything is written to it, so what it holds
    /// is never readable by more than the file it replaces lets read.
    pub(super) fn make_temp(
        dir: BorrowedFd<'_>,
        name: &OsStr,
        mode: Option<u32>,
    ) -> anyhow::Result<(CString, OwnedFd)> {
        for _ in 0..TEMP_TRIES {
            let mut random = [0u8; 8];
            getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!("random name: {e}"))?;
            let mut temp = b".".to_vec();
            temp.extend_from_slice(name.as_bytes());
            temp.extend_from_slice(
                format!(".{:016x}.atomic-writing", u64::from_le_bytes(random)).as_bytes(),
            );
            let temp = CString::new(temp).context("a name holds a NUL byte")?;
            match create_new(dir, &temp, if mode.is_some() { 0o600 } else { 0o666 }) {
                Ok(fd) => {
                    let file = std::fs::File::from(fd);
                    if let Some(mode) = mode {
                        let given = file.set_permissions(std::fs::Permissions::from_mode(mode));
                        if let Err(e) = given {
                            // SAFETY: `temp` is valid for the call, and `dir` is open.
                            #[allow(unsafe_code)]
                            unsafe {
                                libc::unlinkat(dir.as_raw_fd(), temp.as_ptr(), 0);
                            }
                            return Err(e).context("give a temporary its file's permissions");
                        }
                    }
                    return Ok((temp, file.into()));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).context("make a temporary"),
            }
        }
        bail!("no temporary could be made new after {TEMP_TRIES} names")
    }

    /// Create `temp` in `dir` with `mode`, new and not through a link.
    pub(super) fn create_new(
        dir: BorrowedFd<'_>,
        temp: &CString,
        mode: libc::c_uint,
    ) -> std::io::Result<OwnedFd> {
        let flags = NEW_NOT_THROUGH_A_LINK.into_iter().fold(
            libc::O_WRONLY | libc::O_CREAT | libc::O_CLOEXEC,
            std::ops::BitOr::bitor,
        );
        openat(dir, temp, flags, mode)
    }
}

/// [`at`] where there are no directory handles: the same names and the same
/// order, resolved by path.
#[cfg(not(unix))]
mod at {
    use anyhow::Context;
    use std::ffi::OsStr;
    use std::io::Write as _;
    use std::path::{Path, PathBuf};

    pub(super) fn open_chain(root: &Path, relative: &Path) -> anyhow::Result<PathBuf> {
        let mut dir = root.to_owned();
        for part in relative.parent().into_iter().flat_map(Path::components) {
            dir.push(part);
            match std::fs::symlink_metadata(&dir) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    anyhow::bail!("{} is reached through a symbolic link", relative.display())
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir(&dir)?,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(dir)
    }

    pub(super) type Dir = PathBuf;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Kind {
        File,
        Dir,
        Link,
        Other,
    }

    pub(super) struct Entry {
        pub(super) kind: Kind,
        pub(super) len: u64,
    }

    pub(super) fn open_dirs(root: &Path, relative: &Path) -> anyhow::Result<Option<PathBuf>> {
        let dir = root.join(relative);
        Ok(dir.is_dir().then_some(dir))
    }

    pub(super) fn entry(dir: &Path, name: &OsStr) -> std::io::Result<Entry> {
        let meta = std::fs::symlink_metadata(dir.join(name))?;
        let kind = if meta.file_type().is_symlink() {
            Kind::Link
        } else if meta.is_dir() {
            Kind::Dir
        } else if meta.is_file() {
            Kind::File
        } else {
            Kind::Other
        };
        Ok(Entry {
            kind,
            len: meta.len(),
        })
    }

    pub(super) fn subdir(dir: &Path, name: &OsStr) -> anyhow::Result<PathBuf> {
        Ok(dir.join(name))
    }

    pub(super) fn list(dir: &Path) -> anyhow::Result<Vec<std::ffi::OsString>> {
        Ok(std::fs::read_dir(dir)?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<Result<_, _>>()?)
    }

    pub(super) fn read_in(dir: &Path, name: &OsStr, max: u64) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(super::read_at_most(&dir.join(name), max)?)
    }

    #[derive(Clone, Copy, Debug)]
    pub(super) struct FileId;

    pub(super) struct Read {
        pub(super) bytes: Vec<u8>,
        pub(super) id: FileId,
    }

    #[derive(Clone, Copy, Debug)]
    pub(super) enum Expect {
        Any,
        Absent,
        Same(FileId),
    }

    pub(super) fn read_plain(
        root: &Path,
        relative: &Path,
        max: u64,
    ) -> anyhow::Result<Option<Read>> {
        match super::read_at_most(&root.join(relative), max) {
            Ok(Some(bytes)) => Ok(Some(Read { bytes, id: FileId })),
            Ok(None) => anyhow::bail!("{} is larger than atomic reads there", relative.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub(super) fn check(root: &Path, relative: &Path, expect: Expect) -> anyhow::Result<()> {
        let there = root.join(relative).symlink_metadata().is_ok();
        match expect {
            Expect::Absent => anyhow::ensure!(!there, "a file was put there since it was read"),
            Expect::Same(FileId) => anyhow::ensure!(there, "the file read there was removed"),
            Expect::Any => {}
        }
        Ok(())
    }

    pub(super) fn remove(root: &Path, relative: &Path, dir: bool) -> anyhow::Result<bool> {
        let path = root.join(relative);
        let done = if dir {
            std::fs::remove_dir(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match done {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    pub(super) fn replace(
        dir: &Path,
        name: &OsStr,
        contents: &[u8],
        expect: Expect,
    ) -> anyhow::Result<()> {
        let there = dir.join(name).symlink_metadata().is_ok();
        match expect {
            Expect::Absent => anyhow::ensure!(!there, "a file was put there since it was read"),
            Expect::Same(FileId) => anyhow::ensure!(there, "the file read there was removed"),
            Expect::Any => {}
        }
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!("random name: {e}"))?;
        let temp = dir.join(format!(
            ".{}.{:016x}.atomic-writing",
            name.to_string_lossy(),
            u64::from_le_bytes(random)
        ));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .context("make a temporary")?;
        let done = file
            .write_all(contents)
            .and_then(|()| file.sync_all())
            .and_then(|()| std::fs::rename(&temp, dir.join(name)));
        if done.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        Ok(done?)
    }
}

/// Refuse to write `file`, which stands at `relative` below `root`, when it is a
/// symbolic link atomic may not write through: one that is broken, leads out of
/// the repository, or leads to something other than another of the files named
/// in `may_lead_to`. A link inside the repository is not enough on its own: what
/// it leads to may be the person's own work, or one of a skill's files, which
/// hold the pack rather than the instructions. Where two agents share one
/// instructions file the link leads to the other's, and that one is followed --
/// one link, to a plain file. A chain of two is refused, because what `install`
/// then writes through is a file that is itself an alias, which is the question
/// this check exists to ask and not an answer to it.
///
/// What comes back is where the file really is, below `root`: `relative`
/// itself, or the other file the one allowed link leads to, resolved once here
/// and read and written there.
fn link_to_own_file(
    root: &Path,
    relative: &Path,
    file: &Path,
    may_lead_to: &[PathBuf],
) -> anyhow::Result<PathBuf> {
    if !is_link(file) {
        return Ok(relative.to_owned());
    }
    let resolved = std::fs::canonicalize(file).with_context(|| {
        format!(
            "{} is a broken symbolic link",
            line(&relative.to_string_lossy())
        )
    })?;
    ensure!(
        resolved.starts_with(root),
        "{} is a symbolic link outside {}",
        line(&relative.to_string_lossy()),
        line(&root.to_string_lossy())
    );
    // The other file, by the canonical path the link resolves to: a path
    // that is that path is no link, so a chain of two links is refused.
    let leads_to = |path: &&PathBuf| {
        let other = root.join(path);
        path.as_path() != relative && other == resolved
    };
    let Some(target) = may_lead_to.iter().find(leads_to) else {
        bail!(
            "{} is a symbolic link to {}, which is not a file atomic may write through from \
             here; atomic writes this path plain, or through a link to the other agent's \
             instructions file",
            line(&relative.to_string_lossy()),
            line(&resolved.to_string_lossy())
        );
    };
    Ok(target.clone())
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

    /// A pack is built in this crate from paths this crate writes, so a path of
    /// it that names no skill directory is a bug here rather than something a
    /// person left behind. It is still refused where it is, because a pack
    /// nobody checked would write loose into the shared skills directory, where
    /// no directory atomic owns covers it.
    #[test]
    fn a_pack_that_writes_outside_a_skill_directory_is_refused() {
        let loose = [
            skills::SkillFile {
                path: "loose.md".to_owned(),
                contents: "no directory in front of it\n".to_owned(),
            },
            skills::SkillFile {
                path: "a-skill/".to_owned(),
                contents: "nothing after the slash\n".to_owned(),
            },
        ];
        for file in &loose {
            let error = owned_dirs(std::slice::from_ref(file)).expect_err("refused");
            assert_eq!(
                format!("{error:#}"),
                format!(
                    "the pack writes {}, which is not a file inside a skill directory",
                    file.path
                )
            );
        }
    }

    /// The bounds setup refuses by size have to sit above what atomic writes
    /// itself, or the check would refuse atomic's own work: every file of the
    /// pack under the bound a listed file is read to, and the record of a whole
    /// skill under the bound a manifest is read to.
    #[test]
    fn pack_files_fit_their_limits() {
        let pack = skills::pack().expect("pack");
        for file in &pack {
            assert!(
                file.contents.len() as u64 <= READ_BYTES_MAX,
                "{} holds {} bytes, over the {} setup will read a file for",
                file.path,
                file.contents.len(),
                READ_BYTES_MAX
            );
        }
        for skill in owned_dirs(&pack).expect("the pack's skill directories") {
            let bytes = Manifest::for_pack(&pack, &skill).text().len();
            assert!(
                bytes as u64 <= MANIFEST_BYTES_MAX,
                "the record of {skill} holds {bytes} bytes, over the {MANIFEST_BYTES_MAX} a \
                 manifest may hold"
            );
        }
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

    /// What an upgrade leaves when it takes a whole nested directory of the old
    /// pack out: the directory stands empty, this pack has no reason to write in
    /// it, and the next setup would refuse the directory it had just made. The
    /// directories a removal empties go with the files they held, innermost
    /// first; one that still holds something stays.
    #[test]
    fn a_directory_emptied_by_an_upgrade_goes_with_the_files_it_held() {
        let root = scratch("emptied");
        install(Agent::Codex, &root).expect("installed");
        let skill = skill_dir(&root, Agent::Codex);
        let old = skill.join("references/old/deep");
        std::fs::create_dir_all(&old).expect("the old pack's nested directory");
        let gone = "<!-- a reference this release no longer ships -->\n";
        std::fs::write(old.join("gone.md"), gone).expect("write");
        let mut manifest = manifest_of(&root, Agent::Codex);
        manifest
            .files
            .insert("references/old/deep/gone.md".to_owned(), sha256(gone));
        std::fs::write(skill.join(MANIFEST), manifest.text()).expect("write the record");
        install(Agent::Codex, &root).expect("the upgrade installs");
        assert!(
            !skill.join("references/old").exists(),
            "the empty shell of the old pack's directory stayed"
        );
        assert!(
            skill.join("references/fields.md").is_file(),
            "the pack's own reference went with it"
        );
        // The next setup is what an up-to-date install meets, and it has
        // nothing there to refuse.
        install(Agent::Codex, &root).expect("the next setup installs");
    }

    /// The other half of the same shell: a directory of the old pack that is
    /// empty on disk while the record still speaks of a file in it. What keeps
    /// such a directory is what stands in it, not what a manifest once named, so
    /// it is refused -- and cleared on the spot, because an empty directory
    /// shows a person nothing to go and remove.
    #[test]
    fn an_empty_directory_the_record_still_lists_is_refused_and_cleared() {
        let root = scratch("shell");
        install(Agent::Codex, &root).expect("installed");
        let skill = skill_dir(&root, Agent::Codex);
        std::fs::create_dir(skill.join("extras")).expect("the shell of a directory");
        let wrote = "what an earlier release wrote there\n";
        let mut manifest = manifest_of(&root, Agent::Codex);
        manifest
            .files
            .insert("extras/gone.md".to_owned(), sha256(wrote));
        std::fs::write(skill.join(MANIFEST), manifest.text()).expect("write the record");
        let error = install(Agent::Codex, &root).expect_err("refused");
        assert_eq!(
            format!("{error:#}"),
            ".agents/skills/atomic-create-atom holds what atomic did not write (an older \
             version of atomic may be the cause): \
             .agents/skills/atomic-create-atom/extras is not a directory atomic writes; remove \
             what is named and run `atomic setup` again \
             (.agents/skills/atomic-create-atom/extras was empty and is removed)"
        );
        assert!(
            !skill.join("extras").exists(),
            "the empty shell was left to be refused again"
        );
        // Nothing else moved: the refusal wrote no pack file and no record.
        assert_eq!(
            manifest_of(&root, Agent::Codex).files.get("extras/gone.md"),
            Some(&sha256(wrote)),
            "the record was rewritten over the refusal"
        );
        for file in skills::pack().expect("pack") {
            assert_eq!(
                std::fs::read(root.join(Agent::Codex.skills_dir()).join(&file.path)).expect("read"),
                file.contents.as_bytes(),
                "{} was written over",
                file.path
            );
        }
        // And the setup after it has nothing to refuse.
        install(Agent::Codex, &root).expect("the next setup installs");
    }

    /// An empty directory atomic cannot account for, one no record lists a
    /// path under, such as one a person made for drafts, is refused and left
    /// where it is: it is not atomic's to take away.
    #[test]
    fn an_empty_directory_no_record_accounts_for_is_refused_and_kept() {
        let root = scratch("person-dir");
        install(Agent::Codex, &root).expect("installed");
        let skill = skill_dir(&root, Agent::Codex);
        std::fs::create_dir_all(skill.join("drafts")).expect("a person's directory");
        let error = install(Agent::Codex, &root).expect_err("refused");
        let text = format!("{error:#}");
        assert!(
            text.contains("atomic-create-atom/drafts is not a directory atomic writes")
                && !text.contains("is removed"),
            "{text}"
        );
        assert!(
            skill.join("drafts").is_dir(),
            "a person's directory was removed"
        );
    }

    /// Clearing empty shells is the remedy only when it is the whole complaint.
    /// With anything else refused as well, setup installs nothing and leaves
    /// every directory where it found it, empty ones included: what it takes away
    /// is never what a person might still have wanted.
    #[test]
    fn an_empty_directory_stays_while_something_else_is_refused() {
        let root = scratch("shell-and-file");
        install(Agent::Codex, &root).expect("installed");
        let skill = skill_dir(&root, Agent::Codex);
        std::fs::create_dir(skill.join("extras")).expect("the shell of a directory");
        std::fs::write(skill.join("mine.md"), "mine\n").expect("write");
        let mut manifest = manifest_of(&root, Agent::Codex);
        manifest
            .files
            .insert("extras/gone.md".to_owned(), sha256("x"));
        std::fs::write(skill.join(MANIFEST), manifest.text()).expect("write the record");
        let error = install(Agent::Codex, &root).expect_err("refused");
        let text = format!("{error:#}");
        assert!(
            text.contains("mine.md is not a file atomic writes")
                && text.contains("extras is not a directory atomic writes"),
            "{text}"
        );
        assert!(
            !text.contains("is removed"),
            "setup cleared a directory while something else was refused too: {text}"
        );
        assert!(skill.join("extras").is_dir(), "the empty shell was removed");
        assert_eq!(
            std::fs::read_to_string(skill.join("mine.md")).expect("theirs"),
            "mine\n"
        );
    }

    /// A run that stopped between writing the files and writing its record
    /// leaves this pack's bytes under the record of the run before it. Those
    /// bytes are atomic's own work, wherever they came from, so running setup
    /// again finishes the install rather than refusing the directory the
    /// interrupted run had just written.
    #[test]
    fn an_interrupted_upgrade_is_finished_by_running_setup_again() {
        let root = scratch("halfway");
        install(Agent::Codex, &root).expect("installed");
        let skill = skill_dir(&root, Agent::Codex);
        // The record of a release before this one, which is what the run that
        // wrote the files and then stopped leaves standing.
        let mut older = manifest_of(&root, Agent::Codex);
        for recorded in older.files.values_mut() {
            *recorded = sha256(format!("what an older release wrote: {recorded}"));
        }
        std::fs::write(skill.join(MANIFEST), older.text()).expect("write the older record");
        install(Agent::Codex, &root).expect("the run after the interruption installs");
        let pack = skills::pack().expect("pack");
        let under = format!("{SKILL}/");
        let now = manifest_of(&root, Agent::Codex);
        for file in &pack {
            let relative = file
                .path
                .strip_prefix(under.as_str())
                .expect("under the skill");
            assert_eq!(
                now.files.get(relative),
                Some(&sha256(&file.contents)),
                "{} is not recorded as what was written",
                file.path
            );
            assert_eq!(
                std::fs::read(root.join(Agent::Codex.skills_dir()).join(&file.path)).expect("read"),
                file.contents.as_bytes(),
                "{}",
                file.path
            );
        }
    }

    /// The order to remove directories in: innermost first, so that emptying one
    /// is what lets the one above it go as well.
    #[test]
    fn a_paths_directories_are_named_innermost_first() {
        assert_eq!(parents_of("a/b/c.md"), ["a/b", "a"]);
        assert_eq!(parents_of("a.md"), Vec::<&str>::new());
        assert_eq!(parents_of("a/b"), ["a"]);
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

    /// The same directory with nothing added to it and nothing changed is what
    /// atomic is willing to take in without a record: bytes this atomic was
    /// going to write there anyway are atomic's own work, whoever put them
    /// there. The refusal above is what anything else costs.
    #[test]
    fn a_directory_written_without_a_record_that_holds_the_pack_installs() {
        let root = scratch("no-record");
        let skills_dir = root.join(Agent::Codex.skills_dir());
        // What a release that kept no record leaves: the pack's files, with the
        // pack's bytes, and no manifest beside them.
        for file in skills::pack().expect("pack") {
            let at = skills_dir.join(&file.path);
            std::fs::create_dir_all(at.parent().expect("a parent")).expect("a directory");
            std::fs::write(&at, &file.contents).expect("write");
        }
        install(Agent::Codex, &root).expect("the pack's own bytes install without a record");
        assert!(
            std::fs::read_to_string(root.join("AGENTS.md"))
                .expect("AGENTS.md")
                .contains("`atomic-create-atom` skill"),
            "the agent was not told to use the skill"
        );
        // And the directory now carries the record that makes the next upgrade
        // an upgrade rather than a clean-up.
        assert_eq!(
            manifest_of(&root, Agent::Codex).files.get("SKILL.md"),
            Some(&sha256(&skills::pack().expect("pack")[0].contents))
        );
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
            (
                // A key of its own making that both names somewhere else and
                // carries a line break, which is what a refusal must not repeat
                // unescaped.
                "newline-in-a-key",
                format!("{{\"format\":1,\"files\":{{\"../evil\\nelsewhere.md\":\"{zeros}\"}}}}")
                    .into_bytes(),
                // `line` writes the break as `\n`, so this is the escaped key.
                "lists ../evil\\nelsewhere.md as",
            ),
        ] {
            let root = scratch("manifest");
            install(Agent::Codex, &root).expect("installed");
            std::fs::write(skill_dir(&root, Agent::Codex).join(MANIFEST), &text)
                .expect("write the impostor");
            let error = format!("{:#}", install(Agent::Codex, &root).expect_err("refused"));
            assert!(
                error.contains(&format!("{MANIFEST} {says}")),
                "{what}: {error}"
            );
            assert_eq!(
                error.lines().count(),
                1,
                "{what}: what the manifest supplied was repeated unescaped: {error}"
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
        let error = format!("{:#}", install(Agent::Codex, &root).expect_err("refused"));
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

    /// Two names that differ only in bytes which are not text are one name once
    /// read as text. An entry weighed on a name another entry is called by can
    /// hide that other entry, so a name atomic cannot read as text is refused
    /// where it stands, and every entry is weighed on its own path.
    #[cfg(unix)]
    #[test]
    fn a_name_that_is_not_text_is_refused_and_hides_nothing() {
        use std::os::unix::ffi::OsStrExt as _;
        let root = scratch("not-text");
        install(Agent::Codex, &root).expect("installed");
        let skill = skill_dir(&root, Agent::Codex);
        // The name that is text, which the record lists as atomic's own, and a
        // second entry whose bytes read as that same name taken lossily.
        let theirs = "what the record vouches for\n";
        std::fs::write(skill.join("n\u{fffd}.md"), theirs).expect("write the name that is text");
        let foreign = std::ffi::OsStr::from_bytes(b"n\xff.md");
        std::fs::write(skill.join(foreign), "what someone else left\n")
            .expect("write the name that is not text");
        let mut manifest = manifest_of(&root, Agent::Codex);
        manifest
            .files
            .insert("n\u{fffd}.md".to_owned(), sha256(theirs));
        std::fs::write(skill.join(MANIFEST), manifest.text()).expect("write the record");
        let error = install(Agent::Codex, &root).expect_err("refused");
        assert_eq!(
            format!("{error:#}"),
            ".agents/skills/atomic-create-atom holds what atomic did not write (an older \
             version of atomic may be the cause): \
             .agents/skills/atomic-create-atom/n\u{fffd}.md is not a name atomic can read as \
             text; remove what is named and run `atomic setup` again"
        );
        // Everything is left as it was found: the entry that shares the name,
        // the one that does not, and the record that lists only the first.
        assert_eq!(
            std::fs::read_to_string(skill.join("n\u{fffd}.md")).expect("theirs"),
            theirs
        );
        assert_eq!(
            std::fs::read(skill.join(foreign)).expect("theirs"),
            b"what someone else left\n",
            "the entry with the name that is not text was written over"
        );
        assert_eq!(
            manifest_of(&root, Agent::Codex).files.get("n\u{fffd}.md"),
            Some(&sha256(theirs)),
            "the record was rewritten over the refusal"
        );
        for file in skills::pack().expect("pack") {
            assert_eq!(
                std::fs::read(root.join(Agent::Codex.skills_dir()).join(&file.path)).expect("read"),
                file.contents.as_bytes(),
                "{} was written over",
                file.path
            );
        }
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
        let error = format!("{:#}", install(Agent::Codex, &root).expect_err("refused"));
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
        let error = format!("{:#}", install(Agent::Codex, &root).expect_err("refused"));
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

    /// One link, to a plain file, is all the instructions file may be. A chain
    /// is refused: the second hop is a file that is itself an alias, which is
    /// the question this check asks and not an answer to it, and what is written
    /// then goes wherever the chain ends rather than to a named file.
    #[cfg(unix)]
    #[test]
    fn a_link_from_the_instructions_file_to_another_link_is_refused() {
        let root = scratch("chain-of-two");
        std::fs::create_dir_all(root.join(".claude")).expect("claude directory");
        std::fs::write(root.join(".claude/shared.md"), "# Shared\n").expect("write");
        std::os::unix::fs::symlink(".claude/shared.md", root.join("AGENTS.md"))
            .expect("the first link");
        std::os::unix::fs::symlink("AGENTS.md", root.join("CLAUDE.md")).expect("the second link");
        let error = install(Agent::Claude, &root).expect_err("refused");
        let text = format!("{error:#}");
        assert!(
            text.contains("CLAUDE.md is a symbolic link to")
                && text.contains(".claude/shared.md")
                && text.contains("which is not a file atomic may write through from here"),
            "{text}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join(".claude/shared.md")).expect("theirs"),
            "# Shared\n",
            "the instructions were written at the end of the chain"
        );
        assert!(
            std::fs::symlink_metadata(root.join("AGENTS.md"))
                .expect("meta")
                .file_type()
                .is_symlink(),
            "the link in the middle of the chain was replaced"
        );
        assert!(
            !skill_dir(&root, Agent::Claude).join("SKILL.md").is_file(),
            "the skill was installed despite the refusal"
        );
    }

    /// A write is confined to the repository it was pointed at: a directory in
    /// its way that leads out of that repository is refused, and nothing is put
    /// where the link leads.
    #[cfg(unix)]
    #[test]
    fn a_write_refuses_a_directory_that_leads_out_of_the_repository() {
        let root = std::fs::canonicalize(scratch("write-out")).expect("canonical root");
        let outside = scratch("write-out-target");
        std::fs::create_dir_all(root.join(".agents/skills")).expect("skills");
        std::os::unix::fs::symlink(&outside, root.join(".agents/skills/atomic-create-atom"))
            .expect("link");
        let error = write_inside(
            &root,
            Path::new(".agents/skills/atomic-create-atom/SKILL.md"),
            b"the pack's bytes",
        )
        .expect_err("refused");
        assert_eq!(
            format!("{error:#}"),
            ".agents/skills/atomic-create-atom/SKILL.md is reached through \
             .agents/skills/atomic-create-atom, which is a symbolic link"
        );
        assert!(
            std::fs::read_dir(&outside)
                .expect("outside")
                .next()
                .is_none(),
            "the bytes were written where the link leads"
        );
    }

    /// A link inside the repository is no place to write either, and it is
    /// refused on the way to the file, however it came to be standing there: a
    /// directory put in place after any earlier weighing cannot move the write
    /// somewhere else.
    #[cfg(unix)]
    #[test]
    fn a_write_refuses_a_directory_reached_through_a_link() {
        let root = std::fs::canonicalize(scratch("write-chain")).expect("canonical root");
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("elsewhere");
        std::fs::create_dir_all(root.join(".agents/skills")).expect("skills");
        std::os::unix::fs::symlink(
            "../../elsewhere",
            root.join(".agents/skills/atomic-create-atom"),
        )
        .expect("link");
        let error = write_inside(
            &root,
            Path::new(".agents/skills/atomic-create-atom/SKILL.md"),
            b"the pack's bytes",
        )
        .expect_err("refused");
        assert_eq!(
            format!("{error:#}"),
            ".agents/skills/atomic-create-atom/SKILL.md is reached through \
             .agents/skills/atomic-create-atom, which is a symbolic link"
        );
        assert!(
            std::fs::read_dir(&elsewhere)
                .expect("elsewhere")
                .next()
                .is_none(),
            "the bytes were written where the link leads"
        );
    }

    /// A temporary is made new and never through a link: a link standing at
    /// the name a temporary is to take is refused, and what it leads to is
    /// left as it was.
    #[cfg(unix)]
    #[test]
    fn a_temporary_is_never_made_through_a_link_at_its_name() {
        let root = std::fs::canonicalize(scratch("temp-link")).expect("canonical root");
        let theirs = root.join("theirs");
        std::fs::write(&theirs, "theirs\n").expect("theirs");
        std::os::unix::fs::symlink("theirs", root.join("planted")).expect("link");
        let dir: std::os::fd::OwnedFd = std::fs::File::open(&root).expect("open").into();
        let name = std::ffi::CString::new("planted").expect("name");
        let made = at::create_new(std::os::fd::AsFd::as_fd(&dir), &name, 0o600);
        assert!(made.is_err(), "a temporary was made through the link");
        assert_eq!(
            std::fs::read_to_string(&theirs).expect("theirs"),
            "theirs\n"
        );
    }

    /// A repository that ships links where an earlier atomic put its
    /// temporaries, beside the instructions file and beside a pack file,
    /// sends nothing through them: setup writes plain files at their own
    /// names, and what the links lead to is left as it was.
    #[cfg(unix)]
    #[test]
    fn links_where_temporaries_once_went_are_never_written_through() {
        let root = std::fs::canonicalize(scratch("temp-names")).expect("canonical root");
        let theirs = root.join("theirs");
        std::fs::write(&theirs, "theirs\n").expect("theirs");
        std::fs::write(root.join("AGENTS.md"), "# Notes\n").expect("instructions");
        std::os::unix::fs::symlink("theirs", root.join("AGENTS.atomic-writing")).expect("link");
        install(Agent::Codex, &root).expect("installed");
        assert!(!is_link(&root.join("AGENTS.md")), "AGENTS.md became a link");
        assert!(
            std::fs::read_to_string(root.join("AGENTS.md"))
                .expect("read")
                .contains("# Notes"),
            "the instructions file lost its text"
        );
        assert_eq!(
            std::fs::read_to_string(&theirs).expect("theirs"),
            "theirs\n"
        );
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .expect("root")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".atomic-writing") && n != "AGENTS.atomic-writing")
            .collect();
        assert!(leftovers.is_empty(), "temporaries were left: {leftovers:?}");
    }

    /// The instructions file is read through the directories held open on
    /// the way and never through a link: one swapped in after it was weighed,
    /// leading to a key of the person's, is refused, not read into the file.
    #[cfg(unix)]
    #[test]
    fn a_file_is_never_read_through_a_link() {
        let root = std::fs::canonicalize(scratch("read-link")).expect("canonical root");
        std::fs::write(root.join("secret"), "a key\n").expect("secret");
        std::os::unix::fs::symlink("secret", root.join("AGENTS.md")).expect("link");
        let refused = at::read_plain(&root, Path::new("AGENTS.md"), 1024)
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert_eq!(
            refused,
            "AGENTS.md is a symbolic link, and atomic never reads through one"
        );
    }

    /// A file is removed by its name in the directories held open on the way:
    /// a directory above it swapped for a link to somewhere else is refused,
    /// and what the link leads to keeps its file.
    #[cfg(unix)]
    #[test]
    fn a_removal_refuses_a_directory_reached_through_a_link() {
        let root = std::fs::canonicalize(scratch("remove-chain")).expect("canonical root");
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("elsewhere");
        std::fs::write(elsewhere.join("old.md"), "theirs\n").expect("theirs");
        std::fs::create_dir_all(root.join(".agents/skills/atomic-create-atom")).expect("skill");
        std::os::unix::fs::symlink(
            "../../../elsewhere",
            root.join(".agents/skills/atomic-create-atom/references"),
        )
        .expect("link");
        let refused = at::remove(
            &root,
            Path::new(".agents/skills/atomic-create-atom/references/old.md"),
            false,
        )
        .err()
        .map(|e| format!("{e:#}"))
        .unwrap_or_default();
        assert_eq!(
            refused,
            ".agents/skills/atomic-create-atom/references/old.md is reached through \
             .agents/skills/atomic-create-atom/references, which is a symbolic link"
        );
        assert!(
            elsewhere.join("old.md").is_file(),
            "the file the link led to went"
        );
    }

    /// A temporary that replaces a file already has that file's permissions
    /// before a byte is written to it: what it holds is never readable by
    /// more than the file it replaces lets read.
    #[cfg(unix)]
    #[test]
    fn a_temporary_has_its_files_permissions_before_it_is_written() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::fs::canonicalize(scratch("temp-mode")).expect("canonical root");
        let dir: std::os::fd::OwnedFd = std::fs::File::open(&root).expect("open").into();
        let (temp, made) = at::make_temp(
            std::os::fd::AsFd::as_fd(&dir),
            std::ffi::OsStr::new("AGENTS.md"),
            Some(0o640),
        )
        .expect("a temporary");
        let meta = std::fs::File::from(made).metadata().expect("meta");
        let _ = std::fs::remove_file(root.join(temp.to_str().expect("text")));
        assert_eq!(meta.len(), 0, "the temporary was written to");
        assert_eq!(meta.permissions().mode() & 0o7777, 0o640);
    }

    /// Run `read` on a fifo with no writer, which a read that waits would wait
    /// on forever: it must come back, refused, at once.
    #[cfg(unix)]
    fn read_of_a_fifo_comes_back(read: fn(&Path) -> bool) {
        let root = std::fs::canonicalize(scratch("fifo")).expect("canonical root");
        let fifo = root.join("AGENTS.md");
        let name = std::ffi::CString::new(fifo.to_str().expect("text")).expect("name");
        // SAFETY: `name` is a valid NUL-terminated path for the call.
        #[allow(unsafe_code)]
        let made = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
        assert_eq!(made, 0, "mkfifo");
        let (send, came) = std::sync::mpsc::channel();
        std::thread::spawn(move || send.send(read(&fifo)));
        let refused = came
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the read of a fifo waited");
        assert!(refused, "a fifo was read as a file");
    }

    /// A file atomic writes carries its permission bits and never setuid,
    /// setgid or sticky, whatever the file it replaces had: `sudo atomic
    /// setup` over a planted setuid file leaves a plain one.
    #[cfg(unix)]
    #[test]
    fn a_replaced_files_special_bits_are_never_kept() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::fs::canonicalize(scratch("special-bits")).expect("canonical root");
        let file = root.join("AGENTS.md");
        std::fs::write(&file, "# Notes\n").expect("file");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o7755)).expect("mode");
        let read = at::read_plain(&root, Path::new("AGENTS.md"), 1024)
            .expect("read")
            .expect("there");
        assert_eq!(at::mode_of(read.id), 0o755);
    }

    /// The instructions file a write replaces must still be the file that was
    /// read: one removed since, so that its permissions would be lost, one
    /// put in its place, or one put where there was none, is refused, and
    /// nothing is written.
    #[cfg(unix)]
    #[test]
    fn the_instructions_file_must_still_be_the_one_read() {
        let root = std::fs::canonicalize(scratch("same-file")).expect("canonical root");
        let file = root.join("AGENTS.md");
        let read = |root: &Path| {
            at::read_plain(root, Path::new("AGENTS.md"), 1024)
                .expect("read")
                .expect("there")
                .id
        };
        let write = |expect| {
            write_expecting(&root, Path::new("AGENTS.md"), b"new\n", expect)
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_default()
        };
        std::fs::write(&file, "theirs\n").expect("file");
        let id = read(&root);
        std::fs::remove_file(&file).expect("removed");
        assert_eq!(
            write(at::Expect::Same(id)),
            "write AGENTS.md: the file read there was removed or replaced since"
        );
        assert!(
            !file.exists(),
            "a file was written in place of the one removed"
        );
        std::fs::write(&file, "theirs\n").expect("file");
        let id = read(&root);
        std::fs::write(root.join("other"), "another\n").expect("other");
        std::fs::rename(root.join("other"), &file).expect("swapped");
        assert_eq!(
            write(at::Expect::Same(id)),
            "write AGENTS.md: the file read there was removed or replaced since"
        );
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "another\n");
        assert_eq!(
            write(at::Expect::Absent),
            "write AGENTS.md: a file was put there since it was read"
        );
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "another\n");
    }

    /// An owner who narrowed the instructions file's permissions between the
    /// read and the write keeps the narrower ones; and a file removed since
    /// the read is refused by the check made before anything is written.
    #[cfg(unix)]
    #[test]
    fn the_narrower_permissions_win_and_the_file_is_checked_before_the_pack() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::fs::canonicalize(scratch("narrower")).expect("canonical root");
        let file = root.join("AGENTS.md");
        std::fs::write(&file, "theirs\n").expect("file");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).expect("mode");
        let id = at::read_plain(&root, Path::new("AGENTS.md"), 1024)
            .expect("read")
            .expect("there")
            .id;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("mode");
        write_expecting(
            &root,
            Path::new("AGENTS.md"),
            b"new\n",
            at::Expect::Same(id),
        )
        .expect("written");
        let mode = std::fs::metadata(&file).expect("meta").permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600);
        let id = at::read_plain(&root, Path::new("AGENTS.md"), 1024)
            .expect("read")
            .expect("there")
            .id;
        std::fs::remove_file(&file).expect("removed");
        let refused = at::check(&root, Path::new("AGENTS.md"), at::Expect::Same(id))
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert_eq!(refused, "the file read there was removed or replaced since");
    }

    /// A file with other names, as a hard link to someone's file has, is not
    /// read into the instructions file.
    #[cfg(unix)]
    #[test]
    fn an_instructions_file_with_other_names_is_refused() {
        let root = std::fs::canonicalize(scratch("hard-link")).expect("canonical root");
        std::fs::write(root.join("theirs"), "a key\n").expect("theirs");
        std::fs::hard_link(root.join("theirs"), root.join("AGENTS.md")).expect("link");
        let refused = at::read_plain(&root, Path::new("AGENTS.md"), 1024)
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert_eq!(
            refused,
            "AGENTS.md has other names, as a hard link does, and atomic reads only a file that \
             is its own"
        );
    }

    /// The check walks the skill directory through directories it holds
    /// open: one swapped for a link elsewhere after it was opened is still
    /// the one listed, a directory reached through a link is refused when it
    /// is opened, and so is a link standing where a directory was seen.
    #[cfg(unix)]
    #[test]
    fn the_check_walks_through_held_directories_and_never_a_link() {
        let root = std::fs::canonicalize(scratch("check-held")).expect("canonical root");
        let skill = root.join(".agents/skills/atomic-create-atom");
        std::fs::create_dir_all(skill.join("references")).expect("skill");
        std::fs::write(skill.join("SKILL.md"), "ours\n").expect("ours");
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(elsewhere.join("skills/atomic-create-atom")).expect("elsewhere");
        std::fs::write(elsewhere.join("skills/atomic-create-atom/theirs.md"), "x").expect("theirs");
        let held = at::open_dirs(&root, Path::new(".agents/skills/atomic-create-atom"))
            .expect("opened")
            .expect("there");
        // `.agents` swapped for a link to elsewhere while the check runs.
        std::fs::rename(root.join(".agents"), root.join(".agents-before")).expect("moved");
        std::os::unix::fs::symlink("elsewhere", root.join(".agents")).expect("link");
        let mut names = at::list(&held).expect("listed");
        names.sort();
        assert_eq!(names, vec!["SKILL.md", "references"]);
        let refused = at::open_dirs(&root, Path::new(".agents/skills/atomic-create-atom"))
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert_eq!(
            refused,
            ".agents/skills/atomic-create-atom is reached through .agents, which is a symbolic \
             link"
        );
        // A link where the walk saw a directory is not gone into.
        std::fs::remove_dir(root.join(".agents-before/skills/atomic-create-atom/references"))
            .expect("emptied");
        std::os::unix::fs::symlink(
            elsewhere.join("skills"),
            root.join(".agents-before/skills/atomic-create-atom/references"),
        )
        .expect("link");
        assert!(
            at::subdir(&held, std::ffi::OsStr::new("references")).is_err(),
            "a link was gone into as a directory"
        );
    }

    /// Only a name that is not there reads as absent on the check's walk:
    /// any other error opening a directory on the way refuses.
    #[cfg(unix)]
    #[test]
    fn the_check_walk_refuses_every_error_but_not_there() {
        let root = std::fs::canonicalize(scratch("check-errors")).expect("canonical root");
        assert!(
            at::open_dirs(&root, Path::new(".agents/skills"))
                .expect("walked")
                .is_none(),
            "a directory not there"
        );
        let long = "a".repeat(300);
        let refused = at::open_dirs(&root, &Path::new(&long).join("skills"))
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(refused.contains("File name too long"), "{refused:?}");
        std::fs::write(root.join(".agents"), "").expect("a file");
        let refused = at::open_dirs(&root, Path::new(".agents/skills"))
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert_eq!(refused, ".agents is not a directory");
    }

    /// A file in a held directory is never read through a link, even one put
    /// there after the walk saw a plain file at its name.
    #[cfg(unix)]
    #[test]
    fn a_file_in_a_held_directory_is_never_read_through_a_link() {
        let root = std::fs::canonicalize(scratch("read-in-link")).expect("canonical root");
        std::fs::write(root.join("secret"), "a key\n").expect("secret");
        std::os::unix::fs::symlink("secret", root.join("x")).expect("link");
        let dir: std::os::fd::OwnedFd = std::fs::File::open(&root).expect("open").into();
        let read = at::read_in(&dir, std::ffi::OsStr::new("x"), 1024);
        let errno = read
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<std::io::Error>())
            .and_then(std::io::Error::raw_os_error);
        assert_eq!(errno, Some(libc::ELOOP), "{:?}", read.map(|_| "read"));
    }

    /// Nor is a file with other names, as a hard link has, read and hashed
    /// by one of them.
    #[cfg(unix)]
    #[test]
    fn a_file_in_a_held_directory_with_other_names_is_refused() {
        let root = std::fs::canonicalize(scratch("read-in-names")).expect("canonical root");
        std::fs::write(root.join("theirs"), "a key\n").expect("theirs");
        std::fs::hard_link(root.join("theirs"), root.join("x")).expect("hard link");
        let dir: std::os::fd::OwnedFd = std::fs::File::open(&root).expect("open").into();
        let refused = at::read_in(&dir, std::ffi::OsStr::new("x"), 1024)
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert_eq!(
            refused,
            "it has other names, as a hard link does, and atomic reads only a file that is its own"
        );
        std::fs::remove_file(root.join("theirs")).expect("one name left");
        let read = at::read_in(&dir, std::ffi::OsStr::new("x"), 1024).expect("read");
        assert_eq!(read.as_deref(), Some(&b"a key\n"[..]));
    }

    /// The instructions file's read does not wait on a fifo standing there.
    #[cfg(unix)]
    #[test]
    fn a_fifo_for_the_instructions_file_is_refused_without_waiting() {
        read_of_a_fifo_comes_back(|fifo| {
            let root = fifo.parent().expect("root");
            at::read_plain(root, Path::new("AGENTS.md"), 1024)
                .is_err_and(|e| format!("{e:#}") == "AGENTS.md is not a plain file")
        });
    }

    /// Nor does the read of a skill directory's file, swapped for a fifo after
    /// the walk saw a file there.
    #[cfg(unix)]
    #[test]
    fn a_fifo_for_a_skill_file_is_refused_without_waiting() {
        read_of_a_fifo_comes_back(|fifo| {
            let dir: std::os::fd::OwnedFd = std::fs::File::open(fifo.parent().expect("dir"))
                .expect("open")
                .into();
            at::read_in(&dir, std::ffi::OsStr::new("AGENTS.md"), 1024)
                .is_err_and(|e| format!("{e:#}") == "not a plain file")
        });
    }

    /// A pack path that is not plain names no file inside a skill directory.
    #[test]
    fn a_pack_path_that_is_not_plain_is_refused() {
        for path in ["atomic-create-atom/../x", "atomic-create-atom//x"] {
            let pack = vec![skills::SkillFile {
                path: path.to_owned(),
                contents: String::new(),
            }];
            let refused = owned_dirs(&pack)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert!(
                refused.contains("which is not a plain path"),
                "{path}: {refused}"
            );
        }
    }

    /// The file a write replaces keeps its permissions: an instructions file
    /// a person kept to themselves stays so.
    #[cfg(unix)]
    #[test]
    fn a_replaced_file_keeps_its_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::fs::canonicalize(scratch("keep-mode")).expect("canonical root");
        let file = root.join("AGENTS.md");
        std::fs::write(&file, "# Notes\n").expect("instructions");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("mode");
        install(Agent::Codex, &root).expect("installed");
        let mode = std::fs::metadata(&file).expect("meta").permissions().mode() & 0o7777;
        assert_eq!(mode, 0o600);
    }

    /// What stands where a file belongs is replaced, never written through: the
    /// bytes go to a temporary beside the target and the temporary is renamed
    /// over it, which takes the link away, leaves a plain file, and does not
    /// touch what the link led to.
    #[cfg(unix)]
    #[test]
    fn a_link_where_a_file_belongs_is_replaced_by_a_plain_file() {
        let root = std::fs::canonicalize(scratch("write-replace")).expect("canonical root");
        let target = root.join("notes.md");
        std::fs::write(&target, "what the link led to\n").expect("write");
        std::fs::create_dir_all(root.join(".agents/skills/atomic-create-atom")).expect("skill");
        let file = root.join(".agents/skills/atomic-create-atom/SKILL.md");
        std::os::unix::fs::symlink("../../notes.md", &file).expect("link");
        write_inside(
            &root,
            Path::new(".agents/skills/atomic-create-atom/SKILL.md"),
            b"the pack's bytes",
        )
        .expect("writes");
        assert!(!is_link(&file), "the link still stands where the file is");
        assert_eq!(std::fs::read(&file).expect("read"), b"the pack's bytes");
        assert_eq!(
            std::fs::read_to_string(&target).expect("theirs"),
            "what the link led to\n",
            "the write went through the link"
        );
    }
}
