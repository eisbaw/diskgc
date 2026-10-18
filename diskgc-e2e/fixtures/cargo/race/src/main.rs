//! Cargo race fixture: prints the line the harness expects
//! (`RACE.output` in diskgc-e2e/src/cargo_under_test.rs), so a test can
//! tell whether the binary ran.

fn main() {
    println!("diskgc-e2e cargo race fixture ran");
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    const GATE_ENV: &str = "DISKGC_E2E_TEST_GATE";
    const GATE_TIMEOUT: Duration = Duration::from_secs(300);

    /// With `DISKGC_E2E_TEST_GATE=G` set, the test binary keeps running at
    /// the gate (the protocol of `Gate` in diskgc-e2e/src/cargo_under_test.rs:
    /// its pid and a newline to `G.waiting`, then wait for `G.go`, failing
    /// after `GATE_TIMEOUT`): a test binary still running. Without the
    /// variable it only prints. Std only: the fixture builds offline.
    #[test]
    fn runs() {
        println!("diskgc-e2e cargo race fixture test ran");
        let Some(gate) = std::env::var_os(GATE_ENV) else {
            return;
        };
        let gate = PathBuf::from(gate);
        let waiting = with_suffix(&gate, ".waiting");
        let go = with_suffix(&gate, ".go");
        let pid = std::process::id();
        if let Err(e) = std::fs::write(&waiting, format!("{pid}\n")) {
            panic!("test gate: write {}: {e}", waiting.display());
        }
        let deadline = Instant::now() + GATE_TIMEOUT;
        while !go.exists() {
            if Instant::now() >= deadline {
                panic!(
                    "test gate: {} did not appear within {GATE_TIMEOUT:?}",
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
}
