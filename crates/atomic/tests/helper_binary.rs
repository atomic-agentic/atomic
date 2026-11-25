// SPDX-License-Identifier: Apache-2.0

//! The installed atomic binary is also the normal one-call Rust helper child.

use atomic_core::helper_runtime::{
    CHILD_ARG, Captured, Clock, Failure, HostCommand, Limits, invoke, set_host_command,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant},
};

const ENTRY: &str = "atomic-helper:///entry.js";
const LIB: &str = "atomic-helper:///math.js";
const CLOCK: Clock = Clock {
    now_ms: 1_795_077_000_250.0,
    seed: [1, 2, 3, 4],
};

fn graph(source: &str) -> Captured {
    Captured {
        modules: BTreeMap::from([
            (ENTRY.to_owned(), source.to_owned()),
            (
                LIB.to_owned(),
                "export const add = (a, b) => a + b;".to_owned(),
            ),
        ]),
        entry: ENTRY.to_owned(),
    }
}

fn call(graph: &Captured, export: &str, args: Value, limits: Limits) -> Result<Value, Failure> {
    invoke(graph, export, &args, limits, CLOCK)
}

#[test]
fn production_binary_enforces_rust_helper_process_controls() {
    set_host_command(HostCommand {
        program: PathBuf::from(env!("CARGO_BIN_EXE_atomic")),
        args: vec![CHILD_ARG.to_owned()],
        env: Vec::new(),
    });
    let code = graph(
        r#"import { add } from './math.js';
           let calls = 0;
           export const sum = () => add(2, 3);
           export const once = () => ++calls;
           export const environment = () => ({
             now: Date.now(), parsed: new Date('2026-01-02T03:04').toISOString(),
             local: new Date(0).getHours(),
             locale: Intl.DateTimeFormat().resolvedOptions().locale,
             zone: Intl.DateTimeFormat().resolvedOptions().timeZone,
             process: typeof process, deno: typeof Deno, wasm: typeof WebAssembly,
             shared: typeof SharedArrayBuffer,
           });
           export const random = () => [Math.random(), Math.random()];
           export const buffers = (count, size) => {
             const kept = [];
             for (let i = 0; i < count; i++) kept.push(new Uint8Array(size));
             return kept.reduce((n, item) => n + item.length, 0);
           };
           export const grow = () => 'x'.repeat(16 << 20).length;
           export const spin = () => { while (true) {} };
           export const dynamic = () => import('./math.js');"#,
    );

    assert_eq!(
        call(&code, "sum", json!([]), Limits::default()),
        Ok(json!(5))
    );
    assert_eq!(
        call(&code, "once", json!([]), Limits::default()),
        Ok(json!(1))
    );
    assert_eq!(
        call(&code, "once", json!([]), Limits::default()),
        Ok(json!(1))
    );
    let environment = call(&code, "environment", json!([]), Limits::default())
        .expect("production child sees a deterministic environment");
    assert_eq!(environment["now"], 1_795_077_000_250u64);
    assert_eq!(environment["parsed"], "2026-01-02T03:04:00.000Z");
    assert_eq!(environment["local"], 0);
    assert_eq!(environment["locale"], "en-US");
    assert_eq!(
        environment["zone"].as_str().map(str::to_ascii_lowercase),
        Some("utc".into())
    );
    for name in ["process", "deno", "wasm", "shared"] {
        assert_eq!(environment[name], "undefined", "{name}");
    }
    let random = call(&code, "random", json!([]), Limits::default())
        .expect("production child returns seeded randomness");
    assert!(
        random
            .as_array()
            .is_some_and(|values| values.len() == 2 && values.iter().all(Value::is_number)),
        "{random}"
    );
    assert_eq!(
        call(&code, "random", json!([]), Limits::default())
            .expect("same seed returns seeded randomness"),
        random
    );

    let outside = graph("import 'file:///etc/passwd'; export const sum = () => 5;");
    assert!(matches!(
        call(&outside, "sum", json!([]), Limits::default()),
        Err(Failure::Refused(text)) if text.contains("uncaptured module")
    ));
    assert!(matches!(
        call(&code, "dynamic", json!([]), Limits::default()),
        Err(Failure::Threw(text) | Failure::Refused(text)) if text.contains("dynamic imports are disabled")
    ));

    let memory = Limits {
        deadline: Duration::from_secs(30),
        ..Limits::default()
    };
    let size = 16 * 1024 * 1024;
    assert_eq!(
        call(&code, "buffers", json!([3, size]), memory),
        Ok(json!(3 * size))
    );
    let buffers = call(&code, "buffers", json!([8, size]), memory);
    assert!(
        matches!(&buffers, Err(Failure::ArrayBufferLimit))
            || matches!(&buffers, Err(Failure::Threw(text)) if text.contains("Array buffer allocation failed")),
        "{buffers:?}"
    );
    let heap = Limits {
        heap_bytes: 8 << 20,
        ..memory
    };
    assert_eq!(call(&code, "grow", json!([]), heap), Err(Failure::Heap));
    let deadline = Limits {
        deadline: Duration::from_millis(250),
        ..Limits::default()
    };
    let started = Instant::now();
    assert_eq!(
        call(&code, "spin", json!([]), deadline),
        Err(Failure::Deadline)
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(
        call(&code, "sum", json!([]), Limits::default()),
        Ok(json!(5))
    );
}
