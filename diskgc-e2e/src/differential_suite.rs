//! One model/kernel contract replay on an isolated tmpfs. The two sides run
//! the same executor request; comparison includes the outcome and the set of
//! fixture objects that actually disappeared, but excludes incidental inode
//! numbers, mount ids, timings, and Host call text.

use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{fs, io};

use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::host::{Base, Errno, Host};
use diskgc_core::profile::Library;
use diskgc_core::resolve::{ResolveError, Want, resolve};
use diskgc_core::units::parse_size;
use diskgc_model::ModelHost;
use diskgc_model::profile_vector::{PlanVerdict, ProfileVector, SkipVerdict};
use diskgc_model::spec::{FileSpec, NodeSpec};
use serde_json::Value;

use crate::ensure;
use crate::ensure_suite::{ensure_run, report};
use crate::model_materialize;
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, TestResult};

const PYTHON_VECTOR: &str =
    include_str!("../../crates/diskgc-model/tests/models/profiles/python-pycache-checkout.yaml");
const BUN_VECTOR: &str =
    include_str!("../../crates/diskgc-model/tests/models/profiles/bun-cache-match.yaml");
const MYPY_VECTOR: &str =
    include_str!("../../crates/diskgc-model/tests/models/profiles/mypy-cache-match.yaml");
const DANGLING_LINK: &str = "/home/tester/project/dangling-link";

#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    dry_plan: Vec<PlanVerdict>,
    exit: i32,
    actions: Vec<PathBuf>,
    skipped: Vec<SkipVerdict>,
    removed: Vec<PathBuf>,
}

/// Compare the checked-in Python vector on ModelHost and the real binary.
/// Mount numbers in the model are nominal; only the effects of selection and
/// reclaim are compared. Both executions use an unreachable 100% goal.
pub fn python_vector_matches_linux_outcomes_and_deletion_diff(ctx: &Ctx) -> TestResult {
    replay_vector(ctx, PYTHON_VECTOR, true)
}

/// Exercise a time-qualified cache root on both hosts.
pub fn bun_vector_matches_linux_outcomes_and_deletion_diff(ctx: &Ctx) -> TestResult {
    replay_vector(ctx, BUN_VECTOR, false)
}

/// Exercise a cache marker and explicit old mtimes on both hosts.
pub fn mypy_vector_matches_linux_outcomes_and_deletion_diff(ctx: &Ctx) -> TestResult {
    replay_vector(ctx, MYPY_VECTOR, false)
}

fn replay_vector(ctx: &Ctx, yaml: &str, with_dangling_link: bool) -> TestResult {
    let mut vector = ProfileVector::parse(yaml)?;
    // The checked-in model's `/home` ext4 is not a rootless-container mount.
    // Place the same tree on the harness's `/home/tester` tmpfs. This changes
    // no candidate pathname or selection expectation.
    adapt_home_mount(&mut vector);
    if with_dangling_link {
        vector.host.tree.insert(
            DANGLING_LINK.into(),
            NodeSpec::File(FileSpec {
                symlink: Some("/missing-differential-target".into()),
                ..FileSpec::default()
            }),
        );
    }
    // The checked-in expectation is independent of both host outcomes. A
    // shared no-op regression must fail here, before comparing the hosts.
    vector.check(&Library::builtin().map_err(|e| e.to_string())?)?;
    let model = model_outcome(&vector)?;
    model_materialize::materialize(ctx, &vector.host)?;
    if with_dangling_link {
        require_present(&[DANGLING_LINK.into()])?;
    }
    require_present(&vector.expect.removed)?;
    require_present(&vector.expect.preserved)?;

    let fixture = vector
        .expected_actions()
        .iter()
        .fold(Fixture::new(), |fixture, path| {
            fixture.eligible(path.clone())
        });
    let oracle = Oracle::arm(fixture)?;
    let select = format!("profile:{}", vector.profile_name());
    let path = vector.root_path().to_str().ok_or("non-UTF-8 vector path")?;
    let root = vector.roots.first().ok_or("vector needs a bound root")?;
    ensure!(
        vector.roots.len() == 1,
        "vector needs exactly one bound root"
    );
    let args = [
        "--json",
        "--no-index",
        "--min-free",
        vector.min_free.as_str(),
        "--select",
        &select,
        "--roots",
        root,
        path,
    ];
    let mut dry_args = vec!["--dry-run"];
    dry_args.extend(args);
    let dry = ensure_run(ctx, &dry_args)?;
    let reclaim = ensure_run(ctx, &args)?;
    oracle.judge_runs(&[&dry, &reclaim])?.assert_clean()?;
    dry.expect_code(0)?;
    let linux = linux_outcome(
        &vector,
        &report(&dry)?,
        &report(&reclaim)?,
        reclaim.code.ok_or("Linux run ended without an exit code")?,
    )?;
    require_absent(&vector.expect.removed)?;
    require_present(&vector.expect.preserved)?;
    if with_dangling_link {
        require_present(&[DANGLING_LINK.into()])?;
    }
    ensure!(
        model == linux,
        "ModelHost/LinuxHost differential mismatch:\nmodel: {model:#?}\nlinux: {linux:#?}"
    );
    Ok(())
}

