// SPDX-License-Identifier: Apache-2.0

//! Every test reaches the PostgreSQL test database through
//! `atomic_durable::testing`, and only this package's tests do.
//!
//! `testing::schema` gives each test a schema it drops afterwards, and
//! `testing::database` fails a test, naming what is missing, when there is
//! no database. A test that read `ATOMIC_TEST_DATABASE_URL` itself would do
//! neither. And scripts/backends-check.sh finds the Postgres tests by running
//! this package's tests without the database, so a test elsewhere that used
//! the database would escape it.

use std::path::{Path, PathBuf};

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read the source tree") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn only_the_testing_module_reads_the_database_variable_and_only_this_package_uses_it() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crates directory")
        .to_owned();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&crates).expect("crates") {
        let dir = entry.expect("entry").path();
        for sub in ["src", "tests"] {
            if dir.join(sub).is_dir() {
                sources(&dir.join(sub), &mut files);
            }
        }
    }
    assert!(files.len() > 50, "too few sources found: {}", files.len());
    let this = file!();
    let mut wrong = Vec::new();
    for file in &files {
        let shown = file.display().to_string();
        if shown.ends_with(this.rsplit('/').next().unwrap_or(this)) {
            continue;
        }
        let text = std::fs::read_to_string(file).expect("source");
        let in_testing = shown.ends_with("atomic-durable/src/testing.rs");
        let in_package = shown.contains("/atomic-durable/");
        if !in_testing && text.contains("ATOMIC_TEST_DATABASE_URL") {
            wrong.push(format!(
                "{shown} reads ATOMIC_TEST_DATABASE_URL; use testing::schema"
            ));
        }
        if !in_package && (text.contains("testing::schema(") || text.contains("testing::database("))
        {
            wrong.push(format!(
                "{shown} uses the test database outside atomic-agentic-durable, where \
                 scripts/backends-check.sh does not look"
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
