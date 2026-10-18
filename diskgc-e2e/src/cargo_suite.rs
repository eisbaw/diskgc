//! Container tests of the Cargo toolchains under test and of the helpers
//! the Cargo race tests use (task-4; docs/testing.md, Cargo toolchains
//! under test). Each [`CargoTest`] runs once per toolchain in the image's
//! manifest, listed as `NAME@TOOLCHAIN`; the suite runs the toolchain's
//! smoke check before its body, so a broken toolchain is reported as such
//! (`TOOLCHAIN`), not as a failure of the test.
//!
//! The helpers' pauses are checked with `/proc/locks` ([`crate::proc_locks`]),
//! independent of anything Cargo or the helpers print.

use std::path::{Path, PathBuf};
use std::time::Duration;

use diskgc_core::engine::CARGO_LOCK_NAMES;

use crate::cargo_probe::{self, Rusage};
use crate::cargo_race_suite;
use crate::cargo_reclaim_suite;
use crate::cargo_target_suite;
use crate::cargo_under_test::{
    self, Cargo, Gate, RACE, REACH_TIMEOUT, SMOKE, Toolchain, run_as_tester_untraced,
};
use crate::ensure_suite;
use crate::holder::Holder;
use crate::oracle::{Fixture, Oracle};
use crate::proc_locks::{self, Watched};
use crate::suite::{Ctx, TestResult, Tier};
use crate::{TESTER, ensure, info};

/// A test run once per toolchain under test, in that toolchain's tier.
pub struct CargoTest {
    pub name: &'static str,
    pub run: fn(&Ctx, &Toolchain) -> TestResult,
}

