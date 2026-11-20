// SPDX-License-Identifier: Apache-2.0

//! Cassettes, with the eval runner and the scripted fake model:
//!
//! - recorded once, an atom's evals replay with no model call and the same
//!   results;
//! - editing only a `validate` rule still replays, with no model call;
//! - changing the instructions, the output schema or the model misses the
//!   recording, and the case fails saying how to record it;
//! - a recording holds no header, so no API key.

use super::*;
use crate::codex::{CodexPaths, ModelEndpoint};
use crate::eval::{AtomSubject, Case, Options, Settings, load_cases, run};
use crate::runtime::{Artifacts, CodexRuntime, Runtimes};
use crate::testing::{FakeModel, Reply};
use crate::typed::AtomConfig;
use atomic_schema::registry::Registry;
use serde_json::json;

fn runtimes(base_url: &str, model: &str, root: &Path) -> Runtimes {
    for sub in ["home", "cwd"] {
        std::fs::create_dir_all(root.join(sub)).expect("dir");
    }
    let endpoint = ModelEndpoint {
        base_url: base_url.into(),
        model: model.into(),
        api_key_env: Some("PATH".into()),
    };
    Runtimes {
        codex: CodexRuntime {
            paths: CodexPaths::without_helpers(),
            codex_home: root.join("home"),
            guard: crate::codex::StateGuard::NoProjectState,
            cwd: root.join("cwd"),
            endpoint: endpoint.clone(),
            defaults: AtomConfig::default(),
            artifacts: Artifacts::none(),
            meter: crate::usage::Metering::none(),
        },
        lite: crate::lite::LiteRuntime::new(endpoint, AtomConfig::default()),
        exec: Default::default(),
        clock: None,
    }
}

