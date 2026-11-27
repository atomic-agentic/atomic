// SPDX-License-Identifier: Apache-2.0

//! The `Atom` trait and the runtime that runs atoms on Codex.
//!
//! This is the only atomic crate that depends on Codex.

pub mod artifact;
pub mod astro;
pub mod atom;
pub mod budget;
mod buffer_budget;
// The build script's V8 archive check, compiled here only for its tests.
pub mod cassette;
mod child_exit;
mod child_memory;
pub mod codex;
pub mod collections;
pub mod compile;
pub mod compiler_host;
mod compiler_process;
pub mod config;
pub mod context;
pub mod credential;
pub mod editor;
pub mod eval;
pub mod evaluation;
pub mod failpoint;
pub mod failure;
pub mod fixture;
pub mod git;
pub mod helper_runtime;
pub mod http_tools;
pub mod lite;
pub mod mounts;
pub mod names;
pub mod network;
pub mod project;
pub mod redact;
pub mod render;
pub mod runtime;
pub mod signin;
pub mod spans;
pub mod state;
pub mod terminal;
#[cfg(any(test, feature = "fake-model"))]
pub mod testing;
pub mod typed;
pub mod usage;
#[cfg(test)]
#[path = "../build/v8_archive.rs"]
mod v8_archive;
#[cfg(test)]
#[path = "../build/v8_archive_tests.rs"]
mod v8_archive_tests;
#[cfg(windows)]
mod windows_cwd;
#[cfg(windows)]
mod windows_worker;
#[cfg(any(target_os = "macos", all(test, unix)))]
mod worker_lifetime;
pub mod workflow;
pub mod worktree;

pub use typed::{Approvals, Atom, AtomConfig, AtomError, AtomKind, MODEL_TIERS, Sandbox};
