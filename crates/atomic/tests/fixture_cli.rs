// SPDX-License-Identifier: Apache-2.0

//! A coding run through the `atomic` executable, bound to a fixture
//! repository: its worktree step runs a Codex atom under the real
//! `workspace-write` sandbox at the top of a worktree of the fixture's
//! snapshot, wherever the project is (outside any repository, or in a
//! subdirectory of the developer's), and atomic commits there. The
//! developer's checkout and the fixture repository itself are left exactly
//! as they were — compared file by file and byte by byte, as
//! `repository_state` does, and not only as git reports them. A read-only
//! Codex atom named directly is the one Codex turn whose working directory
//! is the developer's checkout, and it too changes nothing there; nothing
//! the fixture's configuration names (hooks, a file system monitor, filter
//! drivers, remotes) ever runs; and a run killed after one step goes on from
//! the snapshot after its source is gone, each step's commit on the last.
//!
//! Those runs need bubblewrap and unprivileged user namespaces, so each one is
//! built on Linux with the `sandbox-tests` feature and nowhere else: it is
//! marked `needs_sandbox` and asks for the sandbox, so with the feature on a
//! sandbox that cannot start fails it. What asks for the sandbox and what does
//! not is what scripts/backends-check.sh counts on: the tests it names are
//! exactly the tests the sandbox is for, and each must fail naming it. Six
//! tests ask for no sandbox, because nothing they run reaches a turn: the two
//! `target.unsafe` refusals, which are made while a run is loaded, and the four
//! checks of that comparison itself. They run wherever atomic builds.

// Wherever the tests that need the sandbox are not built — Linux without the
// `sandbox-tests` feature, and every other operating system — the sandbox is
// there to be asked for nowhere, so those tests are not built here and the
// helpers they alone use would read as dead.
#![cfg_attr(
    not(all(target_os = "linux", feature = "sandbox-tests")),
    allow(dead_code, unused_imports)
)]
#![recursion_limit = "512"]
#![allow(clippy::unwrap_used)]

use git_repo::{git_in, git_state};

mod git_repo;
#[cfg(all(target_os = "linux", feature = "sandbox-tests"))]
mod support;

use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
use serde_json::{Value, json};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ATOMIC: &str = env!("CARGO_BIN_EXE_atomic");

fn text(out: &Output) -> String {
    format!(
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// `--json` output, read back as JSON.
fn stdout_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", text(out)))
}

/// The command `git` runs in `dir`: as a person runs it, with no global or
/// system configuration read. [`git_in`] runs it and asks for success; this one
/// is for a caller that reads a non-zero exit as an answer (see [`git_try_in`]).
fn git_command(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .args([
            "-c",
            "user.name=dev",
            "-c",
            "user.email=dev@example.com",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    command
}

/// git in `dir`, where a non-zero exit is an answer of its own: whether it
/// succeeded, and what it printed. `git symbolic-ref -q HEAD` exits 1 and
/// prints nothing exactly when HEAD is detached, which is a state a test
/// wants to see rather than a failure to compare.
fn git_try_in(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = git_command(dir, args).output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_owned(),
    )
}

/// The sha256 of `bytes`, so a file's whole content is compared without its
/// content landing in a test's failure message.
fn digest(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// Say `line` where the person running the tests will see it whether or not
/// the harness is capturing output: written straight to standard error with
/// `write(2)`, past anything that takes a test's output and keeps it for a
/// failure that never happens. A test that says nothing about what it did not
/// check reads exactly like one that checked it, so this fails if the line
/// cannot be written; a short write is finished and an interrupted one retried.
fn shout(line: &str) {
    let line = format!("{line}\n");
    let bytes = line.as_bytes();
    let mut written = 0;
    while written < bytes.len() {
        // SAFETY: fd 2 is the process's standard error, and `bytes[written..]`
        // is a live slice of `line` for the whole call.
        #[allow(unsafe_code)]
        let n = unsafe {
            libc::write(
                2,
                bytes[written..].as_ptr() as *const libc::c_void,
                bytes.len() - written,
            )
        };
        if n < 0 {
            let why = std::io::Error::last_os_error();
            assert_eq!(
                why.raw_os_error(),
                Some(libc::EINTR),
                "could not say: {line}{why}"
            );
            continue;
        }
        let n = usize::try_from(n).expect("a write of fewer bytes than were asked for");
        assert!(n > 0, "saying {line} made no progress");
        written += n;
    }
}

/// The euid of this run when it is root. Root is told past a directory's mode:
/// it lists a directory whose mode is 000 and reads what an ordinary person is
/// refused there, so a row that rests on a mode cannot bite for it. Each such
/// row says so with [`shout`] and every other row still runs.
fn root_euid() -> Option<u32> {
    // SAFETY: geteuid takes no arguments, touches no memory and cannot fail.
    #[allow(unsafe_code)]
    let euid = unsafe { libc::geteuid() };
    (euid == 0).then_some(euid)
}

/// A path the comparison of a checkout holds as its own. What it is decides how
/// it is compared, and that is settled once, by the walk that found it, rather
/// than asked of the file system again later (see [`working_files`]).
enum Working {
    /// Compared by the bytes it holds.
    File(PathBuf),
    /// Compared by what it points at, read from the link and never followed: a
    /// link points where its writer chose, which may be an ancestor of the
    /// checkout or somewhere wholly outside it.
    Link { path: PathBuf, target: PathBuf },
}

impl Working {
    /// The path itself, wherever it stands.
    fn path(&self) -> &Path {
        match self {
            Working::File(path) | Working::Link { path, .. } => path,
        }
    }

    /// How `path` stands right now, for a caller that has one and did not walk
    /// to it: a link by what it points at, anything else by its bytes.
    /// `symlink_metadata` answers about the path itself and never follows a
    /// link *at the end of it*, so a repository whose `.git/config` is one is
    /// compared as the link it is. It does follow every part before the end: a
    /// caller asking about a path under a directory a turn could have replaced
    /// is answered through that link, which is why [`classify_root`] asks about
    /// such a directory first. A path that is not there is kept as the file it
    /// would be, whose reading says so.
    fn at(path: &Path) -> Self {
        let kind = match std::fs::symlink_metadata(path) {
            Ok(kind) => kind,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Working::File(path.into());
            }
            Err(e) => panic!("cannot read what {} is to compare it: {e}", path.display()),
        };
        if !kind.is_symlink() {
            return Working::File(path.into());
        }
        Working::Link {
            path: path.into(),
            target: match std::fs::read_link(path) {
                Ok(target) => target,
                Err(e) => panic!("cannot read where {} points: {e}", path.display()),
            },
        }
    }

    /// The line [`file_state`] records for it: `<path, relative to dir>` and
    /// its sha256, or the same path with `link -> ` and what it points at, or
    /// that path and why it could not be read.
    fn line(&self, dir: &Path) -> String {
        let path = self.path();
        let name = path.strip_prefix(dir).unwrap_or(path).display();
        match self {
            Working::File(_) => match std::fs::read(path) {
                Ok(bytes) => format!("{name} {}", digest(&bytes)),
                Err(e) => format!("{name} unreadable: {e}"),
            },
            Working::Link { target, .. } => format!("{name} link -> {}", target.display()),
        }
    }
}

/// How one of the roots under `.git` stands, which is what decides how much of
/// it is compared — see [`classify_root`].
enum Root {
    /// A directory, so what is under it is compared, by the walk it starts.
    Directory,
    /// A link, already recorded in `found` by what it points at. Whatever
    /// stands under that name lives wherever the turn that made the link
    /// chose, so nothing under it is compared: a walk that begins by listing a
    /// link lists whatever the link points at, which is how a copy holding the
    /// same names and the same bytes comes to read as no change at all.
    Link,
    /// There, and no directory: the ordinary file a turn put where the
    /// directory was, already recorded in `found` and compared by its bytes
    /// like any other path.
    File,
    /// Not there at all, which leaves nothing to compare.
    Absent,
}

/// Ask `path` what it is before anything under it is read or listed, and record
/// in `found` the two answers that are a line of their own. It is asked of the
/// roots a turn with write access can replace wholesale — `.git` itself, and
/// under it `.git/info` and `.git/hooks` — because each is one path that no
/// walk of the checkout can see: asking about a path *under* one is answered
/// through a link standing in its place (see [`Working::at`]), and listing one
/// lists whatever a link points at. What the answer is decides what is read:
/// under a link and under a path that is no directory, nothing at all.
///
/// Fails closed. Only NotFound is an absence. Anything else — a component along
/// the way that cannot be searched, a directory whose mode refuses the search,
/// a link earlier in the path that loops back on itself — is not "nothing is
/// here", and refuses rather than reads the root as an empty one. A root that is
/// itself a link that loops back on itself is no such error: the answer is about
/// the link, which is recorded by where it points.
fn classify_root(path: &Path, found: &mut Vec<Working>) -> Root {
    match std::fs::symlink_metadata(path) {
        Ok(kind) if kind.is_symlink() => {
            let target = match std::fs::read_link(path) {
                Ok(target) => target,
                Err(e) => panic!("cannot read where {} points: {e}", path.display()),
            };
            found.push(Working::Link {
                path: path.to_owned(),
                target,
            });
            Root::Link
        }
        Ok(kind) if kind.is_dir() => Root::Directory,
        Ok(_) => {
            found.push(Working::File(path.to_owned()));
            Root::File
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Root::Absent,
        Err(e) => panic!("cannot ask what {} is to compare it: {e}", path.display()),
    }
}

/// What `entry` is, asked of the entry itself. `DirEntry::file_type` answers
/// about a link as the link it is; asking the file system after the path —
/// `Path::is_dir`, `Path::metadata` — answers about whatever the link points
/// at, which is how a link to an ancestor comes to be walked and a link to
/// somewhere outside the checkout comes to be read as though it were inside.
fn entry_type(entry: &std::fs::DirEntry, path: &Path) -> std::fs::FileType {
    match entry.file_type() {
        Ok(kind) => kind,
        Err(e) => panic!("cannot read what {} is to compare it: {e}", path.display()),
    }
}

/// Which walk [`working_files`] is making, which is what decides whether a
/// directory named `.git` is left out of it.
#[derive(Clone, Copy, PartialEq)]
enum Walk {
    /// The checkout itself, whose `.git` at the top is the one
    /// [`git_metadata_files`] compares and so is left out of the walk. A `.git`
    /// deeper down is nothing special there: a file planted at `sub/.git/x` is
    /// compared with every other file.
    TheCheckout,
    /// Somewhere inside the checkout, such as `.git/hooks`, where nothing is
    /// left out by name: a directory there spelled `.git` is walked and
    /// compared like any other, because nothing else in the comparison covers
    /// what sits under it.
    SomewhereElse,
}

/// Whether the walk leaves `path` out: only the checkout's own `.git`, at the
/// very top of a walk of the checkout, because that is the one
/// [`git_metadata_files`] compares. A `.git` deeper down is nothing special — a
/// file planted at `sub/.git/x` is compared with every other file — and one at
/// the top of a walk that is not of the checkout, `.git/hooks/.git` say, is
/// covered by nothing else and is compared just the same.
///
/// The name is asked before the entry's type, so a `.git` that is a link is left
/// out with the same answer as a directory, whatever the link points at. That is
/// sound only because [`classify_root`] asks what that one path is before
/// anything under it is read: a link there is recorded as the link it is, so a
/// path is never left out of the comparison by both.
fn skips_the_git_at_the_top(walk: Walk, at_the_top: bool, path: &Path) -> bool {
    at_the_top && walk == Walk::TheCheckout && path.file_name() == Some(OsStr::new(".git"))
}

/// Every working path under `dir`, which is the checkout itself only for
/// [`Walk::TheCheckout`] — see [`Walk`] for what that decides. A directory that
/// is there but cannot be listed fails rather than goes unseen: a file hidden
/// inside it is a change this comparison would otherwise read as nothing. One
/// that is not there at all, like a repository with no `.git/hooks`, has
/// nothing to list.
///
/// Nothing here follows a link (see [`entry_type`]): a link is recorded by what
/// it points at and the walk goes no further, so an ancestor linked to from
/// inside the checkout ends the walk where it stands, and what a link outside
/// it holds stays outside. `dir` *itself* is one path this walk cannot see —
/// listing a link lists whatever it points at — so a caller that starts a walk
/// at a path a turn could have replaced asks about that path first, as
/// [`git_metadata_files`] does of `.git/hooks`.
fn working_files(dir: &Path, walk: Walk) -> Vec<Working> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(at) = stack.pop() {
        let at_the_top = at == *dir;
        let entries = match std::fs::read_dir(&at) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => panic!("cannot list {} to compare it: {e}", at.display()),
        };
        for entry in entries {
            let entry =
                entry.unwrap_or_else(|e| panic!("cannot read an entry of {}: {e}", at.display()));
            let path = entry.path();
            if skips_the_git_at_the_top(walk, at_the_top, &path) {
                continue;
            }
            let kind = entry_type(&entry, &path);
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_symlink() {
                let target = match std::fs::read_link(&path) {
                    Ok(target) => target,
                    Err(e) => panic!("cannot read where {} points: {e}", path.display()),
                };
                found.push(Working::Link { path, target });
            } else {
                found.push(Working::File(path));
            }
        }
    }
    found
}

/// What a run could leave changed inside `<dir>/.git` that no `git status`
/// line and no `for-each-ref` shows: the repository's own configuration,
/// every hook, and `info/exclude`, the ignore file that lives in the
/// repository itself. `.git/index` is left out on purpose: `git status`
/// refreshes it as it runs, and a change *to* the index is what `git status`'s
/// own lines report.
///
/// The three roots here — `.git` itself, and under it `.git/info` and
/// `.git/hooks` — are each asked what they are before anything under them is
/// read or listed, and compared as the answer (see [`classify_root`]). A turn
/// with write access can move any of those directories elsewhere and leave a
/// link in its place, and a copy at the other end of the link can hold these
/// very names and these very bytes: reading through the link, whether by asking
/// about a path under the root or by listing it, answers with the copy and
/// reads the swap as no change at all. So nothing under a root is read unless
/// that root is a directory.
fn git_metadata_files(dir: &Path) -> Vec<Working> {
    let git = dir.join(".git");
    let mut found = Vec::new();
    // The checkout's own `.git`, which the checkout walk leaves out by name
    // before it asks what the entry is (see [`skips_the_git_at_the_top`]), so
    // this is where that one path is compared — and once it is a link,
    // everything below here would be read from whatever the link points at.
    if let Root::Directory = classify_root(&git, &mut found) {
        // The configuration, the ignore file, and the directory that holds it
        // asked for before anything under it: `info/exclude` is read by a path,
        // and a path is followed through a link standing where a directory was.
        //
        // What is asked first matters where `.git` itself cannot be searched:
        // `symlink_metadata` succeeds on a directory whose mode refuses the
        // search, so it is the asking of a path *under* the unsearchable one
        // that fails, and the path asked first is the one the refusal names.
        if let Root::Directory = classify_root(&git.join("info"), &mut found) {
            found.push(Working::at(&git.join("info/exclude")));
        }
        found.push(Working::at(&git.join("config")));
        // The hooks, which are a walk and not one path. This walk is not of the
        // checkout, so a directory inside it spelled `.git` is walked and
        // compared like any other: nothing else here covers what sits under it.
        let hooks = git.join("hooks");
        if let Root::Directory = classify_root(&hooks, &mut found) {
            found.extend(working_files(&hooks, Walk::SomewhereElse));
        }
    }
    found
}

/// Every path under `dir`, and the git metadata above, as one sorted line
/// each — see [`Working::line`]. Two answers differ whenever a path appeared,
/// vanished or changed a byte, or a link started pointing somewhere else:
/// tracked, untracked or ignored, in the working tree or in `.git`.
fn file_state(dir: &Path) -> String {
    let mut entries = working_files(dir, Walk::TheCheckout);
    entries.extend(git_metadata_files(dir));
    let mut lines: Vec<String> = entries.iter().map(|entry| entry.line(dir)).collect();
    lines.sort();
    lines.join("\n")
}

/// The line [`repository_state`] puts between what git reported and the files
/// it compared, so a test can tell the two apart.
const FILES_MARK: &str = "files:";

/// Everything about the repository at `dir` that a run could leave changed:
/// what git records ([`git_state`], the one reading the suites that run a step
/// share — its refs, its worktrees, its working tree with every untracked file,
/// its own configuration and its objects); HEAD's own commit and name, read
/// from the repository (`for-each-ref` lists the refs and never HEAD, and only
/// `git worktree list` says anything of HEAD, in its own shape); and every
/// path's bytes, or the target every link points at ([`file_state`]) — which
/// nothing in `git status` shows for an ignored file, a hook, a link or
/// `info/exclude`, and which reads every file, the repository's configuration
/// included, as the bytes it holds rather than as git makes them out. Two
/// answers are equal only when the repository is as it was, byte for byte,
/// apart from `.git/index` (see [`git_metadata_files`]).
fn repository_state(dir: &Path) -> String {
    let head = git_in(dir, &["rev-parse", "HEAD"]);
    let (named, branch) = git_try_in(dir, &["symbolic-ref", "-q", "HEAD"]);
    let where_head = format!(
        "HEAD at {head} as {}",
        if named { &branch } else { "(detached)" }
    );
    format!(
        "{}\n{where_head}\n{FILES_MARK}\n{}",
        git_state(dir),
        file_state(dir)
    )
}

/// The lines of a [`repository_state`] answer that name HEAD: where it stands,
/// and what it is called when it has a name.
fn head_state(state: &str) -> Vec<&str> {
    state
        .lines()
        .filter(|l| l.starts_with("HEAD at "))
        .collect()
}

/// The file lines of a [`repository_state`] answer, which follow
/// [`FILES_MARK`]; what git reported comes before it.
fn file_lines(state: &str) -> std::str::Lines<'_> {
    let (_, files) = state.split_once(FILES_MARK).unwrap_or_else(|| {
        panic!("no {FILES_MARK} line in a repository state:\n{state}");
    });
    files.lines()
}

