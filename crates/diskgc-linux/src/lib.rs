//! diskgc-linux: `LinuxHost`, the real implementation of the `Host` port.
//! A library because the differential contract tests use it
//! (docs/architecture.md, Implementation).
//!
//! - [`host`]: [`LinuxHost`] over raw syscalls (rustix `linux_raw`).
//! - [`mountinfo`], [`procfs`]: parsers for the `/proc` files it reads.
//! - [`sandbox`]: the test-sandbox markers shared by the e2e interlock and
//!   the pre-release sandbox gate.
//! - `source_scan` (tests only): source scans of the production crates that
//!   keep deleting operations behind the gate and `/proc` behind its fd.

pub mod host;
pub mod mountinfo;
pub mod procfs;
pub mod sandbox;
mod stat;
mod tasks;

#[cfg(test)]
mod source_scan;

pub use host::{LinuxFd, LinuxHost};
