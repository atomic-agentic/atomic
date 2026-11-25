// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn only_hex_commits_pass() {
    let sha1 = "0123456789abcdef0123456789abcdef01234567";
    assert!(is_sha(sha1) && is_sha(&"a".repeat(64)));
    assert!(is_sha_prefix("0123456") && is_sha_prefix(sha1));
    for bad in [
        "",
        "012345",
        "HEAD",
        "HEAD~1",
        "main",
        "@{-1}",
        ":/fix",
        "--output=/tmp/synthetic",
        "-p",
        "0123456789ABCDEF0123456789abcdef01234567",
        "0123456789abcdef0123456789abcdef0123456g",
        "0123456^{tree}",
        "0123456..89abcde",
        &"a".repeat(65),
    ] {
        assert!(!is_sha(bad), "{bad}");
        assert!(!is_sha_prefix(bad) || bad == "0123456", "{bad}");
    }
    assert!(!is_sha(&"a".repeat(41)));
}

#[test]
fn hostile_paths_are_refused() {
    for good in [
        "a.txt",
        "src/a b.rs",
        "dir/sub/file",
        ":(glob)x",
        "a:b",
        "é/ü.rs",
    ] {
        assert!(is_git_path(good), "{good}");
    }
    for bad in [
        "",
        "-x",
        "--output=/tmp/synthetic",
        "/etc/passwd",
        "../outside",
        "a/../../b",
        "a/./b",
        "a//b",
        "a/",
        "a\nb",
        "a\0b",
        "a\tb",
    ] {
        assert!(!is_git_path(bad), "{bad:?}");
    }
    assert!(!is_git_path(&"a".repeat(4097)));
}

#[test]
fn hostile_refs_are_refused() {
    for good in ["main", "release/1.2", "v1.0.0", "feature_x-y"] {
        assert!(is_ref_name(good), "{good}");
    }
    for bad in [
        "",
        "-v",
        "--upload-pack=/tmp/synthetic",
        "a..b",
        "HEAD@{1}",
        "main^",
        "main~2",
        "a b",
        ".hidden",
        "a/.b",
        "x.lock",
        "a//b",
        "a/",
        "/a",
        "a.",
        "a:b",
        "a\\b",
        "a*",
        "a?",
        "a[",
    ] {
        assert!(!is_ref_name(bad), "{bad:?}");
    }
}

#[test]
fn every_command_is_hardened() {
    let repo = Git::at("/usr/bin/git").worktree("/repo/.git/worktrees/w", "/state/w");
    let command = repo.command();
    let args: Vec<String> = command
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    for setting in HARDENING {
        assert!(
            args.windows(2).any(|w| w[0] == "-c" && w[1] == *setting),
            "{setting} is missing: {args:?}"
        );
    }
    for flag in [
        "--no-pager",
        "--no-replace-objects",
        "--literal-pathspecs",
        "--no-lazy-fetch",
    ] {
        assert!(args.iter().any(|a| a == flag), "{flag}");
    }
    let env: Vec<(String, Option<String>)> = command
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    let get = |name: &str| {
        env.iter()
            .find(|(k, _)| k == name)
            .and_then(|(_, v)| v.clone())
    };
    // The repository is named explicitly; the worktree is never searched.
    assert_eq!(get("GIT_DIR").as_deref(), Some("/repo/.git/worktrees/w"));
    assert_eq!(get("GIT_WORK_TREE").as_deref(), Some("/state/w"));
    assert_eq!(get("GIT_CONFIG_GLOBAL").as_deref(), Some("/dev/null"));
    assert_eq!(get("GIT_CONFIG_NOSYSTEM").as_deref(), Some("1"));
    assert_eq!(get("GIT_ALLOW_PROTOCOL").as_deref(), Some("none"));
    // A missing object is never fetched from a promisor remote.
    assert_eq!(get("GIT_NO_LAZY_FETCH").as_deref(), Some("1"));
    assert!(get("GIT_CONFIG_PARAMETERS").is_none());
}