/// The file lines of a [`repository_state`] answer as `<path, sha256>` pairs:
/// a pair that is in both of two answers is a file that did not change, and a
/// path whose pair is not is one that appeared, vanished or changed a byte.
fn file_entries(state: &str) -> Vec<(&str, &str)> {
    file_lines(state)
        .map(|line| line.split_once(' ').unwrap_or((line, "")))
        .collect()
}

/// What a run left in the checkout, as the paths whose file line in
/// [`repository_state`] differs between `before` and `after` — appeared,
/// vanished or changed a byte. A checkout left exactly as it was gives nothing.
fn left_behind<'a>(before: &'a str, after: &'a str) -> Vec<&'a str> {
    let (was, now) = (file_entries(before), file_entries(after));
    let mut changed: Vec<&'a str> = was
        .iter()
        .filter(|e| !now.contains(e))
        .map(|(path, _)| *path)
        .collect();
    changed.extend(
        now.iter()
            .filter(|e| !was.contains(e))
            .map(|(path, _)| *path),
    );
    changed.sort();
    changed.dedup();
    changed
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A project with one codex atom that may write, run by `coding`, one
/// worktree step at the mapped base, `twice`, a second step on the
/// first's commit, and `relay`, one step at a base its input gives as a
/// plain string.
fn project(at: &Path) -> PathBuf {
    let project = at.to_owned();
    write(&project.join("atomic.yaml"), "atomic: 1\nname: coding\n");
    write(
        &project.join("atoms/fix-code.atom.astro"),
        "---\nimport type { Props, Input, Output } from '../.atomic/types/fix-code';\n\
         import type { AtomConfig } from '../.atomic/types/authoring';\n\
         const { input }: Props = Astro.props;\n\
         export const config: AtomConfig<Input, Output> = { description: 'Fix the code.', \
         kind: 'codex', sandbox: 'workspace-write' };\n---\nFix this: {input.task}\n",
    );
    write(
        &project.join("atoms/fix-code.input.schema.json"),
        &json!({"type": "object", "additionalProperties": false, "required": ["base", "task"],
            "properties": {"base": {"type": "string", "format": "git-commit"}, "task": {"type": "string"}}})
        .to_string(),
    );
    write(
        &project.join("atoms/fix-code.output.schema.json"),
        &json!({"type": "object", "additionalProperties": false, "required": ["done"],
            "properties": {"done": {"type": "boolean"}}})
        .to_string(),
    );
    write(
        &project.join("workflows/coding.workflow.yaml"),
        "atomic: 1\nname: coding\n\
         input: { $ref: ../atoms/fix-code.input.schema.json }\n\
         output: { $ref: ../atoms/fix-code.output.schema.json }\n\
         steps:\n  fix:\n    atom: fix-code\n    with: { base: input.base, task: input.task }\n    worktree: true\n\
         returns: steps.fix.output\n",
    );
    write(
        &project.join("workflows/twice.workflow.yaml"),
        "atomic: 1\nname: twice\n\
         input: { $ref: ../atoms/fix-code.input.schema.json }\n\
         output: { $ref: ../atoms/fix-code.output.schema.json }\n\
         steps:\n  first:\n    atom: fix-code\n    with: { base: input.base, task: input.task }\n    worktree: true\n\
         \x20 second:\n    after: [first]\n    atom: fix-code\n    \
         with: { base: steps.first.metadata.commit, task: input.task }\n    worktree: true\n\
         returns: steps.second.output\n",
    );
    write(
        &project.join("schemas/relay.input.schema.json"),
        &json!({"type": "object", "additionalProperties": false, "required": ["from", "task"],
            "properties": {"from": {"type": "string"}, "task": {"type": "string"}}})
        .to_string(),
    );
    write(
        &project.join("workflows/relay.workflow.yaml"),
        "atomic: 1\nname: relay\n\
         input: { $ref: ../schemas/relay.input.schema.json }\n\
         output: { $ref: ../atoms/fix-code.output.schema.json }\n\
         steps:\n  fix:\n    atom: fix-code\n    with: { base: input.from, task: input.task }\n    worktree: true\n\
         returns: steps.fix.output\n",
    );
    project
}

/// `read-note`, a read-only Codex atom, in `project`: named directly, with no
/// worktree and no fixture, and no `base` its input would need. A Codex atom
/// that may not write is the one `atomic run` lets work where its project sits.
fn read_only_atom(project: &Path) {
    write(
        &project.join("atoms/read-note.atom.astro"),
        "---\nimport type { Props, Input, Output } from '../.atomic/types/read-note';\n\
         import type { AtomConfig } from '../.atomic/types/authoring';\n\
         const { input }: Props = Astro.props;\n\
         export const config: AtomConfig<Input, Output> = { description: 'Read the note.', \
         kind: 'codex', sandbox: 'read-only' };\n---\nRead this: {input.task}\n",
    );
    write(
        &project.join("atoms/read-note.input.schema.json"),
        &json!({"type": "object", "additionalProperties": false, "required": ["task"],
            "properties": {"task": {"type": "string"}}})
        .to_string(),
    );
    write(
        &project.join("atoms/read-note.output.schema.json"),
        &json!({"type": "object", "additionalProperties": false, "required": ["done"],
            "properties": {"done": {"type": "boolean"}}})
        .to_string(),
    );
}

/// The project in a developer's checkout of their own, under `dir`: written at
/// `dev/examples/coding`, then committed, so the checkout is clean and every
/// comparison of it has one answer to compare with. Returns the checkout and
/// the project.
fn project_in(dir: &Path) -> (PathBuf, PathBuf) {
    let checkout = dir.join("dev");
    let project = project(&checkout.join("examples/coding"));
    git_in(&checkout, &["init", "-q"]);
    git_in(&checkout, &["add", "."]);
    git_in(&checkout, &["commit", "-q", "-m", "project"]);
    (checkout, project)
}

/// A fixture repository at `at` with one commit; returns it.
fn fixture(at: &Path) -> String {
    write(&at.join("price.txt"), "125\n");
    git_in(at, &["init", "-q"]);
    git_in(at, &["add", "price.txt"]);
    git_in(at, &["commit", "-q", "-m", "seed"]);
    git_in(at, &["rev-parse", "HEAD"])
}

/// A scratch directory of the test's own.
fn scratch(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-fixture-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

/// A directory of the test's own outside every git repository: under the
/// system's temporary directory, since the target directory is inside the
/// checkout being tested. Checked, so the test fails rather than run inside
/// a repository if that ever changes.
fn outside_any_repository(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("atomic-fixture-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dir = std::fs::canonicalize(&dir).unwrap();
    let inside = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(&dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        !inside.status.success(),
        "{} is inside a git work tree: {}",
        dir.display(),
        text(&inside)
    );
    dir
}

/// `atomic` with `args`, from `cwd`, on the fake model at `url`.
async fn atomic(dir: &Path, cwd: &Path, args: &[&str], url: &str, env: &[(&str, &str)]) -> Output {
    let (d, cwd, url) = (dir.to_owned(), cwd.to_owned(), url.to_owned());
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    let env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    tokio::task::spawn_blocking(move || {
        let mut command = Command::new(ATOMIC);
        command
            .args(&args)
            .current_dir(&cwd)
            .env("HOME", d.join("home"))
            .env("XDG_CONFIG_HOME", d.join("home/.config"))
            .env("ATOMIC_HOME", d.join("atomic-home"))
            .env("USER", "dev")
            .env("ATOMIC_MODEL_BASE_URL", url)
            .env("ATOMIC_MODEL", "synthetic")
            .env("ATOMIC_DETECT_OLLAMA_ADDR", "127.0.0.1:9")
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENAI_BASE_URL")
            .env_remove("ATOMIC_DATABASE_URL")
            .env_remove("ATOMIC_FAILPOINT")
            .env_remove("CODEX_HOME");
        for (k, v) in &env {
            command.env(k, v);
        }
        command.output().unwrap()
    })
    .await
    .unwrap()
}

/// `atomic` with `args`, from `project`, on the fake model at `url`, with the
/// run bound to the repository at `fixture`: every fixture-bound run of this
/// file ends with the same pair, since atomic takes a fixture only with the
/// revision to start from.
async fn run_bound(
    dir: &Path,
    project: &Path,
    args: &[&str],
    fixture: &Path,
    url: &str,
    env: &[(&str, &str)],
) -> Output {
    let fixture_arg = fixture.to_string_lossy().into_owned();
    let all: Vec<&str> = args
        .iter()
        .copied()
        .chain(["--fixture", &fixture_arg, "--fixture-rev", "main"])
        .collect();
    atomic(dir, project, &all, url, env).await
}

/// The snapshots atomic made under `dir`'s atomic home.
fn snapshots(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir.join("atomic-home/projects"))
        .unwrap()
        .flat_map(|p| {
            std::fs::read_dir(p.unwrap().path().join("fixtures"))
                .into_iter()
                .flatten()
        })
        .map(|e| e.unwrap().path())
        .filter(|p| {
            !p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        })
        .collect()
}

/// The commit atomic accepted for `step` of `run` in `snapshot`.
fn accepted(snapshot: &Path, run: &str, step: &str) -> String {
    let refs = git_in(
        snapshot,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            &format!("refs/atomic/{run}/"),
        ],
    );
    refs.lines()
        .find(|l| l.starts_with(&format!("refs/atomic/{run}/steps/{step}/")))
        .unwrap_or_else(|| panic!("no accepted commit for {step} in {refs}"))
        .split(' ')
        .nth(1)
        .unwrap()
        .to_owned()
}

/// What the model's command printed, in the request after its call.
fn printed(model: &FakeModel) -> String {
    model.requests()[1]["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "function_call_output")
        .map(|item| item["output"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run `coding` bound to `fixture` at `main` from `project`, its turn
/// reading the fixture's file, and check it works at the top of the
/// snapshot's worktree and leaves both repositories as they were.
async fn coding_run_at_the_top(dir: &Path, project: &Path, fixture: &Path, base: &str) {
    let model = FakeModel::start(vec![
        Reply::Tool {
            name: "exec_command".into(),
            arguments: json!({"cmd": "cat price.txt; pwd", "yield_time_ms": 20000}),
        },
        Reply::Text(r#"{"done": true}"#.into()),
    ])
    .await
    .unwrap();
    let source = repository_state(fixture);
    let data = json!({"base": base, "task": "multiply by quantity"}).to_string();
    let out = run_bound(
        dir,
        project,
        &[
            "--json", "run", "coding", "--run-id", "fix-1", "--data", &data,
        ],
        fixture,
        &model.base_url(),
        &[],
    )
    .await;
    assert!(out.status.success(), "{}", text(&out));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["output"], json!({"done": true}), "{doc}");
    // The turn worked at the top of its worktree, where the fixture's
    // files are, inside atomic's state.
    let said = printed(&model);
    assert!(said.contains("125"), "{said}");
    assert!(
        said.contains(&dir.join("atomic-home").display().to_string()),
        "{said}"
    );
    let snapshots = snapshots(dir);
    assert_eq!(snapshots.len(), 1, "{snapshots:?}");
    assert!(snapshots[0].ends_with(format!("{base}.git")));
    let step = accepted(&snapshots[0], "fix-1", "fix");
    assert_eq!(
        git_in(&snapshots[0], &["rev-parse", &format!("{step}^")]),
        base
    );
    let after = repository_state(fixture);
    assert_eq!(
        after,
        source,
        "the fixture is exactly as it was: {}",
        left_behind(&source, &after).join(", ")
    );
}

/// A fixture-bound coding step whose reviewer asks for changes, then
/// approves the repair: the repair round's worktree starts from the run's
/// base in the snapshot, as check_base requires, and the commit published is
/// the repair's, the one the second verdict was given.
// needs_sandbox
#[cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#[test]
fn a_repair_round_of_a_fixture_bound_step_publishes_the_repairs_commit() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("repaired");
        let outside = outside_any_repository("repaired-project");
        let project = project(&outside.join("project"));
        reviewed_coding(&project);
        let base = fixture(&dir.join("fixture"));
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"done": true}"#.into()),
            Reply::Text(r#"{"verdict": "request_changes"}"#.into()),
            Reply::Text(r#"{"done": true}"#.into()),
            Reply::Text(r#"{"verdict": "approve"}"#.into()),
        ])
        .await
        .unwrap();
        let data = json!({"base": base, "task": "multiply by quantity"}).to_string();
        let out = run_bound(
            &dir,
            &project,
            &[
                "--json",
                "run",
                "reviewed",
                "--run-id",
                "repaired-1",
                "--data",
                &data,
            ],
            &dir.join("fixture"),
            &model.base_url(),
            &[],
        )
        .await;
        assert!(out.status.success(), "{}", text(&out));
        assert_eq!(model.calls(), 4);
        // The run keeps one accepted commit, the repair round's, made from
        // the run's base in the snapshot.
        let snapshots = snapshots(&dir);
        let refs = git_in(
            &snapshots[0],
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/atomic/repaired-1/",
            ],
        );
        let kept: Vec<&str> = refs.lines().collect();
        assert_eq!(kept.len(), 1, "{refs}");
        assert!(kept[0].contains("repair_1"), "{refs}");
        let repaired = kept[0].split(' ').nth(1).unwrap().to_owned();
        assert_eq!(
            git_in(&snapshots[0], &["rev-parse", &format!("{repaired}^")]),
            base
        );
        // The second verdict, which approved, was given the repair's
        // commit; the first, which asked for changes, another.
        let requests = model.requests();
        assert!(
            !requests[1].to_string().contains(&repaired),
            "{}",
            requests[1]
        );
        assert!(
            requests[3].to_string().contains(&repaired),
            "the approved candidate is not the repair's"
        );
        let _ = std::fs::remove_dir_all(&outside);
    });
}

/// The `reviewed` workflow and its reviewer, `check-commit`, in `project`.
fn reviewed_coding(project: &Path) {
    write(
        &project.join("atoms/check-commit.atom.astro"),
        "---\nimport type { Props, Input, Output } from '../.atomic/types/check-commit';\n\
         import type { AtomConfig } from '../.atomic/types/authoring';\n\
         const { input }: Props = Astro.props;\n\
         export const config: AtomConfig<Input, Output> = { description: 'Check the commit.', \
         kind: 'lite' };\n---\nReview commit {input.commit}.\n",
    );
    write(
        &project.join("atoms/check-commit.input.schema.json"),
        &json!({"type": "object", "additionalProperties": false, "required": ["commit"],
            "properties": {"commit": {"type": "string"}}})
        .to_string(),
    );
    write(
        &project.join("atoms/check-commit.output.schema.json"),
        &json!({"type": "object", "additionalProperties": false, "required": ["verdict"],
            "properties": {"verdict": {"type": "string", "enum": ["approve", "request_changes"]}}})
        .to_string(),
    );
    write(
        &project.join("workflows/reviewed.workflow.yaml"),
        "atomic: 1\nname: reviewed\n\
         input: { $ref: ../atoms/fix-code.input.schema.json }\n\
         output: { $ref: ../atoms/fix-code.output.schema.json }\n\
         steps:\n  fix:\n    atom: fix-code\n    with: { base: input.base, task: input.task }\n    worktree: true\n\
         \x20   review:\n      reviewer: check-commit\n      with: { commit: candidate.metadata.commit }\n\
         returns: steps.fix.output\n",
    );
}

/// A fixture-bound coding step under review: its reviewer is given the
/// commit atomic made in the fixture's snapshot, as a SHA (it runs in the
/// project, not the snapshot, so the SHA is what it has of the change).
// needs_sandbox
#[cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#[test]
fn a_reviewer_of_a_fixture_bound_step_is_given_the_commit_in_the_snapshot() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("reviewed");
        let outside = outside_any_repository("reviewed-project");
        let project = project(&outside.join("project"));
        reviewed_coding(&project);
        let base = fixture(&dir.join("fixture"));
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"done": true}"#.into()),
            Reply::Text(r#"{"verdict": "approve"}"#.into()),
        ])
        .await
        .unwrap();
        let data = json!({"base": base, "task": "multiply by quantity"}).to_string();
        let out = run_bound(
            &dir,
            &project,
            &[
                "--json",
                "run",
                "reviewed",
                "--run-id",
                "reviewed-1",
                "--data",
                &data,
            ],
            &dir.join("fixture"),
            &model.base_url(),
            &[],
        )
        .await;
        assert!(out.status.success(), "{}", text(&out));
        let snapshots = snapshots(&dir);
        let step = accepted(&snapshots[0], "reviewed-1", "fix");
        let review = model.requests()[1].to_string();
        assert!(
            review.contains(&step),
            "the reviewer was not given {step}: {review}"
        );
        let _ = std::fs::remove_dir_all(&outside);
    });
}

