// SPDX-License-Identifier: Apache-2.0

//! `atomic runs show <run-id>`: everything the store recorded about one run,
//! read from the store alone, with no model, no lock and nothing run.
//!
//! Each recorded step is shown with the execution that recorded it (see
//! [`atomic_durable::history`]). A run killed and resumed has two or more
//! executions: a later one found the earlier ones' steps checkpointed and
//! replayed them, and ran the rest itself.

use crate::engine;
use crate::failure::Failure;
use anyhow::Context;
use atomic_app::pipeline::LAYOUT_VERSION;
use atomic_durable::gate::{self, GateFilter};
use atomic_durable::history;
use atomic_durable::{DurableRuntime, Durare, RunStatus};
use clap::Args;
use serde_json::{Value, json};

/// `atomic runs show`.
#[derive(Debug, Args)]
pub struct ShowArgs {
    /// The run's id, as `atomic run` printed it.
    pub run_id: String,
}

/// A run's status as a word.
fn status_word(status: &RunStatus) -> String {
    match status {
        RunStatus::Pending => "unfinished".into(),
        RunStatus::Succeeded => "succeeded".into(),
        RunStatus::Failed(_) => "failed".into(),
        RunStatus::Other(state) => state.to_lowercase(),
    }
}

/// Milliseconds since the Unix epoch as a UTC time, `YYYY-MM-DD HH:MM:SS`.
fn utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Gather what the store has about `id` as one JSON document.
async fn gather(rt: &Durare, id: &str) -> anyhow::Result<Option<Value>> {
    let Some(record) = rt.run_record(id).await? else {
        return Ok(None);
    };
    let steps = rt.steps(id).await?;
    let executions = history::executions(rt, id).await?;
    let recorded: Vec<Option<i64>> = steps
        .iter()
        .map(|s| history::recorded_by(s, &executions))
        .collect();
    let attempts: Vec<Value> = executions
        .iter()
        .map(|e| {
            let ran = recorded.iter().filter(|a| **a == Some(e.attempt)).count();
            let replayed = recorded
                .iter()
                .filter(|a| a.is_some_and(|a| a < e.attempt))
                .count();
            json!({
                "attempt": e.attempt,
                "started_at": e.started_at,
                "ran": ran,
                "replayed": replayed,
            })
        })
        .collect();
    let steps: Vec<Value> = steps
        .iter()
        .zip(&recorded)
        .map(|(s, attempt)| {
            json!({
                "position": s.step_id,
                "name": s.name,
                "attempt": attempt,
                "started_at": s.started_at,
                "completed_at": s.completed_at,
                "output": s.output,
                "error": s.error,
            })
        })
        .collect();
    let output = match &record.status {
        RunStatus::Succeeded => rt.result::<Value>(id).await.ok(),
        _ => None,
    };
    let error = match &record.status {
        RunStatus::Failed(why) => Some(why.clone()),
        _ => None,
    };
    let calls = rt.ledger().attempts(id).await?;
    // Model calls: an exec command's time is metered too, as work, but it
    // is no call.
    let settled: Vec<_> = calls
        .iter()
        .filter(|a| a.usage.is_some() && a.model != atomic_core::usage::WORK_MODEL)
        .collect();
    let usage = json!({
        "calls": settled.len(),
        "input_tokens": settled.iter().filter_map(|a| a.usage).map(|u| u.0).sum::<i64>(),
        "output_tokens": settled.iter().filter_map(|a| a.usage).map(|u| u.1).sum::<i64>(),
        "micro_usd": calls.iter().map(|a| a.charged.micro_usd).sum::<i64>(),
    });
    let gates: Vec<Value> = gate::gates(
        rt,
        GateFilter {
            open_only: false,
            run: Some(id.to_owned()),
        },
    )
    .await?
    .into_iter()
    .map(|g| serde_json::to_value(&g).unwrap_or(Value::Null))
    .collect();
    let threads: Vec<Value> = rt
        .threads(id)
        .await?
        .into_iter()
        .map(|t| json!({"step": t.step, "thread": t.thread_id}))
        .collect();
    // run_pipeline's input names the workflow and holds the run's own input.
    let workflow = record.input["pipeline"]["name"]
        .as_str()
        .map_or_else(|| record.workflow.clone(), str::to_owned);
    Ok(Some(json!({
        "run": record.id,
        "workflow": workflow,
        "status": status_word(&record.status),
        "error": error,
        "layout": record.version,
        "executor": record.executor,
        "input": record.input.get("input").cloned().unwrap_or(record.input.clone()),
        "output": output,
        "attempts": attempts,
        "steps": steps,
        "usage": usage,
        "gates": gates,
        "threads": threads,
    })))
}

