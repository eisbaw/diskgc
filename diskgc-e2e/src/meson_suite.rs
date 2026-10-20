//! Real Meson-generated output in a disposable container. No developer host
//! project path is mounted into this test; Meson and Ninja are pinned by the
//! flake and reached through the read-only Nix store bind.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use crate::ensure_suite::{ensure_run, report};
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, TestResult};
use crate::{TESTER, ensure};

const TEST_PROFILE: &str = r#"version: 1
profiles:
  - name: meson-anchor-kernel
    find: {roots: required, paths: ["*", "*/*"], type: dir}
    where: {has: build.ninja, has_dir: meson-private, lacks_entry: meson.build, source_anchor: meson}
    action: delete
    examples: {root: /tmp, match: [/tmp/meson-app/odd-output]}
"#;

fn setup_meson(ctx: &Ctx, project: &str) -> TestResult {
    let source = ctx.scratch.mkdirs(project, 0o700, TESTER)?;
    ctx.scratch.write_file(
        &format!("{project}/meson.build"),
        b"project('anchor-fixture')\n",
        0o600,
        TESTER,
    )?;
    let output = ctx.scratch.path(&format!("{project}/odd-output"));
    let meson = ctx.bin_dir.join("meson");
    let result = Command::new(&meson)
        .arg("setup")
        .arg(&output)
        .arg(&source)
        .env_clear()
        .env("PATH", format!("{}:/bin", ctx.bin_dir.display()))
        .env("HOME", "/home/tester")
        .env("LANG", "C.UTF-8")
        .uid(TESTER.uid)
        .gid(TESTER.gid)
        .output()
        .map_err(|error| format!("run pinned Meson for {project}: {error}"))?;
    ensure!(
        result.status.success(),
        "pinned Meson setup for {project} failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    ensure!(
        output.join("build.ninja").is_file()
            && output.join("meson-private/coredata.dat").is_file()
            && output.join("meson-info/meson-info.json").is_file(),
        "Meson did not produce the expected Ninja and source metadata"
    );
    Ok(())
}

fn du_bytes(ctx: &Ctx, path: &Path) -> Result<u64, String> {
    let output = Command::new(ctx.bin_dir.join("du"))
        .arg("-sB1")
        .arg(path)
        .output()
        .map_err(|error| format!("run pinned du: {error}"))?;
    ensure!(
        output.status.success(),
        "du failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).map_err(|error| error.to_string())?;
    stdout
        .split_whitespace()
        .next()
        .ok_or_else(|| format!("du reported no size: {stdout:?}"))?
        .parse()
        .map_err(|error| format!("du size is invalid: {error}"))
}

/// The source-anchored built-in sees an actual Meson 1.x Ninja output and
/// applies its three-day floor. The equivalent no-floor test profile then
/// reclaims only the generated output whose source still exists.
pub fn meson_generated_output_uses_surviving_source_anchor(ctx: &Ctx) -> TestResult {
    for project in ["meson-app", "meson-missing", "meson-checkout"] {
        setup_meson(ctx, project)?;
    }
    ctx.scratch.remove_file("meson-missing/meson.build")?;
    ctx.scratch
        .mkdirs("meson-checkout/odd-output/.git", 0o700, TESTER)?;
    ctx.scratch.write_file(
        "meson-checkout/odd-output/.git/HEAD",
        b"ref: refs/heads/main\n",
        0o600,
        TESTER,
    )?;
    ctx.home
        .write_file("diskgc.yaml", TEST_PROFILE.as_bytes(), 0o644, TESTER)?;

    let eligible = ctx.scratch.path("meson-app/odd-output");
    let missing = ctx.scratch.path("meson-missing/odd-output");
    let checkout = ctx.scratch.path("meson-checkout/odd-output");
    let expected_bytes = du_bytes(ctx, &eligible)?;
    let oracle = Oracle::arm(Fixture::new().eligible(eligible.clone()))?;
    let built_in = ensure_run(
        ctx,
        &[
            "--json",
            "--dry-run",
            "--no-index",
            "--min-free",
            "100%",
            "--select",
            "profile:meson-build-anchored",
            "--roots",
            "/tmp",
            "/tmp",
        ],
    )?;
    built_in.expect_code(0)?;
    let plan = report(&built_in)?;
    ensure!(
        plan["plan"]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|entry| {
                entry["candidate"].as_str() == eligible.to_str()
                    && entry["reason"].as_str() == Some("not_idle")
                    && entry["build_evidence"]["marker"].as_str()
                        == Some("meson-info/meson-info.json")
                    && entry["build_evidence"]["source"].as_str() == Some("/tmp/meson-app")
            })),
        "built-in Meson anchor or idle floor not observed: {plan}"
    );

    let measured = ensure_run(
        ctx,
        &[
            "--json",
            "--dry-run",
            "--config",
            "/home/tester/diskgc.yaml",
            "--no-index",
            "--min-free",
            "100%",
            "--select",
            "profile:meson-anchor-kernel",
            "--roots",
            "/tmp",
            "/tmp",
        ],
    )?;
    measured.expect_code(0)?;
    let measured_plan = report(&measured)?;
    ensure!(
        measured_plan["plan"]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|entry| {
                entry["candidate"].as_str() == eligible.to_str()
                    && entry["estimated_bytes"].as_u64() == Some(expected_bytes)
            })),
        "Meson estimate differs from du ({expected_bytes} bytes): {measured_plan}"
    );

    let run = ensure_run(
        ctx,
        &[
            "--json",
            "--config",
            "/home/tester/diskgc.yaml",
            "--no-index",
            "--min-free",
            "100%",
            "--select",
            "profile:meson-anchor-kernel",
            "--roots",
            "/tmp",
            "/tmp",
        ],
    )?;
    let verdict = oracle.judge_runs(&[&built_in, &measured, &run])?;
    run.expect_code(1)?;
    let outcome = report(&run)?;
    ensure!(
        outcome["actions"]
            .as_array()
            .is_some_and(|actions| actions.len() == 1
                && actions[0]["candidate"].as_str() == eligible.to_str()),
        "Meson reclaim selected unexpected output: {outcome}"
    );
    ensure!(!eligible.exists(), "eligible Meson output remains");
    ensure!(
        missing.join("build.ninja").exists(),
        "missing-source output was removed"
    );
    ensure!(
        checkout.join(".git/HEAD").exists(),
        "checkout output was removed"
    );
    verdict.assert_clean()
}