/// The registry of per-toolchain tests. Names are stable: `just e2e-one
/// NAME@TOOLCHAIN` uses them.
pub static CARGO_TESTS: &[CargoTest] = &[
    CargoTest {
        name: "cargo_smoke",
        run: cargo_smoke,
    },
    CargoTest {
        name: "cargo_traced_build_within_bound",
        run: cargo_traced_build_within_bound,
    },
    CargoTest {
        name: "cargo_fixtures_build_without_network",
        run: cargo_fixtures_build_without_network,
    },
    CargoTest {
        name: "cargo_lock_holder_blocks_a_build_on_cargo_lock",
        run: cargo_lock_holder_blocks_a_build_on_cargo_lock,
    },
    CargoTest {
        name: "cargo_build_script_gate_pauses_a_build_holding_its_locks",
        run: cargo_build_script_gate_pauses_a_build_holding_its_locks,
    },
    CargoTest {
        name: "cargo_runner_gate_pauses_between_compile_and_run_holding_no_lock",
        run: cargo_runner_gate_pauses_between_compile_and_run_holding_no_lock,
    },
    CargoTest {
        name: "cargo_targets_discovered_and_judged",
        run: cargo_target_suite::cargo_targets_discovered_and_judged,
    },
    CargoTest {
        name: "cargo_split_build_dir_halves_are_unsupported_layout",
        run: cargo_target_suite::cargo_split_build_dir_halves_are_unsupported_layout,
    },
    CargoTest {
        name: "cargo_running_cargo_keeps_its_target_in_use",
        run: cargo_target_suite::cargo_running_cargo_keeps_its_target_in_use,
    },
    CargoTest {
        name: "cargo_running_executable_below_a_non_utf8_name_keeps_its_target_in_use",
        run: cargo_target_suite::cargo_running_executable_below_a_non_utf8_name_keeps_its_target_in_use,
    },
    CargoTest {
        name: "cargo_detached_target_is_in_use_while_any_cargo_runs",
        run: cargo_target_suite::cargo_detached_target_is_in_use_while_any_cargo_runs,
    },
    CargoTest {
        name: "cargo_reclaim_deletes_only_inside_locked_profile_dirs",
        run: cargo_reclaim_suite::cargo_reclaim_deletes_only_inside_locked_profile_dirs,
    },
    CargoTest {
        name: "cargo_reclaim_skips_a_target_whose_lock_is_held",
        run: cargo_reclaim_suite::cargo_reclaim_skips_a_target_whose_lock_is_held,
    },
    CargoTest {
        name: "cargo_reclaim_changed_between_discovery_and_locking",
        run: cargo_reclaim_suite::cargo_reclaim_changed_between_discovery_and_locking,
    },
    CargoTest {
        name: "cargo_race_2_a_build_started_during_deletion_waits_writing_nothing",
        run: cargo_race_suite::cargo_race_2_a_build_started_during_deletion_waits_writing_nothing,
    },
    CargoTest {
        name: "cargo_race_2_split_build_dir_on_a_target_first_built_whole",
        run: cargo_race_suite::cargo_race_2_split_build_dir_on_a_target_first_built_whole,
    },
    CargoTest {
        name: "cargo_race_2_a_cargo_clean_during_deletion",
        run: cargo_race_suite::cargo_race_2_a_cargo_clean_during_deletion,
    },
    CargoTest {
        name: "cargo_race_3_a_blocked_build_and_a_later_one_never_run_together",
        run: cargo_race_suite::cargo_race_3_a_blocked_build_and_a_later_one_never_run_together,
    },
    CargoTest {
        name: "cargo_race_locks_held_through_the_last_part_released_per_target",
        run: cargo_race_suite::cargo_race_locks_held_through_the_last_part_released_per_target,
    },
    CargoTest {
        name: "cargo_race_4_a_target_whose_test_binary_still_runs_is_skipped",
        run: cargo_race_suite::cargo_race_4_a_target_whose_test_binary_still_runs_is_skipped,
    },
    CargoTest {
        name: "cargo_race_6_killed_at_seeded_positions_the_next_build_succeeds",
        run: cargo_race_suite::cargo_race_6_killed_at_seeded_positions_the_next_build_succeeds,
    },
    CargoTest {
        name: "cargo_race_7_a_check_between_its_locks_and_a_build_keep_the_target",
        run: cargo_race_suite::cargo_race_7_a_check_between_its_locks_and_a_build_keep_the_target,
    },
    CargoTest {
        name: "cargo_race_8_run_or_test_paused_before_its_binary_keeps_the_target",
        run: cargo_race_suite::cargo_race_8_run_or_test_paused_before_its_binary_keeps_the_target,
    },
    CargoTest {
        name: "cargo_race_9_an_unknown_lock_file_is_unsupported_layout",
        run: cargo_race_suite::cargo_race_9_an_unknown_lock_file_is_unsupported_layout,
    },
    CargoTest {
        name: "cargo_race_split_build_dir_is_never_reclaimed",
        run: cargo_race_suite::cargo_race_split_build_dir_is_never_reclaimed,
    },
    CargoTest {
        name: "cargo_race_shared_target_dir_from_cargo_config",
        run: cargo_race_suite::cargo_race_shared_target_dir_from_cargo_config,
    },
    CargoTest {
        name: "ensure_reclaims_the_oldest_real_target_and_stops_at_min_free",
        run: ensure_suite::ensure_reclaims_the_oldest_real_target_and_stops_at_min_free,
    },
    CargoTest {
        name: "ensure_exits_1_when_every_candidate_was_processed_below_min_free",
        run: ensure_suite::ensure_exits_1_when_every_candidate_was_processed_below_min_free,
    },
    CargoTest {
        name: "ensure_dry_run_plans_real_targets_and_deletes_nothing",
        run: ensure_suite::ensure_dry_run_plans_real_targets_and_deletes_nothing,
    },
];

/// Separates a Cargo test's name from its toolchain's in a listed name.
pub const TOOLCHAIN_SEPARATOR: char = '@';

