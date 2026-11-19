// SPDX-License-Identifier: Apache-2.0

//! Schema registry, lowering to generation profiles, and full-contract validation.
//!
//! JSON Schema is atomic's contract: every model output and tool argument is
//! validated here against the full schema, whatever subset a provider enforces.

pub mod artifact;
pub mod compact;
pub mod expr;
pub mod lower;
pub mod openapi;
pub mod registry;
pub mod typescript;
pub mod yaml;
