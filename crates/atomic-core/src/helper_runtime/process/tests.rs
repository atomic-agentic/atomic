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
fn a_call_runs_in_a_child_and_returns_its_result() {
    assert_eq!(probe("({ sum: 1 + 2 })"), Ok(json!({ "sum": 3 })));
    let thrown = probe("(() => { throw new Error('no'); })()");
    assert!(
        matches!(&thrown, Err(Failure::Threw(text)) if text.starts_with("Error: no")),
        "{thrown:?}"
    );
}

#[test]
fn a_large_string_split_fails_the_call_and_the_host_survives() {
    let started = Instant::now();
    assert_eq!(
        probe("'x'.repeat(2 ** 29 - 24).split('').length"),
        Err(Failure::Heap)
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(probe("1"), Ok(json!(1)));
}

// Memory allocated outside the engine heap, a single allocation larger than the
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
                 new Date(0).toLocaleString([]), 'i'.toLocaleUpperCase([]), Number.isNaN(Date.parse('January 1, 2020 00:00:00'))];"
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
        json!(["1,234.5", "1,234.5", "1/1/70, 12:00:00 AM", "I", true])
    );
}
