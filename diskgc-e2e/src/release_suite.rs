//! `release-candidate` package probes. All invocations use the binary copied from
//! the release derivation, under the disposable container's deletion oracle.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::cargo_harness::reclaimable;
use crate::cargo_under_test::{Cargo, SMOKE, Toolchain};
use crate::ensure_suite::report;
use crate::holder::Holder;
use crate::oracle::{Fixture, Oracle};
use crate::owner_pid_suite::{HIDEPID, NOT_INITIAL, Namespace, expect_refused};
use crate::suite::{Ctx, TestResult};
use crate::{TESTER, ensure};

// The release candidate's (0.2) catalog: 0.1's, signed off by TASK-65
// (Mark, 2026-10-24: Cargo, the two /tmp built-ins with a liveness
// contract, and the idle filters), plus nix-garbage (Mark, 2026-11-04,
// TASK-335).
const ADMITTED: &[&str] = &[
    "cargo-target",
    "nix-garbage",
    "nix-shell-rc",
    "pytest-tmp",
    "idle-1h",
    "idle-1d",
    "idle-2d",
    "idle-3d",
    "idle-7d",
    "idle-10d",
    "idle-30d",
];

// Every built-in source currently excluded by the release catalog.
// The list assertion below also fails if a newly added source leaks in.
const EXCLUDED: &[&str] = &[
    "docker-build-cache",
    "docker-dangling",
    "docker-unreferenced",
    // TASK-93, pending Mark's admission: development library only.
    "podman-dangling",
    "podman-unreferenced",
    "pytest-cache",
    "nix-user-cache",
    "bun-cache",
    "go-mod-cache",
    "python-pycache",
    "mypy-cache",
    "cmake-build-anchored",
    "meson-build-anchored",
    "autotools-build-anchored",
    "pip-cache",
    "uv-cache",
    "go-build-cache",
    "npm-cache",
    "pnpm-store",
    "yarn-cache",
    "ccache",
    "jest-cache",
    "west-build-anchored",
    "platformio-cache",
    "node-compile-cache",
    "brave-browser-cache",
    "chrome-browser-cache",
    "chromium-browser-cache",
    "edge-browser-cache",
    "cursor-agent-versions",
    "codex-releases",
    "claude-code-versions",
    "cursor-extensions",
    // TASK-94, pending Mark's admission: development library only.
    "rustup-toolchains",
    "elan-toolchains",
    "node-modules",
    "python-venv",
    // TASK-78, pending Mark's admission of the app names: development
    // library only.
    "teams-for-linux-electron-cache",
    "figma-linux-electron-cache",
    // TASK-40, pending Mark's admission: development library only.
    "huggingface-hub-repos",
    "cargo-registry-src",
    "node-gyp-headers",
    "electron-download-cache",
    // TASK-101, pending Mark's admission: development library only.
    "buildroot-output-anchored",
    "yocto-tmpdir-anchored",
    "bazel-output-base-anchored",
];

pub(crate) fn run_binary(
    ctx: &Ctx,
    binary: &str,
    args: &[&str],
    expected: i32,
) -> Result<crate::under_test::Run, String> {
    run_binary_with(ctx, binary, args, expected, Fixture::new(), &[])
}

/// The binary named `binary` under test: `diskgc-release` is the
/// release-candidate package's, which the release runner installs as the
/// default `diskgc` ([`crate::RELEASE_DEFAULT_MARKER`]).
pub(crate) fn binary_path(ctx: &Ctx, binary: &str) -> PathBuf {
    if binary == "diskgc-release" && ctx.bin_dir.join(crate::RELEASE_DEFAULT_MARKER).exists() {
        ctx.diskgc()
    } else {
        ctx.bin_dir.join(binary)
    }
}

