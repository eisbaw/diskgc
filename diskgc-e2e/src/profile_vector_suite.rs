//! Kernel replay of a checked-in ModelHost profile vector. The replay is
//! deliberately limited to ordinary files and directories with no modelled
//! clock override or process state; unsupported model features fail here.
//! Each run still goes through the container's tmpfs, strace, and deletion
//! oracle. The same contract is replayed under both standard rootless Podman
//! and the native Docker Engine dind fixture.

use std::path::Path;

use diskgc_model::profile_vector::{PlanVerdict, ProfileVector, SkipVerdict};
use diskgc_model::spec::{FileSpec, NodeSpec};
use serde_json::Value;

use crate::ensure_suite::{ensure_run, report};
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, TestResult};
use crate::{TESTER, ensure};

const PYTHON_VECTOR: &str =
    include_str!("../../crates/diskgc-model/tests/models/profiles/python-pycache-checkout.yaml");
const HOME: &str = "/home/tester";

/// The model's positive and checkout cases on a real tmpfs and the real
/// LinuxHost. This proves the vector contract survives the kernel boundary.
pub fn checked_in_python_profile_vector_replays_in_container(ctx: &Ctx) -> TestResult {
    replay_python_vector(ctx, &[])
}

/// Replays the same checked-in expectations with only fixture-owned writers
/// exempted from the deletion oracle. Dind's daemon storage is concurrent
/// state; the profile candidate and all neighboring paths remain checked.
pub(crate) fn replay_python_vector(ctx: &Ctx, concurrent_writers: &[&str]) -> TestResult {
    let vector = ProfileVector::parse(PYTHON_VECTOR)?;
    ensure!(
        vector.host.processes.is_empty(),
        "replay cannot model processes"
    );
    ensure!(vector.host.locks.is_empty(), "replay cannot model locks");
    ensure!(vector.host.events.is_empty(), "replay cannot model events");
    for (path, node) in &vector.host.tree {
        if path == Path::new("/run/diskgc/lock") {
            continue; // Already provisioned on the container's private /run.
        }
        materialize(ctx, path, node)?;
    }
    let mut fixture = vector
        .expected_actions()
        .iter()
        .fold(Fixture::new(), |fixture, path| {
            fixture.eligible(path.clone())
        });
    for path in concurrent_writers {
        fixture = fixture.concurrent_writer(*path);
    }
    let oracle = Oracle::arm(fixture)?;
    let select = format!("profile:{}", vector.profile_name());
    let path = vector.root_path().to_str().ok_or("non-UTF-8 vector path")?;
    let roots = vector
        .roots
        .first()
        .ok_or("replay requires one bound root")?;
    ensure!(
        vector.roots.len() == 1,
        "replay requires exactly one bound root"
    );
    let args = [
        "--json",
        "--no-index",
        "--min-free",
        vector.min_free.as_str(),
        "--select",
        select.as_str(),
        "--roots",
        roots,
        path,
    ];
    let mut dry_args = vec!["--dry-run"];
    dry_args.extend(args);
    let dry = ensure_run(ctx, &dry_args)?;
    let run = ensure_run(ctx, &args)?;
    let verdict = oracle.judge_runs(&[&dry, &run])?;
    dry.expect_code(0)?;
    run.expect_code(vector.expect.reclaim_exit)?;
    let mut actual_plan = plan_verdicts(&report(&dry)?)?;
    actual_plan.sort_by(|a, b| a.candidate.cmp(&b.candidate));
    let mut expected_plan = vector.expect.plan.clone();
    expected_plan.sort_by(|a, b| a.candidate.cmp(&b.candidate));
    let mut wrong_plan = expected_plan.clone();
    let first = wrong_plan
        .first_mut()
        .ok_or("replay vector has no plan entry")?;
    first.candidate = "/deliberately-wrong-candidate".into();
    ensure!(
        check_plan(&actual_plan, &wrong_plan).is_err(),
        "kernel replay accepted a deliberately wrong expected candidate"
    );
    check_plan(&actual_plan, &expected_plan)?;
    let result = report(&run)?;
    let mut actions = paths(&result, "actions")?;
    actions.sort();
    let mut expected_actions = vector.expected_actions().to_vec();
    expected_actions.sort();
    ensure!(
        actions == expected_actions,
        "kernel actions {actions:?}, vector {expected_actions:?}"
    );
    let mut skipped = skip_verdicts(&result)?;
    skipped.sort_by(|a, b| a.candidate.cmp(&b.candidate));
    let mut expected_skipped = vector.expect.skipped.clone();
    expected_skipped.sort_by(|a, b| a.candidate.cmp(&b.candidate));
    ensure!(
        skipped == expected_skipped,
        "kernel skips {skipped:?}, vector {expected_skipped:?}"
    );
    for path in vector.expected_actions() {
        ensure!(!path.exists(), "replayed action left {}", path.display());
    }
    for path in vector.expected_preserved() {
        ensure!(path.exists(), "replay removed protected {}", path.display());
    }
    verdict.assert_clean()
}

