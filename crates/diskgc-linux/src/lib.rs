//! diskgc-linux: `LinuxHost`, the real implementation of the `Host` port.
//! A library because the differential contract tests use it
//! (docs/architecture.md, Implementation).
//!
//! - [`host`]: [`LinuxHost`] over raw syscalls (rustix `linux_raw`).
//! - [`mountinfo`], [`procfs`]: parsers for the `/proc` files it reads.
//! - [`sandbox`]: the test-sandbox markers shared by the e2e interlock and
//!   the pre-release sandbox gate.

pub mod host;
pub mod mountinfo;
pub mod procfs;
pub mod sandbox;
mod stat;
mod tasks;

pub use host::{LinuxFd, LinuxHost};
