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
            backend: Default::default(),
            caches: None,
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
    let model = FakeModel::start(vec![Reply::Text("one".into())])
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
