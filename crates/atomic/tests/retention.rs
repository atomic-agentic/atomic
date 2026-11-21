// SPDX-License-Identifier: Apache-2.0

//! Retention keeps what runs and gates may still read.
//!
//! - A sweep removes content nothing keeps, with its manifests, and keeps
//!   every digest a gate in the store pins as evidence, open or resolved, as
//!   well as what the caller keeps; content pinned and kept twice over
//!   survives once. The gate store's pins keep content by themselves.
//! - A run waiting at a gate keeps its pinned definitions and the artifact
//!   its step recorded, with nothing passed in to keep.
//! - A run keeps what its input, its steps' outputs and errors and its own
//!   result name, whole or inside longer text; and every run is read, however
//!   many there are.
//! - Content written again is recent again, and survives a sweep's grace.
//! - Two sweeps at once both succeed and remove each unkept blob once.
//! - A write fails, within its deadline and saying why, while something
//!   else holds the store's lock.

#![allow(clippy::unwrap_used)]

use atomic_app::retention;
use atomic_core::artifact::{ArtifactRef, ArtifactStore, Grace, LocalStore, Provenance};
use atomic_core::testing::on_codex_runtime;
use atomic_durable::gate::{
    self, DecisionCommand, Deployment, GateSpec, HumanCredential, OnTimeout, Principal, Tier,
    Verdict,
};
use atomic_durable::{DurableError, DurableRuntime, Durare, DurareContext, WorkflowContext};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

