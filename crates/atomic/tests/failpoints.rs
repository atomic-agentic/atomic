// SPDX-License-Identifier: Apache-2.0

//! Every failpoint can be named and triggered from a test.
//!
//! A small workflow of a step, a transaction and an atom (its turn and its
//! publication) runs on SQLite in a child process:
//!
//! - run once with `ATOMIC_FAILPOINT_LOG`, it reaches `checkpoint:before:<op>`
//!   and then `checkpoint:after:<op>` for each operation, in order, and every
//!   name it reaches belongs to a site in the inventory;
//! - a replay of the finished run reaches none of them, because no operation
//!   runs again, and neither does the replay of a killed run's committed
//!   operations;
//! - every failpoint named in atomic's source belongs to a site in the
//!   inventory, and every site is named in the source;
//! - each of those names, given in `ATOMIC_FAILPOINT`, kills a fresh run
//!   there, and only there; a new process then recovers the run to the same
//!   result, and no operation whose checkpoint committed before the kill runs
//!   again.
//!
//! The crash suite (`crash.rs`) does the same for real workflows against the
//! scripted fake model.

use atomic_durable::failpoints::site_of;
use atomic_durable::{DurableRuntime, Durare, DurareContext, Proposal, WorkflowContext, run_atom};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

const ENV_ROLE: &str = "ATOMIC_FAILPOINTS_ROLE";
const ENV_DIR: &str = "ATOMIC_FAILPOINTS_DIR";
const RUN: &str = "probe-1";
/// The workflow's operations, in the order it runs them.
const OPS: &[&str] = &["one", "two", "three.turn", "three.publish"];

fn scratch(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-failpoints-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// Note that `what` ran, in `dir/ran.log`.
fn note(dir: &Path, what: &str) {
    use std::io::Write;
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("ran.log"))
        .expect("ran.log");
    log.write_all(format!("{what}\n").as_bytes()).expect("note");
}

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

async fn engine(dir: &Path) -> Durare {
    let mut rt = Durare::sqlite(&dir.join("atomic.db"))
        .await
        .expect("engine");
    let at = dir.to_owned();
    rt.register("probe", move |ctx: DurareContext, (): ()| {
        let dir = at.clone();
        async move {
            let d = dir.clone();
            let one: i64 = ctx
                .step("one", move |_| async move {
                    note(&d, "one");
                    Ok(1)
                })
                .await?;
            let two: i64 = ctx
                .transaction("two", |mut tx| {
                    Box::pin(async move {
                        let row = tx.query_one("SELECT 2 AS two", &[]).await?;
                        row.int("two")
                    })
                })
                .await?;
            let d = dir.clone();
            let three = run_atom(
                &ctx,
                "three",
                move || async move {
                    note(&d, "three");
                    Ok(Proposal {
                        output: json!(3),
                        metadata: Value::Null,
                        writes: Vec::new(),
                    })
                },
                Vec::new(),
            )
            .await?;
            Ok(json!([one, two, three.output]))
        }
    });
    rt.launch().await.expect("launch");
    rt
}

/// Not a test on its own: the parent runs it in a child process, which
/// starts the run (role `start`) or recovers it (role `recover`) and writes
/// its result next to the store.
#[test]
#[ignore = "run by the failpoint tests in a child process"]
fn failpoints_child() {
    let (Ok(role), Ok(dir)) = (std::env::var(ENV_ROLE), std::env::var(ENV_DIR)) else {
        return;
    };
    let dir = PathBuf::from(dir);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            let rt = engine(&dir).await;
            if role == "start" {
                rt.start("probe", RUN, ()).await.expect("start");
            } else {
                rt.recover().await.expect("recover");
            }
            let out: Value = rt.result(RUN).await.expect("finishes");
            std::fs::write(dir.join("result.json"), out.to_string()).expect("result");
            rt.shutdown(std::time::Duration::from_secs(5))
                .await
                .expect("shutdown");
        });
}

/// Run one child process in `dir`, killed at `kill` if given, logging the
/// failpoints it reaches to `dir/<log>`. Returns how it exited and what it
/// wrote to stderr.
fn child(dir: &Path, role: &str, kill: Option<&str>, log: &str) -> (ExitStatus, String) {
    let stderr = dir.join(format!("{log}.stderr"));
    let mut cmd = Command::new(std::env::current_exe().expect("this test binary"));
    cmd.args(["--exact", "failpoints_child", "--ignored", "--nocapture"])
        .env(ENV_ROLE, role)
        .env(ENV_DIR, dir)
        .env("ATOMIC_FAILPOINT_LOG", dir.join(log))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&stderr).expect("stderr"));
    match kill {
        Some(name) => cmd.env("ATOMIC_FAILPOINT", name),
        None => cmd.env_remove("ATOMIC_FAILPOINT"),
    };
    let status = cmd.status().expect("run the child");
    (status, std::fs::read_to_string(&stderr).unwrap_or_default())
}

fn result(dir: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(dir.join("result.json")).expect("result"))
        .expect("decode")
}

