// SPDX-License-Identifier: Apache-2.0

//! Measure how often a Codex atom's answer fits its output schema on a
//! provider: on the first turn, after the repair turn, or not at all.
//!
//! Codex speaks only the Responses API, and some OpenAI-compatible servers
//! accept its structured-output request there but do not enforce it in
//! decoding. atomic then states the schema in the atom's instructions (see
//! docs/models.md); this shows how well that holds on a given provider and
//! model, and how much the repair turn is needed.
//!
//! ```text
//! ATOMIC_MODEL_BASE_URL=http://127.0.0.1:13305/api/v1 ATOMIC_MODEL=<model> \
//!     cargo run -p atomic-agentic-atoms --example schema_fit -- 3
//! ```
//!
//! The argument is how many runs per request (default 2). Each
//! run is a fresh Codex thread in the read-only sandbox, in a scratch
//! directory, on the `draft-task` contract. The program starts through
//! Codex's entry point, as the `atomic` binary does, so the sandbox helper
//! is there when the model runs a command.

use atomic_atoms::{ChangeRequest, DraftTask, DraftedTask};
use atomic_core::codex::{self, ModelEndpoint};
use atomic_core::runtime::{Artifacts, CodexRuntime, run_codex_atom};
use atomic_core::{Atom, AtomConfig, AtomError, AtomKind};
use serde_json::Value;
use std::borrow::Cow;
use std::time::Instant;

/// `draft-task`, run as a Codex atom.
struct Draft;

impl Atom for Draft {
    type In = ChangeRequest;
    type Out = DraftedTask;
    const NAME: &'static str = "draft-task-on-codex";
    fn input_schema(&self) -> Value {
        DraftTask.input_schema()
    }
    fn output_schema(&self) -> Value {
        DraftTask.output_schema()
    }
    fn instructions(&self) -> Cow<'static, str> {
        DraftTask.instructions()
    }
    fn kind(&self) -> AtomKind {
        AtomKind::Codex
    }
    fn validate(&self, out: &DraftedTask) -> Result<(), AtomError> {
        DraftTask.validate(out)
    }
}

const REQUESTS: &[&str] = &[
    "Checkout charges for one unit no matter how many are in the cart. Two units at 1.25 should cost 2.50.",
    "Add a --json flag to the export command so scripts can read its output.",
    "The login page hangs for 30 seconds when the identity provider is down; it should fail fast with a clear message.",
    "Rename the 'Archive' button to 'Move to archive' and keep the keyboard shortcut.",
    "Nightly reports sometimes contain yesterday's totals twice after a daylight saving change.",
];

#[derive(Default)]
struct Tally {
    first: u32,
    repaired: u32,
    failed: u32,
    tokens: i64,
    seconds: f64,
}

fn main() {
    let runs: u32 = std::env::args()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(2);
    let endpoint = ModelEndpoint {
        base_url: std::env::var("ATOMIC_MODEL_BASE_URL").expect("set ATOMIC_MODEL_BASE_URL"),
        model: std::env::var("ATOMIC_MODEL").expect("set ATOMIC_MODEL"),
        api_key_env: std::env::var("ATOMIC_MODEL_API_KEY_ENV").ok(),
        sign_in: None,
    };
    let home = std::env::temp_dir().join(format!("atomic-schema-fit-{}", std::process::id()));
    std::fs::create_dir_all(&home).expect("codex home");
    let result = codex::dispatch(&home.clone(), move |paths| async move {
        let atom = Draft;
        let mut tally = Tally::default();
        {
            for (i, request) in REQUESTS.iter().enumerate() {
                for run in 0..runs {
                    let dir = std::env::temp_dir().join(format!(
                        "atomic-schema-fit-{}-{i}-{run}",
                        std::process::id()
                    ));
                    for sub in ["home", "cwd"] {
                        std::fs::create_dir_all(dir.join(sub)).expect("scratch");
                    }
                    let rt = CodexRuntime {
                        paths: paths.clone(),
                        codex_home: dir.join("home"),
                        guard: atomic_core::codex::StateGuard::deny_state_dir(dir.clone())
                            .expect("guard"),
                        cwd: dir.join("cwd"),
                        endpoint: endpoint.clone(),
                        defaults: AtomConfig::default(),
                        artifacts: Artifacts::none(),
                        meter: atomic_core::usage::Metering::none(),
                        caches: None,
                    };
                    let input = ChangeRequest {
                        request: (*request).to_owned(),
                    };
                    let started = Instant::now();
                    let result = run_codex_atom(&atom, &input, &rt, &Default::default()).await;
                    let seconds = started.elapsed().as_secs_f64();
                    tally.seconds += seconds;
                    let outcome = match &result {
                        Ok((_, meta)) => {
                            tally.tokens += meta.total_tokens;
                            if meta.turns == 1 {
                                tally.first += 1;
                                "fit on the first turn"
                            } else {
                                tally.repaired += 1;
                                "fit after the repair turn"
                            }
                        }
                        Err(_) => {
                            tally.failed += 1;
                            "failed"
                        }
                    };
                    eprintln!("request {i}, run {run}: {outcome} ({seconds:.0} s)");
                    match &result {
                        Err(AtomError::Schema { message, answer }) => {
                            let start: String = answer.chars().take(300).collect();
                            eprintln!("    {message}\n    answer begins: {start:?}");
                        }
                        Err(e) => eprintln!("    {e}"),
                        Ok(_) => {}
                    }
                    let _ = std::fs::remove_dir_all(&dir);
                }
            }
        }
        let t = tally;
        let n = t.first + t.repaired + t.failed;
        let ok = (t.first + t.repaired).max(1);
        println!("| Runs | First turn fits | Repaired | Failed | Mean tokens | Mean seconds |");
        println!("| --- | --- | --- | --- | --- | --- |");
        println!(
            "| {n} | {} | {} | {} | {} | {:.0} |",
            t.first,
            t.repaired,
            t.failed,
            t.tokens / i64::from(ok),
            t.seconds / f64::from(n.max(1)),
        );
        Ok(())
    });
    let _ = std::fs::remove_dir_all(&home);
    result.expect("run");
}
