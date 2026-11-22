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

/// The redacted message of the panic run `run`'s workflow code raised in
/// this process, if it did: what `atomic run` and `atomic runs resume`
/// report as the cause of the run they followed.
pub fn noted(run: &str) -> Option<String> {
    NOTED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(run)
        .cloned()
}

/// [`noted`], or else the panic kept in the project's private state by an
/// earlier process: what `atomic serve` names when it starts, and what
/// `atomic runs resume` warns of before it resumes.
pub fn of(run: &str) -> Option<String> {
    noted(run).or_else(|| KEPT_IN.get().and_then(|dir| kept(dir, run)))
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
/// it was resumed, or it finished.
pub fn forget(run: &str) {
    forget_in(KEPT_IN.get().map(PathBuf::as_path), run);
}

/// [`forget`], with the panics kept under `dir`, if anywhere.
fn forget_in(dir: Option<&Path>, run: &str) {
    NOTED.lock().unwrap_or_else(|e| e.into_inner()).remove(run);
    if let Some(dir) = dir {
        let _ = std::fs::remove_file(file(dir, run));
    }
}

/// The message a panic carries, with where it happened when the panic hook
/// saw it, bounded and then redacted.
fn message(payload: &(dyn std::any::Any + Send)) -> String {
    let raw = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic with no message".to_owned());
    let raw = match atomic_core::redact::take_panic_location() {
        Some(at) => format!("{raw}, at {at}"),
        None => raw,
    };
    // Bounded, then redacted within the bound, so a huge message costs no
    // more to redact than a short one.
    atomic_core::redact::bounded(&raw, 2_000)
}

/// Keep `kept` as run `run`'s note under `dir`: written whole to a file of
/// its own, readable by its owner only, synced, then renamed into place.
fn keep(dir: &Path, run: &str, kept: &serde_json::Value) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{}-{n}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut f = options.open(&tmp)?;
    f.write_all(kept.to_string().as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, file(dir, run))?;
    std::fs::File::open(dir)?.sync_all()
}

/// Note that run `run`'s workflow code panicked with `payload`.
fn note(run: &str, payload: &(dyn std::any::Any + Send)) -> String {
    note_in(KEPT_IN.get().map(PathBuf::as_path), run, payload)
}

/// [`note`], keeping it under `dir`, if anywhere.
fn note_in(dir: Option<&Path>, run: &str, payload: &(dyn std::any::Any + Send)) -> String {
    let message = message(payload);
    NOTED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(run.to_owned(), message.clone());
    if let Some(dir) = dir {
        let kept = serde_json::json!({
            "run": run,
            "message": message,
            "at": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        });
        // The run is parked either way, and this process says why; a later
        // one cannot, so say that too.
        if let Err(e) = keep(dir, run, &kept) {
            atomic_core::redact::Line::new()
                .plain("atomic: run ")
                .shown(run)
                .plain("'s panic is not kept for later processes: ")
                .redacted(&e.to_string())
                .log();
        }
    }
    message
}

/// What a person is told about run `run`, parked because its workflow code
/// panicked with `message`.
pub fn cause(run: &str, message: &str) -> String {
    format!("run {run}{PANICKED}{message}{NOT_RETRIED}")
}

/// What [`cause`] and [`hint`] say around the run's id and the panic's
/// message.
const PANICKED: &str = " stopped: atomic's own workflow code panicked (";
const NOT_RETRIED: &str = "), so the run is parked, not retried";
const HINT: &str = "this is a bug in atomic, and the panic will likely happen again if the run \
                    is resumed as it is: report it with the message above, then continue the \
                    run with `atomic runs resume ";
const HINT_END: &str = "` once an atomic without it is installed";

/// What `atomic serve` says of run `run`, whose workflow code panicked with
/// `message`: [`cause`] and [`hint`], with the run's id as it is and the
/// message redacted on its own, so a run named for a secret, such as
/// `rotate-secret`, is still named.
pub fn served(run: &str, message: &str) -> atomic_core::redact::Line {
    atomic_core::redact::Line::new()
        .plain("atomic serve: run ")
        .shown(run)
        .plain(PANICKED)
        .redacted(message)
        .plain(NOT_RETRIED)
        .plain("; ")
        .plain(HINT)
        .shown(run)
        .plain(HINT_END)
}

/// What a person is told about run `run`, whose workflow code panicked with
/// `message` when this command could not see the engine park it.
pub fn unparked(run: &str, message: &str) -> String {
    format!(
        "run {run} stopped: atomic's own workflow code panicked ({message}), and the engine has \
         not parked it yet, so a later `atomic serve` may run it again"
    )
}

/// What a person can do about it.
pub fn hint(run: &str) -> String {
    format!("{HINT}{run}{HINT_END}")
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
        // Only a location recorded during this poll is this panic's.
        let _ = atomic_core::redact::take_panic_location();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            this.workflow.as_mut().poll(cx)
        })) {
            // The run's workflow ended without a panic: any panic noted
            // for it before is behind it.
            Ok(Poll::Ready(output)) => {
                forget(&this.run);
                Poll::Ready(output)
            }
            Ok(Poll::Pending) => Poll::Pending,
            Err(payload) => {
                let message = note(&this.run, payload.as_ref());
                if crate::workflows::serving() {
                    served(&this.run, &message).log();
                    // Said; a server keeps no note of every run it served.
                    NOTED
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&this.run);
                }
                std::panic::resume_unwind(payload)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("atomic-panics-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_noted_panic_is_redacted_kept_privately_and_forgotten() {
        let dir = scratch("noted");
        let key = concat!("ghp_", "abcdefghijklmnopqrstuvwxyz0123456789").to_owned();
        let payload: Box<dyn std::any::Any + Send> = Box::new(format!("the key {key} leaked"));
        let message = note_in(Some(&dir), "noted-run", payload.as_ref());
        assert!(!message.contains(&key), "{message}");
        assert_eq!(noted("noted-run").as_deref(), Some(message.as_str()));
        assert_eq!(kept(&dir, "noted-run").as_deref(), Some(message.as_str()));
        let text = std::fs::read_to_string(file(&dir, "noted-run")).expect("kept");
        assert!(!text.contains(&key), "{text}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(file(&dir, "noted-run"))
                .expect("kept")
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "{mode:o}");
        }
        // Another run's note under the same name is not this run's.
        assert_eq!(kept(&dir, "another-run"), None);
        forget_in(Some(&dir), "noted-run");
        assert_eq!(noted("noted-run"), None);
        assert_eq!(kept(&dir, "noted-run"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_long_message_is_bounded_and_redacted() {
        let key = concat!("sk-", "abcdefghijklmnopqrstuvwxyz0123456789ABCD").to_owned();
        let raw = format!("{key} {}", "x".repeat(1_000_000));
        let payload: Box<dyn std::any::Any + Send> = Box::new(raw);
        let message = message(payload.as_ref());
        assert!(message.chars().count() <= 2_100, "{}", message.len());
        assert!(!message.contains(&key), "{message}");
    }
}
