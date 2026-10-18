//! diskgc-core: config schema and selection grammar, index types and queue
//! derivation, the pure planner, and the tally and executor over the `Host`
//! port. No syscalls of its own (docs/architecture.md, Implementation).
//!
//! So far: the [`host`] port, path traversal over it ([`resolve`]), the
//! minimum-kernel check ([`kernel`]), the deletion [`engine`] for
//! filesystem candidates (eligibility walk and identity-checked delete) and
//! the [`inuse`] check that goes between them, the selection grammar
//! ([`select`]: parser, normalised AST and static rules), the profile
//! [`kind`]s it checks against, and the [`profile`] model with the built-in
//! library (YAML through serde), which the grammar checks names against.
//! [`cargo_target`] is the `cargo-target` kind up to its reclaim protocol:
//! discovery, lock files and layouts, last build, eligibility and the Cargo
//! process heuristics, and its reclaim protocol. [`units`] parses sizes
//! and durations.
//!
//! The first slice of the executor ([`executor`]): the minimal `ensure` of
//! one `--select` tier of kind `cargo-target`, its [`report`] (exit codes,
//! JSON, summary), the host lock ([`hostlock`]) and the configuration
//! lookup with the part of schema `version: 1` loaded so far ([`config`]).

pub mod cargo_target;
pub mod config;
pub mod engine;
pub mod executor;
pub mod host;
pub mod hostlock;
pub mod inuse;
pub mod kernel;
pub mod kind;
pub mod profile;
pub mod report;
pub mod resolve;
pub mod select;
pub mod units;

#[cfg(test)]
mod doc;