#[test]
fn printable_error_text_has_no_control_characters() {
    assert_eq!(printable(b"fatal: a\x1b[31mb\n\n"), "fatal: a?[31mb");
    assert_eq!(printable(b"one\ntwo\rthree\r"), "one\ntwo?three");
}

/// A scratch directory of the test's own.
pub(crate) fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("atomic-git-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    std::fs::canonicalize(dir).expect("scratch")
}

/// git as a person runs it, with no configuration of theirs, in `dir`.
pub(crate) fn plain(dir: &Path, args: &[&str]) -> String {
    let git = Git::find().expect("git");
    let out = std::process::Command::new(git.program())
        .args(["-c", "user.name=dev", "-c", "user.email=dev@example.com"])
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "protocol.file.allow=always",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A repository with two commits, the second holding `a.txt` and
/// `-dir/.gitattributes`; returns the repository and both commits.
pub(crate) fn history(dir: &Path) -> (PathBuf, String, String) {
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join("-dir")).expect("repo");
    plain(&repo, &["init", "-q"]);
    std::fs::write(repo.join("a.txt"), "one\n").expect("file");
    plain(&repo, &["add", "a.txt"]);
    plain(&repo, &["commit", "-q", "-m", "one"]);
    let first = plain(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("a.txt"), "two\n").expect("file");
    std::fs::write(repo.join("-dir/.gitattributes"), "*.bin -text\n").expect("attributes");
    plain(&repo, &["add", "."]);
    plain(&repo, &["commit", "-q", "-m", "two"]);
    let second = plain(&repo, &["rev-parse", "HEAD"]);
    (repo, first, second)
}

#[test]
fn review_diff_is_the_host_patch_between_recorded_commits() {
    let dir = scratch("review-diff");
    let (source, first, second) = history(&dir);
    let repo = Git::find().expect("git").discover(&source).expect("repo");
    let patch = repo.review_diff(&first, &second).expect("patch");
    assert!(patch.contains("-one\n"), "{patch}");
    assert!(patch.contains("+two\n"), "{patch}");
    assert!(
        patch.contains("a/a.txt") && patch.contains("b/a.txt"),
        "{patch}"
    );
    assert_eq!(repo.review_diff(&first, &first).expect("same commit"), "");
    assert!(matches!(
        repo.review_diff("--output=/tmp/forbidden", &second),
        Err(GitError::Invalid(_))
    ));
    std::fs::write(source.join("oversized.txt"), "x".repeat(600_000)).expect("large file");
    plain(&source, &["add", "oversized.txt"]);
    plain(&source, &["commit", "-q", "-m", "large"]);
    let third = plain(&source, &["rev-parse", "HEAD"]);
    assert!(matches!(
        repo.review_diff(&second, &third),
        Err(GitError::TooLarge(_))
    ));
}

#[test]
fn a_bare_repository_starts_from_no_template() {
    let dir = scratch("bare");
    let repo = Git::find()
        .expect("git")
        .init_bare(&dir.join("b.git"))
        .expect("init");
    assert!(dir.join("b.git/HEAD").is_file());
    let hooks: Vec<_> = std::fs::read_dir(dir.join("b.git/hooks"))
        .map(|d| d.flatten().collect())
        .unwrap_or_default();
    assert!(hooks.is_empty(), "no sample hooks: {hooks:?}");
    assert_eq!(repo.object_format().expect("format"), "sha1");
}

/// While something is at the home git is given, which must not exist, no
/// git runs: not one run through `Git::run`, nor the two `copy_reachable`
/// starts itself, which copies nothing.
#[test]
fn no_git_runs_while_its_home_is_there() {
    let dir = scratch("home-there");
    let (source, first, _) = history(&dir);
    let git = Git::find().expect("git");
    let from = git.discover(&source).expect("source");
    let to = git.init_bare(&dir.join("copy.git")).expect("init");
    let there = dir.join("home");
    std::fs::create_dir(&there).expect("home");
    let refused = |result: Result<(), GitError>, what: &str| match result {
        Err(GitError::Home(why)) => assert!(
            why.starts_with(&format!(
                "{} is the home atomic gives commands",
                there.display()
            )),
            "{what}: {why}"
        ),
        other => panic!("{what}: git ran: {other:?}"),
    };
    {
        let _home = crate::codex::ShellHomeAt::new(&there);
        refused(from.object_format().map(|_| ()), "a run");
        refused(from.copy_reachable(&first, &to), "a copy");
    }
    assert!(!to.has_everything(&first).expect("walk"), "the copy ran");
}

