//! CLI contract probes against the real binary in disposable containers.

use std::path::Path;

use serde_json::Value;

use crate::ensure_suite::{ensure_run, report};
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, TestResult};
use crate::under_test;
use crate::{TESTER, ensure};

fn expect_reclaim_result(run: &under_test::Run, result: &Value) -> TestResult {
    run.expect_code(1)?;
    ensure!(
        result["status"] == "exhausted",
        "reclaim did not exhaust below the pressure goal: {result}"
    );
    Ok(())
}

/// Without a selected profile, ensure needs an installed policy. The
/// disposable test home and image have neither user nor system config.
pub fn ensure_without_config_or_select_is_usage_error(ctx: &Ctx) -> TestResult {
    let oracle = Oracle::arm(Fixture::new())?;
    let run = ensure_run(ctx, &["--json", "--min-free", "100%", "/tmp"])?;
    oracle.judge(&run)?.assert_clean()?;
    run.expect_code(2)?;
    let result = report(&run)?;
    ensure!(
        result["status"] == "usage_error"
            && result["actions"].as_array().is_some_and(Vec::is_empty)
            && result["errors"][0]["message"]
                .as_str()
                .is_some_and(|message| message.contains("no configuration file found")
                    && message.contains("ensure without --select needs one")),
        "ensure without a policy was not refused: {result}"
    );
    Ok(())
}

