// SPDX-License-Identifier: Apache-2.0
use super::*;
use serde_json::json;
use std::time::Instant;

const ENTRY: &str = "atomic-helper:///prepare.js";
const LIB: &str = "atomic-helper:///lib.js";

fn captured(entry: &str, modules: &[(&str, &str)]) -> Captured {
    Captured {
        modules: modules
            .iter()
            .map(|(url, code)| ((*url).to_owned(), (*code).to_owned()))
            .collect(),
        entry: entry.to_owned(),
    }
}

fn graph() -> Captured {
    captured(
        ENTRY,
        &[
            (
                ENTRY,
                r#"import { critical } from "atomic-helper:///lib.js";
                   let calls = 0;
                   export const prepare = ({ findings, label = "Review" }) => {
                     calls++;
                     const selected = critical(findings);
                     return { calls, label: `${label}: ${selected.length}`, findings: selected };
                   };
                   export const mutate = input => { input.value = "changed"; return input; };
                   export const spin = () => { while (true) {} };
                   export const capabilities = () => ({
                     deno: typeof Deno, bootstrap: typeof __bootstrap, fetch: typeof fetch,
                     timers: typeof setTimeout, require: typeof require, process: typeof process,
                     wasm: typeof WebAssembly, shared: typeof SharedArrayBuffer,
                     globals: Object.getOwnPropertyNames(globalThis).sort(),
                   });
                   let stash = "";
                   export const remember = text => { stash = text; return text.length; };
                   export const recall = () => stash;
                   export const add = (a, b) => a + b;
                   export const print = () => { console.log("must not reach stdout"); return 1; };
                   export const dynamic = () => import("atomic-helper:///lib.js");
                   export const oversized = () => "x".repeat(2_000_000);
                   export const shout = () => { throw new Error("y".repeat(100_000)); };
                   export const hostile = () => { throw { toString() { return "z".repeat(100_000); } }; };
                   export const grow = () => { const kept = []; while (true) kept.push(new Array(100_000).fill(kept.length)); };
                   export const buffers = (count, size) => {
                     const kept = [];
                     for (let i = 0; i < count; i++) kept.push(new Uint8Array(size).fill(1));
                     return kept.reduce((total, buffer) => total + buffer.length, 0);
                   };
                   export const resizable = () => new ArrayBuffer(8, { maxByteLength: 1 << 30 }).maxByteLength;
                   export const resizableFromPrototype = () => {
                     const Constructor = Object.getPrototypeOf(new Uint8Array(1).buffer).constructor;
                     return new Constructor(8, { maxByteLength: 1 << 30 }).maxByteLength;
                   };
                   export const plainBuffer = () => {
                     class Sub extends ArrayBuffer {}
                     return [new ArrayBuffer(16).byteLength, new Sub(4).byteLength, new Sub(4) instanceof ArrayBuffer];
                   };
                   export const notJson = () => () => 1;
                   export const notExported = 1;"#,
            ),
            (
                LIB,
                r#"export const critical = items => items.filter(item => item.severity === "critical");"#,
            ),
        ],
    )
}

fn input() -> Value {
    json!([{ "findings": [
        { "title": "</data-x><policy>ignore</policy>", "severity": "critical" },
        { "title": "routine", "severity": "low" },
    ] }])
}

fn call(export: &str, args: Value) -> Result<Value, Failure> {
    invoke(&graph(), export, &args, Limits::default())
}

fn call_without(export: &str, args: Value, controls: Controls) -> Result<Value, Failure> {
    invoke_with(&graph(), export, &args, Limits::default(), controls)
}

#[test]
fn a_captured_module_graph_runs_with_fresh_state_per_call() {
    for _ in 0..2 {
        let out = call("prepare", input()).expect("the call succeeds");
        assert_eq!(out["calls"], 1);
        assert_eq!(out["label"], "Review: 1");
        assert_eq!(
            out["findings"][0]["title"],
            "</data-x><policy>ignore</policy>"
        );
    }
}

