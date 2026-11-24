// SPDX-License-Identifier: Apache-2.0

//! The bounded Astro parser/interpreter worker and immutable source snapshots.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

// Identify the native frontend, its locked dependencies, and the interpreter host.
const COMPILER_HASH: &str = env!("ATOMIC_COMPILER_HASH");

/// An immutable atom and its transitive local component sources.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Program {
    /// Canonical entry file.
    pub file: PathBuf,
    /// Canonical project root.
    pub root: PathBuf,
    /// Sources captured by the compiler. Rendering never rereads them.
    pub sources: BTreeMap<String, String>,
    /// Resolved full contracts, including constraints erased by the type projection.
    pub contracts: Value,
    /// Canonical paths for explicitly imported source aliases.
    #[serde(default)]
    pub source_paths: BTreeMap<String, String>,
    /// The captured helper module graphs the sources import, keyed by digest.
    /// Helper calls run only these, so a run keeps the helpers it started with.
    #[serde(default)]
    pub helpers: BTreeMap<String, crate::helper_runtime::Captured>,
}

/// A compiler failure, located in the authored source when the compiler knows where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// What went wrong.
    pub message: String,
    /// The authored file, when known.
    pub file: Option<String>,
    /// The one-based authored line, when known.
    pub line: Option<usize>,
    /// The one-based UTF-16 authored column, when known.
    pub column: Option<usize>,
    /// Further located diagnostics, one per line, such as type errors.
    pub details: Vec<String>,
}

impl Diagnostic {
    fn host(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            file: None,
            line: None,
            column: None,
            details: Vec::new(),
        }
    }

    fn from_response(error: &Value) -> Self {
        let Some(object) = error.as_object() else {
            return Self::host(error.as_str().unwrap_or("compiler error").to_owned());
        };
        let text = |value: &Value| value.as_str().map(str::to_owned);
        let details = object
            .get("diagnostics")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        format!(
                            "{}:{}:{}: {}",
                            item["file"].as_str().unwrap_or("types"),
                            item["line"].as_u64().unwrap_or(1),
                            item["column"].as_u64().unwrap_or(1),
                            item["message"].as_str().unwrap_or("error")
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let first = object
            .get("diagnostics")
            .and_then(Value::as_array)
            .and_then(|items| items.first());
        let positive = |value: Option<&Value>| {
            value
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .filter(|value| *value > 0)
        };
        Self {
            message: object
                .get("message")
                .and_then(text)
                .unwrap_or_else(|| "compiler error".into()),
            file: object.get("file").and_then(text),
            line: positive(object.get("line")),
            column: positive(
                object
                    .get("column")
                    .or_else(|| first.and_then(|item| item.get("column"))),
            ),
            details,
        }
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.details.is_empty() {
            return f.write_str(&self.details.join("\n"));
        }
        match (&self.file, self.line, self.column) {
            (Some(file), Some(line), Some(column)) => {
                write!(f, "{file}:{line}:{column}: {}", self.message)
            }
            (Some(file), Some(line), None) => write!(f, "{file}:{line}: {}", self.message),
            (Some(file), None, _) => write!(f, "{file}: {}", self.message),
            _ => f.write_str(&self.message),
        }
    }
}

impl From<String> for Diagnostic {
    fn from(message: String) -> Self {
        Self::host(message)
    }
}

impl From<&str> for Diagnostic {
    fn from(message: &str) -> Self {
        Self::host(message)
    }
}

/// Keep bounded process and filesystem work off the asynchronous executor.
pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    static SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
    let permit = SLOTS.acquire().await.map_err(|e| e.to_string())?;
    // The permit belongs to the blocking task even if its awaiting caller is cancelled.
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .map_err(|e| e.to_string());
    result?
}

