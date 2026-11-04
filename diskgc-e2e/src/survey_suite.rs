//! `survey` (TASK-38; docs/reference.md, `survey`) against the real binary
//! in a disposable container: read-only over a laptop-shaped `/tmp`
//! fixture while another process holds the host lock, once without and
//! once with `--in-use`. It must report each profile's candidates (not
//! checked, then eligible) and the unmatched name families, finish
//! without waiting for the lock, delete or rename nothing (no traced
//! removal call, oracle clean) and write no index snapshot.

use std::path::Path;

use serde_json::Value;

use crate::holder::Holder;
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, HOST_LOCK, TestResult};
use crate::{TESTER, ensure, info};

/// The fixture below `/tmp` (relative names) the survey must see.
const PYTEST_SESSION: &str = "nix-shell.Ab12Cd/pytest-of-tester/pytest-0";
const RC_DIR: &str = "nix-shell-4242-17";
const SHELL_TMPS: [&str; 3] = ["tmp.Xy12ab", "tmp.Qw34er", "tmp.Zz56zz"];
const CLAUDE_ROOT: &str = "claude-1000";

pub fn survey_reports_tmp_by_profile_and_family_and_changes_nothing(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdirs(&format!("{PYTEST_SESSION}/test_a0"), 0o700, TESTER)?;
    s.write_file(
        &format!("{PYTEST_SESSION}/test_a0/out"),
        &[1; 64 * 1024],
        0o600,
        TESTER,
    )?;
    s.mkdirs(RC_DIR, 0o700, TESTER)?;
    for file in ["rc", "env-vars"] {
        s.write_file(&format!("{RC_DIR}/{file}"), b"export X=1\n", 0o600, TESTER)?;
    }
    for (n, dir) in SHELL_TMPS.iter().enumerate() {
        s.mkdirs(dir, 0o700, TESTER)?;
        s.write_file(
            &format!("{dir}/payload"),
            &vec![2; (n + 1) * 32 * 1024],
            0o600,
            TESTER,
        )?;
    }
    s.mkdirs(
        &format!("{CLAUDE_ROOT}/-home-tester-project"),
        0o700,
        TESTER,
    )?;
    s.write_file(
        &format!("{CLAUDE_ROOT}/-home-tester-project/task.output"),
        b"agent output",
        0o600,
        TESTER,
    )?;

    // Nothing may change: the oracle is armed with no eligible path.
    let oracle = Oracle::arm(Fixture::new())?;
    // A reclaim waits for this lock; a survey must not even ask for it.
    let holder = Holder::root(ctx, &["flock-hold", HOST_LOCK])?;
    let checked = ctx.run_as_tester(
        &ctx.diskgc(),
        &["survey", "--json", "--in-use", "--timeout", "60s", "/tmp"],
    )?;
    let run = ctx.run_as_tester(
        &ctx.diskgc(),
        &["survey", "--json", "--timeout", "60s", "/tmp"],
    )?;
    holder.finish()?;
    info!(
        "diskgc survey --in-use -> {:?}\n--- stdout\n{}--- stderr\n{}",
        checked.code, checked.stdout, checked.stderr
    );
    info!(
        "diskgc survey -> {:?}\n--- stdout\n{}--- stderr\n{}",
        run.code, run.stdout, run.stderr
    );
    oracle.judge_runs(&[&checked, &run])?.assert_clean()?;
    for r in [&checked, &run] {
        ensure!(
            r.events.is_empty(),
            "survey made removal or rename calls: {:?}",
            r.events
        );
        r.expect_code(0)?;
    }
    // With --in-use: the session is checked (no process holds it).
    let with: Value = serde_json::from_str(&checked.stdout)
        .map_err(|e| format!("survey --in-use JSON: {e}: {}", checked.stdout))?;
    let session = with["paths"][0]["summary"]
        .as_array()
        .and_then(|b| b.iter().find(|b| b["profile"] == "pytest-tmp"))
        .ok_or_else(|| format!("no pytest-tmp block under --in-use: {with}"))?;
    ensure!(
        with["in_use"] == "checked"
            && session["eligible"]["count"] == 1
            && session["not_checked"].is_null(),
        "under --in-use the idle session is eligible: {session}"
    );
    let survey: Value = serde_json::from_str(&run.stdout)
        .map_err(|e| format!("survey JSON: {e}: {}", run.stdout))?;
    ensure!(
        survey["schema_version"] == 2
            && survey["status"] == "dry_run"
            && survey["in_use"] == "not_checked",
        "unexpected survey: {survey}"
    );
    let tmp = &survey["paths"][0];
    ensure!(tmp["path"] == "/tmp", "first path is not /tmp: {tmp}");

    let block = |profile: &str| -> Result<&Value, String> {
        tmp["summary"]
            .as_array()
            .and_then(|blocks| blocks.iter().find(|b| b["profile"] == profile))
            .ok_or_else(|| format!("no summary block for {profile}: {tmp}"))
    };
    ensure!(
        block("pytest-tmp")?["found"] == 1
            && block("pytest-tmp")?["not_checked"]["count"] == 1
            && block("pytest-tmp")?["eligible"].is_null(),
        "the nested pytest session was not found, or not as not checked: {}",
        block("pytest-tmp")?
    );
    ensure!(
        block("nix-shell-rc")?["found"] == 1,
        "the rc dir was not found: {}",
        block("nix-shell-rc")?
    );
    let candidates: Vec<&str> = tmp["candidates"]
        .as_array()
        .ok_or("candidates is not an array")?
        .iter()
        .filter_map(|c| c["candidate"].as_str())
        .collect();
    for want in [PYTEST_SESSION, RC_DIR] {
        let path = s.path(want);
        ensure!(
            candidates.contains(&path.to_string_lossy().as_ref()),
            "{} is not listed: {candidates:?}",
            path.display()
        );
    }

    let families = tmp["unmatched"]["families"]
        .as_array()
        .ok_or("unmatched.families is not an array")?;
    let family = |name: &str| families.iter().find(|f| f["family"] == name);
    let shell = family("tmp.*").ok_or_else(|| format!("no tmp.* family: {families:?}"))?;
    ensure!(
        shell["count"] == 3
            && shell["unsized"] == 0
            && shell["estimated_bytes"].as_u64() >= Some(6 * 32 * 1024)
            && shell["examples"][0] == s.path(SHELL_TMPS[2]).to_string_lossy().as_ref(),
        "tmp.* is not the three shell temp dirs, largest first: {shell}"
    );
    ensure!(
        family("claude-#").is_some_and(|f| f["count"] == 1),
        "no claude-# family of one: {families:?}"
    );
    ensure!(
        family("nix-shell.*").is_none() && family("nix-shell-#-#").is_none(),
        "an entry holding a candidate is not unmatched: {families:?}"
    );

    // No index snapshot: a survey neither reads nor writes one.
    let state = Path::new("/home/tester/.local/state/diskgc");
    if let Ok(entries) = std::fs::read_dir(state) {
        let written: Vec<String> = entries
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("index-"))
            .collect();
        ensure!(written.is_empty(), "survey wrote a snapshot: {written:?}");
    }
    for rel in [
        format!("{PYTEST_SESSION}/test_a0/out"),
        format!("{RC_DIR}/rc"),
        format!("{CLAUDE_ROOT}/-home-tester-project/task.output"),
    ] {
        ensure!(s.path(&rel).exists(), "{rel} vanished");
    }
    Ok(())
}