/// An explicit or environment-selected config is authoritative even when
/// missing. A malformed existing config has the same static error in check
/// and ensure, before the latter can reclaim an otherwise eligible entry.
pub fn invalid_config_never_falls_back_or_reclaims(ctx: &Ctx) -> TestResult {
    ctx.scratch.mkdir("cli-config-guard", 0o755, TESTER)?;
    let candidate = ctx.scratch.mkdir("cli-config-guard/cache", 0o755, TESTER)?;
    ctx.scratch
        .write_file("cli-config-guard/cache/payload", &[7; 8192], 0o644, TESTER)?;
    let root = ctx.scratch.path("cli-config-guard");
    ctx.home.mkdir(".config", 0o755, TESTER)?;
    ctx.home.mkdir(".config/diskgc", 0o755, TESTER)?;
    let fallback = format!(
        "version: 1\nprofiles:\n  - name: cli-config-cache\n    find: {{roots: [{}], paths: [cache], type: dir}}\n    action: delete\n    examples: {{match: [{}/cache]}}\ngroups:\n  - name: reclaim\n    tiers:\n      - name: cache\n        select: profile:cli-config-cache\n",
        root.display(),
        root.display()
    );
    ctx.home.write_file(
        ".config/diskgc/config.yaml",
        fallback.as_bytes(),
        0o600,
        TESTER,
    )?;

    let missing = "/home/tester/misspelled-config.yaml";
    for (source, env, flag) in [
        ("environment", Some(missing), None),
        (
            "flag",
            Some("/home/tester/.config/diskgc/config.yaml"),
            Some(missing),
        ),
    ] {
        let env = env
            .map(|value| vec![("DISKGC_CONFIG".to_owned(), value.to_owned())])
            .unwrap_or_default();
        let mut check_args = vec!["check"];
        let mut ensure_args = vec!["ensure", "--json", "--min-free", "100%"];
        if let Some(path) = flag {
            check_args.extend(["--config", path]);
            ensure_args.extend(["--config", path]);
        }
        ensure_args.push("/tmp");
        let oracle = Oracle::arm(Fixture::new())?;
        let checked = under_test::run_as_tester_with_env(
            &ctx.ledger,
            &ctx.records,
            Path::new("/"),
            &env,
            &ctx.diskgc(),
            &check_args,
        )?;
        oracle.judge(&checked)?.assert_clean()?;
        checked.expect_code(2)?;
        ensure!(
            checked.stderr.contains(missing),
            "{source} missing config was not reported: {}",
            checked.stderr
        );

        let oracle = Oracle::arm(Fixture::new())?;
        let run = under_test::run_as_tester_with_env(
            &ctx.ledger,
            &ctx.records,
            Path::new("/"),
            &env,
            &ctx.diskgc(),
            &ensure_args,
        )?;
        oracle.judge(&run)?.assert_clean()?;
        run.expect_code(2)?;
        ensure!(
            report(&run)?["errors"][0]["message"]
                .as_str()
                .is_some_and(|message| message.contains(missing))
                && candidate.exists(),
            "{source} missing config fell back or reclaimed: {}",
            run.stdout
        );
    }

    let invalid = "/home/tester/invalid-config.yaml";
    ctx.home.write_file(
        "invalid-config.yaml",
        b"version: 1\nunknown_field: true\n",
        0o600,
        TESTER,
    )?;
    let oracle = Oracle::arm(Fixture::new())?;
    let checked = under_test::run_as_tester(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        &ctx.diskgc(),
        &["check", "--config", invalid],
    )?;
    oracle.judge(&checked)?.assert_clean()?;
    checked.expect_code(2)?;
    let check_error = checked
        .stderr
        .trim()
        .strip_prefix("diskgc: error: ")
        .ok_or_else(|| format!("check did not report a config error: {}", checked.stderr))?;
    ensure!(
        check_error.contains("unknown_field"),
        "check missed the invalid key: {check_error}"
    );

    let oracle = Oracle::arm(Fixture::new())?;
    let run = under_test::run_as_tester(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        &ctx.diskgc(),
        &[
            "ensure",
            "--json",
            "--config",
            invalid,
            "--min-free",
            "100%",
            "/tmp",
        ],
    )?;
    oracle.judge(&run)?.assert_clean()?;
    run.expect_code(2)?;
    let result = report(&run)?;
    ensure!(
        result["errors"][0]["message"] == check_error
            && result["actions"].as_array().is_some_and(Vec::is_empty)
            && candidate.exists(),
        "ensure did not preserve check's error before reclaim: {result}"
    );

    // An owners list is valid config syntax, so check accepts it. Ordinary
    // ensure must reject that root-mode policy before acting on the candidate.
    let owners = "/home/tester/owners-config.yaml";
    ctx.home.write_file(
        "owners-config.yaml",
        format!("{fallback}owners: [tester]\n").as_bytes(),
        0o600,
        TESTER,
    )?;
    let oracle = Oracle::arm(Fixture::new())?;
    let checked = under_test::run_as_tester(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        &ctx.diskgc(),
        &["check", "--config", owners],
    )?;
    oracle.judge(&checked)?.assert_clean()?;
    checked.expect_code(0)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let run = under_test::run_as_tester(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        &ctx.diskgc(),
        &[
            "ensure",
            "--json",
            "--config",
            owners,
            "--min-free",
            "100%",
            "/tmp",
        ],
    )?;
    oracle.judge(&run)?.assert_clean()?;
    run.expect_code(2)?;
    let result = report(&run)?;
    ensure!(
        result["errors"][0]["message"] == "owners: is allowed only with --allow-root"
            && result["actions"].as_array().is_some_and(Vec::is_empty)
            && candidate.exists(),
        "ordinary ensure did not reject the root-mode policy before reclaim: {result}"
    );

    // The file's selected tier is valid; an explicit malformed selection
    // must still fail static invocation validation before reclaim.
    let oracle = Oracle::arm(Fixture::new())?;
    let run = under_test::run_as_tester(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        &ctx.diskgc(),
        &[
            "ensure",
            "--json",
            "--config",
            "/home/tester/.config/diskgc/config.yaml",
            "--min-free",
            "100%",
            "--select",
            "profile:cli-config-cache &&",
            "/tmp",
        ],
    )?;
    oracle.judge(&run)?.assert_clean()?;
    run.expect_code(2)?;
    let result = report(&run)?;
    ensure!(
        result["errors"][0]["message"]
            .as_str()
            .is_some_and(|message| message.contains("invalid selection"))
            && result["actions"].as_array().is_some_and(Vec::is_empty)
            && candidate.exists(),
        "malformed --select reached reclaim: {result}"
    );
    Ok(())
}

