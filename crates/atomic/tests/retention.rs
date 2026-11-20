// SPDX-License-Identifier: Apache-2.0

//! Retention keeps what gates pin.
//!
//! - A sweep removes content nothing keeps, with its manifests, and keeps
//!   every digest a gate in the store pins as evidence, open or resolved,
//!   as well as what the caller keeps and what was written within the grace
//!   period; content pinned and kept twice over survives once.
//! - Two sweeps at once against the same store, while a step writes again
//!   content they would remove, both succeed, remove each unkept blob once
//!   between them, and leave the rewritten content whole: the write and
//!   the sweeps take the store's lock in turn, and a write makes content
//!   recent again.

#![allow(clippy::unwrap_used)]

use atomic_app::retention;
use atomic_core::artifact::{ArtifactStore, LocalStore, Provenance};
use atomic_core::testing::on_codex_runtime;
use atomic_durable::gate::{
    self, DecisionCommand, Deployment, GateSpec, HumanCredential, OnTimeout, Principal, Tier,
    Verdict,
};
use atomic_durable::{DurableRuntime, Durare, DurareContext};
use serde_json::json;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn scratch(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("atomic-retention-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn put(store: &LocalStore, bytes: &[u8]) -> String {
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
        .digest()
        .to_owned()
}

fn held(store: &LocalStore, digest: &str) -> bool {
    let a = atomic_core::artifact::ArtifactRef::from_digest(digest).unwrap();
    store.contains(&a).unwrap() && !store.manifests(&a).unwrap().is_empty()
}

/// A run that opens a gate showing `evidence` and waits for it.
fn register(rt: &mut Durare) {
    rt.register(
        "pinning",
        |ctx: DurareContext, evidence: Vec<String>| async move {
            let spec = GateSpec {
                workflow: "pinning".into(),
                title: "Accept the patch".into(),
                show: json!({"patch": evidence.first()}),
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
            };
            gate::gate(&ctx, "accept", spec).await
        },
    );
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
            vec![format!("sha256:{pinned}"), format!("sha256:{both}")],
        )
        .await
        .unwrap();
        let g = opened(&rt, "pinning-run").await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        let keep: BTreeSet<String> = [kept.clone(), format!("sha256:{both}")].into();
        let swept = retention::sweep(&store, &rt, &keep, Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(
            swept.removed.iter().map(|a| a.digest()).collect::<Vec<_>>(),
            [loose.as_str()]
        );
        assert_eq!(swept.kept, 3);
        assert!(!held(&store, &loose));
        for digest in [&pinned, &both, &kept] {
            assert!(held(&store, digest), "{digest} was swept");
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
            },
        )
        .await
        .unwrap();
        let swept = retention::sweep(&store, &rt, &BTreeSet::new(), Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(
            swept.removed.iter().map(|a| a.digest()).collect::<Vec<_>>(),
            [kept.as_str()],
            "only what the caller no longer keeps"
        );
        assert!(held(&store, &pinned) && held(&store, &both));

        // Content written within the grace period is kept, referenced or not.
        let fresh = put(&store, b"just written");
        let swept = retention::sweep(&store, &rt, &BTreeSet::new(), Duration::from_secs(3600))
            .await
            .unwrap();
        assert!(swept.removed.is_empty());
        assert!(held(&store, &fresh));
        rt.shutdown(Duration::from_secs(5)).await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    });
}

#[test]
fn sweeps_at_once_remove_each_unkept_blob_once_and_spare_a_rewrite() {
    on_codex_runtime(async {
        let dir = scratch("concurrent");
        let store = LocalStore::open(dir.join("artifacts")).unwrap();
        let pinned = put(&store, b"evidence");
        let loose: Vec<String> = (0..40)
            .map(|i| put(&store, format!("loose {i}").as_bytes()))
            .collect();
        let mut rt = Durare::sqlite(&dir.join("atomic.db")).await.unwrap();
        register(&mut rt);
        rt.launch().await.unwrap();
        rt.start(
            "pinning",
            "concurrent-run",
            vec![format!("sha256:{pinned}")],
        )
        .await
        .unwrap();
        opened(&rt, "concurrent-run").await;
        // Everything already stored is an hour old: past the grace period.
        let hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
        for entry in walk(&dir.join("artifacts/blobs")) {
            std::fs::File::options()
                .write(true)
                .open(&entry)
                .unwrap()
                .set_modified(hour_ago)
                .unwrap();
        }

        // While both sweep, a step writes one of the loose blobs again: it
        // must be whole afterwards, whichever ran first.
        let rewriter = {
            let store = store.clone();
            tokio::task::spawn_blocking(move || put(&store, b"loose 7"))
        };
        let keep = BTreeSet::new();
        let grace = Duration::from_secs(60);
        let (a, b) = tokio::join!(
            retention::sweep(&store, &rt, &keep, grace),
            retention::sweep(&store, &rt, &keep, grace),
        );
        let rewritten = rewriter.await.unwrap();
        let (a, b) = (a.unwrap(), b.unwrap());
        let mut removed: Vec<&str> = a
            .removed
            .iter()
            .chain(&b.removed)
            .map(|r| r.digest())
            .collect();
        removed.sort_unstable();
        let mut once = removed.clone();
        once.dedup();
        assert_eq!(removed, once, "a blob was removed twice");
        assert!(held(&store, &pinned), "the pinned evidence was swept");
        assert!(held(&store, &rewritten), "the rewritten content is gone");
        for digest in loose.iter().filter(|d| **d != rewritten) {
            assert!(!held(&store, digest), "{digest} was left");
        }
        rt.shutdown(Duration::from_secs(5)).await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    });
}

/// Every file under `dir`.
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
