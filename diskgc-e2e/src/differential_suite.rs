//! One model/kernel contract replay on an isolated tmpfs. The two sides run
//! the same executor request; comparison includes the outcome and the set of
//! fixture objects that actually disappeared, but excludes incidental inode
//! numbers, mount ids, timings, and Host call text.

use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{fs, io};

use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::host::{Base, Dev, Errno, Host};
use diskgc_core::profile::Library;
use diskgc_core::resolve::{ResolveError, Want, resolve};
use diskgc_core::units::parse_size;
use diskgc_model::ModelHost;
use diskgc_model::profile_vector::{PlanVerdict, ProfileVector, SkipVerdict};
use diskgc_model::spec::{Bytes, DevSpec, DirSpec, FileSpec, MountSpec, NodeSpec};
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
const PYTHON_IN_USE_VECTOR: &str =
    include_str!("../../crates/diskgc-model/tests/models/profiles/python-pycache-in-use.yaml");
const DANGLING_LINK: &str = "/home/tester/project/dangling-link";

// The fresh kernel tmpfs gives every new inode a current ctime. These
// checked-in vectors need an eligible candidate whose *newest mtime or
// ctime* is days old. Setting mtime cannot reproduce the old ctime; a real
// LinuxHost must therefore skip that candidate as not_idle.
#[cfg(test)]
const HISTORICAL_CTIME_VECTORS: &[&str] = &[
    "autotools-source-anchor.yaml",
    "cmake-source-anchor.yaml",
    "meson-source-anchor.yaml",
    "west-build-anchored.yaml",
    "xdg-cache-entry-v2-match.yaml",
];

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

/// The checked-in in-use vector names a nominal Python executable. The
/// container probe supplies the same open-file and cwd evidence, so remove
/// only the nominal executable/mapping/environment fields before replay.
/// The unchanged expected in-use verdict still comes from the vector.
pub fn python_in_use_vector_matches_linux(ctx: &Ctx) -> TestResult {
    replay_model(ctx, python_in_use_probe_vector()?, false)
}

/// A Python cache containing a mounted filesystem must remain untouched.
/// The stacked variant has two distinct tmpfs mounts at the same point;
/// only the topmost mount's file is visible to both hosts.
pub fn python_nested_mount_matches_linux(ctx: &Ctx) -> TestResult {
    replay_model(ctx, python_mounted_cache_vector(false)?, false)
}

pub fn python_stacked_mount_matches_linux(ctx: &Ctx) -> TestResult {
    replay_model(ctx, python_mounted_cache_vector(true)?, false)
}

fn python_mounted_cache_vector(stacked: bool) -> Result<ProfileVector, String> {
    let mut vector = ProfileVector::parse(PYTHON_VECTOR)?;
    let cache = PathBuf::from("/home/tester/project/app/__pycache__");
    let at = cache.join("mounted");
    let mut mount = MountSpec {
        id: 3,
        at: at.clone(),
        fs: "tmpfs".into(),
        dev: DevSpec(Dev {
            major: 0,
            minor: 77,
        }),
        size: Bytes(1 << 20),
        other_used: Bytes(0),
        parent: Some(2),
        read_only: false,
        statx_missing: Vec::new(),
    };
    vector.host.mounts.push(mount.clone());
    if stacked {
        mount.id = 4;
        mount.dev = DevSpec(Dev {
            major: 0,
            minor: 78,
        });
        mount.parent = Some(3);
        vector.host.mounts.push(mount);
    }
    vector.host.tree.insert(
        at.join("visible"),
        NodeSpec::File(FileSpec {
            content: Some("topmost mount".into()),
            ..FileSpec::default()
        }),
    );
    let plan = vector
        .expect
        .plan
        .iter_mut()
        .find(|entry| entry.candidate == cache)
        .ok_or("Python cache candidate missing from expectation")?;
    plan.reason = Some("contains_mount".into());
    vector.expect.actions.clear();
    vector.expect.removed.clear();
    vector.expect.skipped.push(SkipVerdict {
        candidate: cache,
        reason: "contains_mount".into(),
    });
    vector.expect.preserved.push(at.join("visible"));
    Ok(vector)
}

fn python_in_use_probe_vector() -> Result<ProfileVector, String> {
    let mut vector = ProfileVector::parse(PYTHON_IN_USE_VECTOR)?;
    vector.host.tree.remove(Path::new("/usr/bin/python"));
    let [process] = vector.host.processes.as_mut_slice() else {
        return Err("Python in-use vector must have exactly one process".into());
    };
    process.exe = None;
    process.maps.clear();
    process.environ.clear();
    Ok(vector)
}

macro_rules! replay_fixture {
    ($function:ident, $file:literal) => {
        pub fn $function(ctx: &Ctx) -> TestResult {
            replay_vector(
                ctx,
                include_str!(concat!(
                    "../../crates/diskgc-model/tests/models/profiles/",
                    $file
                )),
                false,
            )
        }
    };
}

