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
//!   store itself;
//! - every gate and every entry of the feedback log, whole, of every
//!   project in the store, whichever the sweeping runtime is for, also read
//!   from the gate store: a question's answer and an approval's comment are
//!   there from the decision, before the run that asked wakes and records
//!   them, and a default before its deadline makes it the answer.
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
//! admitted and its input recorded. A decision's answer and comment come
//! from outside the run in the same way, and a sweep that read the gate
//! store before the decision would remove what they name:
//! [`deciding_keeping_named`] holds and marks it while the decision is
//! recorded.
//!
//! Nothing sweeps on its own: a caller decides when.

use atomic_core::artifact::{
    ArtifactError, ArtifactRef, ArtifactStore, Grace, Hold, LocalStore, Refreshed, Swept,
};
use atomic_durable::{DurableRuntime, Durare};
use atomic_schema::artifact::ArtifactCheckError;
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

/// What the store's runs and gates refer to: [`recorded`], [`pinned`] and
/// [`answered`].
pub async fn referenced(store: &LocalStore, rt: &Durare) -> anyhow::Result<BTreeSet<ArtifactRef>> {
    // Listed once, for every reading that looks for what the store holds.
    let present = present(store).await?;
    let mut found = recorded(&present, rt).await?;
    // Every project's gates, whichever the sweeping runtime is for, read
    // once for what they pin and what they name.
    let gates = atomic_durable::gate::every_gate(rt).await?;
    found.extend(pinned_in(&gates)?);
    found.extend(answered_in(&present, &gates, rt).await?);
    Ok(found)
}

/// The digests of everything in `store`, read off the async workers.
async fn present(store: &LocalStore) -> anyhow::Result<HashSet<String>> {
    let store = store.clone();
    Ok(tokio::task::spawn_blocking(move || store.stored())
        .await??
        .into_iter()
        .map(|a| a.digest().to_owned())
        .collect())
}

/// Every blob of `present` that the gate store names, open gates and
/// resolved ones alike: anywhere in a gate, such as its title, what it
/// shows, its question, the question's default, and the answer and comment
/// it was decided with, which later steps are handed; and anywhere in an
/// entry of the feedback log.
/// Every project's gates are read, whichever the runtime is for: a store's
/// content is kept for all of them.
pub async fn answered(
    present: &HashSet<String>,
    rt: &Durare,
) -> anyhow::Result<BTreeSet<ArtifactRef>> {
    answered_in(present, &atomic_durable::gate::every_gate(rt).await?, rt).await
}

/// [`answered`], of `gates` and the store's feedback log.
async fn answered_in(
    present: &HashSet<String>,
    gates: &[atomic_durable::gate::Gate],
    rt: &Durare,
) -> anyhow::Result<BTreeSet<ArtifactRef>> {
    let feedback = atomic_durable::gate::feedback(rt).await?;
    let mut found = BTreeSet::new();
    for record in answers(gates, &feedback)? {
        references(&record, present, &mut found);
    }
    Ok(found)
}

/// Each gate and each feedback entry the gate store holds, whole, as JSON:
/// whatever part of one names content is read.
fn answers(
    gates: &[atomic_durable::gate::Gate],
    feedback: &[atomic_durable::gate::Feedback],
) -> anyhow::Result<Vec<Value>> {
    let mut all = Vec::with_capacity(gates.len() + feedback.len());
    for gate in gates {
        all.push(serde_json::to_value(gate)?);
    }
    for entry in feedback {
        all.push(serde_json::to_value(entry)?);
    }
    Ok(all)
}

