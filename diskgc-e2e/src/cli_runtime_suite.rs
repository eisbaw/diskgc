//! Runtime CLI option probes against the real binary in disposable containers.

use std::thread;
use std::time::Duration;

use crate::ensure_suite::{ensure_run, report};
use crate::holder::Holder;
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, HOST_LOCK, TestResult};
use crate::{TESTER, ensure};

/// A short timeout must expire against the held host lock, while a bare
/// `--timeout 0` waits until that same lock is released and continues.
pub fn timeout_zero_waits_for_a_real_host_lock(ctx: &Ctx) -> TestResult {
    ctx.scratch.mkdir("cli-runtime-empty", 0o755, TESTER)?;
    ctx.scratch
        .write_file("cli-runtime-empty/payload", &[7; 8192], 0o644, TESTER)?;
    ctx.home.write_file(
        "cli-runtime-empty.yaml",
        b"version: 1\nprofiles:\n  - name: cli-runtime-empty\n    find: {roots: [/tmp/cli-runtime-empty], paths: [absent], type: dir}\n    action: delete\n    examples: {match: [/tmp/cli-runtime-empty/absent]}\n",
        0o600,
        TESTER,
    )?;
    let oracle = Oracle::arm(Fixture::new())?;
    let holder = Holder::root(ctx, &["flock-hold", HOST_LOCK])?;
    let short = ensure_run(
        ctx,
        &[
            "--json",
            "--config",
            "/home/tester/cli-runtime-empty.yaml",
            "--min-free",
            "100%",
            "--timeout",
            "1s",
            "--select",
            "profile:cli-runtime-empty",
            "/tmp",
        ],
    )?;
    short.expect_code(75)?;
    ensure!(
        report(&short)?["status"] == "lock_timeout",
        "short timeout did not expire against the held host lock: {}",
        short.stdout
    );
    let run = ctx.spawn_as_tester(
        &ctx.diskgc(),
        &[
            "ensure",
            "--json",
            "--config",
            "/home/tester/cli-runtime-empty.yaml",
            "--min-free",
            "100%",
            "--timeout",
            "0",
            "--select",
            "profile:cli-runtime-empty",
            "/tmp",
        ],
    )?;
    thread::sleep(Duration::from_millis(1200));
    holder.finish()?;
    let run = run.wait()?;
    oracle.judge_runs(&[&short, &run])?.assert_clean()?;
    run.expect_code(1)?;
    let result = report(&run)?;
    ensure!(
        result["status"] == "exhausted"
            && result["actions"].as_array().is_some_and(Vec::is_empty)
            && run.took >= Duration::from_secs(1),
        "--timeout 0 did not wait through the held lock: {result}; took {:?}",
        run.took
    );
    Ok(())
}

/// Bare `--tally-budget 0` must be accepted by the real CLI and run the
/// unrestricted tally and reclaim path on an eligible candidate.
pub fn tally_budget_zero_reaches_reclaim(ctx: &Ctx) -> TestResult {
    let candidate = ctx.scratch.mkdir("cli-runtime-budget", 0o755, TESTER)?;
    ctx.scratch
        .write_file("cli-runtime-budget/payload", &[3; 8192], 0o644, TESTER)?;
    ctx.scratch
        .write_file("cli-runtime-budget-pad", &[9; 8192], 0o644, TESTER)?;
    ctx.home.write_file(
        "cli-runtime-budget.yaml",
        b"version: 1\nprofiles:\n  - name: cli-runtime-budget\n    find: {roots: [/tmp], paths: [cli-runtime-budget], type: dir}\n    action: delete\n    examples: {match: [/tmp/cli-runtime-budget]}\n",
        0o600,
        TESTER,
    )?;
    let oracle = Oracle::arm(Fixture::new().eligible(candidate.clone()))?;
    let run = ensure_run(
        ctx,
        &[
            "--json",
            "--config",
            "/home/tester/cli-runtime-budget.yaml",
            "--min-free",
            "100%",
            "--tally-budget",
            "0",
            "--select",
            "profile:cli-runtime-budget",
            "/tmp",
        ],
    )?;
    oracle.judge(&run)?.assert_clean()?;
    run.expect_code(1)?;
    let result = report(&run)?;
    ensure!(
        result["status"] == "exhausted"
            && result["actions"].as_array().is_some_and(|actions| {
                actions.len() == 1
                    && actions[0]["candidate"] == candidate.to_string_lossy().as_ref()
                    && actions[0]["profile"] == "cli-runtime-budget"
            })
            && !candidate.exists(),
        "--tally-budget 0 did not complete the real reclaim: {result}"
    );
    Ok(())
}

/// Recreate an equivalent fixture between runs, so both options pass through
/// the real reclaim path and the deletion oracle independently checks each.
pub fn no_index_matches_reclaim_on_fresh_fixture(ctx: &Ctx) -> TestResult {
    ctx.scratch
        .write_file("cli-runtime-index-pad", &[9; 8192], 0o644, TESTER)?;
    ctx.home.write_file(
        "cli-runtime-index.yaml",
        b"version: 1\nprofiles:\n  - name: cli-runtime-index\n    find: {roots: [/tmp], paths: [cli-runtime-index], type: dir}\n    action: delete\n    examples: {match: [/tmp/cli-runtime-index]}\n",
        0o600,
        TESTER,
    )?;
    let candidate = ctx.scratch.path("cli-runtime-index");
    let common = [
        "--json",
        "--config",
        "/home/tester/cli-runtime-index.yaml",
        "--min-free",
        "100%",
        "--select",
        "profile:cli-runtime-index",
        "/tmp",
    ];
    let mut outcomes = Vec::new();
    for no_index in [false, true] {
        ctx.scratch.mkdir("cli-runtime-index", 0o755, TESTER)?;
        ctx.scratch
            .write_file("cli-runtime-index/payload", &[5; 8192], 0o644, TESTER)?;
        let oracle = Oracle::arm(Fixture::new().eligible(candidate.clone()))?;
        let mut args = Vec::from(common);
        if no_index {
            args.insert(0, "--no-index");
        }
        let run = ensure_run(ctx, &args)?;
        oracle.judge(&run)?.assert_clean()?;
        run.expect_code(1)?;
        let result = report(&run)?;
        ensure!(
            result["status"] == "exhausted"
                && result["actions"].as_array().is_some_and(|actions| {
                    actions.len() == 1
                        && actions[0]["candidate"] == candidate.to_string_lossy().as_ref()
                        && actions[0]["tier"] == "select"
                        && actions[0]["profile"] == "cli-runtime-index"
                })
                && result["errors"].as_array().is_some_and(Vec::is_empty)
                && !candidate.exists(),
            "reclaim with no_index={no_index} failed: {result}"
        );
        outcomes.push(result);
    }
    let normal = &outcomes[0];
    let no_index = &outcomes[1];
    ensure!(
        normal["status"] == no_index["status"]
            && normal["actions"][0]["tier"] == no_index["actions"][0]["tier"]
            && normal["actions"][0]["profile"] == no_index["actions"][0]["profile"]
            && normal["actions"][0]["candidate"] == no_index["actions"][0]["candidate"]
            && normal["skipped"] == no_index["skipped"]
            && normal["errors"] == no_index["errors"]
            && normal["index"] == no_index["index"],
        "--no-index changed reclaim semantics: normal {normal}; no-index {no_index}"
    );
    Ok(())
}
