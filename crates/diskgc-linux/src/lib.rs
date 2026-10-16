//! diskgc-linux: `LinuxHost`, the real implementation of the `Host` port.
//! A library because the differential contract tests use it
//! (docs/architecture.md, Implementation).
//!
//! The host primitives land with task-3. For now it holds the test-sandbox
//! markers shared by the e2e interlock and the pre-release sandbox gate.

pub mod sandbox;
