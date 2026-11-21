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
//! - a call reserves its whole bound, priced from the table;
//! - a cap smaller than one call's reservation fails configuration, naming
//!   the settings that make up the reservation;
//! - the caps the person who runs atomic set are ceilings: a written limit
//!   and an inherited default are both held to them, a smaller limit still
//!   tightens, a dollar ceiling binds where nothing else set a dollar cap,
//!   and a refusal says whose cap held;
//! - out-of-range settings and budget values are refused.

use super::*;
use crate::config::{Layers, Source, resolve as layered};
use serde_json::json;

/// Settings with everything the default call reserves inside every cap, so a
/// ceiling can be seen to bind without the room check getting in the way.
fn roomy() -> BudgetSettings {
    let mut settings = BudgetSettings::default();
    settings.call.input_tokens = 1_000;
    settings.call.output_tokens = 500;
    settings.call.minutes = 1.0;
    settings
}

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
    let declared = Budget::parse(&json!({"tokens": 100000, "minutes": 12.5})).expect("parse");
    assert_eq!(
        declared,
        Budget {
            tokens: Some(100_000),
            millis: Some(750_000),
            micro_usd: None,
        }
    );
    let r = atom(&declared, &BudgetSettings::default());
    assert_eq!((r.tokens, r.millis), (100_000, 750_000));
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
        (json!({"minutes": 1e300}), "/minutes", "sane size"),
        (json!({"tokens": 1u64 << 60}), "/tokens", "at most"),
        (json!({"cost_usd": 1e20}), "/cost_usd", "sane size"),
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
        "budget:\n  atom:\n    tokens: 123400\n  pricing:\n    gpt-test:\n      input_usd_per_mtok: 2.5\n      output_usd_per_mtok: 10\n",
    )
    .expect("write");
    let env = |name: &str| (name == "ATOMIC_BUDGET_ATOM_MINUTES").then(|| "12".to_owned());
    let resolved = layered::<Config>(&Layers {
        dir: &dir,
        user: None,
        profile: None,
        env: &env,
        env_names: vec!["ATOMIC_BUDGET_ATOM_MINUTES".into()],
        flags: Vec::new(),
    })
    .expect("layers");
    let settings = resolved.config.budget;
    assert_eq!(settings.atom.tokens, 123_400);
    assert!((settings.atom.minutes - 12.0).abs() < f64::EPSILON);
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

/// Every cap the person's own configuration writes here, in one place.
fn capped(tokens: u64, minutes: f64, cost_usd: f64) -> Caps {
    Caps {
        tokens: Some(tokens),
        minutes: Some(minutes),
        cost_usd: Some(cost_usd),
    }
}

