//! diskgc-core: config schema and selection grammar, index types and queue
//! derivation, the pure planner, and the tally and executor over the `Host`
//! port. No syscalls of its own (docs/architecture.md, Implementation).
//!
//! So far: the [`host`] port, path traversal over it ([`resolve`]), the
//! minimum-kernel check ([`kernel`]), the deletion [`engine`] for
//! filesystem candidates (eligibility walk and identity-checked delete) and
//! the [`inuse`] check that goes between them, the selection grammar
//! ([`select`]: parser, normalised AST and static rules) and the profile
//! [`kind`]s it checks against.

pub mod engine;
pub mod host;
pub mod inuse;
pub mod kernel;
pub mod kind;
pub mod resolve;
pub mod select;
