//! The container tests and their per-test context.
//!
//! Every test runs in its own container, as container root, after the
//! interlock passed: [`Ctx::setup`] mounts fresh tmpfs scratch mounts at
//! `/tmp`, `/run/diskgc` (with the host lock) and `/home/tester`, plus a
//! private records tmpfs; the test builds its fixture through [`Scratch`],
//! runs the code under test as `tester` (uid 1000) under strace, and judges
//! the result with the deletion oracle. Judging is mandatory: a test that
//! leaves a traced run unjudged fails ([`Ctx::teardown`]).
//!
//! Tiers: `fast` tests run in `just e2e` (the commit gate, 5-minute budget);
//! `full` tests run only in `just e2e-full`.
//!
//! Besides [`TESTS`], the Cargo tests ([`crate::cargo_suite`]) run once per
//! Cargo toolchain under test, listed as `NAME@TOOLCHAIN`; each first runs
//! its toolchain's smoke check, and a test whose toolchain fails it is
//! reported as `TOOLCHAIN` (exit [`exit::TOOLCHAIN`]), not as `FAIL`.
//!
//! Which of them an image lists and runs, whether it has toolchains at
//! all, and what [`Ctx::setup`] mounts read-only, the image's descriptor
//! says ([`crate::image`]); an image whose descriptor says `tests portable`
//! runs every test but [`NIX_ONLY_TESTS`].

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use rustix::mount::MountFlags;

use crate::cargo_under_test::{self, Toolchain, ToolchainBroken};
use crate::image::{self, Image};
use crate::oracle::{Change, Fixture, Oracle, Unmatched};
use crate::probe::{ESCAPE_CASES, ESCAPE_TARGET_DIR, ESCAPED_NAME};
use crate::scratch::{self, Scratch};
use crate::snapshot::{self, Skip};
use crate::under_test::{self, Ledger, Run};
use crate::{OTHER, ROOT, TESTER, UNDER_TEST_BIN_DIR, ensure, exit, info};
use crate::{
    agent_journey, autotools_suite, browser_suite, buildah_suite, cache_suite, cargo_race_suite,
    cargo_reclaim_suite, cargo_suite, cli_contract_suite, cli_runtime_suite, differential_suite,
    docker_suite, docker_vector_suite, driver_suite, embedded_suite, engine_suite, ensure_suite,
    fs_reclaim_suite, fuse_stall_suite, host_suite, inuse_suite, laptop_tmp_journey, meson_suite,
    packaging_suite, profile_vector_suite, project_cache_suite, signal_suite, stdout_suite,
    temp_safety_suite,
};

/// Which gate runs a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// `just e2e` and `just e2e-full`.
    Fast,
    /// `just e2e-full` only.
    Full,
    /// Only in the disposable, Nix-built QEMU guest with SYS_TIME.
    Vm,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Fast => "fast",
            Tier::Full => "full",
            Tier::Vm => "vm",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "fast" => Some(Tier::Fast),
            "full" => Some(Tier::Full),
            "vm" => Some(Tier::Vm),
            _ => None,
        }
    }
}

pub type TestResult = Result<(), String>;

/// What a test's body must do for the test to pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    /// Pass, as almost every test.
    Pass,
    /// Fail with a message containing this text: a bite proof that a
    /// harness rule (applied by [`Ctx::teardown`] or elsewhere) really fails
    /// a test that breaks it.
    FailWith(&'static str),
}

/// One container test.
pub struct Test {
    pub name: &'static str,
    pub tier: Tier,
    pub expect: Expect,
    pub run: fn(&Ctx) -> TestResult,
}

/// Where the image bakes a tester-owned file on the overlay root, outside
/// every scratch mount (flake.nix, e2eBaseImage).
pub const TESTER_OVERLAY_FILE: &str = "/srv/tester/on-overlay";

/// What the oracle-mandatory bite test must fail with (the ledger's
/// message, see [`Ledger::require_all_judged`]).
const UNJUDGED_MESSAGE: &str = "never judged by the deletion oracle";

/// The tests that run only on the Nix image, each with why (docs/testing.md,
/// Container setup: Images). Every other test runs on every image: an image
/// whose descriptor says `tests portable` (the Debian image) runs all but
/// these, so a test that cannot run there fails loudly instead of being
/// left out silently. Plain tests by name, Cargo tests by their name before
/// `@TOOLCHAIN`.
pub static NIX_ONLY_TESTS: &[(&str, &str)] = &[
    (
        "vm_cmake_original_matches_linux_outcomes_and_deletion_diff",
        "requires a Nix-built disposable QEMU guest with a guest-only mutable clock",
    ),
    (
        "vm_autotools_original_matches_linux_outcomes_and_deletion_diff",
        "requires a Nix-built disposable QEMU guest with a guest-only mutable clock",
    ),
    (
        "vm_meson_original_matches_linux_outcomes_and_deletion_diff",
        "requires a Nix-built disposable QEMU guest with a guest-only mutable clock",
    ),
    (
        "vm_containers_image_original_matches_linux_outcomes_and_deletion_diff",
        "requires a Nix-built disposable QEMU guest with a guest-only mutable clock",
    ),
    (
        "agent_harness_uses_json_for_all_reclaim_decisions",
        "the shipped Python example needs the pinned Python interpreter in the Nix image",
    ),
    (
        MESON_ANCHOR_TEST,
        "needs the pinned Meson and Ninja from the Nix store",
    ),
    (
        AUTOTOOLS_ANCHOR_TEST,
        "needs pinned GNU Autoconf from the Nix store",
    ),
    (
        WEST_REAL_BUILD_TEST,
        "needs pinned West, Zephyr source and host build tools from the Nix store",
    ),
    (
        "live_pytest_without_session_lock_is_unsafe_to_reclaim",
        "needs pinned pytest in the Nix image",
    ),
    (
        BUILDAH_SPOOL_TEST,
        "needs pinned Buildah and UID mapping tools from the Nix store",
    ),
    ("pip_cache_cleaner", "needs the pinned pip in the Nix image"),
    ("uv_cache_cleaner", "needs the pinned uv in the Nix image"),
    (
        "go_build_cache_cleaner",
        "needs the pinned Go in the Nix image",
    ),
    ("npm_cache_cleaner", "needs the pinned npm in the Nix image"),
    (
        "pnpm_store_cleaner",
        "needs the pinned pnpm in the Nix image",
    ),
    (
        "yarn_cache_cleaner",
        "needs the pinned Yarn Classic in the Nix image",
    ),
    ("ccache_cleaner", "needs the pinned ccache in the Nix image"),
    (
        "jest_cache_cleaner",
        "needs the pinned Jest in the Nix image",
    ),
    (
        "platformio_cache_cleaner",
        "needs the pinned PlatformIO in the Nix image",
    ),
    (
        "cargo_race_6_a_mixed_layout_target_killed_at_seeded_positions",
        "needs a nightly toolchain beside the fast stable one (a mixed-layout target); the \
         Debian image has one Cargo",
    ),
    (
        "nix_evaluation_keeps_its_user_eval_cache_in_use",
        "needs the real Nix evaluator and the flake baked into the Nix image",
    ),
];

/// Tests that require the digest-pinned Docker Engine dind image. They are
/// never listed by the Nix or Debian images, which have no Docker daemon.
pub static DOCKER_ONLY_TESTS: &[(&str, &str)] = &[
    (
        DOCKER_PACKAGING_TEST,
        "applies the shipped tmpfiles rule with Alpine sd-tmpfiles in the pinned dind image",
    ),
    (
        DOCKER_DIND_TEST,
        "needs the native Docker Engine daemon in the pinned dind image",
    ),
    (
        "docker_query_descendant_pipe_holder_hits_deadline",
        "needs the native Docker Engine daemon in the pinned dind image",
    ),
    (
        DOCKER_VECTOR_TEST,
        "replays a checked-in profile vector while native Docker Engine runs",
    ),
    (
        "native_docker_image_prune_preserves_containers_volumes",
        "needs the native Docker Engine daemon in the pinned dind image",
    ),
    (
        "native_docker_prune_child_keeps_lock_after_parent_kill",
        "needs the native Docker Engine daemon in the pinned dind image",
    ),
    (
        "native_docker_dangling_and_build_cache_prune",
        "needs native Docker Engine and BuildKit in the pinned dind image",
    ),
];

pub const DOCKER_DIND_TEST: &str = "native_docker_engine_dind_smoke";
pub const DOCKER_PACKAGING_TEST: &str = "docker_shipped_tmpfiles_and_example_work_in_container";
pub const DOCKER_VECTOR_TEST: &str = "checked_in_python_profile_vector_replays_under_native_docker";

/// The tests whose container gets the FUSE device (`/dev/fuse`, when the
/// host has it): only these mount a FUSE filesystem.
pub static FUSE_TESTS: &[&str] = &[
    "fs_reclaim_a_dead_fuse_mount_is_never_touched",
    "fs_protected_dead_fuse_root_is_refused_without_opening_it",
    "inuse_scan_finishes_with_a_process_in_a_stalled_fuse_mount",
];

/// The one test whose container receives the pinned Buildah closure.
pub const BUILDAH_SPOOL_TEST: &str = "containers_image_spool_held_by_buildah_is_in_use";
/// The test whose container receives the pinned Meson and Ninja closure.
pub const MESON_ANCHOR_TEST: &str = "meson_generated_output_uses_surviving_source_anchor";
/// The test whose container receives the pinned GNU Autoconf closure.
pub const AUTOTOOLS_ANCHOR_TEST: &str = "autotools_generated_output_uses_surviving_source_anchor";
/// Actual West and Zephyr generated output, isolated from host project files.
pub const WEST_REAL_BUILD_TEST: &str = "west_generated_output_uses_surviving_source_anchor";
/// The release package's tmpfiles drop-in and example config in a container.
pub const PACKAGING_TEST: &str = "shipped_tmpfiles_and_example_work_in_container";