#[test]
fn a_persons_cap_holds_a_written_limit_and_says_whose_cap_held() {
    let mut settings = roomy();
    // A dollar cap, theirs or the block's, needs a priced model to sit on.
    settings.pricing = priced().pricing;
    settings.ceilings.atom = capped(100_000, 10.0, 2.0);
    // A `budget` block above every cap of theirs: the smaller of the two
    // holds for each measure, and says that their cap was the smaller.
    let above =
        Budget::parse(&json!({"tokens": 400_000, "minutes": 25, "cost_usd": 9})).expect("parse");
    let r = resolve(&above, Level::Atom, &settings, &["gpt-test"]).expect("resolves");
    assert_eq!(
        (r.tokens, r.millis, r.micro_usd),
        (100_000, 600_000, Some(2_000_000)),
        "{r:?}"
    );
    assert_eq!(
        r.origins,
        [
            Origin::Ceiled("budget.atom.tokens".into()),
            Origin::Ceiled("budget.atom.minutes".into()),
            Origin::Ceiled("budget.atom.cost_usd".into()),
        ]
    );
    // Below their caps, the block's own numbers hold, and say they are the
    // block's: a project may only bring a cap down to what they allowed.
    let under =
        Budget::parse(&json!({"tokens": 5_000, "minutes": 5, "cost_usd": 0.5})).expect("parse");
    let r = resolve(&under, Level::Atom, &settings, &["gpt-test"]).expect("resolves");
    assert_eq!(
        (r.tokens, r.millis, r.micro_usd),
        (5_000, 300_000, Some(500_000)),
        "{r:?}"
    );
    assert_eq!(
        r.origins,
        [Origin::Declared, Origin::Declared, Origin::Declared,]
    );
    // A measure they cap, and the block writes nothing for: their cap holds
    // the inherited default too, whose source is no longer the whole story.
    settings.ceilings.atom = capped(100_000, 2.0, 2.0);
    let r = resolve(&Budget::default(), Level::Atom, &settings, &["gpt-test"]).expect("resolves");
    assert_eq!(r.tokens, 100_000);
    assert_eq!(r.origins[0], Origin::Ceiled("budget.atom.tokens".into()));
    assert_eq!(r.millis, 120_000);
    assert_eq!(r.origins[1], Origin::Ceiled("budget.atom.minutes".into()));
    // The default dollar cap is held the same way, and a measure they never
    // wrote keeps the configured default and its setting.
    let mut one = roomy();
    one.pricing = priced().pricing;
    one.ceilings.atom = Caps {
        tokens: None,
        minutes: None,
        cost_usd: Some(1.0),
    };
    one.atom.cost_usd = Some(7.0);
    let r = resolve(&Budget::default(), Level::Atom, &one, &["gpt-test"]).expect("resolves");
    assert_eq!(r.micro_usd, Some(1_000_000));
    assert_eq!(r.origins[2], Origin::Ceiled("budget.atom.cost_usd".into()));
    assert_eq!(r.tokens, BudgetSettings::default().atom.tokens);
    assert_eq!(r.origins[0], Origin::Default("budget.atom.tokens".into()));
}

#[test]
fn a_persons_dollar_cap_binds_where_nothing_else_set_one() {
    // Neither the block nor any layer above the person sets a dollar cap:
    // theirs still binds, by their setting, and needs the model priced.
    let mut settings = roomy();
    settings.pricing = priced().pricing;
    settings.ceilings.atom = capped(100_000, 1.0, 1.0);
    let r = resolve(&Budget::default(), Level::Atom, &settings, &["gpt-test"]).expect("resolves");
    assert_eq!(r.micro_usd, Some(1_000_000));
    assert_eq!(
        r.origins[2],
        Origin::Ceiled("budget.atom.cost_usd".into()),
        "the person's cap, by its setting"
    );
    settings.ceilings.workflow = capped(1_000_000, 60.0, 5.0);
    let w = resolve(
        &Budget::default(),
        Level::Workflow,
        &settings,
        &["gpt-test"],
    )
    .expect("resolves");
    assert_eq!(w.micro_usd, Some(5_000_000));
    // On a model they did not price, their cap refuses the run rather than
    // run it uncapped.
    settings.pricing.clear();
    assert!(matches!(
        resolve(&Budget::default(), Level::Atom, &settings, &["gpt-test"]),
        Err(BudgetError::Unpriced { model }) if model == "gpt-test"
    ));
}

