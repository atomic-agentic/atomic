// SPDX-License-Identifier: Apache-2.0
use super::*;
use serde_json::json;
use std::time::Instant;

const ENTRY: &str = "atomic-helper:///prepare.js";
// 2026-11-19T08:30:00.250Z.
const CLOCK: Clock = Clock {
    now_ms: 1_795_077_000_250.0,
    seed: [1, 2, 3, 4],
};
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
                r#"import { critical } from "./lib.js";
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
                   export const grow = () => { const kept = []; while (true) kept.push(String(kept.length).repeat(1024 * 1024)); };
                   export const buffers = (count, size) => {
                     const kept = [];
                     for (let i = 0; i < count; i++) kept.push(new Uint8Array(size));
                     return kept.reduce((total, buffer) => total + buffer.length, 0);
                   };
                   export const resizable = () => new ArrayBuffer(8, { maxByteLength: 1024 }).maxByteLength;
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
    invoke(&graph(), export, &args, Limits::default(), CLOCK)
}

fn call_without(export: &str, args: Value, controls: Controls) -> Result<Value, Failure> {
    invoke_with(&graph(), export, &args, Limits::default(), CLOCK, controls)
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
    assert_eq!(extra, ["__atomicHost", "console"]);
}

// The prelude removes SharedArrayBuffer from the helper's globals.
#[test]
fn without_stripping_shared_memory_is_reachable() {
    let controls = Controls {
        strip_globals: false,
        ..Controls::default()
    };
    let out = call_without("capabilities", json!([]), controls).expect("the call succeeds");
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
    let error = invoke(&escaping, "prepare", &json!([]), Limits::default(), CLOCK)
        .expect_err("the call fails");
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
        matches!(&error, Failure::Threw(text) if text.contains("non-writable property: value")),
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
        invoke(&loud, "prepare", &json!([]), Limits::default(), CLOCK).expect_err("the call fails");
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
        invoke(&graph(), "spin", &json!([]), limits, CLOCK),
        Err(Failure::Deadline)
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(
        call("prepare", input()).expect("the call succeeds")["calls"],
        1
    );
}

/// Limits for a call that must end at the heap limit: a deadline long enough
/// that no runner, however slow, reaches it first.
fn heap_limits() -> Limits {
    Limits {
        deadline: Duration::from_secs(30),
        ..Limits::default()
    }
}

/// Large buffer allocations exercise the memory budget, not the wall-clock
/// budget. Give them room to complete when the test runner is heavily loaded.
fn buffer_limits() -> Limits {
    Limits {
        deadline: Duration::from_secs(30),
        ..Limits::default()
    }
}

#[test]
fn heap_exhaustion_fails_the_call_and_the_host_survives() {
    assert_eq!(
        invoke(&graph(), "grow", &json!([]), heap_limits(), CLOCK),
        Err(Failure::Heap)
    );
    assert_eq!(call("add", json!([2, 3])), Ok(json!(5)));
}

#[test]
fn array_buffer_memory_is_bounded() {
    let size = 16 * 1024 * 1024;
    // 48 MiB fits the 64 MiB budget; 128 MiB does not.
    assert_eq!(
        invoke(
            &graph(),
            "buffers",
            &json!([3, size]),
            buffer_limits(),
            CLOCK
        ),
        Ok(json!(3 * size))
    );
    let error = invoke(
        &graph(),
        "buffers",
        &json!([8, size]),
        buffer_limits(),
        CLOCK,
    )
    .expect_err("the call fails");
    assert!(
        matches!(&error, Failure::ArrayBufferLimit)
            || matches!(&error, Failure::Threw(text) if text.contains("Array buffer allocation failed")),
        "{error:?}"
    );
    assert_eq!(call("add", json!([2, 3])), Ok(json!(5)));
}