fn scratch(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-retention-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn put(store: &LocalStore, bytes: &[u8]) -> ArtifactRef {
    store
        .put(
            &mut &bytes[..],
            Provenance {
                media_type: "text/plain".into(),
                ..Provenance::default()
            },
        )
        .unwrap()
        .artifact
}

/// Whether the content and its manifests are both there; a half that is
/// there without the other fails the test.
fn held(store: &LocalStore, a: &ArtifactRef) -> bool {
    let blob = store.contains(a).unwrap();
    let manifests = !store.manifests(a).unwrap().is_empty();
    assert_eq!(blob, manifests, "{a}: blob {blob}, manifests {manifests}");
    blob
}

/// Date every blob in the store an hour back.
fn backdate(dir: &Path) {
    let hour_ago = SystemTime::now() - Duration::from_secs(3600);
    for file in walk(&dir.join("blobs")) {
        std::fs::File::open(&file)
            .unwrap()
            .set_modified(hour_ago)
            .unwrap();
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files
}

fn spec(evidence: Vec<String>) -> Result<GateSpec, atomic_durable::DurableError> {
    Ok(GateSpec {
        workflow: "retained".into(),
        title: "Accept the patch".into(),
        show: json!({}),
        evidence,
        approvers: gate::resolve(
            "accept",
            None,
            &Deployment::Local {
                operator: Principal::local("dev"),
            },
        )
        .map_err(|e| atomic_durable::DurableError::Failed(e.to_string()))?,
        tier: Tier::High,
        timeout: None,
        on_timeout: OnTimeout::Reject,
        question: None,
    })
}

/// A run that opens a gate pinning `evidence` and waits for it; and one as
/// run_pipeline records a run: the definitions pinned by a step, a step whose
/// output refers to an artifact, then a gate with no evidence.
fn register(rt: &mut Durare) {
    rt.register(
        "pinning",
        |ctx: DurareContext, evidence: Vec<String>| async move {
            gate::gate(&ctx, "accept", spec(evidence)?).await
        },
    );
    rt.register(
        "pipeline-like",
        |ctx: DurareContext, (snapshot, patch): (String, String)| async move {
            let _: serde_json::Value = ctx
                .step(
                    "pin",
                    move |_| async move { Ok(json!({"snapshot": snapshot})) },
                )
                .await?;
            let _: serde_json::Value = ctx
                .step("draft", move |_| async move {
                    Ok(json!({"output": {"patch": patch, "notes": ["one"]}}))
                })
                .await?;
            gate::gate(&ctx, "accept", spec(Vec::new())?).await
        },
    );
    rt.register("names", |_ctx: DurareContext, named: String| async move {
        Ok::<_, DurableError>(named)
    });
}

async fn opened(rt: &Durare, run: &str) -> gate::Gate {
    let id = gate::gate_id(run, "accept");
    loop {
        if let Some(g) = gate::find(rt, &id).await.unwrap() {
            return g;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[test]
fn a_sweep_removes_what_nothing_keeps_and_keeps_what_gates_pin() {
    on_codex_runtime(async {
        let dir = scratch("pins");
        let store = LocalStore::open(dir.join("artifacts")).unwrap();
        let pinned = put(&store, b"the patch a gate shows");
        let both = put(&store, b"pinned, and kept by the caller too");
        let kept = put(&store, b"what the caller keeps");
        let loose = put(&store, b"nothing refers to this");
        let mut rt = Durare::sqlite(&dir.join("atomic.db")).await.unwrap();
        register(&mut rt);
        rt.launch().await.unwrap();
        rt.start(
            "pinning",
            "pinning-run",
            vec![
                format!("sha256:{}", pinned.digest()),
                format!("sha256:{}", both.digest()),
            ],
        )
        .await
        .unwrap();
        let g = opened(&rt, "pinning-run").await;
        // The gate store pins them itself, apart from the run's records.
        assert_eq!(
            retention::pinned(&rt).await.unwrap(),
            [pinned.clone(), both.clone()].into()
        );

        let keep: BTreeSet<ArtifactRef> = [kept.clone(), both.clone()].into();
        let swept = retention::sweep(&store, &rt, &keep, Grace::none_for_tests())
            .await
            .unwrap();
        assert_eq!(swept.removed, std::slice::from_ref(&loose));
        assert_eq!(swept.kept, 3);
        assert!(!held(&store, &loose));
        for a in [&pinned, &both, &kept] {
            assert!(held(&store, a), "{a} was swept");
        }

        // Resolved, the gate still pins its evidence.
        let me = HumanCredential::new(Principal::local("dev"), "the operator's secret");
        gate::enroll(&rt, &me).await.unwrap();
        gate::decide(
            &rt,
            &me,
            DecisionCommand {
                command_id: "approve".into(),
                gate: g.id.clone(),
                verdict: Verdict::Approve,
                comment: None,
                proposal: g.proposal.clone(),
                answer: None,
            },
        )
        .await
        .unwrap();
        let swept = retention::sweep(&store, &rt, &BTreeSet::new(), Grace::none_for_tests())
            .await
            .unwrap();
        assert_eq!(
            swept.removed,
            [kept],
            "only what the caller no longer keeps"
        );
        assert!(held(&store, &pinned) && held(&store, &both));
        rt.shutdown(Duration::from_secs(5)).await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn a_run_at_a_gate_keeps_its_snapshot_and_its_steps_artifacts() {
    on_codex_runtime(async {
        let dir = scratch("run");
        let store = LocalStore::open(dir.join("artifacts")).unwrap();
        let snapshot = put(&store, b"the run's pinned definitions");
        let patch = put(&store, b"the step's patch");
        let loose = put(&store, b"nothing refers to this");
        let mut rt = Durare::sqlite(&dir.join("atomic.db")).await.unwrap();
        register(&mut rt);
        rt.launch().await.unwrap();
        rt.start(
            "pipeline-like",
            "waiting-run",
            (snapshot.to_string(), patch.to_string()),
        )
        .await
        .unwrap();
        opened(&rt, "waiting-run").await;
        // Nothing passed in to keep: the run's own records keep its content.
        let swept = retention::sweep(&store, &rt, &BTreeSet::new(), Grace::none_for_tests())
            .await
            .unwrap();
        assert_eq!(swept.removed, [loose]);
        assert!(held(&store, &snapshot), "the run's snapshot was swept");
        assert!(held(&store, &patch), "the step's artifact was swept");
        rt.shutdown(Duration::from_secs(5)).await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn content_written_again_is_recent_again() {
    on_codex_runtime(async {
        let dir = scratch("rewrite");
        let store = LocalStore::open(dir.join("artifacts")).unwrap();
        let rt = Durare::sqlite(&dir.join("atomic.db")).await.unwrap();
        let rewritten = put(&store, b"written twice");
        let once = put(&store, b"written once");
        backdate(&dir.join("artifacts"));
        // Written again after it was dated: within the grace period again.
        assert_eq!(put(&store, b"written twice"), rewritten);
        let swept = retention::sweep(
            &store,
            &rt,
            &BTreeSet::new(),
            Grace::at_least(Duration::from_secs(600)),
        )
        .await
        .unwrap();
        assert_eq!(swept.removed, [once]);
        assert!(held(&store, &rewritten));
        rt.shutdown(Duration::from_secs(5)).await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn sweeps_at_once_remove_each_unkept_blob_once() {
    on_codex_runtime(async {
        let dir = scratch("concurrent");
        let store = LocalStore::open(dir.join("artifacts")).unwrap();
        let pinned = put(&store, b"evidence");
        let loose: BTreeSet<ArtifactRef> = (0..40)
            .map(|i| put(&store, format!("loose {i}").as_bytes()))
            .collect();
        let mut rt = Durare::sqlite(&dir.join("atomic.db")).await.unwrap();
        register(&mut rt);
        rt.launch().await.unwrap();
        rt.start(
            "pinning",
            "concurrent-run",
            vec![format!("sha256:{}", pinned.digest())],
        )
        .await
        .unwrap();
        opened(&rt, "concurrent-run").await;
        let keep = BTreeSet::new();
        let (a, b) = tokio::join!(
            retention::sweep(&store, &rt, &keep, Grace::none_for_tests()),
            retention::sweep(&store, &rt, &keep, Grace::none_for_tests()),
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        let mut removed: Vec<ArtifactRef> = a.removed.into_iter().chain(b.removed).collect();
        removed.sort();
        assert_eq!(
            removed,
            loose.into_iter().collect::<Vec<_>>(),
            "each loose blob once"
        );
        assert!(held(&store, &pinned));
        rt.shutdown(Duration::from_secs(5)).await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn a_write_fails_in_time_while_something_holds_the_lock() {
    let dir = scratch("held");
    let store = LocalStore::open(dir.join("artifacts"))
        .unwrap()
        .waiting_at_most(Duration::from_millis(300));
    // Whoever holds it, for as long as they like.
    let holder = std::fs::File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("artifacts/lock"))
        .unwrap();
    holder.lock().unwrap();
    let started = std::time::Instant::now();
    let refused = store
        .put(&mut &b"blocked"[..], Provenance::default())
        .expect_err("refused");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "it waited too long"
    );
    assert!(
        matches!(refused, atomic_core::artifact::ArtifactError::Locked { .. }),
        "{refused}"
    );
    assert!(
        refused.to_string().contains("was held elsewhere"),
        "{refused}"
    );
    drop(holder);
    put(&store, b"once it is free");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_run_keeps_what_its_input_steps_and_result_name_however_written() {
    on_codex_runtime(async {
        let dir = scratch("named");
        let store = LocalStore::open(dir.join("artifacts")).unwrap();
        let [note, report, fails, result, gave_up, loose] = [
            "named in the run's input",
            "named in a step's markdown",
            "named in a step's error",
            "named in the run's result",
            "named in another run's error",
            "named nowhere",
        ]
        .map(|text| put(&store, text.as_bytes()));
        let mut rt = Durare::sqlite(&dir.join("atomic.db")).await.unwrap();
        // Only the note comes in with the input; the rest the run's steps and
        // its own code name.
        let names = (
            report.to_string(),
            format!("sha256:{}", fails.digest()),
            result.to_string(),
        );
        rt.register("naming", move |ctx: DurareContext, _note: String| {
            let (report, fails, result) = names.clone();
            async move {
                let _: Value = ctx
                    .step("report", move |_| async move {
                        Ok(json!({"body": format!("# Report\n\nThe diff is {report}.")}))
                    })
                    .await?;
                // A step that fails, naming what it could not read; the run
                // goes on.
                let flaky: Result<Value, DurableError> = ctx
                    .step("flaky", move |_| async move {
                        Err(DurableError::Failed(format!("could not read {fails}")))
                    })
                    .await;
                assert!(flaky.is_err());
                // The run's own result, which no step returned.
                Ok(json!({"summary": format!("done: {result}, in the notes")}))
            }
        });
        // A run that fails, naming an artifact in its own error.
        let why = format!("gave up on {gave_up}");
        rt.register("giving-up", move |_ctx: DurareContext, (): ()| {
            let why = why.clone();
            async move { Err::<(), _>(DurableError::Failed(why)) }
        });
        rt.launch().await.unwrap();
        rt.start("giving-up", "giving-up-run", ()).await.unwrap();
        assert!(rt.result::<()>("giving-up-run").await.is_err());
        rt.start(
            "naming",
            "naming-run",
            format!("Notes: see sha256:{} and the rest.", note.digest()),
        )
        .await
        .unwrap();
        let _: Value = rt.result("naming-run").await.unwrap();
        let swept = retention::sweep(&store, &rt, &BTreeSet::new(), Grace::none_for_tests())
            .await
            .unwrap();
        assert_eq!(swept.removed, [loose]);
        for (what, a) in [
            ("input", &note),
            ("step output", &report),
            ("step error", &fails),
            ("result", &result),
            ("other run's error", &gave_up),
        ] {
            assert!(held(&store, a), "what the run's {what} names was swept");
        }
        rt.shutdown(Duration::from_secs(5)).await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn every_run_is_read_however_many_there_are() {
    on_codex_runtime(async {
        let dir = scratch("many");
        let store = LocalStore::open(dir.join("artifacts")).unwrap();
        let mut rt = Durare::sqlite(&dir.join("atomic.db")).await.unwrap();
        register(&mut rt);
        rt.launch().await.unwrap();
        // More runs than a scan reads at once.
        let mut named = Vec::new();
        for i in 0..230 {
            let a = put(&store, format!("result {i}").as_bytes());
            let run = format!("names-{i:03}");
            rt.start("names", &run, a.to_string()).await.unwrap();
            named.push((run, a));
        }
        for (run, _) in &named {
            let _: String = rt.result(run).await.unwrap();
        }

        let swept = retention::sweep(&store, &rt, &BTreeSet::new(), Grace::none_for_tests())
            .await
            .unwrap();
        assert!(swept.removed.is_empty(), "{:?}", swept.removed);
        assert_eq!(swept.kept, named.len());
        rt.shutdown(Duration::from_secs(5)).await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn the_keep_set_is_dated_from_before_the_runs_are_read() {
    on_codex_runtime(async {
        let dir = scratch("dated");
        let mut rt = Durare::sqlite(&dir.join("atomic.db")).await.unwrap();
        register(&mut rt);
        rt.launch().await.unwrap();
        for i in 0..300 {
            let run = format!("dated-{i:03}");
            rt.start("names", &run, format!("run {i}")).await.unwrap();
            let _: String = rt.result(&run).await.unwrap();
        }
        let before = SystemTime::now();
        let store = LocalStore::open(dir.join("artifacts")).unwrap();
        let kept = retention::read_kept(&store, &rt, &BTreeSet::new())
            .await
            .unwrap();
        let reading = before.elapsed().unwrap();
        // Dated when the reading began, not when it ended: content written
        // while the runs were read is within the grace.
        let late = kept.read_at.duration_since(before).unwrap_or_default();
        assert!(
            late < reading / 2,
            "dated {late:?} into a reading of {reading:?}"
        );
        rt.shutdown(Duration::from_secs(5)).await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    });
}

/// A hold let go between a sweep's reading of the runs and its lock still
/// keeps its content: the step that held it recorded the reference after
/// the sweep read past it, then let go, and the sweep finds no hold under
/// its lock. The sweep keeps what was held when it began reading.
#[test]
fn a_hold_let_go_while_a_sweep_reads_the_runs_still_keeps_its_content() {
    on_codex_runtime(async {
        let dir = scratch("let-go");
        let mut rt = Durare::sqlite(&dir.join("atomic.db")).await.unwrap();
        register(&mut rt);
        rt.launch().await.unwrap();
        let store = LocalStore::open(dir.join("artifacts")).unwrap();
        let patch = put(&store, b"handed over long ago");
        backdate(&dir.join("artifacts"));
        let hold = store.hold(&patch).unwrap();
        // The sweep reads what to keep: no run names the patch yet...
        let kept = retention::read_kept(&store, &rt, &BTreeSet::new())
            .await
            .unwrap();
        // ...then the step records it and lets go, before the sweep takes
        // the store's lock.
        drop(hold);
        let swept = store
            .sweep(&kept.artifacts, Grace::none_for_tests(), kept.read_at)
            .unwrap();
        assert!(swept.removed.is_empty(), "{swept:?}");
        assert!(held(&store, &patch), "removed after its hold was let go");
        rt.shutdown(Duration::from_secs(5)).await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    });
}

/// The holds are read before the runs: a step that records its reference
/// and lets its hold go while the sweep reads the runs, after the sweep has
/// read past its run, still keeps its content.
#[test]
fn the_holds_are_read_before_the_runs() {
    on_codex_runtime(async {
        let dir = scratch("holds-first");
        let mut rt = Durare::sqlite(&dir.join("atomic.db")).await.unwrap();
        register(&mut rt);
        rt.launch().await.unwrap();
        let store = LocalStore::open(dir.join("artifacts")).unwrap();
        let patch = put(&store, b"let go as the runs are read");
        backdate(&dir.join("artifacts"));
        let hold = store.hold(&patch).unwrap();
        let kept = retention::read_kept_then(&store, &rt, &BTreeSet::new(), || drop(hold))
            .await
            .unwrap();
        let swept = store
            .sweep(&kept.artifacts, Grace::none_for_tests(), kept.read_at)
            .unwrap();
        assert!(swept.removed.is_empty(), "{swept:?}");
        assert!(held(&store, &patch), "removed after its hold was let go");
        rt.shutdown(Duration::from_secs(5)).await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn a_digest_is_found_in_any_case_escaped_or_bare() {
    on_codex_runtime(async {
        let dir = scratch("spellings");
        let store = LocalStore::open(dir.join("artifacts")).unwrap();
        let [upper, escaped, bare, loose] =
            ["upper", "escaped", "bare", "loose"].map(|text| put(&store, text.as_bytes()));
        let mut rt = Durare::sqlite(&dir.join("atomic.db")).await.unwrap();
        register(&mut rt);
        rt.launch().await.unwrap();
        for (run, named) in [
            (
                "upper",
                format!("SHA256:{}", upper.digest().to_ascii_uppercase()),
            ),
            (
                "escaped",
                format!(r#"{{"patch":"artifact:\/\/sha256\/{}"}}"#, escaped.digest()),
            ),
            ("bare", format!("digest {} in a log", bare.digest())),
        ] {
            rt.start("names", run, named).await.unwrap();
            let _: String = rt.result(run).await.unwrap();
        }
        let swept = retention::sweep(&store, &rt, &BTreeSet::new(), Grace::none_for_tests())
            .await
            .unwrap();
        assert_eq!(swept.removed, [loose]);
        rt.shutdown(Duration::from_secs(5)).await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn a_digest_is_found_within_longer_hex_and_in_the_stores_path_form() {
    on_codex_runtime(async {
        let dir = scratch("windows");
        let store = LocalStore::open(dir.join("artifacts")).unwrap();
        let [encoded, trailed, pathed, loose] =
            ["encoded", "trailed", "pathed", "loose"].map(|text| put(&store, text.as_bytes()));
        let mut rt = Durare::sqlite(&dir.join("atomic.db")).await.unwrap();
        register(&mut rt);
        rt.launch().await.unwrap();
        let d = |a: &ArtifactRef| a.digest().to_owned();
        for (run, named) in [
            (
                "encoded",
                format!("https://example.invalid/a?ref=sha256%2F{}", d(&encoded)),
            ),
            (
                "trailed",
                format!("{}f00d is the digest and more", d(&trailed)),
            ),
            (
                "pathed",
                format!("blobs/sha256/{}/{}", &d(&pathed)[..2], &d(&pathed)[2..]),
            ),
        ] {
            rt.start("names", run, named).await.unwrap();
            let _: String = rt.result(run).await.unwrap();
        }
        let swept = retention::sweep(&store, &rt, &BTreeSet::new(), Grace::none_for_tests())
            .await
            .unwrap();
        assert_eq!(swept.removed, [loose]);
        rt.shutdown(Duration::from_secs(5)).await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    });
}