fn safe_json(value: &Value, depth: usize, remaining: &mut usize) -> Result<(), String> {
    if depth > 64 || *remaining == 0 {
        return Err("compiler JSON complexity limit exceeded".into());
    }
    *remaining -= 1;
    match value {
        Value::Number(number) => {
            let value = number.as_f64().ok_or("unsupported numeric precision")?;
            if !value.is_finite() || (value.fract() == 0.0 && value.abs() > 9_007_199_254_740_991.0)
            {
                return Err(
                    "Astro numbers require finite values and safe integer precision".into(),
                );
            }
        }
        Value::Array(values) => {
            for value in values {
                safe_json(value, depth + 1, remaining)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                safe_json(value, depth + 1, remaining)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Invoke the shipped compiler in its isolated child process without network,
/// environment, filesystem or subprocess access (see [`crate::compiler_host`]).
pub fn worker(request: Value) -> Result<Value, String> {
    worker_located(request).map_err(|error| error.to_string())
}

/// As [`worker`], keeping where in the authored source a failure is.
pub fn worker_located(request: Value) -> Result<Value, Diagnostic> {
    worker_with(request, &BTreeMap::new(), None)
}

/// The logical time and seed of one durable step, recorded when the step
/// first starts and reused on every retry of it, so the step's helper calls
/// see the same clock and randomness each time. Each call's
/// [`crate::helper_runtime::Clock`] is derived from it, the operation (a
/// render, a validation round) and the call's position in that operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepClock {
    /// Milliseconds since the epoch.
    pub now_ms: i64,
    /// 32 random bytes, in hex.
    pub seed: String,
}

impl StepClock {
    /// The current time and a fresh seed: what a step records, and what an
    /// operation outside a durable step (a preview, a check) uses.
    pub fn fresh() -> Result<Self, String> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis();
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| e.to_string())?;
        Ok(Self {
            now_ms: i64::try_from(now_ms).map_err(|e| e.to_string())?,
            seed: seed.iter().map(|b| format!("{b:02x}")).collect(),
        })
    }

    /// The clock of call `index` within `operation`.
    pub(crate) fn call(&self, operation: &str, index: usize) -> crate::helper_runtime::Clock {
        let mut digest = Sha256::new();
        digest.update(b"atomic-helper-clock-v1\0");
        digest.update(self.seed.as_bytes());
        digest.update([0]);
        digest.update(operation.as_bytes());
        digest.update([0]);
        digest.update((index as u64).to_le_bytes());
        let bytes = digest.finalize();
        let word =
            |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
        crate::helper_runtime::Clock {
            now_ms: self.now_ms as f64,
            seed: [word(0), word(4), word(8), word(12)],
        }
    }
}

/// The helper calls one compiler operation may make, together: a render, a
/// validation or a judge binding. Each call has its own limits as well
/// ([`crate::helper_runtime::Limits`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct HelperBudget {
    /// Calls.
    pub calls: usize,
    /// Time spent in helpers.
    pub time: std::time::Duration,
    /// JSON result bytes.
    pub bytes: usize,
}

impl Default for HelperBudget {
    fn default() -> Self {
        Self {
            calls: 256,
            time: std::time::Duration::from_secs(10),
            bytes: 4 * 1024 * 1024,
        }
    }
}

/// Runs one helper call the worker asked for, against the captures the atom
/// recorded when it loaded, and spends it from `left`.
pub(crate) fn helper_call(
    helpers: &BTreeMap<String, crate::helper_runtime::Captured>,
    left: &mut HelperBudget,
    call: &Value,
    clock: crate::helper_runtime::Clock,
) -> Value {
    use crate::helper_runtime::{Failure, Limits, invoke};
    let failure =
        |kind: &str, message: String| json!({ "error": { "kind": kind, "message": message } });
    let Some(captured) = call["capture"]
        .as_str()
        .and_then(|digest| helpers.get(digest))
    else {
        return failure(
            "refused",
            "the helper is not in the atom's captured helpers".into(),
        );
    };
    let Some(export) = call["export"].as_str() else {
        return failure("refused", "a helper call names its export".into());
    };
    if left.calls == 0 {
        return failure("budget", "helper call limit exceeded".into());
    }
    left.calls -= 1;
    if left.time.is_zero() {
        return failure("budget", "helper time limit exceeded".into());
    }
    let limits = Limits {
        deadline: left.time.min(Limits::default().deadline),
        ..Limits::default()
    };
    let started = std::time::Instant::now();
    let result = invoke(captured, export, &call["args"], limits, clock);
    left.time = left.time.saturating_sub(started.elapsed());
    match result {
        Ok(value) => {
            let size = value.to_string().len();
            if size > left.bytes {
                left.bytes = 0;
                return failure("budget", "helper result limit exceeded".into());
            }
            left.bytes -= size;
            json!({ "ok": value })
        }
        Err(Failure::Threw(text)) => failure("threw", text),
        Err(other) => {
            let kind = match other {
                Failure::Deadline => "deadline",
                Failure::Heap | Failure::ArrayBufferLimit => "memory",
                Failure::OutputLimit | Failure::ErrorLimit => "size",
                Failure::NotJson(_) => "not-json",
                Failure::Refused(_) => "refused",
                Failure::Threw(_) | Failure::Host(_) => "host",
            };
            failure(kind, other.to_string())
        }
    }
}

