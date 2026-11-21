// SPDX-License-Identifier: Apache-2.0

//! `atomic setup`, driven as a person drives it: through the `atomic`
//! executable, in a repository written while the test runs.
//!
//! - an extra file, an edited pack file, a link inside the skill directory and a
//!   link on the way to it each cost a refusal: status 1, one `error:` line
//!   naming the path and saying what it is, and a repository whose tree is
//!   afterwards exactly what it was before;
//! - what an earlier setup wrote is upgraded where it stands, because the
//!   manifest in the skill directory says those bytes are atomic's own;
//! - what is refused for a person is refused for a script as well, as the same
//!   text in the JSON `description`.
//!
//! The wording is asserted because it is the remedy: a refusal that names the
//! wrong thing to remove is as useless as no refusal.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ATOMIC: &str = env!("CARGO_BIN_EXE_atomic");
/// The skill the pack installs.
const SKILL: &str = "atomic-create-atom";

fn scratch(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "atomic-setup-cli-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

/// A home for atomic's own state, kept in a scratch directory of its own so that
/// nothing it creates is mistaken for the repository's.
fn home(tag: &str) -> PathBuf {
    let dir = scratch(tag).join("home");
    std::fs::create_dir_all(&dir).expect("home");
    dir
}

/// Runs `atomic setup <agent>` on `root` with a throwaway home, data directory
/// and working directory, and no model endpoint or key.
fn setup(agent: &str, root: &Path, home: &Path, json: bool) -> Output {
    let mut run = Command::new(ATOMIC);
    if json {
        run.arg("--json");
    }
    run.args(["setup", agent, "--path"])
        .arg(root)
        .current_dir(root)
        .env("HOME", home)
        .env("ATOMIC_HOME", home.join("atomic-home"))
        .env_remove("CODEX_HOME")
        .env_remove("XDG_DATA_HOME")
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("ATOMIC_DETECT_OLLAMA_ADDR", "127.0.0.1:9")
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("run atomic")
}

fn out(ran: &Output) -> String {
    String::from_utf8_lossy(&ran.stdout).into_owned()
}

fn err(ran: &Output) -> String {
    String::from_utf8_lossy(&ran.stderr).into_owned()
}

/// Everything below `root`, sorted, each entry with what it holds: a directory,
/// a link and its target, or a file and its bytes. Two equal lists mean nothing
/// below that directory was written, replaced, removed or changed.
fn tree(root: &Path) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    walk(root, Path::new(""), &mut entries);
    entries.sort();
    entries
}

fn walk(at: &Path, below: &Path, entries: &mut Vec<(String, String)>) {
    for entry in std::fs::read_dir(at).expect("read the directory") {
        let entry = entry.expect("entry");
        let under = below.join(entry.file_name());
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path).expect("entry type");
        let shown = if meta.is_dir() {
            walk(&path, &under, entries);
            "directory".to_owned()
        } else if meta.is_symlink() {
            format!(
                "link to {}",
                std::fs::read_link(&path)
                    .expect("a link has a target")
                    .display()
            )
        } else {
            format!("file {:?}", std::fs::read(&path).expect("read the file"))
        };
        entries.push((under.to_string_lossy().into_owned(), shown));
    }
}

/// What every refusal here owes: status 1, nothing on stdout, one `error:` line
/// saying exactly this, and the repository as it was found.
fn assert_refused(ran: &Output, says: &str, root: &Path, before: &[(String, String)]) {
    let error = err(ran);
    assert_eq!(
        ran.status.code(),
        Some(1),
        "expected a failed run; stdout {} stderr {error}",
        out(ran)
    );
    assert!(
        out(ran).is_empty(),
        "a refusal wrote to stdout: {}",
        out(ran)
    );
    assert!(
        error.contains(&format!("error: {says}")),
        "not the refusal expected: {error}"
    );
    assert_eq!(
        error.matches("error:").count(),
        1,
        "more than one error line: {error}"
    );
    assert_eq!(
        tree(root),
        before,
        "the refusal wrote, replaced or removed something"
    );
}

