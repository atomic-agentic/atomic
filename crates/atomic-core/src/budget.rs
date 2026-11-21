// SPDX-License-Identifier: Apache-2.0

//! Budgets: the `budget` field of atoms and workflows, the finite defaults
//! omitted limits inherit, and the model price table a dollar cap needs.
//!
//! A `budget` block takes three limits, each optional and each positive:
//!
//! ```yaml
//! budget:
//!   tokens: 200000   # cumulative input and output tokens of real calls
//!   minutes: 20      # cumulative active execution, excluding gate waits
//!   cost_usd: 1.00   # cumulative model-call cost, where pricing is reliable
//! ```
//!
//! A limit that is not written inherits a default from [`BudgetSettings`], a
//! configuration section (`budget`) layered like every other setting, so
//! `atomic config --explain` can show where each one came from. Tokens and
//! time always resolve to a finite cap; a dollar cap applies only when one
//! is written or configured, and then only for a model the price table
//! prices: [`resolve`] rejects it otherwise rather than claim a bound it
//! cannot keep.
//!
//! Caps the person who runs atomic set in their own configuration are the
//! exception to that layering, and [`Ceilings`] carries them: a project's
//! file, a variable and a written limit may only bring a cap down to one of
//! them, never above it.
//!
//! Caps are held as integers: tokens, milliseconds and micro-dollars
//! (millionths of a US dollar). A price in US dollars per million tokens is
//! therefore exactly a price in micro-dollars per token. Every amount is at
//! most [`LIMIT`], so the ledger's sums cannot overflow.
//!
//! A cap must leave room for at least one call: [`resolve`] refuses a cap
//! smaller than what one call to a model it covers reserves, since such a
//! budget could never admit anything.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// The limits a `budget` block wrote; `None` inherits the default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    /// Cumulative input and output tokens.
    pub tokens: Option<u64>,
    /// Cumulative active execution, in milliseconds (written as `minutes`).
    pub millis: Option<u64>,
    /// Cumulative model-call cost, in micro-dollars (written as `cost_usd`).
    pub micro_usd: Option<u64>,
}

/// The fields a `budget` block may have.
pub const FIELDS: &[&str] = &["tokens", "minutes", "cost_usd"];

/// The most any one amount may be, in its integer unit (tokens,
/// milliseconds or micro-dollars): 2^53, the largest integer every JSON
/// reader keeps exactly. Far past any real budget, and far enough below
/// the ledger's 64-bit columns that its sums cannot overflow.
pub const LIMIT: u64 = 1 << 53;

/// `n` as an integer amount, if it is finite, positive and at most
/// [`LIMIT`] once `scale`d and rounded (at least one).
fn amount(n: f64, scale: f64) -> Option<u64> {
    let scaled = (n * scale).round();
    // In range: finite, positive and at most LIMIT, checked first.
    (n.is_finite() && n > 0.0 && scaled <= LIMIT as f64).then(|| (scaled as u64).max(1))
}

impl Budget {
    /// Read a `budget` block: an object with any of [`FIELDS`], each a
    /// positive number, `tokens` an integer. On a bad block, the JSON
    /// Pointer below the block that is wrong and why.
    pub fn parse(value: &Value) -> Result<Self, (String, String)> {
        let Value::Object(fields) = value else {
            return Err((
                String::new(),
                "budget is a mapping of tokens, minutes and cost_usd".into(),
            ));
        };
        let mut budget = Self::default();
        for (key, v) in fields {
            let at = format!("/{key}");
            match key.as_str() {
                "tokens" => {
                    budget.tokens = Some(v.as_u64().filter(|n| *n > 0 && *n <= LIMIT).ok_or_else(
                        || {
                            (
                                at.clone(),
                                format!("tokens must be a positive integer, at most {LIMIT}"),
                            )
                        },
                    )?);
                }
                "minutes" => {
                    budget.millis = Some(v.as_f64().and_then(|m| amount(m, 60_000.0)).ok_or_else(
                        || {
                            (
                                at.clone(),
                                "minutes must be a positive number of a sane size".to_owned(),
                            )
                        },
                    )?);
                }
                "cost_usd" => {
                    budget.micro_usd = Some(
                        v.as_f64()
                            .and_then(|usd| amount(usd, 1_000_000.0))
                            .ok_or_else(|| {
                                (
                                    at.clone(),
                                    "cost_usd must be a positive number of a sane size".to_owned(),
                                )
                            })?,
                    );
                }
                other => {
                    return Err((
                        at,
                        format!(
                            "unknown budget field `{other}`: a budget has tokens, minutes and \
                             cost_usd"
                        ),
                    ));
                }
            }
        }
        Ok(budget)
    }
}

