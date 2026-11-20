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
    resolved_where(dir, args, vars, ollama).expect("resolves")
}

/// [`resolved`], and its error.
fn resolved_where(
    dir: &Path,
    args: &EngineArgs,
    vars: &[(&str, &str)],
    ollama: &dyn Fn(SocketAddr) -> Option<Vec<String>>,
) -> Result<Resolved, ConfigError> {
    resolved_signed_in(dir, args, vars, ollama, &|_| Ok(None))
}

/// [`resolved_where`], with `sign_in` saying who is signed in.
fn resolved_signed_in(
    dir: &Path,
    args: &EngineArgs,
    vars: &[(&str, &str)],
    ollama: &dyn Fn(SocketAddr) -> Option<Vec<String>>,
    sign_in: &dyn Fn(Option<&Path>) -> SignInFound,
) -> Result<Resolved, ConfigError> {
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
            sign_in,
        },
    )
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
fn a_configured_endpoint_comes_first_then_the_sign_in_then_detection() {
    let d = dir(&[("atomic.yaml", MANIFEST_ONLY)]);
    let auth = d.join("auth");
    let looked = AtomicUsize::new(0);
    let signed_in = |project: Option<&Path>| {
        looked.fetch_add(1, Ordering::Relaxed);
        assert_eq!(project, Some(d.as_path()), "kept apart from the project");
        Ok(Some(SignIn::in_files(&auth)))
    };
    let model = ("ATOMIC_MODEL", "gpt-5-codex");
    // Signed in, with a key set and an Ollama answering: the sign-in.
    let r = resolved_signed_in(
        &d,
        &EngineArgs::default(),
        &[("OPENAI_API_KEY", "sk-secret-value"), model],
        &|_| panic!("no probe when someone is signed in"),
        &signed_in,
    )
    .expect("resolves");
    assert!(r.on_sign_in());
    let endpoint = r.endpoint().expect("an endpoint");
    assert_eq!(endpoint.base_url, atomic_core::signin::CODEX_BACKEND);
    assert_eq!(endpoint.model, "gpt-5-codex");
    assert_eq!(endpoint.api_key_env, None, "no key beside the sign-in");
    assert!(endpoint.sign_in.is_some());
    assert!(
        !matches!(source(&r, "model.base_url"), Source::Detected(_)),
        "nothing was detected: {:?}",
        source(&r, "model.base_url")
    );
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("OPENAI_API_KEY is set and not used")),
        "{:?}",
        r.notes
    );
    let line = r.announcement().expect("announced");
    assert!(
        line.contains("gpt-5-codex on your ChatGPT sign-in"),
        "{line}"
    );
    // A key named beside the sign-in is not sent, and that is said.
    let r = resolved_signed_in(
        &d,
        &EngineArgs::default(),
        &[("ATOMIC_MODEL_API_KEY_ENV", "MY_KEY"), model],
        &nobody,
        &signed_in,
    )
    .expect("resolves");
    assert_eq!(r.endpoint().expect("an endpoint").api_key_env, None);
    assert!(
        r.notes
            .iter()
            .any(|n| n.contains("model.api_key_env is set and not used")),
        "{:?}",
        r.notes
    );
    // A configured endpoint, from a variable, a flag or a file, comes
    // before the sign-in, which is not even looked for.
    looked.store(0, Ordering::Relaxed);
    let flag = EngineArgs {
        model_base_url: Some("http://127.0.0.1:4000/v1".into()),
        ..EngineArgs::default()
    };
    for (args, vars) in [
        (
            EngineArgs::default(),
            vec![("ATOMIC_MODEL_BASE_URL", "http://127.0.0.1:4000/v1"), model],
        ),
        (flag, vec![model]),
    ] {
        let r = resolved_signed_in(&d, &args, &vars, &nobody, &signed_in).expect("resolves");
        assert!(!r.on_sign_in());
        let endpoint = r.endpoint().expect("an endpoint");
        assert_eq!(endpoint.base_url, "http://127.0.0.1:4000/v1");
        assert!(endpoint.sign_in.is_none());
    }
    let file = dir(&[(
        "atomic.yaml",
        "atomic: 1\nname: notes\nmodel:\n  base_url: http://127.0.0.1:4000/v1\n  name: m\n",
    )]);
    let r = resolved_signed_in(&file, &EngineArgs::default(), &[], &nobody, &|_| {
        looked.fetch_add(1, Ordering::Relaxed);
        Ok(Some(SignIn::in_files(&auth)))
    })
    .expect("resolves");
    assert!(r.endpoint().expect("an endpoint").sign_in.is_none());
    assert_eq!(
        looked.load(Ordering::Relaxed),
        0,
        "the sign-in was looked for"
    );
    // An empty base URL is no endpoint: the sign-in.
    let empty = EngineArgs {
        model_base_url: Some(" ".into()),
        ..EngineArgs::default()
    };
    let r = resolved_signed_in(&d, &empty, &[model], &nobody, &signed_in).expect("resolves");
    assert!(r.on_sign_in(), "{:?}", r.settings.model);
    // No one signed in: detection, as before.
    let r = resolved_signed_in(
        &d,
        &EngineArgs::default(),
        &[("OPENAI_API_KEY", "sk-secret-value")],
        &nobody,
        &|_| Ok(None),
    )
    .expect("resolves");
    assert_eq!(r.endpoint().expect("an endpoint").base_url, OPENAI_BASE_URL);
    // A sign-in that cannot be read is said, and detection goes on.
    let r = resolved_signed_in(
        &d,
        &EngineArgs::default(),
        &[("OPENAI_API_KEY", "sk-secret-value")],
        &nobody,
        &|_| Err("the keyring is locked".into()),
    )
    .expect("resolves");
    assert_eq!(r.endpoint().expect("an endpoint").base_url, OPENAI_BASE_URL);
    assert_eq!(
        r.sign_in_unreadable.as_deref(),
        Some("the keyring is locked")
    );
    assert!(r.notes.iter().any(|n| n.contains("the keyring is locked")));
}

