// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn dir(files: &[(&str, &str)]) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "atomic-settings-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    for (name, text) in files {
        std::fs::write(dir.join(name), text).expect("file");
    }
    dir
}

const MANIFEST_ONLY: &str = "atomic: 1\nname: notes\n";

/// Resolve in `dir` with exactly `vars` set and `ollama` answering.
fn resolved(
    dir: &Path,
    args: &EngineArgs,
    vars: &[(&str, &str)],
    ollama: &dyn Fn(SocketAddr) -> Option<Vec<String>>,
) -> Resolved {
    let vars: BTreeMap<String, String> = vars
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    let env = |name: &str| vars.get(name).cloned();
    resolve(
        dir,
        args,
        &Surroundings {
            env: &env,
            env_names: vars.keys().cloned().collect(),
            ollama,
        },
    )
    .expect("resolves")
}

fn nobody(_: SocketAddr) -> Option<Vec<String>> {
    None
}

fn source<'a>(r: &'a Resolved, path: &str) -> &'a Source {
    r.sources
        .get(path)
        .unwrap_or_else(|| panic!("no source for {path}"))
}

#[test]
fn with_nothing_around_nothing_is_detected() {
    let d = dir(&[("atomic.yaml", MANIFEST_ONLY)]);
    let r = resolved(&d, &EngineArgs::default(), &[], &nobody);
    assert!(r.endpoint().is_none());
    assert_eq!(source(&r, "model.base_url"), &Source::Default);
    assert_eq!(r.settings.runner, "local");
    assert_eq!(r.settings.database.schema, "atomic");
}

#[test]
fn an_openai_key_makes_openai_the_endpoint_without_showing_the_key() {
    let d = dir(&[("atomic.yaml", MANIFEST_ONLY)]);
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[("OPENAI_API_KEY", "sk-secret-value")],
        &|_| panic!("no probe when a key is set"),
    );
    let endpoint = r.endpoint().expect("an endpoint");
    assert_eq!(endpoint.base_url, OPENAI_BASE_URL);
    assert_eq!(endpoint.model, OPENAI_DEFAULT_MODEL);
    assert_eq!(endpoint.api_key_env.as_deref(), Some("OPENAI_API_KEY"));
    for path in ["model.base_url", "model.name", "model.api_key_env"] {
        assert!(
            matches!(source(&r, path), Source::Detected(why) if why.contains("OPENAI_API_KEY")),
            "{path}: {:?}",
            source(&r, path)
        );
    }
    let env = |name: &str| (name == "OPENAI_API_KEY").then(|| "sk-secret-value".to_owned());
    let shown = format!("{:?}", r.explain(&env));
    assert!(!shown.contains("sk-secret-value"), "{shown}");
    assert!(
        shown.contains("OPENAI_API_KEY (set; its value is not shown)"),
        "{shown}"
    );
}

#[test]
fn a_local_ollama_is_the_endpoint_when_no_key_is_set() {
    let d = dir(&[("atomic.yaml", MANIFEST_ONLY)]);
    let asked = AtomicUsize::new(0);
    let ollama = |addr: SocketAddr| {
        asked.fetch_add(1, Ordering::Relaxed);
        assert_eq!(addr.to_string(), OLLAMA_ADDRESS);
        Some(vec!["qwen3:8b".to_owned(), "llama3.2:latest".to_owned()])
    };
    let r = resolved(&d, &EngineArgs::default(), &[], &ollama);
    assert_eq!(asked.load(Ordering::Relaxed), 1);
    let endpoint = r.endpoint().expect("an endpoint");
    assert_eq!(endpoint.base_url, "http://127.0.0.1:11434/v1");
    assert_eq!(endpoint.model, "qwen3:8b");
    assert_eq!(endpoint.api_key_env, None);
    assert!(
        matches!(source(&r, "model.name"), Source::Detected(why) if why.contains("first model"))
    );
}

#[test]
fn detection_probes_this_machine_only() {
    let d = dir(&[("atomic.yaml", MANIFEST_ONLY)]);
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[(OLLAMA_ADDRESS_VARIABLE, "10.0.0.5:11434")],
        &|_| panic!("a non-loopback address is never probed"),
    );
    assert!(r.endpoint().is_none());
    assert!(
        r.notes.iter().any(|n| n.contains("not a loopback address")),
        "{:?}",
        r.notes
    );
}

