// SPDX-License-Identifier: Apache-2.0

//! Retention: sweeping the artifact store, keeping everything the store's
//! runs and gates may still read.
//!
//! The sweep fails closed. Besides what the caller keeps, it keeps every
//! blob whose digest appears, in either case, anywhere in a string at any
//! depth: as `artifact://sha256/<hex>`, `sha256:<hex>`, the store's path
//! form `sha256/<2 hex>/<62 hex>`, bare, within a longer hex string, with
//! JSON-escaped or percent-encoded slashes, or inside markdown or JSON
//! carried as text. It looks for digests the store holds, in:
//!
//! - every run's input, and its result or error once it has one;
//! - every step's recorded output or error, in every run in the store,
//!   finished or not: a run's pinned definitions (its snapshot) and the
//!   results its steps hand on by reference among them;
//! - every gate's pinned evidence, open or resolved, read from the gate
//!   store itself.
//!
//! It also keeps anything written within the grace period before it began
//! reading these, however long the reading and the wait for the store's
//! lock take, and anything a store holds as it begins reading or as it
//! sweeps: an exec step holds the files its command handed over from before
//! they are stored until its turn is checkpointed. What it cannot see: any
//! other producer that stores content more than a grace period before the
//! step that refers to it is checkpointed. And what lives in the source tree
//! (eval cases, cassettes, reports) refers to artifacts no run records, so
//! only the caller's keep set covers it.
//!
//! A run's input can name content stored long before the run starts. Until
//! the input is recorded, no reading of the runs finds it, so a sweep that
//! read the runs before then could remove it: [`start_keeping_input`] holds
//! what the input names, and marks it as just written, while the run is
//! admitted and its input recorded.
//!
//! Nothing sweeps on its own: a caller decides when.

use atomic_core::artifact::{
    ArtifactRef, ArtifactStore, Grace, Hold, LocalStore, Refreshed, Swept,
};
use atomic_durable::{DurableRuntime, Durare};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};

/// How many runs a scan reads at a time.
const PAGE: i64 = 100;

/// What a sweep keeps, as read from the store's runs and gates at a time.
#[derive(Clone, Debug)]
pub struct Kept {
    /// The artifacts to keep.
    pub artifacts: BTreeSet<ArtifactRef>,
    /// When the reading began: content written within a grace period before
    /// it is kept too.
    pub read_at: std::time::SystemTime,
}

/// Read what a sweep of `store` keeps: `keep`, everything held as the
/// reading begins, and everything [`referenced`] finds, with the time the
/// reading began.
///
/// The holds are read first: a step lets its hold go once it has recorded
/// the reference, which the reading of the runs may already have passed,
/// so the holds the sweep reads later under the store's lock may no longer
/// show it.
pub async fn read_kept(
    store: &LocalStore,
    rt: &Durare,
    keep: &BTreeSet<ArtifactRef>,
) -> anyhow::Result<Kept> {
    read_kept_then(store, rt, keep, || {}).await
}

/// [`read_kept`], calling `between` once the holds are read and before the
/// runs are: [`read_kept`] calls nothing there, and this module's tests let
/// a hold go there, as a step that records its reference in that window
/// would.
async fn read_kept_then(
    store: &LocalStore,
    rt: &Durare,
    keep: &BTreeSet<ArtifactRef>,
    between: impl FnOnce(),
) -> anyhow::Result<Kept> {
    let read_at = std::time::SystemTime::now();
    let mut artifacts = keep.clone();
    let held = held_now(store).await?;
    between();
    artifacts.extend(held);
    artifacts.extend(referenced(store, rt).await?);
    Ok(Kept { artifacts, read_at })
}

/// What every store holds now, read off the async workers.
async fn held_now(store: &LocalStore) -> anyhow::Result<BTreeSet<ArtifactRef>> {
    let store = store.clone();
    Ok(tokio::task::spawn_blocking(move || store.held()).await??)
}

