// SPDX-License-Identifier: Apache-2.0

//! The `ExecutionBackend` trait between the control plane and its runners.
//!
//! The control plane drives runners only through this trait, even when both run
//! in one process, so moving runners elsewhere never changes workflow code.
