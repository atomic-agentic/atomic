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
    // The same resolution the state check and the denial use, with the
    // environment as `vars` gives it and no password home: a test's
    // environment says nothing about this machine's user, and nothing here may
    // read the real person's configuration.
    let config_dir = || {
        atomic_core::state::user_config_dir_in(
            &|name| vars.get(name).cloned().map(std::ffi::OsString::from),
            &None,
        )
    };
    resolve(
        dir,
        args,
        &Surroundings {
            env: &env,
            env_names: vars.keys().cloned().collect(),
            ollama,
            user_config_dir: &config_dir,
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
        "backend_wait",
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

/// `backend_wait` comes from a file, a variable or the flag, the flag
/// first; an empty variable sets nothing. Anything but a whole number of
/// seconds of at least one is refused wherever it came from, an explicit
/// null or blank included, and the refusal names its source.
#[test]
fn the_backend_wait_is_a_setting_of_at_least_a_second() {
    let d = dir(&[("atomic.yaml", "atomic: 1\nname: notes\nbackend_wait: 300\n")]);
    let r = resolved(&d, &EngineArgs::default(), &[], &nobody);
    assert_eq!(r.settings.backend_wait(), Some(300));
    assert!(matches!(source(&r, "backend_wait"), Source::File { .. }));
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[("ATOMIC_BACKEND_WAIT", "5")],
        &nobody,
    );
    assert_eq!(r.settings.backend_wait(), Some(5));
    assert_eq!(
        source(&r, "backend_wait"),
        &Source::Env("ATOMIC_BACKEND_WAIT".into())
    );
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[("ATOMIC_BACKEND_WAIT", "")],
        &nobody,
    );
    assert_eq!(
        r.settings.backend_wait(),
        Some(300),
        "an empty variable sets nothing"
    );
    let flag = EngineArgs {
        backend_wait: Some(9),
        ..EngineArgs::default()
    };
    let r = resolved(&d, &flag, &[("ATOMIC_BACKEND_WAIT", "5")], &nobody);
    assert_eq!(r.settings.backend_wait(), Some(9));
    assert_eq!(
        source(&r, "backend_wait"),
        &Source::Flag("--backend-wait".into())
    );
    let quoted = dir(&[(
        "atomic.yaml",
        "atomic: 1\nname: notes\nbackend_wait: '30'\n",
    )]);
    let r = resolved(&quoted, &EngineArgs::default(), &[], &nobody);
    assert_eq!(
        r.settings.backend_wait(),
        Some(30),
        "a quoted number is that number"
    );
    let none = dir(&[("atomic.yaml", MANIFEST_ONLY)]);
    let r = resolved(&none, &EngineArgs::default(), &[], &nobody);
    assert_eq!(
        r.settings.backend_wait(),
        None,
        "without one, a step waits as long as it takes"
    );
    for (file, var) in [
        ("backend_wait: 0\n", None),
        ("backend_wait: '0'\n", None),
        ("backend_wait: -3\n", None),
        ("backend_wait: 1.5\n", None),
        ("backend_wait: soon\n", None),
        ("backend_wait: ~\n", None),
        ("backend_wait: null\n", None),
        ("backend_wait: [1]\n", None),
        ("", Some("0")),
        ("", Some("ten")),
        ("", Some("  ")),
        ("", Some("~")),
        ("", Some("null")),
        ("backend_wait: 300\n", Some("~")),
    ] {
        let d = dir(&[("atomic.yaml", &format!("{MANIFEST_ONLY}{file}"))]);
        let vars: Vec<(&str, &str)> = var
            .map(|v| ("ATOMIC_BACKEND_WAIT", v))
            .into_iter()
            .collect();
        let e = resolved_where(&d, &EngineArgs::default(), &vars, &nobody)
            .expect_err("refused")
            .to_string();
        let from = match var {
            Some(_) => "from environment variable ATOMIC_BACKEND_WAIT".to_owned(),
            None => format!("from {}", d.join("atomic.yaml").display()),
        };
        assert!(
            e.contains(&format!("backend_wait, {from}"))
                && e.contains("a whole number of seconds, at least 1"),
            "{file:?} {var:?}: {e}"
        );
    }
}

