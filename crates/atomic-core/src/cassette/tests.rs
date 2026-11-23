// SPDX-License-Identifier: Apache-2.0

//! Cassettes, with the eval runner and the scripted fake model:
//!
//! - recorded once, an atom's evals replay with no model call and the same
//!   results;
//! - editing only a `validate` rule still replays, with no model call;
//! - changing the instructions, the output schema or the model misses the
//!   recording, and the case fails saying how to record it;
//! - a recording holds no header, so no API key, and names its endpoint with
//!   no credentials, query or default port;
//! - the same request twice is recorded twice and replayed in order;
//! - a recording that is truncated, a link, or for another request is
//!   reported corrupt, not missing; a cassette directory reached through a
//!   link is refused;
//! - a body that is not JSON is refused, and dropping the cassette stops
//!   every connection it was serving.

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
        sign_in: None,
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
            caches: None,
        },
        lite: crate::lite::LiteRuntime::new(endpoint, AtomConfig::default()),
        backend: Default::default(),
        exec_meter: Default::default(),
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
    let recorder = Cassette::start(&tapes, &root, &upstream, Mode::Record)
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
            let cassette = Cassette::start(&tapes, &root, &upstream, Mode::Replay)
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
    let mut tagged = moved.clone();
    tagged["metadata"] = json!({"run": "r-1"});
    tagged["user"] = json!("dev");
    tagged["client_metadata"] = json!({"x": 1});
    assert_eq!(
        one,
        identity("http://h/v1", "/v1/responses", &tagged).0,
        "nor do metadata, user and client_metadata"
    );
    let other = json!({"model": "m2", "input": [{"role": "user", "content": "hi"}]});
    assert_ne!(one, identity("http://h/v1", "/v1/responses", &other).0);
    // Every other field counts, a model setting sent with the request too.
    for (setting, value) in [
        ("max_output_tokens", json!(64)),
        ("temperature", json!(0.2)),
    ] {
        let mut set = body.clone();
        set[setting] = value;
        assert_ne!(
            one,
            identity("http://h/v1", "/v1/responses", &set).0,
            "{setting} counts"
        );
    }
    assert_ne!(
        one,
        identity("http://other/v1", "/v1/responses", &body).0,
        "the endpoint counts"
    );
}

#[test]
fn an_endpoint_is_recorded_without_credentials_query_or_default_port() {
    assert_eq!(
        normalize("HTTP://API.Example.COM:80/v1/").expect("plain"),
        "http://api.example.com/v1"
    );
    assert_eq!(
        normalize("https://api.example.com:443/v1").expect("plain"),
        "https://api.example.com/v1"
    );
    assert_eq!(
        normalize("http://127.0.0.1:8080/v1").expect("a port"),
        "http://127.0.0.1:8080/v1"
    );
    for bad in [
        "https://user:secret@api.example.com/v1",
        "https://token@api.example.com/v1",
        "https://api.example.com/v1?key=secret",
        "https://api.example.com/v1#x",
        "ftp://api.example.com/v1",
        "not a url",
    ] {
        assert!(normalize(bad).is_err(), "{bad}");
    }
}

/// A scratch project root with a cassette directory under it.
fn root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("atomic-cassette-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    std::fs::canonicalize(root).expect("root")
}

/// POST `body` to the cassette's `path`, with `key` as a bearer token.
async fn post(cassette: &Cassette, path: &str, body: &str, key: Option<&str>) -> (u16, String) {
    let mut request = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{}{path}", cassette.port))
        .header("content-type", "application/json")
        .body(body.to_owned());
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    let response = request.send().await.expect("the cassette answers");
    let status = response.status().as_u16();
    (status, response.text().await.expect("a body"))
}

const ASK: &str = r#"{"model": "m", "messages": [{"role": "user", "content": "hi"}]}"#;

