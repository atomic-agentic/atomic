// SPDX-License-Identifier: Apache-2.0

//! Every test reaches the PostgreSQL test database through
//! `atomic_durable::testing`, and only where scripts/backends-check.sh
//! looks.
//!
//! `testing::schema` gives each test a schema it drops afterwards, and fails
//! the test, naming what is missing, when there is no database. A test that
//! read `ATOMIC_TEST_DATABASE_URL` itself would do neither. The testing
//! module keeps the variable and its readers private, so the compiler
//! refuses most such tests; this test catches a test that reads the variable
//! from the environment itself, and one that uses the database outside the
//! places in [`USERS`], which backends-check.sh checks.

use std::path::{Path, PathBuf};

/// Where tests may use the test database, under crates/: a directory, with
/// its slash, or a file. scripts/backends-check.sh checks each of these.
const USERS: &[&str] = &["atomic-durable/", "atomic/tests/run_cli.rs"];

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
fn only_the_testing_module_reads_the_database_variable_and_only_listed_tests_use_it() {
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
    let mut wrong = Vec::new();
    for file in &files {
        // Matched on the path under crates/, not on part of it.
        let relative = file
            .strip_prefix(&crates)
            .expect("under crates/")
            .to_string_lossy()
            .replace('\\', "/");
        if relative == "atomic-durable/tests/test_database.rs" {
            continue;
        }
        let text = std::fs::read_to_string(file).expect("source");
        if relative != "atomic-durable/src/testing.rs"
            && (text.contains("ATOMIC_TEST_DATABASE_URL") || text.contains("DATABASE_VAR"))
        {
            wrong.push(format!(
                "{relative} reads the test database's variable; use testing::schema"
            ));
        }
        let allowed = USERS.iter().any(|u| match u.strip_suffix('/') {
            Some(dir) => relative.starts_with(&format!("{dir}/")),
            None => relative == *u,
        });
        if !allowed && text.contains("atomic_durable::testing") {
            wrong.push(format!(
                "{relative} uses the test database, and scripts/backends-check.sh does not \
                 look there: add it to both"
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