/// A negated filter containing a retention guard is a static error, even
/// when another predicate in that filter has known evidence. The real
/// check and ensure commands must agree before either version can be
/// reclaimed; one version is newest and selected by the current link.
pub fn negated_retention_filters_fail_before_reclaim(ctx: &Ctx) -> TestResult {
    let old = ctx
        .scratch
        .mkdirs("retention-contract/releases/v1", 0o700, TESTER)?;
    let current = ctx
        .scratch
        .mkdir("retention-contract/releases/v2", 0o700, TESTER)?;
    ctx.scratch.write_file(
        "retention-contract/releases/v1/payload",
        b"old",
        0o600,
        TESTER,
    )?;
    ctx.scratch.write_file(
        "retention-contract/releases/v2/payload",
        b"current",
        0o600,
        TESTER,
    )?;
    ctx.scratch
        .symlink("retention-contract/current", &current)?;
    for (filter, predicate) in [
        ("newest", "keep_newest"),
        ("current", "protect_links"),
        ("installed", "protect_cursor_extensions"),
        ("mixed", "keep_newest"),
    ] {
        let selection = format!("profile:retention-contract-release && !filter:{filter}");
        let config_name = format!("retention-contract-{filter}.yaml");
        let config_path = format!("/home/tester/{config_name}");
        let config = format!(
            "version: 1\nprofiles:\n  - name: retention-contract-release\n    find: {{roots: [/tmp], paths: ['retention-contract/releases/*'], type: dir}}\n    action: delete\n    examples: {{match: [/tmp/retention-contract/releases/v1]}}\n  - name: newest\n    where: {{keep_newest: 1}}\n  - name: current\n    where: {{protect_parent_link: current}}\n  - name: installed\n    where: {{protect_cursor_extensions: retention-contract/extensions.json}}\n  - name: mixed\n    where: {{has: payload, keep_newest: 1}}\ngroups:\n  - name: reclaim\n    tiers:\n      - name: releases\n        select: {selection}\n"
        );
        ctx.home
            .write_file(&config_name, config.as_bytes(), 0o600, TESTER)?;
        let oracle = Oracle::arm(Fixture::new())?;
        let checked = under_test::run_as_tester(
            &ctx.ledger,
            &ctx.records,
            Path::new("/"),
            &ctx.diskgc(),
            &["check", "--config", &config_path],
        )?;
        let run = ensure_run(
            ctx,
            &[
                "--json",
                "--config",
                &config_path,
                "--min-free",
                "100%",
                "/tmp",
            ],
        )?;
        oracle.judge_runs(&[&checked, &run])?.assert_clean()?;
        checked.expect_code(2)?;
        run.expect_code(2)?;
        let wanted = format!(
            "`!filter:{filter}` uses `{predicate}`, a retention guard that cannot be negated"
        );
        ensure!(
            checked.stderr.contains(&wanted),
            "check accepted or misreported {filter}: {}",
            checked.stderr
        );
        let result = report(&run)?;
        ensure!(
            result["status"] == "usage_error"
                && result["actions"].as_array().is_some_and(Vec::is_empty)
                && result["errors"][0]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains(&wanted))
                && old.join("payload").exists()
                && current.join("payload").exists(),
            "ensure passed a negated retention guard {filter}: {result}"
        );
    }
    Ok(())
}