#[test]
fn a_null_base_url_in_the_persons_own_file_is_unset() {
    let d = dir(&[("atomic.yaml", MANIFEST_ONLY)]);
    let (home, home_var) = person("model:\n  base_url: null\n");
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[("OPENAI_API_KEY", "sk-secret-value"), ("HOME", &home_var)],
        &nobody,
    );
    let _ = std::fs::remove_dir_all(&home);
    assert_eq!(r.endpoint().expect("an endpoint").base_url, OPENAI_BASE_URL);
}

#[test]
fn outside_a_project_the_sign_in_is_looked_for_with_no_project() {
    let d = dir(&[]);
    let r = resolved_signed_in(&d, &EngineArgs::default(), &[], &nobody, &|project| {
        assert_eq!(project, None);
        Ok(None)
    })
    .expect("resolves");
    assert!(!r.on_sign_in());
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
        // A path segment that could be a key is not shown.
        (
            "https://gateway.example.com/v1/sk-0123456789abcdefghij/openai",
            "https://gateway.example.com/v1/***/openai",
        ),
        (
            "https://h.example.com/v1/k%20ey",
            "https://h.example.com/v1/***",
        ),
        // No scheme, or not a URL at all: nothing of it is shown.
        ("atomic:secret@db/atomic", NOT_SHOWN),
        ("host=db user=atomic password=secret", NOT_SHOWN),
        ("secret", NOT_SHOWN),
    ] {
        assert_eq!(mask_url(url), shown, "{url}");
    }
}

#[test]
fn explain_masks_the_endpoint_and_a_key_put_where_its_name_belongs_is_refused() {
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
        ],
        &nobody,
    );
    let shown = format!("{:?}", r.explain(&|_| None));
    assert!(!shown.contains("secret"), "{shown}");
    assert!(
        shown.contains("https://***@api.example.com/v1?***"),
        "{shown}"
    );
    // The key itself, where its variable's name belongs.
    let said = resolved_where(
        &d,
        &EngineArgs::default(),
        &[("ATOMIC_MODEL_API_KEY_ENV", "sk-live-secret")],
        &nobody,
    )
    .expect_err("refused")
    .to_string();
    assert!(
        said.contains("not an environment variable's name"),
        "{said}"
    );
    assert!(!said.contains("secret"), "{said}");
}