#[test]
fn what_is_set_wins_over_detection_and_each_says_where_it_came_from() {
    let d = dir(&[(
        "atomic.yaml",
        "atomic: 1\nname: notes\nmodel:\n  base_url: http://127.0.0.1:8080/v1\n  name: from-file\n",
    )]);
    // A file sets the endpoint: no detection, though a key is set.
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[("OPENAI_API_KEY", "sk-x")],
        &|_| panic!("no probe when the endpoint is set"),
    );
    assert_eq!(
        r.endpoint().map(|e| (e.base_url, e.model)),
        Some(("http://127.0.0.1:8080/v1".into(), "from-file".into()))
    );
    assert!(matches!(
        source(&r, "model.base_url"),
        Source::File { line: 4, .. }
    ));
    // A variable beats the file; ATOMIC_MODEL names the model.
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[
            ("ATOMIC_MODEL", "from-env"),
            ("ATOMIC_MODEL_BASE_URL", "http://127.0.0.1:9000/v1"),
        ],
        &nobody,
    );
    assert_eq!(r.settings.model.name.as_deref(), Some("from-env"));
    assert_eq!(
        source(&r, "model.name"),
        &Source::Env("ATOMIC_MODEL".into())
    );
    assert_eq!(
        source(&r, "model.base_url"),
        &Source::Env("ATOMIC_MODEL_BASE_URL".into())
    );
    // A flag beats them both.
    let args = EngineArgs {
        model: Some("from-flag".into()),
        ..EngineArgs::default()
    };
    let r = resolved(&d, &args, &[("ATOMIC_MODEL", "from-env")], &nobody);
    assert_eq!(r.settings.model.name.as_deref(), Some("from-flag"));
    assert_eq!(source(&r, "model.name"), &Source::Flag("--model".into()));
}

#[test]
fn the_store_is_atomics_own_variable_never_database_url_and_its_password_is_masked() {
    let d = dir(&[("atomic.yaml", MANIFEST_ONLY)]);
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[("DATABASE_URL", "postgres://app:apppass@db/app")],
        &nobody,
    );
    assert_eq!(r.settings.database.url, None);
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("DATABASE_URL is set and ignored")),
        "{:?}",
        r.notes
    );
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[(
            "ATOMIC_DATABASE_URL",
            "postgres://atomic:hunter2@db:5432/atomic",
        )],
        &nobody,
    );
    let shown = format!("{:?}", r.explain(&|_| None));
    assert!(!shown.contains("hunter2"), "{shown}");
    assert!(shown.contains("postgres://***@db:5432/atomic"), "{shown}");
}

#[test]
fn every_setting_has_a_source_and_the_manifest_keys_are_not_settings() {
    let d = dir(&[("atomic.yaml", "atomic: 1\nname: notes\ngenerated: .gen\n")]);
    let r = resolved(&d, &EngineArgs::default(), &[], &nobody);
    let shown = r.explain(&|_| None);
    let paths: Vec<&str> = shown.iter().map(|(p, _, _)| p.as_str()).collect();
    for path in [
        "model.base_url",
        "model.name",
        "model.api_key_env",
        "database.url",
        "database.schema",
        "runner",
        "executor_id",
        "budget.atom.tokens",
        "budget.workflow.minutes",
        "budget.call.input_tokens",
    ] {
        assert!(paths.contains(&path), "{path} is explained: {paths:?}");
    }
    assert!(
        !paths
            .iter()
            .any(|p| p.starts_with("generated") || *p == "name"),
        "{paths:?}"
    );
    assert!(shown.iter().all(|(_, _, source)| !source.is_empty()));
}

#[test]
fn a_url_is_shown_without_its_user_password_or_query_and_never_if_it_does_not_parse() {
    for (url, shown) in [
        (
            "postgres://atomic:secret@db:5432/atomic",
            "postgres://***@db:5432/atomic",
        ),
        (
            "postgres://db/atomic?password=secret",
            "postgres://db/atomic?***",
        ),
        (
            "postgres://atomic@db?password=secret",
            "postgres://***@db?***",
        ),
        (
            "postgresql://:secret@db/atomic",
            "postgresql://***@db/atomic",
        ),
        (
            "https://user:secret@api.example.com/v1?key=secret",
            "https://***@api.example.com/v1?***",
        ),
        ("https://api.openai.com/v1", "https://api.openai.com/v1"),
        ("http://127.0.0.1:11434/v1", "http://127.0.0.1:11434/v1"),
        // No scheme, or not a URL at all: nothing of it is shown.
        ("atomic:secret@db/atomic", NOT_SHOWN),
        ("host=db user=atomic password=secret", NOT_SHOWN),
        ("secret", NOT_SHOWN),
    ] {
        assert_eq!(mask_url(url), shown, "{url}");
    }
}

