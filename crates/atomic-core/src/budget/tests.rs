// SPDX-License-Identifier: Apache-2.0

//! The budget field, its resolved defaults and the price table:
//!
//! - an atom or workflow with no budget still has finite caps, each naming
//!   the default setting it came from;
//! - written limits win, one by one;
//! - a budget block must be positive and well formed;
//! - the defaults layer like every other setting, so `--explain` can say
//!   which layer set each one;
//! - a dollar cap on an unpriced model fails configuration with a hint to
//!   cap tokens or minutes; on a priced model it resolves;
//! - a call reserves its whole bound, priced from the table.

use super::*;
use crate::config::{Layers, Source, resolve as layered};
use serde_json::json;

fn atom(declared: &Budget, settings: &BudgetSettings) -> Resolved {
    resolve(declared, Level::Atom, settings, &["synthetic"]).expect("resolves")
}

#[test]
fn an_atom_with_no_budget_still_has_a_finite_resolved_cap() {
    let settings = BudgetSettings::default();
    let r = atom(&Budget::default(), &settings);
    assert!(r.tokens > 0 && r.tokens < u64::MAX, "{r:?}");
    assert!(r.millis > 0 && r.millis < u64::MAX, "{r:?}");
    assert_eq!(r.micro_usd, None, "no dollar cap until one is configured");
    assert_eq!(
        r.origins,
        [
            Origin::Default("budget.atom.tokens".into()),
            Origin::Default("budget.atom.minutes".into()),
            Origin::Default("budget.atom.cost_usd".into()),
        ]
    );
    let w = resolve(&Budget::default(), Level::Workflow, &settings, &[]).expect("resolves");
    assert!(w.tokens >= r.tokens && w.millis >= r.millis, "{w:?}");
    assert_eq!(
        w.origins[0],
        Origin::Default("budget.workflow.tokens".into())
    );
    // The default bound of one call fits inside the default caps.
    let call = settings.reservation("synthetic");
    assert!(call.tokens <= r.tokens && call.millis <= r.millis);
}

#[test]
fn written_limits_win_one_by_one() {
    let declared = Budget::parse(&json!({"tokens": 20000, "minutes": 1.5})).expect("parse");
    assert_eq!(
        declared,
        Budget {
            tokens: Some(20_000),
            millis: Some(90_000),
            micro_usd: None,
        }
    );
    let r = atom(&declared, &BudgetSettings::default());
    assert_eq!((r.tokens, r.millis), (20_000, 90_000));
    assert_eq!(r.origins[0], Origin::Declared);
    assert_eq!(r.origins[1], Origin::Declared);
    assert_eq!(r.origins[2], Origin::Default("budget.atom.cost_usd".into()));
}

#[test]
fn a_budget_block_must_be_positive_and_well_formed() {
    for (block, at, says) in [
        (json!({"tokens": 0}), "/tokens", "positive integer"),
        (json!({"tokens": 1.5}), "/tokens", "positive integer"),
        (json!({"tokens": -3}), "/tokens", "positive integer"),
        (json!({"minutes": 0}), "/minutes", "positive number"),
        (json!({"minutes": "5"}), "/minutes", "positive number"),
        (json!({"cost_usd": -1}), "/cost_usd", "positive number"),
        (json!({"dollars": 1}), "/dollars", "unknown budget field"),
        (json!(5), "", "mapping"),
    ] {
        let (pointer, message) = Budget::parse(&block).expect_err("refused");
        assert_eq!(pointer, at, "{block}");
        assert!(message.contains(says), "{block}: {message}");
    }
    assert_eq!(
        Budget::parse(&json!({"cost_usd": 0.25}))
            .expect("parse")
            .micro_usd,
        Some(250_000)
    );
}