/// A person whose own configuration, at `home/.config/atomic/config.yaml`,
/// holds `text`: the variables that find it.
fn person(text: &str) -> (PathBuf, String) {
    let home = dir(&[]);
    std::fs::create_dir_all(home.join(".config/atomic")).expect("config dir");
    std::fs::write(home.join(".config/atomic/config.yaml"), text).expect("config");
    let home_var = home.display().to_string();
    (home, home_var)
}

#[test]
fn the_projects_files_cannot_choose_the_variable_the_key_is_read_from() {
    let local = "model:\n  base_url: https://example.com/v1\n  api_key_env: MY_KEY\n";
    for (files, line) in [
        (
            vec![(
                "atomic.yaml",
                "atomic: 1\nname: notes\nmodel:\n  base_url: https://example.com/v1\n  \
                 api_key_env: AWS_SECRET_ACCESS_KEY\n",
            )],
            "atomic.yaml:5",
        ),
        // atomic.local.yaml is in the project, tracked by git or not.
        (
            vec![("atomic.yaml", MANIFEST_ONLY), ("atomic.local.yaml", local)],
            "atomic.local.yaml:3",
        ),
    ] {
        let d = dir(&files);
        let (_home, home) = person("");
        let said = resolved_where(&d, &EngineArgs::default(), &[("HOME", &home)], &nobody)
            .expect_err("refused")
            .to_string();
        assert!(said.contains(line), "{said}");
        assert!(said.contains(".config/atomic/config.yaml"), "{said}");
        assert!(said.contains("ATOMIC_MODEL_API_KEY_ENV"), "{said}");
    }
    // The person's own configuration, a variable or a flag may.
    let d = dir(&[("atomic.yaml", MANIFEST_ONLY)]);
    let (_home, home) = person(local);
    let r = resolved(&d, &EngineArgs::default(), &[("HOME", &home)], &nobody);
    assert_eq!(r.settings.model.api_key_env.as_deref(), Some("MY_KEY"));
    assert_eq!(
        r.settings.model.base_url.as_deref(),
        Some("https://example.com/v1")
    );
    // Found through XDG_CONFIG_HOME as well.
    let (xdg, _) = person("");
    std::fs::create_dir_all(xdg.join("atomic")).expect("xdg");
    std::fs::write(xdg.join("atomic/config.yaml"), local).expect("xdg config");
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[("XDG_CONFIG_HOME", &xdg.display().to_string())],
        &nobody,
    );
    assert_eq!(r.settings.model.api_key_env.as_deref(), Some("MY_KEY"));
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
        "atomic: 1\nname: notes\nmodel:\n  base_url: https://example.com/v1\n  name: m\n",
    )]);
    let r = resolved(&d, &EngineArgs::default(), &[], &nobody);
    let said = r.announcement().expect("announced");
    assert!(said.contains("https://example.com/v1"), "{said}");
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