#[test]
fn explain_masks_the_endpoint_and_shows_the_key_variable_only_by_a_name() {
    let d = dir(&[("atomic.yaml", MANIFEST_ONLY)]);
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[
            (
                "ATOMIC_MODEL_BASE_URL",
                "https://me:tok-secret@api.example.com/v1?key=k-secret",
            ),
            ("ATOMIC_MODEL", "m"),
            // The key itself, where its variable's name belongs.
            ("ATOMIC_MODEL_API_KEY_ENV", "sk-live-secret"),
        ],
        &nobody,
    );
    let shown = format!("{:?}", r.explain(&|_| None));
    assert!(!shown.contains("secret"), "{shown}");
    assert!(
        shown.contains("https://***@api.example.com/v1?***"),
        "{shown}"
    );
    assert!(shown.contains("not a variable's name"), "{shown}");
}

#[test]
fn a_committed_manifest_cannot_choose_the_variable_the_key_is_read_from() {
    let manifest = "atomic: 1\nname: notes\nmodel:\n  base_url: https://example.com/v1\n  \
                    api_key_env: AWS_SECRET_ACCESS_KEY\n";
    let d = dir(&[("atomic.yaml", manifest)]);
    let env = |_: &str| None;
    let refused = resolve(
        &d,
        &EngineArgs::default(),
        &Surroundings {
            env: &env,
            env_names: vec![],
            ollama: &nobody,
        },
    )
    .expect_err("refused");
    let said = refused.to_string();
    assert!(said.contains("atomic.yaml:5"), "{said}");
    assert!(said.contains("atomic.local.yaml"), "{said}");
    assert!(said.contains("ATOMIC_MODEL_API_KEY_ENV"), "{said}");
    // The person's own file, a variable or a flag may.
    let d = dir(&[
        (
            "atomic.yaml",
            "atomic: 1\nname: notes\nmodel:\n  base_url: https://example.com/v1\n",
        ),
        ("atomic.local.yaml", "model:\n  api_key_env: MY_KEY\n"),
    ]);
    let r = resolved(&d, &EngineArgs::default(), &[], &nobody);
    assert_eq!(r.settings.model.api_key_env.as_deref(), Some("MY_KEY"));
    let d = dir(&[("atomic.yaml", MANIFEST_ONLY)]);
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[("ATOMIC_MODEL_API_KEY_ENV", "MY_KEY")],
        &nobody,
    );
    assert_eq!(r.settings.model.api_key_env.as_deref(), Some("MY_KEY"));
}

#[test]
fn a_detected_ollama_is_sent_no_key_and_is_announced() {
    let d = dir(&[("atomic.yaml", MANIFEST_ONLY)]);
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[("ATOMIC_MODEL_API_KEY_ENV", "MY_KEY"), ("MY_KEY", "k")],
        &|_| Some(vec!["qwen3:8b".to_owned()]),
    );
    let endpoint = r.endpoint().expect("an endpoint");
    assert_eq!(endpoint.base_url, "http://127.0.0.1:11434/v1");
    assert_eq!(endpoint.api_key_env, None, "no key for an endpoint found");
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("model.api_key_env is set and not used")),
        "{:?}",
        r.notes
    );
    let said = r.announcement().expect("announced");
    assert!(said.contains("qwen3:8b"), "{said}");
    assert!(said.contains("http://127.0.0.1:11434/v1"), "{said}");
    assert!(said.contains("detected: Ollama answered"), "{said}");
}