/// Every blob of `present` that `text` may name, in either case: any 64 hex
/// digits within it, however much hex surrounds them, and the store's own
/// path form, `sha256/<2 hex>/<62 hex>`. Slashes may be JSON-escaped (`\/`)
/// or percent-encoded (`%2F`). That finds `artifact://sha256/<hex>`,
/// `sha256:<hex>`, a bare digest and one inside a longer hex string; only a
/// digest the store holds is kept, so unrelated hashes cost nothing.
fn named_in(text: &str, present: &HashSet<String>, into: &mut BTreeSet<ArtifactRef>) {
    let text = text
        .to_ascii_lowercase()
        .replace("\\/", "/")
        .replace("%2f", "/");
    let bytes = text.as_bytes();
    let mut keep = |digest: &str| {
        if present.contains(digest)
            && let Ok(a) = ArtifactRef::from_digest(digest)
        {
            into.insert(a);
        }
    };
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_hexdigit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
            i += 1;
        }
        let run = &text[start..i];
        for at in 0..run.len().saturating_sub(63) {
            keep(&run[at..at + 64]);
        }
        // The path form: two hex digits, a slash, then the other 62.
        if run.len() >= 62
            && start >= 3
            && bytes[start - 1] == b'/'
            && bytes[start - 2].is_ascii_hexdigit()
            && bytes[start - 3].is_ascii_hexdigit()
            && (start == 3 || !bytes[start - 4].is_ascii_hexdigit())
        {
            keep(&format!("{}{}", &text[start - 3..start - 1], &run[..62]));
        }
    }
}

/// Every blob of `present` named in any string in `value`, at any depth,
/// keys included.
fn references(value: &Value, present: &HashSet<String>, into: &mut BTreeSet<ArtifactRef>) {
    match value {
        Value::String(s) => named_in(s, present, into),
        Value::Array(items) => items.iter().for_each(|v| references(v, present, into)),
        Value::Object(map) => map.iter().for_each(|(k, v)| {
            named_in(k, present, into);
            references(v, present, into);
        }),
        _ => {}
    }
}

/// What the store's runs and gates refer to: [`recorded`] and [`pinned`].
pub async fn referenced(store: &LocalStore, rt: &Durare) -> anyhow::Result<BTreeSet<ArtifactRef>> {
    let mut found = recorded(store, rt).await?;
    found.extend(pinned(rt).await?);
    Ok(found)
}

/// Every blob in `store` any run in the store names, finished or not: in its
/// input, its result or error, and every step's recorded output or error.
/// Runs are read a page at a time, in order of creation and id.
pub async fn recorded(store: &LocalStore, rt: &Durare) -> anyhow::Result<BTreeSet<ArtifactRef>> {
    let present: HashSet<String> = {
        let store = store.clone();
        tokio::task::spawn_blocking(move || store.stored())
            .await??
            .into_iter()
            .map(|a| a.digest().to_owned())
            .collect()
    };
    let mut found = BTreeSet::new();
    let mut after = None;
    loop {
        let page = rt.runs_after(after.as_ref(), PAGE).await?;
        for (_, run) in &page {
            references(&run.input, &present, &mut found);
            if let Some(output) = &run.output {
                references(output, &present, &mut found);
            }
            if let Some(error) = &run.error {
                named_in(error, &present, &mut found);
            }
            for step in rt.steps(&run.record.id).await? {
                if let Some(output) = &step.output {
                    references(output, &present, &mut found);
                }
                if let Some(error) = &step.error {
                    named_in(error, &present, &mut found);
                }
            }
        }
        match page.last() {
            Some((key, _)) if page.len() as i64 == PAGE => after = Some(key.clone()),
            _ => return Ok(found),
        }
    }
}