// needs_sandbox
#[cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#[test]
fn a_project_outside_any_repository_runs_its_coding_step_in_the_snapshot() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("outside");
        let outside = outside_any_repository("outside-project");
        let project = project(&outside.join("project"));
        let base = fixture(&dir.join("fixture"));
        coding_run_at_the_top(&dir, &project, &dir.join("fixture"), &base).await;
        let _ = std::fs::remove_dir_all(&outside);
    });
}

// needs_sandbox
#[cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#[test]
fn a_project_in_a_subdirectory_of_a_checkout_runs_at_the_top_of_the_fixture() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("subdirectory");
        let (checkout, project) = project_in(&dir);
        let before = repository_state(&checkout);
        let base = fixture(&dir.join("fixture"));
        coding_run_at_the_top(&dir, &project, &dir.join("fixture"), &base).await;
        let after = repository_state(&checkout);
        assert_eq!(
            after,
            before,
            "the checkout is exactly as it was: {}",
            left_behind(&before, &after).join(", ")
        );
    });
}

/// The comparison every claim below makes when it says the checkout is exactly
/// as it was is [`repository_state`], which is a control of its own: a change
/// a run could leave in the checkout must show up in it, whether or not git
/// reports it. Here one such change is planted by hand and required to show,
/// and then undone and required not to: HEAD detached at its own commit, a
/// hook, a file ignored by the repository's own `info/exclude`, a file under a
/// `.git` below the top level, a link to an ancestor of the checkout, and a
/// directory whose contents cannot be listed — none of which `git status` names
/// — and the repository's own configuration, which `git status` does not name
/// either but which [`git_state`], the reading this comparison is built on,
/// does. That last directory is shown only where the test does not run as root,
/// which is told past a directory's mode; a run as root says in its output that
/// it skipped that one, and every other row still runs. Nothing here runs a
/// turn or reaches a model, so this test needs no sandbox.
#[test]
fn the_state_a_checkout_is_compared_by_sees_what_git_does_not() {
    let dir = scratch("state");
    let checkout = dir.join("dev");
    write(&checkout.join("price.txt"), "125\n");
    git_in(&checkout, &["init", "-q"]);
    git_in(&checkout, &["add", "price.txt"]);
    git_in(&checkout, &["commit", "-q", "-m", "seed"]);
    let tip = git_in(&checkout, &["rev-parse", "HEAD"]);
    let (git, whole) = (git_state(&checkout), repository_state(&checkout));

    // Where HEAD stands, and what it is called, read from the repository
    // itself: `for-each-ref` lists the refs and never HEAD, and only
    // `git worktree list` says anything of HEAD, in its own shape. Detaching
    // HEAD at its own commit moves no file and no ref, and leaves HEAD with no
    // name at all.
    assert_eq!(
        head_state(&whole),
        [format!("HEAD at {tip} as refs/heads/main").as_str()],
        "HEAD is not recorded where it stands and what it is called"
    );
    git_in(&checkout, &["checkout", "-q", "--detach"]);
    let moved = repository_state(&checkout);
    assert_eq!(
        head_state(&moved),
        [format!("HEAD at {tip} as (detached)").as_str()],
        "a detached HEAD is not recorded: {moved}"
    );
    assert!(
        left_behind(&whole, &moved).is_empty(),
        "detaching HEAD moved a file: {}",
        left_behind(&whole, &moved).join(", ")
    );
    assert_ne!(moved, whole, "a detached HEAD makes no difference");
    git_in(&checkout, &["checkout", "-q", "main"]);

    // A hook: code that runs the next time anyone runs git in the checkout.
    let hook = checkout.join(".git/hooks").join("post-checkout");
    write(&hook, "#!/bin/sh\nexit 0\n");
    assert_eq!(git_state(&checkout), git, "git reports no hook");
    assert_eq!(
        left_behind(&whole, &repository_state(&checkout)),
        [".git/hooks/post-checkout"],
        "a planted hook is not compared"
    );
    std::fs::remove_file(&hook).unwrap();

    // The repository's own configuration: what it would run, fetch or filter.
    // `git status` never names it, so this is the one plant of these that the
    // shared reading of git already answers, through `git config --list
    // --local`; here that answers for the reading itself. What the walk adds is
    // the file rather than the settings read out of it — the bytes it holds, or
    // the link it is, which is what a comparison of parsed settings cannot see.
    let config = checkout.join(".git/config");
    let config_bytes = std::fs::read(&config).unwrap();
    git_in(&checkout, &["config", "atomic.test", "planted"]);
    let read = git_state(&checkout);
    let named: Vec<&str> = read
        .lines()
        .filter(|l| l.starts_with("atomic.test"))
        .collect();
    assert_eq!(
        named,
        ["atomic.test=planted"],
        "git's reading misses a setting planted in the repository's own \
         configuration: {read}"
    );
    assert_eq!(
        left_behind(&whole, &repository_state(&checkout)),
        [".git/config"],
        "the repository's configuration is not compared"
    );
    std::fs::write(&config, &config_bytes).unwrap();

    // A `.git` below the top level is nothing special: only the checkout's own
    // is left to [`git_metadata_files`]. A file planted under one is a file of
    // the checkout like any other, and a walk that skips that name at any depth
    // leaves it unseen — as git itself leaves it unseen, tracking nothing under
    // a directory it takes for repository metadata.
    let nested = checkout.join("sub/.git");
    write(&nested.join("x"), "planted\n");
    assert_eq!(
        git_state(&checkout),
        git,
        "git reports the file planted under a `.git` below the top level"
    );
    assert_eq!(
        left_behind(&whole, &repository_state(&checkout)),
        ["sub/.git/x"],
        "a file under a `.git` below the top level is not compared"
    );

    // A link to an ancestor of the checkout, which is one touch from a turn with
    // write access. `git status` names a link of its own, but names nothing of
    // where it points, and a comparison that asks the file system what the path
    // is gets an answer through the link: it walks an ancestor for ever, or
    // reads what lies outside the checkout as though it lay inside. So the walk
    // answers about the entry itself — it compares the link and stops there,
    // which is also why the walk ends at all.
    let ancestor = checkout.join("sub/loop");
    std::os::unix::fs::symlink(&checkout, &ancestor).unwrap();
    assert!(
        git_state(&checkout).lines().any(|l| l == "?? sub/loop"),
        "git does not name the link at all, so this row shows nothing: {}",
        git_state(&checkout)
    );
    let planted = repository_state(&checkout);
    assert_eq!(
        left_behind(&whole, &planted),
        ["sub/.git/x", "sub/loop"],
        "a link to an ancestor, or the file under the nested `.git`, is not recorded"
    );
    let links: Vec<&str> = file_lines(&planted)
        .filter(|l| l.starts_with("sub/loop "))
        .collect();
    assert_eq!(
        links,
        [format!("sub/loop link -> {}", checkout.display()).as_str()],
        "the link is not recorded by what it points at"
    );
    std::fs::remove_file(&ancestor).unwrap();
    std::fs::remove_dir_all(checkout.join("sub")).unwrap();

    // And the very file the writing test looks for, once the repository is
    // told to ignore it: `git status` never names an ignored file, and the
    // ignore file a repository keeps for itself, `info/exclude`, is one status
    // never shows either.
    let exclude = checkout.join(".git/info/exclude");
    let exclude_bytes = std::fs::read(&exclude).unwrap();
    write(
        &exclude,
        &format!("{}\nnote.txt\n", String::from_utf8_lossy(&exclude_bytes)),
    );
    write(&checkout.join("note.txt"), "written\n");
    assert_eq!(
        git_state(&checkout),
        git,
        "git sees neither an ignored file nor the rule that ignores it"
    );
    assert_eq!(
        left_behind(&whole, &repository_state(&checkout)),
        [".git/info/exclude", "note.txt"],
        "an ignored file, and the repository's own ignore file, are not compared"
    );

    // A directory whose contents cannot be listed, which is where a turn with
    // write access would hide a file. Nothing can be read inside it, so the
    // walk names the directory and fails: reading the checkout as unchanged
    // here is the hole this closes.
    use std::os::unix::fs::PermissionsExt;
    let hidden = checkout.join("hidden");
    write(&hidden.join("planted.txt"), "hidden\n");
    std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o000)).unwrap();
    let refusal = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = repository_state(&checkout);
    }));
    std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o755)).unwrap();
    if let Some(euid) = root_euid() {
        // Root is told past a directory's mode: it lists `hidden` and every
        // file in it, so nothing is refused here and this row cannot be made to
        // bite. Say so where it will be seen rather than fail; every other row
        // above and below runs as an ordinary person would see it. Written with
        // [`shout`] rather than printed, because a test's output is kept for
        // when the test fails, and this one passes.
        shout(&format!(
            "SKIP the mode-000 directory: this runs as root (euid {euid}), which is told \
                 past a directory's mode and lists one whose mode is 000, so the walk's \
                 refusal cannot be shown here. Run this test as an unprivileged person to \
                 see it."
        ));
    } else {
        let why = match refusal {
            Ok(()) => {
                let listed: Vec<String> = std::fs::read_dir(&hidden)
                    .unwrap()
                    .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect();
                panic!(
                    "a directory whose mode is 000 was listed and read as nothing to \
                     compare; it holds {listed:?}"
                );
            }
            Err(why) => why
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| why.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_else(|| "a panic with no message".to_owned()),
        };
        // Not merely "something failed": the walk names the directory it
        // cannot list, which is the only way an unseen file there is reported.
        assert!(
            why.contains("cannot list") && why.contains("hidden"),
            "the walk failed for another reason: {why}"
        );
    }
    std::fs::remove_dir_all(&hidden).unwrap();

    // Every one of these undone: what the checkout holds is what it held
    // before, so none of the comparisons above fired on anything else a
    // moment of ordinary git left behind.
    std::fs::remove_file(checkout.join("note.txt")).unwrap();
    std::fs::write(&exclude, &exclude_bytes).unwrap();
    let again = repository_state(&checkout);
    assert_eq!(
        again,
        whole,
        "the checkout is as it was again: {}",
        left_behind(&whole, &again).join(", ")
    );
}

