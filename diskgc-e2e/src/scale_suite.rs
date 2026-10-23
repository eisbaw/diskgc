//! Full-tier scale evidence on disposable container tmpfs. These cases use
//! the shipped binary and the deletion oracle, not the in-memory host model.

use std::collections::HashSet;
use std::fs;
use std::thread;
use std::time::{Duration, Instant};

use crate::ensure_suite::report;
use crate::holder::Holder;
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, TestResult};
use crate::under_test::Run;
use crate::{TESTER, ensure, info};

const CANDIDATES: usize = 40_000;
const PROFILE: &str = "scale-small-dirs";
const CONFIG: &str = "/home/tester/scale-small-dirs.yaml";

/// The first product invocation plans 40,000 tiny directories while a
/// path-heavy fixture process is present. Its process is fresh, but the
/// fixture creation has necessarily populated the kernel's dentry cache;
/// this is not a cold page-cache experiment. The next invocation performs
/// the real deletion protocol under strace and the independent oracle.
pub fn fs_40k_small_candidates_plan_and_reclaim_scale(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    let built = Instant::now();
    let mut fixture = Fixture::new();
    for number in 0..CANDIDATES {
        let relative = format!("scale-small-{number:05}");
        fixture = fixture.eligible(s.mkdir(&relative, 0o700, TESTER)?);
        s.write_file(&format!("{relative}/payload"), b"x", 0o600, TESTER)?;
    }
    info!(
        "built {CANDIDATES} small candidates in {:?}",
        built.elapsed()
    );
    ctx.home.write_file(
        "scale-small-dirs.yaml",
        format!(
            "version: 1\nprofiles:\n  - name: {PROFILE}\n    find: {{roots: [/tmp], paths: ['scale-small-*'], type: dir}}\n    action: delete\n    examples: {{match: [/tmp/scale-small-00000]}}\n"
        )
        .as_bytes(),
        0o600,
        TESTER,
    )?;

    // Three initial environment variables remain below Linux's per-string
    // exec limit. Their distinct deep paths exercise ancestor-index growth
    // without naming any candidate directory.
    let env_values: Vec<String> = (0..3)
        .map(|group| {
            (0..1_500)
                .map(|i| format!("/tmp/scale-env-{group}/branch-{i:04}/leaf/data"))
                .collect::<Vec<_>>()
                .join(":")
        })
        .collect();
    let env = [
        ("SCALE_PATHS_A", env_values[0].as_str()),
        ("SCALE_PATHS_B", env_values[1].as_str()),
        ("SCALE_PATHS_C", env_values[2].as_str()),
    ];
    let holder = Holder::tester(ctx, &["hold-export", "SCALE_READY", "yes"], &env)?;
    let oracle = Oracle::arm(fixture)?;
    let args = [
        "ensure",
        "--json",
        "--config",
        CONFIG,
        "--min-free",
        "100%",
        "--select",
        "profile:scale-small-dirs",
        "/tmp",
    ];
    let mut dry_args = vec!["ensure", "--dry-run"];
    dry_args.extend_from_slice(&args[1..]);
    let (dry, dry_peak_kib) = observed_run(ctx, &dry_args)?;
    dry.expect_code(0)?;
    let dry_report = report(&dry)?;
    let plan = dry_report["plan"]
        .as_array()
        .ok_or_else(|| format!("missing plan array: {dry_report}"))?;
    ensure!(
        plan.len() == CANDIDATES && plan.iter().all(|entry| entry["reason"].is_null()),
        "first plan judged {} of {CANDIDATES} candidates, status={}",
        plan.len(),
        dry_report["status"]
    );
    info!(
        "first-process 40k plan: elapsed_ms={} peak_product_rss_kib={} (kernel cache warmed by fixture creation)",
        dry.took.as_millis(),
        dry_peak_kib
    );

    let (reclaim, reclaim_peak_kib) = observed_run(ctx, &args)?;
    let verdict = oracle.judge_runs(&[&dry, &reclaim])?;
    let result = report(&reclaim)?;
    let status = result["status"].as_str();
    match status {
        Some("met") => {
            reclaim.expect_code(0)?;
        }
        Some("exhausted") => {
            reclaim.expect_code(1)?;
        }
        _ => return Err(format!("unexpected reclaim status {status:?}")),
    }
    let actions = result["actions"]
        .as_array()
        .ok_or_else(|| format!("missing actions array: {result}"))?;
    let detailed: HashSet<&str> = actions
        .iter()
        .filter_map(|action| action["candidate"].as_str())
        .filter(|candidate| !candidate.is_empty())
        .collect();
    let action_count: u64 = actions
        .iter()
        .map(|action| action["count"].as_u64().unwrap_or(0))
        .sum();
    ensure!(
        action_count == CANDIDATES as u64
            && actions.len() == 1_001
            && detailed.len() == 1_000
            && actions[1_000]["candidate"] == ""
            && actions[1_000]["count"] == 39_000
            && result["errors"].as_array().is_some_and(Vec::is_empty),
        "reclaim reported {action_count} actions, {} detailed candidates, expected {CANDIDATES}; status={}",
        detailed.len(),
        result["status"]
    );
    for entry in fs::read_dir("/tmp").map_err(|e| format!("list scratch after reclaim: {e}"))? {
        let entry = entry.map_err(|e| format!("read scratch entry after reclaim: {e}"))?;
        ensure!(
            !entry
                .file_name()
                .to_string_lossy()
                .starts_with("scale-small-"),
            "{} survived reclaim",
            entry.path().display()
        );
    }
    info!(
        "40k real deletion: elapsed_ms={} candidates_per_second={:.1} peak_product_rss_kib={}",
        reclaim.took.as_millis(),
        CANDIDATES as f64 / reclaim.took.as_secs_f64(),
        reclaim_peak_kib
    );

    let memory = ctx.probe_as_root(&["host-inuse-index-memory"])?;
    ensure!(
        memory.status.success(),
        "index memory probe failed: status={} stderr={}",
        memory.status,
        String::from_utf8_lossy(&memory.stderr)
    );
    let memory_report = String::from_utf8(memory.stdout)
        .map_err(|e| format!("index memory probe output UTF-8: {e}"))?;
    let metric = |name: &str| -> Result<usize, String> {
        memory_report
            .split_whitespace()
            .find_map(|field| field.strip_prefix(&format!("{name}=")))
            .ok_or_else(|| format!("missing {name} in index memory report: {memory_report}"))?
            .parse()
            .map_err(|e| format!("parse {name} in index memory report: {e}"))
    };
    ensure!(
        metric("paths")? >= 4_500 && metric("index_minimum_bytes")? > 0,
        "path-heavy holder was not represented in index memory scan: {memory_report}"
    );
    info!(
        "ancestor-index retained allocation lower bound: {}",
        memory_report.trim()
    );
    holder.finish()?;
    verdict.assert_clean()
}

/// Observe only the diskgc tracee, excluding the harness and strace. Linux
/// reports VmHWM in each live process's status, so polling retains its peak
/// even when the sampling interval misses the exact allocation instant.
fn observed_run(ctx: &Ctx, args: &[&str]) -> Result<(Run, usize), String> {
    let mut spawned = ctx.spawn_as_tester(&ctx.diskgc(), args)?;
    let tracer = spawned.strace_pid();
    let mut peak_kib = 0;
    let mut observed = false;
    while !spawned.finished()? {
        let children = fs::read_to_string(format!("/proc/{tracer}/task/{tracer}/children"))
            .unwrap_or_default();
        for pid in children.split_whitespace() {
            let comm = fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
            if comm.trim() != "diskgc" {
                continue;
            }
            if let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) {
                if let Some(kib) = status
                    .lines()
                    .find_map(|line| line.strip_prefix("VmHWM:"))
                    .and_then(|value| value.split_whitespace().next())
                    .and_then(|number| number.parse::<usize>().ok())
                {
                    peak_kib = peak_kib.max(kib);
                    observed = true;
                }
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    let run = spawned.wait()?;
    ensure!(observed, "never sampled diskgc VmHWM during {:?}", run.argv);
    Ok((run, peak_kib))
}