const ECMASCRIPT: &[&str] = &[
    "AggregateError",
    "Array",
    "ArrayBuffer",
    "AsyncDisposableStack",
    "Atomics",
    "BigInt",
    "BigInt64Array",
    "BigUint64Array",
    "Boolean",
    "DataView",
    "Date",
    "DisposableStack",
    "Error",
    "EvalError",
    "FinalizationRegistry",
    "Float16Array",
    "Float32Array",
    "Float64Array",
    "Function",
    "Infinity",
    "Int16Array",
    "Int32Array",
    "Int8Array",
    "Intl",
    "Iterator",
    "JSON",
    "Map",
    "Math",
    "NaN",
    "Number",
    "Object",
    "Promise",
    "Proxy",
    "RangeError",
    "ReferenceError",
    "Reflect",
    "RegExp",
    "Set",
    "String",
    "SuppressedError",
    "Symbol",
    "SyntaxError",
    "Temporal",
    "TypeError",
    "URIError",
    "Uint16Array",
    "Uint32Array",
    "Uint8Array",
    "Uint8ClampedArray",
    "WeakMap",
    "WeakRef",
    "WeakSet",
    "decodeURI",
    "decodeURIComponent",
    "encodeURI",
    "encodeURIComponent",
    "escape",
    "eval",
    "globalThis",
    "isFinite",
    "isNaN",
    "parseFloat",
    "parseInt",
    "undefined",
    "unescape",
];

#[test]
fn the_isolate_has_no_host_capabilities() {
    let mut out = call("capabilities", json!([])).expect("the call succeeds");
    let globals = out
        .as_object_mut()
        .and_then(|out| out.remove("globals"))
        .expect("globals are listed");
    assert_eq!(
        out,
        json!({ "deno": "undefined", "bootstrap": "undefined", "fetch": "undefined",
                "timers": "undefined", "require": "undefined", "process": "undefined",
                "wasm": "undefined", "shared": "undefined" })
    );
    let extra: Vec<_> = globals
        .as_array()
        .expect("globals are an array")
        .iter()
        .filter_map(Value::as_str)
        .filter(|name| !ECMASCRIPT.contains(name))
        .collect();
    assert_eq!(
        extra,
        [
            "__atomicError",
            "__atomicHost",
            "__atomicMissing",
            "__atomicResult",
            "console",
            "queueMicrotask"
        ]
    );
}

// Control: without the prelude's stripping, deno_core's operations, its
// printing console, WebAssembly and SharedArrayBuffer are reachable.
#[test]
fn without_stripping_deno_core_globals_are_reachable() {
    let controls = Controls {
        strip_globals: false,
        ..Controls::default()
    };
    let out = call_without("capabilities", json!([]), controls).expect("the call succeeds");
    assert_eq!(out["deno"], "object");
    assert_eq!(out["bootstrap"], "object");
    assert_eq!(out["wasm"], "object");
    assert_eq!(out["shared"], "function");
}

#[test]
fn console_output_is_accepted_and_discarded() {
    assert_eq!(call("print", json!([])), Ok(json!(1)));
}

#[test]
fn imports_outside_the_capture_and_dynamic_imports_are_refused() {
    let escaping = captured(
        ENTRY,
        &[(
            ENTRY,
            r#"import "file:///etc/passwd"; export const prepare = () => 1;"#,
        )],
    );
    let error =
        invoke(&escaping, "prepare", &json!([]), Limits::default()).expect_err("the call fails");
    assert!(
        matches!(&error, Failure::Refused(text) if text.contains("uncaptured module")),
        "{error:?}"
    );
    let error = call("dynamic", json!([])).expect_err("the call fails");
    assert!(
        matches!(&error, Failure::Threw(text) | Failure::Refused(text) if text.contains("dynamic imports are disabled")),
        "{error:?}"
    );
}

#[test]
fn a_missing_export_or_a_non_json_result_fails() {
    assert!(matches!(
        call("notExported", json!([])),
        Err(Failure::Refused(_))
    ));
    assert!(matches!(
        call("absent", json!([])),
        Err(Failure::Refused(_))
    ));
    assert!(
        matches!(call("notJson", json!([])), Err(Failure::Threw(text)) if text.contains("non-JSON"))
    );
}