/// The test atom with `body` as its instructions and `config`, and two
/// cases.
fn atom(root: &Path, body: &str, config: &str) -> crate::atom::Atom {
    let source = std::fs::read_to_string(root.join("test.atom.astro")).expect("source");
    let (head, _) = source.rsplit_once("---\n").expect("frontmatter");
    let head = head
        .lines()
        .map(|l| {
            if l.starts_with("export const config") {
                format!("export const config: AtomConfig<Input, Output> = {config};")
            } else {
                l.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(
        root.join("test.atom.astro"),
        format!("{head}\n---\n{body}\n"),
    )
    .expect("write");
    crate::atom::load(&root.join("test.atom.astro"), &mut Registry::new()).expect("loads")
}

async fn evals(
    atom: &crate::atom::Atom,
    root: &Path,
    rt: &Runtimes,
) -> (crate::eval::Report, Vec<Case>) {
    let manifest = root.join("atomic.yaml");
    if !manifest.exists() {
        std::fs::write(&manifest, "atomic: 1\n").expect("manifest");
    }
    let settings = Settings {
        dir: root.join("evals"),
        pass_rate: 1.0,
        project: crate::project::load(&manifest).expect("project"),
        declared: true,
    };
    let cases = load_cases(atom, &settings).expect("cases");
    let subject = AtomSubject { atom, runtimes: rt };
    (
        run(atom, cases.clone(), &settings, &subject, Options::default()).await,
        cases,
    )
}

#[tokio::test]
async fn recorded_evals_replay_offline_until_the_request_changes() {
    // The endpoint's key is read from an environment variable every process
    // has; its value must never reach a recording.
    let key = std::env::var("PATH").expect("PATH");
    let root = crate::atom::tests::project("Ticket for: {input.text}", "{}", None);
    std::fs::create_dir_all(root.join("evals")).expect("evals");
    std::fs::write(
        root.join("evals/cases.jsonl"),
        "{\"input\": {\"text\": \"Export crashes\"}, \"expect\": [\"output.ticket == 'BUG-1'\"]}\n\
         {\"input\": {\"text\": \"Add dark mode\"}, \"expect\": [\"output.ticket.size() > 0\"]}\n",
    )
    .expect("cases");
    let tapes = root.join("cassettes");
    let model = FakeModel::start(vec![
        Reply::Text(json!({"ticket": "BUG-1"}).to_string()),
        Reply::Text(json!({"ticket": "FEAT-2"}).to_string()),
    ])
    .await
    .expect("fake model");
    let upstream = model.base_url();
    let ticket = atom(&root, "Ticket for: {input.text}", "{}");

    // Recorded once, live.
    let recorder = Cassette::start(&tapes, &upstream, Mode::Record)
        .await
        .expect("cassette");
    let rt = runtimes(&recorder.base_url(&upstream), "synthetic", &root.join("rt"));
    let (live, _) = evals(&ticket, &root, &rt).await;
    assert!(live.passed, "{live:?}");
    assert_eq!(model.calls(), 2);
    assert_eq!(recorder.tally().recorded, 2);
    drop(recorder);
    // No header was recorded, so no key.
    for entry in std::fs::read_dir(&tapes).expect("tapes") {
        let text = std::fs::read_to_string(entry.expect("entry").path()).expect("tape");
        assert!(!text.contains(&key), "{text}");
        let tape: Recording = serde_json::from_str(&text).expect("a recording");
        assert_eq!(tape.path, "/v1/chat/completions");
    }

    let replay = |atom: crate::atom::Atom, model_id: &'static str| {
        let (root, tapes, upstream) = (root.clone(), tapes.clone(), upstream.clone());
        async move {
            let cassette = Cassette::start(&tapes, &upstream, Mode::Replay)
                .await
                .expect("cassette");
            let rt = runtimes(&cassette.base_url(&upstream), model_id, &root.join("rt"));
            let (report, _) = evals(&atom, &root, &rt).await;
            (report, cassette.tally())
        }
    };
    // Replayed: the same results, no model call.
    let (again, tally) = replay(ticket.clone(), "synthetic").await;
    assert_eq!(model.calls(), 2, "the replay made no model call");
    assert_eq!((tally.replayed, tally.missed), (2, 0));
    let outcomes = |r: &crate::eval::Report| {
        r.cases
            .iter()
            .map(|c| (c.id.clone(), c.passed, c.output.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(outcomes(&again), outcomes(&live));

    // A validate rule changes nothing that is sent: still replayed, and the
    // new rule is checked against the recorded outputs.
    let strict = atom(
        &root,
        "Ticket for: {input.text}",
        "{validate: [({output}) => output.ticket.length < 6]}",
    );
    let (checked, tally) = replay(strict, "synthetic").await;
    // The second answer breaks the new rule, and its repair turn, a request
    // never recorded, misses.
    assert_eq!((tally.replayed, tally.missed), (2, 1));
    assert_eq!(model.calls(), 2);
    assert!(checked.cases[0].passed, "{checked:?}");
    assert!(
        !checked.cases[1].passed,
        "the new rule rejects FEAT-2: {checked:?}"
    );

    // New instructions, or another model, miss every recording.
    for (changed, model_id) in [
        (
            atom(&root, "Open a ticket for: {input.text}", "{}"),
            "synthetic",
        ),
        (
            atom(&root, "Ticket for: {input.text}", "{}"),
            "another-model",
        ),
    ] {
        let (missed, tally) = replay(changed, model_id).await;
        assert_eq!((tally.replayed, tally.missed), (0, 2), "{model_id}");
        assert!(!missed.passed);
        assert!(
            missed.cases[0].failures[0].contains("run the eval with --live"),
            "{missed:?}"
        );
    }
    assert_eq!(model.calls(), 2, "no miss reached the model");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_identity_ignores_what_varies_between_runs_and_nothing_else() {
    let body = json!({"model": "m", "input": [{"role": "user", "content": "hi"}], "prompt_cache_key": "a"});
    let moved = json!({"prompt_cache_key": "b", "input": [{"content": "hi", "role": "user"}], "model": "m"});
    let (one, _) = identity("http://h/v1", "/v1/responses", &body);
    let (two, _) = identity("http://h/v1", "/v1/responses", &moved);
    assert_eq!(one, two, "key order and the cache key do not count");
    let other = json!({"model": "m2", "input": [{"role": "user", "content": "hi"}]});
    assert_ne!(one, identity("http://h/v1", "/v1/responses", &other).0);
    assert_ne!(
        one,
        identity("http://other/v1", "/v1/responses", &body).0,
        "the endpoint counts"
    );
}