/// Every blob in `store` any run in the store names, finished or not: in its
/// input, its result or error, and every step's recorded output or error.
/// Runs are read a page at a time, in order of creation and id.
pub async fn recorded(
    present: &HashSet<String>,
    rt: &Durare,
) -> anyhow::Result<BTreeSet<ArtifactRef>> {
    let mut found = BTreeSet::new();
    let mut after = None;
    loop {
        let page = rt.runs_after(after.as_ref(), PAGE).await?;
        for (_, run) in &page {
            references(&run.input, present, &mut found);
            if let Some(output) = &run.output {
                references(output, present, &mut found);
            }
            if let Some(error) = &run.error {
                named_in(error, present, &mut found);
            }
            for step in rt.steps(&run.record.id).await? {
                if let Some(output) = &step.output {
                    references(output, present, &mut found);
                }
                if let Some(error) = &step.error {
                    named_in(error, present, &mut found);
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
    pinned_in(&atomic_durable::gate::every_gate(rt).await?)
}

/// [`pinned`], of `gates`.
fn pinned_in(gates: &[atomic_durable::gate::Gate]) -> anyhow::Result<BTreeSet<ArtifactRef>> {
    let mut found = BTreeSet::new();
    for pin in atomic_durable::gate::evidence_of(gates) {
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
    /// A field the workflow's input contract types as `atomic:artifact`
    /// names content the store does not hold, or a sweep removed content
    /// the input names after the store was listed.
    #[error("the run's input names {0}, which the artifact store does not hold")]
    Absent(ArtifactRef),
    /// A sweep held the store's lock for longer than a write waits for it.
    #[error(
        "a sweep of the artifact store held its lock for more than {} s while the run started",
        .0.as_secs()
    )]
    Locked(std::time::Duration),
    /// A field the workflow's input contract types as `atomic:artifact`
    /// names stored content the contract does not allow, such as one of
    /// another media type.
    #[error("the run's input does not fit its contract: {0}")]
    Invalid(String),
    /// The caller, or the process, was told to stop while it waited for
    /// the store's lock.
    #[error("the run was stopped while it waited for the artifact store, before it started")]
    Stopped,
}

/// Start run `id` of workflow `name` on `input`, keeping every artifact
/// `store` holds that the input names from every sweep until the run, and
/// so its input, is recorded. Each is held first, so a sweep that takes the
/// store's lock from then on keeps it; then marked as just written, under
/// the store's shared lock, so a sweep that read the runs before the run
/// existed keeps it by its grace. Holding and marking share one wait for
/// the lock, as long as a write waits; it is said once, and a process told
/// to stop waits no longer. `typed` is the workflow's input contract and
/// the input it is held to: the run is refused, before anything is
/// recorded, when a field it types as `atomic:artifact` names content the
/// store does not hold, or content the contract does not allow there (the
/// first such field it finds). It is refused too when a sweep removed what
/// the input names after the store was listed, or held the lock past the
/// wait, or when `halt` is set, or the process told to stop, while it
/// waits. Anything else the input's text names is held, never refused.
pub async fn start_keeping_input<I>(
    store: &LocalStore,
    rt: &Durare,
    name: &str,
    id: &str,
    input: I,
    typed: Option<(Value, Value)>,
    halt: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> anyhow::Result<()>
where
    I: Serialize + Send,
{
    start_keeping_input_then(store, rt, name, id, input, typed, halt, || {}).await
}

/// [`start_keeping_input`], calling `once_held` once the input's artifacts
/// are held and before they are marked: [`start_keeping_input`] calls
/// nothing there, and this module's tests sweep there, as a sweep that
/// takes the store's lock in that window would.
#[allow(clippy::too_many_arguments)] // the public function's, and the hook
async fn start_keeping_input_then<I>(
    store: &LocalStore,
    rt: &Durare,
    name: &str,
    id: &str,
    input: I,
    typed: Option<(Value, Value)>,
    halt: std::sync::Arc<std::sync::atomic::AtomicBool>,
    once_held: impl FnOnce(),
) -> anyhow::Result<()>
where
    I: Serialize + Send,
{
    let named = serde_json::to_value(&input)?;
    let began = std::time::Instant::now();
    let wait = Wait {
        what: "run",
        who: id.to_owned(),
        before: "starts",
        for_at_most: store.lock_wait(),
        began,
        until: began + store.lock_wait(),
        said: std::sync::atomic::AtomicBool::new(false),
        halt,
    };
    let wait = std::sync::Arc::new(wait);
    let held = {
        let (store, wait) = (store.clone(), wait.clone());
        tokio::task::spawn_blocking(move || hold_named(&store, &named, typed.as_ref(), &wait))
            .await??
    };
    once_held();
    {
        let store = store.clone();
        let artifacts: Vec<ArtifactRef> = held.iter().map(|(a, _)| a.clone()).collect();
        tokio::task::spawn_blocking(move || mark_named(&store, &artifacts, &wait)).await??;
    }
    rt.start(name, id, input).await?;
    // The input is recorded: a reading of the runs finds it from now on.
    drop(held);
    Ok(())
}

/// Why a decision whose answer or comment names stored content was not
/// recorded: nothing of it is.
#[derive(Debug, thiserror::Error)]
pub enum DecisionRefused {
    /// A sweep removed content the answer or comment names after the store
    /// was listed.
    #[error("the decision names {0}, which the artifact store no longer holds")]
    Absent(ArtifactRef),
    /// A sweep held the store's lock for longer than a write waits for it.
    #[error(
        "a sweep of the artifact store held its lock for more than {} s while the decision \
         was recorded",
        .0.as_secs()
    )]
    Locked(std::time::Duration),
    /// The process was told to stop while it waited for the store's lock.
    #[error("the decision was stopped while it waited for the artifact store")]
    Stopped,
    /// What the decision names does not fit a contract. Nothing gives a
    /// decision's content one today, so no decision is refused this way;
    /// it is here so that one never is quietly taken for another refusal.
    #[error("the decision does not fit its contract: {0}")]
    Invalid(String),
}

impl From<InputRefused> for DecisionRefused {
    fn from(refused: InputRefused) -> Self {
        match refused {
            InputRefused::Absent(artifact) => Self::Absent(artifact),
            InputRefused::Locked(waited) => Self::Locked(waited),
            InputRefused::Stopped => Self::Stopped,
            InputRefused::Invalid(why) => Self::Invalid(why),
        }
    }
}

/// Record a decision on gate `gate` through `decide`, keeping every
/// artifact `store` holds that `answer` or `comment` names from every sweep
/// until it is recorded. An answer comes from outside the run, as a run's
/// input does, and is only in the gate store until its run wakes: a sweep
/// that read the gate store before the decision and sweeps after it would
/// remove what the answer names. So each is held first, so a sweep that
/// takes the store's lock from then on keeps it; then marked as just
/// written, under the store's shared lock, so a sweep that read the gate
/// store before keeps it by its grace; then `decide` runs, and the holds
/// go once it has. It is refused, with nothing recorded, when a sweep
/// removed what they name after the store was listed, or held the lock
/// past the wait, or when the process is told to stop while it waits.
pub async fn deciding_keeping_named<T, F, Fut>(
    store: &LocalStore,
    gate: &str,
    answer: Option<&Value>,
    comment: Option<&str>,
    decide: F,
) -> anyhow::Result<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    deciding_keeping_named_then(store, gate, answer, comment, || {}, decide).await
}

/// [`deciding_keeping_named`], calling `once_held` once the content is held
/// and before it is marked: [`deciding_keeping_named`] calls nothing there,
/// and this module's tests remove content there, as a sweep that finished
/// just before the hold was taken would have.
async fn deciding_keeping_named_then<T, F, Fut>(
    store: &LocalStore,
    gate: &str,
    answer: Option<&Value>,
    comment: Option<&str>,
    once_held: impl FnOnce(),
    decide: F,
) -> anyhow::Result<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let named = serde_json::json!([answer, comment]);
    let began = std::time::Instant::now();
    let wait = std::sync::Arc::new(Wait {
        what: "the decision on gate",
        who: gate.to_owned(),
        before: "is recorded",
        for_at_most: store.lock_wait(),
        began,
        until: began + store.lock_wait(),
        said: std::sync::atomic::AtomicBool::new(false),
        halt: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    });
    let refused = |e: anyhow::Error| match e.downcast::<InputRefused>() {
        Ok(refused) => anyhow::Error::from(DecisionRefused::from(refused)),
        Err(e) => e,
    };
    let held = {
        let (store, wait) = (store.clone(), wait.clone());
        tokio::task::spawn_blocking(move || hold_named(&store, &named, None, &wait))
            .await?
            .map_err(refused)?
    };
    once_held();
    {
        let store = store.clone();
        let decision_names: Vec<ArtifactRef> = held.iter().map(|(a, _)| a.clone()).collect();
        tokio::task::spawn_blocking(move || mark_named(&store, &decision_names, &wait))
            .await?
            .map_err(refused)?;
    }
    let decided = decide().await;
    // Recorded, so a reading of the gate store finds what it names from
    // now on; or not, so nothing needs keeping.
    drop(held);
    decided
}