/// Every setting may be written in the manifest, which shares its file with
/// the settings: the project loader knows each top-level key.
#[test]
fn the_manifest_accepts_every_setting() {
    let Value::Object(defaults) = serde_json::to_value(Settings::default()).expect("defaults")
    else {
        panic!("the settings are a mapping");
    };
    let mut manifest = serde_json::Map::new();
    manifest.insert("atomic".into(), Value::from(1));
    manifest.insert("name".into(), Value::from("notes"));
    let mut settings = Vec::new();
    for (key, value) in defaults {
        if MANIFEST.contains(&key.as_str()) {
            continue;
        }
        settings.push(key.clone());
        manifest.insert(key, value);
    }
    assert!(
        settings.contains(&"backend_wait".to_owned()),
        "{settings:?}"
    );
    let text = serde_json::to_string(&Value::Object(manifest)).expect("json");
    let d = dir(&[("atomic.yaml", &text)]);
    atomic_core::project::load(&d.join("atomic.yaml"))
        .unwrap_or_else(|e| panic!("the manifest refuses a setting of {settings:?}: {e}"));
}

#[test]
fn the_budget_layers_like_every_other_setting_and_says_where_each_limit_came_from() {
    // Each layer sets the atom's tokens; each lower one also sets a limit
    // the layers above leave alone.
    // Their own file sets only a limit nothing above it touches: a cap they
    // write is a ceiling on every layer above, which is its own test.
    let (_home, home) = person("budget:\n  call:\n    minutes: 3\n");
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
    // project's; without either, the built-in default.
    let r = resolved(&d, &EngineArgs::default(), &vars[..1], &nobody);
    assert_eq!(r.settings.budget.atom.tokens, 300);
    assert_eq!(file(&r, "budget.atom.tokens"), "atomic.local.yaml");
    std::fs::remove_file(d.join("atomic.local.yaml")).expect("remove");
    let r = resolved(&d, &EngineArgs::default(), &vars[..1], &nobody);
    assert_eq!(r.settings.budget.atom.tokens, 200);
    assert_eq!(file(&r, "budget.atom.tokens"), "atomic.yaml");
    std::fs::write(d.join("atomic.yaml"), MANIFEST_ONLY).expect("manifest");
    let r = resolved(&d, &EngineArgs::default(), &vars[..1], &nobody);
    assert_eq!(
        r.settings.budget.atom.tokens,
        BudgetSettings::default().atom.tokens
    );
    assert_eq!(source(&r, "budget.atom.tokens"), &Source::Default);

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

/// The file a setting came from, by name, and the line it is on.
fn file_and_line(r: &Resolved, path: &str) -> (String, usize) {
    match source(r, path) {
        Source::File { path, line } => (
            path.file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned(),
            *line,
        ),
        other => panic!("{path} came from {other:?}"),
    }
}

/// A person's own `budget` section: a cap on what an atom and a run may spend,
/// and a bound on one call. Its lines are named in the assertions below.
const THEIR_OWN_CAPS: &str = "budget:\n  atom:\n    tokens: 100000\n  workflow:\n    \
                              tokens: 200000\n  call:\n    input_tokens: 1000\n    \
                              output_tokens: 500\n    minutes: 1\n";

#[test]
fn your_own_cap_holds_a_larger_one_in_the_projects_committed_file() {
    let (_home, home) = person(THEIR_OWN_CAPS);
    let d = dir(&[(
        "atomic.yaml",
        "atomic: 1\nname: notes\nbudget:\n  atom:\n    tokens: 400000\n  workflow:\n    \
         tokens: 800000\n  call:\n    input_tokens: 60000\n",
    )]);
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[("HOME", home.as_str())],
        &nobody,
    );
    let budget = &r.settings.budget;
    assert_eq!(budget.atom.tokens, 100_000);
    assert_eq!(budget.workflow.tokens, 200_000);
    assert_eq!(budget.call.input_tokens, 1_000);
    assert_eq!(budget.call.output_tokens, 500);
    assert_eq!(budget.call.minutes, 1.0);

    // The ceilings travel with the settings, so a run holds an atom's or
    // workflow's own `budget` block to them too, and `atomic config --explain`
    // names the file the effective number came from.
    assert_eq!(
        budget.ceilings,
        Ceilings {
            atom: Caps {
                tokens: Some(100_000),
                minutes: None,
                cost_usd: None,
            },
            workflow: Caps {
                tokens: Some(200_000),
                minutes: None,
                cost_usd: None,
            },
            call: CallCaps {
                input_tokens: Some(1_000),
                output_tokens: Some(500),
                minutes: Some(1.0),
            },
        }
    );
    assert_eq!(
        file_and_line(&r, "budget.atom.tokens"),
        ("config.yaml".to_owned(), 3)
    );
    assert_eq!(
        file_and_line(&r, "budget.workflow.tokens"),
        ("config.yaml".to_owned(), 5)
    );
    assert_eq!(
        file_and_line(&r, "budget.call.input_tokens"),
        ("config.yaml".to_owned(), 7)
    );

    let said = r.notes.join("\n");
    assert!(said.contains("caps budget.atom.tokens at 100000"), "{said}");
    assert!(said.contains("no project or variable can raise"), "{said}");
}