fn run_binary_with(
    ctx: &Ctx,
    binary: &str,
    args: &[&str],
    expected: i32,
    fixture: Fixture,
    env: &[(String, String)],
) -> Result<crate::under_test::Run, String> {
    let binary = binary_path(ctx, binary);
    let oracle = Oracle::arm(fixture)?;
    let output = crate::under_test::run_as_tester_with_env(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        env,
        &binary,
        args,
    )?;
    oracle.judge(&output)?.assert_clean()?;
    output.expect_code(expected)?;
    Ok(output)
}

fn run(ctx: &Ctx, args: &[&str], expected: i32) -> Result<crate::under_test::Run, String> {
    run_binary(ctx, "diskgc-release", args, expected)
}

pub(crate) fn profile_names(output: &str) -> Result<BTreeSet<String>, String> {
    let listed: Value = serde_json::from_str(output)
        .map_err(|error| format!("parse profiles list: {error}: {output}"))?;
    let entries = listed["profiles"]
        .as_array()
        .ok_or("profiles list omitted profiles")?;
    let names: BTreeSet<String> = entries
        .iter()
        .map(|profile| {
            profile["name"]
                .as_str()
                .map(str::to_owned)
                .ok_or("profile omitted name")
        })
        .collect::<Result<_, _>>()?;
    ensure!(
        names.len() == entries.len(),
        "profiles list has duplicate names"
    );
    Ok(names)
}

pub fn candidate_catalog_refuses_every_excluded_builtin(ctx: &Ctx) -> TestResult {
    let list = run(ctx, &["profiles", "list", "--json"], 0)?;
    let names = profile_names(&list.stdout)?;
    let admitted: BTreeSet<String> = ADMITTED.iter().map(|name| (*name).to_owned()).collect();
    ensure!(
        names == admitted,
        "candidate catalog differs from the reviewed catalog: {names:?}"
    );
    let development = run_binary(
        ctx,
        "diskgc-development",
        &["profiles", "list", "--json"],
        0,
    )?;
    let full = profile_names(&development.stdout)?;
    let excluded: BTreeSet<String> = full.difference(&names).cloned().collect();
    let expected_excluded: BTreeSet<String> =
        EXCLUDED.iter().map(|name| (*name).to_owned()).collect();
    ensure!(
        excluded == expected_excluded,
        "excluded built-in test vector is stale: {excluded:?}"
    );

    let help = run(ctx, &["--help"], 0)?;
    for name in ["cargo-target", "nix-garbage", "nix-shell-rc", "pytest-tmp"] {
        ensure!(help.stdout.contains(name), "help omits {name}");
        let shown = run(ctx, &["profiles", "show", name, "--json"], 0)?;
        ensure!(
            shown.stdout.contains(name),
            "profiles show omits admitted {name}: {shown:?}"
        );
    }
    for name in EXCLUDED {
        ensure!(!help.stdout.contains(name), "help exposes excluded {name}");
        let shown = run(ctx, &["profiles", "show", name, "--json"], 2)?;
        ensure!(
            shown.stdout.contains(name) || shown.stderr.contains(name),
            "profiles show did not identify excluded {name}: {shown:?}"
        );
        let select = format!("profile:{name}");
        let refused = run(
            ctx,
            &[
                "ensure",
                "--json",
                "--min-free",
                "100%",
                "--select",
                &select,
                "--roots",
                "/tmp",
                "/tmp",
            ],
            2,
        )?;
        ensure!(
            refused.stdout.contains(name) || refused.stderr.contains(name),
            "ensure did not identify excluded {name}: {refused:?}"
        );
    }
    Ok(())
}