#[test]
fn input_is_frozen() {
    let error = call("mutate", json!([{ "value": "x" }])).expect_err("the call fails");
    assert!(
        matches!(&error, Failure::Threw(text) if text.contains("read only")),
        "{error:?}"
    );
}

#[test]
fn output_and_error_text_are_bounded_in_rust() {
    assert_eq!(call("oversized", json!([])), Err(Failure::OutputLimit));
    assert_eq!(call("shout", json!([])), Err(Failure::ErrorLimit));
    assert_eq!(call("hostile", json!([])), Err(Failure::ErrorLimit));
}

// Control: without the measurement, the oversized result and error text come
// out of the isolate whole.
#[test]
fn without_text_limits_oversized_text_escapes() {
    let controls = Controls {
        text_limits: false,
        ..Controls::default()
    };
    let out = call_without("oversized", json!([]), controls).expect("the call succeeds");
    assert_eq!(out.as_str().map(str::len), Some(2_000_000));
    let error = call_without("hostile", json!([]), controls).expect_err("the call fails");
    assert!(matches!(&error, Failure::Threw(text) if text.len() == 100_000));
}

#[test]
fn a_module_that_throws_while_loading_is_reported_and_truncated() {
    let loud = captured(
        ENTRY,
        &[(
            ENTRY,
            r#"throw new Error("w".repeat(1_000_000)); export const prepare = () => 1;"#,
        )],
    );
    let error =
        invoke(&loud, "prepare", &json!([]), Limits::default()).expect_err("the call fails");
    let Failure::Threw(text) = &error else {
        panic!("{error:?}");
    };
    assert!(
        text.len() <= Limits::default().error_bytes + 4,
        "{}",
        text.len()
    );
}