replay_fixture!(cargo_target_vector, "cargo-target-locked.yaml");
replay_fixture!(chromium_tmp_vector, "chromium-tmp-match.yaml");
replay_fixture!(claude_code_tmp_vector, "claude-code-tmp-match.yaml");
replay_fixture!(
    claude_code_versions_vector,
    "claude-code-versions-versions.yaml"
);
replay_fixture!(codex_releases_vector, "codex-releases-versions.yaml");
replay_fixture!(
    cursor_agent_versions_vector,
    "cursor-agent-versions-versions.yaml"
);
replay_fixture!(cursor_extensions_vector, "cursor-extensions-versions.yaml");
replay_fixture!(go_mod_cache_vector, "go-mod-cache-match.yaml");
replay_fixture!(nix_user_cache_vector, "nix-user-cache-match.yaml");
replay_fixture!(node_compile_cache_vector, "node-compile-cache-match.yaml");
replay_fixture!(
    node_compile_cache_nested_vector,
    "node-compile-cache-nested-claude.yaml"
);
replay_fixture!(playwright_tmp_vector, "playwright-tmp-match.yaml");
replay_fixture!(pytest_cache_vector, "pytest-cache-match.yaml");
replay_fixture!(renode_tmp_vector, "renode-tmp-match.yaml");
replay_fixture!(zephyr_tmp_vector, "zephyr-tmp-match.yaml");

/// One container runs 100 distinct, deterministic model trees. Every case
/// gets a disjoint bound root, so earlier cases cannot become candidates for
/// later ones. The checked-in Python expectation supplies an independent
/// base; generated branches add eligible, checkout-protected, and decoy
/// paths with their own independent expectations.
pub fn generated_python_models_match_linux(ctx: &Ctx) -> TestResult {
    for index in 0..100 {
        let vector = generated_python_vector(index)?;
        replay_model(ctx, vector, false)
            .map_err(|error| format!("generated Python model {index}: {error}"))?;
    }
    Ok(())
}

fn generated_python_vector(index: u32) -> Result<ProfileVector, String> {
    let root = format!("/home/tester/differential-{index:03}");
    let yaml = PYTHON_VECTOR.replace("/home/tester/project", &root);
    let mut vector = ProfileVector::parse(&yaml)?;
    let base_cache = PathBuf::from(format!("{root}/app/__pycache__"));
    let Some(NodeSpec::Dir(cache)) = vector.host.tree.get_mut(&base_cache) else {
        return Err("generated Python vector is missing its base cache".into());
    };
    cache.entries.insert(
        format!("generated-{index}.pyc"),
        NodeSpec::File(FileSpec {
            size: Some(Bytes(1024 + u64::from(index) * 97)),
            ..FileSpec::default()
        }),
    );

    let extra = PathBuf::from(format!("{root}/variation/__pycache__"));
    match index % 4 {
        0 => {} // The varied base cache alone is eligible.
        1 => {
            let mut cache = DirSpec::default();
            cache.entries.insert(
                "extra.pyc".into(),
                NodeSpec::File(FileSpec {
                    size: Some(Bytes(512 + u64::from(index) * 31)),
                    ..FileSpec::default()
                }),
            );
            vector.host.tree.insert(extra.clone(), NodeSpec::Dir(cache));
            vector.expect.plan.push(PlanVerdict {
                candidate: extra.clone(),
                reason: None,
                source: None,
            });
            vector.expect.actions.push(extra.clone());
            vector.expect.removed.push(extra);
        }
        2 => {
            let mut git = DirSpec::default();
            git.entries
                .insert("HEAD".into(), NodeSpec::File(FileSpec::default()));
            let mut cache = DirSpec::default();
            cache.entries.insert(".git".into(), NodeSpec::Dir(git));
            vector.host.tree.insert(extra.clone(), NodeSpec::Dir(cache));
            vector.expect.plan.push(PlanVerdict {
                candidate: extra.clone(),
                reason: Some("contains_checkout".into()),
                source: None,
            });
            vector.expect.skipped.push(SkipVerdict {
                candidate: extra.clone(),
                reason: "contains_checkout".into(),
            });
            vector.expect.preserved.push(extra.join(".git/HEAD"));
        }
        3 => {
            let decoy = PathBuf::from(format!("{root}/variation/__pycache__-backup"));
            let mut cache = DirSpec::default();
            cache
                .entries
                .insert("keep".into(), NodeSpec::File(FileSpec::default()));
            vector.host.tree.insert(decoy.clone(), NodeSpec::Dir(cache));
            vector.expect.preserved.push(decoy.join("keep"));
        }
        _ => unreachable!(),
    }
    Ok(vector)
}

fn replay_vector(ctx: &Ctx, yaml: &str, with_dangling_link: bool) -> TestResult {
    replay_model(ctx, ProfileVector::parse(yaml)?, with_dangling_link)
}

