// SPDX-License-Identifier: Apache-2.0

use super::*;

static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn tree(files: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "atomic-project-{}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    for (path, text) in files {
        let p = root.join(path);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(&p, text).expect("write");
    }
    std::fs::create_dir_all(&root).expect("root");
    std::fs::canonicalize(&root).expect("canonical root")
}

#[test]
fn a_project_is_found_from_a_subdirectory() {
    let root = tree(&[
        ("atomic.yaml", "atomic: 1\nname: billing\n"),
        ("src/deep/x.rs", ""),
    ]);
    let p = discover(&root.join("src/deep")).expect("discover");
    assert_eq!(p.manifest, root.join("atomic.yaml"));
    assert_eq!(p.dir, root);
    assert_eq!(p.name, "billing");
    assert_eq!(p.collections["atoms"][0], "atoms/**/*.atom.astro");
    assert_eq!(p.generated, PathBuf::from(".atomic/cache"));
}

#[test]
fn an_embedded_project_is_found_under_dot_atomic() {
    let root = tree(&[(".atomic/atomic.yaml", "atomic: 1\n"), ("src/main.rs", "")]);
    let p = discover(&root.join("src")).expect("discover");
    assert_eq!(p.manifest, root.join(".atomic/atomic.yaml"));
    assert_eq!(p.dir, root.join(".atomic"));
    assert_eq!(p.root(), root);
    // With no name, the repository's directory names the project.
    assert_eq!(p.name, root.file_name().expect("name").to_string_lossy());
}

#[test]
fn a_directory_with_both_manifest_forms_fails_clearly() {
    let root = tree(&[
        ("atomic.yaml", "atomic: 1\n"),
        (".atomic/atomic.yaml", "atomic: 1\n"),
    ]);
    let e = discover(&root).expect_err("both forms");
    assert!(
        matches!(e, ProjectError::BothManifests(ref d) if *d == root),
        "{e}"
    );
    assert!(e.to_string().contains("keep one"), "{e}");
}

#[test]
fn a_nested_manifest_bounds_discovery() {
    let root = tree(&[
        ("atomic.yaml", "atomic: 1\nname: outer\n"),
        ("tools/inner/atomic.yaml", "atomic: 1\nname: inner\n"),
        ("tools/inner/atoms/x.atom.astro", ""),
    ]);
    assert_eq!(
        discover(&root.join("tools/inner/atoms"))
            .expect("inner")
            .name,
        "inner"
    );
    assert_eq!(discover(&root.join("tools")).expect("outer").name, "outer");
}