#[test]
fn your_own_cap_holds_a_larger_one_in_the_projects_local_file() {
    let (_home, home) = person(THEIR_OWN_CAPS);
    let d = dir(&[
        ("atomic.yaml", MANIFEST_ONLY),
        (
            "atomic.local.yaml",
            "budget:\n  atom:\n    tokens: 900000\n",
        ),
    ]);
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[("HOME", home.as_str())],
        &nobody,
    );
    assert_eq!(r.settings.budget.atom.tokens, 100_000);
    assert_eq!(
        file_and_line(&r, "budget.atom.tokens"),
        ("config.yaml".to_owned(), 3)
    );
}

#[test]
fn your_own_cap_holds_a_larger_one_set_by_a_variable() {
    let (_home, home) = person(THEIR_OWN_CAPS);
    let d = dir(&[("atomic.yaml", MANIFEST_ONLY)]);
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[
            ("HOME", home.as_str()),
            ("ATOMIC_BUDGET_WORKFLOW_TOKENS", "900000"),
        ],
        &nobody,
    );
    assert_eq!(r.settings.budget.workflow.tokens, 200_000);
    assert_eq!(
        file_and_line(&r, "budget.workflow.tokens"),
        ("config.yaml".to_owned(), 5)
    );
    let said = r.notes.join("\n");
    assert!(
        said.contains("budget.workflow.tokens at 200000, which no project or variable can raise"),
        "{said}"
    );
}

#[test]
fn a_smaller_project_cap_still_tightens_yours() {
    let (_home, home) = person(THEIR_OWN_CAPS);
    let d = dir(&[(
        "atomic.yaml",
        "atomic: 1\nname: notes\nbudget:\n  atom:\n    tokens: 500\n    minutes: 2\n    \
         cost_usd: 0.5\n",
    )]);
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[("HOME", home.as_str())],
        &nobody,
    );
    let budget = &r.settings.budget;
    assert_eq!(budget.atom.tokens, 500);
    assert_eq!(budget.atom.minutes, 2.0);
    assert_eq!(budget.atom.cost_usd, Some(0.5));
    assert_eq!(
        file_and_line(&r, "budget.atom.tokens"),
        ("atomic.yaml".to_owned(), 5)
    );
    assert_eq!(
        file_and_line(&r, "budget.atom.minutes"),
        ("atomic.yaml".to_owned(), 6)
    );
    // Nothing of theirs held, so nothing is said about it.
    assert_eq!(r.notes, Vec::<String>::new());
    // Their ceiling is still the ceiling a written `budget` block is held to.
    assert_eq!(budget.ceilings.atom.tokens, Some(100_000));
}

