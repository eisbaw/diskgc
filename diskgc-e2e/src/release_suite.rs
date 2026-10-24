//! Candidate 0.1 package probes. All invocations use the binary copied from
//! the release derivation, under the disposable container's deletion oracle.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;

use crate::cargo_harness::reclaimable;
use crate::cargo_under_test::{Cargo, SMOKE, Toolchain};
use crate::holder::Holder;
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, TestResult};
use crate::{TESTER, ensure};

const ADMITTED: &[&str] = &[
    "cargo-target",
    "idle-1h",
    "idle-1d",
    "idle-2d",
    "idle-3d",
    "idle-7d",
    "idle-10d",
    "idle-30d",
];

// Every built-in source currently excluded by the proposed 0.1 catalog.
// The list assertion below also fails if a newly added source leaks in.
const EXCLUDED: &[&str] = &[
    "docker-build-cache",
    "docker-dangling",
    "docker-unreferenced",
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
    "claude-code-tmp",
    "chromium-tmp",
    "node-compile-cache",
    "playwright-tmp",
    "zephyr-tmp",
    "renode-tmp",
    "nix-shell-rc",
    "xdg-cache-entry-v2",
    "brave-browser-cache",
    "chrome-browser-cache",
    "chromium-browser-cache",
    "edge-browser-cache",
    "cursor-agent-versions",
    "codex-releases",
    "claude-code-versions",
    "cursor-extensions",
];

fn run_binary(
    ctx: &Ctx,
    binary: &str,
    args: &[&str],
    expected: i32,
) -> Result<crate::under_test::Run, String> {
    run_binary_with(ctx, binary, args, expected, Fixture::new(), &[])
}

fn run_binary_with(
    ctx: &Ctx,
    binary: &str,
    args: &[&str],
    expected: i32,
    fixture: Fixture,
    env: &[(String, String)],
) -> Result<crate::under_test::Run, String> {
    let binary =
        if binary == "diskgc-release-01" && ctx.bin_dir.join("diskgc-release-default").exists() {
            ctx.diskgc()
        } else {
            ctx.bin_dir.join(binary)
        };
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
    run_binary(ctx, "diskgc-release-01", args, expected)
}

fn profile_names(output: &str) -> Result<BTreeSet<String>, String> {
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
        "candidate catalog differs from the reviewed Cargo-only subset: {names:?}"
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
    ensure!(
        help.stdout.contains("cargo-target"),
        "help omits cargo-target"
    );
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
            let refused =
                run_binary_with(ctx, "diskgc-release-01", &args, 2, Fixture::new(), &env)?;
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

pub fn candidate_checkout_policy_accepts_only_admitted_builtin(ctx: &Ctx) -> TestResult {
    for (name, expected) in [
        ("cargo-target", 0),
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

    let reclaimed = run_binary_with(
        ctx,
        "diskgc-release-01",
        &args,
        1,
        reclaimable(&target)?,
        &[],
    )?;
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