#[test]
fn every_checkpoint_has_a_failpoint_before_and_after_it() {
    let dir = scratch("record");
    let (status, stderr) = child(&dir, "start", None, "first.log");
    assert!(status.success(), "the run finished: {status}\n{stderr}");
    assert_eq!(result(&dir), json!([1, 2, 3]));
    let expected: Vec<String> = OPS
        .iter()
        .flat_map(|op| {
            [
                format!("checkpoint:before:{op}"),
                format!("checkpoint:after:{op}"),
            ]
        })
        .collect();
    let reached = lines(&dir.join("first.log"));
    assert_eq!(reached, expected);
    for name in &reached {
        assert!(site_of(name).is_some(), "{name} is in the inventory");
    }

    // A new process recovering the finished run runs nothing again, so it
    // reaches no checkpoint failpoint.
    let (status, stderr) = child(&dir, "recover", None, "second.log");
    assert!(status.success(), "{status}\n{stderr}");
    assert_eq!(lines(&dir.join("second.log")), Vec::<String>::new());
    assert_eq!(lines(&dir.join("ran.log")), ["one", "three"]);
}

#[test]
fn each_failpoint_kills_the_process_there_and_the_run_recovers() {
    let names: Vec<String> = OPS
        .iter()
        .flat_map(|op| {
            [
                format!("checkpoint:before:{op}"),
                format!("checkpoint:after:{op}"),
            ]
        })
        .collect();
    for (i, name) in names.iter().enumerate() {
        let dir = scratch(&format!("kill-{i}"));
        let (status, stderr) = child(&dir, "start", Some(name), "first.log");
        assert!(!status.success(), "{name}: the first process was killed");
        assert!(
            stderr.contains(&format!("failpoint {name}: killing the process")),
            "{name}: {stderr}"
        );
        let first = lines(&dir.join("first.log"));
        assert_eq!(first.last(), Some(name), "{name}: it stopped there");
        assert!(!dir.join("result.json").exists(), "{name}");

        let (status, stderr) = child(&dir, "recover", None, "second.log");
        assert!(status.success(), "{name}: recovered: {status}\n{stderr}");
        assert_eq!(result(&dir), json!([1, 2, 3]), "{name}");

        // No operation whose checkpoint committed before the kill ran again,
        // and its replay reached neither of its failpoints.
        let second = lines(&dir.join("second.log"));
        for op in OPS {
            if first.contains(&format!("checkpoint:after:{op}")) {
                for when in ["before", "after"] {
                    assert!(
                        !second.contains(&format!("checkpoint:{when}:{op}")),
                        "{name}: replaying {op} reached checkpoint:{when}: {second:?}"
                    );
                }
            }
        }
        // The same, counted by the bodies themselves.
        let ran = lines(&dir.join("ran.log"));
        for (op, body) in [("one", "one"), ("three.turn", "three")] {
            let times = ran.iter().filter(|r| *r == body).count();
            let committed = first.contains(&format!("checkpoint:after:{op}"));
            let reached = first.contains(&format!("checkpoint:before:{op}"));
            let expected = if committed || !reached { 1 } else { 2 };
            assert_eq!(times, expected, "{name}: {body} ran {times} times: {ran:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Every failpoint name or name template in a source file: the string
/// literal passed to `failpoint(` directly or through `format!(`, with each
/// `{...}` replaced by `x`.
fn named_in(source: &str) -> Vec<String> {
    let mut names = Vec::new();
    for (at, _) in source.match_indices("failpoint(") {
        let before = &source[..at];
        if before.ends_with("fn ") || before.ends_with("_") {
            continue;
        }
        let rest = source[at + "failpoint(".len()..].trim_start();
        let rest = rest.strip_prefix("&format!(").unwrap_or(rest).trim_start();
        let Some(rest) = rest.strip_prefix('"') else {
            continue;
        };
        let Some(end) = rest.find('"') else {
            continue;
        };
        let mut name = String::new();
        let mut inside = false;
        for c in rest[..end].chars() {
            match c {
                '{' => {
                    inside = true;
                    name.push('x');
                }
                '}' => inside = false,
                c if !inside => name.push(c),
                _ => {}
            }
        }
        names.push(name);
    }
    names
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
fn every_failpoint_in_the_source_is_in_the_inventory_and_back() {
    use atomic_durable::failpoints::SITES;
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&crates).expect("crates") {
        let src = entry.expect("entry").path().join("src");
        if src.is_dir() {
            sources(&src, &mut files);
        }
    }
    let (mut found, mut all) = (Vec::new(), String::new());
    for file in &files {
        let text = std::fs::read_to_string(file).expect("source");
        for name in named_in(&text) {
            found.push((file.display().to_string(), name));
        }
        all.push_str(&text);
    }
    assert!(found.len() >= 10, "too few failpoints found: {found:?}");
    for (file, name) in &found {
        assert!(site_of(name).is_some(), "{file}: {name} is not in SITES");
    }
    // A name may be built before it is passed, so the other way round looks
    // for the site's literal anywhere in the source.
    for site in SITES {
        let literal = format!("\"{}", site.name.split('{').next().unwrap_or(site.name));
        assert!(
            all.contains(&literal),
            "{} is in SITES and named nowhere in the source",
            site.name
        );
    }
}