#[test]
fn an_endpoint_from_the_projects_files_is_announced_and_one_the_runner_chose_is_not() {
    let d = dir(&[(
        "atomic.yaml",
        "atomic: 1\nname: notes\nmodel:\n  base_url: https://u:p@example.com/v1\n  name: m\n",
    )]);
    let r = resolved(&d, &EngineArgs::default(), &[], &nobody);
    let said = r.announcement().expect("announced");
    assert!(said.contains("https://***@example.com/v1"), "{said}");
    assert!(said.contains("atomic.yaml:4"), "{said}");
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[("ATOMIC_MODEL_BASE_URL", "http://127.0.0.1:8000/v1")],
        &nobody,
    );
    assert_eq!(r.announcement(), None);
}

#[test]
fn an_ollama_address_that_is_a_name_is_said_to_be_one() {
    let d = dir(&[("atomic.yaml", MANIFEST_ONLY)]);
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[(OLLAMA_ADDRESS_VARIABLE, "localhost:11434")],
        &|_| panic!("a name is never probed"),
    );
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("is not an IP address and port")),
        "{:?}",
        r.notes
    );
    assert!(
        !r.notes.iter().any(|n| n.contains("loopback")),
        "{:?}",
        r.notes
    );
}

#[test]
fn string_settings_keep_what_was_written_and_an_empty_variable_is_unset() {
    let d = dir(&[("atomic.yaml", "atomic: 1\nname: notes\nexecutor_id: 7\n")]);
    let r = resolved(&d, &EngineArgs::default(), &[], &nobody);
    assert_eq!(r.settings.executor_id.as_deref(), Some("7"));
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[
            ("ATOMIC_EXECUTOR_ID", "1"),
            ("ATOMIC_MODEL_NAME", "3.10"),
            ("ATOMIC_MODEL_BASE_URL", "http://127.0.0.1:8000/v1"),
            ("ATOMIC_DATABASE_SCHEMA", ""),
        ],
        &nobody,
    );
    assert_eq!(r.settings.executor_id.as_deref(), Some("1"));
    assert_eq!(r.settings.model.name.as_deref(), Some("3.10"));
    assert_eq!(r.settings.database.schema, "atomic");
    assert_eq!(source(&r, "database.schema"), &Source::Default);
}

/// A fake Ollama on a loopback port that answers one request with
/// `respond`, given the request's head; and that head, once read.
fn fake_ollama(
    respond: impl FnOnce(&mut TcpStream) + Send + 'static,
) -> (SocketAddr, std::sync::mpsc::Receiver<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (tell, told) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).is_ok_and(|n| n == 1) {
            head.push(byte[0]);
        }
        let _ = tell.send(String::from_utf8_lossy(&head).into_owned());
        respond(&mut stream);
    });
    (addr, told)
}

#[test]
fn the_probe_reads_a_chunked_answer_of_many_models() {
    let names: Vec<String> = (0..60).map(|i| format!("model-{i:02}:latest")).collect();
    let body = serde_json::to_vec(&serde_json::json!({
        "models": names.iter().map(|n| serde_json::json!({"name": n, "size": 1})).collect::<Vec<_>>()
    }))
    .expect("json");
    assert!(body.len() > 2048, "chunked as Ollama chunks a long list");
    let (addr, told) = fake_ollama(move |stream| {
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n");
        for part in body.chunks(700) {
            let _ = write!(stream, "{:x}\r\n", part.len());
            let _ = stream.write_all(part);
            let _ = stream.write_all(b"\r\n");
            let _ = stream.flush();
        }
        let _ = stream.write_all(b"0\r\n\r\n");
        // Kept open, as a keep-alive server would: the last chunk ends it.
        std::thread::sleep(Duration::from_secs(3));
    });
    let started = Instant::now();
    assert_eq!(probe_ollama(addr), Some(names));
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "{:?}",
        started.elapsed()
    );
    let head = told.recv().expect("the request");
    assert!(head.starts_with("GET /api/tags HTTP/1.0\r\n"), "{head}");
}

#[test]
fn the_probe_gives_up_at_its_deadline_however_slowly_an_answer_trickles() {
    let (addr, _told) = fake_ollama(|stream| {
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n");
        // A byte at a time, each well inside any one read's patience.
        for _ in 0..50 {
            let _ = stream.write_all(b" ");
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    let started = Instant::now();
    assert_eq!(probe_ollama(addr), None);
    let took = started.elapsed();
    assert!(
        took < PROBE_DEADLINE + Duration::from_millis(500),
        "{took:?}"
    );
}