#[test]
fn a_key_goes_to_an_endpoint_the_project_chose_only_if_the_person_chose_it_too() {
    let d = dir(&[(
        "atomic.yaml",
        "atomic: 1\nname: notes\nmodel:\n  base_url: https://example.com/v1\n  name: m\n",
    )]);
    let withheld = |r: &Resolved| {
        assert_eq!(r.settings.model.api_key_env, None, "no key is sent");
        assert_eq!(r.endpoint().and_then(|e| e.api_key_env), None);
        let why = r.withheld_key.clone().expect("withheld, and why");
        assert!(r.notes.contains(&why), "{:?}", r.notes);
        why
    };
    // The person's shell names a key; the project names where it goes. The
    // settings still resolve, for commands that call no model.
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[("ATOMIC_MODEL_API_KEY_ENV", "OPENAI_API_KEY")],
        &nobody,
    );
    let why = withheld(&r);
    assert!(why.contains("atomic.yaml:4"), "{why}");
    assert!(
        why.contains("environment variable ATOMIC_MODEL_API_KEY_ENV"),
        "{why}"
    );
    // So is a flag's, and one from the person's own configuration.
    let args = EngineArgs {
        model_api_key_env: Some("MY_KEY".into()),
        ..EngineArgs::default()
    };
    let why = withheld(&resolved(&d, &args, &[], &nobody));
    assert!(why.contains("flag --model-api-key-env"), "{why}");
    let (_home, home) = person("model:\n  api_key_env: MY_KEY\n");
    let why = withheld(&resolved(
        &d,
        &EngineArgs::default(),
        &[("HOME", &home)],
        &nobody,
    ));
    assert!(why.contains(".config/atomic/config.yaml:2"), "{why}");
    let (_home, home) =
        person("model:\n  base_url: https://other.example.com/v1\n  api_key_env: MY_KEY\n");
    withheld(&resolved(
        &d,
        &EngineArgs::default(),
        &[("HOME", &home)],
        &nobody,
    ));
    // The person's configuration naming the same endpoint takes the key.
    let (_home, home) =
        person("model:\n  base_url: https://example.com/v1/\n  api_key_env: MY_KEY\n");
    let r = resolved(&d, &EngineArgs::default(), &[("HOME", &home)], &nobody);
    assert_eq!(r.settings.model.api_key_env.as_deref(), Some("MY_KEY"));
    assert_eq!(r.withheld_key, None);
    // An endpoint the person sets by variable or flag takes the key.
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[
            ("ATOMIC_MODEL_API_KEY_ENV", "OPENAI_API_KEY"),
            ("ATOMIC_MODEL_BASE_URL", "https://api.openai.com/v1"),
        ],
        &nobody,
    );
    assert_eq!(
        r.settings.model.api_key_env.as_deref(),
        Some("OPENAI_API_KEY")
    );
    assert_eq!(r.withheld_key, None);
}

#[test]
fn a_projects_endpoint_that_is_not_a_url_is_refused_as_one() {
    let d = dir(&[(
        "atomic.yaml",
        "atomic: 1\nname: notes\nmodel:\n  base_url: example.com/v1\n",
    )]);
    let said = resolved_where(&d, &EngineArgs::default(), &[], &nobody)
        .expect_err("refused")
        .to_string();
    assert!(said.contains("does not read as a URL"), "{said}");
    assert!(!said.contains("credential"), "{said}");
}

#[test]
fn a_personal_configuration_inside_the_project_is_refused() {
    // The home directory is where the project is.
    let d = dir(&[("atomic.yaml", MANIFEST_ONLY)]);
    std::fs::create_dir_all(d.join(".config/atomic")).expect("config dir");
    std::fs::write(d.join(".config/atomic/config.yaml"), "runner: local\n").expect("config");
    let said = resolved_where(
        &d,
        &EngineArgs::default(),
        &[("HOME", &d.display().to_string())],
        &nobody,
    )
    .expect_err("refused")
    .to_string();
    assert!(said.contains("inside the project"), "{said}");
    // Outside it, it is read.
    let (_home, home) = person("runner: process\n");
    let r = resolved(&d, &EngineArgs::default(), &[("HOME", &home)], &nobody);
    assert_eq!(r.settings.runner, "process");
}

#[test]
fn outside_a_project_the_personal_configuration_below_is_simply_read() {
    // `atomic config` from the home directory, which holds the file.
    let home = dir(&[]);
    std::fs::create_dir_all(home.join(".config/atomic")).expect("config dir");
    std::fs::write(home.join(".config/atomic/config.yaml"), "runner: process\n").expect("config");
    let r = resolved(
        &home,
        &EngineArgs::default(),
        &[("HOME", &home.display().to_string())],
        &nobody,
    );
    assert_eq!(r.settings.runner, "process");
}

#[test]
fn a_personal_configuration_in_the_checkout_holding_the_project_is_refused() {
    // The home directory is the repository's root; the project is below it.
    let checkout = dir(&[]);
    std::fs::create_dir_all(checkout.join(".git")).expect("git dir");
    std::fs::create_dir_all(checkout.join("services/notes")).expect("project dir");
    std::fs::write(checkout.join("services/notes/atomic.yaml"), MANIFEST_ONLY).expect("manifest");
    std::fs::create_dir_all(checkout.join(".config/atomic")).expect("config dir");
    std::fs::write(
        checkout.join(".config/atomic/config.yaml"),
        "runner: local\n",
    )
    .expect("config");
    let said = resolved_where(
        &checkout.join("services/notes"),
        &EngineArgs::default(),
        &[("HOME", &checkout.display().to_string())],
        &nobody,
    )
    .expect_err("refused")
    .to_string();
    assert!(said.contains("checkout holding the project"), "{said}");
}