/// The limits an atom or workflow inherits when it writes none.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Cumulative tokens.
    pub tokens: u64,
    /// Cumulative active minutes.
    pub minutes: f64,
    /// A dollar cap, or `None` for none.
    pub cost_usd: Option<f64>,
}

/// The most one model call may use, as a provider profile documents it.
/// Admission reserves all of it before the call is dispatched.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallBound {
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens, including reasoning.
    pub output_tokens: u64,
    /// Active minutes.
    pub minutes: f64,
}

/// One model's price, in US dollars per million tokens, and optionally its
/// own maximum per call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Price {
    /// Per million input tokens.
    pub input_usd_per_mtok: f64,
    /// Per million output tokens.
    pub output_usd_per_mtok: f64,
    /// The most input tokens one call to this model takes, if not the
    /// default call bound's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    /// The most output tokens one call to this model returns, if not the
    /// default call bound's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
}

/// A cap the person's own configuration wrote, in the units the `budget`
/// section is written in. `None` caps nothing: a measure they leave unset
/// keeps whatever a project's file or a written `budget` block says.
// Not `deny_unknown_fields`, as the person's own file is read field by
// field and these are only ever read back from a budget plan, which reads
// past the fields a later version adds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Caps {
    /// Cumulative tokens.
    pub tokens: Option<u64>,
    /// Cumulative active minutes.
    pub minutes: Option<f64>,
    /// A dollar cap, or `None` for none.
    pub cost_usd: Option<f64>,
}

/// The most one model call the person's own configuration allows, as written.
// Read past later fields, as [`Caps`] is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CallCaps {
    /// The most input tokens one call takes.
    pub input_tokens: Option<u64>,
    /// The most output tokens one call returns.
    pub output_tokens: Option<u64>,
    /// The most active minutes one call takes.
    pub minutes: Option<f64>,
}

/// The caps the person who runs atomic set in their own configuration, which
/// nothing above it may raise.
///
/// A ceiling is a cap on the cap: for each measure it sets, the effective cap
/// is the smaller of it and what the project's files, an `ATOMIC_*` variable
/// or the atom's or workflow's own `budget` block says. A measure it leaves
/// unset caps nothing. Its dollar ceiling binds even where nothing else sets
/// a dollar cap. Where a ceiling binds, the cap's origin is
/// [`Origin::Ceiled`], so a refusal says whose cap held.
///
/// Ceilings are not a setting: no file may write `budget.ceilings`, and they
/// come only from the person's own configuration file, which the configuration
/// layer of the command reads into [`BudgetSettings::ceilings`]. The command
/// also holds [`BudgetSettings::atom`], [`BudgetSettings::workflow`] and
/// [`BudgetSettings::call`] to them, so what `atomic config` shows is what a
/// run is held to.
// Read past later fields, as [`Caps`] is: it is `serde(skip)` in
// [`BudgetSettings`], so no configuration file is read through it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ceilings {
    /// Ceilings on what an atom invocation may spend.
    pub atom: Caps,
    /// Ceilings on what a workflow run, children included, may spend.
    pub workflow: Caps,
    /// Ceilings on the most one model call may use.
    pub call: CallCaps,
}

