// SPDX-License-Identifier: Apache-2.0
//! The process boundary, with the probes that used to escape an in-process
//! isolate: each now fails its call, within the call's time, and the host
//! goes on.
use super::*;
use serde_json::json;

/// Set in a child started by [`default_host`].
pub(super) const CHILD_ENV: &str = "ATOMIC_HELPER_TEST_CHILD";

#[test]
#[ignore = "serves one helper call in a child process"]
fn child() {
    if std::env::var_os(CHILD_ENV).is_some() {
        std::process::exit(serve());
    }
}

const ENTRY: &str = "atomic-helper:///probe.js";
const CLOCK: Clock = Clock {
    now_ms: 0.0,
    seed: [1, 2, 3, 4],
};

fn probe(body: &str) -> Result<Value, Failure> {
    let captured = Captured {
        modules: [(
            ENTRY.to_owned(),
            format!("export const probe = () => {body};"),
        )]
        .into(),
        entry: ENTRY.to_owned(),
    };
    invoke(&captured, "probe", &json!([]), Limits::default(), CLOCK)
}

#[test]
fn oversized_helper_limits_are_rejected_before_spawning() {
    let command = HostCommand {
        program: std::path::PathBuf::from("/no-such-atomic-helper"),
        args: Vec::new(),
        env: Vec::new(),
    };
    let captured = Captured {
        modules: Default::default(),
        entry: ENTRY.to_owned(),
    };
    let limits = Limits {
        output_bytes: usize::MAX,
        error_bytes: 1,
        ..Limits::default()
    };
    assert_eq!(
        run(&command, &captured, "probe", &json!([]), limits, CLOCK),
        Err(Failure::Host("helper byte limits are too large".into()))
    );
}

#[test]
fn a_call_runs_in_a_child_and_returns_its_result() {
    assert_eq!(probe("({ sum: 1 + 2 })"), Ok(json!({ "sum": 3 })));
    let thrown = probe("(() => { throw new Error('no'); })()");
    assert!(
        matches!(&thrown, Err(Failure::Threw(text)) if text.starts_with("Error: no")),
        "{thrown:?}"
    );
}

// V8 aborts the process on these, which no isolate limit catches.
#[test]
fn a_v8_fatal_error_fails_the_call_and_the_host_survives() {
    let started = Instant::now();
    assert_eq!(
        probe("'x'.repeat(2 ** 29 - 24).split('').length"),
        Err(Failure::Heap)
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(probe("1"), Ok(json!(1)));
}

// Memory V8 allocates outside its heap, a single allocation larger than the
// heap, and work inside one long built-in are each bounded.
#[test]
fn memory_and_time_outside_the_heap_are_bounded() {
    for body in [
        "'\\u1234'.repeat(2 ** 29 - 24).charCodeAt(5)",
        "new RegExp('(?:a|b|c|d)'.repeat(3e6)).test('a')",
        "Array.from({ length: 2 ** 27 + 5 }).length",
        "new Array(150e6).fill(0).length",
    ] {
        let started = Instant::now();
        let result = probe(body);
        assert!(
            matches!(result, Err(Failure::Heap | Failure::Deadline)),
            "{body}: {result:?}"
        );
        assert!(
            started.elapsed() < STARTUP + Limits::default().deadline + Duration::from_secs(2),
            "{body}: {:?}",
            started.elapsed()
        );
    }
}

// Where the child cannot limit its own memory, the host holds the limit on
// the memory it uses (see `child_memory`). The host is given a limit far
// below the call's own here, on every platform, so that a host that does
// not hold it lets the call run to its deadline. The memory is ArrayBuffer
// contents within the call's own budget, and is filled: memory not written
// to is not in use.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn memory_a_call_uses_past_its_limit_is_refused_by_the_host() {
    const LIMIT: u64 = 256 * 1024 * 1024;
    let captured = Captured {
        modules: [(
            ENTRY.to_owned(),
            "export const probe = (megabytes, spin) => { \
               const kept = []; \
               for (let size = 0; size < megabytes; size += 16) kept.push(new Uint8Array(16 << 20).fill(1)); \
               while (spin) {} \
               return kept.length; \
             };"
            .to_owned(),
        )]
        .into(),
        entry: ENTRY.to_owned(),
    };
    // A deadline a busy machine does not reach while the call fills memory.
    let limits = Limits {
        deadline: Duration::from_secs(10),
        array_buffer_bytes: 512 * 1024 * 1024,
        ..Limits::default()
    };
    let fill = |megabytes: u64, spin: bool| {
        crate::child_memory::with_limit(LIMIT, || {
            invoke(&captured, "probe", &json!([megabytes, spin]), limits, CLOCK)
        })
    };
    // Within the limit, a call uses what it needs.
    assert_eq!(fill(64, false), Ok(json!(4)));
    // Past it, the call is ended as one that exhausted its memory, while it
    // still holds the memory; a host without the limit ends it at its
    // deadline instead.
    let started = Instant::now();
    assert_eq!(fill(384, true), Err(Failure::Heap));
    assert!(
        started.elapsed() < limits.deadline,
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn code_cannot_be_generated_from_strings() {
    for body in ["eval('1')", "new Function('return 1')()", "(0, eval)('1')"] {
        let result = probe(body);
        assert!(
            matches!(&result, Err(Failure::Threw(text)) if text.contains("EvalError")),
            "{body}: {result:?}"
        );
    }
}

// The child's environment is the host command's alone: a host timezone
// and locale set in it are still overridden on the first call.
#[test]
fn the_first_call_in_a_child_sees_utc_and_en_us() {
    let mut command = default_host().expect("host");
    let mut outputs = Vec::new();
    for (zone, locale) in [
        ("Asia/Kolkata", "tr_TR.UTF-8"),
        ("America/Los_Angeles", "en_US.UTF-8"),
    ] {
        command.env.retain(|(key, _)| key == CHILD_ENV);
        command.env.extend(
            [("TZ", zone), ("LANG", locale), ("LC_ALL", locale)]
                .map(|(k, v)| (k.to_owned(), v.to_owned())),
        );
        let captured = Captured {
            modules: [(
                ENTRY.to_owned(),
                "export const probe = () => [(1234.5).toLocaleString([]), (1234.5).toLocaleString('tlh'), \
                 new Date(0).toLocaleString([]), 'i'.toLocaleUpperCase([]), Date.parse('January 1, 2020 00:00:00')];"
                    .to_owned(),
            )]
            .into(),
            entry: ENTRY.to_owned(),
        };
        outputs.push(
            run(
                &command,
                &captured,
                "probe",
                &json!([]),
                Limits::default(),
                CLOCK,
            )
            .expect("runs"),
        );
    }
    assert_eq!(outputs[0], outputs[1]);
    assert_eq!(
        outputs[0],
        json!([
            "1,234.5",
            "1,234.5",
            "1/1/1970, 12:00:00 AM",
            "I",
            1_577_836_800_000u64
        ])
    );
}
