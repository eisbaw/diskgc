//! diskgc-e2e: the rootless-container test harness and the destructive tests
//! (docs/testing.md, End-to-end tests in containers).
//!
//! Two sides, one crate:
//!
//! - **In the container** (`diskgc-e2e`, running as container root): the
//!   [`interlock`] first, then a fresh scratch tmpfs per test ([`scratch`]),
//!   fixtures built through checked operations, the code under test started
//!   as uid 1000 under strace ([`under_test`]), and the deletion oracle
//!   ([`snapshot`], [`strace`], [`oracle`]). [`probe`] holds deliberately
//!   misbehaving stand-ins for code under test that prove the checks bite.
//!   The tests themselves are in [`suite`]; those of the `Host` primitives
//!   in [`host_suite`], driven through [`host_probe`]; those that pause or
//!   kill diskgc at a chosen port call in [`driver_suite`], driven through
//!   the test driver ([`driver`], binary `diskgc-e2e-driver`) and its
//!   `Host` wrapper ([`controlled`]); those of the deletion engine, through
//!   the same driver, in [`engine_suite`]; those of the in-use check and
//!   the deletion step around it in [`inuse_suite`], with the fixture
//!   processes of [`inuse_probe`] (started through [`holder`]); those of
//!   the Cargo toolchains under test and the Cargo race helpers in
//!   [`cargo_suite`], with [`cargo_under_test`], the helper processes of
//!   [`cargo_probe`] and the `/proc/locks` sampler [`proc_locks`]; those
//!   of the `cargo-target` kind's discovery, eligibility and process
//!   heuristics on real Cargo builds in [`cargo_target_suite`], and of
//!   its reclaim protocol in [`cargo_reclaim_suite`]; those of the
//!   `diskgc ensure` binary itself (thresholds, the host lock, exit codes,
//!   `--json`, `--dry-run`, real Cargo reclaims) in [`ensure_suite`];
//!   the user journeys, the product driven through the files its users
//!   write (the CI operator's pre-job hook), in [`journey_suite`]. What
//!   the test image is (Nix or Debian: what it mounts read-only, whether
//!   it has Cargo toolchains, which tests it runs) is its descriptor,
//!   [`image`].
//! - **On the host** (`diskgc-e2e-runner`): [`runner`] makes sure the base
//!   image is loaded (reused when its Nix hash is already present), lists the
//!   tests from inside a container, runs one container per test, reports
//!   per-test durations and enforces the time budget. [`release_check`]
//!   (`diskgc-e2e-release-check`, a flake check) proves the release
//!   package contains neither the driver nor the wrapper.
//!
//! Exit codes shared by both sides are in [`exit`].

pub mod agent_journey;
pub mod autotools_suite;
pub mod browser_suite;
pub mod buildah_suite;
pub mod cache_suite;
pub mod cargo_harness;
pub mod cargo_probe;
pub mod cargo_race_suite;
pub mod cargo_reclaim_suite;
pub mod cargo_suite;
pub mod cargo_target_suite;
pub mod cargo_under_test;
pub mod cli_contract_suite;
pub mod cli_runtime_suite;
pub mod controlled;
pub mod differential_suite;
pub mod docker_suite;
pub mod docker_vector_suite;
pub mod driver;
pub mod driver_suite;
pub mod embedded_suite;
pub mod engine_suite;
pub mod ensure_suite;
pub mod fs_reclaim_suite;
pub mod fuse_stall_suite;
pub mod holder;
pub mod host_probe;
pub mod host_suite;
pub mod image;
pub mod interlock;
pub mod inuse_probe;
pub mod inuse_suite;
pub mod journey_suite;
pub mod laptop_tmp_journey;
pub mod lifecycle_suite;
pub mod lock_release;
pub mod meson_suite;
pub mod model_materialize;
pub mod nix_gc_suite;
pub mod oracle;
pub mod owner_pid_suite;
pub mod packaging_suite;
pub mod probe;
pub mod proc_locks;
pub mod profile_vector_suite;
pub mod project_cache_suite;
pub mod pytest_contract_suite;
pub mod release_check;
pub mod release_suite;
pub mod runner;
pub mod scale_suite;
pub mod scratch;
pub mod signal_suite;
pub mod slow_stderr;
pub mod snapshot;
pub mod stdout_suite;
pub mod strace;
pub mod suite;
pub mod survey_suite;
pub mod tally_probe;
pub mod temp_safety_suite;
pub mod under_test;
pub mod watch_suite;

/// Exit codes of the in-container binaries, read by the runner and by tests
/// that run probes as subprocesses.
pub mod exit {
    /// The test (or probe) ran and passed.
    pub const PASS: i32 = 0;
    /// The test ran and failed.
    pub const FAIL: i32 = 1;
    /// A Cargo test did not run because its toolchain under test failed the
    /// smoke check: a toolchain problem, not a failure of what it tests.
    pub const TOOLCHAIN: i32 = 3;
    /// Bad command line.
    pub const USAGE: i32 = 2;
    /// The interlock refused to run (layer 2) or aborted an operation outside
    /// the scratch mount (layer 3).
    pub const INTERLOCK: i32 = 86;
}

/// Uid and gid of the unprivileged user the code under test runs as
/// (`tester` in the image's /etc/passwd).
pub const TESTER: Owner = Owner {
    uid: 1000,
    gid: 1000,
};

/// A second mapped user, owner of the "other user's" fixtures.
pub const OTHER: Owner = Owner {
    uid: 1001,
    gid: 1001,
};

/// Container root (the harness).
pub const ROOT: Owner = Owner { uid: 0, gid: 0 };

/// Login name of [`TESTER`] in the image (USER and LOGNAME of the code
/// under test).
pub const TESTER_NAME: &str = "tester";

/// A file the release runner's under-test directory holds in `bin/`: the
/// release-candidate package is the default `diskgc` there (flake.nix,
/// `e2eReleaseUnderTest`). Its containers then share the host's PID
/// namespace, since that build refuses to reclaim in one of its own
/// (docs/safety.md, In use; [`crate::runner`]).
pub const RELEASE_DEFAULT_MARKER: &str = "diskgc-release-default";

/// Where the runner mounts the binaries under test, read-only.
pub const UNDER_TEST_BIN_DIR: &str = "/opt/diskgc-e2e/bin";

/// A file owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
}

/// Informational log line on stderr (tests' stdout is reserved for results).
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { eprintln!("info: {}", format_args!($($arg)*)) };
}

/// Fails the current test (returns `Err(String)`) unless `cond` holds.
#[macro_export]
macro_rules! ensure {
    ($cond:expr, $($arg:tt)*) => {
        if !$cond {
            return Err(format!($($arg)*));
        }
    };
}
