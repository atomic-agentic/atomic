// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::project;

fn tree(files: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "atomic-collections-{}-{}",
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
    std::fs::canonicalize(&root).expect("root")
}

fn names(c: &BTreeMap<String, Vec<SourceFile>>, collection: &str) -> Vec<String> {
    c[collection].iter().map(|f| f.name.clone()).collect()
}

#[test]
fn overlapping_globs_load_an_atom_once() {
    // atoms/review.atom.md matches both default globs: atoms/**/*.atom.md and
    // **/*.atom.md.
    let root = tree(&[
        ("atomic.yaml", "atomic: 1\n"),
        ("atoms/review.atom.md", ""),
        ("src/billing/plan.atom.md", ""),
        ("workflows/release.workflow.yaml", ""),
        ("schemas/task.schema.yaml", ""),
        ("evals/review/missing-tests.yaml", ""),
        ("README.md", ""),
    ]);
    let c = collect(&project::discover(&root).expect("project")).expect("collect");
    assert_eq!(names(&c, "atoms"), ["plan", "review"]);
    assert_eq!(names(&c, "workflows"), ["release"]);
    assert_eq!(names(&c, "schemas"), ["task"]);
    assert_eq!(names(&c, "evals"), ["evals/review/missing-tests"]);
}

#[test]
fn duplicate_names_in_one_scope_fail_the_load() {
    let root = tree(&[
        ("atomic.yaml", "atomic: 1\n"),
        ("atoms/review.atom.md", ""),
        ("src/review.atom.md", ""),
    ]);
    match collect(&project::discover(&root).expect("project")) {
        Err(CollectError::DuplicateName {
            collection, name, ..
        }) => {
            assert_eq!((collection.as_str(), name.as_str()), ("atoms", "review"));
        }
        other => panic!("expected DuplicateName, got {other:?}"),
    }
}

#[test]
fn gitignore_ignore_globs_generated_files_and_nested_projects_are_skipped() {
    let root = tree(&[
        ("atomic.yaml", "atomic: 1\nignore: [\"vendor/**\"]\n"),
        (".gitignore", "build/\n"),
        ("atoms/keep.atom.md", ""),
        ("build/out.atom.md", ""),
        ("vendor/lib/x.atom.md", ""),
        (".atomic/cache/gen.atom.md", ""),
        ("tools/other/atomic.yaml", "atomic: 1\n"),
        ("tools/other/theirs.atom.md", ""),
    ]);
    let c = collect(&project::discover(&root).expect("project")).expect("collect");
    assert_eq!(names(&c, "atoms"), ["keep"]);
}

#[test]
fn a_supplied_entry_replaces_the_default_globs() {
    let root = tree(&[
        (
            "atomic.yaml",
            "atomic: 1\ncollections:\n  atoms: [\"agents/*.atom.md\"]\n",
        ),
        ("agents/a.atom.md", ""),
        ("atoms/b.atom.md", ""),
    ]);
    let c = collect(&project::discover(&root).expect("project")).expect("collect");
    assert_eq!(names(&c, "atoms"), ["a"]);
}