/// The one wait for the store's lock that admitting a run, or recording a
/// decision, shares.
struct Wait {
    /// What waits, as said: `run`, or `the decision on gate`.
    what: &'static str,
    /// Its id: the run's, or the gate's.
    who: String,
    /// What it waits to do: `starts`, or `is recorded`.
    before: &'static str,
    for_at_most: std::time::Duration,
    began: std::time::Instant,
    until: std::time::Instant,
    said: std::sync::atomic::AtomicBool,
    halt: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// How long admission waits for the store's lock before it says so: a
/// sweep's lock is usually let go sooner.
const SAY_AFTER: std::time::Duration = std::time::Duration::from_millis(500);

impl Wait {
    /// What is left of it.
    fn left(&self) -> std::time::Duration {
        self.until
            .saturating_duration_since(std::time::Instant::now())
    }

    /// Say, once, that the run waits for the store, once it has waited a
    /// moment.
    fn say(&self) {
        if self.began.elapsed() >= SAY_AFTER
            && !self.said.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            atomic_core::redact::Line::new()
                .plain("atomic: ")
                .plain(self.what)
                .plain(" ")
                .shown(&self.who)
                .plain(
                    ": the artifact store is held by another process, such as a sweep; it \
                     waits for it, up to ",
                )
                .shown(&self.for_at_most.as_secs().to_string())
                .plain(" s, before it ")
                .plain(self.before)
                .log();
        }
    }