/// Every digest a gate in the store pins as evidence, open or resolved. The
/// gate store keeps these apart from the run's own records, which a run's
/// gate step may not repeat, so they are read from it directly.
pub async fn pinned(rt: &Durare) -> anyhow::Result<BTreeSet<ArtifactRef>> {
    let mut found = BTreeSet::new();
    for pin in atomic_durable::gate::pinned_evidence(rt).await? {
        let digest = pin
            .strip_prefix("sha256:")
            .ok_or_else(|| anyhow::anyhow!("a gate pins {pin}, which is not a content digest"))?;
        found.insert(ArtifactRef::from_digest(digest)?);
    }
    Ok(found)
}

/// Sweep `store`, keeping `keep`, everything [`referenced`] finds, and
/// anything written within `grace` before it began reading the store's runs
/// (see [`read_kept`] and [`LocalStore::sweep`]).
pub async fn sweep(
    store: &LocalStore,
    rt: &Durare,
    keep: &BTreeSet<ArtifactRef>,
    grace: Grace,
) -> anyhow::Result<Swept> {
    let kept = read_kept(store, rt, keep).await?;
    let store = store.clone();
    Ok(
        tokio::task::spawn_blocking(move || store.sweep(&kept.artifacts, grace, kept.read_at))
            .await??,
    )
}

/// Why a run's input could not be kept from a sweep: the run is refused,
/// and nothing of it is recorded.
#[derive(Debug, thiserror::Error)]
pub enum InputRefused {
    /// The input names, as `artifact://sha256/…`, content the store does
    /// not hold, or a sweep removed content it names after the store was
    /// listed.
    #[error("the run's input names {0}, which the artifact store does not hold")]
    Absent(ArtifactRef),
    /// A sweep held the store's lock for longer than a write waits for it.
    #[error(
        "a sweep of the artifact store held its lock for more than {} s while the run started",
        .0.as_secs()
    )]
    Locked(std::time::Duration),
}

/// Start run `id` of workflow `name` on `input`, keeping every artifact
/// `store` holds that the input names from every sweep until the run, and
/// so its input, is recorded. Each is held first, so a sweep that takes the
/// store's lock from then on keeps it; then marked as just written, under
/// the store's shared lock, waited for as a write waits, so a sweep that
/// read the runs before the run existed keeps it by its grace. The run is
/// refused, before anything is recorded, when its input names
/// `artifact://sha256/…` content the store does not hold, when a sweep
/// removed what it names after the store was listed, or when a sweep holds
/// the lock for longer than a write waits.
pub async fn start_keeping_input<I>(
    store: &LocalStore,
    rt: &Durare,
    name: &str,
    id: &str,
    input: I,
) -> anyhow::Result<()>
where
    I: Serialize + Send,
{
    start_keeping_input_then(store, rt, name, id, input, || {}).await
}

/// [`start_keeping_input`], calling `once_held` once the input's artifacts
/// are held and before they are marked: [`start_keeping_input`] calls
/// nothing there, and this module's tests sweep there, as a sweep that
/// takes the store's lock in that window would.
async fn start_keeping_input_then<I>(
    store: &LocalStore,
    rt: &Durare,
    name: &str,
    id: &str,
    input: I,
    once_held: impl FnOnce(),
) -> anyhow::Result<()>
where
    I: Serialize + Send,
{
    let named = serde_json::to_value(&input)?;
    let held = {
        let store = store.clone();
        tokio::task::spawn_blocking(move || hold_named(&store, &named)).await??
    };
    once_held();
    {
        let store = store.clone();
        let artifacts: Vec<ArtifactRef> = held.iter().map(|(a, _)| a.clone()).collect();
        tokio::task::spawn_blocking(move || mark_named(&store, &artifacts)).await??;
    }
    rt.start(name, id, input).await?;
    // The input is recorded: a reading of the runs finds it from now on.
    drop(held);
    Ok(())
}