fn replay_model(ctx: &Ctx, mut vector: ProfileVector, with_dangling_link: bool) -> TestResult {
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
    let materialized = model_materialize::materialize(ctx, &vector.host)?;
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
    let mut args = vec![
        "--json",
        "--no-index",
        "--min-free",
        vector.min_free.as_str(),
        "--select",
        &select,
    ];
    if !vector.roots.is_empty() {
        args.push("--roots");
        args.extend(vector.roots.iter().map(String::as_str));
    }
    args.push(path);
    let mut dry_args = vec!["--dry-run"];
    dry_args.extend(&args);
    let dry = ensure_run(ctx, &dry_args)?;
    materialized.assert_holding()?;
    let reclaim = ensure_run(ctx, &args)?;
    materialized.assert_holding()?;
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
    materialized.finish()?;
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
    let library = if vector.user_profiles.is_empty() {
        Library::builtin().map_err(|e| e.to_string())?
    } else {
        Library::with_user_profiles(&vector.user_profiles).map_err(|e| e.to_string())?
    };
    let tier = Tier::select(
        &format!("profile:{}", vector.profile),
        &library,
        (!vector.roots.is_empty()).then_some(vector.roots.as_slice()),
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
    fn generated_models_have_independent_expectations() {
        let library = Library::builtin().expect("built-in library");
        let mut roots = std::collections::BTreeSet::new();
        for index in 0..100 {
            let vector = generated_python_vector(index).expect("generated model");
            assert!(
                roots.insert(vector.path.clone()),
                "generated roots must differ"
            );
            assert_eq!(vector.expect.actions.len(), 1 + usize::from(index % 4 == 1));
            vector
                .check(&library)
                .expect("independent generated expectation");
        }
    }

    #[test]
    fn projected_in_use_vector_keeps_its_independent_expected_verdict() {
        let mut vector = python_in_use_probe_vector().expect("checked-in in-use vector");
        adapt_home_mount(&mut vector);
        model_materialize::validate(&vector.host).expect("probe-supported process shape");
        vector
            .check(&Library::builtin().expect("built-in library"))
            .expect("the checked-in in-use expectation");
    }

    #[test]
    fn nested_and_stacked_mounts_have_independent_refusal_expectations() {
        let library = Library::builtin().expect("built-in library");
        for stacked in [false, true] {
            let mut vector = python_mounted_cache_vector(stacked).expect("mounted vector");
            vector.check(&library).expect("mount refusal expectation");
            adapt_home_mount(&mut vector);
            model_materialize::validate(&vector.host).expect("supported tmpfs mount layout");
            let last = vector.host.mounts.len() - 1;
            vector.host.mounts[last].dev = vector.host.mounts[last - 1].dev;
            assert!(
                model_materialize::validate(&vector.host)
                    .expect_err("bind representation must fail")
                    .contains("bind mounts sharing a device are not modelled"),
            );
            vector.host.mounts[last].dev = DevSpec(Dev {
                major: 0,
                minor: 99,
            });
            vector.host.mounts[last].fs = "ext4".into();
            assert!(
                model_materialize::validate(&vector.host)
                    .expect_err("non-tmpfs inner mount must fail")
                    .contains("only scratch tmpfs mounts are supported"),
            );
            vector.host.mounts[last].fs = "tmpfs".into();
            let mount_at = vector.host.mounts[last].at.clone();
            vector
                .host
                .tree
                .insert(mount_at, NodeSpec::Dir(DirSpec::default()));
            assert!(
                model_materialize::validate(&vector.host)
                    .expect_err("explicit mounted root must fail")
                    .contains("explicit mount-root attributes are unsupported"),
            );
        }
    }

    #[test]
    fn every_checked_in_profile_vector_has_an_explicit_replay_classification() {
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
        let mut replayable = 0;
        let mut historical_ctime = 0;
        let mut unsupported_tree = 0;
        for path in files {
            let yaml = fs::read_to_string(&path).expect("read profile fixture");
            let mut vector = ProfileVector::parse(&yaml).expect("parse profile fixture");
            adapt_home_mount(&mut vector);
            let name = path.file_name().expect("fixture name").to_string_lossy();
            match model_materialize::validate(&vector.host) {
                Ok(()) => {
                    if HISTORICAL_CTIME_VECTORS.contains(&name.as_ref()) {
                        historical_ctime += 1;
                        eprintln!("model-only {name}: historical ctime cannot be created on tmpfs");
                    } else {
                        replayable += 1;
                        eprintln!("replayable {name}");
                    }
                }
                Err(reason) => {
                    assert!(
                        !HISTORICAL_CTIME_VECTORS.contains(&name.as_ref()),
                        "{name}: ctime classification overlaps unsupported tree: {reason}"
                    );
                    unsupported_tree += 1;
                    assert!(
                        reason.contains("model-only")
                            || reason.contains("outside the container scratch mounts"),
                        "{name}: unclassified materialization failure: {reason}"
                    );
                    eprintln!("model-only {name}: {reason}");
                }
            }
        }
        assert_eq!(historical_ctime, HISTORICAL_CTIME_VECTORS.len());
        assert_eq!(unsupported_tree, 7);
        assert_eq!(replayable, 18);
        assert_eq!(
            crate::suite::TESTS
                .iter()
                .filter(|test| test
                    .name
                    .ends_with("_vector_matches_linux_outcomes_and_deletion_diff"))
                .count(),
            replayable,
            "every replayable vector needs a registered container test"
        );
    }
}
