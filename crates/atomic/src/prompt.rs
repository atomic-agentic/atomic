// SPDX-License-Identifier: Apache-2.0

//! The terminal prompt `atomic run` and `atomic runs resume` show while
//! their run waits at a gate.
//!
//! The person at the terminal decides there: approve or reject, with a
//! comment, or the answer to a question gate. The decision goes through
//! [`crate::gates::decide_as_operator`], with the operator's enrolled
//! credential, exactly as `atomic gates approve` from another terminal
//! would; the prompt is a person at a terminal, not a shortcut past the gate
//! store's checks.
//!
//! A decision counts only if the person saw the gate it decides:
//!
//! - typed-ahead input never decides a gate: before each gate is shown the
//!   terminal's pending input is discarded, and a line that arrived before
//!   the prompt was shown is dropped;
//! - commands atomic runs have no controlling terminal (they start in a
//!   session of their own), so none can type into this one;
//! - everything printed from a gate is made safe for the terminal first
//!   (see [`crate::shown`]);
//! - the prompt shows only when standard input and standard error are both
//!   a terminal and atomic is in the terminal's foreground; otherwise it
//!   says once per gate how to decide it from another terminal, and the run
//!   waits.

use crate::gates;
use crate::shown;
use atomic_core::redact::Line;
use atomic_core::state::ProjectState;
use atomic_durable::Durare;
use atomic_durable::gate::{self, DecideError, Gate, GateFilter, Verdict};
use std::collections::BTreeSet;
use std::io::IsTerminal;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How often the prompt looks for a newly opened gate, and checks the one
/// it shows.
const POLL: Duration = Duration::from_millis(250);

/// Whether this process may prompt now: its standard input and error are a
/// terminal, and it is in that terminal's foreground process group (one in
/// the background is stopped when it reads).
fn interactive() -> bool {
    if !(std::io::stdin().is_terminal() && atomic_core::redact::stderr_is_terminal()) {
        return false;
    }
    #[cfg(unix)]
    {
        // SAFETY: tcgetpgrp and getpgrp take no pointers.
        #[allow(unsafe_code)]
        unsafe {
            libc::tcgetpgrp(0) == libc::getpgrp()
        }
    }
    #[cfg(not(unix))]
    true
}

/// Discard input typed but not yet read.
fn flush_input() {
    #[cfg(unix)]
    // SAFETY: tcflush on standard input takes no pointers.
    #[allow(unsafe_code)]
    unsafe {
        libc::tcflush(0, libc::TCIFLUSH);
    }
}

/// Lines typed at the terminal, each with the time it arrived, read by a
/// thread of its own so a read still waiting when the run finishes does not
/// hold the process. It reads only while atomic is in the foreground, so it
/// is never stopped for reading in the background.
struct Lines(mpsc::Receiver<(Instant, String)>);

impl Lines {
    fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut pending = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                if !interactive() {
                    std::thread::sleep(POLL);
                    continue;
                }
                #[cfg(unix)]
                {
                    let mut fd = libc::pollfd {
                        fd: 0,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // SAFETY: one pollfd that lives across the call.
                    #[allow(unsafe_code)]
                    let ready = unsafe { libc::poll(&mut fd, 1, 200) };
                    if ready <= 0 {
                        continue;
                    }
                }
                // Straight from the descriptor: a buffer in between would
                // hold lines poll cannot see.
                #[cfg(unix)]
                // SAFETY: reads into a buffer that outlives the call, at most
                // its length.
                #[allow(unsafe_code)]
                let read = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
                #[cfg(not(unix))]
                let read: isize = {
                    use std::io::Read;
                    std::io::stdin()
                        .read(&mut buf)
                        .map_or(-1, |n| isize::try_from(n).unwrap_or(-1))
                };
                let n = match read {
                    0 => return,
                    n if n > 0 => usize::try_from(n).unwrap_or(0),
                    // EIO from the background, EINTR: wait and try again.
                    _ => {
                        std::thread::sleep(POLL);
                        continue;
                    }
                };
                pending.extend_from_slice(&buf[..n]);
                while let Some(end) = pending.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = pending.drain(..=end).collect();
                    let line = String::from_utf8_lossy(&line[..end]).trim_end().to_owned();
                    if tx.send((Instant::now(), line)).is_err() {
                        return;
                    }
                }
            }
        });
        Self(rx)
    }

    /// Drop every line read so far.
    fn drain(&self) {
        while self.0.try_recv().is_ok() {}
    }

    /// The next line that arrived after `since`, if one has; `Err` once
    /// standard input is closed.
    fn next_after(&self, since: Instant) -> Result<Option<String>, ()> {
        loop {
            match self.0.try_recv() {
                Ok((at, _)) if at < since => continue,
                Ok((_, line)) => return Ok(Some(line)),
                Err(mpsc::TryRecvError::Empty) => return Ok(None),
                Err(mpsc::TryRecvError::Disconnected) => return Err(()),
            }
        }
    }
}

