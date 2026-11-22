// SPDX-License-Identifier: Apache-2.0

//! The Astro compiler's child process: the bundled worker, run on the V8
//! that atomic embeds through `deno_core`, with no Deno installation.
//!
//! atomic starts itself with [`CHILD_ARG`] and a cleared environment (see
//! `astro::worker_with` and `compiler_process`, which keep the time limit,
//! the message limits and the kill). The child runs the worker, embedded in
//! the binary with its WASM, under a V8 heap limit, on a host that gives it
//! the protocol's stdin and stdout, stderr for its console, its exit status,
//! its own WASM, random bytes, SHA-256 and timers, and nothing else: no
//! network, environment, process or filesystem (`compiler_host/host.js`).
//! A worker that fails to start, exceeds a limit, or calls what the host does
//! not provide fails its compile; nothing falls back to an external Deno.

use deno_core::{
    JsRuntime, ModuleLoadOptions, ModuleLoadReferrer, ModuleLoadResponse, ModuleLoader,
    ModuleSource, ModuleSourceCode, ModuleSpecifier, ModuleType, PollEventLoopOptions,
    ResolutionKind, RuntimeOptions, error::ModuleLoaderError, op2, v8,
};
use std::io::{Read, Write};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

mod limits;
#[cfg(test)]
mod tests;

/// The argument that makes the atomic binary serve one compiler call.
pub const CHILD_ARG: &str = "__atomic-compiler";

// Statics, not consts, so the binary holds one copy of each wherever they
// are used.
/// The bundled worker, built from `authoring/src`.
pub(crate) static WORKER: &str = include_str!("../assets/worker.mjs");
/// The Go-compiled Astro parser the worker loads.
pub(crate) static WASM: &[u8] = include_bytes!("../assets/astro.wasm");
/// What the worker may use of Deno, Node and the web platform.
pub(crate) const HOST: &str = include_str!("compiler_host/host.js");

/// The `deno_core` release the compiler runs on (Cargo.toml pins it).
pub(crate) const DENO_CORE: &str = "deno_core 0.413.0";

/// The V8 heap one compile may use.
pub(crate) const HEAP_LIMIT: usize = 1024 * 1024 * 1024;

/// Where the worker is loaded from; its WASM is next to it.
const MAIN: &str = "atomic-compiler:///worker.mjs";
const WASM_URL: &str = "atomic-compiler:///astro.wasm";
/// The Node modules the worker imports, served from the host.
const NODE_MODULES: [&str; 4] = ["fs/promises", "path", "crypto", "url"];

/// The command that starts a compiler child: this atomic binary, with
/// [`CHILD_ARG`] and no environment.
pub(crate) fn command() -> Result<crate::helper_runtime::HostCommand, String> {
    Ok(crate::helper_runtime::HostCommand {
        program: std::env::current_exe()
            .map_err(|e| format!("cannot find the atomic binary to run the Astro compiler: {e}"))?,
        args: vec![CHILD_ARG.to_owned()],
        env: Vec::new(),
    })
}

/// Serves one compiler call on stdin and stdout and returns the exit status:
/// the worker's own, 0 when it finishes, and 1 with the reason on stderr
/// when it fails to start, throws or exceeds the heap limit.
pub fn serve() -> i32 {
    serve_module(WORKER, HEAP_LIMIT)
}

fn serve_module(source: &str, heap_limit: usize) -> i32 {
    let status = match limits::limit_self(limits::memory_limit(heap_limit))
        .and_then(|()| run(source, heap_limit))
    {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("{message}");
            1
        }
    };
    let _ = std::io::stdout().flush();
    status
}

// A test binary that links atomic-core serves compiler calls the way the
// atomic binary's `main` does, so tests that compile atoms in-process start
// the same child. It runs before libtest's `main`, which would read the
// argument as a test filter and print to stdout. Never in a release build:
// only in atomic-core's own tests and with the dev-only feature.
#[cfg(any(test, feature = "compiler-in-tests"))]
#[allow(unsafe_code)]
mod test_dispatch {
    use super::*;

