// SPDX-License-Identifier: Apache-2.0

//! What the `atomic` binary runs: its workflows, written in Rust on the
//! [`atomic_durable::DurableRuntime`] trait.

// The workflows' futures, spans around them included, nest deeply inside
// Codex's; proving them Send needs more than the default query depth.
#![recursion_limit = "512"]

pub mod metering;
pub mod panics;
pub mod pipeline;
pub mod retention;
pub mod telemetry;
pub mod workflows;