pub fn candidate_rejects_docker_command_and_arbitrary_delete_policy(ctx: &Ctx) -> TestResult {
    ctx.scratch.mkdirs("release-policy-guard", 0o700, TESTER)?;
    let keep = ctx.scratch.write_file(
        "release-policy-guard/keep",
        b"release refusal must preserve this",
        0o600,
        TESTER,
    )?;
    ctx.home.mkdirs("release-spy-bin", 0o700, TESTER)?;
    let docker_spy = ctx.home.write_file(
        "release-spy-bin/docker",
        b"#!/bin/sh\nprintf invoked > /tmp/release-policy-guard/docker-invoked\nexit 99\n",
        0o700,
        TESTER,
    )?;
    let command_spy = ctx.home.write_file(
        "release-command-spy",
        b"#!/bin/sh\nprintf invoked > /tmp/release-policy-guard/command-invoked\nexit 99\n",
        0o700,
        TESTER,
    )?;
    let env = [(
        "PATH".to_owned(),
        "/home/tester/release-spy-bin:/bin".to_owned(),
    )];
    let configs = [
        (
            "docker",
            "version: 1\ngroups:\n  - name: blocked\n    tiers:\n      - name: docker\n        select: profile:docker-build-cache\n",
            "docker-build-cache",
        ),
        (
            "command",
            "version: 1\nprofiles:\n  - name: custom-cleaner\n    find: {roots: required, paths: [release-policy-guard], type: dir}\n    action: {command: [/home/tester/release-command-spy, '{path}']}\n    examples: {match: [/tmp/release-policy-guard]}\ngroups:\n  - name: blocked\n    tiers:\n      - name: command\n        select: profile:custom-cleaner\n        roots: [/tmp]\n",
            "custom-cleaner",
        ),
        (
            "delete",
            "version: 1\nprofiles:\n  - name: custom-delete\n    find: {roots: required, paths: [release-policy-guard], type: dir}\n    action: delete\n    examples: {match: [/tmp/release-policy-guard]}\ngroups:\n  - name: blocked\n    tiers:\n      - name: delete\n        select: profile:custom-delete\n        roots: [/tmp]\n",
            "custom-delete",
        ),
    ];
    for (label, config, rejected) in configs {
        let file = format!("release-{label}.yaml");
        ctx.home
            .write_file(&file, config.as_bytes(), 0o600, TESTER)?;
        let path = format!("/home/tester/{file}");
        let selection = format!("profile:{rejected}");
        for args in [
            vec!["check", "--config", &path],
            vec![
                "ensure",
                "--json",
                "--min-free",
                "100%",
                "--config",
                &path,
                "/tmp",
            ],
            vec![
                "ensure",
                "--json",
                "--min-free",
                "100%",
                "--config",
                &path,
                "--select",
                &selection,
                "--roots",
                "/tmp",
                "/tmp",
            ],
        ] {
            let refused = run_binary_with(ctx, "diskgc-release", &args, 2, Fixture::new(), &env)?;
            ensure!(
                refused.stdout.contains(rejected) || refused.stderr.contains(rejected),
                "{label}: refusal did not name {rejected}: {refused:?}"
            );
            ensure!(keep.exists(), "{label}: candidate mutated fixture");
            ensure!(
                !ctx.scratch
                    .path("release-policy-guard/docker-invoked")
                    .exists()
                    && !ctx
                        .scratch
                        .path("release-policy-guard/command-invoked")
                        .exists(),
                "{label}: rejected policy started a backend child"
            );
        }
    }
    ensure!(
        docker_spy.exists() && command_spy.exists(),
        "spy fixture disappeared"
    );
    Ok(())
}