/// As [`worker_located`], answering the worker's helper calls from `helpers`
/// with clocks derived from `clock` and the operation; without one, the
/// operation's calls share the current time and a fresh seed.
pub(crate) fn worker_with(
    mut request: Value,
    helpers: &BTreeMap<String, crate::helper_runtime::Captured>,
    clock: Option<(&StepClock, &str)>,
) -> Result<Value, Diagnostic> {
    safe_json(&request, 0, &mut 100_000)?;
    let root = request
        .get("root")
        .and_then(Value::as_str)
        .ok_or("missing compiler root")?;
    // The compiler reads no files (see `compiler_host`): atom sources arrive
    // as bounded messages, so even a standalone atom in the home directory
    // grants it no read access.
    if request.get("sources").is_none() {
        if request["action"] != "load" {
            return Err("compiler operation requires frozen sources".into());
        }
        let root = PathBuf::from(root);
        let mut sources = BTreeMap::<String, String>::new();
        let mut source_paths = BTreeMap::<String, String>::new();
        let mut targets = vec![
            request["file"]
                .as_str()
                .ok_or("missing compiler file")?
                .to_owned(),
        ];
        let mut total = 0;
        // Requested files that do not exist; the compiler reports an import of
        // one where the import is.
        let mut absent = std::collections::BTreeSet::<String>::new();
        // Capture all missing siblings together; one worker per import depth.
        for _ in 0..33 {
            for target in targets {
                let canonical = match std::fs::canonicalize(&target) {
                    Ok(canonical) => canonical,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound && !sources.is_empty() => {
                        absent.insert(target);
                        continue;
                    }
                    Err(e) => return Err(format!("read component {target}: {e}").into()),
                };
                let extension = canonical.extension().unwrap_or_default();
                let helper = extension == "ts" && !canonical.to_string_lossy().ends_with(".d.ts");
                // A literal Markdown import's file: inert authored prompt text,
                // captured under the same root and size limits as a component
                // or a helper. It is never parsed, and nothing runs it.
                let literal = extension == "md";
                if !canonical.starts_with(&root) || !(helper || literal || extension == "astro") {
                    return Err("an imported source must be an Astro component, a .ts helper or a literal Markdown file inside the project root".into());
                }
                let canonical_name = canonical
                    .to_str()
                    .ok_or("source path must be UTF-8")?
                    .to_owned();
                source_paths.insert(target.clone(), canonical_name.clone());
                if let std::collections::btree_map::Entry::Vacant(entry) =
                    sources.entry(canonical_name)
                {
                    let mut bytes = Vec::new();
                    std::fs::File::open(&canonical)
                        .map_err(|e| e.to_string())?
                        .take(262_145)
                        .read_to_end(&mut bytes)
                        .map_err(|e| e.to_string())?;
                    let source = String::from_utf8(bytes).map_err(|e| e.to_string())?;
                    total += source.len();
                    if source.len() > 262_144 || total > 1_048_576 {
                        return Err("source size limit exceeded".into());
                    }
                    entry.insert(source);
                }
                if sources.len() > 128 || source_paths.len() > 512 || absent.len() > 128 {
                    return Err("component graph limit exceeded".into());
                }
            }
            request["sources"] = json!(sources);
            request["sourcePaths"] = json!(source_paths);
            request["absent"] = json!(absent);
            let result = worker_with(request.clone(), helpers, clock)?;
            if let Some(next) = result.get("needsSources") {
                targets = serde_json::from_value::<Vec<String>>(next.clone())
                    .map_err(|_| "invalid source discovery response")?;
                if targets.is_empty() || targets.len() > 128 {
                    return Err("invalid source discovery response".into());
                }
            } else {
                return Ok(result);
            }
        }
        return Err("component graph limit exceeded".into());
    }
    request["protocol"] = json!(1);
    let bytes = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
    if bytes.len() > crate::compiler_process::MESSAGE_LIMIT {
        return Err("compiler request limit exceeded".into());
    }
    let mut left = HelperBudget::default();
    let fresh;
    let (step_clock, operation) = match clock {
        Some(clock) => clock,
        None => {
            fresh = StepClock::fresh()?;
            (&fresh, request["action"].as_str().unwrap_or("operation"))
        }
    };
    let operation = operation.to_owned();
    let mut index = 0usize;
    let command = crate::compiler_host::command()?;
    let output = crate::compiler_process::run_interactive(
        &command.program,
        &command.args,
        &command.env,
        &bytes,
        crate::compiler_process::TIME_LIMIT,
        &mut |call: &Value| {
            index += 1;
            helper_call(
                helpers,
                &mut left,
                call,
                step_clock.call(&operation, index - 1),
            )
        },
    )?;
    let response: Value =
        serde_json::from_slice(&output).map_err(|e| format!("invalid compiler response: {e}"))?;
    if let Some(error) = response.get("error") {
        return Err(Diagnostic::from_response(error));
    }
    response
        .get("ok")
        .cloned()
        .ok_or_else(|| "missing compiler result".into())
}

