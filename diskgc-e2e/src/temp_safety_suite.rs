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
    let environ = std::fs::read(format!("/proc/{}/environ", holder.pid()))
        .map_err(|e| format!("read holder's initial environment: {e}"))?;
    ensure!(
        !environ.windows(b"TMPDIR=".len()).any(|w| w == b"TMPDIR="),
        "fixture's TMPDIR unexpectedly appeared in /proc environ"
    );

    ctx.home.write_file(
        "temp-probe.yaml",
        b"version: 1\nprofiles:\n  - name: shell-temp-probe\n    find: {roots: [/tmp], paths: ['tmp.*'], type: dir}\n    action: delete\n    examples: {match: [/tmp/tmp.active]}\n",
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
    let verdict = oracle.judge_runs(&[&list, &dry])?;
    list.expect_code(0)?;
    dry.expect_code(0)?;
    let plan = report(&dry)?;
    ensure!(
        plan["plan"].as_array().is_some_and(|entries| entries
            .iter()
            .any(|entry| { entry["candidate"] == "/tmp/tmp.active" && entry["reason"].is_null() })),
        "post-exec TMPDIR should expose the known false negative: {plan}"
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
    holder.finish()?;
    verdict.assert_clean()
}