// Control: without cumulative backing-store accounting, ArrayBuffers grow past the budget.
#[test]
fn without_the_bounded_allocator_array_buffers_pass_the_budget() {
    let controls = Controls {
        bounded_array_buffers: false,
        ..Controls::default()
    };
    let size = 16 * 1024 * 1024;
    assert_eq!(
        invoke_with(
            &graph(),
            "buffers",
            &json!([8, size]),
            buffer_limits(),
            CLOCK,
            controls,
        ),
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
        Ok(json!(1024))
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
        Isolate::new(&graph(), Limits::default(), CLOCK, Controls::default()).expect("isolate");
    isolate
        .call("remember", &json!(["external secret"]))
        .expect("the call succeeds");
    assert_eq!(
        isolate.call("recall", &json!([])),
        Ok(json!("external secret"))
    );
}

/// A fresh isolate per call costs little. In what it holds, a measure no
/// load on the machine changes: the heap an isolate has used once it has
/// served a call, which grows with whatever each call sets up. In time, only
/// a loose bound, since a loaded machine stretches the time of each call's
/// child process: the start-up allowance a child is given before its call's
/// deadline begins, a hundred times what a call takes on an idle machine.
#[test]
fn fresh_isolates_cost_little_per_call() {
    assert!(
        invoke(
            &graph(),
            "prepare",
            &input(),
            Limits {
                heap_bytes: FRESH_HEAP_MAX,
                ..Limits::default()
            },
            CLOCK
        )
        .is_ok()
    );
    let rounds = 20u32;
    let started = Instant::now();
    for _ in 0..rounds {
        call("prepare", input()).expect("the call succeeds");
    }
    let fresh = started.elapsed() / rounds;
    eprintln!("per call: fresh isolate {fresh:?}");
    assert!(
        fresh < Duration::from_secs(2),
        "a fresh isolate per call took {fresh:?}"
    );
}

/// The most heap a fresh isolate holds after one call: about three times the
/// 1.4 MB it holds (see `fresh_isolates_cost_little_per_call`).
const FRESH_HEAP_MAX: usize = 4 << 20;

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
    // The watchdog control spins until the deadline; the heap controls grow
    // until the heap limit, with a deadline that cannot fire first.
    let (export, limits) = if control == "watchdog" {
        (
            "spin",
            Limits {
                deadline: Duration::from_millis(250),
                ..Limits::default()
            },
        )
    } else {
        ("grow", heap_limits())
    };
    let finite = captured(
        ENTRY,
        &[(
            ENTRY,
            "export const grow = () => 'x'.repeat(16 << 20).length",
        )],
    );
    let graph = if export == "spin" { graph() } else { finite };
    let limits = Limits {
        heap_bytes: 8 << 20,
        ..limits
    };
    let result = process::supervised(&graph, export, &json!([]), limits, CLOCK, controls);
    println!("survived: {result:?}");
}