/// A path as the compiler sees it. A path that is not UTF-8 is an error, not a panic.
pub(crate) fn path_text(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()))
}

impl Program {
    /// Run an operation exclusively against this source snapshot.
    pub fn invoke(&self, action: &str, args: Value) -> Result<Value, String> {
        self.invoke_located(action, args)
            .map_err(|error| error.to_string())
    }

    /// As [`Program::invoke`], keeping where in the authored source a failure is.
    pub fn invoke_located(&self, action: &str, args: Value) -> Result<Value, Diagnostic> {
        self.invoke_clocked(action, args, None)
    }

    /// As [`Program::invoke_located`], with helper calls clocked by a durable
    /// step's record and named `operation` within it (a render, one round of
    /// validation), so a retried step sees the same values.
    pub fn invoke_clocked(
        &self,
        action: &str,
        mut args: Value,
        clock: Option<(&StepClock, &str)>,
    ) -> Result<Value, Diagnostic> {
        args["action"] = json!(action);
        args["file"] = json!(path_text(&self.file)?);
        args["root"] = json!(path_text(&self.root)?);
        args["sources"] = json!(self.sources);
        args["sourcePaths"] = json!(self.source_paths);
        // Whether this atom's compiled contract declares a public context, which
        // only the host can say. The worker's own look at the source sees a
        // context in the fifth `AtomConfig` type argument or a `context` prop
        // bound in a template; it cannot see one declared through the per-atom
        // `Config` alias, nor one the sidecar beside the atom declares. Every
        // action this program invokes is told, so its render, validation and
        // judge calls can refuse a missing snapshot as an operation error rather
        // than let a callback read a missing value and return a repairable rejection.
        args["contextDeclared"] = json!(self.contracts.get("Context").is_some());
        worker_with(args, &self.helpers, clock)
    }

    /// Invoke on the bounded blocking pool rather than an executor thread.
    pub async fn invoke_async(&self, action: &str, args: Value) -> Result<Value, String> {
        let program = self.clone();
        let action = action.to_owned();
        blocking(move || program.invoke(&action, args)).await
    }

    /// Checks that every captured helper still has the digest it is recorded
    /// under, so a damaged stored program fails instead of running code the
    /// atom never captured.
    pub fn verify_helpers(&self) -> Result<(), String> {
        for (digest, captured) in &self.helpers {
            if captured.digest() != *digest {
                return Err(format!(
                    "a captured helper of {} does not match its digest {digest}: the stored atom is damaged",
                    self.file.display()
                ));
            }
        }
        Ok(())
    }

    /// What this program's helpers depend on besides their code: the helper
    /// runtime and the compiler that interprets the atom. A run records it
    /// when it starts; see [`crate::helper_runtime::identity`].
    pub fn helper_runtime(&self) -> String {
        Self::current_helper_runtime()
    }

    /// [`Program::helper_runtime`] for this build. Debug builds honour
    /// `ATOMIC_TEST_HELPER_RUNTIME`, so tests can stand in for another
    /// release; release builds ignore it.
    pub fn current_helper_runtime() -> String {
        if cfg!(debug_assertions)
            && let Some(identity) = std::env::var_os("ATOMIC_TEST_HELPER_RUNTIME")
        {
            return identity.to_string_lossy().into_owned();
        }
        let mut digest = Sha256::new();
        digest.update(crate::helper_runtime::identity().as_bytes());
        digest.update([0]);
        digest.update(COMPILER_HASH.as_bytes());
        format!("{:x}", digest.finalize())
    }

    /// A stable fingerprint of the entry and all component bytes.
    pub fn hash(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(COMPILER_HASH.as_bytes());
        digest.update(self.contracts.to_string().as_bytes());
        digest.update(serde_json::to_vec(&self.source_paths).unwrap_or_default());
        digest.update(serde_json::to_vec(&self.helpers).unwrap_or_default());
        for (name, source) in &self.sources {
            digest.update(name.as_bytes());
            digest.update([0]);
            digest.update(source.as_bytes());
            digest.update([0]);
        }
        format!("{:x}", digest.finalize())
    }
}

