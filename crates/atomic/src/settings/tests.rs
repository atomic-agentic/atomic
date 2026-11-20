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