#[test]
fn your_own_dollar_cap_holds_a_larger_one_and_leaves_none_as_none() {
    let (_home, home) = person("budget:\n  atom:\n    cost_usd: 1\n");
    let d = dir(&[(
        "atomic.yaml",
        "atomic: 1\nname: notes\nbudget:\n  atom:\n    tokens: 999999\n    cost_usd: 5\n",
    )]);
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[("HOME", home.as_str())],
        &nobody,
    );
    let budget = &r.settings.budget;
    assert_eq!(budget.atom.cost_usd, Some(1.0));
    assert_eq!(
        file_and_line(&r, "budget.atom.cost_usd"),
        ("config.yaml".to_owned(), 3)
    );
    // A cap they wrote caps only what it is a cap on: a measure they left
    // alone is the project's, and a dollar cap nothing set stays unset rather
    // than becoming one.
    assert_eq!(budget.atom.tokens, 999999);
    assert_eq!(
        file_and_line(&r, "budget.atom.tokens"),
        ("atomic.yaml".to_owned(), 5)
    );
    assert_eq!(budget.workflow.cost_usd, None);
    assert_eq!(budget.ceilings.workflow, Caps::default());
    assert!(
        r.notes
            .join("\n")
            .contains("caps budget.atom.cost_usd at 1"),
        "{}",
        r.notes.join("\n")
    );
}

/// One way a layer above sets a cap to nothing: its name, the variables it
/// sets and the project's files it writes.
type Case<'a> = (&'a str, Vec<(&'a str, &'a str)>, Vec<(&'a str, String)>);

/// A layer above the person's own configuration cannot remove their dollar
/// cap by setting it to nothing: `null` in the project's committed file, in
/// its local file, or from the variable, for an atom and for a run, all
/// leave their number standing; a whole group set to `null` is refused.
#[test]
fn a_null_from_a_layer_above_cannot_remove_your_own_dollar_cap() {
    let (_home, home) = person("budget:\n  atom:\n    cost_usd: 1\n  workflow:\n    cost_usd: 2\n");
    for level in ["atom", "workflow"] {
        let want = if level == "atom" { 1.0 } else { 2.0 };
        let line = if level == "atom" { 3 } else { 5 };
        let variable = format!("ATOMIC_BUDGET_{}_COST_USD", level.to_uppercase());
        let committed =
            format!("atomic: 1\nname: notes\nbudget:\n  {level}:\n    cost_usd: null\n");
        let local = format!("budget:\n  {level}:\n    cost_usd: null\n");
        let group = format!("atomic: 1\nname: notes\nbudget:\n  {level}: null\n");
        let cases: [Case; 3] = [
            ("committed", vec![], vec![("atomic.yaml", committed)]),
            (
                "local",
                vec![],
                vec![
                    ("atomic.yaml", MANIFEST_ONLY.to_owned()),
                    ("atomic.local.yaml", local),
                ],
            ),
            (
                "variable",
                vec![(variable.as_str(), "null")],
                vec![("atomic.yaml", MANIFEST_ONLY.to_owned())],
            ),
        ];
        for (case, vars, files) in cases {
            let files: Vec<(&str, &str)> = files.iter().map(|(n, t)| (*n, t.as_str())).collect();
            let d = dir(&files);
            let mut env = vec![("HOME", home.as_str())];
            env.extend(vars);
            let r = resolved(&d, &EngineArgs::default(), &env, &nobody);
            let budget = &r.settings.budget;
            let (cap, ceiling) = if level == "atom" {
                (budget.atom.cost_usd, budget.ceilings.atom.cost_usd)
            } else {
                (budget.workflow.cost_usd, budget.ceilings.workflow.cost_usd)
            };
            assert_eq!(cap, Some(want), "{level}, {case}");
            assert_eq!(ceiling, Some(want), "{level}, {case}");
            assert_eq!(
                file_and_line(&r, &format!("budget.{level}.cost_usd")),
                ("config.yaml".to_owned(), line),
                "{level}, {case}"
            );
        }
        // A whole group set to nothing is no setting at all, and refused.
        let d = dir(&[("atomic.yaml", group.as_str())]);
        let said = resolved_where(
            &d,
            &EngineArgs::default(),
            &[("HOME", home.as_str())],
            &nobody,
        )
        .expect_err("refused")
        .to_string();
        assert!(
            said.contains(&format!("budget.{level} is a section")),
            "{level}: {said}"
        );
    }
}

/// Under the person's own dollar cap, a project cannot make a model they did
/// not price free: a price of nothing for both input and output is refused,
/// naming the model and where to price it. A price that is not nothing is
/// the project's, and so is any price where they set no dollar cap, or for a
/// model they priced themselves.
#[test]
fn under_your_own_dollar_cap_a_project_cannot_make_a_model_free() {
    let priced = |input: &str, output: &str| {
        format!(
            "atomic: 1\nname: notes\nbudget:\n  pricing:\n    free-model:\n      \
             input_usd_per_mtok: {input}\n      output_usd_per_mtok: {output}\n"
        )
    };
    let free = priced("0", "0");
    let (_home, home) = person("budget:\n  workflow:\n    cost_usd: 1\n");
    let d = dir(&[("atomic.yaml", free.as_str())]);
    let said = resolved_where(
        &d,
        &EngineArgs::default(),
        &[("HOME", home.as_str())],
        &nobody,
    )
    .expect_err("refused")
    .to_string();
    assert!(said.contains("budget.pricing.free-model"), "{said}");
    assert!(
        said.contains("price free-model in your own configuration"),
        "{said}"
    );

    let cheap = priced("0", "0.5");
    let d = dir(&[("atomic.yaml", cheap.as_str())]);
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[("HOME", home.as_str())],
        &nobody,
    );
    assert_eq!(
        r.settings.budget.pricing["free-model"].output_usd_per_mtok,
        0.5
    );

    let d = dir(&[("atomic.yaml", free.as_str())]);
    let (_theirs, priced_by_them) = person(
        "budget:\n  atom:\n    cost_usd: 1\n  pricing:\n    free-model:\n      \
         input_usd_per_mtok: 0\n      output_usd_per_mtok: 0\n",
    );
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[("HOME", priced_by_them.as_str())],
        &nobody,
    );
    assert_eq!(
        r.settings.budget.pricing["free-model"].input_usd_per_mtok,
        0.0
    );
    let (_capless, no_dollars) = person("budget:\n  atom:\n    tokens: 1000\n");
    resolved(
        &d,
        &EngineArgs::default(),
        &[("HOME", no_dollars.as_str())],
        &nobody,
    );
}