fn adapt_home_mount(vector: &mut ProfileVector) {
    if let Some(mount) = vector
        .host
        .mounts
        .iter_mut()
        .find(|mount| mount.at == Path::new("/home"))
    {
        mount.at = "/home/tester".into();
        mount.fs = "tmpfs".into();
    }
}

fn model_outcome(vector: &ProfileVector) -> Result<Outcome, String> {
    let library = Library::builtin().map_err(|e| e.to_string())?;
    let tier = Tier::select(
        &format!("profile:{}", vector.profile),
        &library,
        Some(&vector.roots),
        vector.host.own_uid.unwrap_or(1000),
    )?;
    let request = Request {
        path: vector.path.clone(),
        min_free: parse_size(&vector.min_free)?,
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        tier,
    };
    let paths = fixture_paths(&vector.host.tree);
    let dry_host = ModelHost::new(&vector.host).map_err(|e| e.to_string())?;
    let uid = executor::privileges(&dry_host, false).map_err(|e| e.to_string())?;
    let dry = executor::dry_run(&dry_host, &request, &[uid], &mut |_| {});
    ensure!(
        dry.errors.is_empty(),
        "model dry-run errors: {:?}",
        dry.errors
    );
    let run_host = ModelHost::new(&vector.host).map_err(|e| e.to_string())?;
    let uid = executor::privileges(&run_host, false).map_err(|e| e.to_string())?;
    let run = executor::ensure(&run_host, &request, &[uid], &mut |_| {});
    ensure!(
        run.errors.is_empty(),
        "model reclaim errors: {:?}",
        run.errors
    );
    let removed = paths
        .into_iter()
        .filter_map(|path| match model_object_absent(&run_host, &path) {
            Ok(true) => Some(Ok(path)),
            Ok(false) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut outcome = Outcome {
        dry_plan: dry
            .plan
            .iter()
            .map(|entry| PlanVerdict {
                candidate: entry.candidate.clone(),
                reason: entry.reason.map(str::to_owned),
                source: entry.build_evidence.as_ref().map(|e| e.source.clone()),
            })
            .collect(),
        exit: i32::from(run.exit_code()),
        actions: run
            .actions
            .iter()
            .map(|entry| entry.candidate.clone())
            .collect(),
        skipped: run
            .skipped
            .iter()
            .map(|entry| SkipVerdict {
                candidate: entry.candidate.clone(),
                reason: entry.reason.to_owned(),
            })
            .collect(),
        removed,
    };
    outcome.sort();
    Ok(outcome)
}

/// ModelHost::stat_path refuses a symlink even when the symlink entry
/// exists. Stat its final name relative to an independently resolved parent,
/// just as the Linux lstat side does. A missing parent means the entry is
/// absent; other resolution errors are test failures.
fn model_object_absent(host: &ModelHost, path: &Path) -> Result<bool, String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("model path {} has no parent", path.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| format!("model path {} has no final name", path.display()))?;
    let dir = match resolve(host, parent, Want::Directory) {
        Ok(dir) => dir,
        Err(ResolveError::Missing { .. }) => return Ok(true),
        Err(error) => {
            return Err(format!(
                "resolve model parent {} for deletion diff: {error}",
                parent.display()
            ));
        }
    };
    match host.statx(Base::Dir(&dir.fd), Path::new(name)) {
        Ok(_) => Ok(false),
        Err(error) if error.is(Errno::ENOENT) => Ok(true),
        Err(error) => Err(format!(
            "stat model entry {} for deletion diff: {error}",
            path.display()
        )),
    }
}

fn linux_outcome(
    vector: &ProfileVector,
    dry: &Value,
    run: &Value,
    code: i32,
) -> Result<Outcome, String> {
    let paths = fixture_paths(&vector.host.tree);
    let mut outcome = Outcome {
        dry_plan: dry["plan"]
            .as_array()
            .ok_or("Linux dry plan is not an array")?
            .iter()
            .map(|entry| {
                Ok(PlanVerdict {
                    candidate: entry["candidate"]
                        .as_str()
                        .ok_or("plan candidate missing")?
                        .into(),
                    reason: entry["reason"].as_str().map(str::to_owned),
                    source: entry["build_evidence"]["source"].as_str().map(Into::into),
                })
            })
            .collect::<Result<_, &str>>()?,
        exit: code,
        actions: report_paths(run, "actions")?,
        skipped: run["skipped"]
            .as_array()
            .ok_or("Linux skips not an array")?
            .iter()
            .map(|entry| {
                Ok(SkipVerdict {
                    candidate: entry["candidate"]
                        .as_str()
                        .ok_or("skip candidate missing")?
                        .into(),
                    reason: entry["reason"]
                        .as_str()
                        .ok_or("skip reason missing")?
                        .to_owned(),
                })
            })
            .collect::<Result<_, &str>>()?,
        removed: missing_paths(paths)?,
    };
    outcome.sort();
    Ok(outcome)
}

/// `Path::exists` follows symlinks and calls a dangling link missing. The
/// deletion diff concerns directory entries, so use lstat semantics. An
/// unexpected I/O error is not evidence of deletion.
fn object_absent(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(format!(
            "lstat {} for deletion diff: {error}",
            path.display()
        )),
    }
}