    /// Pause before trying the lock again: whether the caller, or the
    /// process, was told to stop meanwhile.
    fn pause(&self) -> bool {
        let until =
            std::time::Instant::now() + std::time::Duration::from_millis(20).min(self.left());
        loop {
            if self.halt.load(std::sync::atomic::Ordering::SeqCst)
                || atomic_core::runtime::stopping()
            {
                return true;
            }
            let left = until.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            std::thread::sleep(left.min(std::time::Duration::from_millis(5)));
        }
    }

    /// The refusal when the store's lock was not had in time.
    fn ran_out(&self) -> InputRefused {
        InputRefused::Locked(self.for_at_most)
    }
}

/// A hold on every artifact `store` holds that `input` names. A field
/// `typed`'s contract types as `atomic:artifact` that names content the
/// store does not hold refuses the run first.
fn hold_named(
    store: &LocalStore,
    input: &Value,
    typed: Option<&(Value, Value)>,
    wait: &Wait,
) -> anyhow::Result<Vec<(ArtifactRef, Hold)>> {
    // Listed first, so what the check finds is in the listing the holds
    // are taken from.
    let present: HashSet<String> = store
        .stored()?
        .into_iter()
        .map(|a| a.digest().to_owned())
        .collect();
    if let Some((contract, value)) = typed
        && let Err(e) = atomic_schema::artifact::check_refs(contract, value, store)
    {
        let refused = match &e {
            ArtifactCheckError::Missing { text, .. } => match text.parse::<ArtifactRef>() {
                Ok(absent) => InputRefused::Absent(absent),
                Err(_) => InputRefused::Invalid(e.to_string()),
            },
            _ => InputRefused::Invalid(e.to_string()),
        };
        return Err(refused.into());
    }
    let mut named = BTreeSet::new();
    references(input, &present, &mut named);
    let mut held = Vec::with_capacity(named.len());
    let at_once = store.clone().waiting_at_most(std::time::Duration::ZERO);
    for artifact in named {
        let hold = loop {
            match at_once.hold(&artifact) {
                Ok(hold) => break hold,
                Err(ArtifactError::Locked { .. }) if !wait.left().is_zero() => {
                    wait.say();
                    if wait.pause() {
                        return Err(InputRefused::Stopped.into());
                    }
                }
                Err(ArtifactError::Locked { .. }) => return Err(wait.ran_out().into()),
                Err(e) => return Err(e.into()),
            }
        };
        held.push((artifact, hold));
    }
    Ok(held)
}