/// `text` safe to print to a terminal: control characters, which a step's
/// error or a gate's title may carry from a model or a command, and the
/// marks that reorder text, are escaped instead of acted on.
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        let reorders = matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
        if c.is_control() || reorders {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// A string field of `v`, [`plain`].
fn field(v: &Value) -> String {
    plain(v.as_str().unwrap_or(""))
}

/// The document for a person.
fn render(doc: &Value) -> String {
    let mut out = String::new();
    let attempts = doc["attempts"].as_array().cloned().unwrap_or_default();
    out.push_str(&format!(
        "run {}: workflow {}, {}, {} attempt{}\n",
        field(&doc["run"]),
        field(&doc["workflow"]),
        field(&doc["status"]),
        attempts.len(),
        if attempts.len() == 1 { "" } else { "s" }
    ));
    if let Some(why) = doc["error"].as_str() {
        out.push_str(&format!("error: {}\n", plain(why)));
    }
    out.push_str(&format!("input: {}\n", doc["input"]));
    if !doc["output"].is_null() {
        out.push_str(&format!("output: {}\n", doc["output"]));
    }
    for a in &attempts {
        let started = a["started_at"].as_i64().map(utc).unwrap_or_default();
        let (ran, replayed) = (
            a["ran"].as_u64().unwrap_or(0),
            a["replayed"].as_u64().unwrap_or(0),
        );
        out.push_str(&format!(
            "attempt {} began {started} UTC: {}ran {ran} step{}\n",
            a["attempt"],
            if replayed > 0 {
                format!(
                    "replayed {replayed} step{} from checkpoints, ",
                    if replayed == 1 { "" } else { "s" }
                )
            } else {
                String::new()
            },
            if ran == 1 { "" } else { "s" }
        ));
    }
    out.push_str("steps:\n");
    for s in doc["steps"].as_array().into_iter().flatten() {
        let by = match s["attempt"].as_i64() {
            Some(a) => format!("attempt {a}"),
            None => "attempt unknown".into(),
        };
        let failed = s["error"]
            .as_str()
            .map(|e| format!("  failed: {}", plain(e)))
            .unwrap_or_default();
        out.push_str(&format!(
            "  {:>3}  {:<28}  {by}{failed}\n",
            s["position"],
            field(&s["name"])
        ));
    }
    let usage = &doc["usage"];
    out.push_str(&format!(
        "usage: {} model call{}, {} input and {} output tokens\n",
        usage["calls"],
        if usage["calls"] == 1 { "" } else { "s" },
        usage["input_tokens"],
        usage["output_tokens"]
    ));
    for g in doc["gates"].as_array().into_iter().flatten() {
        let state = match &g["resolution"] {
            Value::Null => "open".to_owned(),
            r => plain(&r.to_string()),
        };
        out.push_str(&format!(
            "gate {}: {} ({state})\n",
            field(&g["step"]),
            field(&g["title"])
        ));
    }
    for t in doc["threads"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "transcript of {}: Codex thread {}\n",
            field(&t["step"]),
            field(&t["thread"])
        ));
    }
    out
}

/// `atomic runs show`.
pub async fn show(
    args: ShowArgs,
    project: Option<&std::path::Path>,
    json: bool,
) -> anyhow::Result<()> {
    let state = crate::runs::state(project)?;
    let database = engine::database(&state)?;
    let rt = Durare::open(&database, None, LAYOUT_VERSION)
        .await
        .with_context(|| format!("open the store at {}", engine::describe(&database)))?;
    let Some(doc) = gather(&rt, &args.run_id).await? else {
        return Err(Failure::new(
            "run.not_found",
            format!(
                "no run {} in this project's store ({})",
                args.run_id,
                engine::describe(&database)
            ),
            "give the id `atomic run` printed, from the project that ran it (or pass --project)",
        )
        .into());
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&doc)?);
    } else {
        print!("{}", render(&doc));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn what_a_run_recorded_cannot_drive_the_terminal() {
        let doc = json!({
            "run": "r1",
            "workflow": "w\u{1b}]0;owned\u{7}",
            "status": "failed",
            "error": "boom\u{1b}[2J\nrun r2: workflow fake, succeeded",
            "input": {"text": "\u{1b}[31m"},
            "attempts": [],
            "steps": [{"position": 1, "name": "s\u{202e}cod", "attempt": 1, "error": "\r\u{9b}x"}],
            "usage": {"calls": 0, "input_tokens": 0, "output_tokens": 0},
            "gates": [{"step": "g", "title": "t\u{1b}[1A", "resolution": null}],
            "threads": [],
        });
        let said = super::render(&doc);
        assert!(
            !said
                .chars()
                .any(|c| c.is_control() && c != '\n' || c == '\u{202e}'),
            "{said:?}"
        );
        assert_eq!(
            said.lines().filter(|l| l.starts_with("run ")).count(),
            1,
            "{said}"
        );
        assert!(said.contains("\\u{1b}[2J"), "{said}");
    }

    #[test]
    fn times_read_as_utc() {
        assert_eq!(super::utc(0), "1970-01-01 00:00:00");
        assert_eq!(super::utc(1_795_046_400_000), "2026-11-19 00:00:00");
        assert_eq!(super::utc(951_782_400_000 + 61_000), "2000-02-29 00:01:01");
    }
}
