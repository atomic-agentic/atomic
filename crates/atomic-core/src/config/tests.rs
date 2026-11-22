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

/// One model's price, as `budget.pricing` holds one: keyed by model name, in a
/// section whose default is empty.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Price {
    input_usd_per_mtok: f64,
    output_usd_per_mtok: f64,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct TestConfig {
    model: Model,
    retries: u32,
    verbose: bool,
    labels: BTreeMap<String, String>,
    pricing: BTreeMap<String, Price>,
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
            pricing: BTreeMap::new(),
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

/// A value of the kind the setting takes but of a type it will not hold is
/// refused by the variable's own name: `resolve` reads the value alone,
/// against a copy of the defaults, so the refusal names the variable. The
/// same value reaching the merged whole is refused too, but as one about the
/// configuration, which says nothing about where to look.
#[test]
fn a_value_the_setting_s_own_type_refuses_names_the_variable() {
    // `soon` is what an optional number's variable gives once it is parsed --
    // a file would read it as the string "soon" -- so only the setting's own
    // type can refuse it, and only the per-variable read sees it by name.
    let shown = match load(
        &dir(&[]),
        None,
        &[("ATOMIC_MODEL_TIMEOUT_SECS", "soon")],
        vec![],
    ) {
        Err(ConfigError::Value { name, message }) => {
            assert_eq!(name, "ATOMIC_MODEL_TIMEOUT_SECS");
            message
        }
        other => panic!(
            "ATOMIC_MODEL_TIMEOUT_SECS=soon: expected a refusal naming the variable, got {other:?}"
        ),
    };
    // The type's own reason, with the word it could not read hidden.
    assert_eq!(shown, "invalid type: string \"…\", expected u32");

    // The same word in a file is the same type refusing it, and there is no
    // variable to name: it comes out as a refusal of the merged whole.
    let d = dir(&[("atomic.yaml", "model:\n  timeout_secs: soon\n")]);
    match load(&d, None, &[], vec![]) {
        Err(ConfigError::Invalid(message)) => {
            assert_eq!(message, "invalid type: string \"…\", expected u32");
        }
        other => panic!(
            "a file's model.timeout_secs: expected a refusal of the merged whole, got {other:?}"
        ),
    }
    // What the type does hold is still taken from the variable.
    let r = load(
        &dir(&[]),
        None,
        &[("ATOMIC_MODEL_TIMEOUT_SECS", "30")],
        vec![],
    )
    .expect("load");
    assert_eq!(r.config.model.timeout_secs, Some(30));
}

/// A value for an optional setting is read as a YAML scalar, and the string it
/// falls back to is faithful only where every YAML version reads that text as a
/// string. `0200` is not such a spelling: this branch read it as the number 200
/// until the loader began refusing it, so it is refused here too, with the
/// loader's own message.
#[test]
fn an_environment_value_refused_as_a_number_is_refused_by_name() {
    let d = dir(&[]);
    for (value, why) in [
        ("0200", "has a leading zero"),
        ("007", "write 7, or 0o7 for octal"),
        ("-0x1F", "puts a sign before its hexadecimal prefix"),
        ("0x-1F", "puts a sign after its 0x"),
    ] {
        match load(&d, None, &[("ATOMIC_MODEL_TIMEOUT_SECS", value)], vec![]) {
            Err(ConfigError::Value { message, .. }) => {
                assert!(message.contains(why), "{value}: {message}");
                assert!(message.contains("quote it"), "{value}: {message}");
            }
            other => panic!("expected a value error for {value:?}, got {other:?}"),
        }
    }
    // Every other refused spelling is a string to both YAML versions, so the
    // fallback gives what a file would give for the quoted form, and a number
    // or a bool keeps its type as it always did.
    let d = dir(&[]);
    let r = load(&d, None, &[("ATOMIC_MODEL_NAME", "yes")], vec![]).expect("load");
    assert_eq!(r.config.model.name.as_deref(), Some("yes"));
    let r = load(&d, None, &[("ATOMIC_MODEL_NAME", ".inf")], vec![]).expect("load");
    assert_eq!(r.config.model.name.as_deref(), Some(".inf"));
    let r = load(&d, None, &[("ATOMIC_MODEL_NAME", "1_000")], vec![]).expect("load");
    assert_eq!(r.config.model.name.as_deref(), Some("1_000"));
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

/// A section whose default is empty is open, so it is one value and not a
/// mapping of settings: the layer that writes it replaces the whole table
/// rather than merging per key. `budget.pricing` is such a section, so a layer
/// that writes it leaves every model it does not name unpriced, whatever a
/// layer below said about them.
#[test]
fn a_layer_writing_an_open_section_replaces_the_whole_table() {
    let priced = "pricing:\n  gpt-test:\n    input_usd_per_mtok: 2.5\n    \
                  output_usd_per_mtok: 10\n";
    let other = "pricing:\n  other:\n    input_usd_per_mtok: 0\n    output_usd_per_mtok: 0\n";
    // A file over a file.
    let d = dir(&[("atomic.yaml", priced), ("atomic.local.yaml", other)]);
    let r = load(&d, None, &[], vec![]).expect("load");
    assert_eq!(
        r.config
            .pricing
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["other"],
        "the table below is gone whole, not merged in per model"
    );
    assert_eq!(r.config.pricing["other"].input_usd_per_mtok, 0.0);
    // The section is one setting, so it is cited as one: the layer that wrote
    // it, at the line its entries start on.
    assert!(
        matches!(
            &r.sources["pricing"],
            Source::File { path, line } if path == &d.join("atomic.local.yaml") && *line == 2
        ),
        "the source names the layer that wrote the table, not the one below: {:?}",
        r.sources.get("pricing")
    );

    // A variable over a file, which is how `ATOMIC_BUDGET_PRICING` writes it.
    let d = dir(&[("atomic.yaml", priced)]);
    let r = load(
        &d,
        None,
        &[(
            "ATOMIC_PRICING",
            r#"{"other":{"input_usd_per_mtok":0,"output_usd_per_mtok":0}}"#,
        )],
        vec![],
    )
    .expect("load");
    assert_eq!(
        r.config
            .pricing
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["other"],
        "the variable's table replaces the file's whole"
    );
    assert!(
        matches!(
            r.sources.get("pricing"),
            Some(Source::Env(name)) if name == "ATOMIC_PRICING"
        ),
        "{:?}",
        r.sources.get("pricing")
    );
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
