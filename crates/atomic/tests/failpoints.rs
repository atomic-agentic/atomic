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
use atomic_durable::gate;
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
                move || {
                    let d = d.clone();
                    async move {
                        note(&d, "three");
                        Ok(Proposal {
                            output: json!(3),
                            metadata: Value::Null,
                            writes: Vec::new(),
                        })
                    }
                },
                Vec::new(),
            )
            .await?;
            Ok(json!([one, two, three.output]))
        }
    });
    // A transaction another connection writes beside while its body runs:
    // the body reads, then the other connection writes. The step holds the
    // store's writer lock while its body runs, so the other write waits for
    // the step to commit, and the body runs once.
    let at = dir.to_owned();
    rt.register("retried", move |ctx: DurareContext, (): ()| {
        let dir = at.clone();
        async move {
            let n: i64 = ctx
                .transaction("race", move |mut tx| {
                    let dir = dir.clone();
                    Box::pin(async move {
                        let row = tx
                            .query_one("SELECT COUNT(*) AS n FROM atomic_parks", &[])
                            .await?;
                        note(&dir, "race");
                        let first = !dir.join("raced").exists();
                        if first {
                            std::fs::write(dir.join("raced"), "").expect("marker");
                            // Not awaited: it waits on this step's lock.
                            tokio::spawn(write_beside(dir.join("atomic.db")));
                            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        }
                        row.int("n")
                    })
                })
                .await?;
            Ok(json!(n))
        }
    });
    // Two operations of one name.
    rt.register("twice", |ctx: DurareContext, (): ()| async move {
        for _ in 0..2 {
            let _: i64 = ctx
                .transaction("x", |mut tx| {
                    Box::pin(async move { tx.query_one("SELECT 1 AS one", &[]).await?.int("one") })
                })
                .await?;
        }
        Ok(json!(2))
    });
    // A run that waits at a gate the local operator decides.
    rt.register("gated", |ctx: DurareContext, (): ()| async move {
        let approvers = gate::resolve(
            "check",
            None,
            &gate::Deployment::Local {
                operator: gate::Principal::local("dev"),
            },
        )
        .map_err(|e| atomic_durable::DurableError::Failed(e.to_string()))?;
        let spec = gate::GateSpec {
            workflow: "gated".into(),
            title: "Ship it".into(),
            show: Value::Null,
            evidence: Vec::new(),
            approvers,
            tier: gate::Tier::High,
            timeout: None,
            on_timeout: Default::default(),
            question: None,
        };
        let resolution = gate::gate(&ctx, "check", spec).await?;
        Ok(json!(resolution.approved()))
    });
    rt.launch().await.expect("launch");
    rt
}

/// Write to the store from a connection of its own.
async fn write_beside(db: PathBuf) {
    let db = db.as_path();
    use sqlx::Connection;
    let mut other = sqlx::SqliteConnection::connect(&format!("sqlite://{}", db.display()))
        .await
        .expect("a second connection");
    sqlx::query(
        "INSERT INTO atomic_parks (park_id, run, step, reason, parked_at) \
         VALUES ('beside', 'beside', 'beside', 'beside', 0)",
    )
    .execute(&mut other)
    .await
    .expect("the write beside");
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
            } else if role == "retried" {
                rt.start("retried", RUN, ()).await.expect("start");
            } else if role == "gated" {
                rt.start("gated", RUN, ()).await.expect("start");
            } else if role == "twice" {
                rt.start("twice", RUN, ()).await.expect("start");
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

/// What a `failpoint(` call in the source passes.
enum Named {
    /// A name or template, with each `{...}` replaced by `x`.
    Literal(String),
    /// Something the scan cannot read, such as a variable, as written.
    Unreadable(String),
}

/// Every `failpoint(` call in a source file: the string literal it passes,
/// directly or through `format!(`, or what it passes instead.
fn named_in(source: &str) -> Vec<Named> {
    let mut names = Vec::new();
    for (at, _) in source.match_indices("failpoint(") {
        let before = &source[..at];
        if before.ends_with("fn ") || before.ends_with("_") {
            continue;
        }
        let rest = source[at + "failpoint(".len()..].trim_start();
        let rest = rest.strip_prefix("&format!(").unwrap_or(rest).trim_start();
        let Some(rest) = rest.strip_prefix('"') else {
            let arg: String = rest.chars().take_while(|c| *c != ')').collect();
            names.push(Named::Unreadable(arg.trim().to_owned()));
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
        names.push(Named::Literal(name));
    }
    names
}

/// Calls whose name the scan cannot read, by file and argument, each with
/// the literal its name is built from, which the scan checks instead.
const BUILT_ELSEWHERE: &[(&str, &str, &str)] =
    &[("atomic/src/pipeline.rs", "&ran", "pipeline:ran:x")];

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
        let shown = file.display().to_string();
        for named in named_in(&text) {
            match named {
                Named::Literal(name) => found.push((shown.clone(), name)),
                Named::Unreadable(arg) => {
                    let Some((_, _, name)) = BUILT_ELSEWHERE
                        .iter()
                        .find(|(f, a, _)| shown.ends_with(f) && *a == arg)
                    else {
                        panic!(
                            "{shown}: failpoint({arg}) passes a name the scan cannot read; pass \
                             a literal or add it to BUILT_ELSEWHERE"
                        );
                    };
                    found.push((shown.clone(), (*name).to_owned()));
                }
            }
        }
        // The inventory itself names every site; it does not count.
        if !shown.ends_with("atomic-durable/src/failpoints.rs") {
            all.push_str(&text);
        }
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

#[test]
fn a_transaction_another_connection_writes_beside_runs_once_and_reaches_its_failpoints_once() {
    let dir = scratch("retried");
    let (status, stderr) = child(&dir, "retried", None, "first.log");
    assert!(status.success(), "the run finished: {status}\n{stderr}");
    // The other connection wrote while the body ran, and the body still ran
    // once: the step held the writer lock, so the write waited for it.
    assert_eq!(lines(&dir.join("ran.log")), ["race"], "the body ran again");
    // One durable operation, whose writes and checkpoint committed once: its
    // failpoints are reached once each.
    assert_eq!(
        lines(&dir.join("first.log")),
        ["checkpoint:before:race", "checkpoint:after:race"]
    );
}

/// The wake-ups queued for `run`, read or not: the engine keeps a message
/// it delivered.
fn notifications(db: &Path, run: &str) -> i64 {
    use sqlx::Connection;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            let mut c = sqlx::SqliteConnection::connect(&format!("sqlite://{}", db.display()))
                .await
                .expect("connect");
            let (n,): (i64,) =
                sqlx::query_as("SELECT COUNT(*) FROM notifications WHERE destination_uuid = ?")
                    .bind(run)
                    .fetch_one(&mut c)
                    .await
                    .expect("count");
            n
        })
}