#[test]
fn a_persons_cap_binds_its_own_level_by_its_own_setting() {
    let mut settings = roomy();
    settings.workflow = Limits {
        tokens: 5_000_000,
        minutes: 600.0,
        cost_usd: Some(50.0),
    };
    settings.ceilings.workflow = capped(1_000_000, 60.0, 5.0);
    let w = resolve(&Budget::default(), Level::Workflow, &settings, &[]).expect("resolves");
    assert_eq!(
        (w.tokens, w.millis, w.micro_usd),
        (1_000_000, 3_600_000, Some(5_000_000))
    );
    assert_eq!(
        w.origins[0],
        Origin::Ceiled("budget.workflow.tokens".into())
    );
    assert_eq!(
        w.origins[1],
        Origin::Ceiled("budget.workflow.minutes".into())
    );
    assert_eq!(
        w.origins[2],
        Origin::Ceiled("budget.workflow.cost_usd".into())
    );
    // A workflow run is one atom's parent, not its ceiling: their atom cap is
    // a separate setting, and unset it leaves the atom's default standing.
    let a = atom(&Budget::default(), &settings);
    assert_eq!(a.tokens, BudgetSettings::default().atom.tokens);
    assert_eq!(a.origins[0], Origin::Default("budget.atom.tokens".into()));
    // And the other way round: their atom cap says nothing about the run.
    let mut only_atom = roomy();
    only_atom.workflow = Limits {
        tokens: 5_000_000,
        minutes: 600.0,
        cost_usd: None,
    };
    only_atom.ceilings.atom.tokens = Some(120_000);
    let w = resolve(&Budget::default(), Level::Workflow, &only_atom, &[]).expect("resolves");
    assert_eq!(w.tokens, 5_000_000);
    assert_eq!(
        w.origins[0],
        Origin::Default("budget.workflow.tokens".into())
    );
    assert_eq!(atom(&Budget::default(), &only_atom).tokens, 120_000);
}

#[test]
fn a_persons_cap_with_no_room_for_one_call_names_whose_cap_held() {
    let mut settings = roomy();
    settings.ceilings.atom.tokens = Some(1_000);
    // The block wrote a cap with room; their cap took it below one call.
    let refused = resolve(
        &Budget::parse(&json!({"tokens": 100_000})).expect("parse"),
        Level::Atom,
        &settings,
        &["gpt-test"],
    )
    .expect_err("no room for one call");
    let message = refused.to_string();
    assert!(
        matches!(&refused, BudgetError::BelowCall(below)
            if below.field == "tokens" && below.from == "capped by your own budget.atom.tokens"),
        "{refused:?}"
    );
    for says in [
        "1000 tokens",
        "capped by your own budget.atom.tokens",
        "smaller than one call to model \"gpt-test\"",
    ] {
        assert!(message.contains(says), "{says:?} in {message}");
    }
}