#[test]
fn every_object_a_commit_reaches_is_copied_and_nothing_else() {
    let dir = scratch("copy");
    let (source, first, second) = history(&dir);
    let git = Git::find().expect("git");
    let from = git.discover(&source).expect("source");
    let to = git.init_bare(&dir.join("copy.git")).expect("init");
    from.copy_reachable(&first, &to).expect("copy");
    assert!(to.has_everything(&first).expect("walk"));
    assert!(
        !to.has_everything(&second).expect("walk"),
        "only what it reaches"
    );
    from.copy_reachable(&second, &to).expect("copy");
    assert!(to.has_everything(&second).expect("walk"));
    assert!(to.is_ancestor(&first, &second).expect("ancestry"));
    assert!(!to.is_ancestor(&second, &first).expect("ancestry"));
    assert!(
        to.is_ancestor(&second, &second).expect("ancestry"),
        "a commit is its own"
    );
}

#[test]
fn a_tree_is_listed_with_its_ids_and_a_blob_read_by_its_id() {
    let dir = scratch("entries");
    let (source, _, second) = history(&dir);
    let repo = Git::find().expect("git").discover(&source).expect("repo");
    let entries = repo.entries(&second).expect("entries");
    let attributes = entries
        .iter()
        .find(|e| e.path == "-dir/.gitattributes")
        .unwrap_or_else(|| panic!("{entries:?}"));
    assert_eq!(
        (attributes.mode.as_str(), attributes.kind.as_str()),
        ("100644", "blob")
    );
    // A path starting with `-` is read the same, by its ID.
    assert_eq!(
        repo.blob_by_id(&attributes.id).expect("blob"),
        b"*.bin -text\n"
    );
    assert!(entries.iter().any(|e| e.path == "a.txt"));
}

#[test]
fn a_shallow_copy_is_found_out() {
    let dir = scratch("shallow");
    let (source, _, _) = history(&dir);
    plain(
        &dir,
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &format!("file://{}", source.display()),
            "shallow",
        ],
    );
    let git = Git::find().expect("git");
    assert!(
        git.discover(&dir.join("shallow"))
            .expect("repo")
            .is_shallow()
            .expect("query")
    );
    assert!(
        !git.discover(&source)
            .expect("repo")
            .is_shallow()
            .expect("query")
    );
    // A copy of a shallow history is missing what it was cut from, and the
    // copy's own check finds that out.
    let shallow = git.discover(&dir.join("shallow")).expect("repo");
    let head = plain(&dir.join("shallow"), &["rev-parse", "HEAD"]);
    let to = git.init_bare(&dir.join("copy.git")).expect("init");
    shallow.copy_reachable(&head, &to).expect("copy");
    assert!(!to.has_everything(&head).expect("walk"));
}

#[test]
fn a_linked_worktree_has_a_git_directory_of_its_own() {
    let dir = scratch("checkout");
    let (source, _, _) = history(&dir);
    plain(
        &source,
        &["worktree", "add", "-q", "-b", "other", "../linked"],
    );
    let git = Git::find().expect("git");
    let main = git.checkout_at(&source).expect("main");
    assert_eq!(main.top.as_deref(), Some(source.as_path()));
    let linked = git.checkout_at(&dir.join("linked")).expect("linked");
    assert_eq!(linked.top.as_deref(), Some(dir.join("linked").as_path()));
    assert_eq!(linked.common_dir, main.common_dir);
    assert_ne!(linked.git_dir, main.git_dir);
    plain(&dir, &["init", "-q", "--bare", "bare.git"]);
    assert_eq!(
        git.checkout_at(&dir.join("bare.git")).expect("bare").top,
        None
    );
}
