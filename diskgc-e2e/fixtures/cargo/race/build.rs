//! Build script of the Cargo race fixture: pauses the build on a control
//! file while Cargo holds its locks (docs/testing.md, Cargo race tests).
//!
//! With `DISKGC_E2E_BUILD_GATE=G` in the environment it follows the gate
//! protocol of `Gate` in diskgc-e2e/src/cargo_under_test.rs: it writes its
//! pid and a newline to `G.waiting`, then waits until `G.go` exists, and
//! fails the build if that takes longer than `GATE_TIMEOUT` (a harness that
//! went away must not leave a build hanging). Without the variable it does
//! nothing. Std only: the fixture builds offline, without dependencies.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const GATE_ENV: &str = "DISKGC_E2E_BUILD_GATE";
const GATE_TIMEOUT: Duration = Duration::from_secs(300);

fn main() {
    // Rerun only when the gate changes, so each gated build of a target
    // that is otherwise fresh still runs (and pauses in) this script.
    println!("cargo:rerun-if-env-changed={GATE_ENV}");
    let Some(gate) = std::env::var_os(GATE_ENV) else {
        return;
    };
    let gate = PathBuf::from(gate);
    let waiting = with_suffix(&gate, ".waiting");
    let go = with_suffix(&gate, ".go");
    let pid = std::process::id();
    if let Err(e) = std::fs::write(&waiting, format!("{pid}\n")) {
        panic!("build.rs gate: write {}: {e}", waiting.display());
    }
    let deadline = Instant::now() + GATE_TIMEOUT;
    while !go.exists() {
        if Instant::now() >= deadline {
            panic!(
                "build.rs gate: {} did not appear within {GATE_TIMEOUT:?}",
                go.display()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn with_suffix(gate: &Path, suffix: &str) -> PathBuf {
    let mut name = gate.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}