/// A cap or price in the person's own file that its setting cannot take is
/// refused with their file and line, even where a project's layer replaces
/// it, so a cap of theirs is never dropped without a word.
#[test]
fn a_cap_of_yours_that_cannot_be_read_is_refused_even_under_a_project_value() {
    for (theirs, project, names, says) in [
        (
            "budget:\n  atom:\n    cost_usd: \"5\"\n",
            "budget:\n  atom:\n    cost_usd: null\n",
            "budget.atom.cost_usd",
            "is not a number",
        ),
        (
            "budget:\n  atom:\n    tokens: 150000.0\n",
            "budget:\n  atom:\n    tokens: 1000\n",
            "budget.atom.tokens",
            "is not a whole number",
        ),
        (
            "budget:\n  pricing:\n    m: null\n",
            "budget:\n  pricing:\n    m:\n      input_usd_per_mtok: 1\n      output_usd_per_mtok: 1\n",
            "budget.pricing.m",
            "is not a price",
        ),
        (
            "budget:\n  pricing:\n    m:\n      input_usd_per_mtok: 1\n",
            "budget:\n  pricing:\n    m:\n      input_usd_per_mtok: 1\n      output_usd_per_mtok: 1\n",
            "budget.pricing.m.output_usd_per_mtok",
            "is missing: a price needs both",
        ),
    ] {
        let (_home, home) = person(theirs);
        let manifest = format!("atomic: 1\nname: notes\n{project}");
        let d = dir(&[("atomic.yaml", manifest.as_str())]);
        let said = resolved_where(
            &d,
            &EngineArgs::default(),
            &[("HOME", home.as_str())],
            &nobody,
        )
        .expect_err("refused")
        .to_string();
        assert!(said.contains("config.yaml:"), "{names}: {said}");
        assert!(said.contains(&format!("{names} {says}")), "{names}: {said}");
    }
}

