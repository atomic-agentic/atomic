// SPDX-License-Identifier: Apache-2.0

//! Runs whose workflow code panicked.
//!
//! A panic in atomic's own workflow code is a bug in atomic, not a fault of
//! the run: the engine parks the run instead of retrying it, since a panic
//! that happened once will likely happen again. Every workflow atomic
//! registers runs inside [`caught`], which notes the panic's message,
//! redacted, before the engine sees the panic: in this process, for `atomic
//! run`, `atomic runs resume` and `atomic serve` to name as the cause, and in
//! the project's private state, so a later process (`atomic serve` when it
//! starts, or a run's inspection) can name it too.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Mutex, OnceLock};
use std::task::{Context, Poll};

/// The panics noted in this process, by run.
static NOTED: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());

/// Where this process keeps them for later ones: the project's private
/// state, set when its engine opens.
static KEPT_IN: OnceLock<PathBuf> = OnceLock::new();

/// Keep noted panics in `dir`, in the project's private state.
pub fn keep_in(dir: PathBuf) {
    let _ = KEPT_IN.set(dir);
}

/// The file a panic of run `run` is kept in, under `dir`.
fn file(dir: &Path, run: &str) -> PathBuf {
    use sha2::Digest;
    dir.join(format!("{:x}.json", sha2::Sha256::digest(run.as_bytes())))
}

/// The redacted message of the panic run `run`'s workflow code raised, if
/// one was noted: in this process, or kept in the project's private state.
pub fn of(run: &str) -> Option<String> {
    if let Some(m) = NOTED.lock().unwrap_or_else(|e| e.into_inner()).get(run) {
        return Some(m.clone());
    }
    KEPT_IN.get().and_then(|dir| kept(dir, run))
}

/// The panic of run `run` kept under `dir`, the project's panics directory.
pub fn kept(dir: &Path, run: &str) -> Option<String> {
    let text = std::fs::read_to_string(file(dir, run)).ok()?;
    let kept: serde_json::Value = serde_json::from_str(&text).ok()?;
    (kept["run"] == run)
        .then(|| kept["message"].as_str().map(str::to_owned))
        .flatten()
}

/// Forget run `run`'s noted panic, here and in the project's private state:
/// it is being resumed, and what it does now is noted afresh.
pub fn forget(run: &str) {
    NOTED.lock().unwrap_or_else(|e| e.into_inner()).remove(run);
    if let Some(dir) = KEPT_IN.get() {
        let _ = std::fs::remove_file(file(dir, run));
    }
}

/// Note that run `run`'s workflow code panicked with `payload`.
fn note(run: &str, payload: &(dyn std::any::Any + Send)) -> String {
    let raw = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic with no message".to_owned());
    let message = atomic_core::redact::bounded(&atomic_core::redact::text(&raw), 2_000);
    NOTED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(run.to_owned(), message.clone());
    if let Some(dir) = KEPT_IN.get() {
        let kept = serde_json::json!({
            "run": run,
            "message": message,
            "at": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        });
        // Best effort: the run is parked either way, and this process says
        // why; a later one may not.
        let _ = std::fs::create_dir_all(dir)
            .and_then(|()| std::fs::write(file(dir, run), kept.to_string()));
    }
    message
}

/// What a person is told about run `run`, parked because its workflow code
/// panicked with `message`.
pub fn cause(run: &str, message: &str) -> String {
    format!(
        "run {run} stopped: atomic's own workflow code panicked ({message}), so the run is \
         parked, not retried"
    )
}

/// What a person can do about it.
pub fn hint(run: &str) -> String {
    format!(
        "this is a bug in atomic, and the panic will likely happen again if the run is resumed \
         as it is: report it with the message above, then continue the run with `atomic runs \
         resume {run}` once an atomic without it is installed"
    )
}

/// `workflow`, the workflow code of run `run`, with a panic in it noted
/// before it reaches the engine, and said at once while serving.
pub fn caught<F: Future>(run: String, workflow: F) -> Caught<F> {
    Caught {
        run,
        workflow: Box::pin(workflow),
    }
}

/// A workflow run inside [`caught`].
pub struct Caught<F> {
    run: String,
    workflow: Pin<Box<F>>,
}

impl<F: Future> Future for Caught<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let this = &mut *self;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            this.workflow.as_mut().poll(cx)
        })) {
            Ok(polled) => polled,
            Err(payload) => {
                let message = note(&this.run, payload.as_ref());
                if crate::workflows::serving() {
                    atomic_core::log_line!(
                        "atomic serve: {}; {}",
                        cause(&this.run, &message),
                        hint(&this.run)
                    );
                }
                std::panic::resume_unwind(payload)
            }
        }
    }
}