/// The registry. Names are stable: `just e2e-one NAME` uses them.
pub static TESTS: &[Test] = &[
    Test {
        name: "ensure_refuses_symlinked_path_without_mutation",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: cli_contract_suite::ensure_refuses_symlinked_path_without_mutation,
    },
    Test {
        name: "select_replaces_configured_groups",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: cli_contract_suite::select_replaces_configured_groups,
    },
    Test {
        name: "json_help_precedence_is_text",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: cli_contract_suite::json_help_precedence_is_text,
    },
    Test {
        name: "no_index_matches_current_dry_run_plan",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: cli_contract_suite::no_index_matches_current_dry_run_plan,
    },
    Test {
        name: "ensure_without_config_or_select_is_usage_error",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: cli_contract_suite::ensure_without_config_or_select_is_usage_error,
    },
    Test {
        name: "invalid_config_never_falls_back_or_reclaims",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: cli_contract_suite::invalid_config_never_falls_back_or_reclaims,
    },
    Test {
        name: "negated_retention_filters_fail_before_reclaim",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: cli_contract_suite::negated_retention_filters_fail_before_reclaim,
    },
    Test {
        name: "config_environment_and_flag_precedence_reclaim_selected_policy",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: cli_contract_suite::config_environment_and_flag_precedence_reclaim_selected_policy,
    },
    Test {
        name: "select_roots_requirements_are_usage_errors",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: cli_contract_suite::select_roots_requirements_are_usage_errors,
    },
    Test {
        name: "dry_run_plans_flocked_candidate_as_eligible",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: cli_contract_suite::dry_run_plans_flocked_candidate_as_eligible,
    },
    Test {
        name: "timeout_zero_waits_for_a_real_host_lock",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: cli_runtime_suite::timeout_zero_waits_for_a_real_host_lock,
    },
    Test {
        name: "tally_budget_zero_reaches_reclaim",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: cli_runtime_suite::tally_budget_zero_reaches_reclaim,
    },
    Test {
        name: "no_index_matches_reclaim_on_fresh_fixture",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: cli_runtime_suite::no_index_matches_reclaim_on_fresh_fixture,
    },
    Test {
        name: "sigterm_mid_reclaim_reports_partial_action_and_stops",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: signal_suite::sigterm_mid_reclaim_reports_partial_action_and_stops,
    },
    Test {
        name: "deadline_mid_reclaim_reports_one_json_object",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: signal_suite::deadline_mid_reclaim_reports_one_json_object,
    },
    Test {
        name: "sigint_mid_reclaim_reports_one_json_object",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: signal_suite::sigint_mid_reclaim_reports_one_json_object,
    },
    Test {
        name: "sigterm_stubborn_command_child_gets_killed_after_grace",
        tier: Tier::Full,
        expect: Expect::Pass,
        run: signal_suite::sigterm_stubborn_command_child_gets_killed_after_grace,
    },
    Test {
        name: "agent_harness_uses_json_for_all_reclaim_decisions",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: agent_journey::agent_harness_uses_json_for_all_reclaim_decisions,
    },
    Test {
        name: PACKAGING_TEST,
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: packaging_suite::shipped_tmpfiles_and_example_work_in_container,
    },
    Test {
        name: "dormant_exported_tmpdir_is_invisible_and_broad_builtins_are_absent",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: temp_safety_suite::dormant_exported_tmpdir_is_invisible_and_broad_builtins_are_absent,
    },
    Test {
        name: "live_pytest_without_session_lock_is_unsafe_to_reclaim",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: temp_safety_suite::live_pytest_without_session_lock_is_unsafe_to_reclaim,
    },
    Test {
        name: "nested_claude_shell_cache_reclaims_only_its_exact_entry",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: temp_safety_suite::nested_claude_shell_cache_reclaims_only_its_exact_entry,
    },
    Test {
        name: "laptop_tmp_groups_protect_live_work_and_reclaim_independent_cruft",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: laptop_tmp_journey::laptop_tmp_groups_protect_live_work_and_reclaim_independent_cruft,
    },
    Test {
        name: "checked_in_python_profile_vector_replays_in_container",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: profile_vector_suite::checked_in_python_profile_vector_replays_in_container,
    },
    Test {
        name: "python_vector_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::python_vector_matches_linux_outcomes_and_deletion_diff,
    },
    Test {
        name: "bun_vector_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::bun_vector_matches_linux_outcomes_and_deletion_diff,
    },
    Test {
        name: "mypy_vector_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::mypy_vector_matches_linux_outcomes_and_deletion_diff,
    },
    Test {
        name: "brave_browser_vector_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::brave_browser_vector,
    },
    Test {
        name: "chrome_browser_vector_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::chrome_browser_vector,
    },
    Test {
        name: "chromium_browser_vector_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::chromium_browser_vector,
    },
    Test {
        name: "edge_browser_vector_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::edge_browser_vector,
    },
    Test {
        name: "pytest_unsafe_unrelated_fd_matches_linux",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::pytest_unsafe_unrelated_fd_matches_linux,
    },
    Test {
        name: "process_reference_negative_checks_bite",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::process_reference_negative_checks_bite,
    },
    Test {
        name: "python_in_use_vector_matches_linux",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::python_in_use_vector_matches_linux,
    },
    Test {
        name: "cargo_target_held_lock_matches_linux",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::cargo_target_held_lock_matches_linux,
    },
    Test {
        name: "cargo_target_shared_lock_matches_linux",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::cargo_target_shared_lock_matches_linux,
    },
    Test {
        name: "materialized_lock_releases_on_finish",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::materialized_lock_releases_on_finish,
    },
    Test {
        name: "missing_lock_probe_does_not_create_a_file",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::missing_lock_probe_does_not_create_a_file,
    },
    Test {
        name: "lock_mode_verifier_rejects_mismatch",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::lock_mode_verifier_rejects_mismatch,
    },
    Test {
        name: "python_nested_mount_matches_linux",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::python_nested_mount_matches_linux,
    },
    Test {
        name: "python_stacked_mount_matches_linux",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::python_stacked_mount_matches_linux,
    },
    Test {
        name: "python_bind_root_matches_linux",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::python_bind_root_matches_linux,
    },
    Test {
        name: "bind_root_alias_mutations_match_linux",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::bind_root_alias_mutations_match_linux,
    },
    Test {
        name: "generated_python_models_match_linux",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::generated_python_models_match_linux,
    },
    Test {
        name: "generated_mypy_models_match_linux",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::generated_mypy_models_match_linux,
    },
    Test {
        name: "historical_ctime_originals_refuse_before_fixture_writes",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::historical_ctime_originals_refuse_before_fixture_writes,
    },
    Test {
        name: "vm_cmake_original_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Vm,
        expect: Expect::Pass,
        run: differential_suite::vm_cmake_original_matches_linux_outcomes_and_deletion_diff,
    },
    Test {
        name: "vm_autotools_original_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Vm,
        expect: Expect::Pass,
        run: differential_suite::vm_autotools_original_matches_linux_outcomes_and_deletion_diff,
    },
    Test {
        name: "vm_meson_original_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Vm,
        expect: Expect::Pass,
        run: differential_suite::vm_meson_original_matches_linux_outcomes_and_deletion_diff,
    },
    Test {
        name: "vm_containers_image_original_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Vm,
        expect: Expect::Pass,
        run: differential_suite::vm_containers_image_original_matches_linux_outcomes_and_deletion_diff,
    },
    Test {
        name: "west_vector_matches_linux_under_clock_offset",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::west_vector_matches_linux_under_clock_offset,
    },
    Test {
        name: "xdg_vector_matches_linux_under_clock_offset",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::xdg_vector_matches_linux_under_clock_offset,
    },
    Test {
        name: "cargo_target_vector_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::cargo_target_vector,
    },
    Test {
        name: "chromium_tmp_vector_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::chromium_tmp_vector,
    },
    Test {
        name: "claude_code_tmp_vector_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::claude_code_tmp_vector,
    },
    Test {
        name: "claude_code_versions_vector_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::claude_code_versions_vector,
    },
    Test {
        name: "codex_releases_vector_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::codex_releases_vector,
    },
    Test {
        name: "cursor_agent_versions_vector_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::cursor_agent_versions_vector,
    },
    Test {
        name: "cursor_extensions_vector_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::cursor_extensions_vector,
    },
    Test {
        name: "go_mod_cache_vector_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::go_mod_cache_vector,
    },
    Test {
        name: "nix_user_cache_vector_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::nix_user_cache_vector,
    },
    Test {
        name: "node_compile_cache_vector_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::node_compile_cache_vector,
    },
    Test {
        name: "node_compile_cache_nested_vector_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::node_compile_cache_nested_vector,
    },
    Test {
        name: "playwright_tmp_vector_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::playwright_tmp_vector,
    },
    Test {
        name: "pytest_cache_vector_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::pytest_cache_vector,
    },
    Test {
        name: "renode_tmp_vector_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::renode_tmp_vector,
    },
    Test {
        name: "zephyr_tmp_vector_matches_linux_outcomes_and_deletion_diff",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: differential_suite::zephyr_tmp_vector,
    },
    Test {
        name: MESON_ANCHOR_TEST,
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: meson_suite::meson_generated_output_uses_surviving_source_anchor,
    },
    Test {
        name: AUTOTOOLS_ANCHOR_TEST,
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: autotools_suite::autotools_generated_output_uses_surviving_source_anchor,
    },
    Test {
        name: WEST_REAL_BUILD_TEST,
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: embedded_suite::west_generated_output_uses_surviving_source_anchor,
    },
    Test {
        name: DOCKER_DIND_TEST,
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: docker_suite::native_docker_engine_dind_smoke,
    },
    Test {
        name: "docker_query_descendant_pipe_holder_hits_deadline",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: docker_suite::docker_query_descendant_pipe_holder_hits_deadline,
    },
    Test {
        name: DOCKER_PACKAGING_TEST,
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: packaging_suite::docker_shipped_tmpfiles_and_example_work_in_container,
    },
    Test {
        name: DOCKER_VECTOR_TEST,
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: docker_vector_suite::checked_in_python_profile_vector_replays_under_native_docker,
    },
    Test {
        name: "native_docker_image_prune_preserves_containers_volumes",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: docker_suite::native_docker_image_prune_preserves_containers_volumes,
    },
    Test {
        name: "native_docker_prune_child_keeps_lock_after_parent_kill",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: docker_suite::native_docker_prune_child_keeps_lock_after_parent_kill,
    },
    Test {
        name: "native_docker_dangling_and_build_cache_prune",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: docker_suite::native_docker_dangling_and_build_cache_prune,
    },
    Test {
        name: "ensure_stdout_failures_exit_three_without_panicking",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: stdout_suite::ensure_stdout_failures_exit_three_without_panicking,
    },
    Test {
        name: BUILDAH_SPOOL_TEST,
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: buildah_suite::containers_image_spool_held_by_buildah_is_in_use,
    },
    Test {
        name: "python_pycache_reclaims_only_selected_project_caches",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: project_cache_suite::python_pycache_reclaims_only_selected_project_caches,
    },
    Test {
        name: "mypy_cache_requires_regular_tag_and_preserves_project",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: project_cache_suite::mypy_cache_requires_regular_tag_and_preserves_project,
    },
    Test {
        name: "pip_cache_cleaner",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: cache_suite::pip_cache_cleaner,
    },
    Test {
        name: "uv_cache_cleaner",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: cache_suite::uv_cache_cleaner,
    },
    Test {
        name: "named_checkout_policy_for_command_caches",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: cache_suite::named_checkout_policy_for_command_caches,
    },
    Test {
        name: "named_checkout_policy_cannot_expand_proposed_catalog",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: cache_suite::named_checkout_policy_cannot_expand_proposed_catalog,
    },
    Test {
        name: "go_build_cache_cleaner",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: cache_suite::go_build_cache_cleaner,
    },
    Test {
        name: "npm_cache_cleaner",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: cache_suite::npm_cache_cleaner,
    },
    Test {
        name: "pnpm_store_cleaner",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: cache_suite::pnpm_store_cleaner,
    },
    Test {
        name: "yarn_cache_cleaner",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: cache_suite::yarn_cache_cleaner,
    },
    Test {
        name: "ccache_cleaner",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: cache_suite::ccache_cleaner,
    },
    Test {
        name: "jest_cache_cleaner",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: cache_suite::jest_cache_cleaner,
    },
    Test {
        name: "platformio_cache_cleaner",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: cache_suite::platformio_cache_cleaner,
    },
    Test {
        name: "anchored_west_build_keeps_unrebuildable_outputs",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: embedded_suite::anchored_west_build_keeps_unrebuildable_outputs,
    },
    Test {
        name: "brave_cache_lock_blocks_until_absent_then_reclaims_only_cache",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: browser_suite::brave_cache_lock_blocks_until_absent_then_reclaims_only_cache,
    },
    Test {
        name: "fs_reclaim_exact_exclusion_keeps_valuable_cache",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: fs_reclaim_suite::fs_reclaim_exact_exclusion_keeps_valuable_cache,
    },
    Test {
        name: "fs_negated_profile_dry_run_excludes_ancestor",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: fs_reclaim_suite::fs_negated_profile_dry_run_excludes_ancestor,
    },
    Test {
        name: "fs_reclaim_cmake_guard_keeps_a_dangling_source_symlink",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: fs_reclaim_suite::fs_reclaim_cmake_guard_keeps_a_dangling_source_symlink,
    },
    Test {
        name: "fs_reclaim_anchored_cmake_keeps_unrebuildable_outputs",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: fs_reclaim_suite::fs_reclaim_anchored_cmake_keeps_unrebuildable_outputs,
    },
    Test {
        name: "fs_reclaim_image_temp_minimum_idle_and_open_file",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: fs_reclaim_suite::fs_reclaim_image_temp_minimum_idle_and_open_file,
    },
    Test {
        name: "fs_reclaim_negated_idle_filter_keeps_old_candidate",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: fs_reclaim_suite::fs_reclaim_negated_idle_filter_keeps_old_candidate,
    },
    Test {
        name: "fs_reclaim_negated_entry_filters",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: fs_reclaim_suite::fs_reclaim_negated_entry_filters,
    },
    Test {
        name: "fs_reclaim_positive_entry_filters",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: fs_reclaim_suite::fs_reclaim_positive_entry_filters,
    },
    Test {
        name: "fs_reclaim_mixed_entry_and_idle_filter",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: fs_reclaim_suite::fs_reclaim_mixed_entry_and_idle_filter,
    },
    Test {
        name: "fs_reclaim_negated_mixed_entry_and_idle_filter",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: fs_reclaim_suite::fs_reclaim_negated_mixed_entry_and_idle_filter,
    },
    Test {
        name: "fs_union_checkout_policy_uses_admitted_terms",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: fs_reclaim_suite::fs_union_checkout_policy_uses_admitted_terms,
    },
    Test {
        name: "fs_union_checkout_aliases_and_mixed_roots",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: fs_reclaim_suite::fs_union_checkout_aliases_and_mixed_roots,
    },
    Test {
        name: "fs_union_retention_guards_veto_shared_candidates",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: fs_reclaim_suite::fs_union_retention_guards_veto_shared_candidates,
    },
    Test {
        name: "fs_reclaim_positive_union_dedupes_and_refuses_shared_checkout",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: fs_reclaim_suite::fs_reclaim_positive_union_dedupes_and_refuses_shared_checkout,
    },
    Test {
        name: "fs_command_union_invokes_once_with_selected_source",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: fs_reclaim_suite::fs_command_union_invokes_once_with_selected_source,
    },
    Test {
        name: "nix_evaluation_keeps_its_user_eval_cache_in_use",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: inuse_suite::nix_evaluation_keeps_its_user_eval_cache_in_use,
    },
    Test {
        name: "go_mod_cache_builtin_deletes_a_read_only_module_tree",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::go_mod_cache_builtin_deletes_a_read_only_module_tree,
    },
    Test {
        name: "codex_version_running_binary_is_in_use",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::codex_version_running_binary_is_in_use,
    },
    Test {
        name: "codex_current_internal_dotdot_is_ambiguous",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::codex_current_internal_dotdot_is_ambiguous,
    },
    Test {
        name: "sandbox_trivial",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: sandbox_trivial,
    },
    Test {
        name: "tester_cannot_read_other_users_mode000_dir",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: tester_cannot_read_other_users_mode000_dir,
    },
    Test {
        name: "interlock_canary_passes_in_container",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: interlock_canary_passes_in_container,
    },
    Test {
        name: "interlock_aborts_outside_scratch",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: interlock_aborts_outside_scratch,
    },
    Test {
        name: "oracle_clean_when_only_eligible_objects_go",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: oracle_clean_when_only_eligible_objects_go,
    },
    Test {
        name: "oracle_catches_deletion_outside_eligible_set",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: oracle_catches_deletion_outside_eligible_set,
    },
    Test {
        name: "oracle_catches_replaced_objects",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: oracle_catches_replaced_objects,
    },
    Test {
        name: "oracle_mandatory_unjudged_run_fails_the_test",
        tier: Tier::Fast,
        expect: Expect::FailWith(UNJUDGED_MESSAGE),
        run: oracle_mandatory_unjudged_run_fails_the_test,
    },
    Test {
        name: "oracle_logs_unmatched_strace_removals",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: oracle_logs_unmatched_strace_removals,
    },
    Test {
        name: "snapshot_skips_read_only_mounts",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: snapshot_skips_read_only_mounts,
    },
    Test {
        name: "strace_seccomp_bpf_churn_within_bound",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: strace_seccomp_bpf_churn_within_bound,
    },
    Test {
        name: "host_resolve_refuses_symlinks",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: host_suite::host_resolve_refuses_symlinks,
    },
    Test {
        name: "host_read_open_of_fifo_does_not_wait_for_writer",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: host_suite::host_read_open_of_fifo_does_not_wait_for_writer,
    },
    Test {
        name: "host_resolve_refuses_mount_crossings",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: host_suite::host_resolve_refuses_mount_crossings,
    },
    Test {
        name: "host_statx_and_statvfs_on_a_real_kernel",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: host_suite::host_statx_and_statvfs_on_a_real_kernel,
    },
    Test {
        name: "host_flock_release_only_by_close",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: host_suite::host_flock_release_only_by_close,
    },
    Test {
        name: "host_inuse_scan_settles_after_fork_churn",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: host_suite::host_inuse_scan_settles_after_fork_churn,
    },
    Test {
        name: "host_inuse_scan_under_sustained_fork_churn",
        tier: Tier::Full,
        expect: Expect::Pass,
        run: host_suite::host_inuse_scan_under_sustained_fork_churn,
    },
    Test {
        name: "host_pidfd_release_after_reap",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: host_suite::host_pidfd_release_after_reap,
    },
    Test {
        name: "host_proc_readers",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: host_suite::host_proc_readers,
    },
    Test {
        name: "host_procfs_stays_pinned_after_overmount",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: host_suite::host_procfs_stays_pinned_after_overmount,
    },
    Test {
        name: "host_proc_readers_see_threads_after_main_exits",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: host_suite::host_proc_readers_see_threads_after_main_exits,
    },
    Test {
        name: "host_proc_readers_see_threads_with_own_fd_table",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: host_suite::host_proc_readers_see_threads_with_own_fd_table,
    },
    Test {
        name: "host_proc_maps_while_the_leader_exits_slowly",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: host_suite::host_proc_maps_while_the_leader_exits_slowly,
    },
    Test {
        name: "host_kernel_older_than_5_8_exits_2",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: host_suite::host_kernel_older_than_5_8_exits_2,
    },
    Test {
        name: "host_unlinkat_deletes_inside_the_sandbox",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: host_suite::host_unlinkat_deletes_inside_the_sandbox,
    },
    Test {
        name: "driver_walk_sees_a_change_made_while_paused",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: driver_suite::driver_walk_sees_a_change_made_while_paused,
    },
    Test {
        name: "driver_delete_pauses_and_resumes_at_a_host_call",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: driver_suite::driver_delete_pauses_and_resumes_at_a_host_call,
    },
    Test {
        name: "driver_delete_killed_at_a_host_call",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: driver_suite::driver_delete_killed_at_a_host_call,
    },
    Test {
        name: "driver_delete_unlinks_what_replaced_an_entry_while_paused",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: driver_suite::driver_delete_unlinks_what_replaced_an_entry_while_paused,
    },
    Test {
        name: "cargo_smoke_reports_a_broken_toolchain_as_a_toolchain_failure",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: cargo_suite::cargo_smoke_reports_a_broken_toolchain_as_a_toolchain_failure,
    },
    Test {
        name: "cargo_reclaim_lock_hold_time_on_a_large_target",
        tier: Tier::Full,
        expect: Expect::Pass,
        run: cargo_reclaim_suite::cargo_reclaim_lock_hold_time_on_a_large_target,
    },
    Test {
        name: "oracle_large_tree",
        tier: Tier::Full,
        expect: Expect::Pass,
        run: oracle_large_tree,
    },
    Test {
        name: "engine_deletes_an_eligible_tree_and_nothing_else",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: engine_suite::engine_deletes_an_eligible_tree_and_nothing_else,
    },
    Test {
        name: "engine_skips_a_mixed_owner_tree_as_not_owner",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: engine_suite::engine_skips_a_mixed_owner_tree_as_not_owner,
    },
    Test {
        name: "engine_skips_a_nested_keep_marker_or_cargo_target",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: engine_suite::engine_skips_a_nested_keep_marker_or_cargo_target,
    },
    Test {
        name: "engine_skips_a_source_checkout",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: engine_suite::engine_skips_a_source_checkout,
    },
    Test {
        name: "engine_fails_closed_on_a_mode_000_directory",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: engine_suite::engine_fails_closed_on_a_mode_000_directory,
    },
    Test {
        name: "engine_skips_sockets_and_fifos",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: engine_suite::engine_skips_sockets_and_fifos,
    },
    Test {
        name: "engine_never_deletes_an_entry_replaced_after_the_walk",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: engine_suite::engine_never_deletes_an_entry_replaced_after_the_walk,
    },
    Test {
        name: "engine_keeps_an_entry_renamed_out_after_the_walk",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: engine_suite::engine_keeps_an_entry_renamed_out_after_the_walk,
    },
    Test {
        name: "engine_stops_at_file_bind_mount_after_identity_check",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: engine_suite::engine_stops_at_file_bind_mount_after_identity_check,
    },
    Test {
        name: "engine_stops_at_a_mount_that_appears_mid_delete",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: engine_suite::engine_stops_at_a_mount_that_appears_mid_delete,
    },
    Test {
        name: "engine_make_writable_deletes_a_read_only_tree",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: engine_suite::engine_make_writable_deletes_a_read_only_tree,
    },
    Test {
        name: "engine_make_writable_preserves_a_concurrent_mode_change",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: engine_suite::engine_make_writable_preserves_a_concurrent_mode_change,
    },
    Test {
        name: "engine_deletes_a_tree_deeper_than_path_max",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: engine_suite::engine_deletes_a_tree_deeper_than_path_max,
    },
    Test {
        name: "engine_deletes_a_100k_entry_30_deep_tree_under_rlimit_nofile_32",
        tier: Tier::Full,
        expect: Expect::Pass,
        run: engine_suite::engine_deletes_a_100k_entry_30_deep_tree_under_rlimit_nofile_32,
    },
    Test {
        name: "engine_deletes_a_200k_entry_directory",
        tier: Tier::Full,
        expect: Expect::Pass,
        run: engine_suite::engine_deletes_a_200k_entry_directory,
    },
    Test {
        name: "fs_reclaim_tmp_fixture_deletes_every_eligible_candidate_and_nothing_else",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: fs_reclaim_suite::fs_reclaim_tmp_fixture_deletes_every_eligible_candidate_and_nothing_else,
    },
    Test {
        name: "fs_reclaim_100k_entry_tmp_deletes_every_eligible_candidate_and_nothing_else",
        tier: Tier::Full,
        expect: Expect::Pass,
        run: fs_reclaim_suite::fs_reclaim_100k_entry_tmp_deletes_every_eligible_candidate_and_nothing_else,
    },
    Test {
        name: "fs_reclaim_never_deletes_a_socket_or_fifo",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: fs_reclaim_suite::fs_reclaim_never_deletes_a_socket_or_fifo,
    },
    Test {
        name: "fs_reclaim_never_enters_private_tmp_inside_a_candidate",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: fs_reclaim_suite::fs_reclaim_never_enters_private_tmp_inside_a_candidate,
    },
    Test {
        name: "fs_reclaim_a_dead_fuse_mount_is_never_touched",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: fs_reclaim_suite::fs_reclaim_a_dead_fuse_mount_is_never_touched,
    },
    Test {
        name: "fs_protected_dead_fuse_root_is_refused_without_opening_it",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: fs_reclaim_suite::fs_protected_dead_fuse_root_is_refused_without_opening_it,
    },
    Test {
        name: "inuse_scan_finishes_with_a_process_in_a_stalled_fuse_mount",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: fuse_stall_suite::inuse_scan_finishes_with_a_process_in_a_stalled_fuse_mount,
    },
    Test {
        name: "configured_groups_reclaim_in_order",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::configured_groups_reclaim_in_order,
    },
    Test {
        name: "configured_random_group_reports_seed_and_order",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::configured_random_group_reports_seed_and_order,
    },
    Test {
        name: "configured_protect_marks_overlapping_fs_candidates_in_dry_run",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::configured_protect_marks_overlapping_fs_candidates_in_dry_run,
    },
    Test {
        name: "ensure_above_min_free_exits_0_without_the_host_lock",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::ensure_above_min_free_exits_0_without_the_host_lock,
    },
    Test {
        name: "ensure_dry_run_plans_while_the_host_lock_is_held",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::ensure_dry_run_plans_while_the_host_lock_is_held,
    },
    Test {
        name: "ensure_flag_errors_are_single_json_reports",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::ensure_flag_errors_are_single_json_reports,
    },
    Test {
        name: "ensure_claude_and_pytest_tmp_profiles_keep_open_units",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::ensure_claude_and_pytest_tmp_profiles_keep_open_units,
    },
    Test {
        name: "command_profile_checks_candidate_and_reports_nonzero_exit",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::command_profile_checks_candidate_and_reports_nonzero_exit,
    },
    Test {
        name: "command_profile_deletes_only_an_eligible_file",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::command_profile_deletes_only_an_eligible_file,
    },
    Test {
        name: "ensure_without_the_host_lock_file_exits_2_printing_the_tmpfiles_lines",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::ensure_without_the_host_lock_file_exits_2_printing_the_tmpfiles_lines,
    },
    Test {
        name: "ensure_goal_met_warns_about_an_unready_host_lock",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::ensure_goal_met_warns_about_an_unready_host_lock,
    },
    Test {
        name: "ensure_as_root_without_allow_root_exits_2",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::ensure_as_root_without_allow_root_exits_2,
    },
    Test {
        name: "root_mode_uses_protected_config_and_named_owner",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::root_mode_uses_protected_config_and_named_owner,
    },
    Test {
        name: "root_mode_uses_explicit_uid_without_passwd_entry",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::root_mode_uses_explicit_uid_without_passwd_entry,
    },
    Test {
        name: "root_mode_refuses_uid_placeholder_for_one_or_two_owners",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::root_mode_refuses_uid_placeholder_for_one_or_two_owners,
    },
    Test {
        name: "root_mode_refuses_untrusted_policy_and_system_owner",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::root_mode_refuses_untrusted_policy_and_system_owner,
    },
    Test {
        name: "profiles_work_as_root_while_ensure_refuses",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::profiles_work_as_root_while_ensure_refuses,
    },
    Test {
        name: "ensure_refuses_a_host_lock_that_is_no_regular_file",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::ensure_refuses_a_host_lock_that_is_no_regular_file,
    },
    Test {
        name: "ensure_waits_for_a_held_host_lock_until_the_deadline",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::ensure_waits_for_a_held_host_lock_until_the_deadline,
    },
    Test {
        name: "ensure_host_lock_is_inherited_by_children_and_released_only_by_close",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::ensure_host_lock_is_inherited_by_children_and_released_only_by_close,
    },
    Test {
        name: "command_action_child_holds_host_lock_after_parent_is_killed",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::command_action_child_holds_host_lock_after_parent_is_killed,
    },
    Test {
        name: "command_action_never_changes_inherited_stderr_flags",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::command_action_never_changes_inherited_stderr_flags,
    },
    Test {
        name: "command_action_timeout_kills_descendant_after_leader_exits",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::command_action_timeout_kills_descendant_after_leader_exits,
    },
    Test {
        name: "command_action_long_output_is_bounded_and_prefixed",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::command_action_long_output_is_bounded_and_prefixed,
    },
    Test {
        name: "command_action_orphaned_output_is_killed_before_reap",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::command_action_orphaned_output_is_killed_before_reap,
    },
    Test {
        name: "command_action_stalled_stderr_fails_without_hanging",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::command_action_stalled_stderr_fails_without_hanging,
    },
    Test {
        name: "command_action_leader_exit_drain_obeys_tier_deadline",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::command_action_leader_exit_drain_obeys_tier_deadline,
    },
    Test {
        name: "command_action_closed_pipe_descendant_is_killed",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: ensure_suite::command_action_closed_pipe_descendant_is_killed,
    },
    Test {
        name: "inuse_a_held_file_cwd_mapping_or_environment_keeps_the_candidate",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: inuse_suite::inuse_a_held_file_cwd_mapping_or_environment_keeps_the_candidate,
    },
    Test {
        name: "inuse_a_flocked_directory_is_locked",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: inuse_suite::inuse_a_flocked_directory_is_locked,
    },
    Test {
        name: "inuse_an_export_after_exec_is_invisible",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: inuse_suite::inuse_an_export_after_exec_is_invisible,
    },
    Test {
        name: "inuse_a_process_in_another_mount_namespace_is_matched",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: inuse_suite::inuse_a_process_in_another_mount_namespace_is_matched,
    },
    Test {
        name: "inuse_unreadable_processes_block_unless_their_status_explains_them",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: inuse_suite::inuse_unreadable_processes_block_unless_their_status_explains_them,
    },
    Test {
        name: "inuse_only_live_sockets_and_fifos_keep_their_candidate",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: inuse_suite::inuse_only_live_sockets_and_fifos_keep_their_candidate,
    },
    Test {
        name: "inuse_a_child_forked_before_its_parent_was_read_is_found",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: inuse_suite::inuse_a_child_forked_before_its_parent_was_read_is_found,
    },
    Test {
        name: "inuse_held_by_deleted_files_lists_open_deleted_files",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: inuse_suite::inuse_held_by_deleted_files_lists_open_deleted_files,
    },
    Test {
        name: "inuse_a_mapping_through_overlayfs_is_matched",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: inuse_suite::inuse_a_mapping_through_overlayfs_is_matched,
    },
    Test {
        name: "inuse_a_process_in_its_own_user_namespace_is_read",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: inuse_suite::inuse_a_process_in_its_own_user_namespace_is_read,
    },
    Test {
        name: "cargo_race_6_a_mixed_layout_target_killed_at_seeded_positions",
        tier: Tier::Full,
        expect: Expect::Pass,
        run: cargo_race_suite::cargo_race_6_a_mixed_layout_target_killed_at_seeded_positions,
    },
    Test {
        name: "inuse_a_more_capable_process_blocks_outside_the_initial_namespace",
        tier: Tier::Fast,
        expect: Expect::Pass,
        run: inuse_suite::inuse_a_more_capable_process_blocks_outside_the_initial_namespace,
    },
];