#[test]
fn the_defaults_layer_like_every_other_setting() {
    #[derive(Default, Serialize, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    struct Config {
        budget: BudgetSettings,
    }
    let dir = std::env::temp_dir().join(format!(
        "atomic-budget-layers-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(
        dir.join("atomic.yaml"),
        "budget:\n  atom:\n    tokens: 1234\n  pricing:\n    gpt-test:\n      input_usd_per_mtok: 2.5\n      output_usd_per_mtok: 10\n",
    )
    .expect("write");
    let env = |name: &str| (name == "ATOMIC_BUDGET_ATOM_MINUTES").then(|| "7".to_owned());
    let resolved = layered::<Config>(&Layers {
        dir: &dir,
        profile: None,
        env: &env,
        env_names: vec!["ATOMIC_BUDGET_ATOM_MINUTES".into()],
        flags: Vec::new(),
    })
    .expect("layers");
    let settings = resolved.config.budget;
    assert_eq!(settings.atom.tokens, 1234);
    assert!((settings.atom.minutes - 7.0).abs() < f64::EPSILON);
    assert!(settings.pricing.contains_key("gpt-test"));
    // Each default the resolution names has a source to explain.
    let r = atom(&Budget::default(), &settings);
    let Origin::Default(tokens) = &r.origins[0] else {
        panic!("a default")
    };
    assert!(matches!(
        resolved.sources.get(tokens),
        Some(Source::File { line: 3, .. })
    ));
    let Origin::Default(minutes) = &r.origins[1] else {
        panic!("a default")
    };
    assert_eq!(
        resolved.sources.get(minutes),
        Some(&Source::Env("ATOMIC_BUDGET_ATOM_MINUTES".into()))
    );
    assert_eq!(
        resolved.sources.get("budget.workflow.tokens"),
        Some(&Source::Default)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn priced() -> BudgetSettings {
    let mut settings = BudgetSettings::default();
    settings.pricing.insert(
        "gpt-test".into(),
        Price {
            input_usd_per_mtok: 2.5,
            output_usd_per_mtok: 10.0,
            max_input_tokens: Some(1_000),
            max_output_tokens: Some(500),
        },
    );
    settings
}

#[test]
fn a_dollar_cap_on_an_unpriced_model_fails_configuration() {
    let capped = Budget::parse(&json!({"cost_usd": 1.0})).expect("parse");
    let settings = priced();
    let refused = resolve(&capped, Level::Atom, &settings, &["local-model"]).expect_err("unpriced");
    assert_eq!(
        refused,
        BudgetError::Unpriced {
            model: "local-model".into()
        }
    );
    let message = refused.to_string();
    assert!(
        message.contains("cap tokens or minutes instead"),
        "the hint names the alternative: {message}"
    );
    // A configured default dollar cap is held to the same rule.
    let mut defaulted = priced();
    defaulted.atom.cost_usd = Some(5.0);
    assert!(
        resolve(
            &Budget::default(),
            Level::Atom,
            &defaulted,
            &["local-model"]
        )
        .is_err()
    );
    // A workflow's cap needs every model it may call priced.
    assert!(
        resolve(
            &capped,
            Level::Workflow,
            &settings,
            &["gpt-test", "local-model"]
        )
        .is_err()
    );
    // Token and time caps need no pricing.
    let plain = Budget::parse(&json!({"tokens": 100})).expect("parse");
    assert!(resolve(&plain, Level::Atom, &settings, &["local-model"]).is_ok());
    // On a priced model the dollar cap holds.
    let r = resolve(&capped, Level::Atom, &settings, &["gpt-test"]).expect("priced");
    assert_eq!(r.micro_usd, Some(1_000_000));
    assert_eq!(r.origins[2], Origin::Declared);
}

#[test]
fn a_call_reserves_its_whole_bound_at_the_models_price() {
    let settings = priced();
    // The model's own maximum, priced: 1000 * 2.5 + 500 * 10 micro-dollars.
    assert_eq!(
        settings.reservation("gpt-test"),
        Reservation {
            tokens: 1_500,
            millis: 600_000,
            micro_usd: 7_500,
        }
    );
    // An unpriced model: the default bound, and no price.
    let other = settings.reservation("local-model");
    assert_eq!(other.tokens, 80_000);
    assert_eq!(other.micro_usd, 0);
    // Usage rounds up to whole micro-dollars.
    assert_eq!(settings.cost("gpt-test", 1, 0), 3);
    assert_eq!(settings.cost("gpt-test", 0, 0), 0);
    assert_eq!(settings.cost("local-model", 1_000, 1_000), 0);
}