/// TASK-276 open: the gate-lifted candidate refuses a reclaiming `watch`
/// (exit 2, in text and under `--json`) before it looks at the machine.
/// PATH does not exist, so a candidate without the refusal fails here on
/// its own startup error, not the refusal's message, instead of running
/// as a service until the budget.
pub fn candidate_refuses_a_reclaiming_watch(ctx: &Ctx) -> TestResult {
    ctx.scratch.mkdirs("release-watch/target", 0o700, TESTER)?;
    let keep = ctx.scratch.write_file(
        "release-watch/target/keep",
        b"a refused watch must preserve this",
        0o600,
        TESTER,
    )?;
    let roots = ctx.scratch.path("release-watch");
    let roots = roots
        .to_str()
        .ok_or_else(|| format!("non-UTF-8 path {}", roots.display()))?;
    let want = "a reclaiming `watch` is not in diskgc ";
    let watch = [
        "watch",
        "--min-free",
        "1G",
        "--target-free",
        "2G",
        "--select",
        "profile:cargo-target",
        "--roots",
        roots,
        "/nonexistent-release-watch",
    ];
    let refused = run(ctx, &watch, 2)?;
    ensure!(
        refused.stdout.is_empty()
            && refused
                .stderr
                .starts_with(&format!("diskgc: error: {want}")),
        "reclaiming watch was not refused as such: {refused:?}"
    );
    let mut with_json = watch.to_vec();
    with_json.push("--json");
    let refused = run(ctx, &with_json, 2)?;
    let error: Value = serde_json::from_str(&refused.stdout)
        .map_err(|e| format!("parse refusal: {e}: {}", refused.stdout))?;
    ensure!(
        error["status"] == "usage_error"
            && error["errors"][0]["cause"] == "usage"
            && error["errors"][0]["message"]
                .as_str()
                .is_some_and(|m| m.starts_with(want)),
        "reclaiming watch --json was not refused as such: {error}"
    );
    ensure!(keep.exists(), "a refused watch mutated the fixture");
    Ok(())
}

/// TASK-289: the gate-lifted candidate runs `watch --tally-only` (its
/// release row's command, which its package ships unit templates for): it
/// starts, tallies a Cargo target, stops on SIGTERM with exit 0, writes
/// its snapshot, writes no record and deletes nothing (the oracle judges
/// the run with no eligible path). Backs the release notes' claim on the
/// artifact itself.
pub fn candidate_runs_watch_tally_only(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdirs("release-tally/proj/target/debug/deps", 0o755, TESTER)?;
    s.write_file("release-tally/proj/Cargo.toml", b"", 0o644, TESTER)?;
    s.write_file(
        "release-tally/proj/target/CACHEDIR.TAG",
        b"Signature: 8a477f597d28d172789f06886806bc55\n",
        0o644,
        TESTER,
    )?;
    s.write_file(
        "release-tally/proj/target/debug/.cargo-lock",
        b"",
        0o644,
        TESTER,
    )?;
    let keep = s.write_file(
        "release-tally/proj/target/debug/deps/old",
        &[7; 8192],
        0o644,
        TESTER,
    )?;
    let roots = s.path("release-tally");
    let roots = roots
        .to_str()
        .ok_or_else(|| format!("non-UTF-8 path {}", roots.display()))?;
    let binary = binary_path(ctx, "diskgc-release");
    let oracle = Oracle::arm(Fixture::new())?;
    let spawned = ctx.spawn_as_tester(
        &binary,
        &[
            "watch",
            "--tally-only",
            "--json",
            "--select",
            "profile:cargo-target",
            "--roots",
            roots,
            roots,
        ],
    )?;
    // `diskgc` in the release runner, `diskgc-release` in the standard one.
    let comm = binary
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("binary name of {}", binary.display()))?;
    let pid = crate::watch_suite::pid_below_strace(&spawned, comm)?;
    std::thread::sleep(std::time::Duration::from_secs(2));
    crate::watch_suite::signal(pid, libc::SIGTERM)?;
    let run = spawned.wait()?;
    let verdict = oracle.judge(&run)?;
    run.expect_code(0)?;
    ensure!(run.stdout.is_empty(), "tally-only wrote: {}", run.stdout);
    ensure!(
        crate::watch_suite::snapshot_written(),
        "no snapshot written: {}",
        run.stderr
    );
    ensure!(
        run.stderr.contains("written: ") && !run.stderr.contains("not written"),
        "the stop did not write the snapshot: {}",
        run.stderr
    );
    ensure!(keep.exists(), "tally-only mutated the fixture");
    verdict.assert_clean()
}