impl Ceilings {
    /// The ceilings on `level`'s caps, as a `budget` block writes them:
    /// tokens, milliseconds and micro-dollars. In range:
    /// [`BudgetSettings::validate`] refuses a ceiling that is not, before
    /// anything is resolved, and a ceiling too large to hold is no ceiling on
    /// a cap that is at most [`LIMIT`] anyway.
    pub fn held_by(&self, level: Level) -> Budget {
        let caps = match level {
            Level::Atom => &self.atom,
            Level::Workflow => &self.workflow,
        };
        Budget {
            tokens: caps.tokens,
            millis: caps.minutes.and_then(|m| amount(m, 60_000.0)),
            micro_usd: caps.cost_usd.and_then(|u| amount(u, 1_000_000.0)),
        }
    }
}

/// The `budget` configuration section.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BudgetSettings {
    /// What an atom invocation inherits.
    pub atom: Limits,
    /// What a workflow run, children included, inherits.
    pub workflow: Limits,
    /// The default maximum of one model call.
    pub call: CallBound,
    /// The price table, by model name. A model it does not name is unpriced:
    /// a dollar cap on it fails configuration.
    pub pricing: BTreeMap<String, Price>,
    /// The person's own caps, which hold over every other layer and over a
    /// written `budget` block (see [`Ceilings`]). Not a setting: it is read
    /// from the person's own configuration file, never from a project's, and
    /// a file that writes `budget.ceilings` is refused as an unknown setting.
    #[serde(skip, default)]
    pub ceilings: Ceilings,
}

/// The built-in defaults. Finite for tokens and time; no dollar cap, since
/// nothing is priced until the project says so.
impl Default for BudgetSettings {
    fn default() -> Self {
        Self {
            atom: Limits {
                tokens: 500_000,
                minutes: 30.0,
                cost_usd: None,
            },
            workflow: Limits {
                tokens: 2_000_000,
                minutes: 120.0,
                cost_usd: None,
            },
            call: CallBound {
                input_tokens: 64_000,
                output_tokens: 16_000,
                minutes: 10.0,
            },
            pricing: BTreeMap::new(),
            ceilings: Ceilings::default(),
        }
    }
}

/// Where a resolved cap came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Origin {
    /// The atom's or workflow's own `budget` block.
    Declared,
    /// A default, by its setting path, such as `budget.atom.tokens`; the
    /// configuration's sources say which layer set it.
    Default(String),
    /// The person's own cap, by its setting path: it was the smaller of it
    /// and what the block wrote or the default said, so it holds.
    Ceiled(String),
}

/// A budget after defaults: every cap that applies, and where each came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolved {
    /// Cumulative tokens.
    pub tokens: u64,
    /// Cumulative active milliseconds.
    pub millis: u64,
    /// Cumulative micro-dollars, if a dollar cap applies.
    pub micro_usd: Option<u64>,
    /// Where `tokens`, `millis` and `micro_usd` came from, in that order.
    pub origins: [Origin; 3],
}

/// What is budgeted: an atom invocation or a whole workflow run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// One atom invocation.
    Atom,
    /// A workflow run and all its children.
    Workflow,
}

impl Level {
    fn key(self) -> &'static str {
        match self {
            Self::Atom => "atom",
            Self::Workflow => "workflow",
        }
    }
}

/// Why a budget cannot be resolved.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BudgetError {
    /// A dollar cap on a model the price table does not price.
    #[error(
        "cost_usd needs reliable pricing, and the price table has none for model {model:?}; \
         cap tokens or minutes instead (subscriptions and local models have no dollar price), \
         or add the model under budget.pricing"
    )]
    Unpriced {
        /// The model.
        model: String,
    },
    /// A configured setting is out of range: not finite, not positive where
    /// it must be, or past [`LIMIT`].
    #[error("{setting} {message}")]
    Default {
        /// The setting, such as `budget.atom.minutes`.
        setting: String,
        /// What is wrong with it.
        message: String,
    },
    /// A cap smaller than what one call reserves: it could never admit a
    /// call.
    #[error("{0}")]
    BelowCall(Box<BelowCall>),
}

