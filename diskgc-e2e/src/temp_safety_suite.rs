//! A live process can change TMPDIR after exec. Linux exposes only its
//! initial environment through /proc, so an idle temporary directory with
//! no open descriptor is not recognizable as active. This container test
//! keeps the unsafe pattern as a user-configured dry-run probe and requires
//! the broad built-ins to stay out of the shipped library.

use crate::ensure_suite::{ensure_run, report};
use crate::holder::Holder;
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, TestResult};
use crate::{TESTER, ensure};

pub fn dormant_exported_tmpdir_is_invisible_and_broad_builtins_are_absent(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("tmp.active", 0o700, TESTER)?;
    s.write_file("tmp.active/scratch", b"live session data", 0o600, TESTER)?;
    let holder = Holder::tester(ctx, &["hold-export", "TMPDIR", "/tmp/tmp.active"], &[])?;
    s.mkdirs("claude-1000/nix-shell.active", 0o700, TESTER)?;
    s.write_file(
        "claude-1000/nix-shell.active/scratch",
        b"live shell data",
        0o600,
        TESTER,
    )?;
    let nested_holder = Holder::tester(
        ctx,
        &["hold-export", "TMPDIR", "/tmp/claude-1000/nix-shell.active"],
        &[],
    )?;
    let environ = std::fs::read(format!("/proc/{}/environ", holder.pid()))
        .map_err(|e| format!("read holder's initial environment: {e}"))?;
    ensure!(
        !environ.windows(b"TMPDIR=".len()).any(|w| w == b"TMPDIR="),
        "fixture's TMPDIR unexpectedly appeared in /proc environ"
    );

    ctx.home.write_file(
        "temp-probe.yaml",
        b"version: 1\nprofiles:\n  - name: shell-temp-probe\n    find: {roots: [/tmp], paths: ['tmp.*'], type: dir}\n    action: delete\n    examples: {match: [/tmp/tmp.active]}\n  - name: nested-shell-temp-probe\n    find: {roots: [/tmp], paths: ['claude-1000/nix-shell.*'], type: dir}\n    action: delete\n    examples: {match: [/tmp/claude-1000/nix-shell.active]}\n",
        0o600,
        TESTER,
    )?;
    let oracle = Oracle::arm(Fixture::new())?;
    let list = ctx.run_as_tester(&ctx.diskgc(), &["profiles", "list", "--json"])?;
    let dry = ensure_run(
        ctx,
        &[
            "--dry-run",
            "--json",
            "--no-index",
            "--config",
            "/home/tester/temp-probe.yaml",
            "--min-free",
            "100%",
            "--select",
            "profile:shell-temp-probe",
            "/tmp",
        ],
    )?;
    let nested_dry = ensure_run(
        ctx,
        &[
            "--dry-run",
            "--json",
            "--no-index",
            "--config",
            "/home/tester/temp-probe.yaml",
            "--min-free",
            "100%",
            "--select",
            "profile:nested-shell-temp-probe",
            "/tmp",
        ],
    )?;
    let verdict = oracle.judge_runs(&[&list, &dry, &nested_dry])?;
    list.expect_code(0)?;
    dry.expect_code(0)?;
    nested_dry.expect_code(0)?;
    let plan = report(&dry)?;
    ensure!(
        plan["plan"].as_array().is_some_and(|entries| entries
            .iter()
            .any(|entry| { entry["candidate"] == "/tmp/tmp.active" && entry["reason"].is_null() })),
        "post-exec TMPDIR should expose the known false negative: {plan}"
    );
    let nested_plan = report(&nested_dry)?;
    ensure!(
        nested_plan["plan"]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|entry| {
                entry["candidate"] == "/tmp/claude-1000/nix-shell.active"
                    && entry["reason"].is_null()
            })),
        "Claude's nested shell TMPDIR should expose the same false negative: {nested_plan}"
    );
    let listed = report(&list)?;
    let names: Vec<&str> = listed["profiles"]
        .as_array()
        .ok_or("profiles list has no profiles array")?
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    for unsafe_name in [
        "mktemp-dir",
        "nix-shell-tmpdir",
        "nix-shell-rc",
        "tmp-entry",
    ] {
        ensure!(
            !names.contains(&unsafe_name),
            "unsafe broad temp profile {unsafe_name} is shipped"
        );
    }
    ensure!(
        s.path("tmp.active/scratch").exists(),
        "dry-run touched the live scratch file"
    );
    ensure!(
        s.path("claude-1000/nix-shell.active/scratch").exists(),
        "dry-run touched the live nested shell"
    );
    holder.finish()?;
    nested_holder.finish()?;
    verdict.assert_clean()
}

/// The shipped exact cache profile reaches an entry in Claude's nested
/// TMPDIR while leaving the enclosing shell and neighboring data intact.
pub fn nested_claude_shell_cache_reclaims_only_its_exact_entry(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdirs(
        "claude-1000/nix-shell.Ab1/node-compile-cache",
        0o700,
        TESTER,
    )?;
    s.write_file(
        "claude-1000/nix-shell.Ab1/node-compile-cache/payload",
        &[0x71; 8192],
        0o600,
        TESTER,
    )?;
    s.write_file(
        "claude-1000/nix-shell.Ab1/important",
        b"keep this shell data",
        0o600,
        TESTER,
    )?;
    let candidate = s.path("claude-1000/nix-shell.Ab1/node-compile-cache");
    let parent = s.path("claude-1000/nix-shell.Ab1");
    let protected = s.path("claude-1000/nix-shell.Ab1/important");
    let oracle = Oracle::arm(Fixture::new().eligible(candidate.clone()))?;
    let args = [
        "--json",
        "--no-index",
        "--min-free",
        "100%",
        "--select",
        "profile:node-compile-cache",
        "/tmp",
    ];
    let mut dry_args = vec!["--dry-run"];
    dry_args.extend(args);
    let dry = ensure_run(ctx, &dry_args)?;
    let run = ensure_run(ctx, &args)?;
    let verdict = oracle.judge_runs(&[&dry, &run])?;
    dry.expect_code(0)?;
    run.expect_code(1)?;
    let plan = report(&dry)?;
    ensure!(
        plan["plan"]
            .as_array()
            .is_some_and(|entries| entries.len() == 1
                && entries[0]["candidate"].as_str() == candidate.to_str()
                && entries[0]["reason"].is_null()),
        "nested cache plan selected the wrong entry: {plan}"
    );
    let result = report(&run)?;
    ensure!(
        result["actions"]
            .as_array()
            .is_some_and(|actions| actions.len() == 1
                && actions[0]["candidate"].as_str() == candidate.to_str()),
        "nested cache reclaim acted on the wrong entry: {result}"
    );
    ensure!(!candidate.exists(), "exact nested cache survived reclaim");
    ensure!(
        parent.is_dir() && protected.exists(),
        "enclosing shell changed"
    );
    verdict.assert_clean()
}
