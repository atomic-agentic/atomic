// SPDX-License-Identifier: Apache-2.0

use super::*;

fn tree(files: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "atomic-project-{}-{}",
        std::process::id(),
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
    assert_eq!(p.collections["atoms"][0], "atoms/**/*.atom.md");
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
        ("tools/inner/atoms/x.atom.md", ""),
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
        "atomic: 1\ncollections:\n  atoms: [\"agents/*.atom.md\"]\n",
    )]);
    let p = discover(&root).expect("discover");
    assert_eq!(p.collections["atoms"], ["agents/*.atom.md"]);
    assert_eq!(
        p.collections["workflows"][0],
        "workflows/**/*.workflow.yaml"
    );
}