fn missing_paths(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>, String> {
    let mut missing = Vec::new();
    for path in paths {
        if object_absent(&path)? {
            missing.push(path);
        }
    }
    Ok(missing)
}

fn require_present(paths: &[PathBuf]) -> Result<(), String> {
    for path in paths {
        ensure!(
            !object_absent(path)?,
            "expected fixture object {} is absent",
            path.display()
        );
    }
    Ok(())
}

fn require_absent(paths: &[PathBuf]) -> Result<(), String> {
    for path in paths {
        ensure!(
            object_absent(path)?,
            "expected deleted object {} survived",
            path.display()
        );
    }
    Ok(())
}

fn report_paths(report: &Value, field: &str) -> Result<Vec<PathBuf>, String> {
    report[field]
        .as_array()
        .ok_or_else(|| format!("{field} not an array"))?
        .iter()
        .map(|entry| {
            entry["candidate"]
                .as_str()
                .map(Into::into)
                .ok_or_else(|| format!("{field} candidate missing: {entry}"))
        })
        .collect()
}

fn fixture_paths(tree: &std::collections::BTreeMap<PathBuf, NodeSpec>) -> Vec<PathBuf> {
    fn walk(path: &Path, node: &NodeSpec, out: &mut Vec<PathBuf>) {
        out.push(path.to_owned());
        if let NodeSpec::Dir(dir) = node {
            for (name, child) in &dir.entries {
                walk(&path.join(name), child, out);
            }
        }
    }
    let mut paths = Vec::new();
    for (path, node) in tree {
        if path != Path::new("/run/diskgc/lock") {
            walk(path, node, &mut paths);
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

impl Outcome {
    fn sort(&mut self) {
        self.dry_plan.sort_by(|a, b| a.candidate.cmp(&b.candidate));
        self.actions.sort();
        self.skipped.sort_by(|a, b| a.candidate.cmp(&b.candidate));
        self.removed.sort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_in_expectation_rejects_a_vacuous_model_and_linux_agreement() {
        let mut vector = ProfileVector::parse(PYTHON_VECTOR).expect("checked-in vector");
        // If both hosts saw this wrong tree they could agree that no action
        // happened. The vector's independent plan/action expectations bite.
        vector
            .host
            .tree
            .remove(Path::new("/home/tester/project/app/__pycache__"));
        vector
            .host
            .tree
            .remove(Path::new("/home/tester/project/checkout/__pycache__"));
        let library = Library::builtin().expect("built-in library");
        let error = vector.check(&library).expect_err("vacuous model must fail");
        assert!(error.contains("dry-run plan"), "{error}");
    }

    #[test]
    fn every_checked_in_profile_vector_has_an_explicit_materialization_classification() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../crates/diskgc-model/tests/models/profiles");
        let mut files: Vec<_> = fs::read_dir(&directory)
            .expect("profile fixture directory")
            .map(|entry| entry.expect("fixture directory entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "yaml"))
            .collect();
        files.sort();
        assert_eq!(
            files.len(),
            30,
            "new profile fixture needs materialization triage"
        );
        let mut accepted = 0;
        for path in files {
            let yaml = fs::read_to_string(&path).expect("read profile fixture");
            let mut vector = ProfileVector::parse(&yaml).expect("parse profile fixture");
            adapt_home_mount(&mut vector);
            let name = path.file_name().expect("fixture name").to_string_lossy();
            match model_materialize::validate(&vector.host) {
                Ok(()) => {
                    accepted += 1;
                    eprintln!("materializable {name}");
                }
                Err(reason) => {
                    assert!(
                        reason.contains("model-only")
                            || reason.contains("outside the container scratch mounts"),
                        "{name}: unclassified materialization failure: {reason}"
                    );
                    eprintln!("model-only {name}: {reason}");
                }
            }
        }
        assert!(
            accepted >= 3,
            "at least the three kernel replay fixtures must materialize"
        );
    }
}