#[test]
fn an_out_of_range_persons_cap_is_refused_as_the_setting_it_caps() {
    fn refused(change: impl Fn(&mut Ceilings), setting: &str) {
        let mut settings = priced();
        change(&mut settings.ceilings);
        let e =
            resolve(&Budget::default(), Level::Atom, &settings, &["gpt-test"]).expect_err(setting);
        assert!(
            matches!(&e, BudgetError::Default { setting: s, .. } if s == setting),
            "{setting}: {e:?}"
        );
    }
    refused(|c| c.atom.tokens = Some(0), "budget.atom.tokens");
    refused(
        |c| c.workflow.minutes = Some(-1.0),
        "budget.workflow.minutes",
    );
    refused(|c| c.atom.cost_usd = Some(f64::NAN), "budget.atom.cost_usd");
    refused(
        |c| c.call.input_tokens = Some(0),
        "budget.call.input_tokens",
    );
    refused(|c| c.call.minutes = Some(0.0), "budget.call.minutes");
    // Their prices are not checked here: they join the price table before
    // anything resolves, which is where a price is checked.
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
    let plain = Budget::parse(&json!({"tokens": 100_000})).expect("parse");
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
            input_tokens: 1_000,
            output_tokens: 500,
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

#[test]
fn a_cap_smaller_than_one_call_fails_configuration() {
    // The default call reserves 64000 + 16000 tokens and 10 minutes.
    let settings = BudgetSettings::default();
    let small = Budget::parse(&json!({"tokens": 20000})).expect("parse");
    let refused = resolve(&small, Level::Atom, &settings, &["synthetic"]).expect_err("too small");
    let message = refused.to_string();
    assert!(
        matches!(&refused, BudgetError::BelowCall(below) if below.field == "tokens"),
        "{refused:?}"
    );
    for says in [
        "20000 tokens",
        "80000 tokens",
        "written in its budget block",
        "budget.call.input_tokens + budget.call.output_tokens",
    ] {
        assert!(message.contains(says), "{says:?} in {message}");
    }
    // Time, against budget.call.minutes.
    let brief = Budget::parse(&json!({"minutes": 5})).expect("parse");
    let message = resolve(&brief, Level::Atom, &settings, &["synthetic"])
        .expect_err("too short")
        .to_string();
    assert!(message.contains("budget.call.minutes"), "{message}");
    // A model's own maximum is named when it makes up the reservation.
    let mut priced = priced();
    let message = resolve(
        &Budget::parse(&json!({"tokens": 1000})).expect("parse"),
        Level::Workflow,
        &priced,
        &["gpt-test"],
    )
    .expect_err("below 1500")
    .to_string();
    assert!(
        message.contains("budget.pricing.gpt-test.max_input_tokens"),
        "{message}"
    );
    // A dollar cap below one priced call: 7500 micro-dollars.
    let message = resolve(
        &Budget::parse(&json!({"cost_usd": 0.005})).expect("parse"),
        Level::Atom,
        &priced,
        &["gpt-test"],
    )
    .expect_err("below one call's cost")
    .to_string();
    assert!(message.contains("cost_usd cap"), "{message}");
    // A default that is too small is named as the default.
    priced.atom.tokens = 1_000;
    let message = resolve(&Budget::default(), Level::Atom, &priced, &["gpt-test"])
        .expect_err("default too small")
        .to_string();
    assert!(
        message.contains("the default budget.atom.tokens"),
        "{message}"
    );
    // Exactly one call's room resolves.
    let exact = Budget::parse(&json!({"tokens": 80000, "minutes": 10})).expect("parse");
    assert!(resolve(&exact, Level::Atom, &settings, &["synthetic"]).is_ok());
}

#[test]
fn out_of_range_settings_are_refused() {
    let refused = |change: &dyn Fn(&mut BudgetSettings), setting: &str| {
        let mut settings = priced();
        change(&mut settings);
        let e =
            resolve(&Budget::default(), Level::Atom, &settings, &["gpt-test"]).expect_err(setting);
        assert!(
            matches!(&e, BudgetError::Default { setting: s, .. } if s == setting),
            "{setting}: {e:?}"
        );
    };
    refused(&|s| s.call.minutes = 0.0, "budget.call.minutes");
    refused(&|s| s.call.minutes = f64::NAN, "budget.call.minutes");
    refused(&|s| s.call.minutes = f64::INFINITY, "budget.call.minutes");
    refused(&|s| s.call.output_tokens = 0, "budget.call.output_tokens");
    refused(&|s| s.atom.tokens = u64::MAX, "budget.atom.tokens");
    refused(&|s| s.workflow.minutes = -1.0, "budget.workflow.minutes");
    refused(
        &|s| s.atom.cost_usd = Some(f64::NAN),
        "budget.atom.cost_usd",
    );
    let price = |f: fn(&mut Price)| {
        move |s: &mut BudgetSettings| {
            if let Some(p) = s.pricing.get_mut("gpt-test") {
                f(p);
            }
        }
    };
    refused(
        &price(|p| p.input_usd_per_mtok = -1.0),
        "budget.pricing.gpt-test.input_usd_per_mtok",
    );
    refused(
        &price(|p| p.output_usd_per_mtok = f64::NAN),
        "budget.pricing.gpt-test.output_usd_per_mtok",
    );
    refused(
        &price(|p| p.max_output_tokens = Some(0)),
        "budget.pricing.gpt-test.max_output_tokens",
    );
    // A free model is priced at zero.
    let mut free = priced();
    if let Some(p) = free.pricing.get_mut("gpt-test") {
        p.input_usd_per_mtok = 0.0;
        p.output_usd_per_mtok = 0.0;
    }
    assert!(free.validate().is_ok());
    // A cost never passes LIMIT, so the ledger's sums cannot overflow.
    assert!(priced().cost("gpt-test", LIMIT, LIMIT) <= LIMIT);
}
