//! Agent pre-job journey: the shipped Python example consumes the real JSON
//! report and decides whether to start, defer, abort, or retry.

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, TestResult};
use crate::under_test::{self, Run};
use crate::{TESTER, UNDER_TEST_BIN_DIR, ensure, info};

const SCRIPT: &str = include_str!("../../docs/agent-pre-job.py");
const SCRIPT_PATH: &str = "/tmp/agent-pre-job.py";
const CONFIG: &str = "/home/tester/agent-diskgc.yaml";
const READY: &str = "/tmp/agent-competitor-ready";
const RELEASE: &str = "/tmp/agent-competitor-release";

/// TASK-55: real `ensure` statuses 0/1/2/3/75, including a misconfigured
/// tier and two overlapping reclaimers. Every traced script and competing
/// diskgc run has a deletion oracle armed before it starts.
pub fn agent_harness_uses_json_for_all_reclaim_decisions(ctx: &Ctx) -> TestResult {
    ctx.scratch
        .write_file("agent-pre-job.py", SCRIPT.as_bytes(), 0o644, TESTER)?;
    ctx.scratch.mkdir("agent-empty", 0o755, TESTER)?;
    ctx.scratch.mkdir("agent-competitor", 0o755, TESTER)?;
    ctx.scratch.write_file(
        "agent-cleaner.sh",
        format!(
            "#!/bin/sh\nset -eu\nprintf 'ready\\n' > {READY}\n\
             while [ ! -e {RELEASE} ]; do sleep 0.1; done\n"
        )
        .as_bytes(),
        0o755,
        TESTER,
    )?;
    let config = r#"version: 1
profiles:
  - name: agent-empty
    find: {roots: [/tmp/agent-empty], paths: [absent], type: dir}
    action: delete
    examples: {match: [/tmp/agent-empty/absent]}
  - name: agent-bad-root
    find: {roots: [/tmp/systemd-private-agent], paths: [old], type: dir}
    action: delete
    examples: {match: [/tmp/systemd-private-agent/old]}
  - name: agent-roots
    find: {roots: required, paths: [absent], type: dir}
    action: delete
    examples: {root: /tmp/agent-empty, match: [/tmp/agent-empty/absent]}
  - name: agent-competitor
    find: {roots: [/tmp], paths: [agent-competitor], type: dir}
    action: {command: [/bin/sh, /tmp/agent-cleaner.sh]}
    examples: {match: [/tmp/agent-competitor]}
groups:
  - name: agent
    tiers:
      - name: broken-root
        select: profile:agent-bad-root
"#;
    ctx.home
        .write_file("agent-diskgc.yaml", config.as_bytes(), 0o600, TESTER)?;

    let common = ["--config", CONFIG, "--select", "profile:agent-empty"];
    let oracle = Oracle::arm(Fixture::new())?;
    let met = agent(
        ctx,
        &[
            "--min-free",
            "1K",
            "--target-free",
            "100%",
            common[0],
            common[1],
            common[2],
            common[3],
            "/tmp",
        ],
    )?;
    oracle.judge(&met)?.assert_clean()?;
    let met_report = expect(&met, 0, "start", "met")?;
    ensure!(
        met_report["target_met"].as_bool() == Some(false),
        "0 below target-free: {met_report}"
    );

    let oracle = Oracle::arm(Fixture::new())?;
    let exhausted = agent(
        ctx,
        &[
            "--min-free",
            "100%",
            "--config",
            CONFIG,
            "--select",
            "profile:agent-roots",
            "--roots",
            "/tmp/agent-empty",
            "--roots",
            "/tmp/agent-competitor",
            "/tmp",
        ],
    )?;
    oracle.judge(&exhausted)?.assert_clean()?;
    expect(&exhausted, 1, "defer_no_space", "exhausted")?;

    let oracle = Oracle::arm(Fixture::new())?;
    let broken = agent(ctx, &["--min-free", "100%", "--config", CONFIG, "/tmp"])?;
    oracle.judge(&broken)?.assert_clean()?;
    let report = expect(&broken, 3, "abort_error", "error")?;
    ensure!(
        report["errors"]
            .as_array()
            .is_some_and(|errors| errors.iter().any(|error| {
                error["tier"] == "broken-root"
                    && error["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("systemd PrivateTmp"))
            })),
        "misconfigured tier was not named in JSON: {report}"
    );

    // TASK-150: a caller error (exit 2) is `abort_config`, not an invalid
    // report; the JSON cause tells it from an unprovisioned host.
    let oracle = Oracle::arm(Fixture::new())?;
    let typo = agent(
        ctx,
        &[
            "--min-free",
            "100%",
            "--config",
            CONFIG,
            "--select",
            "profile:agent-emtpy",
            "/tmp",
        ],
    )?;
    oracle.judge(&typo)?.assert_clean()?;
    let report = expect(&typo, 2, "abort_config", "usage_error")?;
    ensure!(
        report["errors"][0]["cause"] == "parse",
        "typo'd selection is not a parse error: {report}"
    );

    // A real competing ensure enters its configured command while holding
    // the same host lock. The command only writes a readiness marker and
    // waits for release; it never removes the candidate.
    let oracle = Oracle::arm(Fixture::new())?;
    let contender = ctx.spawn_as_tester(
        &ctx.diskgc(),
        &[
            "ensure",
            "--json",
            "--min-free",
            "100%",
            "--timeout",
            "20s",
            "--config",
            CONFIG,
            "--select",
            "profile:agent-competitor",
            "/tmp",
        ],
    )?;
    let until = Instant::now() + Duration::from_secs(10);
    while !Path::new(READY).exists() && Instant::now() < until {
        thread::sleep(Duration::from_millis(20));
    }
    ensure!(
        Path::new(READY).exists(),
        "competing reclaim never entered its command"
    );
    let waiting = agent(
        ctx,
        &[
            "--min-free",
            "100%",
            "--timeout",
            "1s",
            common[0],
            common[1],
            common[2],
            common[3],
            "/tmp",
        ],
    )?;
    ctx.scratch
        .write_file("agent-competitor-release", b"go\n", 0o644, TESTER)?;
    let competitor = contender.wait()?;
    oracle
        .judge_runs(&[&waiting, &competitor])?
        .assert_clean()?;
    expect(&waiting, 75, "retry_later", "lock_timeout")?;
    competitor.expect_code(1)?;
    ensure!(
        Path::new("/tmp/agent-competitor").exists(),
        "competing command removed its candidate"
    );
    Ok(())
}

fn agent(ctx: &Ctx, args: &[&str]) -> Result<Run, String> {
    let env = [("PATH".to_owned(), format!("{UNDER_TEST_BIN_DIR}:/bin"))];
    let mut argv = vec![SCRIPT_PATH];
    argv.extend_from_slice(args);
    let run = under_test::run_as_tester_with_env(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        &env,
        &ctx.bin_dir.join("agent-python3"),
        &argv,
    )?;
    info!("agent {:?} -> {:?}: {}", args, run.code, run.stdout);
    Ok(run)
}

fn expect(run: &Run, code: i32, decision: &str, status: &str) -> Result<Value, String> {
    run.expect_code(code)?;
    let lines: Vec<_> = run.stdout.lines().collect();
    ensure!(
        lines.len() == 1,
        "agent wrote more than one JSON line: {:?}",
        run.stdout
    );
    let report: Value = serde_json::from_str(lines[0])
        .map_err(|error| format!("agent report is not JSON: {error}"))?;
    ensure!(
        report["decision"] == decision && report["status"] == status,
        "agent chose wrong action for {code}: {report}"
    );
    Ok(report)
}
