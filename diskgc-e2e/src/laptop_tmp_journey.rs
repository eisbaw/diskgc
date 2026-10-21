//! A laptop owner's `/tmp` under pressure, using only user-defined cleanup
//! profiles. The fixed-root Claude profile is used solely as a protection
//! boundary. Every run is judged against fixture paths, not the CLI report.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde_json::Value;

use crate::ensure_suite::{ensure_run, report};
use crate::holder::Holder;
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, TestResult};
use crate::{TESTER, ensure};

const CONFIG: &str = "/home/tester/laptop-tmp.yaml";

/// The owner's policy and observed temp layout: a nested pytest directory,
/// Nix shell rc directory, independent shell temp dirs, and Claude's temp
/// root. The live shell holds an open file and has TMPDIR in its initial
/// environment. Its socket, the Claude session socket, and an unrelated
/// socket must all survive.
pub fn laptop_tmp_groups_protect_live_work_and_reclaim_independent_cruft(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    let stale_test = "nix-shell.A1/pytest-of-tester/pytest-0/test_stale0";
    let live_test = "nix-shell.A1/pytest-of-tester/pytest-0/test_live0";
    let stale_rc = "nix-shell-123-456";
    let stale_tmp = "tmp.stale";
    let live_tmp = "tmp.live";
    let claude_root = "claude-1000";
    let claude_session = "claude-1000/-home-tester-project/session-1";

    for dir in [
        stale_test,
        live_test,
        stale_rc,
        stale_tmp,
        live_tmp,
        claude_session,
    ] {
        s.mkdirs(dir, 0o700, TESTER)?;
        s.write_file(
            &format!("{dir}/payload"),
            b"owner's temp data",
            0o600,
            TESTER,
        )?;
    }
    s.socket("tmp.live/server.sock", TESTER)?;
    s.socket(
        "claude-1000/-home-tester-project/session-1/control.sock",
        TESTER,
    )?;
    s.mkdirs("ssh-agent", 0o700, TESTER)?;
    s.socket("ssh-agent/agent.sock", TESTER)?;

    let live_test_path = s.path(live_test);
    let live_tmp_path = s.path(live_tmp);
    let claude_path = s.path(claude_root);
    let untouched_socket = s.path("ssh-agent/agent.sock");
    let stale_paths = [s.path(stale_test), s.path(stale_rc)];
    let pressure_paths = [s.path(stale_tmp)];
    let deletable: BTreeSet<PathBuf> = stale_paths.iter().chain(&pressure_paths).cloned().collect();

    // Both descriptors remain open through the preview and the real run.
    let pytest_holder = Holder::tester(
        ctx,
        &[
            "hold",
            "/tmp/nix-shell.A1/pytest-of-tester/pytest-0/test_live0/payload",
            "/",
        ],
        &[],
    )?;
    let shell_holder = Holder::tester(
        ctx,
        &["hold", "/tmp/tmp.live/payload", "/tmp/tmp.live"],
        &[("TMPDIR", "/tmp/tmp.live")],
    )?;

    ctx.home.write_file(
        "laptop-tmp.yaml",
        br#"version: 1
profiles:
  - name: laptop-pytest
    find: {roots: [/tmp], paths: ['nix-shell.*/pytest-of-*/pytest-*/*'], type: dir}
    action: delete
    examples: {match: [/tmp/nix-shell.A1/pytest-of-tester/pytest-0/test_stale0]}
  - name: laptop-nix-rc
    find: {roots: [/tmp], paths: ['nix-shell-*-*'], type: dir}
    action: delete
    examples: {match: [/tmp/nix-shell-123-456]}
  - name: laptop-shell-tmp
    find: {roots: [/tmp], paths: ['tmp.*'], type: dir}
    action: delete
    examples: {match: [/tmp/tmp.stale]}
  - name: laptop-claude-parent
    find: {roots: [/tmp], paths: [claude-1000], type: dir}
    action: delete
    examples: {match: [/tmp/claude-1000]}
protect: [claude-code-tmp]
groups:
  - name: stale
    tiers:
      - name: pytest-results
        select: profile:laptop-pytest
      - name: nix-rc
        select: profile:laptop-nix-rc
  - name: pressure
    tiers:
      - name: shell-tmp
        select: profile:laptop-shell-tmp
      - name: claude-parent
        select: profile:laptop-claude-parent
"#,
        0o600,
        TESTER,
    )?;

    let args = [
        "--json",
        "--no-index",
        "--config",
        CONFIG,
        "--min-free",
        "100%",
        "--timeout",
        "10s",
        "/tmp",
    ];
    let mut preview_args = vec!["--dry-run"];
    preview_args.extend(args);
    let preview_oracle = Oracle::arm(Fixture::new())?;
    let preview = ensure_run(ctx, &preview_args)?;
    let preview_verdict = preview_oracle.judge(&preview)?;
    preview.expect_code(0)?;
    let plan = report(&preview)?;
    ensure!(plan["status"] == "dry_run", "unexpected preview: {plan}");
    let planned = plan_entries(&plan)?;
    let wanted: BTreeSet<String> = deletable
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    let eligible: BTreeSet<String> = planned
        .iter()
        .filter(|(_, reason)| reason.is_none())
        .map(|(path, _)| path.clone())
        .collect();
    ensure!(
        eligible == wanted,
        "unexpected dry-run eligibility: {planned:?}"
    );
    for (path, reason) in [
        (&live_test_path, "in_use"),
        // The socket guard runs before the process-use check for this tree.
        (&live_tmp_path, "contains_ipc"),
        (&claude_path, "protected"),
    ] {
        ensure!(
            planned.get(path.to_string_lossy().as_ref()) == Some(&Some(reason.to_owned())),
            "{} was not skipped as {reason}: {planned:?}",
            path.display()
        );
    }
    preview_verdict.assert_clean()?;

    let fixture = deletable.iter().fold(Fixture::new(), |fixture, path| {
        fixture.eligible(path.clone())
    });
    let run_oracle = Oracle::arm(fixture)?;
    let run = ensure_run(ctx, &args)?;
    let run_verdict = run_oracle.judge(&run)?;
    run.expect_code(1)?; // 100% cannot be reached while protected data remains.
    let result = report(&run)?;
    ensure!(
        result["status"] == "exhausted",
        "unexpected reclaim result: {result}"
    );
    let actions = result["actions"]
        .as_array()
        .ok_or("actions is not an array")?;
    let tiers: Vec<&str> = actions
        .iter()
        .map(|action| action["tier"].as_str().unwrap_or(""))
        .collect();
    ensure!(
        tiers == ["pytest-results", "nix-rc", "shell-tmp"],
        "stale and pressure groups ran out of order: {result}"
    );
    let acted: BTreeSet<String> = actions
        .iter()
        .map(|action| {
            action["candidate"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("action has no candidate: {action}"))
        })
        .collect::<Result<_, _>>()?;
    ensure!(
        acted == eligible,
        "run disagrees with dry-run plan: {result}"
    );
    for path in &deletable {
        ensure!(
            !path.exists(),
            "planned candidate survived: {}",
            path.display()
        );
    }
    for path in [
        live_test_path.join("payload"),
        live_tmp_path.join("payload"),
        live_tmp_path.join("server.sock"),
        claude_path.join("-home-tester-project/session-1/payload"),
        claude_path.join("-home-tester-project/session-1/control.sock"),
        untouched_socket,
    ] {
        ensure!(
            path.exists(),
            "protected or in-use object vanished: {}",
            path.display()
        );
    }
    run_verdict.assert_clean()?;
    pytest_holder.finish()?;
    shell_holder.finish()
}

fn plan_entries(report: &Value) -> Result<BTreeMap<String, Option<String>>, String> {
    let plan = report["plan"].as_array().ok_or("plan is not an array")?;
    let mut entries = BTreeMap::new();
    for entry in plan {
        let path = entry["candidate"]
            .as_str()
            .ok_or_else(|| format!("plan entry has no candidate: {entry}"))?;
        let reason = entry["reason"].as_str().map(str::to_owned);
        ensure!(
            !entries.contains_key(path),
            "duplicate plan candidate {path}: {plan:?}"
        );
        entries.insert(path.to_owned(), reason);
    }
    Ok(entries)
}