/// Mark each of `artifacts` as just written. [`LocalStore::refresh`] does
/// not wait for the store's lock, and a sweep holding it leaves the mark
/// unmade, so it is tried again until it is made, or finds the content
/// gone, for what is left of `wait`. One no longer stored, a lock held that
/// long, or a process told to stop refuses the run.
fn mark_named(store: &LocalStore, artifacts: &[ArtifactRef], wait: &Wait) -> anyhow::Result<()> {
    for artifact in artifacts {
        loop {
            match store.refresh(artifact)? {
                Refreshed::Now => break,
                Refreshed::Missing => return Err(InputRefused::Absent(artifact.clone()).into()),
                Refreshed::Held if !wait.left().is_zero() => {
                    wait.say();
                    if wait.pause() {
                        return Err(InputRefused::Stopped.into());
                    }
                }
                Refreshed::Held => return Err(wait.ran_out().into()),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::artifact::{ArtifactStore, Provenance};

    /// Every part of each gate and feedback entry the gate store keeps is
    /// read: a question's default and schema, the answer and comment a gate
    /// was decided with, a defaulted answer, a gate's title and what it
    /// shows, and an entry of the feedback log.
    #[test]
    fn every_part_of_what_the_gate_store_keeps_is_read() {
        use atomic_durable::gate::{
            Decision, Feedback, Gate, OnTimeout, Principal, Question, Resolution, Tier, Verdict,
        };
        // One digest for each place, each a blob the store holds.
        let digest = |n: u8| format!("{:064x}", u64::from(n) + 0xd1d1);
        let named = |n: u8| format!("sha256:{}", digest(n));
        let gate = |question: Option<Question>, resolution: Option<Resolution>, n: u8| Gate {
            id: "run:ask".into(),
            run: "run".into(),
            workflow: "w".into(),
            step: "ask".into(),
            title: format!("Which file? {}", named(n)),
            show: serde_json::json!({"shown": named(n + 1)}),
            evidence: Vec::new(),
            proposal: "p".into(),
            approvers: Vec::new(),
            tier: Tier::High,
            opened_at: 0,
            deadline: None,
            on_timeout: OnTimeout::Default,
            question,
            resolution,
        };
        let question = |default: Option<Value>, n: u8| Question {
            answer: serde_json::json!({"description": named(n)}),
            remember: true,
            default,
        };
        let decided = Resolution::Decided(Decision {
            verdict: Verdict::Approve,
            by: Principal::local("dev"),
            comment: Some(format!("see {}", named(20))),
            answer: Some(serde_json::json!({"file": named(21)})),
            command: "c".into(),
            at: 1,
        });
        let gates = [
            gate(
                Some(question(Some(serde_json::json!(named(3))), 4)),
                None,
                1,
            ),
            gate(Some(question(None, 5)), Some(decided), 6),
            gate(
                Some(question(None, 8)),
                Some(Resolution::Defaulted {
                    at: 1,
                    answer: serde_json::json!(named(9)),
                }),
                10,
            ),
            gate(None, None, 12),
        ];
        let feedback = [Feedback {
            gate: "run:ask".into(),
            workflow: "w".into(),
            step: "ask".into(),
            ask: "Which file?".into(),
            answer: serde_json::json!(named(30)),
            by: None,
            at: 1,
        }];
        let places: Vec<u8> = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 20, 21, 30];
        let present: HashSet<String> = places.iter().map(|n| digest(*n)).collect();
        let mut found = BTreeSet::new();
        for record in answers(&gates, &feedback).expect("serialized") {
            references(&record, &present, &mut found);
        }
        let found: BTreeSet<String> = found.iter().map(|a| a.digest().to_owned()).collect();
        let missed: Vec<u8> = places
            .into_iter()
            .filter(|n| !found.contains(&digest(*n)))
            .collect();
        assert!(missed.is_empty(), "not read: {missed:?}");
    }

    /// A decision naming content a sweep removed just before it was held is
    /// refused, with nothing recorded: the gate stays open, and no hold is
    /// left.
    #[test]
    fn a_decision_naming_swept_content_is_refused_and_the_gate_stays_open() {
        use atomic_durable::gate::{
            self, DecisionCommand, Deployment, GateSpec, HumanCredential, OnTimeout, Principal,
            Question, Tier, Verdict,
        };
        atomic_core::testing::on_codex_runtime(async {
            let (dir, _, store, chosen) = racing("decision-swept").await;
            let mut rt = Durare::sqlite(&dir.join("gates.db")).await.expect("store");
            rt.register(
                "asking",
                |ctx: atomic_durable::DurareContext, (): ()| async move {
                    let spec = GateSpec {
                        workflow: "asking".into(),
                        title: "Which file?".into(),
                        show: serde_json::json!({}),
                        evidence: Vec::new(),
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
                        question: Some(Question {
                            answer: serde_json::json!({"type": "object"}),
                            remember: false,
                            default: None,
                        }),
                    };
                    gate::gate(&ctx, "accept", spec).await
                },
            );
            rt.launch().await.expect("launch");
            rt.start("asking", "asking-run", ()).await.expect("start");
            let id = gate::gate_id("asking-run", "accept");
            let found = loop {
                if let Some(g) = gate::find(&rt, &id).await.expect("find") {
                    break g;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            };
            let me = HumanCredential::new(Principal::local("dev"), "the operator's secret");
            gate::enroll(&rt, &me).await.expect("enroll");
            let answer = serde_json::json!({"file": chosen.to_string()});
            let command = DecisionCommand {
                command_id: "swept".into(),
                gate: found.id.clone(),
                verdict: Verdict::Approve,
                comment: None,
                proposal: found.proposal.clone(),
                answer: Some(answer.clone()),
            };
            let error = deciding_keeping_named_then(
                &store,
                &found.id,
                Some(&answer),
                None,
                || {
                    // As if a sweep finished just before the hold was taken.
                    let blob = dir
                        .join("artifacts/blobs/sha256")
                        .join(&chosen.digest()[..2])
                        .join(&chosen.digest()[2..]);
                    std::fs::remove_file(blob).expect("remove");
                },
                || async { Ok(gate::decide(&rt, &me, command).await?) },
            )
            .await
            .expect_err("refused");
            assert!(
                matches!(
                    error.downcast_ref::<DecisionRefused>(),
                    Some(DecisionRefused::Absent(a)) if *a == chosen
                ),
                "{error}"
            );
            let still = gate::find(&rt, &found.id)
                .await
                .expect("find")
                .expect("the gate");
            assert!(still.resolution.is_none(), "recorded: {still:?}");
            assert!(store.held().expect("held").is_empty(), "a hold outlived it");
            rt.shutdown(std::time::Duration::from_secs(5))
                .await
                .expect("shutdown");
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

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
                None,
                Default::default(),
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
                None,
                Default::default(),
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
                None,
                Default::default(),
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
                    // Long enough that the mark is tried while it is held,
                    // however slow the machine.
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(3));
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

    /// A sweep holding the store's lock for longer than admission waits
    /// refuses the run, with the lock named, and nothing is recorded or
    /// left held.
    #[test]
    fn a_lock_held_past_the_wait_refuses_the_run() {
        atomic_core::testing::on_codex_runtime(async {
            let (dir, rt, store, input) = racing("held-past").await;
            let impatient = store.clone().waiting_at_most(std::time::Duration::ZERO);
            let lock = dir.join("artifacts/lock");
            let holder = std::sync::Mutex::new(None);
            let error = start_keeping_input_then(
                &impatient,
                &rt,
                "noop",
                "run-1",
                serde_json::json!({ "file": input.to_string() }),
                None,
                Default::default(),
                || {
                    let file = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&lock)
                        .expect("lock file");
                    file.lock().expect("the sweep's lock");
                    *holder.lock().expect("holder") = Some(file);
                },
            )
            .await
            .expect_err("refused");
            drop(holder);
            assert!(
                matches!(
                    error.downcast_ref::<InputRefused>(),
                    Some(InputRefused::Locked(_))
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
            rt.shutdown(std::time::Duration::from_secs(5))
                .await
                .expect("shutdown");
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// The store's lock, taken as a sweep takes it, until the returned
    /// file is dropped.
    fn sweeping(dir: &std::path::Path) -> std::fs::File {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join("artifacts/lock"))
            .expect("lock file");
        file.lock().expect("the sweep's lock");
        file
    }

    /// A sweep holding the store's lock before admission begins: with no
    /// wait left, the hold is refused as the lock, and nothing is recorded
    /// or held; let go part way through the wait, the run is admitted.
    #[test]
    fn a_hold_waits_for_the_lock_within_the_wait() {
        atomic_core::testing::on_codex_runtime(async {
            let (dir, rt, store, input) = racing("hold-wait").await;
            let lock = sweeping(&dir);
            let error = start_keeping_input(
                &store.clone().waiting_at_most(std::time::Duration::ZERO),
                &rt,
                "noop",
                "run-1",
                serde_json::json!({ "file": input.to_string() }),
                None,
                Default::default(),
            )
            .await
            .expect_err("refused");
            assert!(
                matches!(
                    error.downcast_ref::<InputRefused>(),
                    Some(InputRefused::Locked(_))
                ),
                "{error}"
            );
            assert!(rt.run_record("run-1").await.expect("record").is_none());
            assert!(
                store.held().expect("held").is_empty(),
                "a hold outlived the refusal"
            );
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(500));
                drop(lock);
            });
            start_keeping_input(
                &store,
                &rt,
                "noop",
                "run-2",
                serde_json::json!({ "file": input.to_string() }),
                None,
                Default::default(),
            )
            .await
            .expect("admitted once the lock is let go");
            rt.shutdown(std::time::Duration::from_secs(5))
                .await
                .expect("shutdown");
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// Told to stop while it waits for the store's lock, admission stops at
    /// once, and nothing is recorded or held.
    #[test]
    fn admission_told_to_stop_while_it_waits_stops() {
        atomic_core::testing::on_codex_runtime(async {
            let (dir, rt, store, input) = racing("halted").await;
            let lock = sweeping(&dir);
            let halt: std::sync::Arc<std::sync::atomic::AtomicBool> = Default::default();
            let told = halt.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(200));
                told.store(true, std::sync::atomic::Ordering::SeqCst);
            });
            let started = std::time::Instant::now();
            let error = start_keeping_input(
                &store,
                &rt,
                "noop",
                "run-1",
                serde_json::json!({ "file": input.to_string() }),
                None,
                halt,
            )
            .await
            .expect_err("stopped");
            drop(lock);
            assert!(
                matches!(
                    error.downcast_ref::<InputRefused>(),
                    Some(InputRefused::Stopped)
                ),
                "{error}"
            );
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "waited out the lock instead of stopping"
            );
            assert!(rt.run_record("run-1").await.expect("record").is_none());
            assert!(
                store.held().expect("held").is_empty(),
                "a hold outlived the stop"
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
                None,
                Default::default(),
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
            // A field the input's contract types as an artifact, naming
            // content never stored, is refused too, before anything is held.
            let never = ArtifactRef::from_digest(&"1".repeat(64)).expect("digest");
            let contract = serde_json::json!({
                "type": "object",
                "properties": {"file": {"$ref": "atomic:artifact"}, "text": {"type": "string"}}
            });
            let typed = serde_json::json!({ "file": never.to_string() });
            let error = start_keeping_input(
                &store,
                &rt,
                "noop",
                "run-2",
                typed.clone(),
                Some((contract.clone(), typed)),
                Default::default(),
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
            // Free text naming it is no reference: the run is admitted.
            let text = serde_json::json!({ "text": format!("{never} crashes the app") });
            start_keeping_input(
                &store,
                &rt,
                "noop",
                "run-3",
                text.clone(),
                Some((contract, text)),
                Default::default(),
            )
            .await
            .expect("admitted");
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