/// Prints `name<TAB>tier` per test (read by the runner): [`TESTS`], then
/// every Cargo test per toolchain in the image, each only if the image
/// runs it (its descriptor, [`crate::image`]). Without a readable
/// descriptor, or a readable toolchain manifest where the descriptor says
/// `toolchains manifest`, the listing fails: tests never silently vanish.
pub fn list() -> i32 {
    match listing() {
        Ok((image, lines)) => {
            println!("{}{image}", crate::runner::IMAGE_LINE);
            for (name, tier) in lines {
                println!("{name}\t{}", tier.as_str());
            }
            exit::PASS
        }
        Err(why) => {
            eprintln!("diskgc-e2e list: {why}");
            exit::FAIL
        }
    }
}

/// The image's name and the tests it runs, with their tiers.
fn listing() -> Result<(String, Vec<(String, Tier)>), String> {
    let image = Image::load()?;
    let toolchains = match image.toolchains {
        image::Toolchains::Manifest => Toolchain::all()?,
        image::Toolchains::None => Vec::new(),
    };
    let all = TESTS
        .iter()
        .map(|t| (t.name.to_owned(), t.tier))
        .chain(cargo_suite::listing(&toolchains));
    let lines: Vec<_> = all
        .filter(|(name, _)| image.runs(name, NIX_ONLY_TESTS, DOCKER_ONLY_TESTS))
        .collect();
    ensure!(!lines.is_empty(), "image {} lists no test", image.name);
    Ok((image.name, lines))
}