#[test]
fn an_endpoint_the_person_configured_is_not_announced() {
    let d = dir(&[("atomic.yaml", MANIFEST_ONLY)]);
    let (_home, home) = person("model:\n  base_url: http://127.0.0.1:8000/v1\n  name: m\n");
    let r = resolved(&d, &EngineArgs::default(), &[("HOME", &home)], &nobody);
    assert_eq!(r.announcement(), None);
}

#[test]
fn the_projects_files_name_no_store_and_no_credential() {
    for (file, text, line) in [
        (
            "atomic.yaml",
            "atomic: 1\nname: notes\ndatabase:\n  url: postgres://db/atomic\n",
            "atomic.yaml:4",
        ),
        (
            "atomic.local.yaml",
            "database:\n  url: postgres://db/atomic\n",
            "atomic.local.yaml:2",
        ),
        (
            "atomic.yaml",
            "atomic: 1\nname: notes\nmodel:\n  base_url: https://u:pw@example.com/v1\n",
            "atomic.yaml:4",
        ),
        // A token as the user, or in the query.
        (
            "atomic.yaml",
            "atomic: 1\nname: notes\nmodel:\n  base_url: https://tok-pw@example.com/v1\n",
            "atomic.yaml:4",
        ),
        (
            "atomic.local.yaml",
            "model:\n  base_url: https://example.com/v1?api_key=pw\n",
            "atomic.local.yaml:2",
        ),
    ] {
        let files = if file == "atomic.yaml" {
            vec![(file, text)]
        } else {
            vec![("atomic.yaml", MANIFEST_ONLY), (file, text)]
        };
        let d = dir(&files);
        let said = resolved_where(&d, &EngineArgs::default(), &[], &nobody)
            .expect_err("refused")
            .to_string();
        assert!(said.contains(line), "{said}");
        assert!(!said.contains("pw"), "{said}");
    }
    // From the person: their own configuration or the variable.
    let d = dir(&[("atomic.yaml", MANIFEST_ONLY)]);
    let (_home, home) = person("database:\n  url: postgres://u:pw@db/atomic\n");
    let r = resolved(&d, &EngineArgs::default(), &[("HOME", &home)], &nobody);
    assert!(r.settings.database.url.is_some());
    resolved(
        &d,
        &EngineArgs::default(),
        &[("ATOMIC_DATABASE_URL", "postgres://u:pw@db/atomic")],
        &nobody,
    );
}

#[test]
fn a_runner_atomic_does_not_know_is_refused_when_read() {
    let d = dir(&[("atomic.yaml", "atomic: 1\nname: notes\nrunner: proces\n")]);
    let said = resolved_where(&d, &EngineArgs::default(), &[], &nobody)
        .expect_err("refused")
        .to_string();
    assert!(said.contains("must be local or process"), "{said}");
    assert!(said.contains("atomic.yaml:3"), "{said}");
}

