//! The model-test harness (docs/testing.md, Model tests): load a fixture,
//! run a scenario against [`ModelHost`], check the result against the
//! fixture's `expect`, and check that every event fired. The host is handed
//! back for assertions on the call trace.

use std::path::{Path, PathBuf};

use crate::host::ModelHost;
use crate::load::{LoadError, parse_yaml};
use crate::spec::{HostModel, Skipped};

/// What a scenario produced, in the terms of `expect`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub exit: i32,
    pub skipped: Vec<Skipped>,
}

/// Reads and parses a fixture file.
pub fn load_fixture(path: &Path) -> Result<HostModel, LoadError> {
    let text = std::fs::read_to_string(path).map_err(|e| LoadError {
        at: String::new(),
        message: format!("cannot read {}: {e}", path.display()),
    })?;
    parse_yaml(&text).map_err(|e| LoadError {
        message: format!("{}: {}", path.display(), e.message),
        ..e
    })
}

/// `tests/models/<area>/<case>.yaml` below a crate's manifest directory.
pub fn fixture_path(manifest_dir: &str, area: &str, case: &str) -> PathBuf {
    Path::new(manifest_dir)
        .join("tests/models")
        .join(area)
        .join(format!("{case}.yaml"))
}

/// Runs `scenario` on `model` and fails the test (panics) if the model does
/// not load, the outcome differs from `expect`, or an event never fired
/// (its race did not happen, so the scenario proved nothing).
pub fn run(model: &HostModel, scenario: impl FnOnce(&ModelHost) -> Outcome) -> ModelHost {
    let host = ModelHost::new(model).unwrap_or_else(|e| panic!("{e}"));
    let outcome = scenario(&host);
    let trace = || {
        host.trace()
            .iter()
            .map(|c| format!("  {c}\n"))
            .collect::<String>()
    };
    if let Some(expect) = &model.expect {
        if let Some(exit) = expect.exit {
            assert_eq!(
                outcome.exit,
                exit,
                "exit code differs from expect.exit; trace:\n{}",
                trace()
            );
        }
        if let Some(skipped) = &expect.skipped {
            assert_eq!(
                &outcome.skipped,
                skipped,
                "skipped candidates differ from expect.skipped; trace:\n{}",
                trace()
            );
        }
    }
    let unfired = host.unfired_events();
    assert!(
        unfired.is_empty(),
        "events {unfired:?} never fired: their call or time was never reached; trace:\n{}",
        trace()
    );
    host
}