/// TASK-263: the release-candidate build has no test-container exception
/// (docs/safety.md, In use: Visibility, a process list that cannot be complete). In a
/// PID namespace of its own (`unshare -p`, as a container), and under
/// `hidepid`, `ensure` below `--min-free` is refused, its dry run too
/// whatever the space: exit 2, `cause: safety`, naming the rule; nothing
/// planned, nothing deleted (the oracle judges every run). TASK-298: with
/// the space already there (`--min-free 1M`) the same reclaiming `ensure`
/// is the usual no-op, exit 0 `met`, as a CI pre-job hook in a container
/// needs, and says on stderr that a reclaim there would be refused. Positive control: the same dry run from the container's
/// namespace, the host's (`--pid=host`), plans.
pub fn candidate_refuses_in_its_own_pid_namespace(ctx: &Ctx) -> TestResult {
    ctx.scratch.mkdirs("release-pidns/project", 0o755, TESTER)?;
    let keep = ctx.scratch.write_file(
        "release-pidns/project/keep",
        b"a refused run must preserve this",
        0o600,
        TESTER,
    )?;
    let roots = ctx.scratch.path("release-pidns");
    let roots = roots
        .to_str()
        .ok_or_else(|| format!("non-UTF-8 path {}", roots.display()))?
        .to_owned();
    let reclaim = [
        "--json",
        "--no-index",
        "--min-free",
        "100%",
        "--select",
        "profile:cargo-target",
        "--roots",
        &roots,
        &roots,
    ];
    let mut dry_run = vec!["--dry-run"];
    dry_run.extend(reclaim);
    // The same call with a goal the scratch filesystem already meets.
    let met = [
        "--json",
        "--no-index",
        "--min-free",
        "1M",
        "--select",
        "profile:cargo-target",
        "--roots",
        &roots,
        &roots,
    ];
    let mut met_dry_run = vec!["--dry-run"];
    met_dry_run.extend(met);
    let binary = binary_path(ctx, "diskgc-release");
    for (hidepid, rule) in [(false, NOT_INITIAL), (true, HIDEPID)] {
        let namespace = Namespace::start(hidepid)?;
        let oracle = Oracle::arm(Fixture::new())?;
        let plan = namespace.binary_as_tester(ctx, &binary, &dry_run)?;
        let run = namespace.binary_as_tester(ctx, &binary, &reclaim)?;
        let met_plan = namespace.binary_as_tester(ctx, &binary, &met_dry_run)?;
        let met_run = namespace.binary_as_tester(ctx, &binary, &met)?;
        oracle
            .judge_runs(&[&plan, &run, &met_plan, &met_run])?
            .assert_clean()?;
        expect_refused(&plan, rule)?;
        expect_refused(&run, rule)?;
        expect_refused(&met_plan, rule)?;
        met_run.expect_code(0)?;
        let report = report(&met_run)?;
        ensure!(
            report["status"] == "met"
                && report["errors"].as_array().is_some_and(Vec::is_empty)
                && report["actions"].as_array().is_some_and(Vec::is_empty),
            "{rule}: a met run in its own namespace is not the usual no-op: {report}"
        );
        ensure!(
            met_run
                .stderr
                .lines()
                .any(|l| l.contains("a reclaim here would be refused: ") && l.contains(rule)),
            "{rule}: the met run does not say a reclaim would be refused: {}",
            met_run.stderr
        );
        drop(namespace);
    }
    let mut control = vec!["ensure"];
    control.extend(dry_run);
    run(ctx, &control, 0)?;
    ensure!(keep.exists(), "a refused run mutated the fixture");
    Ok(())
}

