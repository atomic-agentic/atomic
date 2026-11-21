// SPDX-License-Identifier: Apache-2.0

//! The configuration layers: precedence, file and environment parsing, and
//! the errors a bad setting gives.

use super::*;

static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Model {
    base_url: String,
    name: Option<String>,
    timeout_secs: Option<u32>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct TestConfig {
    model: Model,
    retries: u32,
    verbose: bool,
    labels: BTreeMap<String, String>,
}

impl Default for TestConfig {
    fn default() -> Self {
        Self {
            model: Model {
                base_url: "http://127.0.0.1:8000/v1".into(),
                ..Model::default()
            },
            retries: 3,
            verbose: false,
            labels: BTreeMap::new(),
        }
    }
}

fn dir(files: &[(&str, &str)]) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "atomic-config-{}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&d).expect("create dir");
    for (name, text) in files {
        std::fs::write(d.join(name), text).expect("write file");
    }
    d
}

fn load(
    dir: &Path,
    profile: Option<&str>,
    env: &[(&str, &str)],
    flags: Vec<(String, Value)>,
) -> Result<Resolved<TestConfig>, ConfigError> {
    let map: HashMap<String, String> = env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let lookup = move |name: &str| map.get(name).cloned();
    resolve(&Layers {
        dir,
        user: None,
        profile,
        env: &lookup,
        env_names: env.iter().map(|(k, _)| k.to_string()).collect(),
        flags,
    })
}

#[test]
fn the_defaults_apply_with_no_files() {
    let d = dir(&[]);
    let r = load(&d, None, &[], vec![]).expect("load");
    assert_eq!(r.config, TestConfig::default());
    assert_eq!(r.sources.get("retries"), Some(&Source::Default));
}

#[test]
fn each_layer_overrides_the_one_below_it() {
    let d = dir(&[
        (
            "atomic.yaml",
            "retries: 4\nverbose: true\nmodel:\n  name: base\n",
        ),
        ("atomic.local.yaml", "retries: 5\nmodel:\n  name: local\n"),
        ("atomic.ci.yaml", "retries: 6\n"),
    ]);
    let stage =
        |profile, env: &[(&str, &str)], flags| load(&d, profile, env, flags).expect("load").config;
    assert_eq!(
        stage(None, &[], vec![]).retries,
        5,
        "local beats atomic.yaml"
    );
    assert!(
        stage(None, &[], vec![]).verbose,
        "atomic.yaml beats the default"
    );
    assert_eq!(
        stage(Some("ci"), &[], vec![]).retries,
        6,
        "the profile beats local"
    );
    assert_eq!(
        stage(Some("ci"), &[("ATOMIC_RETRIES", "7")], vec![]).retries,
        7,
        "env beats the profile"
    );
    let top = stage(
        Some("ci"),
        &[("ATOMIC_RETRIES", "7")],
        vec![("retries".into(), Value::from(8))],
    );
    assert_eq!(top.retries, 8, "a flag beats the environment");
    // Settings a layer does not mention keep the value from below.
    assert_eq!(top.model.name.as_deref(), Some("local"));
    assert_eq!(top.model.base_url, "http://127.0.0.1:8000/v1");

    let r = load(&d, Some("ci"), &[("ATOMIC_RETRIES", "7")], vec![]).expect("load");
    assert_eq!(
        r.sources.get("retries"),
        Some(&Source::Env("ATOMIC_RETRIES".into()))
    );
    assert_eq!(
        r.sources.get("model.name"),
        Some(&Source::File {
            path: d.join("atomic.local.yaml"),
            line: 3
        })
    );
}

#[test]
fn the_environment_beats_the_file_and_the_file_beats_the_defaults() {
    let d = dir(&[(
        "atomic.yaml",
        "model:\n  base_url: http://file\nretries: 9\n",
    )]);
    let r = load(&d, None, &[("ATOMIC_MODEL_BASE_URL", "http://env")], vec![]).expect("load");
    assert_eq!(r.config.model.base_url, "http://env");
    assert_eq!(r.config.retries, 9);
    assert!(!r.config.verbose);
}

#[test]
fn environment_names_are_forgiving() {
    let d = dir(&[]);
    for name in [
        "ATOMIC_MODEL_BASE_URL",
        "ATOMIC_MODEL_BASEURL",
        "ATOMIC_model-base.url",
    ] {
        let r = load(&d, None, &[(name, "http://x")], vec![]).expect("load");
        assert_eq!(r.config.model.base_url, "http://x", "{name}");
    }
    // Optional settings keep their type.
    let r = load(&d, None, &[("ATOMIC_MODEL_TIMEOUT_SECS", "30")], vec![]).expect("load");
    assert_eq!(r.config.model.timeout_secs, Some(30));
    // Other ATOMIC_* variables are not configuration and are left alone.
    load(&d, None, &[("ATOMIC_HOME", "/somewhere")], vec![]).expect("load");
}