/// The hooks directory: one of the roots under `.git` that a turn with write
/// access can replace wholesale, the others being `.git` itself and `.git/info`
/// (see [`the_git_directory_at_the_top_is_compared_by_what_it_is_and_not_through_a_link`]
/// and [`the_info_directory_is_compared_by_what_it_is_and_not_through_a_link`],
/// and [`classify_root`] for the asking). Listing a directory that is a link
/// lists whatever the link points at, so hooks moved elsewhere and linked back
/// in leave every line of [`repository_state`] exactly as they were — and a
/// comparison that never fires reads exactly like one that passed. So the
/// comparison asks what `.git/hooks` is before it lists it, and never through a
/// link: a link is recorded by what it points at and nothing under it is
/// compared; a directory is compared by its contents, and the walk it starts
/// leaves nothing out by name, which is what keeps a `.git` planted under the
/// hooks directory in the answer too. And a hooks directory whose mode refuses
/// the listing fails the comparison rather than reads as an empty one — a mode
/// like that leaves the directory askable and the listing refused, which is
/// what this row reaches; a root that cannot be asked at all is the `.git`
/// test's row.
///
/// Like the check above, nothing here runs a turn or reaches a model, so this
/// test needs no sandbox. The one state it cannot show is that directory asked
/// of a run that is root, which is told past a mode; a run as root says so in
/// its output, as the other tests do, and every other row still runs.
#[test]
fn the_hooks_directory_is_compared_by_what_it_is_and_not_through_a_link() {
    let dir = scratch("hooks-state");
    let checkout = dir.join("dev");
    write(&checkout.join("price.txt"), "125\n");
    git_in(&checkout, &["init", "-q"]);
    git_in(&checkout, &["add", "price.txt"]);
    git_in(&checkout, &["commit", "-q", "-m", "seed"]);
    let hooks = checkout.join(".git/hooks");
    // git's own sample hooks go first, so that every answer below is the
    // directory's and nothing else's: the rows move the whole directory,
    // samples and all, and none of them puts a sample back.
    let samples = std::fs::read_dir(&hooks).unwrap_or_else(|e| {
        panic!(
            "a new repository has no `.git/hooks` directory to compare, so there is \
             nothing here for the rows below to replace ({}): {e}",
            hooks.display()
        )
    });
    for entry in samples {
        let entry = entry.unwrap();
        let sample = entry.path();
        std::fs::remove_file(&sample)
            .unwrap_or_else(|e| panic!("cannot remove the sample hook: {e}"));
    }
    // One hook of this checkout's own left in the emptied directory, so that the
    // first swap moves a directory that holds something: what the swap is
    // claimed to leave untouched — every hook's name and every byte under the
    // link — is then shown by a hook, and not by an empty directory.
    let hook = hooks.join("post-checkout");
    write(&hook, "#!/bin/sh\nexit 0\n");
    let (git, whole) = (git_state(&checkout), repository_state(&checkout));
    assert_eq!(
        file_lines(&whole)
            .filter(|l| l.starts_with(".git/hooks"))
            .collect::<Vec<_>>(),
        [format!(
            ".git/hooks/post-checkout {}",
            digest(&std::fs::read(&hook).unwrap())
        )
        .as_str()],
        "the checkout's own hook is not compared, by a line of its own and with \
         the directory for once having contents, before anything is swapped: {}",
        file_lines(&whole).collect::<Vec<_>>().join(", ")
    );

    // The swap this is about, made with a link to the directory its hooks were
    // moved out to: every hook keeps its name and every byte under the link, so
    // `git status` says nothing — it names nothing inside `.git` — and neither
    // does a walk that begins by listing the link, which lists those same hooks.
    // What changed is the path itself, and it is a link.
    let moved = dir.join("hooks-elsewhere");
    std::fs::create_dir(&moved).unwrap();
    std::fs::rename(&hooks, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &hooks).unwrap();
    assert_eq!(
        git_state(&checkout),
        git,
        "git reports a hooks directory replaced by a link"
    );
    let replaced = repository_state(&checkout);
    assert_eq!(
        left_behind(&whole, &replaced),
        [".git/hooks", ".git/hooks/post-checkout"],
        "a hooks directory replaced by a link is not reported as a change"
    );
    assert_eq!(
        file_lines(&replaced)
            .filter(|l| l.starts_with(".git/hooks "))
            .collect::<Vec<_>>(),
        [format!(".git/hooks link -> {}", moved.display()).as_str()],
        "the hooks root is not recorded as the link it is and where it points"
    );
    std::fs::remove_file(&hooks).unwrap();
    std::fs::rename(&moved, &hooks).unwrap();
    let back = repository_state(&checkout);
    assert_eq!(
        back,
        whole,
        "the directory put back is not read as it was: {}",
        left_behind(&whole, &back).join(", ")
    );

    // The same swap with a source a turn made: the names under the link and the
    // bytes they hold are whatever its writer chose, in hook names this checkout
    // would itself use. None of it is this checkout's, so none of it is
    // compared; what is reported is one link and where it leads.
    let planted = dir.join("planted-hooks");
    write(&planted.join("pre-commit"), "#!/bin/sh\nexit 0\n");
    std::fs::rename(&hooks, &moved).unwrap();
    std::os::unix::fs::symlink(&planted, &hooks).unwrap();
    let linked = repository_state(&checkout);
    assert_eq!(
        left_behind(&whole, &linked),
        [".git/hooks", ".git/hooks/post-checkout"],
        "hooks read through a link are reported as the checkout's own: {}",
        file_lines(&linked).collect::<Vec<_>>().join(", ")
    );
    assert_eq!(
        file_lines(&linked)
            .filter(|l| l.starts_with(".git/hooks "))
            .collect::<Vec<_>>(),
        [format!(".git/hooks link -> {}", planted.display()).as_str()],
        "a hooks link is not recorded by where it points"
    );
    assert!(
        file_lines(&linked).all(|l| !l.contains("pre-commit")),
        "a hook under the directory a link points at is compared as this \
         checkout's own: {}",
        file_lines(&linked).collect::<Vec<_>>().join(", ")
    );

    // The directory again, and this time what its own walk must not skip. A
    // `.git` below the top of the checkout is compared by that walk, which is
    // what leaves the checkout's own `.git` to this one; applying that skip at
    // the top of the hooks walk leaves `.git/hooks/.git` covered by nothing at
    // all, so a file planted inside it would be seen by no part of the
    // comparison.
    std::fs::remove_file(&hooks).unwrap();
    std::fs::rename(&moved, &hooks).unwrap();
    write(&hooks.join(".git/planted"), "planted\n");
    let nested = repository_state(&checkout);
    assert_eq!(
        git_state(&checkout),
        git,
        "git reports the file planted under a `.git` inside the hooks directory"
    );
    assert_eq!(
        left_behind(&whole, &nested),
        [".git/hooks/.git/planted"],
        "a file under a `.git` inside the hooks directory is not compared"
    );
    std::fs::remove_dir_all(hooks.join(".git")).unwrap();

    // A hooks directory whose mode is 000 is how a turn with write access hides
    // what it put there: the walk starts, cannot list it, and fails naming that
    // path rather than reading the hooks inside it as nothing. The files are
    // asked directly: `git status` neither reads nor runs a hook, so git's own
    // answer is unchanged here and this refusal belongs to the comparison
    // alone. Root is told past a directory's mode and lists one whose mode is
    // 000, so that row cannot be shown there; every row above runs however this
    // one goes.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&hooks, std::fs::Permissions::from_mode(0o000)).unwrap();
    if let Some(euid) = root_euid() {
        shout(&format!(
            "SKIP the mode-000 hooks directory: this runs as root (euid {euid}), which is \
                 told past a directory's mode and lists one whose mode is 000, so the walk's \
                 refusal cannot be shown here. Run this test as an unprivileged person to see \
                 it."
        ));
        // The mode goes back before the directory is compared again, so that
        // the last row below — that the checkout is as it was — is answered
        // from the state the answer was first captured from.
        std::fs::set_permissions(&hooks, std::fs::Permissions::from_mode(0o755)).unwrap();
    } else {
        let why = refusal_of(|| file_state(&checkout));
        std::fs::set_permissions(&hooks, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            why.contains("cannot list")
                && why.contains(".git/hooks")
                && why.contains("Permission denied"),
            "a hooks directory that cannot be listed was not refused for its \
             own reason: {why}"
        );
    }
    std::fs::remove_dir_all(&planted).unwrap();

    // Every one of these undone, and no state left between them: what the
    // checkout holds is what it held before.
    let again = repository_state(&checkout);
    assert_eq!(
        again,
        whole,
        "the checkout is as it was again: {}",
        left_behind(&whole, &again).join(", ")
    );
}