/// `NAME@TOOLCHAIN` and its tier (the toolchain's), for every Cargo test
/// and toolchain.
pub fn listing(toolchains: &[Toolchain]) -> Vec<(String, Tier)> {
    let mut listed = Vec::new();
    for test in CARGO_TESTS {
        for toolchain in toolchains {
            listed.push((
                format!("{}{TOOLCHAIN_SEPARATOR}{}", test.name, toolchain.name),
                toolchain.tier,
            ));
        }
    }
    listed
}

/// The Cargo test and toolchain a listed name stands for, if it names one.
pub fn find(listed: &str) -> Option<Result<(&'static CargoTest, Toolchain), String>> {
    let (name, toolchain) = listed.split_once(TOOLCHAIN_SEPARATOR)?;
    let test = CARGO_TESTS.iter().find(|t| t.name == name)?;
    Some(Toolchain::named(toolchain).map(|tc| (test, tc)))
}

/// The smoke check on its own: the suite has already run it before this
/// body (see [`cargo_under_test::smoke`]), so reaching here means the
/// toolchain builds, links and runs the smoke fixture offline.
fn cargo_smoke(_ctx: &Ctx, toolchain: &Toolchain) -> TestResult {
    info!(
        "toolchain {} ({}) passed its smoke check",
        toolchain.name, toolchain.version
    );
    Ok(())
}

/// Bite proof of the toolchain failure category: the smoke check reports a
/// toolchain whose `cargo` is missing, and one without its C toolchain (no
/// linker), as broken at the step that failed; the real toolchain passes.
/// (That the suite then reports `TOOLCHAIN` instead of `FAIL` is
/// `suite::tests::a_broken_toolchain_is_reported_as_such`.)
pub fn cargo_smoke_reports_a_broken_toolchain_as_a_toolchain_failure(ctx: &Ctx) -> TestResult {
    let real = Toolchain::fast()?;
    let no_cargo = Toolchain {
        bin_dir: PathBuf::from("/opt/cargo-under-test/no-such-toolchain/bin"),
        ..real.clone()
    };
    let no_linker = Toolchain {
        cc_bin_dir: PathBuf::from("/opt/cargo-under-test/no-such-cc/bin"),
        ..real.clone()
    };
    for (broken, step, needle) in [
        (&no_cargo, "cargo --version", "exited with"),
        (&no_linker, "cargo build", "cc"),
    ] {
        match cargo_under_test::smoke(ctx, broken) {
            Ok(_) => {
                return Err(format!(
                    "the smoke check passed a broken toolchain: {broken:?}"
                ));
            }
            Err(why) => {
                info!("broken toolchain reported as: {why}");
                ensure!(
                    why.step == step && why.detail.contains(needle),
                    "expected a failure at {step:?} mentioning {needle:?}, got: {why}"
                );
            }
        }
    }
    cargo_under_test::smoke(ctx, &real)
        .map(|_| ())
        .map_err(|why| format!("control: the real toolchain failed: {why}"))
}