/// A test resolved from its listed name.
enum Resolved {
    Plain(&'static Test),
    /// A Cargo test on one toolchain; its smoke check runs first.
    Cargo(&'static cargo_suite::CargoTest, Toolchain),
}

impl Resolved {
    fn find(name: &str) -> Result<Self, String> {
        if let Some(test) = TESTS.iter().find(|t| t.name == name) {
            return Ok(Self::Plain(test));
        }
        match cargo_suite::find(name) {
            Some(found) => found.map(|(test, toolchain)| Self::Cargo(test, toolchain)),
            None => Err(format!("no test named {name:?}")),
        }
    }

    fn expect(&self) -> Expect {
        match self {
            Self::Plain(test) => test.expect,
            Self::Cargo(..) => Expect::Pass,
        }
    }

    /// Runs the body; a Cargo test's smoke check first, recording a broken
    /// toolchain in `ctx`.
    fn run(&self, ctx: &Ctx) -> TestResult {
        match self {
            Self::Plain(test) => (test.run)(ctx),
            Self::Cargo(test, toolchain) => {
                cargo_under_test::smoke(ctx, toolchain)
                    .map_err(|broken| ctx.toolchain_broken(&broken))?;
                (test.run)(ctx, toolchain)
            }
        }
    }
}

/// Sets up the context, runs test `name`, applies the teardown rules and
/// prints its result.
pub fn run(name: &str) -> i32 {
    let test = match Resolved::find(name) {
        Ok(test) => test,
        Err(why) => {
            eprintln!("diskgc-e2e: {why}");
            return exit::USAGE;
        }
    };
    let image = match Image::load() {
        Ok(image) if !image.runs(name, NIX_ONLY_TESTS, DOCKER_ONLY_TESTS) => {
            let base = name.split('@').next().unwrap_or(name);
            let why = NIX_ONLY_TESTS
                .iter()
                .chain(DOCKER_ONLY_TESTS.iter())
                .find(|(n, _)| *n == base)
                .map_or("", |(_, why)| *why);
            eprintln!(
                "diskgc-e2e: {name} is not run on the {} image: {why}",
                image.name
            );
            return exit::USAGE;
        }
        Ok(image) => image,
        Err(why) => {
            eprintln!("diskgc-e2e: {why}");
            return exit::FAIL;
        }
    };
    let started = Instant::now();
    let (outcome, toolchain) = match Ctx::setup(image) {
        Ok(ctx) => {
            let body = test.run(&ctx);
            let outcome = judge_expectation(test.expect(), body, ctx.teardown());
            (outcome, ctx.toolchain_failure.get().cloned())
        }
        Err(why) => (Err(format!("setup failed: {why}")), None),
    };
    let (line, code) = result_line(name, started.elapsed(), outcome, toolchain);
    println!("{line}");
    code
}

/// The result line and exit code of a test: `PASS`, `FAIL`, or `TOOLCHAIN`
/// when it failed and a toolchain under test it needed had failed its smoke
/// check (a toolchain problem, not a failure of what the test checks).
pub fn result_line(
    name: &str,
    took: Duration,
    outcome: Result<String, String>,
    toolchain_failure: Option<String>,
) -> (String, i32) {
    match (outcome, toolchain_failure) {
        (Ok(note), _) => (
            format!("PASS {name} ({took:.2?} in container){note}"),
            exit::PASS,
        ),
        (Err(why), Some(broken)) => (
            format!(
                "TOOLCHAIN {name} ({took:.2?} in container): not run, its toolchain is broken: \
                 {broken}\n(test result: {why})"
            ),
            exit::TOOLCHAIN,
        ),
        (Err(why), None) => (
            format!("FAIL {name} ({took:.2?} in container): {why}"),
            exit::FAIL,
        ),
    }
}

/// Whether a test did what it expects, from its body's result and the
/// teardown rules' result ([`Ctx::teardown`]); `Ok` carries a note for the
/// PASS line. A [`Expect::FailWith`] bite proof passes only when its body
/// passed (so the body's own checks still count) and the rules then failed
/// it with the expected text.
pub fn judge_expectation(
    expect: Expect,
    body: TestResult,
    rules: TestResult,
) -> Result<String, String> {
    match (expect, body, rules) {
        (Expect::Pass, Ok(()), Ok(())) => Ok(String::new()),
        (Expect::Pass, Ok(()), Err(owed)) => Err(owed),
        (Expect::Pass, Err(why), Ok(())) => Err(why),
        (Expect::Pass, Err(why), Err(owed)) => Err(format!("{why}\n(also: {owed})")),
        (Expect::FailWith(text), Err(why), rules) => Err(format!(
            "bite proof for {text:?}: its body failed, so the rule was not what failed it: \
             {why}{}",
            rules
                .err()
                .map(|o| format!("\n(also: {o})"))
                .unwrap_or_default()
        )),
        (Expect::FailWith(text), Ok(()), Err(owed)) if owed.contains(text) => {
            Ok(format!(": failed as it must: {owed}"))
        }
        (Expect::FailWith(text), Ok(()), Err(owed)) => Err(format!(
            "expected the teardown rules to fail with {text:?}, they failed with: {owed}"
        )),
        (Expect::FailWith(text), Ok(()), Ok(())) => Err(format!(
            "expected to fail with {text:?}, but passed: the rule it proves does not bite"
        )),
    }
}

/// What every test gets: its scratch mounts, the harness's records mount,
/// the ledger of traced runs and the binaries under test.
pub struct Ctx {
    /// tmpfs at `/tmp` (mode 1777), where diskgc's filesystem profiles look.
    pub scratch: Scratch,
    /// tmpfs at `/var/tmp` for container image spool fixtures.
    pub var_tmp: Scratch,
    /// tmpfs at `/run/diskgc` (0755 root) holding the host lock created
    /// from the shipped tmpfiles rule on every image.
    pub run_diskgc: Scratch,
    /// tmpfs at `/home/tester` (0755 tester), tester's home.
    pub home: Scratch,
    pub records: Scratch,
    pub ledger: Ledger,
    pub bin_dir: PathBuf,
    pub self_exe: PathBuf,
    /// The image's descriptor ([`crate::image`]).
    pub image: Image,
    /// Set when a Cargo toolchain under test failed its smoke check
    /// ([`Ctx::toolchain_broken`]).
    toolchain_failure: OnceLock<String>,
}

/// The host lock (docs/safety.md, Host lock).
pub const HOST_LOCK: &str = "/run/diskgc/lock";

impl Ctx {
    /// Checks the container's root filesystem, bind-mounts read-only what
    /// the image's descriptor names (the Nix image's store, the Debian
    /// image's `/usr`), then mounts the records tmpfs (root only) and the
    /// scratch tmpfs mounts, and provisions the host lock and tester's
    /// profile on them.
    pub fn setup(image: Image) -> Result<Self, String> {
        let uid = rustix::process::getuid().as_raw();
        ensure!(
            uid == 0,
            "the harness must run as container root to build fixtures, runs as uid {uid}"
        );
        require_native_overlay_root()?;
        for dir in &image.read_only {
            bind_read_only(dir)?;
        }
        let records = Scratch::mount("/run/diskgc-e2e", "0700", ROOT)?;
        let scratch = Scratch::mount("/tmp", "1777", ROOT)?;
        let var_tmp = Scratch::mount("/var/tmp", "1777", ROOT)?;
        let run_diskgc = Scratch::mount("/run/diskgc", "0755", ROOT)?;
        let tool = &image.tmpfiles;
        let rule = Path::new(UNDER_TEST_BIN_DIR).join("diskgc-tmpfiles.conf");
        let applied = Command::new(tool)
            .arg("--create")
            .arg(&rule)
            .env_clear()
            .env("PATH", UNDER_TEST_BIN_DIR)
            .output()
            .map_err(|e| format!("run {} on shipped {}: {e}", tool.display(), rule.display()))?;
        ensure!(
            applied.status.success(),
            "{} failed to apply shipped {}: status {}, stdout {}, stderr {}",
            tool.display(),
            rule.display(),
            applied.status,
            String::from_utf8_lossy(&applied.stdout),
            String::from_utf8_lossy(&applied.stderr)
        );
        expect_meta("/run/diskgc", true, 0o755, 0, 0)?;
        expect_meta(HOST_LOCK, false, 0o644, 0, 0)?;
        ensure!(
            scratch::mount_id(Path::new(HOST_LOCK), false)? == run_diskgc.mnt_id(),
            "shipped tmpfiles rule created the host lock outside /run/diskgc scratch"
        );
        let home = Scratch::mount("/home/tester", "0755", TESTER)?;
        home.write_file(".profile", b"# tester profile\n", 0o644, TESTER)?;
        let self_exe =
            std::env::current_exe().map_err(|e| format!("cannot find own executable: {e}"))?;
        Ok(Self {
            scratch,
            var_tmp,
            run_diskgc,
            home,
            records,
            ledger: Ledger::new(),
            bin_dir: PathBuf::from(UNDER_TEST_BIN_DIR),
            self_exe,
            image,
            toolchain_failure: OnceLock::new(),
        })
    }

    /// Records that a Cargo toolchain under test failed its smoke check, so
    /// the test is reported as `TOOLCHAIN`; returns the message.
    pub fn toolchain_broken(&self, broken: &ToolchainBroken) -> String {
        let message = broken.to_string();
        // The first broken toolchain is the one reported.
        let _ = self.toolchain_failure.set(message.clone());
        message
    }

    /// The rules every test obeys, checked after its body: every traced run
    /// of code under test was judged by the deletion oracle.
    pub fn teardown(&self) -> TestResult {
        self.ledger.require_all_judged()
    }

    /// The diskgc binary under test.
    pub fn diskgc(&self) -> PathBuf {
        self.bin_dir.join("diskgc")
    }

    /// Runs `program args...` as `tester` under strace, from `/`.
    pub fn run_as_tester(&self, program: &Path, args: &[&str]) -> Result<Run, String> {
        under_test::run_as_tester(&self.ledger, &self.records, Path::new("/"), program, args)
    }

    /// Runs `program args...` as container root under strace, from `/`
    /// (the root refusal: docs/safety.md, Privileges).
    pub fn run_as_root(&self, program: &Path, args: &[&str]) -> Result<Run, String> {
        under_test::run_as_root(&self.ledger, &self.records, Path::new("/"), program, args)
    }

    /// The test driver under test (crate::driver).
    pub fn driver(&self) -> PathBuf {
        self.bin_dir.join("diskgc-e2e-driver")
    }

    /// Starts `program args...` as `tester` under strace, from `/`, and
    /// returns while it runs (to pause, resume or see it killed).
    pub fn spawn_as_tester(
        &self,
        program: &Path,
        args: &[&str],
    ) -> Result<under_test::Spawned, String> {
        under_test::spawn_as_tester(&self.ledger, &self.records, Path::new("/"), program, args)
    }

    /// A traced tester whose stderr is an fd supplied by the harness.
    pub fn spawn_as_tester_with_stderr(
        &self,
        program: &Path,
        args: &[&str],
        stderr: std::os::fd::OwnedFd,
    ) -> Result<under_test::Spawned, String> {
        under_test::spawn_as_tester_with_stderr(
            &self.ledger,
            &self.records,
            Path::new("/"),
            program,
            args,
            stderr,
        )
    }

    /// Runs a probe as the code under test (`tester`, traced).
    pub fn probe_as_tester(&self, args: &[&str]) -> Result<Run, String> {
        let mut full = vec!["probe"];
        full.extend_from_slice(args);
        self.run_as_tester(&self.self_exe, &full)
    }

    /// Runs a probe as container root (untraced), for interlock checks and
    /// baselines.
    pub fn probe_as_root(&self, args: &[&str]) -> Result<Output, String> {
        Command::new(&self.self_exe)
            .arg("probe")
            .args(args)
            .output()
            .map_err(|e| format!("spawn probe {args:?}: {e}"))
    }
}

/// Bind-mounts `path` (a directory of the image) read-only over itself, so
/// the deletion oracle's snapshot skips it like every read-only mount
/// (docs/testing.md, Grounding): the Cargo toolchains under test are
/// several 10,000 files on the overlay root, which nothing deletes.
fn bind_read_only(path: &Path) -> Result<(), String> {
    // A test-specific Podman volume can already be read-only. Remounting a
    // container bind of the host's Nix store is denied by the outer user
    // namespace; the existing read-only mount satisfies the same invariant.
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|e| format!("read /proc/self/mountinfo: {e}"))?;
    let mounts = snapshot::parse_mountinfo(&mountinfo)?;
    let exact: Vec<_> = mounts.iter().filter(|m| m.mount_point == path).collect();
    if !exact.is_empty() && exact.iter().all(|m| m.read_only) {
        return Ok(());
    }
    rustix::mount::mount_bind(path, path)
        .map_err(|e| format!("bind mount {}: {e}", path.display()))?;
    rustix::mount::mount_remount(path, MountFlags::BIND | MountFlags::RDONLY, c"")
        .map_err(|e| format!("remount {} read-only: {e}", path.display()))
}

/// The snapshot's key rules (docs/testing.md, Grounding) are validated on
/// Podman's native overlay root only. fuse-overlayfs (`fuse.fuse-overlayfs`
/// in mountinfo) reports inode numbers and birth times differently; the
/// runner refuses it up front (`podman info`), this is the in-container
/// second check.
fn require_native_overlay_root() -> Result<(), String> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|e| format!("read /proc/self/mountinfo: {e}"))?;
    let root_id = scratch::mount_id(Path::new("/"), false)?;
    let fs_type = snapshot::parse_mountinfo(&mountinfo)?
        .into_iter()
        .find(|m| m.mount_id == root_id)
        .map(|m| m.fs_type)
        .ok_or_else(|| format!("the root mount (id {root_id}) is not in mountinfo"))?;
    ensure!(
        fs_type == "overlay",
        "the container's root filesystem is {fs_type:?}, not native overlayfs: the deletion \
         oracle's snapshot keys are validated on Podman's native overlay driver only \
         (docs/testing.md, Container setup)"
    );
    Ok(())
}

