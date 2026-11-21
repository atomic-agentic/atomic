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
const USERS: &[&str] = &[
    "atomic-durable/",
    "atomic/tests/run_cli.rs",
    "atomic/tests/failpoints.rs",
];

/// Why `text`, a source outside [`USERS`], reaches the test database's
/// helpers, if it does: by naming `atomic_durable::testing`, in a path or
/// inside a `use atomic_durable::{...}` group, through a renamed
/// `atomic_durable`, or by naming what the module gives (`testing::schema`,
/// `TestSchema`).
fn reaches_testing(text: &str) -> Option<String> {
    for token in ["testing::schema", "TestSchema"] {
        if text.contains(token) {
            return Some(format!("names `{token}`"));
        }
    }
    // Whitespace runs as one space, and none around `::`, braces and `*`:
    // words stay apart, so `testing as t` is three words, not one.
    let spaced = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut tight = String::with_capacity(spaced.len());
    let chars: Vec<char> = spaced.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        let sticky = |c: Option<&char>| matches!(c, Some(':' | '{' | '}' | '*'));
        if *c == ' ' && (sticky(chars.get(i + 1)) || i > 0 && sticky(chars.get(i - 1))) {
            continue;
        }
        tight.push(*c);
    }
    if tight.contains("atomic_durable::testing") {
        return Some("names `atomic_durable::testing`".into());
    }
    // A glob brings `testing` in under its own name, unseen.
    if tight.contains("atomic_durable::*") {
        return Some("imports everything from atomic_durable; name what it uses".into());
    }
    // `use atomic_durable::{Durare, testing as t};`, nested groups included.
    let mut rest = tight.as_str();
    while let Some(at) = rest.find("atomic_durable::{") {
        let group = &rest[at + "atomic_durable::".len()..];
        let mut depth = 0;
        let mut end = group.len();
        for (i, c) in group.char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let names = &group[..end];
        if names
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .any(|n| n == "testing")
        {
            return Some("imports `testing` from atomic_durable".into());
        }
        rest = &group[end..];
    }
    // A renamed crate hides its paths from the checks above.
    if spaced.contains("atomic_durable as ") {
        return Some("renames atomic_durable; name it as it is".into());
    }
    None
}

#[test]
fn the_scan_sees_every_way_to_reach_the_testing_module() {
    for text in [
        "use atomic_durable::testing;",
        "let s = atomic_durable :: testing :: schema(\"x\");",
        "use atomic_durable::{Durare, testing};",
        "use atomic_durable::{Durare, testing as t};",
        "use atomic_durable :: { Durare,\n    testing as t };",
        "use atomic_durable::*;",
        "use atomic_durable :: * ;",
        "use atomic_durable::{gate::{self, Gate}, testing::TestSchema as S};",
        "use atomic_durable as ad;",
        "let schema = testing::schema(\"t\").await;",
        "fn f(_: TestSchema) {}",
    ] {
        assert!(reaches_testing(text).is_some(), "missed: {text}");
    }
    for text in [
        "use atomic_durable::{Durare, gate};",
        "use atomic_core::testing::{FakeModel, Reply};",
        "use atomic_durable::Durare;",
    ] {
        assert_eq!(reaches_testing(text), None, "{text}");
    }
}

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
        if !allowed && let Some(how) = reaches_testing(&text) {
            wrong.push(format!(
                "{relative} uses the test database ({how}), and scripts/backends-check.sh does \
                 not look there: add it to both"
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
