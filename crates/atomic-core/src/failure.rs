// SPDX-License-Identifier: Apache-2.0

//! Failures in tool and step output: a [`summary`] that keeps every failure
//! line of a large output visible, and a [`fingerprint`] that names the same
//! failure the same way across runs, for recognizing a step that is stuck.

pub mod fingerprint;
pub mod summary;