// The heap budget terminates the child with a recognizable exit status.
#[test]
fn without_the_heap_callback_heap_exhaustion_aborts_the_process() {
    let (status, out) = run_child("none", Duration::from_secs(60));
    assert!(
        status.and_then(|s| s.code()) == Some(crate::js_budget::HEAP_EXIT),
        "{status:?} {out}"
    );
    let (status, out) = run_child("heap", Duration::from_secs(60));
    assert!(
        status.is_some_and(|s| s.success()) && out.contains("survived: Ok"),
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

const CLOCKED: &str = r#"
export const time = () => ({
  now: Date.now(), made: new Date().toISOString(), called: Date(),
  local: new Date(2026, 0, 2, 3, 4, 5).toISOString(), parsed: new Date('2026-01-02T03:04').toISOString(),
  hours: new Date(0).getHours(), offset: new Date().getTimezoneOffset(), text: new Date(0).toString(),
});
export const temporal = () => ({
  instant: Temporal.Now.instant().epochMilliseconds, zone: Temporal.Now.timeZoneId(),
  date: Temporal.Now.plainDateISO().toString(),
});
export const random = () => [Math.random(), Math.random(), Math.random()];
export const zone = () => ({
  format: new Intl.DateTimeFormat(undefined, { hour: 'numeric', minute: 'numeric', hour12: false }).format(new Date(0)),
  resolved: Intl.DateTimeFormat().resolvedOptions().timeZone, locale: Intl.DateTimeFormat().resolvedOptions().locale,
  local: new Date(0).toLocaleString(), number: (1234567.5).toLocaleString(),
  empty: [(1234.5).toLocaleString([]), new Date(0).toLocaleString([]), 'i'.toLocaleUpperCase([])],
  unsupported: (1234.5).toLocaleString('tlh'),
});
const finite = n => Number.isFinite(n) ? n : null;
export const escapes = () => ({
  formatted: new Intl.DateTimeFormat('en-US', { dateStyle: 'full', timeStyle: 'long' }).format(),
  parts: new Intl.DateTimeFormat('en-US', { year: 'numeric' }).formatToParts?.()?.[0]?.value ?? null,
  given: new Intl.DateTimeFormat('en-US', { year: 'numeric' }).format(new Date(0)),
  legacy: finite(Date.parse('January 1, 2020 00:00:00')), constructed: finite(new Date('January 1, 2020 00:00:00').getTime()),
  zoned: finite(Date.parse('Tue, 01 Jan 2020 00:00:00 GMT')), own: finite(Date.parse(new Date(0).toString())),
  year: new Date(0).getYear(),
});
"#;

fn clocked() -> Captured {
    captured(ENTRY, &[(ENTRY, CLOCKED)])
}

fn clocked_call(export: &str, clock: Clock, controls: Controls) -> Value {
    invoke_with(
        &clocked(),
        export,
        &json!([]),
        Limits::default(),
        clock,
        controls,
    )
    .expect("the call succeeds")
}

#[test]
fn date_is_the_logical_time_in_utc() {
    let out = clocked_call("time", CLOCK, Controls::default());
    assert_eq!(out["now"], 1_795_077_000_250u64);
    assert_eq!(out["made"], "2026-11-19T08:30:00.250Z");
    assert_eq!(
        out["called"],
        "Thu Nov 19 2026 08:30:00 GMT+0000 (Coordinated Universal Time)"
    );
    assert_eq!(out["local"], "2026-01-02T03:04:05.000Z");
    assert_eq!(out["parsed"], "2026-01-02T03:04:00.000Z");
    assert_eq!(out["hours"], 0);
    assert_eq!(out["offset"], 0);
    assert_eq!(
        out["text"],
        "Thu Jan 01 1970 00:00:00 GMT+0000 (Coordinated Universal Time)"
    );
}

#[test]
fn temporal_now_is_the_logical_time_in_utc() {
    let out = clocked_call("temporal", CLOCK, Controls::default());
    assert_eq!(
        out,
        json!({ "instant": 1_795_077_000_250u64, "zone": "UTC", "date": "2026-11-19" })
    );
}

#[test]
fn math_random_follows_the_seed() {
    let first = clocked_call("random", CLOCK, Controls::default());
    assert_eq!(clocked_call("random", CLOCK, Controls::default()), first);
    let other = Clock {
        seed: [5, 6, 7, 8],
        ..CLOCK
    };
    assert_ne!(clocked_call("random", other, Controls::default()), first);
    assert!(
        first
            .as_array()
            .expect("numbers")
            .iter()
            .all(|n| (0.0..1.0).contains(&n.as_f64().unwrap_or(-1.0)))
    );
}

// Built-ins the prelude does not replace: a DateTimeFormat given no date,
// and Date's legacy string formats and Annex B accessors, which read ICU's
// local time.
#[test]
fn formatting_the_current_time_and_legacy_dates_use_the_clock_and_utc() {
    let out = clocked_call("escapes", CLOCK, Controls::default());
    assert_eq!(
        out,
        json!({
            "formatted": "Thursday, November 19, 2026 at 8:30:00 AM",
            "parts": null, "given": "1970",
            "legacy": null, "constructed": null,
            "zoned": 1_577_836_800_000u64, "own": null, "year": 70,
        })
    );
}

#[test]
fn formatting_uses_utc_and_en_us() {
    let out = clocked_call("zone", CLOCK, Controls::default());
    assert_eq!(
        out,
        json!({
            "format": "12:00 AM", "resolved": "utc", "locale": "en-US", "local": "1/1/70, 12:00:00 AM",
            "number": "1,234,567.5", "empty": ["1,234.5", "1/1/70, 12:00:00 AM", "I"], "unsupported": "1,234.5",
        })
    );
}

// Control: without the replacement, Date, Temporal.Now and Math.random read
// the host, so two calls with one clock disagree and the time is not the
// logical one.
#[test]
fn without_the_virtual_clock_helpers_read_the_host() {
    let controls = Controls {
        virtual_clock: false,
        ..Controls::default()
    };
    let time = clocked_call("time", CLOCK, controls);
    assert_ne!(time["now"], 1_795_077_000_250u64);
    let temporal = clocked_call("temporal", CLOCK, controls);
    assert_ne!(temporal["instant"], 1_795_077_000_250u64);
    assert_ne!(
        clocked_call("random", CLOCK, controls),
        clocked_call("random", CLOCK, controls)
    );
}

// The host's timezone and locale come from the process environment, so they
// are varied in child processes: with the replacement every zone formats
// alike; without it, they differ. Only what the zone and locale decide is
// compared for that: `escapes` formats the clock's own time, which, with the
// host's clock, differs between two children whenever a second passes.
#[test]
fn the_host_timezone_and_locale_do_not_reach_helpers() {
    // The child's line that starts with `label`.
    let line = |text: &str, label: &str| {
        text.lines()
            .find_map(|l| l.find(label).map(|at| l[at..].to_owned()))
            .unwrap_or_default()
    };
    let outputs = |control: &str| -> Vec<(String, String)> {
        ["Asia/Kolkata", "America/Los_Angeles"]
            .iter()
            .map(|zone| {
                let exe = std::env::current_exe().expect("the test binary");
                let out = std::process::Command::new(exe)
                    .args([
                        "--exact",
                        "helper_runtime::tests::zone_child",
                        "--include-ignored",
                        "--nocapture",
                        "--test-threads=1",
                    ])
                    .env(CHILD, control)
                    .env("TZ", zone)
                    .env(
                        "LANG",
                        if zone.starts_with("Asia") {
                            "tr_TR.UTF-8"
                        } else {
                            "en_US.UTF-8"
                        },
                    )
                    .env(
                        "LC_ALL",
                        if zone.starts_with("Asia") {
                            "tr_TR.UTF-8"
                        } else {
                            "en_US.UTF-8"
                        },
                    )
                    .output()
                    .expect("the child runs");
                let text = String::from_utf8_lossy(&out.stdout).into_owned();
                (line(&text, "zone: "), line(&text, "escapes: "))
            })
            .collect()
    };
    let virtual_ = outputs("clock");
    assert!(
        virtual_.iter().all(|(z, e)| !z.is_empty() && !e.is_empty()),
        "{virtual_:?}"
    );
    assert_eq!(virtual_[0], virtual_[1]);
    assert!(virtual_[0].1.contains(r#""legacy":null"#), "{virtual_:?}");
    let host = outputs("host-clock");
    assert!(host.iter().all(|(z, _)| !z.is_empty()), "{host:?}");
    // On macOS the zone and locale the runtime starts with come from the
    // system's settings rather than TZ, LANG and LC_ALL: there, without the
    // replacement, both children resolved UTC and en-US, so varying the
    // environment cannot show the control.
    #[cfg(target_os = "linux")]
    assert_ne!(
        host[0].0, host[1].0,
        "the control shows the host zone reaching helpers"
    );
}

#[test]
#[ignore = "run in a child process by the_host_timezone_and_locale_do_not_reach_helpers"]
fn zone_child() {
    let controls = match std::env::var(CHILD).as_deref() {
        Ok("host-clock") => Controls {
            virtual_clock: false,
            ..Controls::default()
        },
        _ => Controls::default(),
    };
    let zone = clocked_call("zone", CLOCK, controls);
    let time = clocked_call("time", CLOCK, controls);
    let escapes = clocked_call("escapes", CLOCK, controls);
    println!("zone: {zone} {} {}", time["hours"], time["text"]);
    println!("escapes: {escapes}");
}
