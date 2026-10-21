//! CLI contract probes against the real binary in disposable containers.

use std::path::Path;

use serde_json::Value;

use crate::ensure_suite::{ensure_run, report};
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, TestResult};
use crate::{TESTER, ensure};

/// Full ensure must reject a symlink at PATH itself or an intermediate
/// component before considering reclaim. Neither case may mutate the target.
pub fn ensure_refuses_symlinked_path_without_mutation(ctx: &Ctx) -> TestResult {
    ctx.scratch.mkdir("cli-path-real", 0o755, TESTER)?;
    ctx.scratch.mkdir("cli-path-real/child", 0o755, TESTER)?;
    let sentinel =
        ctx.scratch
            .write_file("cli-path-real/child/sentinel", b"keep", 0o644, TESTER)?;
    ctx.scratch
        .symlink("cli-path-final", Path::new("/tmp/cli-path-real"))?;
    ctx.scratch
        .symlink("cli-path-prefix", Path::new("/tmp/cli-path-real"))?;

    let oracle = Oracle::arm(Fixture::new())?;
    let mut runs = Vec::new();
    for path in ["/tmp/cli-path-final", "/tmp/cli-path-prefix/child"] {
        let run = ensure_run(
            ctx,
            &[
                "--json",
                "--min-free",
                "100%",
                "--select",
                "profile:cargo-target",
                "--roots",
                "/tmp/cli-path-real",
                path,
            ],
        )?;
        runs.push((path, run));
    }
    oracle
        .judge_runs(&runs.iter().map(|(_, run)| run).collect::<Vec<_>>())?
        .assert_clean()?;
    for (path, run) in &runs {
        run.expect_code(2)?;
        let result = report(run)?;
        ensure!(
            result["status"] == "usage_error"
                && result["actions"].as_array().is_some_and(Vec::is_empty)
                && result["errors"][0]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("unsafe_path")),
            "unsafe PATH {path} was not refused before reclaim: {result}"
        );
    }
    ensure!(
        std::fs::read(&sentinel).map_err(|error| format!("read {sentinel:?}: {error}"))? == b"keep",
        "unsafe PATH changed its target"
    );
    Ok(())
}

/// An explicit selection replaces the configured group graph; the user
/// profiles still load from that config. Only the selected candidate may go.
pub fn select_replaces_configured_groups(ctx: &Ctx) -> TestResult {
    let configured = ctx.scratch.mkdir("cli-configured", 0o755, TESTER)?;
    let selected = ctx.scratch.mkdir("cli-selected", 0o755, TESTER)?;
    ctx.scratch
        .write_file("cli-configured/payload", &[1; 8192], 0o644, TESTER)?;
    ctx.scratch
        .write_file("cli-selected/payload", &[2; 8192], 0o644, TESTER)?;
    ctx.home.write_file(
        "cli-select.yaml",
        b"version: 1\nprofiles:\n  - name: cli-configured\n    find: {roots: [/tmp], paths: [cli-configured], type: dir}\n    action: delete\n    examples: {match: [/tmp/cli-configured]}\n  - name: cli-selected\n    find: {roots: [/tmp], paths: [cli-selected], type: dir}\n    action: delete\n    examples: {match: [/tmp/cli-selected]}\ngroups:\n  - name: configured-group\n    tiers:\n      - name: configured-tier\n        select: profile:cli-configured\n",
        0o600,
        TESTER,
    )?;
    let oracle = Oracle::arm(Fixture::new().eligible(selected.clone()))?;
    let run = ensure_run(
        ctx,
        &[
            "--json",
            "--config",
            "/home/tester/cli-select.yaml",
            "--min-free",
            "100%",
            "--timeout",
            "10s",
            "--select",
            "profile:cli-selected",
            "/tmp",
        ],
    )?;
    oracle.judge(&run)?.assert_clean()?;
    run.expect_code(1)?;
    let result = report(&run)?;
    let actions = result["actions"]
        .as_array()
        .ok_or("actions is not an array")?;
    ensure!(
        actions.len() == 1
            && actions[0]["candidate"] == selected.to_string_lossy().as_ref()
            && actions[0]["tier"] == "--select"
            && actions[0]["profile"] == "cli-selected",
        "--select did not replace configured groups: {result}"
    );
    ensure!(
        configured.join("payload").exists() && !selected.exists(),
        "configured group ran or selected candidate survived"
    );
    Ok(())
}

/// Help takes precedence even over JSON and an otherwise invalid ensure
/// invocation. Its output is the human help text, never a JSON object.
pub fn json_help_precedence_is_text(ctx: &Ctx) -> TestResult {
    let oracle = Oracle::arm(Fixture::new())?;
    let first = ensure_run(ctx, &["--json", "--help"])?;
    let second = ensure_run(ctx, &["--min-free", "not-a-size", "--json", "-h"])?;
    oracle.judge_runs(&[&first, &second])?.assert_clean()?;
    for run in [&first, &second] {
        run.expect_code(0)?;
        ensure!(
            run.stdout.starts_with("usage: diskgc ensure")
                && run.stdout.contains("--json prints exactly one JSON object")
                && serde_json::from_str::<Value>(&run.stdout).is_err(),
            "--help did not win over --json: {:?}",
            run.stdout
        );
    }
    Ok(())
}

/// Before task-29 adds an index, --no-index has identical candidate and
/// report semantics. Both dry runs must discover and plan the real fixture.
pub fn no_index_matches_current_dry_run_plan(ctx: &Ctx) -> TestResult {
    let candidate = ctx.scratch.mkdir("cli-no-index", 0o755, TESTER)?;
    ctx.scratch
        .write_file("cli-no-index/payload", &[4; 8192], 0o644, TESTER)?;
    ctx.home.write_file(
        "cli-no-index.yaml",
        b"version: 1\nprofiles:\n  - name: cli-no-index\n    find: {roots: [/tmp], paths: [cli-no-index], type: dir}\n    action: delete\n    examples: {match: [/tmp/cli-no-index]}\n",
        0o600,
        TESTER,
    )?;
    let common = [
        "--dry-run",
        "--json",
        "--config",
        "/home/tester/cli-no-index.yaml",
        "--min-free",
        "100%",
        "--select",
        "profile:cli-no-index",
        "/tmp",
    ];
    let oracle = Oracle::arm(Fixture::new())?;
    let normal = ensure_run(ctx, &common)?;
    let mut without_index = vec!["--no-index"];
    without_index.extend(common);
    let no_index = ensure_run(ctx, &without_index)?;
    oracle.judge_runs(&[&normal, &no_index])?.assert_clean()?;
    normal.expect_code(0)?;
    no_index.expect_code(0)?;
    let a = report(&normal)?;
    let b = report(&no_index)?;
    ensure!(
        a["status"] == "dry_run"
            && a["plan"].as_array().is_some_and(|plan| {
                plan.len() == 1
                    && plan[0]["candidate"] == candidate.to_string_lossy().as_ref()
                    && plan[0]["eligible"].as_bool() == Some(true)
            })
            && a["plan"] == b["plan"]
            && a["skipped"] == b["skipped"]
            && a["actions"] == b["actions"]
            && a["index"] == b["index"]
            && a["mount"] == b["mount"]
            && a["path"] == b["path"],
        "--no-index changed dry-run planning: normal {a}; no-index {b}"
    );
    ensure!(
        candidate.join("payload").exists(),
        "dry-run removed candidate"
    );
    Ok(())
}
