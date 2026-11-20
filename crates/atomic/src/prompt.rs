// SPDX-License-Identifier: Apache-2.0

//! The terminal prompt `atomic run` and `atomic runs resume` show while
//! their run waits at a gate.
//!
//! The person at the terminal decides there: approve or reject, with a
//! comment, or the answer to a question gate. The decision goes through
//! [`crate::gates::decide_as_operator`], with the operator's enrolled
//! credential, exactly as `atomic gates approve` from another terminal
//! would; the prompt is a person at a terminal, not a shortcut past the gate
//! store's checks. With no terminal on standard input, it says once per gate
//! how to decide it from another terminal, and the run waits.

use crate::gates;
use atomic_core::state::ProjectState;
use atomic_durable::Durare;
use atomic_durable::gate::{self, Gate, GateFilter, Verdict};
use serde_json::Value;
use std::collections::BTreeSet;
use std::io::{BufRead, IsTerminal, Write};
use std::sync::mpsc;
use std::time::Duration;

/// How often the prompt looks for a newly opened gate.
const POLL: Duration = Duration::from_millis(250);

/// Lines typed at the terminal, read by a thread of their own: a read that
/// is still waiting when the run finishes must not hold the process.
struct Lines(mpsc::Receiver<String>);

impl Lines {
    fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self(rx)
    }

    /// The next line, or `None` once standard input is closed.
    async fn next(&self) -> Option<String> {
        loop {
            match self.0.try_recv() {
                Ok(line) => return Some(line),
                Err(mpsc::TryRecvError::Disconnected) => return None,
                Err(mpsc::TryRecvError::Empty) => tokio::time::sleep(POLL / 5).await,
            }
        }
    }
}

/// What the person typed, as a decision.
#[derive(Debug, PartialEq)]
struct Reply {
    verdict: Verdict,
    comment: Option<String>,
    answer: Option<Value>,
}

/// Read `line` as a decision on `g`, or say why it is not one.
fn reply(g: &Gate, line: &str) -> Result<Reply, String> {
    let line = line.trim();
    let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let comment = Some(rest.trim().to_owned()).filter(|c| !c.is_empty());
    let Some(question) = &g.question else {
        return match word.to_lowercase().as_str() {
            "a" | "approve" | "y" | "yes" => Ok(Reply {
                verdict: Verdict::Approve,
                comment,
                answer: None,
            }),
            "r" | "reject" | "n" | "no" => Ok(Reply {
                verdict: Verdict::Reject,
                comment,
                answer: None,
            }),
            _ => Err("type `a` to approve or `r` to reject, then a comment if you like".into()),
        };
    };
    if word == "/reject" {
        return Ok(Reply {
            verdict: Verdict::Reject,
            comment,
            answer: None,
        });
    }
    if line.is_empty() {
        return Err("type an answer, or /reject to refuse".into());
    }
    let answer = match question.choices() {
        // A choice by its number, or by its value.
        Some(choices) => match line.parse::<usize>() {
            Ok(n) if (1..=choices.len()).contains(&n) => choices[n - 1].clone(),
            _ => choices
                .iter()
                .find(|c| c.as_str() == Some(line))
                .cloned()
                .unwrap_or_else(|| gates::parse_answer(line)),
        },
        None if question.answer.get("type").and_then(Value::as_str) == Some("string") => {
            Value::String(line.to_owned())
        }
        None => gates::parse_answer(line),
    };
    // Refused here for a quicker retry; the gate store checks it again.
    question
        .check(&answer)
        .map_err(|why| format!("{answer} is not an answer to this question: {why}"))?;
    Ok(Reply {
        verdict: Verdict::Approve,
        comment: None,
        answer: Some(answer),
    })
}

/// Show gate `g` and what to type.
fn describe(g: &Gate) {
    let mut e = std::io::stderr().lock();
    let _ = writeln!(e, "\ngate {} is waiting for you", g.id);
    if g.question.is_some() {
        let _ = writeln!(e, "  question: {}", g.title);
    } else {
        let _ = writeln!(e, "  {}", g.title);
    }
    if !g.show.is_null() {
        let shown = serde_json::to_string_pretty(&g.show).unwrap_or_default();
        let _ = writeln!(e, "  shows: {}", shown.replace('\n', "\n  "));
    }
    if let Some(choices) = g.question.as_ref().and_then(|q| q.choices()) {
        for (i, c) in choices.iter().enumerate() {
            let shown = c.as_str().map_or_else(|| c.to_string(), str::to_owned);
            let _ = writeln!(e, "  {}) {shown}", i + 1);
        }
    }
    if let Some(deadline) = g.deadline {
        let left = (deadline - now_ms()).max(0) / 1000;
        let _ = writeln!(e, "  it waits {left}s more");
    }
}