/// The hex sha256 of `bytes`, which is how the manifest records a file.
fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    let mut hash = sha2::Sha256::new();
    hash.update(bytes);
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn an_extra_file_in_the_skill_directory_is_refused_and_nothing_is_written() {
    let root = scratch("extra");
    let home = home("extra");
    let extra = root.join(".claude/skills").join(SKILL).join("extra.md");
    std::fs::create_dir_all(extra.parent().expect("a parent")).expect("skill directory");
    std::fs::write(&extra, "mine").expect("write");
    let before = tree(&root);
    let ran = setup("claude", &root, &home, false);
    assert_refused(
        &ran,
        ".claude/skills/atomic-create-atom holds what atomic did not write (an older version \
         of atomic may be the cause): .claude/skills/atomic-create-atom/extra.md is not a \
         file atomic writes; remove what is named and run `atomic setup` again",
        &root,
        &before,
    );
    assert!(
        !root
            .join(".claude/skills")
            .join(SKILL)
            .join("SKILL.md")
            .exists(),
        "the skill was installed anyway"
    );
    assert!(
        !root.join("CLAUDE.md").exists(),
        "the agent was told to use it"
    );
    assert_eq!(std::fs::read_to_string(&extra).expect("theirs"), "mine");
}

/// The same refusal for a script: the reason is the `description` field of one
/// JSON object on stdout, and nothing is written either.
#[test]
fn a_refusal_says_the_same_thing_to_a_script_in_json() {
    let root = scratch("json");
    let home = home("json");
    let extra = root.join(".agents/skills").join(SKILL).join("extra.md");
    std::fs::create_dir_all(extra.parent().expect("a parent")).expect("skill directory");
    std::fs::write(&extra, "mine").expect("write");
    let before = tree(&root);
    let ran = setup("codex", &root, &home, true);
    let error = err(&ran);
    assert_eq!(ran.status.code(), Some(1), "stderr {error}");
    assert!(error.is_empty(), "a JSON run wrote to stderr: {error}");
    let said = out(&ran);
    let line = said
        .lines()
        .find(|line| line.contains("\"description\""))
        .unwrap_or_else(|| panic!("no error object on stdout: {said}"));
    let json: serde_json::Value = serde_json::from_str(line).expect("one JSON object");
    let description = json["error"]["description"]
        .as_str()
        .expect("a description field");
    assert!(
        description.contains(
            ".agents/skills/atomic-create-atom/extra.md is not a file atomic writes; remove \
             what is named and run `atomic setup` again"
        ),
        "{description}"
    );
    assert!(
        json["error"]["action"]
            .as_str()
            .is_some_and(|action| !action.is_empty()),
        "{json}"
    );
    assert_eq!(tree(&root), before, "the refusal wrote something");
}

/// A file atomic wrote and the person has since changed is not atomic's to throw
/// away: the setup is refused, the edit stays, and nothing else moves.
#[test]
fn an_edited_pack_file_is_refused_and_kept() {
    let root = scratch("edited");
    let home = home("edited");
    let first = setup("codex", &root, &home, false);
    assert!(
        first.status.success(),
        "the first setup failed: {}",
        err(&first)
    );
    let skill = root.join(".agents/skills").join(SKILL);
    let theirs = "what the person added to the skill\n";
    std::fs::write(skill.join("SKILL.md"), theirs).expect("write");
    let before = tree(&root);
    let ran = setup("codex", &root, &home, false);
    assert_refused(
        &ran,
        ".agents/skills/atomic-create-atom holds what atomic did not write (an older version \
         of atomic may be the cause): .agents/skills/atomic-create-atom/SKILL.md no longer \
         holds what atomic wrote there; remove what is named and run `atomic setup` again",
        &root,
        &before,
    );
    assert_eq!(
        std::fs::read_to_string(skill.join("SKILL.md")).expect("read"),
        theirs,
        "the edit was written over"
    );
}

/// A link inside the directory is refused and left alone: what it points at is
/// neither read nor written.
#[cfg(unix)]
#[test]
fn a_link_inside_the_skill_directory_is_refused_and_not_followed() {
    let root = scratch("inside-link");
    let home = home("inside-link");
    let elsewhere = scratch("elsewhere");
    let theirs = elsewhere.join("someone-elses.md");
    std::fs::write(&theirs, "theirs").expect("write");
    let skill = root.join(".claude/skills").join(SKILL);
    std::fs::create_dir_all(skill.join("references")).expect("references");
    std::os::unix::fs::symlink(&theirs, skill.join("references").join("fields.md")).expect("link");
    let before = tree(&root);
    let ran = setup("claude", &root, &home, false);
    assert_refused(
        &ran,
        ".claude/skills/atomic-create-atom holds what atomic did not write: \
         .claude/skills/atomic-create-atom/references/fields.md is a symbolic link, and atomic \
         never writes through one; remove what is named and run `atomic setup` again",
        &root,
        &before,
    );
    assert_eq!(
        std::fs::read_to_string(&theirs).expect("read"),
        "theirs",
        "what the link points at was touched"
    );
}