/// Under the person's own dollar cap, a model only the project prices is
/// named, with what to do, in what `atomic config --explain` shows.
#[test]
fn under_your_own_dollar_cap_a_model_only_the_project_prices_is_named() {
    let (_home, home) = person("budget:\n  atom:\n    cost_usd: 1\n");
    let d = dir(&[(
        "atomic.yaml",
        "atomic: 1\nname: notes\nbudget:\n  pricing:\n    cheap-model:\n      \
         input_usd_per_mtok: 0.000001\n      output_usd_per_mtok: 0.000001\n",
    )]);
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[("HOME", home.as_str())],
        &nobody,
    );
    assert!(r.settings.budget.priced_by_project.contains("cheap-model"));
    let said = r.notes.join("\n");
    assert!(
        said.contains("counts calls to cheap-model at prices only a layer above yours sets"),
        "{said}"
    );
    // Without a dollar cap of theirs, nothing is named.
    let (_capless, no_dollars) = person("budget:\n  atom:\n    tokens: 1000\n");
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[("HOME", no_dollars.as_str())],
        &nobody,
    );
    assert!(r.settings.budget.priced_by_project.is_empty());
}

#[test]
fn your_own_dollar_cap_holds_a_larger_one_across_the_levels() {
    let (_home, home) = person("budget:\n  atom:\n    cost_usd: 0.5\n");
    let d = dir(&[(
        "atomic.yaml",
        "atomic: 1\nname: notes\nbudget:\n  workflow:\n    cost_usd: 9\n",
    )]);
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[("HOME", home.as_str())],
        &nobody,
    );
    // The hold is per level: their cap on an atom says nothing about a run,
    // and a project's own cap is nobody's ceiling.
    assert_eq!(r.settings.budget.workflow.cost_usd, Some(9.0));
    assert_eq!(
        file_and_line(&r, "budget.workflow.cost_usd"),
        ("atomic.yaml".to_owned(), 5)
    );
    assert_eq!(r.settings.budget.atom.cost_usd, Some(0.5));
    assert_eq!(
        file_and_line(&r, "budget.atom.cost_usd"),
        ("config.yaml".to_owned(), 3)
    );
    assert_eq!(
        r.settings.budget.ceilings,
        Ceilings {
            atom: Caps {
                tokens: None,
                minutes: None,
                cost_usd: Some(0.5),
            },
            ..Ceilings::default()
        }
    );
    assert_eq!(r.notes, Vec::<String>::new());
}

#[test]
fn your_own_price_holds_where_a_project_writes_a_cheaper_one() {
    let (_home, home) = person(
        "budget:\n  pricing:\n    gpt-test:\n      input_usd_per_mtok: 2.5\n      \
         output_usd_per_mtok: 10\n",
    );
    // A project's `budget.pricing` is one setting, so it can write the model
    // at no price at all, in the committed file or in the local one.
    let cheap = "budget:\n  pricing:\n    gpt-test:\n      input_usd_per_mtok: 0\n      \
                output_usd_per_mtok: 0\n";
    for name in ["atomic.yaml", "atomic.local.yaml"] {
        let files = if name == "atomic.yaml" {
            vec![("atomic.yaml", cheap)]
        } else {
            vec![("atomic.yaml", MANIFEST_ONLY), ("atomic.local.yaml", cheap)]
        };
        let d = dir(&files);
        let r = resolved(
            &d,
            &EngineArgs::default(),
            &[("HOME", home.as_str())],
            &nobody,
        );
        let budget = &r.settings.budget;
        assert_eq!(budget.pricing["gpt-test"].input_usd_per_mtok, 2.5, "{name}");
        assert_eq!(
            budget.pricing["gpt-test"].output_usd_per_mtok, 10.0,
            "{name}"
        );
        // A model that looks free is how a project slips past a dollar cap.
        assert_eq!(budget.cost("gpt-test", 1_000, 0), 2_500, "{name}");
        assert_eq!(
            file_and_line(&r, "budget.pricing.gpt-test.input_usd_per_mtok"),
            ("config.yaml".to_owned(), 4),
            "{name}"
        );
        let said = r.notes.join("\n");
        assert!(
            said.contains("gpt-test: input_usd_per_mtok, output_usd_per_mtok"),
            "{name}: {said}"
        );
    }
}

