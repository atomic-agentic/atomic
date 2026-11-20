// SPDX-License-Identifier: Apache-2.0

//! atomic's log lines go through redaction: no product code writes to
//! stderr except through `atomic_core::redact`.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// atomic's log lines go through `atomic_core::redact::log`, so none can
/// carry a secret: no product code writes to stderr itself.
#[test]
fn every_log_line_goes_through_redaction() {
    // The failpoints only print their own names, in crash tests; the log
    // function is the one place that writes.
    const ALLOWED: &[&str] = &[
        "atomic-core/src/redact.rs",
        "atomic-core/src/failpoint.rs",
        "atomic-durable/src/failpoints.rs",
    ];
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_owned();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&crates).unwrap() {
        let src = entry.unwrap().path().join("src");
        if src.is_dir() {
            sources(&src, &mut files);
        }
    }
    let mut writers = Vec::new();
    for file in &files {
        let relative = file
            .strip_prefix(&crates)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if ALLOWED.contains(&relative.as_str())
            || relative.ends_with("/tests.rs")
            || relative.contains("/tests/")
            || relative.ends_with("/testing.rs")
        {
            continue;
        }
        let text = std::fs::read_to_string(file).unwrap();
        // A file's own unit tests may print.
        let product = text
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap_or(&text);
        // Every way to stderr: the print macros, `dbg!`, and the handle,
        // whether written to with `writeln!` or `write_all`.
        for writer in ["eprintln!(", "eprint!(", "dbg!(", "io::stderr()", "Stderr"] {
            if product.contains(writer) {
                writers.push(format!("{relative}: {writer}"));
            }
        }
    }
    assert!(
        writers.is_empty(),
        "these write to stderr without redaction; use atomic_core::log_line! or \
         atomic_core::redact::stderr_lines: {writers:?}"
    );
}