/// The traced Cargo build the harness's strace must afford (task-59 AC#2;
/// docs/testing.md, Cargo toolchains under test). Three builds of the smoke
/// fixture, each in its own directory with its own `CARGO_HOME`: untraced
/// (the baseline, logged), under the harness's strace without
/// `--seccomp-bpf` (the full trace: two ptrace stops per syscall of every
/// task), and as code under test (the harness's strace). Bounds:
///
/// - the traced build's voluntary context switches (cargo, rustc and the
///   linker together; one per ptrace stop) are at most
///   [`FULL_TRACE_PERCENT`] % of the full trace's. Not a per-traced-call
///   bound as in `strace_seccomp_bpf_churn_within_bound`: strace 7.2
///   restarts every new process and thread with `PTRACE_SYSCALL` until its
///   first traced syscall ("We don't know if forks/clones have a seccomp
///   filter when they are created", strace.c), so the tasks of a build
///   that never unlink are stopped at every syscall; about a third of the
///   full trace's stops remain (measurements in docs/testing.md);
/// - the tracee carries one seccomp filter more than the harness;
/// - wall time stays within [`TRACED_BUILD_BOUND`] (a hang guard only:
///   wall time on a loaded machine varies several-fold);
/// - the deletion oracle finds nothing touched that existed before.
fn cargo_traced_build_within_bound(ctx: &Ctx, toolchain: &Toolchain) -> TestResult {
    let s = &ctx.scratch;
    let rusage_args: Vec<String> = vec![
        "probe".into(),
        "rusage".into(),
        "--".into(),
        toolchain.cargo().display().to_string(),
        "build".into(),
    ];
    let own_home = |rel: &str| -> Result<Vec<(String, String)>, String> {
        let home = s.mkdir(rel, 0o755, TESTER)?;
        let mut env = toolchain.env();
        env.push(("CARGO_HOME".into(), home.display().to_string()));
        Ok(env)
    };

    let baseline_dir = SMOKE.write(s, "untraced")?;
    let baseline = run_as_tester_untraced(
        ctx,
        &ctx.self_exe,
        &rusage_args,
        &own_home("untraced-cargo-home")?,
        &baseline_dir,
    )?;
    baseline.expect_success()?;
    let base = cargo_probe::parse_rusage(&baseline.stderr)?;

    let full_dir = SMOKE.write(s, "full-trace")?;
    let full = full_trace(
        ctx,
        &full_dir,
        &own_home("full-trace-cargo-home")?,
        &rusage_args,
    )?;

    let traced_dir = SMOKE.write(s, "traced")?;
    let oracle = Oracle::arm(Fixture::new())?;
    let run = crate::under_test::run_as_tester_with_env(
        &ctx.ledger,
        &ctx.records,
        &traced_dir,
        &toolchain.env(),
        &ctx.self_exe,
        &rusage_args,
    )?;
    run.expect_code(0)?;
    let verdict = oracle.judge(&run)?;
    let traced = cargo_probe::parse_rusage(&run.stderr)?;
    let bound = switch_bound(full);
    info!(
        "toolchain {}: traced build {:.2?} (bound {TRACED_BUILD_BOUND:?}); voluntary context \
         switches: traced {} (bound {bound}, {FULL_TRACE_PERCENT}% of the full trace's {}), \
         untraced {}; {} traced calls in the record, from {} processes",
        toolchain.name,
        run.took,
        traced.voluntary_switches,
        full.voluntary_switches,
        base.voluntary_switches,
        run.events.len(),
        run.events
            .iter()
            .map(|e| e.pid)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    );

    ensure!(
        traced.voluntary_switches <= bound,
        "the traced build was stopped about {} times, over the bound {bound} \
         ({FULL_TRACE_PERCENT}% of the {} of a full trace): strace stops it at untraced \
         syscalls",
        traced.voluntary_switches,
        full.voluntary_switches
    );
    let own_filters = own_seccomp_filters()?;
    ensure!(
        traced.seccomp_filters == own_filters + 1,
        "the traced build has {} seccomp filters, the harness {own_filters}: strace's \
         --seccomp-bpf filter is not installed",
        traced.seccomp_filters
    );
    ensure!(
        run.took <= TRACED_BUILD_BOUND,
        "the traced build took {:.2?}, over the bound {TRACED_BUILD_BOUND:?}",
        run.took
    );
    ensure!(
        SMOKE.debug_binary(&traced_dir).exists(),
        "the traced build produced no binary"
    );
    verdict.assert_clean()
}

/// Wall-time bound of the traced smoke build (docs/testing.md): a hang
/// guard, far above the second or so it takes.
const TRACED_BUILD_BOUND: Duration = Duration::from_secs(120);

/// The traced build may cost at most this share of a full trace's ptrace
/// stops (without `--seccomp-bpf` it is the full trace, 100 %; measured
/// shares are in docs/testing.md).
const FULL_TRACE_PERCENT: u64 = 60;

fn switch_bound(full: Rusage) -> u64 {
    full.voluntary_switches * FULL_TRACE_PERCENT / 100
}