#[test]
fn a_layer_that_replaces_the_price_table_cannot_drop_your_own_price() {
    let (_home, home) = person(
        "budget:\n  pricing:\n    gpt-test:\n      input_usd_per_mtok: 2.5\n      \
         output_usd_per_mtok: 10\n",
    );
    // A price table is open: keys atomic does not define are not merged into
    // it, so a layer that writes `budget.pricing` replaces the whole table and
    // leaves every model it does not name unpriced — which is how a model is
    // made to look free under a dollar cap. A project's file and a variable
    // both write it this way.
    let d = dir(&[(
        "atomic.yaml",
        "budget:\n  pricing:\n    other:\n      input_usd_per_mtok: 0\n      \
         output_usd_per_mtok: 0\n",
    )]);
    held_theirs(&resolved(
        &d,
        &EngineArgs::default(),
        &[("HOME", home.as_str())],
        &nobody,
    ));
    let d = dir(&[("atomic.yaml", MANIFEST_ONLY)]);
    held_theirs(&resolved(
        &d,
        &EngineArgs::default(),
        &[
            ("HOME", home.as_str()),
            (
                "ATOMIC_BUDGET_PRICING",
                r#"{"other":{"input_usd_per_mtok":0,"output_usd_per_mtok":0}}"#,
            ),
        ],
        &nobody,
    ));

    /// The price they wrote holds, the layer's own model is its own, and what
    /// holds is said and cited.
    fn held_theirs(r: &Resolved) {
        let budget = &r.settings.budget;
        assert_eq!(budget.pricing["gpt-test"].input_usd_per_mtok, 2.5);
        assert_eq!(budget.pricing["gpt-test"].output_usd_per_mtok, 10.0);
        assert_eq!(budget.cost("gpt-test", 1_000, 0), 2_500);
        // A model they never priced is the layer's, at the price it gave.
        assert_eq!(budget.pricing["other"].input_usd_per_mtok, 0.0);
        assert_eq!(
            file_and_line(r, "budget.pricing.gpt-test.input_usd_per_mtok"),
            ("config.yaml".to_owned(), 4)
        );
        let said = r.notes.join("\n");
        assert!(said.contains("cannot change"), "{said}");
        assert!(said.contains("gpt-test"), "{said}");
    }
}

#[test]
fn a_project_cannot_give_a_model_a_bigger_call_than_your_own() {
    let (_home, home) = person("budget:\n  call:\n    output_tokens: 500\n");
    let d = dir(&[(
        "atomic.yaml",
        "atomic: 1\nname: notes\nbudget:\n  pricing:\n    gpt-test:\n      \
         input_usd_per_mtok: 1\n      output_usd_per_mtok: 1\n      max_output_tokens: 100000\n      \
         max_input_tokens: 20\n",
    )]);
    let r = resolved(
        &d,
        &EngineArgs::default(),
        &[("HOME", home.as_str())],
        &nobody,
    );
    let budget = &r.settings.budget;
    // A model's own maximum replaces the call bound for a call to it, so a
    // project that writes one above their cap would loosen it for that model.
    assert_eq!(budget.pricing["gpt-test"].max_output_tokens, Some(500));
    // Cited from the cap that holds it, at their line, not from the file that
    // wrote the larger number.
    assert_eq!(
        file_and_line(&r, "budget.pricing.gpt-test.max_output_tokens"),
        ("config.yaml".to_owned(), 3)
    );
    // A maximum under their cap is the project's: only what their cap holds is
    // cited from their file.
    assert_eq!(budget.pricing["gpt-test"].max_input_tokens, Some(20));
    assert_eq!(budget.reservation("gpt-test").output_tokens, 500);
    assert!(
        r.notes
            .join("\n")
            .contains("caps the most one call may use"),
        "{}",
        r.notes.join("\n")
    );
}

