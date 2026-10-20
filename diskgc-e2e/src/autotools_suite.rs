//! A GNU Autoconf generated build in a disposable container. The only host
//! input is the pinned, read-only Nix store tool closure.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use crate::ensure_suite::{ensure_run, report};
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, TestResult};
use crate::{TESTER, ensure};

const TEST_PROFILE: &str = r#"version: 1
profiles:
  - name: autotools-anchor-kernel
    find: {roots: required, paths: ["*", "*/*"], type: dir}
    where: {has: config.status, lacks_entry: configure.ac, source_anchor: autotools}
    action: delete
    examples: {root: /tmp, match: [/tmp/autotools-app/odd-output]}
"#;

fn setup_autotools(ctx: &Ctx, project: &str) -> TestResult {
    let source = ctx.scratch.mkdirs(project, 0o700, TESTER)?;
    let output = ctx
        .scratch
        .mkdirs(&format!("{project}/odd-output"), 0o700, TESTER)?;
    ctx.scratch.write_file(
        &format!("{project}/configure.ac"),
        b"AC_INIT([anchor-fixture],[1.0])\nAC_CONFIG_FILES([Makefile])\nAC_OUTPUT\n",
        0o600,
        TESTER,
    )?;
    ctx.scratch.write_file(
        &format!("{project}/Makefile.in"),
        b"all:\n\t@true\n",
        0o600,
        TESTER,
    )?;
    let autoconf = Command::new(ctx.bin_dir.join("autoconf"))
        .current_dir(&source)
        .env_clear()
        .env("PATH", format!("{}:/bin", ctx.bin_dir.display()))
        .env("HOME", "/home/tester")
        .env("LANG", "C.UTF-8")
        .uid(TESTER.uid)
        .gid(TESTER.gid)
        .output()
        .map_err(|error| format!("run pinned Autoconf for {project}: {error}"))?;
    ensure!(
        autoconf.status.success(),
        "pinned Autoconf failed for {project}: {}",
        String::from_utf8_lossy(&autoconf.stderr)
    );
    let configure = Command::new(source.join("configure"))
        .current_dir(&output)
        .env_clear()
        .env("PATH", format!("{}:/bin", ctx.bin_dir.display()))
        .env("HOME", "/home/tester")
        .env("LANG", "C.UTF-8")
        .uid(TESTER.uid)
        .gid(TESTER.gid)
        .output()
        .map_err(|error| format!("run generated configure for {project}: {error}"))?;
    ensure!(
        configure.status.success(),
        "generated configure failed for {project}: {}",
        String::from_utf8_lossy(&configure.stderr)
    );
    ensure!(
        output.join("config.status").is_file()
            && output.join("config.log").is_file()
            && output.join("Makefile").is_file(),
        "Autoconf did not generate the expected output layout"
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

/// The built-in identifies a real Autoconf 2.73 build and applies its idle
/// floor. A no-floor equivalent reclaims only the build with surviving
/// configure.ac; the oracle checks the entire container filesystem.
pub fn autotools_generated_output_uses_surviving_source_anchor(ctx: &Ctx) -> TestResult {
    for project in ["autotools-app", "autotools-missing", "autotools-checkout"] {
        setup_autotools(ctx, project)?;
    }
    ctx.scratch.remove_file("autotools-missing/configure.ac")?;
    ctx.scratch
        .mkdirs("autotools-checkout/odd-output/.git", 0o700, TESTER)?;
    ctx.scratch.write_file(
        "autotools-checkout/odd-output/.git/HEAD",
        b"ref: refs/heads/main\n",
        0o600,
        TESTER,
    )?;
    ctx.home
        .write_file("diskgc.yaml", TEST_PROFILE.as_bytes(), 0o644, TESTER)?;

    let eligible = ctx.scratch.path("autotools-app/odd-output");
    let missing = ctx.scratch.path("autotools-missing/odd-output");
    let checkout = ctx.scratch.path("autotools-checkout/odd-output");
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
            "profile:autotools-build-anchored",
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
                    && entry["build_evidence"]["marker"].as_str() == Some("config.status")
                    && entry["build_evidence"]["source"].as_str() == Some("/tmp/autotools-app")
            })),
        "built-in Autotools anchor or idle floor not observed: {plan}"
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
            "profile:autotools-anchor-kernel",
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
        "Autotools estimate differs from du ({expected_bytes} bytes): {measured_plan}"
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
            "profile:autotools-anchor-kernel",
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
        "Autotools reclaim selected unexpected output: {outcome}"
    );
    ensure!(!eligible.exists(), "eligible Autotools output remains");
    ensure!(
        missing.join("Makefile").exists(),
        "missing-source output was removed"
    );
    ensure!(
        checkout.join(".git/HEAD").exists(),
        "checkout output was removed"
    );
    verdict.assert_clean()
}