/// A cap smaller than what one call to a model reserves.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "the {level} {field} cap ({cap}{unit}, {from}) is smaller than one call to model {model:?} \
     reserves ({need}{unit}, from {bound}); raise the cap, or lower {bound}"
)]
pub struct BelowCall {
    /// `atom` or `workflow`.
    pub level: String,
    /// `tokens`, `minutes` or `cost_usd`.
    pub field: String,
    /// The cap, in the field's unit.
    pub cap: String,
    /// What one call reserves, in the same unit.
    pub need: String,
    /// The unit, such as ` tokens`.
    pub unit: String,
    /// Where the cap came from: written in the budget block, the default's
    /// setting, or the person's own cap that bounds it.
    pub from: String,
    /// The model.
    pub model: String,
    /// The settings that make up one call's reservation, such as
    /// `budget.call.input_tokens + budget.call.output_tokens`.
    pub bound: String,
}

fn out_of_range(setting: String, message: &str) -> BudgetError {
    BudgetError::Default {
        setting,
        message: message.to_owned(),
    }
}

impl BudgetSettings {
    /// Check every setting is in range: caps, the call bound and prices
    /// finite, positive (a price may be zero) and at most [`LIMIT`] in their
    /// integer units, the person's own caps with them.
    pub fn validate(&self) -> Result<(), BudgetError> {
        let count = |setting: String, n: u64| {
            if n == 0 || n > LIMIT {
                Err(out_of_range(
                    setting,
                    &format!("must be a positive integer, at most {LIMIT}"),
                ))
            } else {
                Ok(())
            }
        };
        let scaled = |setting: String, n: f64, scale: f64| {
            amount(n, scale)
                .map(|_| ())
                .ok_or_else(|| out_of_range(setting, "must be a positive number of a sane size"))
        };
        for (level, limits) in [("atom", &self.atom), ("workflow", &self.workflow)] {
            count(format!("budget.{level}.tokens"), limits.tokens)?;
            scaled(format!("budget.{level}.minutes"), limits.minutes, 60_000.0)?;
            if let Some(usd) = limits.cost_usd {
                scaled(format!("budget.{level}.cost_usd"), usd, 1_000_000.0)?;
            }
        }
        count("budget.call.input_tokens".into(), self.call.input_tokens)?;
        count("budget.call.output_tokens".into(), self.call.output_tokens)?;
        scaled("budget.call.minutes".into(), self.call.minutes, 60_000.0)?;
        // A person's own cap is held to the same rule as the limit it caps,
        // under the same setting name: it is that setting, in their file.
        for (level, caps) in [
            ("atom", &self.ceilings.atom),
            ("workflow", &self.ceilings.workflow),
        ] {
            if let Some(n) = caps.tokens {
                count(format!("budget.{level}.tokens"), n)?;
            }
            if let Some(minutes) = caps.minutes {
                scaled(format!("budget.{level}.minutes"), minutes, 60_000.0)?;
            }
            if let Some(usd) = caps.cost_usd {
                scaled(format!("budget.{level}.cost_usd"), usd, 1_000_000.0)?;
            }
        }
        if let Some(n) = self.ceilings.call.input_tokens {
            count("budget.call.input_tokens".into(), n)?;
        }
        if let Some(n) = self.ceilings.call.output_tokens {
            count("budget.call.output_tokens".into(), n)?;
        }
        if let Some(minutes) = self.ceilings.call.minutes {
            scaled("budget.call.minutes".into(), minutes, 60_000.0)?;
        }
        for (model, price) in &self.pricing {
            let at = |field: &str| format!("budget.pricing.{model}.{field}");
            for (field, usd) in [
                ("input_usd_per_mtok", price.input_usd_per_mtok),
                ("output_usd_per_mtok", price.output_usd_per_mtok),
            ] {
                // A price may be zero, never negative, and a million tokens
                // at it stays under LIMIT micro-dollars.
                if !(usd.is_finite() && usd >= 0.0 && usd * 1e6 <= LIMIT as f64) {
                    return Err(out_of_range(
                        at(field),
                        "must be a number of US dollars, zero or more, of a sane size",
                    ));
                }
            }
            if let Some(n) = price.max_input_tokens {
                count(at("max_input_tokens"), n)?;
            }
            if let Some(n) = price.max_output_tokens {
                count(at("max_output_tokens"), n)?;
            }
        }
        Ok(())
    }
}

