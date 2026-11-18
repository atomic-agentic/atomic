// SPDX-License-Identifier: Apache-2.0

//! The `Atom` trait and the runtime that runs atoms on Codex.
//!
//! This is the only atomic crate that depends on Codex.

pub mod artifact;
pub mod astro;
pub mod atom;
pub mod codex;
pub mod collections;
pub mod compile;
mod compiler_process;
pub mod config;
pub mod context;
pub mod evaluation;
pub mod project;
pub mod render;
pub mod runtime;
pub mod state;
#[cfg(any(test, feature = "fake-model"))]
pub mod testing;
pub mod typed;
pub mod workflow;

pub use typed::{Approvals, Atom, AtomConfig, AtomError, AtomKind, Sandbox};