fn check_plan(actual: &[PlanVerdict], expected: &[PlanVerdict]) -> TestResult {
    ensure!(
        actual == expected,
        "kernel plan {actual:?}, vector {expected:?}"
    );
    Ok(())
}

fn materialize(ctx: &Ctx, path: &Path, node: &NodeSpec) -> TestResult {
    let relative = path
        .strip_prefix(HOME)
        .map_err(|_| format!("replay path {} lies outside {HOME}", path.display()))?;
    let relative = relative
        .to_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("invalid replay path {}", path.display()))?;
    match node {
        NodeSpec::Dir(dir) => {
            ensure!(
                dir.attrs == Default::default(),
                "replay directory attributes unsupported: {path:?}"
            );
            ctx.home.mkdirs(relative, 0o700, TESTER)?;
            for (name, child) in &dir.entries {
                materialize(ctx, &path.join(name), child)?;
            }
        }
        NodeSpec::File(file) => {
            let content = plain_content(file, path)?;
            let parent = Path::new(relative)
                .parent()
                .ok_or("replay file has no parent")?;
            let parent = parent.to_str().ok_or("non-UTF-8 parent")?;
            ctx.home.mkdirs(parent, 0o700, TESTER)?;
            ctx.home.write_file(relative, &content, 0o600, TESTER)?;
        }
    }
    Ok(())
}

fn plain_content(file: &FileSpec, path: &Path) -> Result<Vec<u8>, String> {
    let simple = file.special.is_none()
        && file.symlink.is_none()
        && file.link.is_none()
        && file.mtime.is_none()
        && file.uid.is_none()
        && file.gid.is_none()
        && file.mode.is_none()
        && !file.cachedir_tag
        && !file.bound;
    if !simple {
        return Err(format!(
            "replay cannot materialize file attributes at {}",
            path.display()
        ));
    }
    let content = file.content.as_deref().unwrap_or("").as_bytes();
    let size = file.size.map_or(content.len(), |size| size.0 as usize);
    if size > 64 * 1024 * 1024 || size < content.len() {
        return Err(format!(
            "replay file size at {} is unsupported",
            path.display()
        ));
    }
    let mut bytes = vec![0; size];
    bytes[..content.len()].copy_from_slice(content);
    Ok(bytes)
}

fn paths(report: &Value, field: &str) -> Result<Vec<std::path::PathBuf>, String> {
    report[field]
        .as_array()
        .ok_or_else(|| format!("{field} is not an array"))?
        .iter()
        .map(|entry| {
            entry["candidate"]
                .as_str()
                .map(Into::into)
                .ok_or_else(|| format!("{field} entry has no candidate: {entry}"))
        })
        .collect()
}

fn plan_verdicts(report: &Value) -> Result<Vec<PlanVerdict>, String> {
    report["plan"]
        .as_array()
        .ok_or("plan is not an array".to_owned())?
        .iter()
        .map(|entry| {
            let candidate = entry["candidate"]
                .as_str()
                .ok_or_else(|| format!("plan entry has no candidate: {entry}"))?
                .into();
            Ok(PlanVerdict {
                candidate,
                reason: entry["reason"].as_str().map(str::to_owned),
                source: entry["build_evidence"]["source"].as_str().map(Into::into),
            })
        })
        .collect()
}

fn skip_verdicts(report: &Value) -> Result<Vec<SkipVerdict>, String> {
    report["skipped"]
        .as_array()
        .ok_or("skipped is not an array".to_owned())?
        .iter()
        .map(|entry| {
            Ok(SkipVerdict {
                candidate: entry["candidate"]
                    .as_str()
                    .ok_or_else(|| format!("skip has no candidate: {entry}"))?
                    .into(),
                reason: entry["reason"]
                    .as_str()
                    .ok_or_else(|| format!("skip has no reason: {entry}"))?
                    .to_owned(),
            })
        })
        .collect()
}