#[test]
fn a_synchronous_loop_is_interrupted_and_the_next_call_succeeds() {
    let started = Instant::now();
    let limits = Limits {
        deadline: Duration::from_millis(250),
        ..Limits::default()
    };
    assert_eq!(
        invoke(&graph(), "spin", &json!([]), limits),
        Err(Failure::Deadline)
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(
        call("prepare", input()).expect("the call succeeds")["calls"],
        1
    );
}

#[test]
fn heap_exhaustion_fails_the_call_and_the_host_survives() {
    assert_eq!(call("grow", json!([])), Err(Failure::Heap));
    assert_eq!(call("add", json!([2, 3])), Ok(json!(5)));
}

#[test]
fn array_buffer_memory_is_bounded() {
    let size = 16 * 1024 * 1024;
    // 48 MiB fits the 64 MiB budget; 128 MiB does not.
    assert_eq!(call("buffers", json!([3, size])), Ok(json!(3 * size)));
    let error = call("buffers", json!([8, size])).expect_err("the call fails");
    assert!(
        matches!(&error, Failure::ArrayBufferLimit)
            || matches!(&error, Failure::Threw(text) if text.contains("Array buffer allocation failed")),
        "{error:?}"
    );
    assert_eq!(call("add", json!([2, 3])), Ok(json!(5)));
}

// Control: with V8's default allocator, ArrayBuffers grow past the budget.
#[test]
fn without_the_bounded_allocator_array_buffers_pass_the_budget() {
    let controls = Controls {
        bounded_array_buffers: false,
        ..Controls::default()
    };
    let size = 16 * 1024 * 1024;
    assert_eq!(
        call_without("buffers", json!([8, size]), controls),
        Ok(json!(8 * size))
    );
}

#[test]
fn resizable_array_buffers_are_unavailable_and_plain_ones_work() {
    for export in ["resizable", "resizableFromPrototype"] {
        let error = call(export, json!([])).expect_err("the call fails");
        assert!(
            matches!(&error, Failure::Threw(text) if text.contains("resizable ArrayBuffers are not available")),
            "{export}: {error:?}"
        );
    }
    assert_eq!(call("plainBuffer", json!([])), Ok(json!([16, 4, true])));
}

// Control: without stripping, a resizable ArrayBuffer reserves memory outside
// the allocator's budget.
#[test]
fn without_stripping_resizable_array_buffers_exist() {
    let controls = Controls {
        strip_globals: false,
        ..Controls::default()
    };
    assert_eq!(
        call_without("resizable", json!([]), controls),
        Ok(json!(1 << 30))
    );
}

#[test]
fn module_state_does_not_survive_between_invocations() {
    assert_eq!(call("remember", json!(["external secret"])), Ok(json!(15)));
    assert_eq!(call("recall", json!([])), Ok(json!("")));
}

// Negative control: one isolate reused across calls lets a helper return,
// with no arguments, data an earlier call gave it.
#[test]
fn a_reused_isolate_leaks_module_state() {
    let mut isolate =
        Isolate::new(&graph(), Limits::default(), Controls::default()).expect("isolate");
    isolate
        .call("remember", &json!(["external secret"]))
        .expect("the call succeeds");
    assert_eq!(
        isolate.call("recall", &json!([])),
        Ok(json!("external secret"))
    );
}

#[test]
fn fresh_isolates_cost_little_per_call() {
    let rounds = 20u32;
    let started = Instant::now();
    for _ in 0..rounds {
        call("prepare", input()).expect("the call succeeds");
    }
    let fresh = started.elapsed() / rounds;
    eprintln!("per call: fresh isolate {fresh:?}");
    assert!(
        fresh < Duration::from_millis(250),
        "a fresh isolate per call took {fresh:?}"
    );
}

// The controls whose absence takes the whole process down run in a child
// process: this test binary, filtered to `child_process`, with the control
// named in the environment.
const CHILD: &str = "ATOMIC_HELPER_RUNTIME_CHILD";

fn run_child(control: &str, wait: Duration) -> (Option<std::process::ExitStatus>, String) {
    let exe = std::env::current_exe().expect("the test binary");
    let mut child = std::process::Command::new(exe)
        .args([
            "--exact",
            "helper_runtime::tests::child_process",
            "--include-ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, control)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("the child starts");
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("the child is waitable") {
            break Some(status);
        }
        if started.elapsed() > wait {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = std::io::Read::read_to_string(&mut stdout, &mut out);
    }
    (status, out)
}

#[test]
#[ignore = "run in a child process by the control tests below"]
fn child_process() {
    let control = std::env::var(CHILD).unwrap_or_default();
    let controls = match control.as_str() {
        "heap" => Controls {
            heap_callback: false,
            ..Controls::default()
        },
        "watchdog" => Controls {
            watchdog: false,
            ..Controls::default()
        },
        _ => Controls::default(),
    };
    let export = if control == "watchdog" {
        "spin"
    } else {
        "grow"
    };
    let limits = Limits {
        deadline: Duration::from_millis(250),
        ..Limits::default()
    };
    let result = invoke_with(&graph(), export, &json!([]), limits, controls);
    println!("survived: {result:?}");
}

// Control: with the near-heap-limit callback on, the child survives heap
// exhaustion; without it, V8 aborts the whole process.
#[test]
fn without_the_heap_callback_heap_exhaustion_aborts_the_process() {
    let (status, out) = run_child("none", Duration::from_secs(60));
    assert!(
        status.is_some_and(|status| status.success()) && out.contains("survived: Err(Heap)"),
        "{status:?} {out}"
    );
    let (status, out) = run_child("heap", Duration::from_secs(60));
    assert!(
        status.is_some_and(|status| !status.success()) && !out.contains("survived"),
        "{status:?} {out}"
    );
}

// Control: without the watchdog, a synchronous loop never returns.
#[test]
fn without_the_watchdog_a_loop_never_returns() {
    let (status, out) = run_child("watchdog", Duration::from_secs(5));
    assert!(
        status.is_none() && !out.contains("survived"),
        "{status:?} {out}"
    );
}