#[test]
fn an_unknown_setting_is_refused_with_its_file_and_line() {
    let d = dir(&[(
        "atomic.yaml",
        "retries: 2\nmodel:\n  base_ulr: http://typo\n",
    )]);
    match load(&d, None, &[], vec![]) {
        Err(ConfigError::Setting {
            path,
            line,
            message,
        }) => {
            assert_eq!(path, d.join("atomic.yaml"));
            assert_eq!(line, 3);
            assert!(message.contains("model.base_ulr"), "{message}");
        }
        other => panic!("expected an unknown-setting error, got {other:?}"),
    }
}

#[test]
fn a_missing_selected_profile_is_an_error() {
    let d = dir(&[]);
    assert!(matches!(
        load(&d, Some("prod"), &[], vec![]),
        Err(ConfigError::Read { .. })
    ));
}

#[test]
fn a_bad_environment_value_is_refused() {
    let d = dir(&[]);
    assert!(matches!(
        load(&d, None, &[("ATOMIC_RETRIES", "lots")], vec![]),
        Err(ConfigError::Value { .. })
    ));
}

/// A number spelled a way the loader refuses is refused by the variable's own
/// name before any arm reads it, whatever the setting's type: an integer setting
/// would read `0200` as the number 200, and the string an optional setting falls
/// back to would say `1_000` where YAML 1.1 reads 1000. A file refuses every one
/// of these spellings, so the message is the loader's own.
#[test]
fn an_environment_value_refused_as_a_number_is_refused_by_name() {
    let d = dir(&[]);
    for (setting, value, why) in [
        ("ATOMIC_MODEL_TIMEOUT_SECS", "0200", "has a leading zero"),
        (
            "ATOMIC_MODEL_TIMEOUT_SECS",
            "007",
            "write 7, or 0o7 for octal",
        ),
        (
            "ATOMIC_MODEL_TIMEOUT_SECS",
            "-0x1F",
            "puts a sign before its hexadecimal prefix",
        ),
        (
            "ATOMIC_MODEL_TIMEOUT_SECS",
            "0x-1F",
            "puts a sign after its 0x",
        ),
        // Numbers YAML 1.1 reads alone, which used to fall back to a string.
        (
            "ATOMIC_MODEL_TIMEOUT_SECS",
            "0b11",
            "is a number in YAML 1.1",
        ),
        (
            "ATOMIC_MODEL_TIMEOUT_SECS",
            "1_000",
            "write 1000 for the number",
        ),
        (
            "ATOMIC_MODEL_TIMEOUT_SECS",
            "12:30",
            "write 750 for the number",
        ),
        (
            "ATOMIC_MODEL_TIMEOUT_SECS",
            "1_000.5",
            "write 1000.5 for the number",
        ),
        // A setting whose own default is a number read `0200` as 200, and one
        // whose default is a string took the text as it stands: both are
        // refused, and the refusal says which variable it is about.
        ("ATOMIC_RETRIES", "0200", "has a leading zero"),
        ("ATOMIC_RETRIES", "0b11", "is a number in YAML 1.1"),
        ("ATOMIC_MODEL_BASE_URL", "0200", "has a leading zero"),
        ("ATOMIC_MODEL_BASE_URL", "1_000", "is a number in YAML 1.1"),
    ] {
        match load(&d, None, &[(setting, value)], vec![]) {
            Err(ConfigError::Value { name, message }) => {
                assert_eq!(name, setting, "{value}: {message}");
                assert!(message.contains(why), "{setting}={value}: {message}");
                assert!(message.contains("quote it"), "{setting}={value}: {message}");
            }
            other => panic!("expected a value error for {setting}={value:?}, got {other:?}"),
        }
    }
    // A refusal that is no number spelling still gives the string a file gives
    // for its quoted form, which is what YAML 1.2 and JSON read: `yes` is a
    // boolean only in YAML 1.1, and `.inf` is a float in both versions that JSON
    // has no form for. A number or a bool keeps its type as it always did.
    let r = load(&d, None, &[("ATOMIC_MODEL_NAME", "yes")], vec![]).expect("load");
    assert_eq!(r.config.model.name.as_deref(), Some("yes"));
    let r = load(&d, None, &[("ATOMIC_MODEL_NAME", ".inf")], vec![]).expect("load");
    assert_eq!(r.config.model.name.as_deref(), Some(".inf"));
    let r = load(&d, None, &[("ATOMIC_MODEL_TIMEOUT_SECS", "30")], vec![]).expect("load");
    assert_eq!(r.config.model.timeout_secs, Some(30));
    let r = load(&d, None, &[("ATOMIC_MODEL_TIMEOUT_SECS", "0o20")], vec![]).expect("load");
    assert_eq!(r.config.model.timeout_secs, Some(16));
}

