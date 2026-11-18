// SPDX-License-Identifier: Apache-2.0

//! The `Atom` trait and the runtime that runs atoms on Codex.
//!
//! This is the only atomic crate that depends on Codex.

pub mod artifact;
pub mod astro;
pub mod atom;
pub mod budget;
pub mod codex;
pub mod collections;
pub mod compile;
mod compiler_process;
pub mod config;
pub mod context;
pub mod evaluation;
pub mod failure;
pub mod git;
pub mod lite;
pub mod project;
pub mod render;
pub mod runtime;
pub mod state;
#[cfg(any(test, feature = "fake-model"))]
pub mod testing;
pub mod typed;
pub mod usage;
pub mod workflow;
pub mod worktree;

pub use typed::{Approvals, Atom, AtomConfig, AtomError, AtomKind, MODEL_TIERS, Sandbox};
