// SPDX-License-Identifier: Apache-2.0

use super::*;

static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
use crate::project;

fn tree(files: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "atomic-collections-{}-{}-{}",
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
    std::fs::canonicalize(&root).expect("root")
}

fn names(c: &BTreeMap<String, Vec<SourceFile>>, collection: &str) -> Vec<String> {
    c[collection].iter().map(|f| f.name.clone()).collect()
}

#[test]
fn overlapping_globs_load_an_atom_once() {
    // atoms/review.atom.astro matches both default globs: atoms/**/*.atom.astro and
    // **/*.atom.astro.
    let root = tree(&[
        ("atomic.yaml", "atomic: 1\n"),
        ("atoms/review.atom.astro", ""),
        ("src/billing/plan.atom.astro", ""),
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
        ("atoms/review.atom.astro", ""),
        ("src/review.atom.astro", ""),
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
        ("atoms/keep.atom.astro", ""),
        ("build/out.atom.astro", ""),
        ("vendor/lib/x.atom.astro", ""),
        (".atomic/cache/gen.atom.astro", ""),
        ("tools/other/atomic.yaml", "atomic: 1\n"),
        ("tools/other/theirs.atom.astro", ""),
    ]);
    let c = collect(&project::discover(&root).expect("project")).expect("collect");
    assert_eq!(names(&c, "atoms"), ["keep"]);
}

#[test]
fn a_supplied_entry_replaces_the_default_globs() {
    let root = tree(&[
        (
            "atomic.yaml",
            "atomic: 1\ncollections:\n  atoms: [\"agents/*.atom.astro\"]\n",
        ),
        ("agents/a.atom.astro", ""),
        ("atoms/b.atom.astro", ""),
    ]);
    let c = collect(&project::discover(&root).expect("project")).expect("collect");
    assert_eq!(names(&c, "atoms"), ["a"]);
}

#[test]
fn removed_atom_formats_fail_instead_of_silently_disappearing_from_discovery() {
    for suffix in ["md", "mdx"] {
        let file = format!("atoms/old.atom.{suffix}");
        let root = tree(&[("atomic.yaml", "atomic: 1\n"), (&file, "old source")]);
        assert!(matches!(
            collect(&project::discover(&root).expect("project")),
            Err(CollectError::LegacyAtom { .. })
        ));
    }
}