/// A link on the way to the skill directory is refused where it stands, and the
/// advice is about the link: the directory holding it is the person's own.
#[cfg(unix)]
#[test]
fn a_link_on_the_way_to_the_skill_directory_is_refused() {
    let root = scratch("chain-link");
    let home = home("chain-link");
    let shared = root.join("shared/.claude/skills").join(SKILL);
    std::fs::create_dir_all(&shared).expect("shared");
    std::fs::write(shared.join("extra.md"), "mine").expect("write");
    std::os::unix::fs::symlink(root.join("shared/.claude"), root.join(".claude")).expect("link");
    let before = tree(&root);
    let ran = setup("claude", &root, &home, false);
    assert_refused(
        &ran,
        ".claude/skills/atomic-create-atom holds what atomic did not write: .claude is a \
         symbolic link, and atomic never writes through one; remove what is named and run \
         `atomic setup` again",
        &root,
        &before,
    );
    assert!(
        !err(&ran).contains("whole directory"),
        "the advice told the person to remove their own directory: {}",
        err(&ran)
    );
    assert_eq!(
        std::fs::read_to_string(shared.join("extra.md")).expect("theirs"),
        "mine"
    );
}

/// What an upgrade meets, and what it owes: a directory an earlier setup wrote,
/// whose pack this binary no longer matches. The manifest says those bytes are
/// atomic's own, so setup replaces them where they stand, drops the file this
/// pack no longer ships, and leaves the rest of the repository alone.
#[test]
fn a_pack_written_by_an_older_setup_is_upgraded_where_it_stands() {
    let root = scratch("upgrade");
    let home = home("upgrade");
    let first = setup("claude", &root, &home, false);
    assert!(
        first.status.success(),
        "the first setup failed: {}",
        err(&first)
    );
    let skill = root.join(".claude/skills").join(SKILL);
    let at = skill.join(".atomic-pack.json");
    let pack_skill = std::fs::read(skill.join("SKILL.md")).expect("the installed SKILL.md");
    // The state an older release leaves behind: its own bytes, and a manifest
    // recording exactly those, which is what says atomic wrote them.
    let mut record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&at).expect("the manifest"))
            .expect("what setup wrote is JSON");
    let older = "---\nname: atomic-create-atom\n---\nan older release\n";
    let gone = "<!-- a reference an older release shipped -->\n";
    std::fs::write(skill.join("SKILL.md"), older).expect("write");
    std::fs::write(skill.join("references/gone.md"), gone).expect("write");
    record["files"]["SKILL.md"] = serde_json::Value::String(sha256(older.as_bytes()));
    record["files"]["references/gone.md"] = serde_json::Value::String(sha256(gone.as_bytes()));
    std::fs::write(&at, serde_json::to_string_pretty(&record).expect("JSON")).expect("write");
    let instructions = "# My project\n";
    std::fs::write(root.join("CLAUDE.md"), instructions).expect("write");
    let ran = setup("claude", &root, &home, false);
    assert_eq!(
        ran.status.code(),
        Some(0),
        "the upgrade was refused: {}",
        err(&ran)
    );
    assert_eq!(
        std::fs::read(skill.join("SKILL.md")).expect("read"),
        pack_skill,
        "the pack this binary carries was not installed over the older one"
    );
    assert!(
        !skill.join("references/gone.md").exists(),
        "the file an older pack shipped, unmodified, stayed"
    );
    let now: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&at).expect("read"))
        .expect("what setup wrote is JSON");
    assert_eq!(
        now["files"]["SKILL.md"].as_str(),
        Some(sha256(&pack_skill)).as_deref(),
        "the new record does not describe what setup wrote"
    );
    assert!(
        now["files"].get("references/gone.md").is_none(),
        "the new record still lists the file it removed"
    );
    let text = std::fs::read_to_string(root.join("CLAUDE.md")).expect("read");
    assert!(text.starts_with(instructions), "{text}");
    assert_eq!(text.matches("<!-- BEGIN atomic").count(), 1, "{text}");
}
