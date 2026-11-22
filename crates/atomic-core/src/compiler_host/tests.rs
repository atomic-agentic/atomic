// SPDX-License-Identifier: Apache-2.0
//! The compiler's host, tried from inside it: each probe is a module run in
//! place of the worker, in the child atomic starts for a compile, with the
//! same host, limits and protocol.
use super::*;
use crate::compiler_process::{TIME_LIMIT, run_interactive};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// The module a child runs instead of the worker; read only in this
/// crate's test binary (see `test_dispatch`).
pub(super) const PROBE_ENV: &str = "ATOMIC_COMPILER_TEST_PROBE";
/// The child's heap limit in bytes, for the heap probe.
pub(super) const HEAP_ENV: &str = "ATOMIC_COMPILER_TEST_HEAP";
/// Shorten only the CPU probe, after checking the production CPU limits.
const CPU_ENV: &str = "ATOMIC_COMPILER_TEST_CPU";

/// A CPU second is not a wall-clock second: a busy test runner can schedule
/// a child too seldom for its production CPU budget to expire before the
/// parent's deadline. Check the real limits first, then exercise their
/// enforcement with a smaller budget in this test binary's probe child.
#[allow(unsafe_code)]
pub(super) fn shorten_cpu_probe() -> Result<(), String> {
    if std::env::var_os(CPU_ENV).is_none() {
        return Ok(());
    }
    let mut current = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `current` is a valid writable rlimit for this process's query.
    if unsafe { libc::getrlimit(libc::RLIMIT_CPU, &mut current) } != 0 {
        return Err(format!(
            "cannot read the compiler CPU probe's limits: {}",
            std::io::Error::last_os_error()
        ));
    }
    if current.rlim_cur != limits::CPU_SECONDS as libc::rlim_t
        || current.rlim_max != (limits::CPU_SECONDS + 1) as libc::rlim_t
    {
        return Err(format!(
            "the compiler CPU probe expected production limits {}/{}, got {}/{}",
            limits::CPU_SECONDS,
            limits::CPU_SECONDS + 1,
            current.rlim_cur,
            current.rlim_max
        ));
    }
    let short = libc::rlimit {
        rlim_cur: 1,
        rlim_max: 2,
    };
    // SAFETY: `short` is valid; this only lowers the probe child's own CPU
    // limits, after the production limits have been checked above.
    if unsafe { libc::setrlimit(libc::RLIMIT_CPU, &short) } != 0 {
        return Err(format!(
            "cannot shorten the compiler CPU probe: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Runs `body`, an async function body, in a compiler child and returns
/// what it returned, or the child's failure.
fn probe_with(body: &str, env: &[(&str, String)], limit: Duration) -> Result<Value, String> {
    // The Node modules the worker imports, imported as it imports them.
    let module = format!(
        "import * as fs from 'node:fs/promises'; import path from 'node:path';\n\
         import {{ createHash }} from 'node:crypto'; import {{ fileURLToPath }} from 'node:url';\n\
         const result = await (async () => {{ {body} }})();\n\
         const line = new TextEncoder().encode(JSON.stringify({{ ok: result }}) + '\\n');\n\
         let written = 0;\n\
         while (written < line.length) written += Deno.stdout.writeSync(line.subarray(written));\n"
    );
    let mut command = command()?;
    command.env.push((PROBE_ENV.to_owned(), module));
    command
        .env
        .extend(env.iter().map(|(k, v)| ((*k).to_owned(), v.clone())));
    let output = run_interactive(
        &command.program,
        &command.args,
        &command.env,
        b"{}",
        limit,
        &mut |_: &Value| panic!("no calls expected"),
    )?;
    let response: Value = serde_json::from_slice(&output).map_err(|e| e.to_string())?;
    Ok(response["ok"].clone())
}

fn probe(body: &str) -> Result<Value, String> {
    probe_with(body, &[], TIME_LIMIT)
}

/// `body` as an expression whose rejection or throw is the answer.
fn refusal(expression: &str) -> String {
    let body = format!(
        "try {{ return {{ allowed: await (async () => {expression})() }}; }} \
         catch (error) {{ return {{ refused: String(error?.message ?? error) }}; }}"
    );
    let value = probe(&body).expect("probe");
    value["refused"]
        .as_str()
        .unwrap_or_else(|| panic!("{expression} was allowed: {value}"))
        .to_owned()
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "atomic-compiler-host-{name}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

#[test]
fn the_worker_runs_in_the_child_and_answers_a_request() {
    let command = command().expect("command");
    let output = run_interactive(
        &command.program,
        &command.args,
        &command.env,
        br#"{"protocol":1,"action":"nothing","file":"/p/a.atom.astro","root":"/p","sources":{}}"#,
        TIME_LIMIT,
        &mut |_: &Value| panic!("no calls expected"),
    )
    .expect("answer");
    let response: Value = serde_json::from_slice(&output).expect("json");
    // The worker started, read its request and refused it in its own words.
    let message = response["error"]["message"].as_str().unwrap_or_default();
    assert!(!message.is_empty(), "{response}");
    assert!(
        !message.contains("requires Deno"),
        "the host is the Deno the worker expects: {message}"
    );
}

#[test]
fn the_worker_reads_its_own_wasm_and_no_other_file() {
    assert_eq!(
        probe("return (await Deno.readFile(new URL('./astro.wasm', import.meta.url))).length;"),
        Ok(json!(WASM.len()))
    );
    let dir = scratch("read");
    let file = dir.join("secret.txt");
    std::fs::write(&file, "do-not-read").expect("file");
    let path = file.to_str().expect("utf-8");
    let refused = refusal(&format!(
        "new TextDecoder().decode(await Deno.readFile({path:?}))"
    ));
    assert!(
        refused.contains("it reads only its own WASM") && refused.contains(path),
        "{refused}"
    );
    let refused = refusal(&format!(
        "new TextDecoder().decode(await Deno.readFile('file://{path}'))"
    ));
    assert!(refused.contains("it reads only its own WASM"), "{refused}");
    // The refusal is the host's, never the filesystem's: a file no one may
    // read and a link to itself are refused the same way, with no error of
    // their own (which root, reading the first, would not get).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let locked = dir.join("locked.txt");
        std::fs::write(&locked, "do-not-read").expect("locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("mode");
        let looped = dir.join("loop");
        std::os::unix::fs::symlink(&looped, &looped).expect("link to itself");
        for target in [&locked, &looped] {
            let target = target.to_str().expect("utf-8");
            let refused = refusal(&format!("await Deno.readFile({target:?})"));
            assert!(
                refused.contains("it reads only its own WASM")
                    && !refused.contains("denied")
                    && !refused.contains("symbolic links"),
                "{refused}"
            );
        }
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o600)).expect("mode");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn node_fs_reads_and_writes_nothing() {
    let dir = scratch("fs");
    let file = dir.join("secret.txt");
    std::fs::write(&file, "do-not-read").expect("file");
    let path = file.to_str().expect("utf-8");
    for (call, name) in [
        (format!("fs.readFile({path:?}, 'utf8')"), "readFile"),
        (format!("fs.realpath({path:?})"), "realpath"),
        (
            format!("fs.readdir({:?})", dir.to_str().expect("utf-8")),
            "readdir",
        ),
        (
            format!("fs.writeFile({path:?}.concat('.new'), 'x')"),
            "writeFile",
        ),
    ] {
        let body = format!(
            "try {{ return {{ allowed: await {call} }}; }} \
             catch (error) {{ return {{ refused: String(error.message) }}; }}"
        );
        let value = probe(&body).expect("probe");
        let refused = value["refused"]
            .as_str()
            .unwrap_or_else(|| panic!("{call}: {value}"));
        assert!(
            refused.contains(&format!("has no filesystem: {name}")),
            "{call}: {refused}"
        );
    }
    assert!(
        !dir.join("secret.txt.new").exists(),
        "a write reached the disk"
    );
    let _ = std::fs::remove_dir_all(dir);
}

// The worker imports these modules statically, as the probe does here.
#[test]
fn the_node_modules_the_worker_imports_do_no_io() {
    let output = probe(
        "return { \
           hash: createHash('sha256').update('abc').digest('hex'), \
           joined: path.join('/a', 'b', '../c'), \
           relative: path.relative('/a/b', '/a/c/d.ts'), \
           url: fileURLToPath('file:///a/b%20c'), \
         };",
    )
    .expect("probe");
    assert_eq!(
        output,
        json!({
            "hash": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "joined": "/a/c",
            "relative": "../c/d.ts",
            "url": "/a/b c",
        })
    );
    // There is no working directory to resolve against.
    let refused = refusal("path.resolve('relative/file.ts')");
    assert!(refused.contains("has no working directory"), "{refused}");
    // A URL that is not a file is no path.
    let refused = refusal("fileURLToPath('http://127.0.0.1/x')");
    assert!(refused.contains("scheme file"), "{refused}");
}

#[test]
fn fetch_reaches_no_network() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listen");
    listener.set_nonblocking(true).expect("nonblocking");
    let port = listener.local_addr().expect("address").port();
    let refused = refusal(&format!(
        "(await fetch('http://127.0.0.1:{port}/')).arrayBuffer()"
    ));
    assert!(
        refused.contains("has no network") && refused.contains(&port.to_string()),
        "{refused}"
    );
    match listener.accept() {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        other => panic!("the compiler connected: {other:?}"),
    }
}

#[test]
fn the_host_offers_no_environment_process_or_deno_internals() {
    let output = probe(
        "return { \
           deno: Object.keys(Deno).sort(), \
           bootstrap: typeof globalThis.__bootstrap, \
           infra: typeof globalThis.__infra, \
           process: typeof globalThis.process, \
           require: typeof globalThis.require, \
         };",
    )
    .expect("probe");
    assert_eq!(
        output,
        json!({
            "deno": ["exit", "readFile", "stdin", "stdout", "version"],
            "bootstrap": "undefined",
            "infra": "undefined",
            "process": "undefined",
            "require": "undefined",
        })
    );
}

#[test]
fn no_other_module_can_be_imported() {
    let refused = refusal("import('node:child_process')");
    assert!(refused.contains("imports nothing dynamically"), "{refused}");
    let refused = refusal("import('node:path')");
    assert!(refused.contains("imports nothing dynamically"), "{refused}");
    // A static import of a module the host lacks fails the compile.
    let mut command = command().expect("command");
    command.env.push((
        PROBE_ENV.to_owned(),
        "import 'node:child_process';".to_owned(),
    ));
    let error = run_interactive(
        &command.program,
        &command.args,
        &command.env,
        b"{}",
        TIME_LIMIT,
        &mut |_: &Value| panic!("no calls expected"),
    )
    .expect_err("refused");
    assert!(
        error.contains("imports only its Node modules, not node:child_process"),
        "{error}"
    );
}

#[test]
fn a_heap_blow_up_is_refused_at_the_limit() {
    let heap = 64 * 1024 * 1024;
    // Within the limit, a compile allocates as it needs.
    assert_eq!(
        probe_with(
            "const kept = []; for (let i = 0; i < 32; i++) kept.push(new Array(1 << 16).fill(i)); return kept.length;",
            &[(HEAP_ENV, heap.to_string())],
            TIME_LIMIT,
        ),
        Ok(json!(32))
    );
    let started = Instant::now();
    // Three times the limit, past what the limit grows to while a compile
    // unwinds; a host without the limit would answer.
    let error = probe_with(
        &format!(
            "const kept = []; let size = 0; \
             while (size < {}) {{ kept.push(new Array(1 << 16).fill(size)); size += 8 << 16; }} \
             return kept.length;",
            3 * heap
        ),
        &[(HEAP_ENV, heap.to_string())],
        TIME_LIMIT,
    )
    .expect_err("refused");
    assert!(
        error.contains("the Astro compiler exceeded its memory limit"),
        "{error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_runaway_compile_is_killed_at_the_time_limit() {
    let started = Instant::now();
    // It would answer after eight seconds; the limit is one.
    let error = probe_with(
        "const end = Date.now() + 8000; while (Date.now() < end) {} return 'finished';",
        &[],
        Duration::from_secs(1),
    )
    .expect_err("killed");
    assert!(
        error.contains("the Astro compiler exceeded 1 seconds"),
        "{error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn the_wasm_runtime_has_its_clock_randomness_and_timers() {
    let output = probe(
        "const bytes = crypto.getRandomValues(new Uint8Array(32)); \
         const waited = await new Promise((resolve) => setTimeout(resolve, 5, 'fired')); \
         const cleared = setTimeout(() => { throw new Error('cleared timer ran'); }, 1); clearTimeout(cleared); \
         return { random: bytes.some((b) => b !== 0), waited, now: typeof performance.now() };",
    )
    .expect("probe");
    assert_eq!(
        output,
        json!({ "random": true, "waited": "fired", "now": "number" })
    );
}

#[test]
fn urls_resolve_as_the_worker_uses_them() {
    let output = probe(
        "return [ \
           new URL('./astro.wasm', import.meta.url).href, \
           new URL('../x/y.d.ts', 'atomic-compiler:///a/b/c.mjs').href, \
           new URL('/z', 'https://h.test/a/b?q').href, \
           new URL('file:///a/b').pathname, \
         ];",
    )
    .expect("probe");
    assert_eq!(
        output,
        json!([
            "atomic-compiler:///astro.wasm",
            "atomic-compiler:///a/x/y.d.ts",
            "https://h.test/z",
            "/a/b",
        ])
    );
}

// stdout is the protocol: console output, the Go runtime's included, goes
// to stderr, where a crash reports it.
#[test]
fn console_output_stays_off_the_protocol() {
    for method in ["log", "info", "warn", "error", "debug", "trace"] {
        assert_eq!(
            probe(&format!(
                "console.{method}('not', {{ a: 1 }}); return '{method}';"
            )),
            Ok(json!(method))
        );
    }
    let mut command = command().expect("command");
    command.env.push((
        PROBE_ENV.to_owned(),
        "console.error('distinct-console-text'); throw new Error('stop');".to_owned(),
    ));
    let error = run_interactive(
        &command.program,
        &command.args,
        &command.env,
        b"{}",
        TIME_LIMIT,
        &mut |_: &Value| panic!("no calls expected"),
    )
    .expect_err("crash");
    assert!(
        error.contains("distinct-console-text") && error.contains("Error: stop"),
        "{error}"
    );
}

// WebAssembly memories and ArrayBuffer contents are outside the V8 heap
// limit; the child's memory limit bounds them. Neither is touched, so a host
// without that limit would grant both without using the memory.
#[test]
fn memory_outside_the_heap_is_refused_past_the_limit() {
    let limit = limits::memory_limit(HEAP_LIMIT);
    assert_eq!(limit, limits::OUTSIDE_HEAP + 2 * HEAP_LIMIT as u64);
    let env = [];
    let grow = |pages: u64| {
        probe_with(
            &format!(
                "return new WebAssembly.Memory({{ initial: 1, maximum: 65536 }}).grow({pages});"
            ),
            &env,
            TIME_LIMIT,
        )
    };
    assert_eq!(grow(16), Ok(json!(1)));
    // A memory grown past the limit: V8 fails the compile.
    let error = grow(limit / 65536 + 1).expect_err("refused");
    assert!(
        error.contains("the Astro compiler exceeded its memory limit"),
        "{error}"
    );
    // An ArrayBuffer past the limit: V8 fails the compile.
    let error = probe_with(
        &format!("return new Uint8Array({limit} + 1).length;"),
        &env,
        TIME_LIMIT,
    )
    .expect_err("refused");
    assert!(
        error.contains("the Astro compiler exceeded its memory limit"),
        "{error}"
    );
}

// Check the production CPU limits and then lower this probe's CPU budget,
// so its CPU limit, not the production wall deadline, stops the loop even
// when a busy runner schedules it infrequently.
#[test]
fn a_compile_is_stopped_at_its_cpu_time_limit() {
    let started = Instant::now();
    let error =
        probe_with("for (;;) {}", &[(CPU_ENV, "1".to_owned())], TIME_LIMIT).expect_err("stopped");
    assert!(
        error.contains("the Astro compiler exceeded its CPU time limit"),
        "{error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(limits::CPU_SECONDS + 10),
        "{:?}",
        started.elapsed()
    );
}
