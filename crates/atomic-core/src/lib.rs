// SPDX-License-Identifier: Apache-2.0

//! The `Atom` trait and the runtime that runs atoms on Codex.
//!
//! This is the only atomic crate that depends on Codex.

pub mod artifact;
pub mod atom;
pub mod codex;
pub mod collections;
pub mod config;
pub mod project;
pub mod state;
pub mod tags;
pub mod workflow;
