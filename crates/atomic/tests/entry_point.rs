// SPDX-License-Identifier: Apache-2.0

//! The `atomic` executable's entry point.
//!
//! On Linux the same executable re-runs itself as Codex's sandbox helper and as
//! `apply_patch`, chosen by the name it is invoked under. And whatever it runs,
//! it must never create files in the user's own Codex home.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ATOMIC: &str = env!("CARGO_BIN_EXE_atomic");

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "atomic-entry-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create a scratch directory");
    dir
}

/// Runs `exe` with a throwaway home, data directory and working directory.
fn run(exe: &Path, args: &[&str], home: &Path) -> Output {
    Command::new(exe)
        .args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("ATOMIC_HOME", home.join("atomic-home"))
        .env_remove("CODEX_HOME")
        .env_remove("XDG_DATA_HOME")
        .output()
        .expect("run the executable")
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn the_binary_starts_and_leaves_the_users_codex_home_alone() {
    let home = scratch("plain");
    let out = run(Path::new(ATOMIC), &[], &home);
    assert!(out.status.success(), "atomic failed: {}", text(&out));
    assert!(text(&out).contains("atomic "), "{}", text(&out));

    assert!(
        !home.join(".codex").exists(),
        "atomic created the user's Codex home"
    );
    // Codex's home is inside the project's state directory instead.
    let projects: Vec<_> = std::fs::read_dir(home.join("atomic-home/projects"))
        .expect("state directories")
        .collect();
    assert_eq!(projects.len(), 1);
    let state = projects[0].as_ref().expect("entry").path();
    assert!(state.join("codex").is_dir());
    // Nothing else appeared in the (project) working directory.
    let mut top: Vec<String> = std::fs::read_dir(&home)
        .expect("home")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    top.sort();
    assert_eq!(top, ["atomic-home"]);
    let _ = std::fs::remove_dir_all(&home);
}

#[cfg(unix)]
fn alias(name: &str, home: &Path) -> PathBuf {
    let link = home.join(name);
    std::os::unix::fs::symlink(ATOMIC, &link).expect("symlink the binary");
    link
}

#[cfg(unix)]
#[test]
fn under_apply_patch_it_takes_the_apply_patch_path() {
    let home = scratch("apply-patch");
    let out = run(&alias("apply_patch", &home), &[], &home);
    let t = text(&out);
    assert!(!out.status.success());
    assert!(
        !t.contains("atomic 0.0.0"),
        "ran atomic's own entry point: {t}"
    );
    assert!(
        t.to_lowercase().contains("patch"),
        "not the apply_patch helper: {t}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[cfg(target_os = "linux")]
#[test]
fn under_the_sandbox_helpers_name_it_takes_the_helper_path() {
    let home = scratch("sandbox");
    let out = run(&alias("codex-linux-sandbox", &home), &[], &home);
    let t = text(&out);
    assert!(!out.status.success());
    assert!(
        !t.contains("atomic 0.0.0"),
        "ran atomic's own entry point: {t}"
    );
    assert!(
        !t.contains("Small, typed, durable agents"),
        "ran atomic's CLI: {t}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_store_written_by_a_newer_atomic_stops_it_from_starting() {
    let home = scratch("newer-store");
    let first = run(Path::new(ATOMIC), &[], &home);
    assert!(first.status.success(), "{}", text(&first));
    let state = std::fs::read_dir(home.join("atomic-home/projects"))
        .expect("state directories")
        .next()
        .expect("one project")
        .expect("entry")
        .path();
    let db = state.join("atomic.db");
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            use sqlx::Connection;
            let mut c = sqlx::SqliteConnection::connect(&format!("sqlite://{}", db.display()))
                .await
                .expect("connect");
            sqlx::query("UPDATE atomic_store_version SET version = version + 1")
                .execute(&mut c)
                .await
                .expect("bump the version");
        });
    let out = run(Path::new(ATOMIC), &[], &home);
    let t = text(&out);
    assert!(
        !out.status.success(),
        "atomic started on a newer store: {t}"
    );
    assert!(t.contains("newer than this atomic understands"), "{t}");
    let _ = std::fs::remove_dir_all(&home);
}