fn ask(g: &Gate) {
    let mut e = std::io::stderr().lock();
    let _ = if g.question.is_some() {
        write!(e, "answer with a number or a value (/reject to refuse) > ")
    } else {
        write!(
            e,
            "approve or reject: `a` or `r`, then a comment if you like > "
        )
    };
    let _ = e.flush();
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Prompt for every gate run `run_id` opens, for as long as it runs. Never
/// returns; the caller stops it when the run ends or a signal stops it.
pub async fn at_gates(rt: &Durare, state: &ProjectState, run_id: &str) {
    let terminal = std::io::stdin().is_terminal();
    let mut done: BTreeSet<String> = BTreeSet::new();
    let mut lines: Option<Lines> = None;
    loop {
        let open = gate::gates(
            rt,
            GateFilter {
                open_only: true,
                run: Some(run_id.to_owned()),
            },
        )
        .await
        .unwrap_or_default();
        let new: Vec<&Gate> = open.iter().filter(|g| !done.contains(&g.id)).collect();
        for g in new {
            done.insert(g.id.clone());
            if !terminal {
                let how = if g.question.is_some() {
                    format!("`atomic gates answer {} <answer>`", g.id)
                } else {
                    format!(
                        "`atomic gates approve {}` or `atomic gates reject {}`",
                        g.id, g.id
                    )
                };
                eprintln!(
                    "run {run_id} waits at gate {} ({}): decide it from a terminal with {how}",
                    g.id, g.title
                );
                continue;
            }
            describe(g);
            let lines = lines.get_or_insert_with(Lines::spawn);
            loop {
                ask(g);
                let Some(line) = lines.next().await else {
                    eprintln!(
                        "\nno more input: run {run_id} waits; decide gate {} with `atomic gates`",
                        g.id
                    );
                    break;
                };
                // Decided from another terminal meanwhile.
                match gate::find(rt, &g.id).await {
                    Ok(Some(now)) if now.resolution.is_some() => {
                        eprintln!("gate {} was decided meanwhile", g.id);
                        break;
                    }
                    _ => {}
                }
                let r = match reply(g, &line) {
                    Ok(r) => r,
                    Err(why) => {
                        eprintln!("{why}");
                        continue;
                    }
                };
                match gates::decide_as_operator(
                    rt, state, g, r.verdict, r.comment, r.answer, None, None,
                )
                .await
                {
                    Ok(d) => {
                        let what = match &d.answer {
                            Some(a) => format!("answered {a}"),
                            None if d.verdict == Verdict::Approve => "approved".into(),
                            None => "rejected".into(),
                        };
                        eprintln!("gate {}: {what} by {}", g.id, d.by);
                        break;
                    }
                    Err(e) => {
                        eprintln!("not recorded: {e:#}");
                        if matches!(
                            e.downcast_ref::<gate::DecideError>(),
                            Some(gate::DecideError::Conflict { .. })
                        ) {
                            break;
                        }
                    }
                }
            }
        }
        tokio::time::sleep(POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn gate(question: Option<Value>) -> Gate {
        Gate {
            id: "r:g".into(),
            run: "r".into(),
            workflow: "w".into(),
            step: "g".into(),
            title: "Ship it?".into(),
            show: Value::Null,
            evidence: Vec::new(),
            proposal: String::new(),
            approvers: Vec::new(),
            tier: Default::default(),
            opened_at: 0,
            deadline: None,
            on_timeout: Default::default(),
            question: question.map(|answer| gate::Question {
                answer,
                remember: false,
            }),
            resolution: None,
        }
    }

    #[test]
    fn an_approval_is_a_or_r_with_a_comment() {
        let g = gate(None);
        assert_eq!(
            reply(&g, "a looks right").expect("approve"),
            Reply {
                verdict: Verdict::Approve,
                comment: Some("looks right".into()),
                answer: None
            }
        );
        assert_eq!(
            reply(&g, "reject").expect("reject").verdict,
            Verdict::Reject
        );
        assert!(reply(&g, "maybe").is_err());
    }

    #[test]
    fn a_choice_is_its_number_or_its_value_and_nothing_else() {
        let g = gate(Some(json!({"enum": ["stable", "beta"]})));
        assert_eq!(
            reply(&g, "2").expect("by number").answer,
            Some(json!("beta"))
        );
        assert_eq!(
            reply(&g, "stable").expect("by value").answer,
            Some(json!("stable"))
        );
        assert!(reply(&g, "nightly").is_err(), "not one of the choices");
        assert!(reply(&g, "3").is_err());
        assert_eq!(
            reply(&g, "/reject no").expect("refuse").verdict,
            Verdict::Reject
        );
        let free = gate(Some(json!({"type": "string", "maxLength": 5})));
        assert_eq!(reply(&free, "12").expect("text").answer, Some(json!("12")));
        assert!(reply(&free, "too long").is_err());
    }
}