/// The cap one measure ends with: what its `budget` block wrote (`declared`),
/// or `default` when it wrote nothing, held to `ceiling` when the person's own
/// cap for the measure is smaller. With it, where that cap came from: the
/// block, the default, or the ceiling that bound it, by its setting `path`.
fn hold(declared: Option<u64>, default: u64, ceiling: Option<u64>, path: &str) -> (u64, Origin) {
    let (want, from) = match declared {
        Some(w) => (w, Origin::Declared),
        None => (default, Origin::Default(path.to_owned())),
    };
    match ceiling {
        Some(c) if c < want => (c, Origin::Ceiled(path.to_owned())),
        _ => (want, from),
    }
}

/// Resolve `declared` for `level` against `settings`, for calls to `models`
/// (every model the atom or workflow may call). A dollar cap, written or
/// inherited, needs every one of them priced, and every cap must leave room
/// for one call to each of them.
///
/// Every cap is the smaller of what the block or the configured default says
/// and what `settings`' [`Ceilings`] cap it to: a written `budget` block can
/// only bring a cap down to a person's own cap, never above it.
pub fn resolve(
    declared: &Budget,
    level: Level,
    settings: &BudgetSettings,
    models: &[&str],
) -> Result<Resolved, BudgetError> {
    settings.validate()?;
    let limits = match level {
        Level::Atom => &settings.atom,
        Level::Workflow => &settings.workflow,
    };
    let path = |field: &str| format!("budget.{}.{field}", level.key());
    // In range: validated above.
    let millis_of = |minutes: f64| amount(minutes, 60_000.0).unwrap_or(LIMIT);
    let micro_of = |usd: f64| amount(usd, 1_000_000.0).unwrap_or(LIMIT);
    // What the person who runs atomic caps this level at, in these units.
    let capped = settings.ceilings.held_by(level);
    let (tokens, tokens_from) = hold(
        declared.tokens,
        limits.tokens,
        capped.tokens,
        &path("tokens"),
    );
    let (millis, millis_from) = hold(
        declared.millis,
        millis_of(limits.minutes),
        capped.millis,
        &path("minutes"),
    );
    let (micro_usd, cost_from) = match (declared.micro_usd, limits.cost_usd) {
        (Some(c), _) => (Some(c), Origin::Declared),
        (None, Some(usd)) => (Some(micro_of(usd)), Origin::Default(path("cost_usd"))),
        (None, None) => (None, Origin::Default(path("cost_usd"))),
    };
    // The person's dollar cap holds a larger one down, and stands where
    // nothing else set one: whatever a block or a layer above them leaves
    // out, their cap still binds.
    let (micro_usd, cost_from) = match (micro_usd, capped.micro_usd) {
        (Some(c), Some(k)) if k < c => (Some(k), Origin::Ceiled(path("cost_usd"))),
        (None, Some(k)) => (Some(k), Origin::Ceiled(path("cost_usd"))),
        _ => (micro_usd, cost_from),
    };
    if micro_usd.is_some()
        && let Some(model) = models.iter().find(|m| !settings.pricing.contains_key(**m))
    {
        return Err(BudgetError::Unpriced {
            model: (*model).to_owned(),
        });
    }
    let resolved = Resolved {
        tokens,
        millis,
        micro_usd,
        origins: [tokens_from, millis_from, cost_from],
    };
    for model in models {
        settings.check_room(&resolved, level, model)?;
    }
    Ok(resolved)
}

/// What one call to a model reserves, and the provider's documented maximum
/// for it, as tokens, milliseconds and micro-dollars.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    /// The input tokens it may take.
    pub input_tokens: u64,
    /// The output tokens it may return, reasoning included: a call is sent
    /// with this as its output limit.
    pub output_tokens: u64,
    /// Input and output tokens together.
    pub tokens: u64,
    /// Active milliseconds: a call is cut off when they run out.
    pub millis: u64,
    /// Micro-dollars at the model's price; zero when it is unpriced.
    pub micro_usd: u64,
}