/// Normal lookup selects the user's policy, an environment-selected policy
/// takes precedence over it, and an explicit flag takes precedence over the
/// environment. Each run reclaims only the candidate named by its policy.
pub fn config_environment_and_flag_precedence_reclaim_selected_policy(ctx: &Ctx) -> TestResult {
    let searched = ctx.scratch.mkdir("cli-searched-policy", 0o755, TESTER)?;
    let from_env = ctx.scratch.mkdir("cli-env-policy", 0o755, TESTER)?;
    let from_flag = ctx.scratch.mkdir("cli-flag-policy", 0o755, TESTER)?;
    for name in ["cli-searched-policy", "cli-env-policy", "cli-flag-policy"] {
        ctx.scratch
            .write_file(&format!("{name}/payload"), &[3; 8192], 0o644, TESTER)?;
    }
    // Keep pressure after all three candidates are removed, so each run
    // must finish exhausted rather than depending on tmpfs free-block rounding.
    ctx.scratch
        .write_file("cli-precedence-pressure", &[9; 8192], 0o644, TESTER)?;
    ctx.home.mkdir(".config", 0o755, TESTER)?;
    ctx.home.mkdir(".config/diskgc", 0o755, TESTER)?;
    for (file, name) in [
        (".config/diskgc/config.yaml", "cli-searched-policy"),
        ("cli-env-policy.yaml", "cli-env-policy"),
        ("cli-flag-policy.yaml", "cli-flag-policy"),
    ] {
        let policy = format!(
            "version: 1\nprofiles:\n  - name: {name}\n    find: {{roots: [/tmp], paths: [{name}], type: dir}}\n    action: delete\n    examples: {{match: [/tmp/{name}]}}\ngroups:\n  - name: policy\n    tiers:\n      - name: policy-tier\n        select: profile:{name}\n"
        );
        ctx.home
            .write_file(file, policy.as_bytes(), 0o600, TESTER)?;
    }
    let oracle = Oracle::arm(Fixture::new().eligible(searched.clone()))?;
    let searched_run = ensure_run(
        ctx,
        &["--json", "--min-free", "100%", "--timeout", "10s", "/tmp"],
    )?;
    oracle.judge(&searched_run)?.assert_clean()?;
    let searched_report = report(&searched_run)?;
    expect_reclaim_result(&searched_run, &searched_report)?;
    ensure!(
        searched_report["actions"]
            .as_array()
            .is_some_and(|actions| {
                actions.len() == 1
                    && actions[0]["candidate"] == searched.to_string_lossy().as_ref()
                    && actions[0]["profile"] == "cli-searched-policy"
            })
            && !searched.exists()
            && from_env.exists()
            && from_flag.exists(),
        "default lookup did not load the searched policy: {searched_report}"
    );
    let env = [(
        "DISKGC_CONFIG".to_owned(),
        "/home/tester/cli-env-policy.yaml".to_owned(),
    )];
    let oracle = Oracle::arm(Fixture::new().eligible(from_env.clone()))?;
    let env_run = under_test::run_as_tester_with_env(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        &env,
        &ctx.diskgc(),
        &[
            "ensure",
            "--json",
            "--min-free",
            "100%",
            "--timeout",
            "10s",
            "/tmp",
        ],
    )?;
    oracle.judge(&env_run)?.assert_clean()?;
    let env_report = report(&env_run)?;
    expect_reclaim_result(&env_run, &env_report)?;
    ensure!(
        env_report["actions"].as_array().is_some_and(|actions| {
            actions.len() == 1
                && actions[0]["candidate"] == from_env.to_string_lossy().as_ref()
                && actions[0]["profile"] == "cli-env-policy"
        }) && !searched.exists()
            && !from_env.exists()
            && from_flag.exists(),
        "DISKGC_CONFIG did not override the searched policy: {env_report}"
    );

    let oracle = Oracle::arm(Fixture::new().eligible(from_flag.clone()))?;
    let flag_run = under_test::run_as_tester_with_env(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        &env,
        &ctx.diskgc(),
        &[
            "ensure",
            "--json",
            "--config",
            "/home/tester/cli-flag-policy.yaml",
            "--min-free",
            "100%",
            "--timeout",
            "10s",
            "/tmp",
        ],
    )?;
    oracle.judge(&flag_run)?.assert_clean()?;
    let flag_report = report(&flag_run)?;
    expect_reclaim_result(&flag_run, &flag_report)?;
    ensure!(
        flag_report["actions"].as_array().is_some_and(|actions| {
            actions.len() == 1
                && actions[0]["candidate"] == from_flag.to_string_lossy().as_ref()
                && actions[0]["profile"] == "cli-flag-policy"
        }) && !searched.exists()
            && !from_flag.exists(),
        "--config did not override DISKGC_CONFIG: {flag_report}"
    );
    Ok(())
}