/// Fails unless `path` has this file type, permission bits and owner.
fn expect_meta(path: &str, dir: bool, mode: u32, uid: u32, gid: u32) -> TestResult {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("lstat {path}: {e}"))?;
    ensure!(
        meta.is_dir() == dir,
        "{path}: is_dir {} expected {dir}",
        meta.is_dir()
    );
    ensure!(
        meta.mode() & 0o7777 == mode && meta.uid() == uid && meta.gid() == gid,
        "{path}: mode {:o} owner {}:{}, expected mode {mode:o} owner {uid}:{gid}",
        meta.mode() & 0o7777,
        meta.uid(),
        meta.gid()
    );
    Ok(())
}

fn to_str(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("non-UTF-8 path {}", path.display()))
}

/// The harness works end to end: host lock provisioned, scratch mounted,
/// diskgc runs as tester under strace, and the oracle sees no change.
fn sandbox_trivial(ctx: &Ctx) -> TestResult {
    // The lock file and directory come from the shipped tmpfiles rule.
    expect_meta("/run/diskgc", true, 0o755, 0, 0)?;
    expect_meta(HOST_LOCK, false, 0o644, 0, 0)?;
    expect_meta("/home/tester", true, 0o755, TESTER.uid, TESTER.gid)?;
    expect_meta(
        "/home/tester/.profile",
        false,
        0o644,
        TESTER.uid,
        TESTER.gid,
    )?;

    // /tmp, /run/diskgc and /home/tester are tmpfs mounts of their own, so
    // the oracle keys every object there by birth time too (on the overlay
    // root it cannot).
    let root_mount = scratch::mount_id(Path::new("/"), false)?;
    let mounts = [&ctx.scratch, &ctx.run_diskgc, &ctx.home];
    for (i, mount) in mounts.iter().enumerate() {
        ensure!(
            mount.mnt_id() != root_mount,
            "{} is on the root mount",
            mount.root().display()
        );
        ensure!(
            mounts[..i].iter().all(|m| m.mnt_id() != mount.mnt_id()),
            "{} shares a mount with another scratch mount",
            mount.root().display()
        );
    }
    ctx.scratch.write_file("hello", b"hello\n", 0o644, TESTER)?;
    let snap = snapshot::take()?;
    for path in [
        "/tmp/hello",
        "/run/diskgc",
        HOST_LOCK,
        "/home/tester",
        "/home/tester/.profile",
    ] {
        let entry = snap
            .entries
            .get(Path::new(path))
            .ok_or_else(|| format!("{path} is not in the snapshot"))?;
        ensure!(
            entry.key.btime.is_some(),
            "{path} is keyed without birth time ({})",
            entry.key
        );
    }

    let oracle = Oracle::arm(Fixture::new())?;
    let run = ctx.run_as_tester(&ctx.diskgc(), &["--version"])?;
    run.expect_code(0)?;
    ensure!(
        run.stdout.starts_with("diskgc "),
        "unexpected diskgc --version output {:?}",
        run.stdout
    );
    oracle.judge(&run)?.assert_clean()
}