/// The message of the panic `what` raises, or a failure of the test's own when
/// it answers instead of refusing: how these rows say what a comparison refused
/// to answer, and that it refused for the reason they are about.
fn refusal_of<T>(what: impl FnOnce() -> T) -> String {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(what)) {
        Ok(_) => panic!("the comparison answered instead of refusing"),
        Err(why) => why
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| why.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_else(|| "a panic with no message".to_owned()),
    }
}

/// The lines of a comparison answer whose path is `named`, or sit under it:
/// what one path of the answer says, which is where the roots under `.git` are
/// seen — as the link they are, as their bytes, or as the paths under them.
/// `lines` is [`file_lines`] of a [`repository_state`] answer, or
/// [`file_state`] read straight for an answer about files alone, which is all a
/// checkout whose `.git` is no repository can give.
fn summarize<'a>(lines: impl Iterator<Item = &'a str>, named: &str) -> Vec<&'a str> {
    let mut prefix = named.to_owned();
    prefix.push(' ');
    let under = format!("{named}/");
    lines
        .filter(|l| l.starts_with(&prefix) || l.starts_with(&under))
        .collect()
}

/// Every path under `from`, with its bytes, at the same path under `to`: what a
/// turn with write access makes when it moves a directory out and leaves
/// something else in its place — a copy that keeps the names and the bytes, and
/// reads as nothing to a comparison that asks through what it left behind.
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let at = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &at);
        } else {
            std::fs::copy(entry.path(), at).unwrap();
        }
    }
}

