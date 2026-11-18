// SPDX-License-Identifier: Apache-2.0

//! The bounded Astro parser/interpreter worker and immutable source snapshots.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

const WORKER: &[u8] = include_bytes!("../assets/worker.mjs");
const WASM: &[u8] = include_bytes!("../assets/astro.wasm");
#[cfg(test)]
static ASSET_READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static COMPILER_HASH: LazyLock<String> = LazyLock::new(|| {
    let mut digest = Sha256::new();
    digest.update(WORKER);
    digest.update(WASM);
    format!("{:x}", digest.finalize())
});

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
    /// Further located diagnostics, one per line, such as type errors.
    pub details: Vec<String>,
}

impl Diagnostic {
    fn host(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            file: None,
            line: None,
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
        Self {
            message: object
                .get("message")
                .and_then(text)
                .unwrap_or_else(|| "compiler error".into()),
            file: object.get("file").and_then(text),
            line: object
                .get("line")
                .and_then(Value::as_u64)
                .and_then(|line| usize::try_from(line).ok())
                .filter(|line| *line > 0),
            details,
        }
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.details.is_empty() {
            return f.write_str(&self.details.join("\n"));
        }
        match (&self.file, self.line) {
            (Some(file), Some(line)) => write!(f, "{file}:{line}: {}", self.message),
            (Some(file), None) => write!(f, "{file}: {}", self.message),
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

/// Where the compiler assets are unpacked: under `ATOMIC_HOME` when it is set,
/// as runtime state is, and otherwise in the user's cache directory.
fn cache_dir() -> Result<PathBuf, String> {
    let base = match std::env::var_os("ATOMIC_HOME").filter(|value| !value.is_empty()) {
        Some(home) => PathBuf::from(home).join("cache"),
        None => dirs::cache_dir()
            .ok_or("no user cache directory for the Astro compiler; set ATOMIC_HOME")?
            .join("atomic"),
    };
    Ok(base.join("compiler").join(COMPILER_HASH.as_str()))
}

fn assets() -> Result<PathBuf, String> {
    let dir = cache_dir()?;
    static INSTALLED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    let mut installed = INSTALLED
        .lock()
        .map_err(|_| "compiler cache lock poisoned")?;
    if installed.contains(&dir) {
        return Ok(dir);
    }
    std::fs::create_dir_all(&dir).map_err(|e| {
        format!(
            "cannot create the compiler cache {}: {e}; set ATOMIC_HOME",
            dir.display()
        )
    })?;
    for (name, content) in [("worker.mjs", WORKER), ("astro.wasm", WASM)] {
        let file = dir.join(name);
        #[cfg(test)]
        ASSET_READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if std::fs::read(&file).ok().as_deref() != Some(content) {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let temp = dir.join(format!(
                "{name}.{}-{}.tmp",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::write(&temp, content).map_err(|e| e.to_string())?;
            std::fs::rename(temp, file).map_err(|e| e.to_string())?;
        }
    }
    installed.push(dir.clone());
    Ok(dir)
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

/// The Deno that runs the compiler: `ATOMIC_DENO`, else `deno` on `PATH`.
fn deno() -> PathBuf {
    std::env::var_os("ATOMIC_DENO")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("PATH")
                .and_then(|paths| {
                    std::env::split_paths(&paths)
                        .map(|dir| dir.join("deno"))
                        .find(|file| file.is_file())
                })
                .unwrap_or_else(|| PathBuf::from("deno"))
        })
}

/// The Deno arguments: no network, npm, config, environment, write, FFI or
/// subprocess access, and reads limited to the compiler assets.
fn deno_args(assets: &Path) -> Vec<String> {
    vec![
        "run".into(),
        "--no-config".into(),
        "--no-lock".into(),
        "--no-remote".into(),
        "--cached-only".into(),
        "--no-npm".into(),
        "--no-prompt".into(),
        format!("--allow-read={}", assets.display()),
        assets.join("worker.mjs").display().to_string(),
    ]
}

/// Invoke the shipped compiler without network, environment, write, FFI or subprocess access.
pub fn worker(request: Value) -> Result<Value, String> {
    worker_located(request).map_err(|error| error.to_string())
}

/// As [`worker`], keeping where in the authored source a failure is.
pub fn worker_located(mut request: Value) -> Result<Value, Diagnostic> {
    safe_json(&request, 0, &mut 100_000)?;
    let dir = assets()?;
    let root = request
        .get("root")
        .and_then(Value::as_str)
        .ok_or("missing compiler root")?;
    // Only the trusted compiler assets are readable by Deno (see `deno_args`).
    // Atom sources arrive as bounded messages, so even a standalone atom in
    // the home directory grants no read access to it.
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
        // Capture all missing siblings together; one worker per import depth.
        for _ in 0..33 {
            for target in targets {
                let canonical = std::fs::canonicalize(&target)
                    .map_err(|e| format!("read component {target}: {e}"))?;
                if !canonical.starts_with(&root)
                    || canonical
                        .extension()
                        .is_none_or(|extension| extension != "astro")
                {
                    return Err("component must be an Astro source inside the project root".into());
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
                if sources.len() > 128 || source_paths.len() > 512 {
                    return Err("component graph limit exceeded".into());
                }
            }
            request["sources"] = json!(sources);
            request["sourcePaths"] = json!(source_paths);
            let result = worker_located(request.clone())?;
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
    let output = crate::compiler_process::run(
        &deno(),
        &deno_args(&dir),
        &[
            (
                "DENO_DIR".into(),
                dir.join("deno-cache").display().to_string(),
            ),
            ("DENO_NO_UPDATE_CHECK".into(), "1".into()),
        ],
        &bytes,
        crate::compiler_process::TIME_LIMIT,
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
    pub fn invoke_located(&self, action: &str, mut args: Value) -> Result<Value, Diagnostic> {
        args["action"] = json!(action);
        args["file"] = json!(path_text(&self.file)?);
        args["root"] = json!(path_text(&self.root)?);
        args["sources"] = json!(self.sources);
        args["sourcePaths"] = json!(self.source_paths);
        worker_located(args)
    }

    /// Invoke on the bounded blocking pool rather than an executor thread.
    pub async fn invoke_async(&self, action: &str, args: Value) -> Result<Value, String> {
        let program = self.clone();
        let action = action.to_owned();
        blocking(move || program.invoke(&action, args)).await
    }

    /// A stable fingerprint of the entry and all component bytes.
    pub fn hash(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(COMPILER_HASH.as_bytes());
        digest.update(self.contracts.to_string().as_bytes());
        digest.update(serde_json::to_vec(&self.source_paths).unwrap_or_default());
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
    fn compiler_assets_are_installed_once_per_process() {
        let dir = assets().expect("install");
        let first = ASSET_READS.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(assets().expect("reuse"), dir);
        assert_eq!(
            first,
            ASSET_READS.load(std::sync::atomic::Ordering::Relaxed),
            "cached assets must not be reread"
        );
    }

    #[test]
    fn deno_may_read_only_the_compiler_assets() {
        let dir = Path::new("/cache/atomic/compiler/abc");
        let args = deno_args(dir);
        let grants: Vec<_> = args
            .iter()
            .filter(|arg| arg.starts_with("--allow"))
            .collect();
        assert_eq!(grants, ["--allow-read=/cache/atomic/compiler/abc"]);
        for flag in [
            "--no-remote",
            "--no-npm",
            "--no-config",
            "--no-prompt",
            "--cached-only",
        ] {
            assert!(args.iter().any(|arg| arg == flag), "{flag} is missing");
        }
        assert!(
            !args
                .iter()
                .any(|arg| arg == "-A" || arg.starts_with("--unstable"))
        );
        assert_eq!(
            args.last().map(String::as_str),
            Some("/cache/atomic/compiler/abc/worker.mjs")
        );
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
    }
}