#[test]
fn the_budget_layers_like_every_other_setting_and_says_where_each_limit_came_from() {
    // Each layer sets the atom's tokens; each lower one also sets a limit
    // the layers above leave alone.
    let (_home, home) = person("budget:\n  atom:\n    tokens: 100\n  call:\n    minutes: 3\n");
    let d = dir(&[
        (
            "atomic.yaml",
            "atomic: 1\nname: notes\nbudget:\n  atom:\n    tokens: 200\n  workflow:\n    \
             minutes: 45\n  pricing:\n    m:\n      input_usd_per_mtok: 1\n      \
             output_usd_per_mtok: 2\n",
        ),
        (
            "atomic.local.yaml",
            "budget:\n  atom:\n    tokens: 300\n  workflow:\n    tokens: 9000\n",
        ),
    ]);
    let vars = [
        ("HOME", home.as_str()),
        ("ATOMIC_BUDGET_ATOM_TOKENS", "400"),
        ("ATOMIC_BUDGET_CALL_INPUT_TOKENS", "5000"),
        ("ATOMIC_BUDGET_WORKFLOW_COST_USD", "1.5"),
    ];
    let r = resolved(&d, &EngineArgs::default(), &vars, &nobody);
    let budget = &r.settings.budget;
    let defaults = BudgetSettings::default();
    assert_eq!(budget.atom.tokens, 400);
    assert_eq!(budget.call.input_tokens, 5000);
    assert_eq!(budget.workflow.cost_usd, Some(1.5));
    assert_eq!(budget.workflow.tokens, 9000);
    assert_eq!(budget.workflow.minutes, 45.0);
    assert_eq!(budget.call.minutes, 3.0);
    assert_eq!(budget.call.output_tokens, defaults.call.output_tokens);
    assert_eq!(budget.atom.minutes, defaults.atom.minutes);
    assert_eq!(budget.pricing["m"].output_usd_per_mtok, 2.0);
    let file = |r: &Resolved, path: &str| match source(r, path) {
        Source::File { path, .. } => path.file_name().expect("name").to_owned(),
        other => panic!("{path} came from {other:?}"),
    };
    assert_eq!(
        source(&r, "budget.atom.tokens"),
        &Source::Env("ATOMIC_BUDGET_ATOM_TOKENS".into())
    );
    assert_eq!(
        source(&r, "budget.call.input_tokens"),
        &Source::Env("ATOMIC_BUDGET_CALL_INPUT_TOKENS".into())
    );
    assert_eq!(file(&r, "budget.workflow.tokens"), "atomic.local.yaml");
    assert_eq!(file(&r, "budget.workflow.minutes"), "atomic.yaml");
    assert_eq!(file(&r, "budget.call.minutes"), "config.yaml");
    assert_eq!(source(&r, "budget.atom.minutes"), &Source::Default);
    assert_eq!(
        source(&r, "budget.workflow.cost_usd"),
        &Source::Env("ATOMIC_BUDGET_WORKFLOW_COST_USD".into())
    );

    // Without the variable, the local file wins; without that, the
    // project's; without that, the person's own.
    let r = resolved(&d, &EngineArgs::default(), &vars[..1], &nobody);
    assert_eq!(r.settings.budget.atom.tokens, 300);
    assert_eq!(file(&r, "budget.atom.tokens"), "atomic.local.yaml");
    std::fs::remove_file(d.join("atomic.local.yaml")).expect("remove");
    let r = resolved(&d, &EngineArgs::default(), &vars[..1], &nobody);
    assert_eq!(r.settings.budget.atom.tokens, 200);
    assert_eq!(file(&r, "budget.atom.tokens"), "atomic.yaml");
    std::fs::write(d.join("atomic.yaml"), MANIFEST_ONLY).expect("manifest");
    let r = resolved(&d, &EngineArgs::default(), &vars[..1], &nobody);
    assert_eq!(r.settings.budget.atom.tokens, 100);
    assert_eq!(file(&r, "budget.atom.tokens"), "config.yaml");

    // A variable that does not read as its limit is refused, by name and
    // without its value: a word, a negative count, and a word for a dollar
    // cap, which has no default to read it by.
    for (name, value) in [
        ("ATOMIC_BUDGET_ATOM_TOKENS", "lots"),
        ("ATOMIC_BUDGET_ATOM_TOKENS", "-5"),
        ("ATOMIC_BUDGET_CALL_MINUTES", "soon"),
        ("ATOMIC_BUDGET_ATOM_COST_USD", "lots"),
        ("ATOMIC_BUDGET_WORKFLOW_COST_USD", "plenty"),
    ] {
        let said = resolved_where(&d, &EngineArgs::default(), &[(name, value)], &nobody)
            .expect_err("refused")
            .to_string();
        assert!(said.contains(name), "{name}={value}: {said}");
        assert!(!said.contains(value), "{name}={value}: {said}");
    }
}