/// The second of the roots a turn with write access can replace wholesale:
/// `.git/info` (see [`classify_root`]). It is never walked, so it looks safe to
/// leave out of the asking — the comparison reads the one path it holds,
/// `info/exclude`, by its name. But `symlink_metadata` follows every part of a
/// path before the last, so asking about `info/exclude` while `info` is a link
/// answers from whatever that link points at: a directory moved elsewhere and
/// linked back, with a copy holding this checkout's very `exclude` at the other
/// end, leaves the ignore file's line exactly as it was. So the comparison asks
/// what `.git/info` is before it reads anything under it, and reads nothing
/// under a link.
///
/// Nothing here runs a turn or reaches a model, so this test needs no sandbox.
/// The one row that cannot bite for a run as root — a directory whose mode is
/// 000, which root is told past — says so with [`shout`] and the rest still
/// run.
#[test]
fn the_info_directory_is_compared_by_what_it_is_and_not_through_a_link() {
    let dir = scratch("info-state");
    let checkout = dir.join("dev");
    write(&checkout.join("price.txt"), "125\n");
    git_in(&checkout, &["init", "-q"]);
    git_in(&checkout, &["add", "price.txt"]);
    git_in(&checkout, &["commit", "-q", "-m", "seed"]);
    let info = checkout.join(".git/info");
    let exclude = info.join("exclude");
    let exclude_bytes = std::fs::read(&exclude).unwrap();
    let (git, whole) = (git_state(&checkout), repository_state(&checkout));
    assert_eq!(
        summarize(file_lines(&whole), ".git/info"),
        [format!(".git/info/exclude {}", digest(&exclude_bytes)).as_str()],
        "the repository's own ignore file is not compared before anything is \
         swapped: {}",
        summarize(file_lines(&whole), ".git").join(", ")
    );

    // An ignore directory whose mode refuses the search is not a directory with
    // nothing in it. The comparison is asked about the directory first, and that
    // asking succeeds — a directory can be seen even where it cannot be searched
    // — so the refusal comes one path nearer, from the ignore file it cannot
    // read, and it names that file. Either way the comparison refuses rather than
    // answer that there is no ignore file to compare. Root is told past a mode,
    // so that row says what it cannot show; the swap below runs however this goes.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&info, std::fs::Permissions::from_mode(0o000)).unwrap();
    if let Some(euid) = root_euid() {
        shout(&format!(
            "SKIP the mode-000 info directory: this runs as root (euid {euid}), which is \
             told past a directory's mode and reads what an ordinary person is refused, so \
             this refusal cannot be shown here. Run this test as an unprivileged person to \
             see it."
        ));
        // The mode goes back before anything is compared again, so that every
        // answer below is read from the state the first one was.
        std::fs::set_permissions(&info, std::fs::Permissions::from_mode(0o755)).unwrap();
    } else {
        let why = refusal_of(|| file_state(&checkout));
        std::fs::set_permissions(&info, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            why.contains(&format!(
                "cannot read what {} is to compare it",
                exclude.display()
            )) && why.contains("Permission denied"),
            "the ignore file under a directory that cannot be searched was not \
             refused for its own reason: {why}"
        );
    }

    // The swap: the directory moved out, a link left where it was, and the copy
    // at the other end holding this checkout's own `exclude`. `git status` says
    // nothing of it — it names nothing inside `.git`, and least of all the
    // ignore file the repository keeps for itself — and neither does a
    // comparison that asks `info/exclude` by name, which is answered the
    // copy's bytes. What changed is the directory itself, and it is a link.
    let copy = dir.join("info-elsewhere");
    write(
        &copy.join("exclude"),
        &String::from_utf8_lossy(&exclude_bytes),
    );
    std::fs::remove_dir_all(&info).unwrap();
    std::os::unix::fs::symlink(&copy, &info).unwrap();
    assert_eq!(
        git_state(&checkout),
        git,
        "git reports the ignore directory replaced by a link"
    );
    let swapped = repository_state(&checkout);
    assert_eq!(
        left_behind(&whole, &swapped),
        [".git/info", ".git/info/exclude"],
        "an ignore directory replaced by a link to a copy of itself is not \
         reported as a change: {}",
        summarize(file_lines(&swapped), ".git").join(", ")
    );
    assert_eq!(
        summarize(file_lines(&swapped), ".git/info"),
        [format!(".git/info link -> {}", copy.display()).as_str()],
        "the ignore directory is not recorded as the link it is, with nothing \
         under the link read as this checkout's own: {}",
        summarize(file_lines(&swapped), ".git").join(", ")
    );

    // The directory and its file put back, byte for byte: every answer above is
    // the swap's and nothing else's.
    std::fs::remove_file(&info).unwrap();
    std::fs::create_dir(&info).unwrap();
    std::fs::write(&exclude, &exclude_bytes).unwrap();
    let again = repository_state(&checkout);
    assert_eq!(
        again,
        whole,
        "the ignore directory put back is not read as it was: {}",
        left_behind(&whole, &again).join(", ")
    );
    std::fs::remove_dir_all(&copy).unwrap();
}