/// Parses the whitespace-separated values of a `Name:` line of
/// /proc/<pid>/status as printed by the whoami probe.
fn status_values<'a>(status: &'a str, name: &str) -> Result<Vec<&'a str>, String> {
    status
        .lines()
        .find_map(|l| l.strip_prefix(name))
        .map(|rest| rest.split_whitespace().collect())
        .ok_or_else(|| format!("no {name} line in {status:?}"))
}

/// The code under test runs as uid 1000 without capabilities or root's
/// groups, and cannot read a mode-000 directory of another mapped uid.
fn tester_cannot_read_other_users_mode000_dir(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("other-private", 0o700, OTHER)?;
    s.write_file("other-private/secret", b"secret\n", 0o600, OTHER)?;
    s.chmod("other-private", 0o000)?;
    s.mkdir("tester-own", 0o700, TESTER)?;
    s.write_file("tester-own/mine", b"mine\n", 0o600, TESTER)?;
    expect_meta(
        to_str(&s.path("other-private"))?,
        true,
        0o000,
        OTHER.uid,
        OTHER.gid,
    )?;

    let oracle = Oracle::arm(Fixture::new())?;
    let who = ctx.probe_as_tester(&["whoami"])?;
    who.expect_code(0)?;
    info!("code under test credentials:\n{}", who.stdout);
    ensure!(
        status_values(&who.stdout, "Uid:")? == ["1000"; 4],
        "code under test is not uid 1000 (real, effective, saved, fs): {}",
        who.stdout
    );
    ensure!(
        status_values(&who.stdout, "Gid:")? == ["1000"; 4],
        "code under test is not gid 1000: {}",
        who.stdout
    );
    ensure!(
        status_values(&who.stdout, "Groups:")? == ["1000"],
        "code under test kept supplementary groups other than tester's: {}",
        who.stdout
    );
    for caps in ["CapEff:", "CapPrm:"] {
        ensure!(
            status_values(&who.stdout, caps)? == ["0000000000000000"],
            "code under test has capabilities ({caps}): {}",
            who.stdout
        );
    }

    let other = s.path("other-private");
    let denied = ctx.probe_as_tester(&["read-dir", to_str(&other)?])?;
    denied.expect_code(0)?;
    ensure!(
        denied.stdout.starts_with("err 13 "),
        "tester read {} (mode 000, owner uid {}): {:?}, expected EACCES (13)",
        other.display(),
        OTHER.uid,
        denied.stdout
    );

    // Controls: tester reads its own private dir, and container root (with
    // CAP_DAC_OVERRIDE over mapped uids) reads the other user's dir, so the
    // denial above comes from tester's identity, not from the mount.
    let own = ctx.probe_as_tester(&["read-dir", to_str(&s.path("tester-own"))?])?;
    ensure!(
        own.stdout.trim() == "ok 1",
        "tester cannot read its own dir: {:?}",
        own.stdout
    );
    let as_root = std::fs::read_dir(&other)
        .map_err(|e| format!("root cannot read {}: {e}", other.display()))?
        .count();
    ensure!(
        as_root == 1,
        "root sees {as_root} entries in {}",
        other.display()
    );
    // Reading changes nothing; the oracle judges every traced run.
    oracle.judge_runs(&[&who, &denied, &own])?.assert_clean()
}