/// A hold on every artifact `store` holds that `input` names; an
/// `artifact://sha256/…` reference to content it does not hold refuses the
/// run.
fn hold_named(store: &LocalStore, input: &Value) -> anyhow::Result<Vec<(ArtifactRef, Hold)>> {
    let present: HashSet<String> = store
        .stored()?
        .into_iter()
        .map(|a| a.digest().to_owned())
        .collect();
    let mut explicit = BTreeSet::new();
    explicit_references(input, &mut explicit);
    if let Some(absent) = explicit.into_iter().find(|a| !present.contains(a.digest())) {
        return Err(InputRefused::Absent(absent).into());
    }
    let mut named = BTreeSet::new();
    references(input, &present, &mut named);
    let mut held = Vec::with_capacity(named.len());
    for artifact in named {
        let hold = store.hold(&artifact)?;
        held.push((artifact, hold));
    }
    Ok(held)
}

/// Every `artifact://sha256/<digest>` reference written out in `value`'s
/// strings and keys.
fn explicit_references(value: &Value, into: &mut BTreeSet<ArtifactRef>) {
    match value {
        Value::String(s) => written_out(s, into),
        Value::Array(items) => items.iter().for_each(|v| explicit_references(v, into)),
        Value::Object(map) => map.iter().for_each(|(k, v)| {
            written_out(k, into);
            explicit_references(v, into);
        }),
        _ => {}
    }
}

/// Every `artifact://sha256/<digest>` written out in `text`.
fn written_out(text: &str, into: &mut BTreeSet<ArtifactRef>) {
    const PREFIX: &str = "artifact://sha256/";
    let mut rest = text;
    while let Some(at) = rest.find(PREFIX) {
        let after = &rest[at + PREFIX.len()..];
        if let Some(digest) = after.get(..64)
            && let Ok(a) = ArtifactRef::from_digest(digest)
        {
            into.insert(a);
        }
        rest = after;
    }
}