/// What the person typed, as a decision.
#[derive(Debug, PartialEq)]
struct Reply {
    verdict: Verdict,
    comment: Option<String>,
    answer: Option<serde_json::Value>,
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
    // Read as `atomic gates answer` reads it.
    let answer = shown::answer(question, line);
    // Refused here for a quicker retry; the gate store checks it again.
    // Raw: the line it is logged as is redacted, then escaped.
    question.check(&answer).map_err(|why| {
        let typed = match &answer {
            serde_json::Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        format!("{typed} is not an answer to this question: {why}")
    })?;
    Ok(Reply {
        verdict: Verdict::Approve,
        comment: None,
        answer: Some(answer),
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// `v` for a person: redacted, then made safe for the terminal, and
/// whether redaction changed it.
fn redacted(v: &serde_json::Value) -> (String, bool) {
    let mut shown_value = v.clone();
    atomic_core::redact::value(&mut shown_value);
    let changed = shown_value != *v;
    (shown::value(&shown_value), changed)
}

/// [`redacted`], for text.
fn redacted_text(text: &str) -> (String, bool) {
    redacted(&serde_json::Value::String(text.to_owned()))
}

/// Show gate `g` and what to type, every part of it made safe. Each line is
/// written by itself, and each value in it redacted on its own: what the
/// title or one value holds, such as the start of a private key, hides
/// nothing else. atomic's own words and the gate's id are written as they
/// are.
fn describe(g: &Gate) {
    let mut hidden = false;
    Line::new()
        .plain("\ngate ")
        .shown(&shown::line(&g.id))
        .plain(" is waiting for you")
        .log();
    let label = if g.question.is_some() {
        "question: "
    } else {
        ""
    };
    let (title, cut) = redacted_text(&g.title);
    hidden |= cut;
    Line::new().plain("  ").plain(label).shown(&title).log();
    if !g.show.is_null() {
        let mut show = g.show.clone();
        atomic_core::redact::value(&mut show);
        hidden |= show != g.show;
        let shows = serde_json::to_string_pretty(&show).unwrap_or_default();
        for (i, l) in shows.split('\n').enumerate() {
            Line::new()
                .plain(if i == 0 { "  shows: " } else { "  " })
                .shown(&shown::line(l))
                .log();
        }
    }
    if let Some(q) = &g.question {
        if let Some(choices) = q.choices() {
            for (i, c) in choices.iter().enumerate() {
                let (choice, cut) = redacted(c);
                hidden |= cut;
                Line::new()
                    .plain("  ")
                    .shown(&shown::label(i))
                    .plain(") ")
                    .shown(&choice)
                    .log();
            }
        }
        if let (Some(default), Some(_)) = (&q.default, g.deadline) {
            let (default, cut) = redacted(default);
            hidden |= cut;
            Line::new()
                .plain("  silence selects ")
                .shown(&default)
                .log();
        }
    }
    if hidden {
        Line::new()
            .shown(&format!(
                "  something here is shown as {}; `atomic gates list` shows it in full",
                atomic_core::redact::REDACTED
            ))
            .log();
    }
    if let Some(deadline) = g.deadline {
        let left = (deadline - now_ms()).max(0) / 1000;
        Line::new().shown(&format!("  it waits {left}s more")).log();
    }
}

/// Ask for a decision on `g`, the cursor left after the prompt.
fn ask(g: &Gate) {
    Line::new()
        .plain(if g.question.is_some() {
            "answer with a value or its letter (/reject to refuse) > "
        } else {
            "approve or reject: `a` or `r`, then a comment if you like > "
        })
        .prompt();
}

/// A fault a person is told about once: the gate store is read again and
/// again while a gate is shown, and one that outlives a single read must
/// not be written on every pass. The same fault again is silence; a
/// different one, or the store answering, lets it be said again.
#[derive(Default)]
struct Notice {
    said: Option<String>,
}

impl Notice {
    /// Whether `what` is not the fault already said, and remember it as
    /// said.
    fn is_new(&mut self, what: &str) -> bool {
        if self.said.as_deref() == Some(what) {
            return false;
        }
        self.said = Some(what.to_owned());
        true
    }

    /// The store answered: a fault that comes back is news again.
    fn clear(&mut self) {
        self.said = None;
    }
}

/// Why the prompt for a gate ended.
enum Done {
    /// Its decision is recorded, or it was resolved elsewhere.
    Settled,
    /// It cannot be decided here; decide it with `atomic gates`.
    Elsewhere,
}

/// Prompt for `g` until it is decided, here or elsewhere, or passes its
/// deadline.
async fn prompt(rt: &Durare, state: &ProjectState, g: &Gate, lines: &Lines) -> Done {
    // What was typed before the gate was shown decides nothing.
    flush_input();
    lines.drain();
    describe(g);
    // A store that keeps failing is said once, not on every pass.
    let mut failing = Notice::default();
    loop {
        // Taken before the prompt is written: a reply to it arrives after
        // the write, so it is never older than this, however long this
        // thread waits between writing the prompt and reading on. Taken
        // after, a reply could arrive in that wait and be dropped as typed
        // ahead, leaving the person typing at a prompt that never answers.
        let since = Instant::now();
        ask(g);
        atomic_core::failpoint::pause("prompt:after-ask");
        let line = loop {
            match lines.next_after(since) {
                Ok(Some(line)) => break line,
                Err(()) => {
                    atomic_core::log_line!("\nno more input");
                    return Done::Elsewhere;
                }
                Ok(None) => {}
            }
            // Decided from another terminal, or past its deadline, while
            // the person looks at it.
            match gate::find(rt, &g.id).await {
                Ok(Some(now)) if now.resolution.is_some() => {
                    Line::new()
                        .plain("\ngate ")
                        .shown(&shown::line(&g.id))
                        .plain(" was resolved meanwhile")
                        .log();
                    return Done::Settled;
                }
                Ok(_) => failing.clear(),
                Err(e) => {
                    let e = e.to_string();
                    if failing.is_new(&e) {
                        atomic_core::log_line!("\nthe gate store: {e}");
                    }
                }
            }
            if g.deadline.is_some_and(|d| now_ms() >= d) {
                Line::new()
                    .plain("\ngate ")
                    .shown(&shown::line(&g.id))
                    .plain(": its deadline passed")
                    .log();
                return Done::Settled;
            }
            if !interactive() {
                return Done::Elsewhere;
            }
            tokio::time::sleep(POLL / 5).await;
        };
        let r = match reply(g, &line) {
            Ok(r) => r,
            Err(why) => {
                atomic_core::log_line!("{why}");
                continue;
            }
        };
        match gates::decide_as_operator(rt, state, g, r.verdict, r.comment, r.answer, None, None)
            .await
        {
            Ok(d) => {
                let what = match &d.answer {
                    Some(a) => format!("answered {}", redacted(a).0),
                    None if d.verdict == Verdict::Approve => "approved".into(),
                    None => "rejected".into(),
                };
                Line::new()
                    .plain("gate ")
                    .shown(&shown::line(&g.id))
                    .plain(": ")
                    .shown(&what)
                    .plain(" by ")
                    .shown(&shown::line(&d.by.to_string()))
                    .log();
                return Done::Settled;
            }
            Err(e) => {
                Line::new()
                    .plain("not recorded: ")
                    .shown(&redacted_text(&format!("{e:#}")).0)
                    .log();
                match e.downcast_ref::<DecideError>() {
                    Some(DecideError::Conflict { .. }) => return Done::Settled,
                    // Nothing typed here can change that.
                    Some(DecideError::NotAnApprover { .. } | DecideError::NoSuchGate(_)) => {
                        return Done::Elsewhere;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Prompt for every gate run `run_id` opens, for as long as it runs. Never
/// returns; the caller stops it when the run ends or a signal stops it.
pub async fn at_gates(rt: &Durare, state: &ProjectState, run_id: &str) {
    let mut done: BTreeSet<String> = BTreeSet::new();
    let mut lines: Option<Lines> = None;
    let mut failing = Notice::default();
    loop {
        let open = match gate::gates(
            rt,
            GateFilter {
                open_only: true,
                run: Some(run_id.to_owned()),
            },
        )
        .await
        {
            Ok(open) => {
                failing.clear();
                open
            }
            Err(e) => {
                // Said once, not every poll.
                let said = e.to_string();
                if failing.is_new(&said) {
                    atomic_core::log_line!("cannot read the gate store: {said}");
                }
                Vec::new()
            }
        };
        let new: Vec<&Gate> = open.iter().filter(|g| !done.contains(&g.id)).collect();
        for g in new {
            done.insert(g.id.clone());
            let settled = if interactive() {
                let lines = lines.get_or_insert_with(Lines::spawn);
                matches!(prompt(rt, state, g, lines).await, Done::Settled)
            } else {
                false
            };
            if !settled {
                let how = if g.question.is_some() {
                    format!("`atomic gates answer {} <answer>`", shown::line(&g.id))
                } else {
                    format!(
                        "`atomic gates approve {id}` or `atomic gates reject {id}`",
                        id = shown::line(&g.id)
                    )
                };
                // The title last, and redacted on its own: nothing in it can
                // hide how to decide the gate.
                Line::new()
                    .plain("run ")
                    .shown(&shown::line(run_id))
                    .plain(" waits at gate ")
                    .shown(&shown::line(&g.id))
                    .plain(": decide it from a terminal with ")
                    .shown(&how)
                    .plain("; it asks: ")
                    .shown(&redacted_text(&g.title).0)
                    .log();
            }
        }
        tokio::time::sleep(POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

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
                default: None,
            }),
            resolution: None,
        }
    }

    /// A store that stays broken is told once, not on every 50 ms pass, and
    /// can be told again once it changes or answers.
    #[test]
    fn a_store_fault_is_said_once_until_it_changes_or_clears() {
        let mut failing = Notice::default();
        assert!(
            failing.is_new("database is locked"),
            "the first pass says it"
        );
        assert!(
            !failing.is_new("database is locked"),
            "the same fault on the next pass is silence"
        );
        assert!(
            failing.is_new("no such table: gate"),
            "a different fault is news again"
        );
        assert!(!failing.is_new("no such table: gate"));
        failing.clear();
        assert!(
            failing.is_new("no such table: gate"),
            "after the store answers, the fault coming back is news"
        );
        assert!(!failing.is_new("no such table: gate"));
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
    fn a_choice_is_its_value_or_its_letter_and_nothing_else() {
        let g = gate(Some(json!({"enum": ["stable", "beta"]})));
        assert_eq!(
            reply(&g, "b").expect("by letter").answer,
            Some(json!("beta"))
        );
        assert_eq!(
            reply(&g, "stable").expect("by value").answer,
            Some(json!("stable"))
        );
        assert!(reply(&g, "nightly").is_err(), "not one of the choices");
        assert!(reply(&g, "c").is_err());
        assert_eq!(
            reply(&g, "/reject no").expect("refuse").verdict,
            Verdict::Reject
        );
        let numbers = gate(Some(json!({"enum": [10, 20, 1]})));
        assert_eq!(reply(&numbers, "1").expect("value").answer, Some(json!(1)));
        let free = gate(Some(json!({"type": "string", "maxLength": 5})));
        assert_eq!(reply(&free, "12").expect("text").answer, Some(json!("12")));
        assert!(reply(&free, "too long").is_err());
    }
}
