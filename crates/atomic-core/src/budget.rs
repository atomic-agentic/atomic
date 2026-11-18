// SPDX-License-Identifier: Apache-2.0

//! Budgets: the `budget` field of atoms and workflows, the finite defaults
//! omitted limits inherit, and the model price table a dollar cap needs.
//!
//! A `budget` block takes three limits, each optional and each positive:
//!
//! ```yaml
//! budget:
//!   tokens: 20000    # cumulative input and output tokens of real calls
//!   minutes: 5       # cumulative active execution, excluding gate waits
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
//! Caps are held as integers: tokens, milliseconds and micro-dollars
//! (millionths of a US dollar). A price in US dollars per million tokens is
//! therefore exactly a price in micro-dollars per token.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// The limits a `budget` block wrote; `None` inherits the default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
            let positive = |v: &Value| v.as_f64().filter(|n| n.is_finite() && *n > 0.0);
            match key.as_str() {
                "tokens" => {
                    budget.tokens = Some(v.as_u64().filter(|n| *n > 0).ok_or_else(|| {
                        (at.clone(), "tokens must be a positive integer".to_owned())
                    })?);
                }
                "minutes" => {
                    let m = positive(v)
                        .ok_or_else(|| (at.clone(), "minutes must be a positive number".into()))?;
                    budget.millis = Some(minutes_to_millis(m));
                }
                "cost_usd" => {
                    let usd = positive(v)
                        .ok_or_else(|| (at.clone(), "cost_usd must be a positive number".into()))?;
                    budget.micro_usd = Some(usd_to_micro(usd));
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

/// Milliseconds in `minutes`, at least one.
fn minutes_to_millis(minutes: f64) -> u64 {
    // In range: a positive, finite number of minutes, saturating.
    ((minutes * 60_000.0).round() as u64).max(1)
}

/// Micro-dollars in `usd`, at least one.
fn usd_to_micro(usd: f64) -> u64 {
    ((usd * 1_000_000.0).round() as u64).max(1)
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
        }
    }
}

/// Where a resolved cap came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// The atom's or workflow's own `budget` block.
    Declared,
    /// A default, by its setting path, such as `budget.atom.tokens`; the
    /// configuration's sources say which layer set it.
    Default(String),
}

/// A budget after defaults: every cap that applies, and where each came from.
#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// A configured default is not positive and finite.
    #[error("{setting} must be a positive number")]
    Default {
        /// The setting, such as `budget.atom.minutes`.
        setting: String,
    },
}

/// Resolve `declared` for `level` against `settings`, for calls to `models`
/// (every model the atom or workflow may call). A dollar cap, written or
/// inherited, needs every one of them priced.
pub fn resolve(
    declared: &Budget,
    level: Level,
    settings: &BudgetSettings,
    models: &[&str],
) -> Result<Resolved, BudgetError> {
    let limits = match level {
        Level::Atom => &settings.atom,
        Level::Workflow => &settings.workflow,
    };
    let path = |field: &str| format!("budget.{}.{field}", level.key());
    let finite = |n: f64, field: &str| {
        if n.is_finite() && n > 0.0 {
            Ok(n)
        } else {
            Err(BudgetError::Default {
                setting: path(field),
            })
        }
    };
    let (tokens, tokens_from) = match declared.tokens {
        Some(t) => (t, Origin::Declared),
        None if limits.tokens > 0 => (limits.tokens, Origin::Default(path("tokens"))),
        None => {
            return Err(BudgetError::Default {
                setting: path("tokens"),
            });
        }
    };
    let (millis, millis_from) = match declared.millis {
        Some(m) => (m, Origin::Declared),
        None => (
            minutes_to_millis(finite(limits.minutes, "minutes")?),
            Origin::Default(path("minutes")),
        ),
    };
    let (micro_usd, cost_from) = match (declared.micro_usd, limits.cost_usd) {
        (Some(c), _) => (Some(c), Origin::Declared),
        (None, Some(usd)) => (
            Some(usd_to_micro(finite(usd, "cost_usd")?)),
            Origin::Default(path("cost_usd")),
        ),
        (None, None) => (None, Origin::Default(path("cost_usd"))),
    };
    if micro_usd.is_some()
        && let Some(model) = models.iter().find(|m| !settings.pricing.contains_key(**m))
    {
        return Err(BudgetError::Unpriced {
            model: (*model).to_owned(),
        });
    }
    Ok(Resolved {
        tokens,
        millis,
        micro_usd,
        origins: [tokens_from, millis_from, cost_from],
    })
}

/// What one call to a model reserves, and the provider's documented maximum
/// for it, as tokens, milliseconds and micro-dollars.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reservation {
    /// Input and output tokens.
    pub tokens: u64,
    /// Active milliseconds.
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
            .unwrap_or(self.call.input_tokens);
        let output = price
            .and_then(|p| p.max_output_tokens)
            .unwrap_or(self.call.output_tokens);
        Reservation {
            tokens: input.saturating_add(output),
            millis: if self.call.minutes.is_finite() && self.call.minutes > 0.0 {
                minutes_to_millis(self.call.minutes)
            } else {
                0
            },
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
}

fn cost(price: &Price, input: u64, output: u64) -> u64 {
    // USD per million tokens is micro-dollars per token.
    let micro = input as f64 * price.input_usd_per_mtok.max(0.0)
        + output as f64 * price.output_usd_per_mtok.max(0.0);
    micro.ceil() as u64
}

#[cfg(test)]
mod tests;