    /// release-package.sh refuses a binary holding this text. It is split
    /// here because this file's own text is in every binary (`COMPILER_HASH`).
    static TEST_DISPATCH: &str = concat!("ATOMIC_COMPILER_", "TEST_DISPATCH");

    #[ctor::ctor(unsafe)]
    fn serve_in_a_test_binary() {
        if std::env::args_os().nth(1).as_deref() == Some(CHILD_ARG.as_ref()) {
            std::hint::black_box(TEST_DISPATCH);
            #[cfg(test)]
            if let Some(probe) = std::env::var_os(tests::PROBE_ENV) {
                let heap = std::env::var(tests::HEAP_ENV)
                    .ok()
                    .and_then(|heap| heap.parse().ok())
                    .unwrap_or(HEAP_LIMIT);
                std::process::exit(serve_module(&probe.to_string_lossy(), heap));
            }
            std::process::exit(serve());
        }
    }
}

struct Loader;

fn node_module(specifier: &str) -> Option<&'static str> {
    let name = specifier.strip_prefix("node:")?;
    NODE_MODULES.into_iter().find(|module| *module == name)
}

impl ModuleLoader for Loader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, ModuleLoaderError> {
        if matches!(kind, ResolutionKind::DynamicImport) {
            return Err(ModuleLoaderError::generic(format!(
                "the Astro compiler imports nothing dynamically, not {specifier}"
            )));
        }
        if let Some(name) = node_module(specifier) {
            return ModuleSpecifier::parse(&format!("atomic-compiler:///node/{name}"))
                .map_err(ModuleLoaderError::from_err);
        }
        let url =
            deno_core::resolve_import(specifier, referrer).map_err(ModuleLoaderError::from_err)?;
        if url.as_str() != MAIN {
            return Err(ModuleLoaderError::generic(format!(
                "the Astro compiler imports only its Node modules, not {specifier}"
            )));
        }
        Ok(url)
    }

    fn load(
        &self,
        url: &ModuleSpecifier,
        _: Option<&ModuleLoadReferrer>,
        _: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        // Only a Node module is loaded here; the worker itself is loaded
        // from its embedded source.
        let name = url.as_str().strip_prefix("atomic-compiler:///node/");
        let Some(name) = name.and_then(|name| NODE_MODULES.into_iter().find(|m| *m == name)) else {
            return ModuleLoadResponse::Sync(Err(ModuleLoaderError::generic(format!(
                "the Astro compiler has no module {url}"
            ))));
        };
        ModuleLoadResponse::Sync(Ok(ModuleSource::new(
            ModuleType::JavaScript,
            ModuleSourceCode::String(node_source(name).into()),
            url,
            None,
        )))
    }
}

/// A Node module as the worker imports it: the default export, and each
/// member as a named export.
fn node_source(name: &str) -> String {
    let members: &[&str] = match name {
        "fs/promises" => &[
            "readFile",
            "realpath",
            "writeFile",
            "mkdir",
            "readdir",
            "rm",
            "open",
            "link",
            "stat",
            "lstat",
            "access",
        ],
        "path" => &[
            "sep",
            "delimiter",
            "isAbsolute",
            "resolve",
            "normalize",
            "join",
            "relative",
            "dirname",
            "basename",
            "extname",
            "posix",
        ],
        "crypto" => &["createHash"],
        _ => &["fileURLToPath"],
    };
    let mut source = format!(
        "const module = globalThis.__atomicCompilerNode[{}];\nexport default module;\n",
        serde_json::Value::from(name)
    );
    for member in members {
        source.push_str(&format!("export const {member} = module.{member};\n"));
    }
    source
}