/// The first of the roots a turn with write access can replace wholesale: the
/// checkout's `.git` itself (see [`classify_root`]). Nothing else in the
/// comparison ever sees that path — the checkout walk leaves the name out at the
/// top before it asks what the entry is (see [`skips_the_git_at_the_top`]), and
/// everything read under it is read by a path, which a link standing in the way
/// answers. So a `.git` moved out and linked back to a copy is doubly invisible:
/// skipped as a name by one half of the comparison and read through by the
/// other. The asking is what closes both: the path is compared as the link it
/// is, and nothing under it is read.
///
/// The root is swapped twice and put back byte for byte both times, because a
/// `.git` left pointing at a copy would have git itself read that copy for every
/// test after this one.
///
/// Nothing here runs a turn or reaches a model, so this test needs no sandbox.
/// The one row that cannot bite for a run as root — a directory whose mode is
/// 000, which root is told past — says so with [`shout`] and the rest still
/// run.
#[test]
fn the_git_directory_at_the_top_is_compared_by_what_it_is_and_not_through_a_link() {
    let dir = scratch("dot-git-state");
    let checkout = dir.join("dev");
    write(&checkout.join("price.txt"), "125\n");
    git_in(&checkout, &["init", "-q"]);
    git_in(&checkout, &["add", "price.txt"]);
    git_in(&checkout, &["commit", "-q", "-m", "seed"]);
    let git_dir = checkout.join(".git");
    let whole = repository_state(&checkout);
    let clean = git_in(
        &checkout,
        &["status", "--porcelain", "--untracked-files=all"],
    );
    let metadata = summarize(file_lines(&whole), ".git");
    assert!(
        metadata.iter().any(|l| l.starts_with(".git/config "))
            && metadata.iter().any(|l| l.starts_with(".git/info/exclude ")),
        "the metadata under `.git` is not compared, so there is nothing here for \
         a swap to hide: {}",
        metadata.join(", ")
    );

    // A `.git` whose mode is 000 is how a turn with write access hides what it
    // did inside the metadata. It is not an empty `.git`, and the comparison
    // refuses rather than answers that there is nothing under it. The asking of
    // the root itself succeeds — a directory can be seen where it cannot be
    // searched — so the refusal names the first path asked *under* it, `info`,
    // which [`git_metadata_files`] asks for before anything else there, and not
    // `.git` itself: what the guard promises is that some path under the
    // metadata is refused, not which one says it. Root is told past a mode, so
    // that row says what it cannot show; every other row here runs however this
    // one goes.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&git_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    if let Some(euid) = root_euid() {
        shout(&format!(
            "SKIP the mode-000 `.git`: this runs as root (euid {euid}), which is told past \
             a directory's mode and reads what an ordinary person is refused, so this \
             refusal cannot be shown here. Run this test as an unprivileged person to see \
             it."
        ));
        // The mode goes back before anything is compared again, so that every
        // answer below is read from the state the first one was.
        std::fs::set_permissions(&git_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    } else {
        let why = refusal_of(|| file_state(&checkout));
        std::fs::set_permissions(&git_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            why.contains(&format!(
                "cannot ask what {} is to compare it",
                git_dir.join("info").display()
            )) && why.contains("Permission denied"),
            "the metadata that cannot be searched was not refused for its own \
             reason: {why}"
        );
    }

    // The swap, twice over. The first is the ordinary one: the metadata moved
    // out and the very same directory linked back, so every path under it
    // answers exactly as it did before the move. The second is what a turn
    // makes: a copy of the metadata planted where the original was, its
    // configuration, hooks and ignore file holding this checkout's own names and
    // bytes, so that a comparison reading through the link reports them as this
    // checkout's own. Each swap is put back before the next row.
    for copy in [dir.join("git-elsewhere"), dir.join("planted-git")] {
        copy_tree(&git_dir, &copy);
        std::fs::remove_dir_all(&git_dir).unwrap();
        std::os::unix::fs::symlink(&copy, &git_dir).unwrap();
        assert_eq!(
            git_in(
                &checkout,
                &["status", "--porcelain", "--untracked-files=all"]
            ),
            clean,
            "git reports the metadata replaced by a link to {}",
            copy.display()
        );
        let swapped = repository_state(&checkout);
        let mut hidden: Vec<&str> = metadata
            .iter()
            .map(|line| line.split_once(' ').map_or(*line, |(path, _)| path))
            .chain(std::iter::once(".git"))
            .collect();
        hidden.sort();
        assert_eq!(
            left_behind(&whole, &swapped),
            hidden,
            "a `.git` replaced by a link to a copy of it is not reported as a \
             change: {}",
            summarize(file_lines(&swapped), ".git").join(", ")
        );
        assert_eq!(
            summarize(file_lines(&swapped), ".git"),
            [format!(".git link -> {}", copy.display()).as_str()],
            "the checkout's `.git` is not recorded as the link it is, with \
             nothing under the link read as this checkout's own: {}",
            summarize(file_lines(&swapped), ".git").join(", ")
        );
        // Put back for the next swap, byte for byte, from the copy itself.
        std::fs::remove_file(&git_dir).unwrap();
        copy_tree(&copy, &git_dir);
    }
    let back = repository_state(&checkout);
    assert_eq!(
        summarize(file_lines(&back), ".git"),
        metadata,
        "the metadata copied back is not read as it was: {}",
        left_behind(&whole, &back).join(", ")
    );
    // `.git` as an ordinary file where the directory was: no directory to read
    // under, so its own bytes are the whole of what the comparison may say about
    // it, and nothing under it is claimed. `git` cannot work at all here, so
    // this row asks the files and not the repository.
    std::fs::remove_dir_all(&git_dir).unwrap();
    std::fs::write(&git_dir, "not a repository\n").unwrap();
    assert_eq!(
        summarize(file_state(&checkout).lines(), ".git"),
        [format!(".git {}", digest(b"not a repository\n")).as_str()],
        "a `.git` that is no directory is not compared by its bytes alone"
    );

    // Every one of these undone, from the first copy: what the checkout holds
    // is what it held before, swap, file and mode included.
    std::fs::remove_file(&git_dir).unwrap();
    copy_tree(&dir.join("git-elsewhere"), &git_dir);
    let again = repository_state(&checkout);
    assert_eq!(
        again,
        whole,
        "the checkout is as it was again: {}",
        left_behind(&whole, &again).join(", ")
    );
    std::fs::remove_dir_all(dir.join("git-elsewhere")).unwrap();
    std::fs::remove_dir_all(dir.join("planted-git")).unwrap();
}

/// The typed text of the `target.unsafe` refusal, and its advice, as
/// `refuse_writes_outside_a_worktree` words them: asserted in full so the
/// tests fail if the refusal is removed or its wording changes.
const UNSAFE: &str = "which may write files, outside a worktree, so it would write in the \
                      project's own checkout";
const UNSAFE_ADVICE: &str = "run it from a workflow step with `worktree: true` and a `base` \
                            commit, or make the atom read-only";

/// `atomic runs show <id>`, which must answer that no run of that id is
/// recorded: a run refused as it was loaded started nothing, plain or `--json`.
async fn assert_nothing_recorded(dir: &Path, project: &Path, id: &str) {
    let out = atomic(
        dir,
        project,
        &["--json", "runs", "show", id],
        "http://127.0.0.1:9/v1",
        &[],
    )
    .await;
    assert!(
        !out.status.success(),
        "a run {id} was recorded: {}",
        text(&out)
    );
    let error = &stdout_json(&out)["error"];
    assert_eq!(error["kind"], "run.not_found", "{error}");
    assert!(
        error["description"]
            .as_str()
            .is_some_and(|d| d.contains(&format!("no run {id} "))),
        "{error}"
    );
}

/// A `workspace-write` Codex atom named directly is a step with no worktree,
/// so it would write in the developer's checkout: `atomic run` refuses it with
/// `target.unsafe` before the run starts, records no run, and leaves the
/// checkout exactly as it was. The refusal is made as the run is loaded, before
/// any turn runs, so this test needs no sandbox.
#[test]
fn a_writing_codex_atom_run_directly_in_a_checkout_is_refused() {
    on_codex_runtime(async {
        let dir = scratch("unsafe-direct");
        let (checkout, project) = project_in(&dir);
        // A JSON object, so the run reaches the write control; its missing
        // `base` is never checked, because the control refuses before the
        // input is. With the control removed, the run stops at the input
        // check — it never reaches a model.
        let data = json!({"task": "multiply"}).to_string();
        let before = repository_state(&checkout);
        let out = atomic(
            &dir,
            &project,
            &[
                "--json",
                "run",
                "fix-code",
                "--run-id",
                "refused-direct",
                "--data",
                &data,
            ],
            "http://127.0.0.1:9/v1",
            &[],
        )
        .await;
        assert!(!out.status.success(), "{}", text(&out));
        let error = &stdout_json(&out)["error"];
        assert_eq!(error["kind"], "target.unsafe", "{error}");
        let description = error["description"].as_str().expect("a description");
        assert_eq!(
            description,
            format!("step `run` runs atom `fix-code`, {UNSAFE}"),
            "{error}"
        );
        assert_eq!(error["action"], UNSAFE_ADVICE, "{error}");
        // Nothing ran, so the checkout is exactly as it was: its refs and
        // HEAD, its worktree list, its working tree with every untracked and
        // every ignored file, its configuration and hooks, and the bytes of
        // the very atom that was refused.
        let after = repository_state(&checkout);
        assert_eq!(
            after,
            before,
            "the checkout is as it was: {}",
            left_behind(&before, &after).join(", ")
        );
        assert_nothing_recorded(&dir, &project, "refused-direct").await;
        // And without `--json`, the same refusal reaches a person: a
        // description naming the write, and its advice on its own line.
        let out = atomic(
            &dir,
            &project,
            &[
                "run",
                "fix-code",
                "--run-id",
                "refused-plain",
                "--data",
                &data,
            ],
            "http://127.0.0.1:9/v1",
            &[],
        )
        .await;
        assert!(!out.status.success(), "{}", text(&out));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("error: step `run` runs atom `fix-code`,")
                && stderr.contains(UNSAFE)
                && stderr.contains(&format!("\n  to fix: {UNSAFE_ADVICE}")),
            "{stderr}"
        );
        let after = repository_state(&checkout);
        assert_eq!(
            after,
            before,
            "the plain run left the checkout as it was: {}",
            left_behind(&before, &after).join(", ")
        );
        assert_nothing_recorded(&dir, &project, "refused-plain").await;
        let _ = std::fs::remove_dir_all(&checkout);
    });
}

/// The same `workspace-write` Codex atom as a workflow step that names no
/// `worktree: true`: refused with `target.unsafe` before anything starts, with
/// nothing recorded, and with the checkout left exactly as it was. Like the
/// refusal of the same atom named directly, this is made as the run is loaded,
/// so it needs no sandbox.
#[test]
fn a_writing_codex_atom_in_a_step_without_a_worktree_is_refused() {
    on_codex_runtime(async {
        let dir = scratch("unsafe-step");
        let (checkout, project) = project_in(&dir);
        // `coding`, but its step names no worktree: the same atom, run in a
        // step that would write in the checkout. Committed with the rest, so
        // the checkout's state covers it too.
        write(
            &project.join("workflows/writable.workflow.yaml"),
            "atomic: 1\nname: writable\n\
             input: { $ref: ../atoms/fix-code.input.schema.json }\n\
             output: { $ref: ../atoms/fix-code.output.schema.json }\n\
             steps:\n  fix:\n    atom: fix-code\n    with: { base: input.base, task: input.task }\n\
             returns: steps.fix.output\n",
        );
        git_in(&checkout, &["add", "."]);
        git_in(
            &checkout,
            &["commit", "-q", "-m", "a step with no worktree"],
        );
        // A JSON object, so the step reaches the write control; its missing
        // `base` is never checked, because the control refuses before the
        // input is. With the control removed the run stops at the input check.
        let data = json!({"task": "multiply"}).to_string();
        let before = repository_state(&checkout);
        let out = atomic(
            &dir,
            &project,
            &[
                "--json",
                "run",
                "writable",
                "--run-id",
                "refused-step",
                "--data",
                &data,
            ],
            "http://127.0.0.1:9/v1",
            &[],
        )
        .await;
        assert!(!out.status.success(), "{}", text(&out));
        let error = &stdout_json(&out)["error"];
        assert_eq!(error["kind"], "target.unsafe", "{error}");
        assert_eq!(
            error["description"],
            format!("step `fix` runs atom `fix-code`, {UNSAFE}"),
            "{error}"
        );
        assert_eq!(error["action"], UNSAFE_ADVICE, "{error}");
        let after = repository_state(&checkout);
        assert_eq!(
            after,
            before,
            "the checkout is as it was: {}",
            left_behind(&before, &after).join(", ")
        );
        assert_nothing_recorded(&dir, &project, "refused-step").await;
        let _ = std::fs::remove_dir_all(&checkout);
    });
}

