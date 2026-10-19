//! diskgc-model: `ModelHost`, a `Host` backed by a YAML host model, and the
//! model-test harness. Test-only: never a dependency of the `diskgc` binary
//! (docs/testing.md, Model tests).
//!
//! - [`spec`]: the host-model format (serde types; JSON Schema);
//! - [`load`]: strict loading, references resolved;
//! - [`state`]: the inode graph, mounts, open files, processes;
//! - [`host`]: [`ModelHost`], its call checks, events and virtual clock;
//! - [`harness`]: run a fixture's scenario and check its `expect`.

pub mod harness;
pub mod host;
pub mod load;
pub mod spec;
pub mod state;
pub mod support;

pub use host::{Call, Holder, ModelFd, ModelHost};
pub use load::LoadError;
pub use spec::HostModel;