pub fn candidate_checkout_policy_accepts_only_admitted_builtin(ctx: &Ctx) -> TestResult {
    for (name, expected) in [
        ("cargo-target", 0),
        ("pytest-tmp", 0),
        ("nix-shell-rc", 0),
        ("npm-cache", 2),
        ("uv-cache", 2),
        ("docker-build-cache", 2),
    ] {
        let file = format!("release-checkouts-{name}.yaml");
        let yaml =
            format!("version: 1\nprofile_policies:\n  - profile: {name}\n    checkouts: refuse\n");
        ctx.home.write_file(&file, yaml.as_bytes(), 0o600, TESTER)?;
        let path = format!("/home/tester/{file}");
        let checked = run(ctx, &["check", "--config", &path], expected)?;
        if expected == 2 {
            ensure!(
                checked.stdout.contains(name) || checked.stderr.contains(name),
                "excluded checkout policy did not name {name}: {checked:?}"
            );
        }
    }
    let explicit_allow = "/home/tester/release-checkouts-cargo-target-allow.yaml";
    ctx.home.write_file(
        "release-checkouts-cargo-target-allow.yaml",
        b"version: 1\nprofile_policies:\n  - profile: cargo-target\n    checkouts: allow\n",
        0o600,
        TESTER,
    )?;
    run(ctx, &["check", "--config", explicit_allow], 0)?;
    // A 0.1 checkout policy may only narrow: allowing checkouts for
    // nix-shell-rc, whose built-in refuses them, is refused.
    ctx.home.write_file(
        "release-checkouts-nix-shell-rc-allow.yaml",
        b"version: 1\nprofile_policies:\n  - profile: nix-shell-rc\n    checkouts: allow\n",
        0o600,
        TESTER,
    )?;
    let widened = run(
        ctx,
        &[
            "check",
            "--config",
            "/home/tester/release-checkouts-nix-shell-rc-allow.yaml",
        ],
        2,
    )?;
    ensure!(
        widened.stderr.contains("may only narrow") || widened.stdout.contains("may only narrow"),
        "widening checkout policy was not refused as such: {widened:?}"
    );
    // The built-in allows vendored checkout content; an operator may narrow
    // it without replacing its matcher, kind or action.
    let built_in = run(ctx, &["profiles", "show", "cargo-target", "--json"], 0)?;
    ensure!(built_in.stdout.contains("\"allow\""), "{built_in:?}");
    let changed = run(
        ctx,
        &[
            "profiles",
            "show",
            "cargo-target",
            "--json",
            "--config",
            "/home/tester/release-checkouts-cargo-target.yaml",
        ],
        0,
    )?;
    ensure!(changed.stdout.contains("\"refuse\""), "{changed:?}");

    // Build a real Cargo target, then put a vendored checkout marker under
    // its output tree. The actual candidate binary must apply the default
    // allow setting, explicit allow and the operator's refusal to the same
    // inode tree.
    let toolchain = Toolchain::fast()?;
    let project = SMOKE.write(&ctx.scratch, "release-cargo-checkout")?;
    Cargo::new(&toolchain, &project, &["build"])
        .output(ctx)?
        .expect_success()?;
    ctx.scratch.mkdirs(
        "release-cargo-checkout/target/debug/build/vendor/out/repo/.git",
        0o755,
        TESTER,
    )?;
    let marker = ctx.scratch.write_file(
        "release-cargo-checkout/target/debug/build/vendor/out/repo/.git/HEAD",
        b"ref: refs/heads/main\n",
        0o644,
        TESTER,
    )?;
    let target = project.join("target");
    let target_text = target.to_string_lossy();
    for (config, expected_checkout, expected_reason) in [
        (None, "allow", None),
        (Some(explicit_allow), "allow", None),
        (
            Some("/home/tester/release-checkouts-cargo-target.yaml"),
            "refuse",
            Some("contains_checkout"),
        ),
    ] {
        let mut args = vec![
            "ensure",
            "--dry-run",
            "--json",
            "--min-free",
            "100%",
            "--select",
            "profile:cargo-target",
        ];
        if let Some(config) = config {
            args.extend(["--config", config]);
        }
        args.extend([
            "--roots",
            "/tmp/release-cargo-checkout",
            "/tmp/release-cargo-checkout",
        ]);
        let dry_run = run(ctx, &args, 0)?;
        let result: Value = serde_json::from_str(&dry_run.stdout)
            .map_err(|error| format!("parse candidate dry run: {error}: {}", dry_run.stdout))?;
        let plan = result["plan"]
            .as_array()
            .ok_or_else(|| format!("candidate dry run omitted plan: {result}"))?;
        let entry = plan
            .iter()
            .find(|entry| entry["candidate"].as_str() == Some(target_text.as_ref()))
            .ok_or_else(|| format!("candidate dry run omitted real target: {result}"))?;
        ensure!(
            entry["checkouts"] == expected_checkout
                && entry["reason"].as_str() == expected_reason
                && entry["eligible"] == expected_reason.is_none(),
            "candidate checkout policy differs from {expected_checkout}: {entry}"
        );
        ensure!(marker.exists(), "candidate dry run removed checkout marker");
    }
    Ok(())
}