/// The harness's strace without `--seccomp-bpf` (the calibration of
/// [`cargo_traced_build_within_bound`]): a build of a fixture, not code
/// under test, so it is not entered in the ledger. Its record goes to the
/// records mount and is not read.
fn full_trace(
    ctx: &Ctx,
    cwd: &Path,
    env: &[(String, String)],
    args: &[String],
) -> Result<Rusage, String> {
    let record = ctx.records.mkdir("full-trace", 0o700, crate::ROOT)?;
    let options: Vec<&str> = crate::under_test::STRACE_OPTIONS
        .iter()
        .copied()
        .filter(|o| *o != "--seccomp-bpf")
        .collect();
    let out = std::process::Command::new(crate::under_test::STRACE)
        .args(&options)
        .arg("-e")
        .arg(format!("trace={}", crate::strace::TRACED_SYSCALLS))
        .arg("-o")
        .arg(record.join("trace"))
        .arg("--")
        .arg(&ctx.self_exe)
        .args(["as-tester", "--"])
        .arg(&ctx.self_exe)
        .args(args)
        .env_clear()
        .envs(crate::under_test::TESTER_ENV.iter().copied())
        .envs(env.iter().map(|(k, v)| (k, v)))
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("spawn the full-trace build: {e}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        return Err(format!(
            "the full-trace build failed ({}):\n{stderr}",
            out.status
        ));
    }
    cargo_probe::parse_rusage(&stderr)
}

fn own_seccomp_filters() -> Result<u64, String> {
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|e| format!("read /proc/self/status: {e}"))?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("Seccomp_filters:"))
        .and_then(|v| v.trim().parse().ok())
        .ok_or_else(|| "no Seccomp_filters in /proc/self/status".to_owned())
}

/// The fixtures need no network: the container has none (only `lo`), every
/// fixture builds offline from an empty `CARGO_HOME`, and a control project
/// with a registry dependency fails as Cargo fails in offline mode (so
/// offline mode is really on: without it Cargo would try the network and
/// fail differently).
fn cargo_fixtures_build_without_network(ctx: &Ctx, toolchain: &Toolchain) -> TestResult {
    let mut interfaces: Vec<String> = std::fs::read_dir("/sys/class/net")
        .map_err(|e| format!("read_dir /sys/class/net: {e}"))?
        .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("read_dir /sys/class/net: {e}"))?;
    interfaces.sort();
    ensure!(
        interfaces == ["lo"],
        "the container has network interfaces {interfaces:?}; it must run with --network=none"
    );

    for project in [SMOKE, RACE] {
        let dir = project.write(&ctx.scratch, project.name)?;
        Cargo::new(toolchain, &dir, &["build"])
            .output(ctx)?
            .expect_success()?;
        ensure!(
            project.debug_binary(&dir).exists(),
            "{}: no binary after the build",
            project.name
        );
    }

    let s = &ctx.scratch;
    s.mkdirs("needs-registry/src", 0o755, TESTER)?;
    s.write_file(
        "needs-registry/Cargo.toml",
        b"[package]\nname = \"needs-registry\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
          [dependencies]\nserde = \"1\"\n\n[workspace]\n",
        0o644,
        TESTER,
    )?;
    s.write_file(
        "needs-registry/src/main.rs",
        b"fn main() {}\n",
        0o644,
        TESTER,
    )?;
    let out = Cargo::new(toolchain, &s.path("needs-registry"), &["build"]).output(ctx)?;
    info!("registry control: {}", out.stderr.trim());
    ensure!(
        out.code != Some(0),
        "a project with a registry dependency built offline:\n{}",
        out.failure()
    );
    let attempted = [
        "Updating crates.io index",
        "Could not resolve",
        "spurious network error",
    ]
    .into_iter()
    .filter(|needle| out.stderr.contains(needle))
    .collect::<Vec<_>>();
    ensure!(
        attempted.is_empty(),
        "the registry control tried the network ({attempted:?}): is CARGO_NET_OFFLINE set?\n{}",
        out.failure()
    );
    ensure!(
        out.stderr.contains("offline mode"),
        "the registry control did not fail as offline mode does (is CARGO_NET_OFFLINE set?):\n{}",
        out.failure()
    );
    Ok(())
}