/// Reads the protocol's stdin into `buffer`: the count, -1 at its end, -2 on
/// an error.
#[op2(fast)]
fn op_compiler_read(#[buffer] buffer: &mut [u8]) -> i32 {
    match std::io::stdin().lock().read(buffer) {
        Ok(0) if !buffer.is_empty() => -1,
        Ok(n) => i32::try_from(n).unwrap_or(-2),
        Err(_) => -2,
    }
}

/// Writes to the protocol's stdout and flushes: the count, or -1 on an error.
#[op2(fast)]
fn op_compiler_write(#[buffer] buffer: &[u8]) -> i32 {
    let mut stdout = std::io::stdout().lock();
    match stdout
        .write(buffer)
        .and_then(|n| stdout.flush().map(|()| n))
    {
        Ok(n) => i32::try_from(n).unwrap_or(-1),
        Err(_) => -1,
    }
}

/// The embedded WASM, the one file the worker reads.
#[op2]
#[buffer]
fn op_compiler_wasm() -> Vec<u8> {
    WASM.to_vec()
}

/// Ends the process with the worker's status, its output flushed.
#[op2(fast)]
fn op_compiler_exit(code: i32) {
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

/// Fills `buffer` with random bytes; the process ends if the system has none.
#[op2(fast)]
fn op_compiler_random(#[buffer] buffer: &mut [u8]) {
    if getrandom::fill(buffer).is_err() {
        eprintln!("the Astro compiler cannot read random bytes");
        std::process::exit(1);
    }
}

/// The SHA-256 of `data`, in hex.
#[op2]
#[string]
fn op_compiler_sha256(#[buffer] data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(data))
}

fn extension() -> deno_core::Extension {
    deno_core::Extension {
        name: "atomic_compiler",
        ops: std::borrow::Cow::Owned(vec![
            op_compiler_read(),
            op_compiler_write(),
            op_compiler_wasm(),
            op_compiler_exit(),
            op_compiler_random(),
            op_compiler_sha256(),
        ]),
        ..Default::default()
    }
}

fn run(source: &str, heap_limit: usize) -> Result<(), String> {
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(|e| format!("cannot start the Astro compiler: {e}"))?;
    let _entered = tokio.enter();
    let mut runtime = JsRuntime::new(RuntimeOptions {
        module_loader: Some(Rc::new(Loader)),
        extensions: vec![extension()],
        create_params: Some(v8::CreateParams::default().heap_limits(0, heap_limit)),
        ..Default::default()
    });
    // At the heap limit the compile is ended, given room to unwind rather
    // than abort, and reported as the limit it hit.
    let exhausted = Arc::new(AtomicBool::new(false));
    let handle = runtime.v8_isolate().thread_safe_handle();
    let flag = exhausted.clone();
    runtime.add_near_heap_limit_callback(move |current, initial| {
        flag.store(true, Ordering::Release);
        handle.terminate_execution();
        current
            .saturating_add(initial / 4)
            .min(initial.saturating_mul(2))
            .max(current)
    });
    let failed = |error: &dyn std::fmt::Display| {
        if exhausted.load(Ordering::Acquire) {
            "the Astro compiler exceeded its memory limit".to_owned()
        } else {
            format!("the Astro compiler failed: {error}")
        }
    };
    runtime
        .execute_script(
            "atomic:compiler-host",
            format!("{HOST}({});", serde_json::Value::from(WASM_URL)),
        )
        .map_err(|e| failed(&e))?;
    let main = ModuleSpecifier::parse(MAIN).map_err(|e| failed(&e))?;
    let result = tokio.block_on(async {
        let id = runtime
            .load_main_es_module_from_code(&main, source.to_owned())
            .await?;
        let evaluated = runtime.mod_evaluate(id);
        runtime
            .run_event_loop(PollEventLoopOptions::default())
            .await?;
        evaluated.await
    });
    result.map_err(|e| failed(&e))?;
    if exhausted.load(Ordering::Acquire) {
        return Err(failed(&"terminated"));
    }
    Ok(())
}