#[test]
fn no_manifest_is_not_found() {
    let root = tree(&[("src/a.rs", "")]);
    // Walking up may meet a manifest above the temp dir on an odd machine;
    // only a NotFound or a manifest outside this tree is acceptable.
    match find_manifest(&root.join("src")) {
        Err(ProjectError::NotFound(_)) => {}
        Ok(found) => assert!(!found.starts_with(&root), "{}", found.display()),
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn the_manifest_must_carry_a_format_version_this_atomic_reads() {
    for (text, needle, line) in [
        ("name: x\n", "add `atomic: 1`", 1),
        ("name: x\natomic: 2\n", "format version 2", 2),
        ("atomic: \"1\"\n", "a number", 1),
    ] {
        let root = tree(&[("atomic.yaml", text)]);
        match discover(&root) {
            Err(ProjectError::Invalid {
                line: l, message, ..
            }) => {
                assert!(message.contains(needle), "{text:?}: {message}");
                assert_eq!(l, line, "{text:?}");
            }
            other => panic!("{text:?}: expected Invalid, got {other:?}"),
        }
    }
}

#[test]
fn unknown_and_unsupported_keys_are_refused_with_their_line() {
    let root = tree(&[("atomic.yaml", "atomic: 1\nname: x\ncolections: {}\n")]);
    let e = discover(&root).expect_err("typo");
    assert!(matches!(e, ProjectError::Invalid { line: 3, .. }), "{e}");
    let root = tree(&[("atomic.yaml", "atomic: 1\npackages: {}\n")]);
    assert!(
        discover(&root)
            .expect_err("later")
            .to_string()
            .contains("not supported")
    );
}

#[test]
fn a_collection_override_replaces_only_that_collection() {
    let root = tree(&[(
        "atomic.yaml",
        "atomic: 1\ncollections:\n  atoms: [\"agents/*.atom.astro\"]\n",
    )]);
    let p = discover(&root).expect("discover");
    assert_eq!(p.collections["atoms"], ["agents/*.atom.astro"]);
    assert_eq!(
        p.collections["workflows"][0],
        "workflows/**/*.workflow.yaml"
    );
}

const WORKSPACE: &[(&str, &str)] = &[
    (
        "atomic.yaml",
        "atomic: 1\nname: engineering\nworkspace:\n  members: [shared, services/*]\n",
    ),
    ("shared/atomic.yaml", "atomic: 1\nname: shared\n"),
    ("services/billing/atomic.yaml", "atomic: 1\nname: billing\n"),
    (
        "services/payments/atomic.yaml",
        "atomic: 1\nname: payments\n",
    ),
    ("services/notes.txt", ""),
    ("tools/atomic.yaml", "atomic: 1\nname: tools\n"),
];

#[test]
fn a_workspace_lists_its_members_by_glob() {
    let root = tree(WORKSPACE);
    let workspace = workspace_of(&discover(&root).expect("discover"))
        .expect("workspace")
        .expect("a workspace");
    let mut names: Vec<_> = workspace.members.iter().map(|m| m.name.as_str()).collect();
    names.sort_unstable();
    // `services/notes.txt` is not a directory and `tools` is not listed.
    assert_eq!(names, ["billing", "payments", "shared"]);
}

#[test]
fn a_member_finds_its_workspace_and_a_project_outside_it_does_not() {
    let root = tree(WORKSPACE);
    let billing = discover(&root.join("services/billing")).expect("discover");
    assert_eq!(billing.name, "billing");
    let workspace = workspace_of(&billing)
        .expect("workspace")
        .expect("a member");
    assert_eq!(workspace.root.name, "engineering");
    let mut names: Vec<_> = workspace.members.iter().map(|m| m.name.clone()).collect();
    names.sort();
    assert_eq!(names, ["billing", "payments", "shared"]);
    // `tools` has a manifest but is not a member.
    let tools = discover(&root.join("tools")).expect("discover");
    assert!(workspace_of(&tools).expect("workspace").is_none());
    // A plain project between a member and the workspace bounds the search.
    let nested = tree(&[
        (
            "atomic.yaml",
            "atomic: 1\nname: engineering\nworkspace:\n  members: [team/*]\n",
        ),
        ("team/atomic.yaml", "atomic: 1\nname: team\n"),
        ("team/app/atomic.yaml", "atomic: 1\nname: app\n"),
    ]);
    let app = discover(&nested.join("team/app")).expect("discover");
    assert!(workspace_of(&app).expect("workspace").is_none());
}

#[test]
fn workspace_mistakes_are_refused() {
    for (manifest, says) in [
        (
            "atomic: 1\nworkspace:\n  members: []\n",
            "lists member directories",
        ),
        (
            "atomic: 1\nworkspace:\n  members: [../elsewhere]\n",
            "inside the workspace",
        ),
        (
            "atomic: 1\nworkspace:\n  members: [shared]\n  exports: []\n",
            "not supported",
        ),
        (
            "atomic: 1\nworkspace: [shared]\n",
            "a mapping with `members`",
        ),
    ] {
        let root = tree(&[("atomic.yaml", manifest)]);
        let error = discover(&root).expect_err(manifest).to_string();
        assert!(error.contains(says), "{manifest}: {error}");
    }
    let root = tree(&[(
        "atomic.yaml",
        "atomic: 1\nworkspace:\n  members: [shared]\n",
    )]);
    let error = workspace_of(&discover(&root).expect("discover")).expect_err("no manifest");
    assert!(
        error.to_string().contains("holds no atomic.yaml"),
        "{error}"
    );
}