#[tokio::test]
async fn cassette_redacts_bodies_but_replays_by_the_raw_request() {
    const INPUT: &str = concat!("sk-ant-", "api03-inputabcdefghijklmnop1234567890");
    const OUTPUT: &str = concat!("sk-ant-", "api03-outputabcdefghijklmnop1234567890");
    const OTHER: &str = concat!("sk-ant-", "api03-otherabcdefghijklmnop1234567890");
    const PATH: &str = "/v1/chat/completions";

    let root = root("redacted-bodies");
    let tapes = root.join("tapes");
    let model = FakeModel::start(vec![Reply::Text(
        json!({"ticket": "BUG-1", "note": OUTPUT}).to_string(),
    )])
    .await
    .expect("fake model");
    let upstream = model.base_url();
    let endpoint = normalize(&upstream).expect("endpoint");
    let ask = |secret: &str| {
        json!({"model": "m", "messages": [{"role": "user", "content": secret}]}).to_string()
    };
    let ask_input = ask(INPUT);
    let ask_other = ask(OTHER);
    let key = |body: &str| {
        identity(
            &endpoint,
            PATH,
            &serde_json::from_str(body).expect("request"),
        )
        .0
    };
    let input_file = file_for(&tapes, &key(&ask_input), 1);
    let other_file = file_for(&tapes, &key(&ask_other), 1);
    assert_ne!(input_file, other_file);

    let recorder = Cassette::start(&tapes, &root, &upstream, Mode::Record)
        .await
        .expect("recorder");
    let (status, live) = post(&recorder, PATH, &ask_input, None).await;
    assert_eq!(status, 200);
    assert!(live.contains(OUTPUT), "live response was changed");
    assert_eq!(model.requests()[0]["messages"][0]["content"], INPUT);
    assert_eq!(recorder.tally().recorded, 1);
    drop(recorder);

    let text = std::fs::read_to_string(&input_file).expect("recording at raw identity");
    assert!(
        !text.contains(INPUT),
        "request secret persisted in cassette"
    );
    assert!(
        !text.contains(OUTPUT),
        "response secret persisted in cassette"
    );
    let record: Value = serde_json::from_str(&text).expect("recording JSON");
    assert_eq!(
        record["redacted"], true,
        "the recording must mark redaction"
    );
    assert_eq!(record["request"]["messages"][0]["content"], "[REDACTED]");

    let player = Cassette::start(&tapes, &root, &upstream, Mode::Replay)
        .await
        .expect("player");
    let (status, replay) = post(&player, PATH, &ask_input, None).await;
    assert_eq!(status, 200);
    let response: Value = serde_json::from_str(&replay).expect("response JSON");
    let output = response["choices"][0]["message"]["content"]
        .as_str()
        .expect("model output");
    let output: Value = serde_json::from_str(output).expect("structured model output");
    assert_eq!(output, json!({"ticket": "BUG-1", "note": "[REDACTED]"}));
    assert_eq!(player.tally().replayed, 1);

    let (status, miss) = post(&player, PATH, &ask_other, None).await;
    assert_eq!(status, 502);
    assert!(miss.contains("no recording answers this request"));
    drop(player);

    let mut missing_key = record;
    missing_key
        .as_object_mut()
        .expect("object")
        .remove("request_sha256");
    std::fs::write(&input_file, missing_key.to_string()).expect("write recording");
    let player = Cassette::start(&tapes, &root, &upstream, Mode::Replay)
        .await
        .expect("player");
    let (status, corrupt) = post(&player, PATH, &ask_input, None).await;
    assert_eq!(status, 502);
    assert!(corrupt.contains("not this one"));
    drop(player);

    // Both requests sanitize to the same text. Copying a file into the
    // other request's slot must still be detected as corruption.
    std::fs::write(&other_file, text).expect("copy recording");
    let player = Cassette::start(&tapes, &root, &upstream, Mode::Replay)
        .await
        .expect("player");
    let (status, corrupt) = post(&player, PATH, &ask_other, None).await;
    assert_eq!(status, 502);
    assert!(corrupt.contains("not this one"));
    assert_eq!(player.tally().corrupt, 1);
    assert_eq!(model.calls(), 1, "replay contacted the model");
    std::fs::remove_dir_all(root).expect("remove test directory");
}

