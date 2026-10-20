//! Shipped project-cache profiles against real filesystem discovery and
//! deletion. The selected project root is explicit; source, virtualenv
//! contents, deeper namesakes, and neighboring projects are oracle-protected.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;

use crate::ensure_suite::{ensure_run, report};
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, TestResult};
use crate::{TESTER, ensure};

const PROJECT: &str = "/home/tester/project";

fn write(ctx: &Ctx, rel: &str, content: &[u8]) -> TestResult {
    let parent = rel
        .rsplit_once('/')
        .ok_or_else(|| format!("fixture file {rel:?} has no parent"))?
        .0;
    ctx.home.mkdirs(parent, 0o700, TESTER)?;
    ctx.home.write_file(rel, content, 0o600, TESTER)?;
    Ok(())
}

fn candidates(report: &Value, field: &str) -> Result<BTreeSet<String>, String> {
    report[field]
        .as_array()
        .ok_or_else(|| format!("{field} is not an array: {report}"))?
        .iter()
        .map(|entry| {
            entry["candidate"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{field} entry lacks candidate: {entry}"))
        })
        .collect()
}

/// Exercise both CLI planning and deletion, with the oracle's allowed paths
/// declared from the fixture rather than inferred from the product's report.
fn reclaim_exactly(ctx: &Ctx, profile: &str, eligible: &[&str], protected: &[&str]) -> TestResult {
    let expected: BTreeSet<String> = eligible
        .iter()
        .map(|rel| ctx.home.path(rel).display().to_string())
        .collect();
    let fixture = eligible.iter().fold(Fixture::new(), |fixture, rel| {
        fixture.eligible(ctx.home.path(rel))
    });
    let oracle = Oracle::arm(fixture)?;
    let select = format!("profile:{profile}");
    let args = [
        "--json",
        "--no-index",
        "--min-free",
        "100%",
        "--select",
        &select,
        "--roots",
        PROJECT,
        PROJECT,
    ];
    let mut dry_args = vec!["--dry-run"];
    dry_args.extend(args);
    let dry = ensure_run(ctx, &dry_args)?;
    let run = ensure_run(ctx, &args)?;
    let verdict = oracle.judge_runs(&[&dry, &run])?;
    dry.expect_code(0)?;
    run.expect_code(1)?;
    let dry_report = report(&dry)?;
    ensure!(
        candidates(&dry_report, "plan")? == expected,
        "{profile} dry-run plan did not select exactly the tagged caches: {dry_report}"
    );
    let run_report = report(&run)?;
    ensure!(
        candidates(&run_report, "actions")? == expected,
        "{profile} did not delete exactly the planned caches: {run_report}"
    );
    for rel in eligible {
        ensure!(
            !ctx.home.path(rel).exists(),
            "{profile} left eligible cache {rel}"
        );
    }
    for rel in protected {
        ensure!(
            std::fs::symlink_metadata(ctx.home.path(rel)).is_ok(),
            "{profile} removed protected fixture {rel}"
        );
    }
    verdict.assert_clean()
}

/// A project root reaches named bytecode caches through the profile's fifth
/// relative depth. A sixth-depth namesake and a sibling project are outside
/// the policy; deleting their files would fail the oracle independently.
pub fn python_pycache_reclaims_only_selected_project_caches(ctx: &Ctx) -> TestResult {
    for rel in [
        "project/__pycache__/main.cpython-313.pyc",
        "project/pkg/sub/__pycache__/module.cpython-313.pyc",
        "project/a/b/c/d/__pycache__/deep.cpython-313.pyc",
        "project/a/b/c/d/e/__pycache__/too-deep.pyc",
        "other-project/__pycache__/outside.pyc",
        "project/main.py",
        "project/pkg/sub/module.py",
        "project/.venv/bin/python",
        "project/pyproject.toml",
    ] {
        write(
            ctx,
            rel,
            b"preserve or reclaim according to the selected profile",
        )?;
    }
    reclaim_exactly(
        ctx,
        "python-pycache",
        &[
            "project/__pycache__",
            "project/pkg/sub/__pycache__",
            "project/a/b/c/d/__pycache__",
        ],
        &[
            "project/a/b/c/d/e/__pycache__/too-deep.pyc",
            "other-project/__pycache__/outside.pyc",
            "project/main.py",
            "project/pkg/sub/module.py",
            "project/.venv/bin/python",
            "project/pyproject.toml",
        ],
    )
}

/// Only a regular CACHEDIR.TAG in an exact `.mypy_cache` directory allows
/// deletion. A symlink marker, directory marker, untagged cache, backup
/// namesake, and a cache beyond the depth bound must survive.
pub fn mypy_cache_requires_regular_tag_and_preserves_project(ctx: &Ctx) -> TestResult {
    for rel in [
        "project/.mypy_cache/CACHEDIR.TAG",
        "project/.mypy_cache/3.13/main.meta.json",
        "project/pkg/.mypy_cache/CACHEDIR.TAG",
        "project/pkg/.mypy_cache/3.13/pkg.meta.json",
        "project/untagged/.mypy_cache/cache.json",
        "project/tag-link/.mypy_cache/cache.json",
        "project/tag-dir/.mypy_cache/cache.json",
        "project/.mypy_cache-backup/CACHEDIR.TAG",
        "project/a/b/c/d/e/.mypy_cache/CACHEDIR.TAG",
        "project/a/b/c/d/e/.mypy_cache/cache.json",
        "other-project/.mypy_cache/CACHEDIR.TAG",
        "other-project/.mypy_cache/cache.json",
        "project/main.py",
        "project/.venv/bin/python",
    ] {
        write(ctx, rel, b"fixture")?;
    }
    ctx.home.symlink(
        "project/tag-link/.mypy_cache/CACHEDIR.TAG",
        Path::new("/home/tester/project/main.py"),
    )?;
    ctx.home
        .mkdir("project/tag-dir/.mypy_cache/CACHEDIR.TAG", 0o700, TESTER)?;
    reclaim_exactly(
        ctx,
        "mypy-cache",
        &["project/.mypy_cache", "project/pkg/.mypy_cache"],
        &[
            "project/untagged/.mypy_cache/cache.json",
            "project/tag-link/.mypy_cache/CACHEDIR.TAG",
            "project/tag-link/.mypy_cache/cache.json",
            "project/tag-dir/.mypy_cache/CACHEDIR.TAG",
            "project/tag-dir/.mypy_cache/cache.json",
            "project/.mypy_cache-backup/CACHEDIR.TAG",
            "project/a/b/c/d/e/.mypy_cache/CACHEDIR.TAG",
            "project/a/b/c/d/e/.mypy_cache/cache.json",
            "other-project/.mypy_cache/CACHEDIR.TAG",
            "other-project/.mypy_cache/cache.json",
            "project/main.py",
            "project/.venv/bin/python",
        ],
    )
}