#[test]
fn a_redelivered_wake_up_is_the_same_wake_up() {
    let dir = scratch("redelivered");
    // The run opens its gate and dies before it waits.
    let (status, _) = child(&dir, "gated", Some("gate:after-open"), "first.log");
    assert!(!status.success(), "killed after the gate opened");

    // While no process runs it, the operator decides, which wakes it, and a
    // resumer redelivers the wake-up, as one does on start.
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            let terminal = Durare::sqlite(&dir.join("atomic.db")).await.expect("store");
            let open = gate::gates(
                &terminal,
                gate::GateFilter {
                    open_only: true,
                    run: Some(RUN.to_owned()),
                },
            )
            .await
            .expect("gates");
            let g = open.first().expect("the open gate").clone();
            let credential =
                gate::HumanCredential::new(gate::Principal::local("dev"), "the operator's secret");
            gate::enroll(&terminal, &credential).await.expect("enroll");
            gate::decide(
                &terminal,
                &credential,
                gate::DecisionCommand {
                    command_id: "approve-1".into(),
                    gate: g.id.clone(),
                    verdict: gate::Verdict::Approve,
                    comment: None,
                    proposal: g.proposal.clone(),
                    answer: None,
                },
            )
            .await
            .expect("decided");
            assert_eq!(gate::redeliver(&terminal).await.expect("redeliver"), 1);
        });
    // A wake-up is keyed by its gate, so the redelivery was the same one: a
    // gate wakes its run at most once.
    assert_eq!(notifications(&dir.join("atomic.db"), RUN), 1);

    // The recovered run takes that wake-up, reads the decision once, and
    // finishes.
    let (status, stderr) = child(&dir, "recover", None, "second.log");
    assert!(status.success(), "recovered: {status}\n{stderr}");
    assert_eq!(result(&dir), json!(true));
    let second = lines(&dir.join("second.log"));
    let count = |name: &str| second.iter().filter(|l| *l == name).count();
    assert_eq!(count("checkpoint:before:check.decision"), 1, "{second:?}");
    assert_eq!(count("checkpoint:after:check.decision"), 1, "{second:?}");
    assert_eq!(
        second
            .iter()
            .filter(|l| l.starts_with("recv:after:"))
            .count(),
        1,
        "{second:?}"
    );
    assert_eq!(notifications(&dir.join("atomic.db"), RUN), 1);
}

#[test]
fn two_operations_of_one_name_each_reach_their_failpoints() {
    let dir = scratch("twice");
    let (status, stderr) = child(&dir, "twice", None, "first.log");
    assert!(status.success(), "the run finished: {status}\n{stderr}");
    // Two calls, two durable operations: a real repeat stays visible.
    assert_eq!(
        lines(&dir.join("first.log")),
        [
            "checkpoint:before:x",
            "checkpoint:after:x",
            "checkpoint:before:x",
            "checkpoint:after:x"
        ]
    );
}