#[tokio::test]
async fn a_non_json_body_uses_text_redaction() {
    const SECRET: &str = concat!("sk-ant-", "api03-outputabcdefghijklmnop1234567890");
    let root = root("non-json-response");
    let tapes = root.join("tapes");
    let model = FakeModel::start(vec![Reply::Http {
        status: 200,
        body: format!("upstream text {SECRET}"),
    }])
    .await
    .expect("fake model");
    let upstream = model.base_url();
    let recorder = Cassette::start(&tapes, &root, &upstream, Mode::Record)
        .await
        .expect("recorder");
    let path = "/v1/chat/completions";
    assert_eq!(post(&recorder, path, ASK, None).await.0, 200);
    assert_eq!(recorder.tally().recorded, 1);
    drop(recorder);

    let key = identity(
        &upstream,
        path,
        &serde_json::from_str(ASK).expect("request"),
    )
    .0;
    let fixture = std::fs::read_to_string(file_for(&tapes, &key, 1)).expect("recording");
    assert!(
        !fixture.contains(SECRET),
        "response secret persisted in text cassette"
    );
    let recording: Recording = serde_json::from_str(&fixture).expect("recording JSON");
    assert!(
        recording.redacted,
        "response redaction alone must be marked"
    );
    let player = Cassette::start(&tapes, &root, &upstream, Mode::Replay)
        .await
        .expect("player");
    assert_eq!(
        post(&player, path, ASK, None).await,
        (200, "upstream text [REDACTED]".into())
    );
    assert_eq!(model.calls(), 1);
    std::fs::remove_dir_all(root).expect("remove test directory");
}

#[tokio::test]
async fn valid_json_that_cannot_be_inspected_is_not_recorded_as_text() {
    let root = root("uninspectable-json");
    let tapes = root.join("tapes");
    let body = format!(
        "{}{}{}",
        "[".repeat(140),
        r#"{"note":"synthetic fixture"}"#,
        "]".repeat(140)
    );
    let model = FakeModel::start(vec![Reply::Http { status: 200, body }])
        .await
        .expect("fake model");
    let upstream = model.base_url();
    let recorder = Cassette::start(&tapes, &root, &upstream, Mode::Record)
        .await
        .expect("recorder");
    let (status, reply) = post(&recorder, "/v1/chat/completions", ASK, None).await;
    assert_eq!(
        std::fs::read_dir(&tapes).expect("tapes").count(),
        0,
        "uninspectable response was written as text"
    );
    assert_eq!(status, 502);
    assert!(reply.contains("response JSON cannot be inspected"));
    assert!(!reply.contains("synthetic fixture"));
    assert_eq!(recorder.tally().recorded, 0);
    assert_eq!(model.calls(), 1);
    std::fs::remove_dir_all(root).expect("remove test directory");
}

#[tokio::test]
async fn a_repeated_classifier_cannot_leave_a_sensitive_response_value_in_a_cassette() {
    let root = root("repeat-classifier-response");
    let tapes = root.join("tapes");
    let model = FakeModel::start(vec![Reply::Http {
        status: 200,
        body: r#"{"mock":{"key":"DB_PASSWORD","value":"plain fixture text","k\u0065y":"label"}}"#
            .into(),
    }])
    .await
    .expect("fake model");
    let upstream = model.base_url();
    let recorder = Cassette::start(&tapes, &root, &upstream, Mode::Record)
        .await
        .expect("recorder");
    let path = "/v1/chat/completions";
    let (status, reply) = post(&recorder, path, ASK, None).await;
    for entry in std::fs::read_dir(&tapes).expect("tapes") {
        let fixture = std::fs::read_to_string(entry.expect("entry").path()).expect("fixture");
        assert!(
            !fixture.contains("plain fixture text"),
            "the fixture stored the surviving sibling of a repeated classifier"
        );
    }
    assert_eq!(status, 502);
    assert!(reply.contains("response JSON contains a repeated object key"));
    assert_eq!(recorder.tally().recorded, 0);
    drop(recorder);

    let player = Cassette::start(&tapes, &root, &upstream, Mode::Replay)
        .await
        .expect("player");
    let (status, miss) = post(&player, path, ASK, None).await;
    assert_eq!(status, 502);
    assert!(miss.contains("no recording answers this request"));
    assert_eq!(player.tally().missed, 1);
    assert_eq!(model.calls(), 1);
    std::fs::remove_dir_all(root).expect("remove test directory");
}