/// Mark each of `artifacts` as just written. [`LocalStore::refresh`] does
/// not wait for the store's lock, and a sweep holding it leaves the mark
/// unmade, so it is tried again until it is made, or finds the content
/// gone, for as long as a write waits for the lock. One no longer stored,
/// or a lock held that long, refuses the run.
fn mark_named(store: &LocalStore, artifacts: &[ArtifactRef]) -> anyhow::Result<()> {
    let wait = store.lock_wait();
    let deadline = std::time::Instant::now() + wait;
    for artifact in artifacts {
        loop {
            match store.refresh(artifact)? {
                Refreshed::Now => break,
                Refreshed::Missing => return Err(InputRefused::Absent(artifact.clone()).into()),
                Refreshed::Held if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Refreshed::Held => return Err(InputRefused::Locked(wait).into()),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::artifact::{ArtifactStore, Provenance};

    /// A store with one artifact in it written an hour ago, and a durable
    /// runtime with a workflow that does nothing, launched.
    async fn racing(name: &str) -> (std::path::PathBuf, Durare, LocalStore, ArtifactRef) {
        let dir =
            std::env::temp_dir().join(format!("atomic-retention-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        let mut rt = Durare::sqlite(&dir.join("atomic.db")).await.expect("store");
        rt.register(
            "noop",
            |_ctx: atomic_durable::DurareContext, _input: Value| async move { Ok(()) },
        );
        rt.launch().await.expect("launch");
        let store = LocalStore::open(dir.join("artifacts")).expect("artifacts");
        let input = store
            .put(
                &mut &b"stored well before the run"[..],
                Provenance::default(),
            )
            .expect("put")
            .artifact;
        for blob in store.stored().expect("stored") {
            let path = dir
                .join("artifacts/blobs/sha256")
                .join(&blob.digest()[..2])
                .join(&blob.digest()[2..]);
            std::fs::File::open(&path)
                .and_then(|f| {
                    f.set_modified(
                        std::time::SystemTime::now() - std::time::Duration::from_secs(3600),
                    )
                })
                .expect("backdate");
        }
        (dir, rt, store, input)
    }

    /// A sweep that read the runs before a run existed, and takes the
    /// store's lock while the run is admitted, keeps what the run's input
    /// names: it is held.
    #[test]
    fn a_sweep_during_admission_keeps_what_the_input_names() {
        atomic_core::testing::on_codex_runtime(async {
            let (dir, rt, store, input) = racing("admitting").await;
            let kept = read_kept(&store, &rt, &BTreeSet::new())
                .await
                .expect("read");
            let swept = std::sync::Mutex::new(None);
            start_keeping_input_then(
                &store,
                &rt,
                "noop",
                "run-1",
                serde_json::json!({ "file": input.to_string() }),
                || {
                    let done = store.sweep(&kept.artifacts, Grace::none_for_tests(), kept.read_at);
                    *swept.lock().expect("lock") = Some(done.expect("sweep"));
                },
            )
            .await
            .expect("admitted");
            let swept = swept.lock().expect("lock").take().expect("swept");
            assert!(swept.removed.is_empty(), "{swept:?}");
            assert!(
                store.contains(&input).expect("contains"),
                "removed during admission"
            );
            rt.shutdown(std::time::Duration::from_secs(5))
                .await
                .expect("shutdown");
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// A sweep that read the runs before a run existed, and takes the
    /// store's lock once the run is admitted and its hold let go, keeps what
    /// the run's input names: it was marked as just written.
    #[test]
    fn a_sweep_after_admission_keeps_what_the_input_names() {
        atomic_core::testing::on_codex_runtime(async {
            let (dir, rt, store, input) = racing("admitted").await;
            let kept = read_kept(&store, &rt, &BTreeSet::new())
                .await
                .expect("read");
            start_keeping_input(
                &store,
                &rt,
                "noop",
                "run-1",
                serde_json::json!({ "file": input.to_string() }),
            )
            .await
            .expect("admitted");
            let swept = store
                .sweep(&kept.artifacts, Grace::none_for_tests(), kept.read_at)
                .expect("sweep");
            assert!(swept.removed.is_empty(), "{swept:?}");
            assert!(
                store.contains(&input).expect("contains"),
                "removed after admission"
            );
            rt.shutdown(std::time::Duration::from_secs(5))
                .await
                .expect("shutdown");
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// Two sweeps that both read the runs before a run existed: the first
    /// takes the store's lock while the run is admitted, keeps what the
    /// input names because it is held, and holds the lock a while longer;
    /// the mark waits for it rather than being skipped, so the second,
    /// queued behind it, keeps what the input names by its grace.
    #[test]
    fn two_overlapping_sweeps_keep_what_the_input_names() {
        atomic_core::testing::on_codex_runtime(async {
            let (dir, rt, store, input) = racing("overlapping").await;
            let first = read_kept(&store, &rt, &BTreeSet::new())
                .await
                .expect("read");
            let second = read_kept(&store, &rt, &BTreeSet::new())
                .await
                .expect("read");
            let lock = dir.join("artifacts/lock");
            start_keeping_input_then(
                &store,
                &rt,
                "noop",
                "run-1",
                serde_json::json!({ "file": input.to_string() }),
                || {
                    let swept = store
                        .sweep(&first.artifacts, Grace::none_for_tests(), first.read_at)
                        .expect("sweep");
                    assert!(swept.removed.is_empty(), "{swept:?}");
                    // Still holding the store's lock as the mark is tried.
                    let file = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&lock)
                        .expect("lock file");
                    file.lock().expect("the sweep's lock");
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(300));
                        drop(file);
                    });
                },
            )
            .await
            .expect("admitted");
            let swept = store
                .sweep(&second.artifacts, Grace::none_for_tests(), second.read_at)
                .expect("sweep");
            assert!(swept.removed.is_empty(), "{swept:?}");
            assert!(
                store.contains(&input).expect("contains"),
                "the second sweep removed what the input names"
            );
            rt.shutdown(std::time::Duration::from_secs(5))
                .await
                .expect("shutdown");
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// A run whose input names content a sweep removed before the run was
    /// admitted is refused, and nothing is recorded.
    #[test]
    fn a_run_naming_swept_content_is_refused() {
        atomic_core::testing::on_codex_runtime(async {
            let (dir, rt, store, input) = racing("swept").await;
            let error = start_keeping_input_then(
                &store,
                &rt,
                "noop",
                "run-1",
                serde_json::json!({ "file": input.to_string() }),
                || {
                    // As if a sweep finished just before the hold was taken.
                    let blob = dir
                        .join("artifacts/blobs/sha256")
                        .join(&input.digest()[..2])
                        .join(&input.digest()[2..]);
                    std::fs::remove_file(blob).expect("remove");
                },
            )
            .await
            .expect_err("refused");
            assert!(
                matches!(
                    error.downcast_ref::<InputRefused>(),
                    Some(InputRefused::Absent(a)) if *a == input
                ),
                "{error}"
            );
            assert!(
                rt.run_record("run-1").await.expect("record").is_none(),
                "recorded"
            );
            assert!(
                store.held().expect("held").is_empty(),
                "a hold outlived the refusal"
            );
            // An input that names, written out, content never stored is
            // refused too, before anything is held.
            let never = ArtifactRef::from_digest(&"1".repeat(64)).expect("digest");
            let error = start_keeping_input(
                &store,
                &rt,
                "noop",
                "run-2",
                serde_json::json!({ "file": never.to_string() }),
            )
            .await
            .expect_err("refused");
            assert!(
                matches!(
                    error.downcast_ref::<InputRefused>(),
                    Some(InputRefused::Absent(a)) if *a == never
                ),
                "{error}"
            );
            assert!(rt.run_record("run-2").await.expect("record").is_none());
            rt.shutdown(std::time::Duration::from_secs(5))
                .await
                .expect("shutdown");
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// The holds are read before the runs: a step that records its
    /// reference and lets its hold go while the sweep reads the runs, after
    /// the sweep has read past its run, still keeps its content.
    #[test]
    fn the_holds_are_read_before_the_runs() {
        atomic_core::testing::on_codex_runtime(async {
            let dir = std::env::temp_dir().join(format!(
                "atomic-retention-holds-first-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch");
            let rt = Durare::sqlite(&dir.join("atomic.db")).await.expect("store");
            rt.launch().await.expect("launch");
            let store = LocalStore::open(dir.join("artifacts")).expect("artifacts");
            let patch = store
                .put(
                    &mut &b"let go as the runs are read"[..],
                    Provenance::default(),
                )
                .expect("put")
                .artifact;
            for blob in store.stored().expect("stored") {
                let path = dir
                    .join("artifacts/blobs/sha256")
                    .join(&blob.digest()[..2])
                    .join(&blob.digest()[2..]);
                std::fs::File::open(&path)
                    .and_then(|f| {
                        f.set_modified(
                            std::time::SystemTime::now() - std::time::Duration::from_secs(3600),
                        )
                    })
                    .expect("backdate");
            }
            let hold = store.hold(&patch).expect("hold");
            let kept = read_kept_then(&store, &rt, &BTreeSet::new(), || drop(hold))
                .await
                .expect("read");
            let swept = store
                .sweep(&kept.artifacts, Grace::none_for_tests(), kept.read_at)
                .expect("sweep");
            assert!(swept.removed.is_empty(), "{swept:?}");
            assert!(
                store.contains(&patch).expect("contains"),
                "removed after its hold was let go"
            );
            rt.shutdown(std::time::Duration::from_secs(5))
                .await
                .expect("shutdown");
            let _ = std::fs::remove_dir_all(&dir);
        });
    }
}
