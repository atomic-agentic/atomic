// SPDX-License-Identifier: Apache-2.0

//! The `Atom` trait and the runtime that runs atoms on Codex.
//!
//! This is the only atomic crate that depends on Codex.

pub mod artifact;
pub mod atom;
pub mod codex;
pub mod collections;
pub mod compile;
pub mod config;
pub mod lite;
pub mod project;
pub mod render;
pub mod runtime;
pub mod state;
pub mod tags;
#[cfg(any(test, feature = "fake-model"))]
pub mod testing;
pub mod typed;
pub mod workflow;

pub use typed::{Approvals, Atom, AtomConfig, AtomError, AtomKind, MODEL_TIERS, Sandbox};