#[test]
fn a_project_that_writes_their_budget_section_as_a_ceiling_is_refused() {
    let d = dir(&[(
        "atomic.yaml",
        "atomic: 1\nname: notes\nbudget:\n  ceilings:\n    atom:\n      tokens: 1\n",
    )]);
    let said = resolved_where(&d, &EngineArgs::default(), &[], &nobody)
        .expect_err("refused")
        .to_string();
    assert!(said.contains("unknown setting"), "{said}");
    assert!(said.contains("budget.ceilings"), "{said}");
    assert!(said.contains("atomic.yaml:"), "{said}");
}

#[test]
fn a_budget_setting_of_yours_that_cannot_be_read_is_refused_not_ignored() {
    let d = dir(&[("atomic.yaml", MANIFEST_ONLY)]);
    // A cap or a price that cannot be read never becomes "no cap" or "no
    // price": a model left unpriced under a dollar cap, or a limit left at its
    // default, is looser than what was written, so each is refused. A key of a
    // section atomic knows is refused where it is written; a key of a model's
    // price, which is an open table, is refused when the settings are read.
    for (text, must, must_not) in [
        ("budget:\n  atom:\n    part: 2\n", "budget.atom.part", ""),
        (
            "budget:\n  atom:\n    tokens: most\n",
            "invalid type",
            "most",
        ),
        (
            "budget:\n  pricing:\n    gpt-test:\n      free: true\n",
            "unknown field",
            "free",
        ),
        (
            "budget:\n  pricing:\n    gpt-test:\n      input_usd_per_mtok: soon\n",
            "invalid type",
            "soon",
        ),
    ] {
        let (_home, home) = person(text);
        let said = resolved_where(
            &d,
            &EngineArgs::default(),
            &[("HOME", home.as_str())],
            &nobody,
        )
        .expect_err("refused")
        .to_string();
        assert!(said.contains(must), "{text}: {said}");
        if !must_not.is_empty() {
            assert!(!said.contains(must_not), "{text}: {said}");
        }
    }
}

#[test]
fn with_no_configuration_of_their_own_nothing_is_capped() {
    let d = dir(&[(
        "atomic.yaml",
        "atomic: 1\nname: notes\nbudget:\n  atom:\n    tokens: 400000\n",
    )]);
    let r = resolved(&d, &EngineArgs::default(), &[], &nobody);
    assert_eq!(r.settings.budget.atom.tokens, 400_000);
    assert_eq!(r.settings.budget.ceilings, Ceilings::default());
    assert_eq!(r.notes, Vec::<String>::new());
    assert_eq!(
        file_and_line(&r, "budget.atom.tokens"),
        ("atomic.yaml".to_owned(), 5)
    );
}

/// The settings a person reads, the denial of their own configuration directory
/// to every atom's commands, and the refusal of a state directory inside it are
/// made from one resolution: [`atomic_core::state::user_config_dir`], which
/// reads `XDG_CONFIG_HOME` and `HOME` as the operating system gives them and
/// falls back to the home the password database gives. Read the variables as
/// text instead — which is what [`Surroundings::env`] does, and what the test
/// helper above does because a test's environment says nothing about this
/// machine's user — the two name two directories as soon as one of them is
/// bytes that are not text, and settings would come from a directory no atom is
/// denied while the denial covered another. That is shown end to end by
/// `a_setting_in_a_configuration_directory_of_bytes_that_are_not_text_is_sourced_from_it`
/// in `crates/atomic/tests/state_cli.rs`, which gives a command such a
/// directory; what is fixed here is that a person's run asks for the directory
/// the same way both times, whatever this process's environment and password
/// database happen to give.
#[test]
fn settings_read_the_configuration_directory_the_denial_and_the_state_check_name() {
    let here = Surroundings::here();
    assert_eq!(
        user_config(here.user_config_dir),
        atomic_core::state::user_config_dir().map(|dir| dir.join("config.yaml")),
        "settings do not read the configuration directory the denial and the state \
         check are made from"
    );
}