#[tokio::test]
async fn a_repeated_classifier_in_a_request_is_refused_in_both_modes() {
    let root = root("repeat-classifier-request");
    let tapes = root.join("tapes");
    let model = FakeModel::start(vec![Reply::Text("answer".into())])
        .await
        .expect("fake model");
    let upstream = model.base_url();
    let path = "/v1/chat/completions";
    let request = r#"{"model":"m","mock":{"key":"DB_PASSWORD","value":"plain fixture text","k\u0065y":"label"}}"#;
    for mode in [Mode::Record, Mode::Replay] {
        let cassette = Cassette::start(&tapes, &root, &upstream, mode)
            .await
            .expect("cassette");
        let (status, reply) = post(&cassette, path, request, None).await;
        assert_eq!(status, 400, "{mode:?}");
        assert!(reply.contains("request JSON contains a repeated object key"));
        assert_eq!(cassette.tally(), Tally::default());
    }
    assert_eq!(model.calls(), 0);
    assert_eq!(std::fs::read_dir(&tapes).expect("tapes").count(), 0);
    std::fs::remove_dir_all(root).expect("remove test directory");
}

#[tokio::test]
async fn a_repeated_classifier_inside_a_model_string_is_redacted_for_replay() {
    let root = root("nested-repeat-classifier");
    let tapes = root.join("tapes");
    let model = FakeModel::start(vec![Reply::Text(
        r#"{"key":"DB_PASSWORD","value":"plain fixture text","k\u0065y":"label"}"#.into(),
    )])
    .await
    .expect("fake model");
    let upstream = model.base_url();
    let recorder = Cassette::start(&tapes, &root, &upstream, Mode::Record)
        .await
        .expect("recorder");
    let path = "/v1/chat/completions";
    let (status, body) = post(&recorder, path, ASK, None).await;
    assert_eq!(status, 200);
    assert!(
        body.contains("plain fixture text"),
        "the live response was changed"
    );
    drop(recorder);

    let key = identity(
        &upstream,
        path,
        &serde_json::from_str(ASK).expect("request"),
    )
    .0;
    let fixture = std::fs::read_to_string(file_for(&tapes, &key, 1)).expect("recording");
    assert!(
        !fixture.contains("plain fixture text"),
        "the fixture stored the surviving sibling of a nested repeated classifier"
    );
    let recording: Recording = serde_json::from_str(&fixture).expect("recording JSON");
    assert!(recording.redacted);
    let player = Cassette::start(&tapes, &root, &upstream, Mode::Replay)
        .await
        .expect("player");
    assert_eq!(post(&player, path, ASK, None).await, (200, recording.body));
    assert_eq!(player.tally().replayed, 1);
    assert_eq!(model.calls(), 1);
    std::fs::remove_dir_all(root).expect("remove test directory");
}

