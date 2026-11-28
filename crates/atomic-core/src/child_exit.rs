// SPDX-License-Identifier: Apache-2.0
//! The exit statuses with which a compiler or helper child reports the limit
//! that ended it, so its parent names the limit rather than a crash.

/// The child's JavaScript heap reached its limit.
pub(crate) const HEAP: i32 = 81;
/// The child's ArrayBuffers asked for more than their budget.
pub(crate) const BUFFERS: i32 = 82;
/// The call ran past its deadline.
pub(crate) const DEADLINE: i32 = 83;