/// `atomic run` executes a Codex atom that writes — its turn runs a command
/// that creates a file — and the developer's checkout is left exactly as it
/// was: the write lands in the step's worktree, which atomic commits in the
/// fixture's snapshot, never in the checkout.
// needs_sandbox
#[cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#[test]
fn running_a_writing_codex_atom_leaves_the_checkout_exactly_as_it_was() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("wrote");
        let (checkout, project) = project_in(&dir);
        let before = repository_state(&checkout);
        let base = fixture(&dir.join("fixture"));
        // The turn writes a file in its worktree and prints it, then answers.
        let model = FakeModel::start(vec![
            Reply::Tool {
                name: "exec_command".into(),
                arguments: json!({"cmd": "echo written > note.txt; cat note.txt", "yield_time_ms": 20000}),
            },
            Reply::Text(r#"{"done": true}"#.into()),
        ])
        .await
        .unwrap();
        let data = json!({"base": base, "task": "write a note"}).to_string();
        let out = run_bound(
            &dir,
            &project,
            &[
                "--json", "run", "coding", "--run-id", "wrote-1", "--data", &data,
            ],
            &dir.join("fixture"),
            &model.base_url(),
            &[],
        )
        .await;
        assert!(out.status.success(), "{}", text(&out));
        let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(doc["output"], json!({"done": true}), "{doc}");
        // The turn's command ran at the top of its worktree and wrote there.
        assert!(
            printed(&model).contains("written"),
            "the turn did not write: {}",
            printed(&model)
        );
        let snapshots = snapshots(&dir);
        assert_eq!(snapshots.len(), 1, "{snapshots:?}");
        let commit = accepted(&snapshots[0], "wrote-1", "fix");
        assert_eq!(
            git_in(&snapshots[0], &["show", &format!("{commit}:note.txt")]),
            "written",
            "the file the turn wrote is not in the fixture's snapshot, at the \
             commit atomic accepted for its step"
        );
        // The developer's checkout is exactly as it was: its refs and where
        // HEAD stands, its worktree list, and the bytes of every file it holds
        // — tracked, untracked or ignored — with its configuration, its hooks
        // and its own ignore file, so nothing the turn wrote is anywhere in it.
        let after = repository_state(&checkout);
        assert_eq!(
            after,
            before,
            "the checkout is exactly as it was: {}",
            left_behind(&before, &after).join(", ")
        );
        // And the one file the turn was told to write, named: the comparison
        // above covers it, this says which path of the checkout answers to that
        // name.
        let strayed: Vec<PathBuf> = working_files(&checkout, Walk::TheCheckout)
            .iter()
            .filter(|found| found.path().file_name().is_some_and(|n| n == "note.txt"))
            .map(|found| found.path().to_owned())
            .collect();
        assert!(
            strayed.is_empty(),
            "the turn's file reached the checkout: {strayed:?}"
        );
        let _ = std::fs::remove_dir_all(&checkout);
    });
}

/// A read-only Codex atom named directly is not refused, and it is the one
/// Codex turn whose working directory is the developer's checkout: it has no
/// worktree to run in, and it may not write there. Its command reads a file of
/// the checkout and prints where it ran, and the checkout is left exactly as it
/// was — the turn read it and changed nothing, not a file, not a ref, not HEAD,
/// not its configuration.
// needs_sandbox
#[cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#[test]
fn a_read_only_codex_atom_run_directly_reads_the_checkout_and_leaves_it_as_it_was() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("read-only");
        let (checkout, project) = project_in(&dir);
        read_only_atom(&project);
        // A file of the checkout for the turn to read, at the top of the
        // project it runs in.
        write(&project.join("note.md"), "read me\n");
        git_in(&checkout, &["add", "."]);
        git_in(&checkout, &["commit", "-q", "-m", "a read-only atom"]);
        let before = repository_state(&checkout);
        // `pwd` is the point: a read-only Codex atom named directly runs where
        // its project sits, which is the checkout being compared. If that ever
        // stops being so, this test says so rather than pass for nothing.
        let model = FakeModel::start(vec![
            Reply::Tool {
                name: "exec_command".into(),
                arguments: json!({"cmd": "cat note.md; pwd", "yield_time_ms": 20000}),
            },
            Reply::Text(r#"{"done": true}"#.into()),
        ])
        .await
        .unwrap();
        let data = json!({"task": "read the note"}).to_string();
        let out = atomic(
            &dir,
            &project,
            &[
                "--json",
                "run",
                "read-note",
                "--run-id",
                "read-1",
                "--data",
                &data,
            ],
            &model.base_url(),
            &[],
        )
        .await;
        assert!(out.status.success(), "{}", text(&out));
        let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(doc["output"], json!({"done": true}), "{doc}");
        let said = printed(&model);
        assert!(
            said.contains("read me"),
            "the turn read no file of the checkout: {said}"
        );
        assert!(
            said.contains(&project.display().to_string()),
            "the turn did not run in the checkout: {said}"
        );
        let after = repository_state(&checkout);
        assert_eq!(
            after,
            before,
            "the checkout is exactly as it was: {}",
            left_behind(&before, &after).join(", ")
        );
        let _ = std::fs::remove_dir_all(&checkout);
    });
}

// needs_sandbox
#[cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#[test]
fn a_step_never_starts_from_a_commit_another_run_made_on_the_same_fixture() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("other-run");
        let project = project(&dir.join("project"));
        let base = fixture(&dir.join("fixture"));
        coding_run_at_the_top(&dir, &project, &dir.join("fixture"), &base).await;
        // The first run's commit is on top of the same bound commit, in the
        // same snapshot, and no git-commit field names it at admission.
        let theirs = accepted(&snapshots(&dir)[0], "fix-1", "fix");
        let model = FakeModel::start(vec![Reply::Text(r#"{"done": true}"#.into())])
            .await
            .unwrap();
        let data = json!({"from": theirs, "task": "multiply"}).to_string();
        let out = run_bound(
            &dir,
            &project,
            &["run", "relay", "--run-id", "relay-1", "--data", &data],
            &dir.join("fixture"),
            &model.base_url(),
            &[],
        )
        .await;
        assert!(!out.status.success(), "{}", text(&out));
        assert!(
            text(&out).contains("nor a commit an earlier step of this run made"),
            "{}",
            text(&out)
        );
        assert_eq!(model.calls(), 0, "no turn ran from another run's commit");
        let refs = git_in(
            &snapshots(&dir)[0],
            &["for-each-ref", "refs/atomic/relay-1/"],
        );
        assert!(refs.is_empty(), "{refs}");
    });
}

// needs_sandbox
#[cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#[test]
fn nothing_a_hostile_fixtures_configuration_names_ever_runs() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("hostile");
        let project = project(&dir.join("project"));
        let source = dir.join("fixture");
        let markers = dir.join("markers");
        std::fs::create_dir_all(&markers).unwrap();
        let mark =
            |name: &str| format!("#!/bin/sh\ntouch '{}'\ncat\n", markers.join(name).display());
        fixture(&source);
        // Committed, so a checkout of the fixture would give every file the
        // filter.
        write(&source.join(".gitattributes"), "* filter=evil\n");
        git_in(&source, &["add", ".gitattributes"]);
        git_in(&source, &["commit", "-q", "-m", "attributes"]);
        let base = git_in(&source, &["rev-parse", "HEAD"]);
        for hook in [
            "pre-commit",
            "post-commit",
            "post-checkout",
            "reference-transaction",
            "pre-auto-gc",
            "post-rewrite",
            "pre-push",
        ] {
            let path = source.join(".git/hooks").join(hook);
            write(&path, &mark(hook));
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
        }
        let script = |name: &str| {
            let path = dir.join(format!("{name}.sh"));
            write(&path, &mark(name));
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
            path.display().to_string()
        };
        for (key, value) in [
            ("core.fsmonitor", script("fsmonitor")),
            ("filter.evil.clean", script("clean")),
            ("filter.evil.smudge", script("smudge")),
            ("filter.evil.process", script("process")),
            (
                "core.hooksPath",
                source.join(".git/hooks").display().to_string(),
            ),
            ("remote.origin.url", format!("ext::{} %S", script("ext"))),
            (
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*".into(),
            ),
        ] {
            git_in(&source, &["config", key, &value]);
        }
        // The fixture's own git, as a person runs it, would run some of
        // these: the markers prove the test can see them.
        let model = FakeModel::start(vec![Reply::Text(r#"{"done": true}"#.into())])
            .await
            .unwrap();
        let data = json!({"base": base, "task": "multiply"}).to_string();
        let out = run_bound(
            &dir,
            &project,
            &[
                "--json",
                "run",
                "coding",
                "--run-id",
                "hostile-1",
                "--data",
                &data,
            ],
            &source,
            &model.base_url(),
            &[],
        )
        .await;
        assert!(out.status.success(), "{}", text(&out));
        let ran: Vec<_> = std::fs::read_dir(&markers).unwrap().flatten().collect();
        assert!(ran.is_empty(), "the fixture's configuration ran: {ran:?}");
        // The positive control: git as a person runs it does run them.
        write(&source.join("touched.txt"), "x\n");
        git_in(&source, &["add", "touched.txt"]);
        let ran: Vec<_> = std::fs::read_dir(&markers).unwrap().flatten().collect();
        assert!(!ran.is_empty(), "the markers are seen when git runs them");
    });
}

// needs_sandbox
#[cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#[test]
fn a_run_killed_after_a_step_goes_on_from_the_snapshot_once_its_source_is_gone() {
    support::require_sandbox();
    on_codex_runtime(async {
        let dir = scratch("resume");
        let project = project(&dir.join("project"));
        let base = fixture(&dir.join("fixture"));
        let model = FakeModel::start(vec![
            Reply::Text(r#"{"done": true}"#.into()),
            Reply::Text(r#"{"done": true}"#.into()),
        ])
        .await
        .unwrap();
        let data = json!({"base": base, "task": "multiply"}).to_string();
        let out = run_bound(
            &dir,
            &project,
            &["run", "twice", "--run-id", "twice-1", "--data", &data],
            &dir.join("fixture"),
            &model.base_url(),
            &[("ATOMIC_FAILPOINT", "pipeline:after:first")],
        )
        .await;
        assert!(
            !out.status.success(),
            "killed after the first step: {}",
            text(&out)
        );
        assert_eq!(model.calls(), 1);
        // The source goes; the run was pinned to its snapshot.
        std::fs::remove_dir_all(dir.join("fixture")).unwrap();
        let out = atomic(
            &dir,
            &project,
            &["runs", "resume", "twice-1"],
            &model.base_url(),
            &[],
        )
        .await;
        assert!(out.status.success(), "{}", text(&out));
        assert_eq!(model.calls(), 2, "the first step was not run again");
        let snapshot = &snapshots(&dir)[0];
        let first = accepted(snapshot, "twice-1", "first");
        let second = accepted(snapshot, "twice-1", "second");
        assert_eq!(git_in(snapshot, &["rev-parse", &format!("{first}^")]), base);
        assert_eq!(
            git_in(snapshot, &["rev-parse", &format!("{second}^")]),
            first
        );
    });
}
