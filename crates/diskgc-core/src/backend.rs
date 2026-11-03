//! What every backend kind's binder returns (docs/reference.md, Backend
//! skips and errors): the bound backend, or a skip of it identified from a
//! specific signal. Errors are the binder's `Err`, never a skip.

use crate::engine::Reason;

/// A backend that offers no supported, reachable store of its kind here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub reason: Reason,
    /// For people (the report's `detail`), not part of the contract.
    pub detail: String,
}

/// The result of binding one backend kind: `B` is that kind's bound facts,
/// compared again right before its prune.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Binding<B> {
    Bound(B),
    Skipped(Skipped),
}

pub(crate) fn skip<B>(reason: Reason, detail: impl Into<String>) -> Binding<B> {
    Binding::Skipped(Skipped {
        reason,
        detail: detail.into(),
    })
}