#[test]
fn an_open_section_takes_any_keys_and_a_section_is_not_a_value() {
    let d = dir(&[("atomic.yaml", "labels:\n  team: core\n  tier: gold\n")]);
    let r = load(&d, None, &[], vec![]).expect("load");
    assert_eq!(
        r.config.labels.get("tier").map(String::as_str),
        Some("gold")
    );
    let d = dir(&[("atomic.yaml", "model: http://flat\n")]);
    assert!(matches!(
        load(&d, None, &[], vec![]),
        Err(ConfigError::Setting { line: 1, .. })
    ));
}

#[test]
fn an_unknown_flag_is_refused() {
    let d = dir(&[]);
    assert!(matches!(
        load(&d, None, &[], vec![("retires".into(), Value::from(1))]),
        Err(ConfigError::UnknownFlag(_))
    ));
}

#[test]
fn an_error_never_repeats_the_value_it_found() {
    const SECRET: &str = "sk-not-a-number";
    let errors = [
        // A variable of the wrong type.
        load(&dir(&[]), None, &[("ATOMIC_RETRIES", SECRET)], vec![]),
        // A value where a section belongs.
        load(
            &dir(&[("atomic.yaml", &format!("model: {SECRET}\n"))]),
            None,
            &[],
            vec![],
        ),
        // A value the type refuses, after the layers merge.
        load(
            &dir(&[("atomic.yaml", &format!("verbose: {SECRET}\n"))]),
            None,
            &[],
            vec![],
        ),
        load(
            &dir(&[]),
            None,
            &[],
            vec![("retries".into(), Value::String(SECRET.into()))],
        ),
    ];
    for error in errors {
        let message = error.expect_err("refused").to_string();
        assert!(!message.contains(SECRET), "{message}");
        assert!(!message.is_empty());
    }
}

#[test]
fn a_variable_set_to_nothing_sets_nothing() {
    let d = dir(&[("atomic.yaml", "model:\n  name: from-the-file\n")]);
    let r = load(
        &d,
        None,
        &[("ATOMIC_MODEL_NAME", ""), ("ATOMIC_RETRIES", "")],
        vec![],
    )
    .expect("load");
    assert_eq!(r.config.model.name.as_deref(), Some("from-the-file"));
    assert_eq!(r.config.retries, 3);
    assert!(matches!(r.sources["retries"], Source::Default));
}

#[test]
fn a_quote_inside_a_value_does_not_let_the_rest_of_it_out() {
    let shown =
        without_values(r#"invalid type: string "a\"sk-secret\" b", expected u32 for `retries`"#);
    assert_eq!(shown, r#"invalid type: string "…", expected u32 for `…`"#);
    assert_eq!(
        without_values("unknown variant `sk-secret`, expected `local` or `process`"),
        "unknown variant `…`, expected `…` or `…`"
    );
    assert_eq!(without_values(r#"unclosed "sk-secret"#), r#"unclosed "…""#);
}

#[test]
fn the_persons_own_file_is_below_the_projects() {
    let user = dir(&[(
        "config.yaml",
        "retries: 9\nverbose: true\nmodel:\n  name: mine\n",
    )]);
    let d = dir(&[("atomic.yaml", "retries: 5\n")]);
    let lookup = |_: &str| None;
    let load_with = |file: &str| {
        resolve::<TestConfig>(&Layers {
            dir: &d,
            user: Some(&user.join(file)),
            profile: None,
            env: &lookup,
            env_names: vec![],
            flags: vec![],
        })
    };
    let r = load_with("config.yaml").expect("load");
    assert_eq!(r.config.retries, 5, "the project's file wins");
    assert!(
        r.config.verbose,
        "what the project leaves, the person's file sets"
    );
    assert_eq!(r.config.model.name.as_deref(), Some("mine"));
    assert!(matches!(
        &r.sources["verbose"],
        Source::File { path, line: 2 } if path.ends_with("config.yaml")
    ));
    // A person's file that is missing is no error.
    load_with("missing.yaml").expect("load");
}