impl BudgetSettings {
    /// The whole bound of one call to `model`: admission reserves all of it.
    pub fn reservation(&self, model: &str) -> Reservation {
        let price = self.pricing.get(model);
        let input = price
            .and_then(|p| p.max_input_tokens)
            .unwrap_or(self.call.input_tokens)
            .min(LIMIT);
        let output = price
            .and_then(|p| p.max_output_tokens)
            .unwrap_or(self.call.output_tokens)
            .min(LIMIT);
        Reservation {
            input_tokens: input,
            output_tokens: output,
            tokens: input.saturating_add(output),
            millis: amount(self.call.minutes, 60_000.0).unwrap_or(LIMIT),
            micro_usd: price.map_or(0, |p| cost(p, input, output)),
        }
    }

    /// What `input` and `output` tokens on `model` cost, in micro-dollars,
    /// rounded up; zero when it is unpriced.
    pub fn cost(&self, model: &str, input: u64, output: u64) -> u64 {
        self.pricing
            .get(model)
            .map_or(0, |p| cost(p, input, output))
    }

    /// Refuse `caps` at `level` if one call to `model` cannot fit in them.
    pub fn check_room(
        &self,
        caps: &Resolved,
        level: Level,
        model: &str,
    ) -> Result<(), BudgetError> {
        let call = self.reservation(model);
        let priced = self.pricing.get(model);
        let tokens_from = match (
            priced.and_then(|p| p.max_input_tokens),
            priced.and_then(|p| p.max_output_tokens),
        ) {
            (None, None) => "budget.call.input_tokens + budget.call.output_tokens".to_owned(),
            (Some(_), Some(_)) => format!(
                "budget.pricing.{model}.max_input_tokens + budget.pricing.{model}.max_output_tokens"
            ),
            (Some(_), None) => {
                format!("budget.pricing.{model}.max_input_tokens + budget.call.output_tokens")
            }
            (None, Some(_)) => {
                format!("budget.call.input_tokens + budget.pricing.{model}.max_output_tokens")
            }
        };
        let below = |field: &str, origin: &Origin, cap: String, need: String, unit: &str, bound| {
            BudgetError::BelowCall(Box::new(BelowCall {
                level: level.key().to_owned(),
                field: field.to_owned(),
                cap,
                need,
                unit: unit.to_owned(),
                from: match origin {
                    Origin::Declared => "written in its budget block".to_owned(),
                    Origin::Default(setting) => format!("the default {setting}"),
                    Origin::Ceiled(setting) => format!("capped by your own {setting}"),
                },
                model: model.to_owned(),
                bound,
            }))
        };
        if caps.tokens < call.tokens {
            return Err(below(
                "tokens",
                &caps.origins[0],
                caps.tokens.to_string(),
                call.tokens.to_string(),
                " tokens",
                tokens_from,
            ));
        }
        if caps.millis < call.millis {
            let minutes = |ms: u64| format!("{}", ms as f64 / 60_000.0);
            return Err(below(
                "minutes",
                &caps.origins[1],
                minutes(caps.millis),
                minutes(call.millis),
                " minutes",
                "budget.call.minutes".to_owned(),
            ));
        }
        if let Some(cap) = caps.micro_usd
            && cap < call.micro_usd
        {
            let usd = |micro: u64| format!("{:.6}", micro as f64 / 1e6);
            return Err(below(
                "cost_usd",
                &caps.origins[2],
                usd(cap),
                usd(call.micro_usd),
                " US dollars",
                format!("{tokens_from}, at budget.pricing.{model}"),
            ));
        }
        Ok(())
    }
}

fn cost(price: &Price, input: u64, output: u64) -> u64 {
    // USD per million tokens is micro-dollars per token. Negative prices are
    // refused by validation; clamped here all the same.
    let micro = input as f64 * price.input_usd_per_mtok.max(0.0)
        + output as f64 * price.output_usd_per_mtok.max(0.0);
    // Saturating, and held to LIMIT so a sum of costs cannot overflow.
    (micro.ceil() as u64).min(LIMIT)
}

#[cfg(test)]
mod tests;