/// The packaged binary makes the operator's ordinary Cargo journey: a real
/// build stays intact while its lock is held, then its reclaimable outputs
/// go after the lock is released. All mutation is inside disposable tmpfs.
pub fn candidate_cargo_busy_then_reclaims_real_target(ctx: &Ctx) -> TestResult {
    let toolchain = Toolchain::fast()?;
    let project = SMOKE.write(&ctx.scratch, "release-cargo-reclaim")?;
    Cargo::new(&toolchain, &project, &["build"])
        .output(ctx)?
        .expect_success()?;
    let target = project.join("target");
    let binary = target.join("debug").join(SMOKE.binary);
    ensure!(binary.exists(), "Cargo did not create the expected binary");
    let keep = ctx.scratch.write_file(
        "release-cargo-reclaim/keep-source",
        b"source must survive reclaim\n",
        0o644,
        TESTER,
    )?;
    let lock = target.join("debug/.cargo-lock");
    let lock_name = lock.to_str().ok_or("non-UTF8 Cargo lock path")?;
    let holder = Holder::tester(ctx, &["flock-hold", lock_name], &[])?;
    let args = [
        "ensure",
        "--json",
        "--min-free",
        "100%",
        "--select",
        "profile:cargo-target",
        "--roots",
        "/tmp/release-cargo-reclaim",
        "/tmp/release-cargo-reclaim",
    ];
    let busy = run(ctx, &args, 1)?;
    let busy_report: Value = serde_json::from_str(&busy.stdout)
        .map_err(|e| format!("parse busy candidate report: {e}: {}", busy.stdout))?;
    ensure!(
        busy_report["actions"].as_array().is_some_and(Vec::is_empty),
        "busy target was acted on: {busy_report}"
    );
    ensure!(
        binary.exists() && keep.exists(),
        "busy target or source was touched"
    );
    holder.finish()?;
    // diskgc tries the lock once. A tester-held probe waits out a release
    // lagging the close (flock-hold retries a busy lock up to
    // RELEASE_BOUND). Its own release is by TESTER, a host subuid under
    // rootless Podman whose /proc fd table host-user readers cannot open;
    // a harness probe runs as container root, the host uid, so they can.
    let lock_str = lock
        .to_str()
        .ok_or_else(|| format!("non-UTF-8 path {}", lock.display()))?;
    Holder::tester(ctx, &["flock-hold", lock_str], &[])
        .map_err(|e| format!("{lock_str} stayed locked after its holder exited: {e}"))?
        .finish()?;

    let reclaimed = run_binary_with(ctx, "diskgc-release", &args, 1, reclaimable(&target)?, &[])?;
    let report: Value = serde_json::from_str(&reclaimed.stdout)
        .map_err(|e| format!("parse reclaim candidate report: {e}: {}", reclaimed.stdout))?;
    ensure!(
        report["actions"].as_array().is_some_and(|actions| actions
            .iter()
            .any(|action| action["candidate"] == target.to_string_lossy().as_ref())),
        "released target was not reclaimed: {report}"
    );
    ensure!(!binary.exists(), "released target binary was not reclaimed");
    ensure!(
        lock.exists() && keep.exists(),
        "Cargo lock or source was touched"
    );
    Cargo::new(&toolchain, &project, &["build"])
        .output(ctx)?
        .expect_success()?;
    ensure!(binary.exists(), "Cargo could not rebuild after reclaim");
    Ok(())
}