/// Builds the race fixture once (so the target, its profile directory and
/// lock files exist) and returns its directory.
fn built_race_project(ctx: &Ctx, toolchain: &Toolchain, rel: &str) -> Result<PathBuf, String> {
    let project = RACE.write(&ctx.scratch, rel)?;
    Cargo::new(toolchain, &project, &["build"])
        .output(ctx)?
        .expect_success()?;
    Ok(project)
}

/// Lock files of `project`'s debug profile as watched files.
fn watch_lock_files(project: &Path) -> Result<(Vec<PathBuf>, Watched), String> {
    let files = cargo_under_test::lock_files(&project.join("target/debug"))?;
    let watched = Watched::files(&files)?;
    Ok((files, watched))
}

fn names(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .filter_map(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .collect()
}

/// Fails if the profile directory holds a lock file the product's
/// allowlist does not know (diskgc would skip such a target as
/// `unsupported_layout`): an early warning of a new Cargo lock.
fn require_known_lock_names(toolchain: &Toolchain, files: &[PathBuf]) -> TestResult {
    let unknown: Vec<String> = names(files)
        .into_iter()
        .filter(|n| !CARGO_LOCK_NAMES.contains(&n.as_str()))
        .collect();
    ensure!(
        unknown.is_empty(),
        "toolchain {} made lock files the allowlist does not know: {unknown:?} (known: \
         {CARGO_LOCK_NAMES:?})",
        toolchain.name
    );
    Ok(())
}

/// No process holds or waits for a lock on the watched files.
fn require_no_locks(watched: &Watched, when: &str) -> TestResult {
    let locks = proc_locks::sample()?;
    let left = watched.locks_on(&locks);
    ensure!(
        left.is_empty(),
        "{when}: locks remain on the lock files: {left:?}"
    );
    Ok(())
}

/// The lock holder (`probe flock-hold`) holds `.cargo-lock`; a Cargo build
/// then blocks waiting for it (`/proc/locks` shows it as a waiter), holding
/// `.cargo-build-lock` where the toolchain has one, stays blocked, and
/// finishes once the holder lets go.
fn cargo_lock_holder_blocks_a_build_on_cargo_lock(ctx: &Ctx, toolchain: &Toolchain) -> TestResult {
    let project = built_race_project(ctx, toolchain, "race")?;
    let (files, watched) = watch_lock_files(&project)?;
    info!(
        "toolchain {} lock files: {:?}",
        toolchain.name,
        names(&files)
    );
    require_known_lock_names(toolchain, &files)?;
    let cargo_lock = project.join("target/debug/.cargo-lock");
    let build_lock = project.join("target/debug/.cargo-build-lock");
    ensure!(
        files.contains(&cargo_lock),
        "no .cargo-lock among {files:?}"
    );

    let holder = Holder::tester(ctx, &["flock-hold", path_str(&cargo_lock)?], &[])?;
    let held = watched.usage(&proc_locks::sample()?, holder.pid());
    ensure!(
        held.holds == [(cargo_lock.clone(), "WRITE".to_owned())],
        "the lock holder (pid {}) {held}, expected it to hold .cargo-lock exclusively",
        holder.pid()
    );

    let mut cargo = Cargo::new(toolchain, &project, &["build"]).spawn(ctx)?;
    let blocked = watched.wait_for(cargo.pid(), REACH_TIMEOUT, "blocked on .cargo-lock", |u| {
        u.waits(&cargo_lock)
    })?;
    info!("toolchain {}: blocked cargo {blocked}", toolchain.name);
    if files.contains(&build_lock) {
        ensure!(
            blocked.holds(&build_lock),
            "cargo waits for .cargo-lock without holding .cargo-build-lock: it {blocked}"
        );
    } else {
        info!(
            "toolchain {} makes no .cargo-build-lock: the check that a blocked build holds it \
             does not apply here (vacuous, not passed)",
            toolchain.name
        );
    }
    // Still blocked a moment later, holding the same files: the holder, not
    // chance, stops it.
    std::thread::sleep(Duration::from_millis(300));
    let later = watched.usage(&proc_locks::sample()?, cargo.pid());
    let held = |u: &proc_locks::Usage| {
        u.holds
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
    };
    ensure!(
        cargo.is_running()? && later.waits(&cargo_lock) && held(&later) == held(&blocked),
        "cargo did not stay blocked behind the holder: it {later}, before it {blocked}"
    );

    holder.finish()?;
    let out = cargo.wait()?;
    out.expect_success()?;
    ensure!(
        out.stderr.contains("Blocking waiting for file lock"),
        "cargo did not report waiting for the lock:\n{}",
        out.failure()
    );
    require_no_locks(&watched, "after the build")
}

/// The race fixture's build script waits at its gate while Cargo builds:
/// the script is Cargo's child, Cargo holds every lock file of the profile
/// directory (all of them known names; `.cargo-lock` exclusively on 1.90.0,
/// shared next to exclusive `.cargo-build-lock` and `.cargo-artifact-lock`
/// on later versions, as logged) and waits for none, and stays paused until
/// the gate opens; then the build finishes.
fn cargo_build_script_gate_pauses_a_build_holding_its_locks(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    let project = RACE.write(&ctx.scratch, "race")?;
    let gate = Gate::new(&ctx.scratch, "build-script")?;
    let mut cargo = Cargo::new(toolchain, &project, &["build"])
        .build_gate(&gate)
        .spawn(ctx)?;
    let script = gate.wait_until_waiting(REACH_TIMEOUT)?;
    let parent = cargo_under_test::parent_of(script).map_err(|e| {
        format!("the build script (pid {script}) is gone ({e}): it did not wait at its gate")
    })?;
    ensure!(
        parent == cargo.pid(),
        "the waiting build script (pid {script}) is not a child of cargo (pid {})",
        cargo.pid()
    );
    let (files, watched) = watch_lock_files(&project)?;
    require_known_lock_names(toolchain, &files)?;
    ensure!(
        files.contains(&project.join("target/debug/.cargo-lock")),
        "no .cargo-lock among {files:?}"
    );
    let paused = watched.wait_for(
        cargo.pid(),
        REACH_TIMEOUT,
        "holding every lock file of the profile",
        |u| files.iter().all(|f| u.holds(f)),
    )?;
    info!(
        "toolchain {}: lock files {:?}; cargo paused in its build script {paused}",
        toolchain.name,
        names(&files)
    );
    ensure!(
        paused.waits.is_empty(),
        "cargo paused in its build script also waits for a lock: it {paused}"
    );
    std::thread::sleep(Duration::from_millis(300));
    ensure!(
        cargo.is_running()? && !RACE.debug_binary(&project).exists(),
        "the build went on while its build script waited at the gate"
    );

    gate.open(&ctx.scratch)?;
    cargo.wait()?.expect_success()?;
    ensure!(
        RACE.debug_binary(&project).exists(),
        "no binary after the gated build"
    );
    require_no_locks(&watched, "after the build")
}

/// The runner wrapper pauses `cargo run` and `cargo test` between compiling
/// and running, while no process holds a lock on any lock file of the
/// target; once the gate opens the binary runs. Where the wrapper sits:
/// `cargo run` execs it in Cargo's own process (Cargo replaces itself with
/// the runner, or without one with the binary, on Unix), so the paused
/// process has Cargo's pid but the wrapper's executable; `cargo test` starts
/// it as a child and waits for it.
fn cargo_runner_gate_pauses_between_compile_and_run_holding_no_lock(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    let project = built_race_project(ctx, toolchain, "race")?;
    // `cargo test` compiles the test binary first, so both pause on an
    // up-to-date target.
    Cargo::new(toolchain, &project, &["test", "--no-run"])
        .output(ctx)?
        .expect_success()?;
    let (files, watched) = watch_lock_files(&project)?;
    require_known_lock_names(toolchain, &files)?;

    for (command, cargo_execs_runner, output) in [
        (&["run"][..], true, RACE.output),
        (
            &["test", "--", "--nocapture"][..],
            false,
            "diskgc-e2e cargo race fixture test ran",
        ),
    ] {
        let gate = Gate::new(&ctx.scratch, &format!("runner-{}", command[0]))?;
        let mut cargo = Cargo::new(toolchain, &project, command)
            .runner_gate(ctx, &gate)
            .spawn(ctx)?;
        let runner = gate.wait_until_waiting(REACH_TIMEOUT)?;
        if cargo_execs_runner {
            ensure!(
                runner == cargo.pid(),
                "cargo {} (pid {}) did not exec its runner (pid {runner})",
                command.join(" "),
                cargo.pid()
            );
        } else {
            let parent = cargo_under_test::parent_of(runner).map_err(|e| {
                format!("the runner (pid {runner}) is gone ({e}): it did not wait at its gate")
            })?;
            ensure!(
                parent == cargo.pid(),
                "the waiting runner (pid {runner}) is not a child of cargo {} (pid {})",
                command.join(" "),
                cargo.pid()
            );
        }
        let exe = std::fs::read_link(format!("/proc/{runner}/exe"))
            .map_err(|e| format!("readlink /proc/{runner}/exe: {e}"))?;
        ensure!(
            exe == ctx.self_exe,
            "the runner (pid {runner}) already runs {}, not the wrapper: not between compile \
             and run",
            exe.display()
        );
        let locks = proc_locks::sample()?;
        let usage = watched.usage(&locks, cargo.pid());
        info!(
            "toolchain {}: cargo {} paused before running the binary: it {usage}",
            toolchain.name,
            command.join(" ")
        );
        ensure!(
            usage.is_empty() && watched.locks_on(&locks).is_empty(),
            "cargo {} paused between compile and run, yet locks are held on the lock files {:?}: \
             {:?}",
            command.join(" "),
            names(&files),
            watched.locks_on(&locks)
        );
        ensure!(
            cargo.is_running()?,
            "cargo (or the runner it became) exited while the runner waited"
        );

        gate.open(&ctx.scratch)?;
        let out = cargo.wait()?;
        out.expect_success()?;
        ensure!(
            out.stdout.contains(output),
            "the binary did not run after the gate opened (expected {output:?}):\n{}",
            out.failure()
        );
    }
    Ok(())
}

fn path_str(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("non-UTF-8 path {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toolchains() -> Vec<Toolchain> {
        cargo_under_test::parse_manifest("1.90.0 1.90.0 full\n1.99.1 1.99.1 fast\n").unwrap()
    }

    #[test]
    fn every_cargo_test_is_listed_per_toolchain_in_its_tier() {
        let listed = listing(&toolchains());
        assert_eq!(listed.len(), CARGO_TESTS.len() * 2);
        assert!(listed.contains(&("cargo_smoke@1.90.0".to_owned(), Tier::Full)));
        assert!(listed.contains(&("cargo_smoke@1.99.1".to_owned(), Tier::Fast)));
    }

    #[test]
    fn names_are_unique_and_never_contain_the_separator() {
        let mut names: Vec<_> = CARGO_TESTS.iter().map(|t| t.name).collect();
        assert!(names.iter().all(|n| !n.contains(TOOLCHAIN_SEPARATOR)));
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), CARGO_TESTS.len());
        for test in crate::suite::TESTS {
            assert!(
                !names.contains(&test.name),
                "{} in both registries",
                test.name
            );
        }
    }

    #[test]
    fn the_switch_bound_is_a_share_of_the_full_trace() {
        let full = Rusage {
            voluntary_switches: 19_000,
            seccomp_filters: 1,
        };
        assert_eq!(switch_bound(full), 11_400);
    }
}