#[tokio::test]
async fn the_same_request_twice_is_recorded_twice_and_replayed_in_order() {
    let root = root("repeat");
    let tapes = root.join("evals/cassettes");
    let model = FakeModel::start(vec![Reply::Text("one".into()), Reply::Text("two".into())])
        .await
        .expect("fake model");
    let upstream = model.base_url();
    // A temporary file a cut-short recording left is cleared away.
    std::fs::create_dir_all(&tapes).expect("tapes");
    let stale = tapes.join(".abc.json.0123456789abcdef.tmp");
    std::fs::write(&stale, "half").expect("stale");
    // A file of someone else's that only looks temporary is kept.
    let theirs = tapes.join(".notes.tmp");
    std::fs::write(&theirs, "mine").expect("theirs");
    let recorder = Cassette::start(&tapes, &root, &upstream, Mode::Record)
        .await
        .expect("cassette");
    assert!(!stale.exists(), "a stale temporary file is left");
    assert!(theirs.exists(), "a file not the cassette's was removed");
    let first = post(&recorder, "/v1/chat/completions", ASK, None).await;
    let second = post(&recorder, "/v1/chat/completions", ASK, None).await;
    assert_ne!(first.1, second.1, "two answers to the same request");
    assert_eq!(recorder.tally().recorded, 2);
    drop(recorder);
    let endpoint = normalize(&upstream).expect("endpoint");
    let path = "/v1/chat/completions";
    let (key, _) = identity(
        &endpoint,
        path,
        &serde_json::from_str(ASK).expect("request"),
    );
    let file = file_for(&tapes, &key, 1);
    let mut fixture: Value =
        serde_json::from_str(&std::fs::read_to_string(&file).expect("tape")).expect("JSON");
    assert_eq!(fixture["redacted"], false);
    let fields = fixture.as_object_mut().expect("object");
    fields.remove("redacted");
    fields.remove("request_sha256");
    std::fs::write(file, fixture.to_string()).expect("legacy recording");
    let player = Cassette::start(&tapes, &root, &upstream, Mode::Replay)
        .await
        .expect("cassette");
    assert_eq!(
        post(&player, "/v1/chat/completions", ASK, None).await,
        first
    );
    assert_eq!(
        post(&player, "/v1/chat/completions", ASK, None).await,
        second
    );
    // A third has no recording, and the miss names the endpoint.
    let (status, text) = post(&player, "/v1/chat/completions", ASK, None).await;
    assert_eq!(status, 502);
    assert!(
        text.contains(&normalize(&upstream).expect("endpoint")),
        "{text}"
    );
    assert_eq!((player.tally().replayed, player.tally().missed), (2, 1));
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_bad_recording_is_corrupt_not_missing_and_links_are_refused() {
    let root = root("corrupt");
    let tapes = root.join("cassettes");
    let model = FakeModel::start(vec![Reply::Text("one".into()), Reply::Text("two".into())])
        .await
        .expect("fake model");
    let upstream = model.base_url();
    let recorder = Cassette::start(&tapes, &root, &upstream, Mode::Record)
        .await
        .expect("cassette");
    post(&recorder, "/v1/chat/completions", ASK, None).await;
    drop(recorder);
    let tape = std::fs::read_dir(&tapes)
        .expect("tapes")
        .map(|e| e.expect("entry").path())
        .find(|p| p.extension().is_some_and(|e| e == "json"))
        .expect("a recording");
    let whole = std::fs::read_to_string(&tape).expect("tape");
    let replayed = |expect: &'static str| {
        let (tapes, root, upstream) = (tapes.clone(), root.clone(), upstream.clone());
        async move {
            let player = Cassette::start(&tapes, &root, &upstream, Mode::Replay)
                .await
                .expect("cassette");
            let (status, text) = post(&player, "/v1/chat/completions", ASK, None).await;
            assert_eq!(status, 502, "{expect}: {text}");
            assert!(text.contains(expect), "{text}");
            assert_eq!(
                (player.tally().corrupt, player.tally().missed),
                (1, 0),
                "{expect}"
            );
        }
    };
    // Truncated, as a write cut short would leave it.
    std::fs::write(&tape, &whole[..whole.len() / 2]).expect("truncate");
    replayed("is corrupt").await;
    // Whole, but for another request.
    let mut other: Recording = serde_json::from_str(&whole).expect("recording");
    other.request = json!({"model": "other"});
    std::fs::write(&tape, serde_json::to_string(&other).expect("json")).expect("write");
    replayed("not this one").await;
    // A link in its place.
    let elsewhere = root.join("elsewhere.json");
    std::fs::write(&elsewhere, &whole).expect("write");
    std::fs::remove_file(&tape).expect("remove");
    std::os::unix::fs::symlink(&elsewhere, &tape).expect("link");
    replayed("a link").await;
    let recorder = Cassette::start(&tapes, &root, &upstream, Mode::Record)
        .await
        .expect("recorder");
    let (status, reply) = post(&recorder, "/v1/chat/completions", ASK, None).await;
    assert_eq!(status, 500);
    assert!(reply.contains("it is a link or not a file"));
    assert_eq!(recorder.tally().recorded, 0);
    assert_eq!(std::fs::read_to_string(&elsewhere).expect("target"), whole);
    drop(recorder);
    // A cassette directory reached through a link is refused outright.
    let linked = root.join("linked");
    std::os::unix::fs::symlink(&tapes, &linked).expect("link");
    for mode in [Mode::Replay, Mode::Record] {
        assert!(
            Cassette::start(&linked, &root, &upstream, mode)
                .await
                .is_err(),
            "{mode:?}"
        );
        // And one that does not exist yet, under a link, is never made
        // through it.
        assert!(
            Cassette::start(&linked.join("new"), &root, &upstream, mode)
                .await
                .is_err(),
            "{mode:?}"
        );
    }
    assert!(!tapes.join("new").exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_body_that_is_not_json_is_refused_and_a_key_in_replay_is_counted() {
    let root = root("body");
    let player = Cassette::start(
        &root.join("tapes"),
        &root,
        "http://127.0.0.1:9/v1",
        Mode::Replay,
    )
    .await
    .expect("cassette");
    let (status, text) = post(&player, "/v1/chat/completions", "not json", None).await;
    assert_eq!(status, 400, "{text}");
    let (status, _) = post(&player, "/v1/chat/completions", ASK, Some("secret")).await;
    assert_eq!(status, 502);
    assert_eq!(player.tally().keyed, 1, "the key reached the replay");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn dropping_the_cassette_stops_its_connections() {
    let root = root("drop");
    let player = Cassette::start(
        &root.join("tapes"),
        &root,
        "http://127.0.0.1:9/v1",
        Mode::Replay,
    )
    .await
    .expect("cassette");
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", player.port))
        .await
        .expect("connect");
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{ASK}",
        ASK.len()
    );
    let mut answer = vec![0u8; 4096];
    socket.write_all(request.as_bytes()).await.expect("send");
    assert!(socket.read(&mut answer).await.expect("an answer") > 0);
    drop(player);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    // The connection it was serving is closed, not served on.
    let _ = socket.write_all(request.as_bytes()).await;
    let read = tokio::time::timeout(std::time::Duration::from_secs(5), socket.read(&mut answer))
        .await
        .expect("not left hanging");
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
    let _ = std::fs::remove_dir_all(&root);
}

/// Recording forwards the atom's key to the real endpoint with the client a
/// lite atom calls with, so a redirect the endpoint answers is passed back,
/// never followed with the key, and nothing is recorded.
#[tokio::test]
async fn recording_never_follows_a_redirect_with_the_key() {
    use std::io::{BufRead, Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let answering = |response: String| {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let seen = Arc::new(AtomicUsize::new(0));
        let counted = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                counted.fetch_add(1, Ordering::SeqCst);
                // The request head, then its body, read whole before
                // answering, so the answer is never cut off by a reset.
                let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone"));
                let mut line = String::new();
                let mut length = 0;
                while reader.read_line(&mut line).is_ok_and(|n| n > 0) && line != "\r\n" {
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap_or(0);
                    }
                    line.clear();
                }
                let _ = reader.read_exact(&mut vec![0; length]);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (format!("http://127.0.0.1:{}", address.port()), seen)
    };
    let (elsewhere, followed) =
        answering("HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}".to_owned());
    let (redirecting, asked) = answering(format!(
        "HTTP/1.1 307 Temporary Redirect\r\nlocation: {elsewhere}/v1/chat/completions\r\n\
         content-length: 0\r\n\r\n"
    ));
    let root = root("redirect");
    let tapes = root.join("cassettes");
    let recorder = Cassette::start(&tapes, &root, &format!("{redirecting}/v1"), Mode::Record)
        .await
        .expect("cassette");
    let (status, _) = post(&recorder, "/v1/chat/completions", ASK, Some("sk-the-key")).await;
    assert_eq!(status, 307, "the redirect is passed back");
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    assert_eq!(
        followed.load(Ordering::SeqCst),
        0,
        "the redirect was not followed"
    );
    assert_eq!(recorder.tally().recorded, 0);
    drop(recorder);
    let _ = std::fs::remove_dir_all(&root);
}
