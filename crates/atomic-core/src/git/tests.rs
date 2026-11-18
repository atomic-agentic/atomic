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
    for flag in ["--no-pager", "--no-replace-objects", "--literal-pathspecs"] {
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
    assert!(get("GIT_CONFIG_PARAMETERS").is_none());
}

#[test]
fn printable_error_text_has_no_control_characters() {
    assert_eq!(printable(b"fatal: a\x1b[31mb\n\n"), "fatal: a?[31mb");
    assert_eq!(printable(b"one\ntwo\r"), "one\ntwo?");
}
