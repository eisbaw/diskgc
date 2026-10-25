//! diskgc-linux: `LinuxHost`, the real implementation of the `Host` port.
//! A library because the differential contract tests use it
//! (docs/architecture.md, Implementation).
//!
//! - [`host`]: [`LinuxHost`] over raw syscalls (rustix `linux_raw`).
//! - [`mountinfo`], [`procfs`]: parsers for the `/proc` files it reads;
//!   [`sockdiag`]: the socket-diagnostics query (request and answer).
//! - [`nofile`]: the open-file limit, raised at startup and restored for
//!   children.
//! - [`sandbox`]: the test-sandbox markers shared by the e2e interlock and
//!   the pre-release sandbox gate.
//! - `source_scan` (tests only): source scans of the production crates that
//!   keep deleting operations behind the gate and `/proc` behind its fd.

pub mod host;
pub mod mountinfo;
pub mod nofile;
pub mod procfs;
pub mod sandbox;
mod scratch;
pub mod sockdiag;
mod stat;
mod tasks;

#[cfg(test)]
mod source_scan;

pub use host::{LinuxFd, LinuxHost};