/// Positive control of the interlock canary: inside the container it runs.
/// (Its refusal outside a container is checked by the runner on the host
/// before every run.)
fn interlock_canary_passes_in_container(ctx: &Ctx) -> TestResult {
    let canary = ctx.bin_dir.join("diskgc-e2e-canary");
    let out = Command::new(&canary)
        .output()
        .map_err(|e| format!("spawn {}: {e}", canary.display()))?;
    ensure!(
        out.status.code() == Some(exit::PASS),
        "canary exited with {:?} inside the container: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    ensure!(
        String::from_utf8_lossy(&out.stdout).contains("interlock passed"),
        "canary output: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    Ok(())
}

/// Interlock layer 3 bites: a fixture operation aimed outside the scratch
/// mount (absolute path, `..`, a symlink out, a bind mount of the scratch
/// filesystem itself) aborts the process before touching anything.
fn interlock_aborts_outside_scratch(ctx: &Ctx) -> TestResult {
    for case in ESCAPE_CASES {
        let out = ctx.probe_as_root(&["escape", case])?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        info!(
            "escape case {case}: exit {:?}; {}",
            out.status.code(),
            stderr.trim()
        );
        if *case == "control" {
            ensure!(
                out.status.code() == Some(exit::PASS) && stdout.contains("NOT ABORTED"),
                "control case (inside scratch) did not run: {:?} {stdout} {stderr}",
                out.status.code()
            );
        } else {
            ensure!(
                out.status.code() == Some(exit::INTERLOCK) && stderr.contains("INTERLOCK"),
                "escape case {case} was not aborted by the interlock: exit {:?}\n{stdout}{stderr}",
                out.status.code()
            );
        }
    }
    let escaped = Path::new(ESCAPE_TARGET_DIR).join(ESCAPED_NAME);
    ensure!(!escaped.exists(), "{} was created", escaped.display());
    let through_bind = ctx.scratch.path("bind-src").join(ESCAPED_NAME);
    ensure!(
        !through_bind.exists(),
        "{} was created",
        through_bind.display()
    );
    let control = ctx.scratch.path("control").join(ESCAPED_NAME);
    ensure!(
        control.exists(),
        "control file {} missing",
        control.display()
    );
    Ok(())
}

/// Fixture shared by the oracle tests: a candidate tree (eligible), a
/// sibling that is not, and a concurrent writer's directory.
fn candidate_fixture(ctx: &Ctx) -> Result<Fixture, String> {
    let s = &ctx.scratch;
    s.mkdirs("cand/sub", 0o755, TESTER)?;
    s.write_file("cand/a", b"a", 0o644, TESTER)?;
    s.write_file("cand/sub/b", b"b", 0o644, TESTER)?;
    s.mkdir("keep", 0o755, TESTER)?;
    s.write_file("keep/file", b"keep", 0o644, TESTER)?;
    s.mkdir("writer", 0o755, TESTER)?;
    s.write_file("writer/log", b"log", 0o644, TESTER)?;
    Ok(Fixture::new()
        .eligible(s.path("cand"))
        .concurrent_writer(s.path("writer")))
}

/// Control for the two tests below: deleting only eligible and allowlisted
/// objects (and creating new ones) leaves the verdict clean.
fn oracle_clean_when_only_eligible_objects_go(ctx: &Ctx) -> TestResult {
    let fixture = candidate_fixture(ctx)?;
    let oracle = Oracle::arm(fixture)?;
    let s = &ctx.scratch;
    let run = ctx.probe_as_tester(&[
        "remove",
        to_str(&s.path("cand"))?,
        to_str(&s.path("writer/log"))?,
    ])?;
    run.expect_code(0)?;
    let verdict = oracle.judge(&run)?;
    ensure!(
        run.events.iter().filter(|e| e.took_effect()).count() >= 4,
        "strace recorded too few removals: {:?}",
        run.events
    );
    verdict.assert_clean()
}

/// Deliberately wrong fixture: the code under test also deletes an object
/// on the `/tmp` scratch mount, one on the home tmpfs and one on the image's
/// overlay root, none eligible. Both views of the oracle must flag exactly
/// those three (keyed with birth time on the tmpfs mounts, without on the
/// overlay).
fn oracle_catches_deletion_outside_eligible_set(ctx: &Ctx) -> TestResult {
    let fixture = candidate_fixture(ctx)?;
    let s = &ctx.scratch;
    let in_home = ctx.home.path(".profile");
    // Baked into the image, owned by tester, outside every scratch mount.
    let on_overlay = PathBuf::from(TESTER_OVERLAY_FILE);
    let oracle = Oracle::arm(fixture)?;
    let run = ctx.probe_as_tester(&[
        "remove",
        to_str(&s.path("cand"))?,
        to_str(&s.path("keep/file"))?,
        to_str(&s.path("writer/log"))?,
        to_str(&in_home)?,
        to_str(&on_overlay)?,
    ])?;
    run.expect_code(0)?;
    let verdict = oracle.judge(&run)?;

    let expected = [s.path("keep/file"), in_home, on_overlay.clone()]
        .into_iter()
        .collect();
    ensure!(
        verdict.snapshot_paths() == expected,
        "snapshot flagged {:?}, expected {expected:?}\n{}",
        verdict.snapshot_paths(),
        verdict.report()
    );
    ensure!(
        verdict
            .snapshot
            .iter()
            .all(|v| matches!(v.change, Change::Deleted { .. })),
        "expected deletions only:\n{}",
        verdict.report()
    );
    for v in &verdict.snapshot {
        if let Change::Deleted { before } = &v.change {
            let tmpfs = v.path != on_overlay;
            ensure!(
                before.key.btime.is_some() == tmpfs,
                "{} keyed by {}, expected birth time {}",
                v.path.display(),
                before.key.kind(),
                if tmpfs { "(tmpfs)" } else { "absent (overlay)" }
            );
        }
    }
    ensure!(
        verdict.traced_paths() == expected,
        "strace record flagged {:?}, snapshot flagged {expected:?}: they disagree\n{}",
        verdict.traced_paths(),
        verdict.report()
    );
    ensure!(verdict.assert_clean().is_err(), "verdict should fail");
    info!("oracle caught, as intended:\n{}", verdict.report());
    Ok(())
}

/// Deliberately wrong fixture: the code under test replaces two objects it
/// was not allowed to touch, keeping their paths: a `.cargo-lock` unlinked
/// and recreated (what Cargo does), and a file renamed over. Both views must
/// flag both paths as replaced.
fn oracle_catches_replaced_objects(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdirs("proj/target/debug/deps", 0o755, TESTER)?;
    s.write_file("proj/target/debug/deps/libx.rlib", b"rlib", 0o644, TESTER)?;
    s.write_file("proj/target/debug/.cargo-lock", b"", 0o644, TESTER)?;
    s.write_file("proj/Cargo.lock", b"version = 4\n", 0o644, TESTER)?;
    let lock = s.path("proj/target/debug/.cargo-lock");
    let manifest_lock = s.path("proj/Cargo.lock");
    let oracle = Oracle::arm(Fixture::new().eligible(s.path("proj/target/debug/deps")))?;

    let recreate = ctx.probe_as_tester(&["recreate", to_str(&lock)?])?;
    recreate.expect_code(0)?;
    let rename = ctx.probe_as_tester(&["rename-over", to_str(&manifest_lock)?])?;
    rename.expect_code(0)?;
    let verdict = oracle.judge_runs(&[&recreate, &rename])?;

    let expected = [lock, manifest_lock].into_iter().collect();
    ensure!(
        verdict.snapshot_paths() == expected,
        "snapshot flagged {:?}, expected {expected:?}\n{}",
        verdict.snapshot_paths(),
        verdict.report()
    );
    ensure!(
        verdict
            .snapshot
            .iter()
            .all(|v| matches!(v.change, Change::Replaced { .. })),
        "expected replacements only:\n{}",
        verdict.report()
    );
    ensure!(
        verdict.traced_paths() == expected,
        "strace record flagged {:?}, snapshot flagged {expected:?}: they disagree\n{}",
        verdict.traced_paths(),
        verdict.report()
    );
    info!("oracle caught, as intended:\n{}", verdict.report());
    Ok(())
}

/// Bite proof of the mandatory oracle (expected to FAIL with
/// [`UNJUDGED_MESSAGE`]): diskgc runs traced and its run is never judged.
/// On the way it checks that an oracle armed after a run refuses to judge
/// it, so arming late cannot launder a run either.
fn oracle_mandatory_unjudged_run_fails_the_test(ctx: &Ctx) -> TestResult {
    let run = ctx.run_as_tester(&ctx.diskgc(), &["--version"])?;
    run.expect_code(0)?;
    let late = Oracle::arm(Fixture::new())?;
    match late.judge(&run) {
        Err(why) if why.contains("started before the oracle was armed") => {
            info!("an oracle armed after the run refused it, as it must: {why}");
        }
        Err(why) => return Err(format!("late oracle failed for another reason: {why}")),
        Ok(_) => return Err("an oracle armed after the run judged it".to_owned()),
    }
    // The run stays unjudged: Ctx::teardown must fail this test (the body
    // itself passes, so its own checks above count).
    Ok(())
}

/// Traced removals whose paths are not the snapshot's paths are logged with
/// a reason, never dropped: one through a symlinked prefix, one with `..`
/// (both eligible), and one through a symlink into a directory that is not
/// eligible. The last is invisible to the strace view by path, and the
/// snapshot view still catches it.
fn oracle_logs_unmatched_strace_removals(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("real", 0o755, TESTER)?;
    for name in ["a", "b", "c"] {
        s.write_file(&format!("real/{name}"), name.as_bytes(), 0o644, TESTER)?;
    }
    s.mkdir("keep", 0o755, TESTER)?;
    s.write_file("keep/x", b"x", 0o644, TESTER)?;
    s.symlink("link", &s.path("real"))?;
    s.symlink("keep-link", &s.path("keep"))?;
    let via_link = s.path("link").join("a");
    let via_dotdot = s.path("real").join("../real/b");
    let via_keep_link = s.path("keep-link").join("x");

    let oracle = Oracle::arm(Fixture::new().eligible(s.path("real")))?;
    let run = ctx.probe_as_tester(&[
        "remove",
        to_str(&via_link)?,
        to_str(&via_dotdot)?,
        to_str(&via_keep_link)?,
    ])?;
    run.expect_code(0)?;
    let verdict = oracle.judge(&run)?;

    let got: Vec<(PathBuf, Unmatched)> = verdict
        .unmatched
        .iter()
        .map(|u| (u.path.clone(), u.why.clone()))
        .collect();
    let want = vec![
        (via_link, Unmatched::SymlinkPrefix(s.path("link"))),
        (via_dotdot, Unmatched::NonCanonical),
        (via_keep_link, Unmatched::SymlinkPrefix(s.path("keep-link"))),
    ];
    ensure!(
        got == want,
        "unmatched strace removals {got:?}, expected {want:?}"
    );
    ensure!(
        verdict.traced.is_empty(),
        "the strace view cannot match these paths, yet flagged:\n{}",
        verdict.report()
    );
    let expected = [s.path("keep/x")].into_iter().collect();
    ensure!(
        verdict.snapshot_paths() == expected,
        "snapshot flagged {:?}, expected {expected:?}\n{}",
        verdict.snapshot_paths(),
        verdict.report()
    );
    info!(
        "oracle caught the deletion through a symlink by snapshot only, as intended:\n{}",
        verdict.report()
    );
    Ok(())
}

/// Read-only mounts are not walked by the snapshot (the binaries under
/// test, what the image's descriptor mounts read-only, e.g. the Nix
/// image's store with the Cargo toolchains under test, a read-only bind
/// mount in scratch), objects stay visible through a
/// writable path to the same filesystem, and a writable mount nested below
/// a read-only one is still walked. The image's Cargo toolchains lie below
/// what its descriptor mounts read-only.
fn snapshot_skips_read_only_mounts(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdirs("ro-src/nested", 0o755, ROOT)?;
    s.write_file("ro-src/file", b"file", 0o644, ROOT)?;
    s.mkdir("ro-dst", 0o755, ROOT)?;
    s.bind_mount_read_only("ro-src", "ro-dst")?;
    let nested_root = s.path("ro-dst/nested");
    let nested = Scratch::mount(to_str(&nested_root)?, "0755", ROOT)?;
    nested.write_file("inside", b"inside", 0o644, ROOT)?;

    let snap = snapshot::take()?;
    info!(
        "{} objects in {:?}; mounts not walked: {:?}",
        snap.entries.len(),
        snap.took,
        snap.skipped
    );
    let mut read_only = vec![PathBuf::from(UNDER_TEST_BIN_DIR), s.path("ro-dst")];
    read_only.extend(ctx.image.read_only.iter().cloned());
    for ro in &read_only {
        ensure!(
            snap.skipped.get(ro) == Some(&Skip::ReadOnly),
            "{} not skipped as read-only: {:?}",
            ro.display(),
            snap.skipped.get(ro)
        );
        let below: Vec<_> = snap
            .entries
            .keys()
            .filter(|p| p.starts_with(ro) && !p.starts_with(nested.root()))
            .collect();
        ensure!(
            below.is_empty(),
            "the snapshot walked the read-only mount {}: {below:?}",
            ro.display()
        );
    }
    // The image's Cargo toolchains (tens of thousands of files) lie below
    // what its descriptor mounts read-only, so no snapshot walks them.
    if ctx.image.toolchains == image::Toolchains::Manifest {
        for toolchain in Toolchain::all()? {
            for dir in [&toolchain.bin_dir, &toolchain.cc_bin_dir] {
                let real = std::fs::canonicalize(dir)
                    .map_err(|e| format!("canonicalize {}: {e}", dir.display()))?;
                ensure!(
                    ctx.image.read_only.iter().any(|ro| real.starts_with(ro)),
                    "{} ({}) is not below a read-only mount of the image's descriptor {:?}",
                    dir.display(),
                    real.display(),
                    ctx.image.read_only
                );
            }
        }
    }
    for walked in [s.path("ro-src/file"), nested.path("inside")] {
        ensure!(
            snap.entries.contains_key(&walked),
            "{} missing from the snapshot",
            walked.display()
        );
    }
    Ok(())
}

/// Files the strace overhead test creates, stats and deletes.
const CHURN_FILES: usize = 20_000;

/// lstat calls per churned file: like a build, the workload makes far more
/// untraced syscalls than traced ones (about 14 per file, one traced).
const CHURN_STATS_PER_FILE: usize = 10;

/// The stated wall-time bound for the traced churn (docs/testing.md,
/// Container setup). A budget guard: wall time on a loaded machine varies
/// several-fold, so it is generous; [`CHURN_MAX_SWITCHES_PER_FILE`] is the
/// load-independent check that strace stops only at traced syscalls.
const CHURN_BOUND: Duration = Duration::from_secs(10);

/// Upper bound on the tracee's voluntary context switches per churned file.
/// Each ptrace stop is one; with `--seccomp-bpf` only the traced unlink
/// stops (seccomp stop plus syscall-exit stop), without it every one of the
/// ~14 syscalls per file does, twice.
const CHURN_MAX_SWITCHES_PER_FILE: usize = 4;

/// strace stops the tracee only at the traced syscalls (`--seccomp-bpf`):
/// the tracee carries one seccomp filter more than the harness, its
/// voluntary context switches (one per ptrace stop) stay under
/// [`CHURN_MAX_SWITCHES_PER_FILE`] per file, a run of [`CHURN_FILES`] files
/// with [`CHURN_STATS_PER_FILE`] stats each stays within [`CHURN_BOUND`],
/// and the record still holds every unlink. The same churn untraced, as
/// root, is logged as the baseline.
fn strace_seccomp_bpf_churn_within_bound(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    let count = CHURN_FILES.to_string();
    let stats = CHURN_STATS_PER_FILE.to_string();
    s.mkdir("baseline", 0o755, ROOT)?;
    let baseline_started = Instant::now();
    let baseline = ctx.probe_as_root(&["churn", to_str(&s.path("baseline"))?, &count, &stats])?;
    let baseline_took = baseline_started.elapsed();
    ensure!(
        baseline.status.success(),
        "untraced churn failed: {}",
        String::from_utf8_lossy(&baseline.stderr)
    );
    let baseline_switches = status_number(
        &String::from_utf8_lossy(&baseline.stdout),
        "voluntary_ctxt_switches:",
    )?;

    s.mkdir("churn", 0o755, TESTER)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let run = ctx.probe_as_tester(&["churn", to_str(&s.path("churn"))?, &count, &stats])?;
    run.expect_code(0)?;
    let verdict = oracle.judge(&run)?;
    let switches = status_number(&run.stdout, "voluntary_ctxt_switches:")?;
    let max_switches = CHURN_FILES * CHURN_MAX_SWITCHES_PER_FILE;
    info!(
        "churn of {CHURN_FILES} files, {CHURN_STATS_PER_FILE} stats each: traced {:.2?} \
         (bound {CHURN_BOUND:?}), untraced {baseline_took:.2?}; tracee voluntary context \
         switches {switches} (bound {max_switches}), untraced {baseline_switches}",
        run.took
    );

    let own_filters = status_number(
        &std::fs::read_to_string("/proc/self/status")
            .map_err(|e| format!("read /proc/self/status: {e}"))?,
        "Seccomp_filters:",
    )?;
    let tracee_filters = status_number(&run.stdout, "Seccomp_filters:")?;
    ensure!(
        tracee_filters == own_filters + 1,
        "the tracee has {tracee_filters} seccomp filters, the harness {own_filters}: \
         strace's --seccomp-bpf filter is not installed"
    );
    ensure!(
        switches <= max_switches,
        "the tracee was stopped about {switches} times for {CHURN_FILES} traced unlinks \
         (bound {max_switches}): strace stops it at untraced syscalls"
    );
    let unlinks = run
        .events
        .iter()
        .filter(|e| e.took_effect() && e.syscall.starts_with("unlink"))
        .count();
    ensure!(
        unlinks == CHURN_FILES,
        "the strace record holds {unlinks} unlinks, the tracee made {CHURN_FILES}"
    );
    ensure!(
        run.took <= CHURN_BOUND,
        "traced churn of {CHURN_FILES} files took {:.2?}, over the bound {CHURN_BOUND:?}",
        run.took
    );
    let created = verdict
        .unmatched
        .iter()
        .filter(|u| u.why == Unmatched::CreatedDuringRun)
        .count();
    ensure!(
        created == CHURN_FILES && verdict.unmatched.len() == CHURN_FILES,
        "expected {CHURN_FILES} unmatched removals, all created during the run: {created} of {}",
        verdict.unmatched.len()
    );
    verdict.assert_clean()
}

/// The number after `name` on its line of a /proc/<pid>/status text.
fn status_number(status: &str, name: &str) -> Result<usize, String> {
    status
        .lines()
        .find_map(|l| l.strip_prefix(name))
        .and_then(|v| v.trim().parse().ok())
        .ok_or_else(|| format!("no {name} count in {status:?}"))
}

/// The oracle at the scale of the 100,000-entry `/tmp` scenarios: it stays
/// clean when half of a large tree (all eligible) is deleted, and reports
/// what the snapshots cost.
fn oracle_large_tree(ctx: &Ctx) -> TestResult {
    const DIRS: usize = 100;
    const FILES_PER_DIR: usize = 1000;
    let s = &ctx.scratch;
    let built = Instant::now();
    let mut fixture = Fixture::new();
    let mut eligible = Vec::new();
    s.mkdir("big", 0o755, TESTER)?;
    for d in 0..DIRS {
        let dir = format!("big/d{d:03}");
        s.mkdir(&dir, 0o755, TESTER)?;
        for f in 0..FILES_PER_DIR {
            s.write_file(&format!("{dir}/f{f:04}"), b"", 0o644, TESTER)?;
        }
        if d % 2 == 0 {
            fixture = fixture.eligible(s.path(&dir));
            eligible.push(s.path(&dir).display().to_string());
        }
    }
    info!(
        "built {} files in {:?}",
        DIRS * FILES_PER_DIR,
        built.elapsed()
    );
    let oracle = Oracle::arm(fixture)?;
    let mut args = vec!["remove"];
    args.extend(eligible.iter().map(String::as_str));
    let run = ctx.probe_as_tester(&args)?;
    run.expect_code(0)?;
    let removed = run.events.iter().filter(|e| e.took_effect()).count();
    ensure!(
        removed >= DIRS / 2 * (FILES_PER_DIR + 1),
        "strace recorded {removed} removals"
    );
    oracle.judge(&run)?.assert_clean()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name on the Nix-only list that no test has would protect nothing;
    /// an entry without a reason explains nothing.
    #[test]
    fn every_nix_only_test_exists_and_says_why() {
        for (name, why) in NIX_ONLY_TESTS {
            assert!(
                TESTS.iter().any(|t| t.name == *name)
                    || cargo_suite::CARGO_TESTS.iter().any(|t| t.name == *name),
                "{name} is in NIX_ONLY_TESTS but is no test"
            );
            assert!(why.len() > 20, "{name}: say why it is Nix-only");
        }
    }

    #[test]
    fn expectations_pass_and_fail_as_declared() {
        let ok = || Ok(());
        let err = |s: &str| Err(s.to_owned());
        assert_eq!(
            judge_expectation(Expect::Pass, ok(), ok()),
            Ok(String::new())
        );
        assert_eq!(
            judge_expectation(Expect::Pass, err("boom"), ok()),
            Err("boom".into())
        );
        assert!(
            judge_expectation(Expect::Pass, ok(), err("1 run never judged"))
                .unwrap_err()
                .contains("never judged")
        );
        let both = judge_expectation(Expect::Pass, err("boom"), err("never judged")).unwrap_err();
        assert!(
            both.contains("boom") && both.contains("never judged"),
            "{both}"
        );

        let bite = Expect::FailWith("never judged");
        assert!(judge_expectation(bite, ok(), err("2 runs never judged by it")).is_ok());
        // The body's own checks are not masked by the rule's message.
        let masked =
            judge_expectation(bite, err("late oracle judged the run"), err("never judged"))
                .unwrap_err();
        assert!(masked.contains("its body failed"), "{masked}");
        assert!(masked.contains("late oracle judged the run"), "{masked}");
        assert!(judge_expectation(bite, err("boom"), ok()).is_err());
        assert!(
            judge_expectation(bite, ok(), err("snapshot incomplete"))
                .unwrap_err()
                .contains("they failed with")
        );
        assert!(
            judge_expectation(bite, ok(), ok())
                .unwrap_err()
                .contains("does not bite")
        );
    }

    #[test]
    fn a_broken_toolchain_is_reported_as_such() {
        let took = Duration::from_millis(5);
        let broken = Some("Cargo toolchain under test 1.90.0 failed its smoke check".to_owned());
        let (line, code) = result_line("cargo_smoke@1.90.0", took, Err("x".into()), broken.clone());
        assert_eq!(code, exit::TOOLCHAIN);
        assert!(line.starts_with("TOOLCHAIN cargo_smoke@1.90.0"), "{line}");
        assert!(line.contains("failed its smoke check"), "{line}");
        let (line, code) = result_line("t", took, Err("boom".into()), None);
        assert_eq!(code, exit::FAIL);
        assert!(
            line.starts_with("FAIL t") && line.contains("boom"),
            "{line}"
        );
        // A recorded failure never turns a pass into anything else.
        let (line, code) = result_line("t", took, Ok(String::new()), broken);
        assert_eq!(code, exit::PASS);
        assert!(line.starts_with("PASS t"), "{line}");
    }

    #[test]
    fn test_names_are_unique() {
        let mut names: Vec<_> = TESTS.iter().map(|t| t.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), TESTS.len());
    }
}