/// The same command-line roots cannot be silently ignored by a fixed-root
/// profile, and a required-root profile cannot run without them.
pub fn select_roots_requirements_are_usage_errors(ctx: &Ctx) -> TestResult {
    let candidate = ctx.scratch.mkdir("cli-roots-candidate", 0o755, TESTER)?;
    ctx.scratch
        .write_file("cli-roots-candidate/sentinel", b"keep", 0o644, TESTER)?;
    ctx.home.write_file(
        "cli-roots-policy.yaml",
        b"version: 1\nprofiles:\n  - name: cli-fixed-roots\n    find: {roots: [/tmp], paths: [cli-roots-candidate], type: dir}\n    action: delete\n    examples: {match: [/tmp/cli-roots-candidate]}\n  - name: cli-required-roots\n    find: {roots: required, paths: [cli-roots-candidate], type: dir}\n    action: delete\n    examples: {root: /tmp, match: [/tmp/cli-roots-candidate]}\n",
        0o600,
        TESTER,
    )?;
    let oracle = Oracle::arm(Fixture::new())?;
    let fixed = ensure_run(
        ctx,
        &[
            "--json",
            "--config",
            "/home/tester/cli-roots-policy.yaml",
            "--min-free",
            "100%",
            "--select",
            "profile:cli-fixed-roots",
            "--roots",
            "/tmp",
            "/tmp",
        ],
    )?;
    let required = ensure_run(
        ctx,
        &[
            "--json",
            "--config",
            "/home/tester/cli-roots-policy.yaml",
            "--min-free",
            "100%",
            "--select",
            "profile:cli-required-roots",
            "/tmp",
        ],
    )?;
    oracle.judge_runs(&[&fixed, &required])?.assert_clean()?;
    for (run, expected) in [(&fixed, "fixed roots"), (&required, "no roots were given")] {
        run.expect_code(2)?;
        let result = report(run)?;
        ensure!(
            result["status"] == "usage_error"
                && result["actions"].as_array().is_some_and(Vec::is_empty)
                && result["errors"][0]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains(expected)),
            "--roots validation missed {expected}: {result}"
        );
    }
    ensure!(
        candidate.join("sentinel").exists(),
        "invalid --roots command changed candidate"
    );
    Ok(())
}

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
            && actions[0]["tier"] == "select"
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

/// Dry-run classifies a candidate without attempting its directory flock.
/// The harness holds a shared lock as container root, outside the selected
/// owner's process scan, while the tester-owned candidate remains eligible.
pub fn dry_run_plans_flocked_candidate_as_eligible(ctx: &Ctx) -> TestResult {
    let candidate = ctx.scratch.mkdir("cli-dry-locked", 0o755, TESTER)?;
    let sentinel = ctx
        .scratch
        .write_file("cli-dry-locked/payload", &[5; 8192], 0o644, TESTER)?;
    ctx.home.write_file(
        "cli-dry-locked.yaml",
        b"version: 1\nprofiles:\n  - name: cli-dry-locked\n    find: {roots: [/tmp], paths: [cli-dry-locked], type: dir}\n    action: delete\n    examples: {match: [/tmp/cli-dry-locked]}\n",
        0o600,
        TESTER,
    )?;
    let lock = std::fs::File::open(&candidate)
        .map_err(|error| format!("open candidate {}: {error}", candidate.display()))?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockShared)
        .map_err(|error| format!("flock candidate {}: {error}", candidate.display()))?;
    let oracle = Oracle::arm(Fixture::new())?;
    let run = ensure_run(
        ctx,
        &[
            "--dry-run",
            "--json",
            "--config",
            "/home/tester/cli-dry-locked.yaml",
            "--min-free",
            "100%",
            "--select",
            "profile:cli-dry-locked",
            "/tmp",
        ],
    )?;
    oracle.judge(&run)?.assert_clean()?;
    run.expect_code(0)?;
    let result = report(&run)?;
    ensure!(
        result["status"] == "dry_run"
            && result["plan"].as_array().is_some_and(|plan| {
                plan.len() == 1
                    && plan[0]["candidate"] == candidate.to_string_lossy().as_ref()
                    && plan[0]["eligible"] == true
                    && plan[0]["reason"].is_null()
            })
            && result["actions"].as_array().is_some_and(Vec::is_empty),
        "dry-run classified a flocked candidate as locked: {result}"
    );
    ensure!(
        sentinel.exists() && candidate.exists(),
        "dry-run removed the flocked candidate"
    );
    drop(lock);
    Ok(())
}