/// Resolve the containing project; standalone atoms are rooted at their directory.
pub fn project_root(file: &Path) -> Result<PathBuf, String> {
    let parent = file.parent().unwrap_or(Path::new("/"));
    match crate::project::find_manifest(parent) {
        Ok(manifest) => {
            let directory = manifest.parent().ok_or("manifest has no directory")?;
            Ok(
                if directory.file_name().is_some_and(|name| name == ".atomic") {
                    directory.parent().unwrap_or(directory).to_owned()
                } else {
                    directory.to_owned()
                },
            )
        }
        Err(crate::project::ProjectError::NotFound(_)) => Ok(parent.to_owned()),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_worker_leaves_the_executor_responsive() {
        let (started, running) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let worker = tokio::spawn(blocking(move || {
            started.send(()).map_err(|_| "start receiver dropped")?;
            wait.recv_timeout(std::time::Duration::from_secs(2))
                .map_err(|e| e.to_string())?;
            Ok(())
        }));
        running.await.expect("worker started");
        // This task must run while the worker waits. Blocking the executor
        // makes the worker time out and fail instead of releasing it.
        tokio::task::yield_now().await;
        release.send(()).expect("worker still waiting");
        worker.await.expect("task").expect("worker");
    }

    #[test]
    fn the_compiler_is_atomic_itself_with_no_environment() {
        let command = crate::compiler_host::command().expect("command");
        assert_eq!(command.program, std::env::current_exe().expect("exe"));
        assert_eq!(command.args, [crate::compiler_host::CHILD_ARG]);
        assert!(command.env.is_empty(), "{:?}", command.env);
    }

    #[test]
    fn requests_beyond_the_json_limits_are_refused() {
        let mut deep = json!(1);
        for _ in 0..70 {
            deep = json!([deep]);
        }
        assert!(safe_json(&deep, 0, &mut 100_000).is_err());
        let wide = json!(vec![0; 200]);
        assert!(safe_json(&wide, 0, &mut 100).is_err());
        assert!(safe_json(&json!(9_007_199_254_740_993_u64), 0, &mut 10).is_err());
        assert!(safe_json(&json!({"fine": [1, 2.5, "x"]}), 0, &mut 10).is_ok());
    }

    #[test]
    fn an_oversized_component_is_refused_before_the_compiler_runs() {
        let dir = std::env::temp_dir().join(format!("atomic-astro-size-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        // The root is canonical, as a project's is: on macOS the temporary
        // directory is reached through a symbolic link.
        let dir = std::fs::canonicalize(dir).expect("canonical");
        let file = dir.join("big.atom.astro");
        std::fs::write(&file, "x".repeat(262_145)).expect("big source");
        let error = worker(json!({"action": "load", "file": path_text(&file).expect("utf-8"), "root": path_text(&dir).expect("utf-8")}))
            .expect_err("too big");
        assert!(error.contains("source size limit"), "{error}");
        let outside =
            std::env::temp_dir().join(format!("atomic-astro-outside-{}.astro", std::process::id()));
        std::fs::write(&outside, "Outside").expect("outside");
        let error = worker(json!({"action": "load", "file": path_text(&outside).expect("utf-8"), "root": path_text(&dir).expect("utf-8")}))
            .expect_err("outside the root");
        assert!(error.contains("inside the project root"), "{error}");
        let _ = std::fs::remove_file(outside);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_utf8_is_an_error_not_a_panic() {
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/\xff.atom.astro"));
        assert!(path_text(path).is_err());
    }

    #[test]
    fn compiler_errors_keep_their_file_and_line() {
        let error = Diagnostic::from_response(
            &json!({"message": "unknown value x", "file": "/p/a.atom.astro", "line": 7}),
        );
        assert_eq!(error.to_string(), "/p/a.atom.astro:7: unknown value x");
        assert_eq!(error.column, None);
        let types = Diagnostic::from_response(&json!({
            "message": "TS2339: no",
            "file": "/p/a.atom.astro",
            "line": 3,
            "diagnostics": [
                {"file": "/p/a.atom.astro", "line": 3, "column": 5, "message": "TS2339: no"},
                {"file": "/p/b.astro", "line": 1, "column": 2, "message": "TS2322: also"}
            ]
        }));
        assert_eq!(
            types.to_string(),
            "/p/a.atom.astro:3:5: TS2339: no\n/p/b.astro:1:2: TS2322: also"
        );
        assert_eq!(types.column, Some(5));
    }
}
