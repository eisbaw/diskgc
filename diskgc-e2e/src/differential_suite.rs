//! One model/kernel contract replay on an isolated tmpfs. The two sides run
//! the same executor request; comparison includes the outcome and the set of
//! fixture objects that actually disappeared, but excludes incidental inode
//! numbers, mount ids, timings, and Host call text.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{fs, io};

use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::host::{Base, Dev, Errno, Host};
use diskgc_core::profile::{Library, describe, parse_profiles};
use diskgc_core::resolve::{ResolveError, Want, resolve};
use diskgc_core::units::parse_size;
use diskgc_model::ModelHost;
use diskgc_model::profile_vector::{PlanVerdict, ProfileVector, SkipVerdict};
use diskgc_model::spec::{
    Bytes, DevSpec, DirSpec, FileSpec, GateSpec, HostModel, LockModeSpec, LockSpec, Mode,
    MountSpec, NodeSpec, ProcAccess, ProcessSpec, Special, StatxField, WallTime,
};
use diskgc_model::state::Fault;
use serde_json::Value;

use crate::ensure_suite::{ensure_run, report};
use crate::holder::Holder;
use crate::lock_release::{RELEASE_BOUND, flock_free_within_release_bound};
use crate::model_materialize;
use crate::oracle::{Fixture, Oracle};
use crate::scratch::Scratch;
use crate::suite::{Ctx, TestResult};
use crate::{TESTER, ensure};

const PYTHON_VECTOR: &str =
    include_str!("../../crates/diskgc-model/tests/models/profiles/python-pycache-checkout.yaml");
const BUN_VECTOR: &str =
    include_str!("../../crates/diskgc-model/tests/models/profiles/bun-cache-match.yaml");
const MYPY_VECTOR: &str =
    include_str!("../../crates/diskgc-model/tests/models/profiles/mypy-cache-match.yaml");
const PYTHON_IN_USE_VECTOR: &str =
    include_str!("../../crates/diskgc-model/tests/models/profiles/python-pycache-in-use.yaml");
const BRAVE_BROWSER_VECTOR: &str =
    include_str!("../../crates/diskgc-model/tests/models/profiles/brave-browser-cache-lock.yaml");
const CHROME_BROWSER_VECTOR: &str =
    include_str!("../../crates/diskgc-model/tests/models/profiles/chrome-browser-cache-lock.yaml");
const CHROMIUM_BROWSER_VECTOR: &str = include_str!(
    "../../crates/diskgc-model/tests/models/profiles/chromium-browser-cache-lock.yaml"
);
const EDGE_BROWSER_VECTOR: &str =
    include_str!("../../crates/diskgc-model/tests/models/profiles/edge-browser-cache-lock.yaml");
const PYTEST_UNSAFE_VECTOR: &str = include_str!(
    "../../crates/diskgc-model/tests/models/profiles/pytest-tmp-unsafe-live-no-fd.yaml"
);
const PYTEST_UNSAFE_PROFILE: &str = "- name: pytest-tmp-unsafe-probe
  find: {roots: [/tmp], paths: [\"pytest-of-*/pytest-*/*\"], type: dir}
  action: delete
  examples: {match: [/tmp/pytest-of-tester/pytest-0/test_active0]}
";
const PYTEST_ACTIVE: &str = "/tmp/pytest-of-tester/pytest-0/test_active0";
const PYTEST_UNRELATED: &str = "/home/tester/pytest-unrelated-open-file";
const PYTEST_RETAINED: &str = "/tmp/differential-persistent/payload";
const WEST_VECTOR: &str =
    include_str!("../../crates/diskgc-model/tests/models/profiles/west-build-anchored.yaml");
const XDG_VECTOR: &str =
    include_str!("../../crates/diskgc-model/tests/models/profiles/xdg-cache-entry-v2-match.yaml");
const CMAKE_VECTOR: &str =
    include_str!("../../crates/diskgc-model/tests/models/profiles/cmake-source-anchor.yaml");
const AUTOTOOLS_VECTOR: &str =
    include_str!("../../crates/diskgc-model/tests/models/profiles/autotools-source-anchor.yaml");
const MESON_VECTOR: &str =
    include_str!("../../crates/diskgc-model/tests/models/profiles/meson-source-anchor.yaml");
const CONTAINERS_IMAGE_VECTOR: &str =
    include_str!("../../crates/diskgc-model/tests/models/profiles/containers-image-age.yaml");
/// The withdrawn `containers-image-tmp` matching set as a user profile
/// (TASK-112 D2); it must equal the vector's `user_profiles`.
const CONTAINERS_IMAGE_PROFILE: &str = "- name: containers-image-probe
  find: {roots: [/var/tmp], paths: [\"buildah*\", \"container_images_storage*\"], type: any}
  min_idle_for: 3d
  action: delete
  examples: {match: [/var/tmp/buildah3558241755, /var/tmp/container_images_storage528829445]}
";
const CONTAINERS_IMAGE_PROBE: &str = "containers-image-probe";
// The sketch has several nonrepresentable fields. Isolate its mount accounting
// in a second probe so both event and exact-space refusals are demonstrated.
const NON_PROFILE_MODELS: &[(&str, &str, &str, Option<&str>)] = &[
    (
        "docker-bundled.yaml",
        include_str!("../../crates/diskgc-model/tests/models/docker-bundled.yaml"),
        "command/docker is model-only",
        None,
    ),
    // The real-kernel counterpart is host_flock_release_only_by_close
    // (`held_after_parent_close`, `released_after_last_close`).
    (
        "hostlock/held-by-a-remaining-reference.yaml",
        include_str!(
            "../../crates/diskgc-model/tests/models/hostlock/held-by-a-remaining-reference.yaml"
        ),
        "events is model-only",
        None,
    ),
    (
        "hostlock/released-by-the-last-reference.yaml",
        include_str!(
            "../../crates/diskgc-model/tests/models/hostlock/released-by-the-last-reference.yaml"
        ),
        "events is model-only",
        None,
    ),
    (
        "sketch/locked-cargo-target.yaml",
        include_str!("../../crates/diskgc-model/tests/models/sketch/locked-cargo-target.yaml"),
        "events is model-only",
        Some("other_used is model-only"),
    ),
];
type NonProfileModelCase = (&'static str, HostModel, &'static str, Option<&'static str>);
#[cfg(test)]
const BROWSER_VECTORS: &[&str] = &[
    BRAVE_BROWSER_VECTOR,
    CHROME_BROWSER_VECTOR,
    CHROMIUM_BROWSER_VECTOR,
    EDGE_BROWSER_VECTOR,
];
pub(crate) const CARGO_TARGET_VECTOR: &str =
    include_str!("../../crates/diskgc-model/tests/models/profiles/cargo-target-locked.yaml");
pub(crate) const CARGO_LOCK_PATH: &str = "/home/tester/project/good/target/debug/.cargo-lock";
const CARGO_TARGET_PATH: &str = "/home/tester/project/good/target";
const DANGLING_LINK: &str = "/home/tester/project/dangling-link";

// This inventory is the contract for every original checked-in profile
// vector. A projection has a separate replay and never changes its original
// YAML or expectation. A VM entry must fail ordinary-container preflight
// before writing a fixture, and names its guest replay.
#[derive(Clone, Copy)]
enum FixtureReplay {
    Direct {
        test: &'static str,
        run: fn(&Ctx) -> TestResult,
    },
    Projected {
        reason: &'static str,
        test: &'static str,
        run: fn(&Ctx) -> TestResult,
    },
    Vm {
        reason: &'static str,
        yaml: &'static str,
        test: &'static str,
        run: fn(&Ctx) -> TestResult,
    },
    /// The vector's exact interleaving (process events timed to diskgc's own
    /// operations) cannot be staged on a real kernel. The named container
    /// test pins only the kernel-side invariant named in `reason`; the
    /// rest of the vector's decision (retry, scan reuse) is model-only and
    /// rests on the vector alone.
    KernelCounterpart {
        reason: &'static str,
        test: &'static str,
        tier: crate::suite::Tier,
        run: fn(&Ctx) -> TestResult,
    },
}

struct FixtureClassification {
    file: &'static str,
    replay: FixtureReplay,
}

macro_rules! direct {
    ($file:literal, $test:literal, $run:path) => {
        FixtureClassification {
            file: $file,
            replay: FixtureReplay::Direct {
                test: $test,
                run: $run,
            },
        }
    };
}

macro_rules! projected {
    ($file:literal, $reason:literal, $test:literal, $run:path) => {
        FixtureClassification {
            file: $file,
            replay: FixtureReplay::Projected {
                reason: $reason,
                test: $test,
                run: $run,
            },
        }
    };
}

macro_rules! vm_replay {
    ($file:literal, $reason:literal, $test:literal, $run:path) => {
        FixtureClassification {
            file: $file,
            replay: FixtureReplay::Vm {
                reason: $reason,
                yaml: include_str!(concat!(
                    "../../crates/diskgc-model/tests/models/profiles/",
                    $file
                )),
                test: $test,
                run: $run,
            },
        }
    };
}

macro_rules! kernel_counterpart {
    ($file:literal, $reason:literal, $test:literal, $tier:ident, $run:path) => {
        FixtureClassification {
            file: $file,
            replay: FixtureReplay::KernelCounterpart {
                reason: $reason,
                test: $test,
                tier: crate::suite::Tier::$tier,
                run: $run,
            },
        }
    };
}

const PROFILE_FIXTURES: &[FixtureClassification] = &[
    vm_replay!(
        "autotools-source-anchor.yaml",
        "historical ctime required by idle_for",
        "vm_autotools_original_matches_linux_outcomes_and_deletion_diff",
        vm_autotools_original_matches_linux_outcomes_and_deletion_diff
    ),
    projected!(
        "brave-browser-cache-lock.yaml",
        "source homes /home/clear and /home/locked are outside scratch",
        "brave_browser_vector_matches_linux_outcomes_and_deletion_diff",
        brave_browser_vector
    ),
    direct!(
        "bun-cache-match.yaml",
        "bun_vector_matches_linux_outcomes_and_deletion_diff",
        bun_vector_matches_linux_outcomes_and_deletion_diff
    ),
    direct!(
        "cargo-target-locked.yaml",
        "cargo_target_vector_matches_linux_outcomes_and_deletion_diff",
        cargo_target_vector
    ),
    projected!(
        "chrome-browser-cache-lock.yaml",
        "source homes /home/clear and /home/locked are outside scratch",
        "chrome_browser_vector_matches_linux_outcomes_and_deletion_diff",
        chrome_browser_vector
    ),
    projected!(
        "chromium-browser-cache-lock.yaml",
        "source homes /home/clear and /home/locked are outside scratch",
        "chromium_browser_vector_matches_linux_outcomes_and_deletion_diff",
        chromium_browser_vector
    ),
    direct!(
        "chromium-tmp-match.yaml",
        "chromium_tmp_vector_matches_linux_outcomes_and_deletion_diff",
        chromium_tmp_vector
    ),
    direct!(
        "claude-code-tmp-match.yaml",
        "claude_code_tmp_vector_matches_linux_outcomes_and_deletion_diff",
        claude_code_tmp_vector
    ),
    direct!(
        "claude-code-versions-versions.yaml",
        "claude_code_versions_vector_matches_linux_outcomes_and_deletion_diff",
        claude_code_versions_vector
    ),
    vm_replay!(
        "cmake-source-anchor.yaml",
        "historical ctime required by idle_for",
        "vm_cmake_original_matches_linux_outcomes_and_deletion_diff",
        vm_cmake_original_matches_linux_outcomes_and_deletion_diff
    ),
    direct!(
        "codex-releases-versions.yaml",
        "codex_releases_vector_matches_linux_outcomes_and_deletion_diff",
        codex_releases_vector
    ),
    vm_replay!(
        "containers-image-age.yaml",
        "historical ctime required by idle_for",
        "vm_containers_image_original_matches_linux_outcomes_and_deletion_diff",
        vm_containers_image_original_matches_linux_outcomes_and_deletion_diff
    ),
    direct!(
        "cursor-agent-versions-versions.yaml",
        "cursor_agent_versions_vector_matches_linux_outcomes_and_deletion_diff",
        cursor_agent_versions_vector
    ),
    direct!(
        "cursor-extensions-versions.yaml",
        "cursor_extensions_vector_matches_linux_outcomes_and_deletion_diff",
        cursor_extensions_vector
    ),
    projected!(
        "edge-browser-cache-lock.yaml",
        "source homes /home/clear and /home/locked are outside scratch",
        "edge_browser_vector_matches_linux_outcomes_and_deletion_diff",
        edge_browser_vector
    ),
    direct!(
        "go-mod-cache-match.yaml",
        "go_mod_cache_vector_matches_linux_outcomes_and_deletion_diff",
        go_mod_cache_vector
    ),
    vm_replay!(
        "meson-source-anchor.yaml",
        "historical ctime required by idle_for",
        "vm_meson_original_matches_linux_outcomes_and_deletion_diff",
        vm_meson_original_matches_linux_outcomes_and_deletion_diff
    ),
    direct!(
        "mypy-cache-match.yaml",
        "mypy_vector_matches_linux_outcomes_and_deletion_diff",
        mypy_vector_matches_linux_outcomes_and_deletion_diff
    ),
    direct!(
        "nix-user-cache-match.yaml",
        "nix_user_cache_vector_matches_linux_outcomes_and_deletion_diff",
        nix_user_cache_vector
    ),
    direct!(
        "node-compile-cache-match.yaml",
        "node_compile_cache_vector_matches_linux_outcomes_and_deletion_diff",
        node_compile_cache_vector
    ),
    direct!(
        "node-compile-cache-nested-claude.yaml",
        "node_compile_cache_nested_vector_matches_linux_outcomes_and_deletion_diff",
        node_compile_cache_nested_vector
    ),
    direct!(
        "playwright-tmp-match.yaml",
        "playwright_tmp_vector_matches_linux_outcomes_and_deletion_diff",
        playwright_tmp_vector
    ),
    direct!(
        "pytest-cache-match.yaml",
        "pytest_cache_vector_matches_linux_outcomes_and_deletion_diff",
        pytest_cache_vector
    ),
    projected!(
        "pytest-tmp-unsafe-live-no-fd.yaml",
        "the original executable and maps are /usr/bin/python outside scratch",
        "pytest_unsafe_unrelated_fd_matches_linux",
        pytest_unsafe_unrelated_fd_matches_linux
    ),
    direct!(
        "python-pycache-checkout.yaml",
        "python_vector_matches_linux_outcomes_and_deletion_diff",
        python_vector_matches_linux_outcomes_and_deletion_diff
    ),
    kernel_counterpart!(
        "python-pycache-fork-burst-settles.yaml",
        "forks timed to diskgc's listings and the retry are model-only; \
         the kernel case pins that churn that ends clears; an unsettled scan, \
         if one occurs, must block (not required to occur)",
        "host_inuse_scan_settles_after_fork_churn",
        Fast,
        crate::host_suite::host_inuse_scan_settles_after_fork_churn
    ),
    kernel_counterpart!(
        "python-pycache-fork-storm-unsettled.yaml",
        "forks timed to diskgc's listings and scan reuse are model-only; \
         the kernel case pins that under sustained churn no unsettled scan clears \
         and the candidate clears after it",
        "host_inuse_scan_under_sustained_fork_churn",
        Full,
        crate::host_suite::host_inuse_scan_under_sustained_fork_churn
    ),
    projected!(
        "python-pycache-in-use.yaml",
        "the original executable and maps are /usr/bin/python outside scratch",
        "python_in_use_vector_matches_linux",
        python_in_use_vector_matches_linux
    ),
    direct!(
        "renode-tmp-match.yaml",
        "renode_tmp_vector_matches_linux_outcomes_and_deletion_diff",
        renode_tmp_vector
    ),
    projected!(
        "west-build-anchored.yaml",
        "single historical age cohort rebased onto the sandbox clock offset",
        "west_vector_matches_linux_under_clock_offset",
        west_vector_matches_linux_under_clock_offset
    ),
    projected!(
        "xdg-cache-entry-v2-match.yaml",
        "single historical age cohort rebased onto the sandbox clock offset",
        "xdg_vector_matches_linux_under_clock_offset",
        xdg_vector_matches_linux_under_clock_offset
    ),
    direct!(
        "zephyr-tmp-match.yaml",
        "zephyr_tmp_vector_matches_linux_outcomes_and_deletion_diff",
        zephyr_tmp_vector
    ),
];

fn verify_registered_replays() -> Result<(), String> {
    for classification in PROFILE_FIXTURES {
        let (test_name, run, tier) = match classification.replay {
            FixtureReplay::Direct { test, run } => (test, run, crate::suite::Tier::Fast),
            FixtureReplay::Projected { reason, test, run } => {
                if reason.is_empty() {
                    return Err(format!(
                        "{}: projection needs a reason",
                        classification.file
                    ));
                }
                (test, run, crate::suite::Tier::Fast)
            }
            FixtureReplay::Vm {
                reason, test, run, ..
            } => {
                if !reason.contains("historical ctime") {
                    return Err(format!("{}: VM reason is incomplete", classification.file));
                }
                (test, run, crate::suite::Tier::Vm)
            }
            FixtureReplay::KernelCounterpart {
                reason,
                test,
                tier,
                run,
            } => {
                if !reason.contains("model-only") {
                    return Err(format!(
                        "{}: kernel counterpart needs its model-only reason",
                        classification.file
                    ));
                }
                (test, run, tier)
            }
        };
        let registered = crate::suite::TESTS
            .iter()
            .find(|entry| entry.name == test_name)
            .ok_or_else(|| format!("{}: missing replay {test_name}", classification.file))?;
        if registered.tier != tier || !std::ptr::fn_addr_eq(registered.run, run) {
            return Err(format!(
                "{}: replay {test_name} is not registered with its declared runner",
                classification.file
            ));
        }
    }
    Ok(())
}

/// Rejected original models and field families must fail before any scratch
/// fixture or CLI config write. A fresh tmpfs inode's ctime is always now.
pub fn historical_ctime_originals_refuse_before_fixture_writes(ctx: &Ctx) -> TestResult {
    verify_registered_replays()?;
    let bite_dir = "preflight-snapshot-bite/nested";
    ctx.home.mkdirs(bite_dir, 0o700, TESTER)?;
    let bite = ctx.home.write_file(
        "preflight-snapshot-bite/nested/payload",
        b"first",
        0o600,
        TESTER,
    )?;
    let prior = preflight_scratch_snapshot(ctx)?;
    fs::write(&bite, b"other")
        .map_err(|error| format!("overwrite nested snapshot bite {}: {error}", bite.display()))?;
    let changed = preflight_scratch_snapshot(ctx)?;
    let before_entry = prior
        .get(&bite)
        .ok_or("snapshot omitted nested bite file")?;
    let after_entry = changed.get(&bite).ok_or("snapshot lost nested bite file")?;
    ensure!(
        before_entry.ino == after_entry.ino && before_entry.content != after_entry.content,
        "nested same-inode overwrite did not change snapshotted content"
    );
    ensure!(
        prior != changed,
        "nested overwrite did not change scratch snapshot"
    );
    fs::write(&bite, b"first")
        .map_err(|error| format!("restore nested snapshot bite {}: {error}", bite.display()))?;
    let before = preflight_scratch_snapshot(ctx)?;
    for classification in PROFILE_FIXTURES {
        let FixtureReplay::Vm { yaml, .. } = classification.replay else {
            continue;
        };
        let name = classification.file;
        let vector = ProfileVector::parse(yaml)?;
        let error = replay_model(ctx, vector, false)
            .expect_err("historical ctime original unexpectedly replayed");
        ensure!(
            error.contains("idle_for") && error.contains("ctime") && error.contains("model-only"),
            "{name}: {error}"
        );
        assert_preflight_scratch_unchanged(ctx, &before, name)?;
    }
    for (name, model, expected, second_expected) in non_profile_models()? {
        let error = model_materialize::materialize(ctx, &model)
            .err()
            .ok_or_else(|| format!("{name}: model-only fixture unexpectedly materialized"))?;
        ensure!(error.contains(expected), "{name}: {error}");
        assert_preflight_scratch_unchanged(ctx, &before, name)?;
        if let Some(second_expected) = second_expected {
            let isolated_accounting = isolate_sketch_mount_accounting(&model);
            let error = model_materialize::materialize(ctx, &isolated_accounting)
                .err()
                .ok_or_else(|| {
                    format!("{name}: second model-only field unexpectedly materialized")
                })?;
            ensure!(error.contains(second_expected), "{name}: {error}");
            assert_preflight_scratch_unchanged(ctx, &before, name)?;
        }
    }
    for (name, model, expected) in model_only_field_cases()? {
        let error = model_materialize::materialize(ctx, &model)
            .err()
            .ok_or_else(|| format!("{name}: model-only field unexpectedly materialized"))?;
        ensure!(error.contains(expected), "{name}: {error}");
        assert_preflight_scratch_unchanged(ctx, &before, name)?;
    }
    for (name, model, expected) in nominal_field_cases()? {
        let error = model_materialize::materialize(ctx, &model)
            .err()
            .ok_or_else(|| format!("{name}: nominal model field unexpectedly materialized"))?;
        ensure!(error.contains(expected), "{name}: {error}");
        assert_preflight_scratch_unchanged(ctx, &before, name)?;
    }
    Ok(())
}

fn nominal_field_cases() -> Result<Vec<(&'static str, HostModel, &'static str)>, String> {
    let mut vector = ProfileVector::parse(PYTHON_VECTOR)?;
    adapt_home_mount(&mut vector);
    let mut literal = vector.host;
    literal.clock = WallTime(jiff::Timestamp::now());
    for mount in &mut literal.mounts {
        if mount.at == Path::new("/") {
            mount.fs = "overlay".into();
        }
        let stat = rustix::fs::statx(
            rustix::fs::CWD,
            &mount.at,
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
            rustix::fs::StatxFlags::BASIC_STATS | rustix::fs::StatxFlags::MNT_ID,
        )
        .map_err(|error| format!("statx {} for nominal bite: {error}", mount.at.display()))?;
        mount.id = stat.stx_mnt_id;
        mount.dev = DevSpec(Dev {
            major: stat.stx_dev_major,
            minor: stat.stx_dev_minor,
        });
        let vfs = rustix::fs::statvfs(&mount.at)
            .map_err(|error| format!("statvfs {} for nominal bite: {error}", mount.at.display()))?;
        mount.size = Bytes(vfs.f_blocks * vfs.f_frsize);
    }
    if literal.mounts.len() != 2 {
        return Err("nominal bite expects root and one home scratch mount".into());
    }
    literal.mounts[1].parent = Some(literal.mounts[0].id);
    let mut clock = literal.clone();
    clock.clock = "2000-01-01T00:00:00Z".parse()?;
    let mut id = literal.clone();
    id.mounts[0].id = id.mounts[0].id.saturating_add(10_000);
    let mut dev = literal.clone();
    dev.mounts[0].dev.0.minor = dev.mounts[0].dev.0.minor.saturating_add(10_000);
    let mut size = literal.clone();
    size.mounts[0].size.0 = size.mounts[0].size.0.saturating_add(1);
    let mut root_fs = literal.clone();
    root_fs.mounts[0].fs = "ext4".into();
    let mut scratch_fs = literal.clone();
    scratch_fs.mounts[1].fs = "ext4".into();
    let mut parent = literal;
    parent.mounts[1].parent = Some(parent.mounts[0].id.saturating_add(10_000));
    Ok(vec![
        ("nominal clock", clock, "clock is model-only"),
        ("nominal mount id", id, "mounts[0].id is model-only"),
        ("nominal mount device", dev, "mounts[0].dev is model-only"),
        ("nominal mount size", size, "mounts[0].size is model-only"),
        (
            "nominal root filesystem",
            root_fs,
            "mounts[0].fs is model-only",
        ),
        (
            "nominal scratch filesystem",
            scratch_fs,
            "mounts[1].fs is model-only",
        ),
        (
            "nominal mount parent",
            parent,
            "mounts[1].parent is model-only",
        ),
    ])
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreflightScratchEntry {
    dev: u64,
    ino: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u64,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    content: Option<Vec<u8>>,
    symlink: Option<PathBuf>,
}

type PreflightScratchSnapshot = BTreeMap<PathBuf, PreflightScratchEntry>;

fn preflight_scratch_snapshot(ctx: &Ctx) -> Result<PreflightScratchSnapshot, String> {
    fn visit(path: &Path, mount_id: u64, entries: &mut PreflightScratchSnapshot) -> TestResult {
        let actual_mount = crate::scratch::mount_id(path, false)?;
        ensure!(
            actual_mount == mount_id,
            "preflight scratch snapshot reached another mount at {}",
            path.display()
        );
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| format!("lstat preflight scratch {}: {error}", path.display()))?;
        let kind = metadata.file_type();
        let content = kind
            .is_file()
            .then(|| {
                fs::read(path)
                    .map_err(|error| format!("read preflight scratch {}: {error}", path.display()))
            })
            .transpose()?;
        let symlink = kind
            .is_symlink()
            .then(|| {
                fs::read_link(path)
                    .map_err(|error| format!("read preflight symlink {}: {error}", path.display()))
            })
            .transpose()?;
        let previous = entries.insert(
            path.to_owned(),
            PreflightScratchEntry {
                dev: metadata.dev(),
                ino: metadata.ino(),
                mode: metadata.mode(),
                uid: metadata.uid(),
                gid: metadata.gid(),
                nlink: metadata.nlink(),
                len: metadata.len(),
                mtime: (metadata.mtime(), metadata.mtime_nsec()),
                ctime: (metadata.ctime(), metadata.ctime_nsec()),
                content,
                symlink,
            },
        );
        ensure!(
            previous.is_none(),
            "duplicate preflight scratch path {}",
            path.display()
        );
        if kind.is_dir() {
            for child in fs::read_dir(path)
                .map_err(|error| format!("list preflight scratch {}: {error}", path.display()))?
            {
                let child =
                    child.map_err(|error| format!("read preflight scratch entry: {error}"))?;
                visit(&child.path(), mount_id, entries)?;
            }
        }
        Ok(())
    }

    let mut entries = BTreeMap::new();
    for scratch in [&ctx.scratch, &ctx.var_tmp, &ctx.home] {
        visit(scratch.root(), scratch.mnt_id(), &mut entries)?;
    }
    Ok(entries)
}

fn assert_preflight_scratch_unchanged(
    ctx: &Ctx,
    before: &PreflightScratchSnapshot,
    case: &str,
) -> TestResult {
    let after = preflight_scratch_snapshot(ctx)?;
    let changed = before
        .keys()
        .chain(after.keys())
        .find(|path| before.get(*path) != after.get(*path));
    ensure!(
        changed.is_none(),
        "{case}: preflight changed scratch entry {}",
        changed.map_or_else(|| "?".to_owned(), |path| path.display().to_string())
    );
    Ok(())
}

fn non_profile_models() -> Result<Vec<NonProfileModelCase>, String> {
    NON_PROFILE_MODELS
        .iter()
        .map(|(name, yaml, expected, second_expected)| {
            let model =
                diskgc_model::load::parse_yaml(yaml).map_err(|error| format!("{name}: {error}"))?;
            Ok((*name, model, *expected, *second_expected))
        })
        .collect()
}

fn isolate_sketch_mount_accounting(model: &HostModel) -> HostModel {
    // Leave the original YAML unchanged. Remove its event and process/lock
    // evidence from this independent probe to reach the exact-space check.
    let mut isolated = model.clone();
    isolated.events.clear();
    isolated.processes.clear();
    isolated.locks.clear();
    isolated
}

/// One representative per field family that `materialize` declares
/// model-only. The container bite invokes materialize, not just validate,
/// and compares the scratch roots before and after each refusal.
fn model_only_field_cases() -> Result<Vec<(&'static str, HostModel, &'static str)>, String> {
    type Mutation = fn(&mut HostModel);
    let cases: &[(&str, &str, Mutation)] = &[
        ("kernel", "kernel is model-only", |m| {
            m.kernel = Some("2.6.0".into())
        }),
        ("hostname", "hostname is model-only", |m| {
            m.hostname = Some("other".into())
        }),
        (
            "own-pid",
            "own_pid/own_uid_map/own_uid is model-only",
            |m| m.own_pid = Some(1),
        ),
        (
            "own-uid-map",
            "own_pid/own_uid_map/own_uid is model-only",
            |m| m.own_uid_map = Some("0 0 1".into()),
        ),
        (
            "own-uid",
            "own_pid/own_uid_map/own_uid is model-only",
            |m| m.own_uid = Some(0),
        ),
        ("sandbox-gate", "sandbox_gate is model-only", |m| {
            m.sandbox_gate = GateSpec::Closed
        }),
        ("command-exit", "command/docker is model-only", |m| {
            m.command_exit = 1
        }),
        ("command-error", "command/docker is model-only", |m| {
            m.command_error_after_start = true
        }),
        ("docker", "command/docker is model-only", |m| {
            m.docker.host = Some("unix:///tmp/docker.sock".into())
        }),
        ("events", "events is model-only", |m| {
            m.events.push(Default::default())
        }),
        ("lock-holder", "locks[0].holder is model-only", |m| {
            m.locks.push(LockSpec {
                path: "/home/tester/project/unused".into(),
                holder: 42,
                mode: LockModeSpec::Exclusive,
            })
        }),
        ("process-proc", "processes[0].proc is model-only", |m| {
            m.processes.push(ProcessSpec {
                proc: ProcAccess::Denied,
                ..minimal_process()
            })
        }),
        (
            "process-executable",
            "processes[0].exe/maps/environ is model-only",
            |m| {
                m.processes.push(ProcessSpec {
                    exe: Some("/usr/bin/python".into()),
                    ..minimal_process()
                })
            },
        ),
        (
            "process-credentials",
            "processes[0].root/name/ids/dumpability/uid_map/privileged is model-only",
            |m| {
                m.processes.push(ProcessSpec {
                    root: Some("/home/tester/project".into()),
                    ..minimal_process()
                })
            },
        ),
        ("process-fds", "processes[0].fds is model-only", |m| {
            m.processes.push(minimal_process())
        }),
        ("statx-mask", "statx_missing is model-only", |m| {
            m.mounts[0].statx_missing.push(StatxField::MntId)
        }),
        ("other-used", "other_used is model-only", |m| {
            m.mounts[0].other_used = Bytes(4096)
        }),
        ("read-only-mount", "read_only is model-only", |m| {
            m.mounts[0].read_only = true
        }),
        (
            "network-filesystem",
            "is model-only: only scratch tmpfs mounts",
            |m| m.mounts[1].fs = "nfs".into(),
        ),
        (
            "outside-scratch",
            "outside the container scratch mounts",
            |m| {
                m.tree.insert(
                    "/home/other/unsupported".into(),
                    NodeSpec::File(FileSpec::default()),
                );
            },
        ),
        ("hard-link", ".link is model-only", |m| {
            m.tree.insert(
                "/home/tester/hard-link".into(),
                NodeSpec::File(FileSpec {
                    link: Some("/home/tester/project/original".into()),
                    ..FileSpec::default()
                }),
            );
        }),
        ("bound-socket", ".bound is model-only", |m| {
            m.tree.insert(
                "/home/tester/bound-socket".into(),
                NodeSpec::File(FileSpec {
                    special: Some(Special::Socket),
                    bound: true,
                    ..FileSpec::default()
                }),
            );
        }),
        ("char-device", "char_device is model-only", |m| {
            m.tree.insert(
                "/home/tester/char-device".into(),
                NodeSpec::File(FileSpec {
                    special: Some(Special::CharDevice),
                    ..FileSpec::default()
                }),
            );
        }),
        ("symlink-mtime", ".mtime on a symlink is model-only", |m| {
            m.tree.insert(
                "/home/tester/symlink-mtime".into(),
                NodeSpec::File(FileSpec {
                    symlink: Some("target".into()),
                    mtime: Some(m.clock),
                    ..FileSpec::default()
                }),
            );
        }),
        ("symlink-mode", ".mode on a symlink is model-only", |m| {
            m.tree.insert(
                "/home/tester/symlink-mode".into(),
                NodeSpec::File(FileSpec {
                    symlink: Some("target".into()),
                    mode: Some(Mode(0o777)),
                    ..FileSpec::default()
                }),
            );
        }),
        ("socket-mode", ".mode on a socket is model-only", |m| {
            m.tree.insert(
                "/home/tester/socket-mode".into(),
                NodeSpec::File(FileSpec {
                    special: Some(Special::Socket),
                    mode: Some(Mode(0o600)),
                    ..FileSpec::default()
                }),
            );
        }),
    ];
    let mut base = ProfileVector::parse(PYTHON_VECTOR)?.host;
    base.mounts[1].at = "/home/tester".into();
    base.mounts[1].fs = "tmpfs".into();
    model_materialize::validate(&base)?;
    Ok(cases
        .iter()
        .map(|(name, expected, mutate)| {
            let mut model = base.clone();
            mutate(&mut model);
            (*name, model, *expected)
        })
        .collect())
}

fn minimal_process() -> ProcessSpec {
    serde_json::from_value(serde_json::json!({"pid": 42})).expect("minimal process shape")
}

/// Replay the unchanged CMake mixed-age vector in the disposable QEMU guest.
/// The guest Podman container holds CAP_SYS_TIME; the kernel timestamps each
/// cohort when it is actually created. The VM kernel command-line token is
/// checked before any clock change, in addition to the ordinary E2E interlock.
pub fn vm_cmake_original_matches_linux_outcomes_and_deletion_diff(ctx: &Ctx) -> TestResult {
    replay_historical_vector(ctx, CMAKE_VECTOR, None, materialize_historical_cmake)
}

pub fn vm_autotools_original_matches_linux_outcomes_and_deletion_diff(ctx: &Ctx) -> TestResult {
    replay_historical_vector(
        ctx,
        AUTOTOOLS_VECTOR,
        None,
        materialize_historical_autotools,
    )
}

pub fn vm_meson_original_matches_linux_outcomes_and_deletion_diff(ctx: &Ctx) -> TestResult {
    replay_historical_vector(ctx, MESON_VECTOR, None, materialize_historical_meson)
}

pub fn vm_containers_image_original_matches_linux_outcomes_and_deletion_diff(
    ctx: &Ctx,
) -> TestResult {
    replay_historical_vector(
        ctx,
        CONTAINERS_IMAGE_VECTOR,
        Some(CONTAINERS_IMAGE_PROFILE),
        materialize_historical_containers_image,
    )
}

/// A live producer can close its spool and remain ready to use it again.
/// With genuine five-day kernel ctimes, the withdrawn source's matching set
/// and 3-day floor (now a user profile, TASK-112 D2) see the open-fd case
/// but cannot distinguish the dormant spool from abandoned data. This is
/// the counterexample that withdrew `containers-image-tmp`. Keep it as a
/// dry-run counterexample: reclaiming the dormant fixture would
/// intentionally destroy live work.
pub fn vm_containers_image_dormant_spool_exposes_liveness_gap(ctx: &Ctx) -> TestResult {
    let mut clock = GuestClock::begin()?;
    let result = (|| {
        ensure!(
            Library::builtin()
                .map_err(|error| error.to_string())?
                .get("containers-image-tmp")
                .is_none(),
            "withdrawn containers-image-tmp is shipped again"
        );
        let user = parse_profiles(CONTAINERS_IMAGE_PROFILE)?;
        let library = Library::with_user_profiles(&user).map_err(|error| error.to_string())?;
        let selection = format!("profile:{CONTAINERS_IMAGE_PROBE}");
        let tier = Tier::select(&selection, &library, None, TESTER.uid)?;
        ensure!(
            tier.idle_for
                .is_some_and(|floor| floor.0 == Duration::from_secs(3 * 86_400)),
            "the containers-image probe no longer has its exact three-day source floor"
        );
        let config = format!(
            "version: 1\nprofiles:\n{}",
            CONTAINERS_IMAGE_PROFILE
                .lines()
                .map(|line| format!("  {line}\n"))
                .collect::<String>()
        );
        let config_path = ctx.home.write_file(
            "containers-image-probe.yaml",
            config.as_bytes(),
            0o644,
            TESTER,
        )?;
        let config_path = config_path.to_str().ok_or("non-UTF-8 config path")?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("read guest clock: {error}"))?;
        let old = now
            .checked_sub(Duration::from_secs(5 * 86_400))
            .ok_or("guest clock is too early for a five-day fixture")?;
        set_guest_clock(i128::try_from(old.as_nanos()).map_err(|error| error.to_string())?)?;
        let spool = &ctx.var_tmp;
        let free = spool.write_file("container_images_storage-free", b"unused", 0o600, TESTER)?;
        let held = spool.write_file("container_images_storage-held", b"open", 0o600, TESTER)?;
        spool.mkdir("buildah-dormant", 0o700, TESTER)?;
        let dormant = spool.path("buildah-dormant");
        let dormant_image =
            spool.write_file("buildah-dormant/image", b"needed on resume", 0o600, TESTER)?;
        ensure!(
            dormant_image == Path::new(crate::inuse_probe::DORMANT_IMAGE),
            "dormant producer fixture path and probe disagree"
        );
        // The producer has used this old spool and closed it. On EOF it
        // resumes and requires the same bytes, so this is live data.
        let dormant_process = Holder::tester(ctx, &["hold-reuse"], &[])?;

        set_guest_clock(i128::try_from(now.as_nanos()).map_err(|error| error.to_string())?)?;
        let fresh = spool.write_file("buildah-fresh", b"new", 0o600, TESTER)?;
        let mount_id = spool.mnt_id();
        let old_seconds = i64::try_from(old.as_secs()).map_err(|error| error.to_string())?;
        let now_seconds = i64::try_from(now.as_secs()).map_err(|error| error.to_string())?;
        for path in [&free, &held, &dormant] {
            let ctime = statx_ctime_seconds(path, mount_id)?;
            ensure!(
                (old_seconds..=old_seconds + 120).contains(&ctime),
                "{} lacks the old kernel ctime: {ctime}",
                path.display()
            );
        }
        let fresh_ctime = statx_ctime_seconds(&fresh, mount_id)?;
        ensure!(
            (now_seconds..=now_seconds + 120).contains(&fresh_ctime),
            "fresh spool has unexpected kernel ctime: {fresh_ctime}"
        );

        let held_text = held.to_str().ok_or("non-UTF-8 held spool")?;
        let held_process = Holder::tester(ctx, &["hold", held_text, "/"], &[])?;
        model_materialize::assert_process_no_reference_to(dormant_process.pid(), &dormant)?;
        let cmdline = fs::read(format!("/proc/{}/cmdline", dormant_process.pid()))
            .map_err(|error| format!("read dormant producer cmdline: {error}"))?;
        ensure!(
            cmdline
                .split(|byte| *byte == 0)
                .any(|arg| arg == b"hold-reuse"),
            "dormant producer command line did not identify the fixture"
        );
        let dormant_text = dormant.to_str().ok_or("non-UTF-8 dormant spool")?;
        ensure!(
            !cmdline
                .windows(dormant_text.len())
                .any(|window| window == dormant_text.as_bytes()),
            "dormant spool leaked through producer command line"
        );

        let oracle = Oracle::arm(Fixture::new())?;
        let dry = ensure_run(
            ctx,
            &[
                "--dry-run",
                "--json",
                "--no-index",
                "--min-free",
                "100%",
                "--select",
                &selection,
                "--config",
                config_path,
                "/var/tmp",
            ],
        )?;
        oracle.judge(&dry)?.assert_clean()?;
        dry.expect_code(0)?;
        let report = report(&dry)?;
        let plan = report["plan"]
            .as_array()
            .ok_or("dry run has no plan array")?;
        for (path, reason) in [
            (&free, None),
            (&held, Some("in_use")),
            (&dormant, None),
            (&fresh, Some("not_idle")),
        ] {
            let entry = plan
                .iter()
                .find(|entry| entry["candidate"].as_str() == path.to_str())
                .ok_or_else(|| {
                    format!("{} missing from the probe's plan: {report}", path.display())
                })?;
            ensure!(
                entry["reason"].as_str() == reason,
                "{}: expected {reason:?}, got {entry}",
                path.display()
            );
        }
        ensure!(
            dormant.join("image").exists(),
            "dry run removed dormant data"
        );
        held_process.finish()?;
        dormant_process.finish()
    })();
    let restored = clock.restore();
    result?;
    restored
}

fn replay_historical_vector(
    ctx: &Ctx,
    source: &str,
    profile_yaml: Option<&str>,
    materialize: fn(&Ctx, &ProfileVector) -> Result<model_materialize::Materialized, String>,
) -> TestResult {
    let vector = ProfileVector::parse(source)?;
    vector.check(&Library::builtin().map_err(|error| error.to_string())?)?;
    let mut clock = GuestClock::begin()?;
    let result =
        replay_model_with_profile(ctx, vector, false, profile_yaml, None, true, materialize);
    let restored = clock.restore();
    result?;
    restored
}

const VM_CLOCK_TOKEN: &str = "diskgc.e2e.historical_ctime=1";
const CMAKE_OLD: &str = "/home/tester/project/app/output";
const CMAKE_CHECKOUT_OLD: &str = "/home/tester/project/checkout/output";
const CMAKE_FRESH: &str = "/home/tester/project/fresh/output";

struct GuestClock {
    started: SystemTime,
    elapsed: Instant,
    restored: bool,
}

impl GuestClock {
    fn begin() -> Result<Self, String> {
        let cmdline = fs::read_to_string("/proc/cmdline")
            .map_err(|error| format!("read guest kernel command line: {error}"))?;
        ensure!(
            cmdline.split_whitespace().any(|arg| arg == VM_CLOCK_TOKEN),
            "historical ctime replay requires the disposable QEMU guest kernel token {VM_CLOCK_TOKEN}"
        );
        Ok(Self {
            started: SystemTime::now(),
            elapsed: Instant::now(),
            restored: false,
        })
    }

    fn restore(&mut self) -> TestResult {
        if self.restored {
            return Ok(());
        }
        let now = self.started + self.elapsed.elapsed();
        let nanos = now
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("restore guest clock: {error}"))?
            .as_nanos();
        set_guest_clock(i128::try_from(nanos).map_err(|error| error.to_string())?)?;
        self.restored = true;
        Ok(())
    }
}

impl Drop for GuestClock {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!("restore disposable guest clock after failed replay: {error}");
        }
    }
}

fn set_guest_clock(nanos: i128) -> TestResult {
    const NANOS_PER_SECOND: i128 = 1_000_000_000;
    let value = libc::timespec {
        tv_sec: nanos
            .div_euclid(NANOS_PER_SECOND)
            .try_into()
            .map_err(|error| format!("guest clock seconds overflow: {error}"))?,
        tv_nsec: nanos
            .rem_euclid(NANOS_PER_SECOND)
            .try_into()
            .map_err(|error| format!("guest clock nanoseconds overflow: {error}"))?,
    };
    // SAFETY: CLOCK_REALTIME and a fully initialized timespec are valid.
    let result = unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &value) };
    ensure!(
        result == 0,
        "set disposable guest CLOCK_REALTIME to {}.{}: {}",
        value.tv_sec,
        value.tv_nsec,
        io::Error::last_os_error()
    );
    Ok(())
}

/// Create the old candidates while the guest kernel clock is old, then the
/// fresh and structural nodes at the vector's original model time. Both
/// stages use the ordinary scratch materializer and its mount checks.
fn materialize_historical_cmake(
    ctx: &Ctx,
    vector: &ProfileVector,
) -> Result<model_materialize::Materialized, String> {
    materialize_historical_source(
        ctx,
        vector,
        &[CMAKE_OLD, CMAKE_CHECKOUT_OLD],
        CMAKE_FRESH,
        "cmake-build-anchored",
    )
}

fn materialize_historical_autotools(
    ctx: &Ctx,
    vector: &ProfileVector,
) -> Result<model_materialize::Materialized, String> {
    materialize_historical_source(
        ctx,
        vector,
        &[
            "/home/tester/project/app/output",
            "/home/tester/project/missing/output",
            "/home/tester/project/copied/output",
            "/home/tester/project/checkout/output",
        ],
        "/home/tester/project/fresh/output",
        "autotools-build-anchored",
    )
}

fn materialize_historical_meson(
    ctx: &Ctx,
    vector: &ProfileVector,
) -> Result<model_materialize::Materialized, String> {
    materialize_historical_source(
        ctx,
        vector,
        &[
            "/home/tester/project/app/odd-output",
            "/home/tester/project/checkout/output",
        ],
        "/home/tester/project/fresh/output",
        "meson-build-anchored",
    )
}

fn materialize_historical_containers_image(
    ctx: &Ctx,
    vector: &ProfileVector,
) -> Result<model_materialize::Materialized, String> {
    let model = &vector.host;
    let old_paths = [
        PathBuf::from("/var/tmp/container_images_storage-old"),
        PathBuf::from("/var/tmp/container_images_storage-held"),
    ];
    let old = match model.tree.get(&old_paths[0]) {
        Some(NodeSpec::File(file)) => file.mtime.ok_or("old container spool has no mtime")?,
        _ => return Err("old container spool is not a file".into()),
    };
    for path in &old_paths {
        let Some(NodeSpec::File(file)) = model.tree.get(path) else {
            return Err(format!("old container spool {} is missing", path.display()));
        };
        ensure!(
            file.mtime == Some(old),
            "old container spool {} has a different model timestamp",
            path.display()
        );
    }
    let mut old_model = model.clone();
    old_model.tree.retain(|path, _| old_paths.contains(path));
    old_model.processes.clear();
    old_model.locks.clear();
    let mut fresh_model = model.clone();
    fresh_model.tree.retain(|path, _| !old_paths.contains(path));
    let mut stages =
        model_materialize::OutcomeStagePlan::checked(vector, vec![&old_model, &fresh_model])?;
    set_guest_clock(old.0.as_nanosecond())?;
    model_materialize::materialize_outcome_projection(ctx, stages.next_stage()?)?.finish()?;
    set_guest_clock(model.clock.0.as_nanosecond())?;
    let materialized =
        model_materialize::materialize_outcome_projection(ctx, stages.next_stage()?)?;
    let library =
        Library::with_user_profiles(&vector.user_profiles).map_err(|error| error.to_string())?;
    assert_kernel_ctimes(
        &library,
        model,
        old,
        CONTAINERS_IMAGE_PROBE,
        "/var/tmp/buildah-fresh",
        &old_paths.into_iter().collect(),
        ctx.var_tmp.mnt_id(),
    )?;
    Ok(materialized)
}

fn materialize_historical_source(
    ctx: &Ctx,
    vector: &ProfileVector,
    old_paths: &[&str],
    fresh_path: &str,
    profile: &str,
) -> Result<model_materialize::Materialized, String> {
    let model = &vector.host;
    model_materialize::validate(model)?;
    let old = match model.tree.get(Path::new(old_paths[0])) {
        Some(NodeSpec::Dir(dir)) => dir.attrs.mtime.ok_or("old source candidate has no mtime")?,
        _ => return Err(format!("old {profile} candidate is not a directory")),
    };
    let old_paths: BTreeSet<_> = old_paths.iter().map(|path| PathBuf::from(*path)).collect();
    for path in &old_paths {
        let Some(NodeSpec::Dir(dir)) = model.tree.get(path) else {
            return Err(format!(
                "old {profile} cohort {} is missing",
                path.display()
            ));
        };
        ensure!(
            dir.attrs.mtime == Some(old),
            "old {profile} cohort {} has a different model timestamp",
            path.display()
        );
    }
    let mut old_model = model.clone();
    old_model.tree.retain(|path, _| old_paths.contains(path));
    old_model.processes.clear();
    old_model.locks.clear();
    let mut fresh_model = model.clone();
    fresh_model.tree.retain(|path, _| !old_paths.contains(path));
    let mut stages =
        model_materialize::OutcomeStagePlan::checked(vector, vec![&old_model, &fresh_model])?;
    set_guest_clock(old.0.as_nanosecond())?;
    model_materialize::materialize_outcome_projection(ctx, stages.next_stage()?)?.finish()?;
    set_guest_clock(model.clock.0.as_nanosecond())?;
    let materialized =
        model_materialize::materialize_outcome_projection(ctx, stages.next_stage()?)?;
    assert_kernel_ctimes(
        &Library::builtin().map_err(|error| error.to_string())?,
        model,
        old,
        profile,
        fresh_path,
        &old_paths,
        ctx.home.mnt_id(),
    )?;
    Ok(materialized)
}

fn statx_ctime_seconds(path: &Path, mount_id: u64) -> Result<i64, String> {
    use rustix::fs::{AtFlags, CWD, StatxFlags};
    let stx = rustix::fs::statx(
        CWD,
        path,
        AtFlags::SYMLINK_NOFOLLOW,
        StatxFlags::CTIME | StatxFlags::MNT_ID,
    )
    .map_err(|error| format!("statx {}: {error}", path.display()))?;
    ensure!(
        stx.stx_mask & StatxFlags::CTIME.bits() != 0,
        "statx {} did not provide ctime",
        path.display()
    );
    ensure!(
        stx.stx_mnt_id == mount_id,
        "{} is outside the container scratch tmpfs",
        path.display()
    );
    Ok(stx.stx_ctime.tv_sec)
}

fn assert_kernel_ctimes(
    library: &Library,
    model: &diskgc_model::spec::HostModel,
    old: WallTime,
    profile: &str,
    fresh_path: &str,
    old_paths: &BTreeSet<PathBuf>,
    mount_id: u64,
) -> TestResult {
    const NANOS_PER_SECOND: i128 = 1_000_000_000;
    let old_seconds = i64::try_from(old.0.as_nanosecond().div_euclid(NANOS_PER_SECOND))
        .map_err(|error| error.to_string())?;
    let now_seconds = i64::try_from(model.clock.0.as_nanosecond().div_euclid(NANOS_PER_SECOND))
        .map_err(|error| error.to_string())?;
    for path in old_paths {
        let ctime = statx_ctime_seconds(path, mount_id)?;
        ensure!(
            (old_seconds..=old_seconds + 120).contains(&ctime),
            "old {profile} kernel ctime {ctime} at {} differs from staged {old_seconds}",
            path.display()
        );
    }
    let old_ctime = statx_ctime_seconds(old_paths.first().ok_or("empty old cohort")?, mount_id)?;
    let fresh_ctime = statx_ctime_seconds(Path::new(fresh_path), mount_id)?;
    ensure!(
        (now_seconds..=now_seconds + 120).contains(&fresh_ctime),
        "fresh {profile} kernel ctime {fresh_ctime} differs from staged {now_seconds}"
    );
    let roots =
        (profile != CONTAINERS_IMAGE_PROBE).then(|| vec!["/home/tester/project".to_owned()]);
    let tier = Tier::select(
        &format!("profile:{profile}"),
        library,
        roots.as_deref(),
        TESTER.uid,
    )?;
    let floor = tier
        .idle_for
        .ok_or("CMake profile has no idle floor")?
        .0
        .as_secs();
    ensure!(
        u64::try_from(now_seconds - old_ctime).unwrap_or(0) >= floor,
        "old {profile} kernel ctime does not meet the {floor}s idle floor"
    );
    ensure!(
        u64::try_from(now_seconds - fresh_ctime).unwrap_or(0) < floor,
        "fresh {profile} kernel ctime unexpectedly meets the {floor}s idle floor"
    );
    eprintln!(
        "guest {profile} statx ctimes: old={old_ctime}, fresh={fresh_ctime}, model_now={now_seconds}, idle_floor={floor}s"
    );
    Ok(())
}

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

pub fn west_vector_matches_linux_under_clock_offset(ctx: &Ctx) -> TestResult {
    replay_single_cohort_vector(ctx, WEST_VECTOR)
}

pub fn xdg_vector_matches_linux_under_clock_offset(ctx: &Ctx) -> TestResult {
    replay_single_cohort_vector(ctx, XDG_VECTOR)
}

macro_rules! browser_replay {
    ($name:ident, $yaml:ident) => {
        pub fn $name(ctx: &Ctx) -> TestResult {
            let original = ProfileVector::parse($yaml)?;
            let library = Library::builtin().map_err(|error| error.to_string())?;
            original.check(&library)?;
            replay_model(ctx, project_browser_vector(original)?, false)
        }
    };
}

browser_replay!(brave_browser_vector, BRAVE_BROWSER_VECTOR);
browser_replay!(chrome_browser_vector, CHROME_BROWSER_VECTOR);
browser_replay!(chromium_browser_vector, CHROMIUM_BROWSER_VECTOR);
browser_replay!(edge_browser_vector, EDGE_BROWSER_VECTOR);

/// A live process whose executable, mapping, descriptor and initial
/// environment stay outside pytest's active tmp_path cannot protect it.
pub fn pytest_unsafe_unrelated_fd_matches_linux(ctx: &Ctx) -> TestResult {
    let original = ProfileVector::parse(PYTEST_UNSAFE_VECTOR)?;
    original.check(&Library::builtin().map_err(|error| error.to_string())?)?;
    let probe_size = fs::metadata(&ctx.self_exe)
        .map_err(|error| format!("stat fixture probe {}: {error}", ctx.self_exe.display()))?
        .len();
    let vector = project_pytest_unrelated_process(original, probe_size)?;
    replay_model_with_profile(
        ctx,
        vector,
        false,
        Some(PYTEST_UNSAFE_PROFILE),
        Some(Path::new(PYTEST_ACTIVE)),
        false,
        projected_materialize,
    )
}

/// Deliberately place each supported process reference inside a candidate
/// and prove the negative-evidence check rejects it on the real kernel.
/// Custom process roots are rejected by materializer preflight.
pub fn process_reference_negative_checks_bite(ctx: &Ctx) -> TestResult {
    let candidate = ctx
        .scratch
        .mkdir("process-reference-active", 0o700, TESTER)?;
    let held = ctx
        .scratch
        .write_file("process-reference-active/held", b"mapped", 0o600, TESTER)?;
    let unrelated =
        ctx.scratch
            .write_file("process-reference-unrelated", b"unrelated", 0o600, TESTER)?;
    let probe_bytes = fs::read(&ctx.self_exe)
        .map_err(|error| format!("read executable for negative probe: {error}"))?;
    let candidate_exe = ctx.scratch.write_file(
        "process-reference-active/probe",
        &probe_bytes,
        0o755,
        TESTER,
    )?;
    let held_text = held.to_str().ok_or("non-UTF-8 process reference file")?;
    let unrelated_text = unrelated.to_str().ok_or("non-UTF-8 unrelated file")?;
    let candidate_text = candidate.to_str().ok_or("non-UTF-8 candidate")?;
    for (args, env, expected) in [
        (vec!["hold", held_text, "/"], vec![], " fd "),
        (
            vec!["hold", unrelated_text, candidate_text],
            vec![],
            " cwd ",
        ),
        (vec!["hold-map", held_text], vec![], "maps entry"),
        (
            vec!["hold", unrelated_text, "/"],
            vec![("TMPDIR", candidate_text)],
            "initial environment",
        ),
    ] {
        let holder = Holder::tester(ctx, &args, &env)?;
        let error = model_materialize::assert_process_no_reference_to(holder.pid(), &candidate)
            .expect_err("candidate reference must fail the kernel evidence check");
        ensure!(
            error.contains(expected),
            "expected {expected:?} evidence, got {error}"
        );
        holder.finish()?;
    }
    let holder = Holder::tester_program(ctx, &candidate_exe, &["hold", unrelated_text, "/"])?;
    let error = model_materialize::assert_process_no_reference_to(holder.pid(), &candidate)
        .expect_err("candidate executable must fail the kernel evidence check");
    ensure!(
        error.contains(" exe "),
        "expected exe evidence, got {error}"
    );
    holder.finish()?;
    Ok(())
}

fn project_pytest_unrelated_process(
    mut vector: ProfileVector,
    probe_size: u64,
) -> Result<ProfileVector, String> {
    let [process] = vector.host.processes.as_mut_slice() else {
        return Err("pytest unsafe vector must have exactly one process".into());
    };
    if vector.profile.as_deref() != Some("pytest-tmp-unsafe-probe")
        || vector.path != Path::new("/tmp")
        || process.exe.as_deref() != Some(Path::new("/usr/bin/python"))
        || process.maps != [PathBuf::from("/usr/bin/python")]
        || process.cwd.as_deref() != Some(Path::new("/"))
        || process.environ != ["PATH=/usr/bin"]
        || process.fds.len() != 1
        || process.fds[0].path != Path::new("/usr/bin/python")
        || vector.host.tree.remove(Path::new("/usr/bin/python"))
            != Some(NodeSpec::File(FileSpec::default()))
        || vector.expect.preserved != [PathBuf::from("/usr/bin/python")]
    {
        return Err("pytest process projection requires the checked-in Python shape".into());
    }
    let probe = PathBuf::from(model_materialize::SCRATCH_PROBE_EXE);
    process.exe = Some(probe.clone());
    process.maps = vec![probe.clone()];
    process.cwd = None;
    process.environ = vec!["PATH=/bin".into()];
    process.fds[0].path = PYTEST_UNRELATED.into();
    vector.host.tree.insert(
        probe.clone(),
        NodeSpec::File(FileSpec {
            size: Some(Bytes(probe_size)),
            mode: Some(Mode(0o755)),
            ..FileSpec::default()
        }),
    );
    vector.host.tree.insert(
        PYTEST_UNRELATED.into(),
        NodeSpec::File(FileSpec {
            content: Some("unrelated live descriptor".into()),
            ..FileSpec::default()
        }),
    );
    // The checked-in model has an unreachable 100% free-space goal. Keep
    // one allocated file outside the candidate so the fresh kernel tmpfs
    // has the same pressure after reclaim.
    vector.host.tree.insert(
        PYTEST_RETAINED.into(),
        NodeSpec::File(FileSpec {
            size: Some(Bytes(8192)),
            ..FileSpec::default()
        }),
    );
    vector.expect.preserved = vec![probe, PYTEST_UNRELATED.into(), PYTEST_RETAINED.into()];
    Ok(vector)
}

/// Project only the two source homes in a checked-in Chromium-family
/// fixture onto the container's scratch home. The profile, relative lock
/// rule, and independent expected reasons remain the checked-in data.
fn project_browser_vector(mut vector: ProfileVector) -> Result<ProfileVector, String> {
    const ORIGINAL: &str = "/home";
    const SCRATCH: &str = "/home/tester";
    if !matches!(
        vector.profile.as_deref(),
        Some(
            "brave-browser-cache"
                | "chrome-browser-cache"
                | "chromium-browser-cache"
                | "edge-browser-cache"
        )
    ) || vector.path != Path::new(ORIGINAL)
        || vector.roots != ["/home/clear", "/home/locked"]
        || !vector.host.processes.is_empty()
        || !vector.host.locks.is_empty()
        || !vector.host.events.is_empty()
        || !vector.user_profiles.is_empty()
    {
        return Err("browser projection requires the checked-in two-home fixture shape".into());
    }
    if vector.host.mounts.len() != 2
        || !vector
            .host
            .mounts
            .iter()
            .any(|mount| mount.at == Path::new("/"))
        || !vector
            .host
            .mounts
            .iter()
            .any(|mount| mount.at == Path::new(ORIGINAL))
    {
        return Err("browser projection requires only the / and /home model mounts".into());
    }
    fn check_embedded_paths(path: &Path, node: &NodeSpec) -> Result<(), String> {
        match node {
            NodeSpec::Dir(dir) => {
                for (name, child) in &dir.entries {
                    check_embedded_paths(&path.join(name), child)?;
                }
            }
            NodeSpec::File(file)
                if file
                    .symlink
                    .as_ref()
                    .is_some_and(|target| target.is_absolute())
                    || file.link.is_some() =>
            {
                return Err(format!(
                    "browser projection cannot preserve an embedded absolute symlink or hard link at {}",
                    path.display()
                ));
            }
            NodeSpec::File(_) => {}
        }
        Ok(())
    }
    for (path, node) in &vector.host.tree {
        check_embedded_paths(path, node)?;
    }
    fn source_path(path: &Path) -> Result<PathBuf, String> {
        let in_source_home = ["/home/clear", "/home/locked"]
            .iter()
            .any(|root| path.starts_with(root));
        if !in_source_home {
            return Err(format!(
                "browser projection expected a path under /home/clear or /home/locked: {}",
                path.display()
            ));
        }
        Ok(Path::new(SCRATCH).join(
            path.strip_prefix(ORIGINAL)
                .expect("validated source home is under /home"),
        ))
    }
    vector.path = SCRATCH.into();
    vector.roots = vector
        .roots
        .iter()
        .map(|root| {
            source_path(Path::new(root)).and_then(|path| {
                path.into_os_string()
                    .into_string()
                    .map_err(|_| "projected browser root is not UTF-8".to_owned())
            })
        })
        .collect::<Result<_, _>>()?;
    vector.host.tree = std::mem::take(&mut vector.host.tree)
        .into_iter()
        .map(|(path, node)| {
            if path == Path::new("/run/diskgc/lock") {
                Ok((path, node))
            } else {
                Ok((source_path(&path)?, node))
            }
        })
        .collect::<Result<_, String>>()?;
    for plan in &mut vector.expect.plan {
        if let Some(candidate) = &mut plan.candidate {
            *candidate = source_path(candidate)?;
        }
        if let Some(source) = &mut plan.source {
            *source = source_path(source)?;
        }
    }
    for path in &mut vector.expect.actions {
        *path = source_path(path)?;
    }
    for path in &mut vector.expect.removed {
        *path = source_path(path)?;
    }
    for skip in &mut vector.expect.skipped {
        if let Some(candidate) = &mut skip.candidate {
            *candidate = source_path(candidate)?;
        }
    }
    for path in &mut vector.expect.preserved {
        *path = source_path(path)?;
    }
    Ok(vector)
}

/// The checked-in in-use vector names a nominal Python executable. The
/// container runs the copied probe instead, holding the same open file, so
/// the projection models that probe as the process (its executable,
/// mapping and environment): a readable process whose fd is the evidence.
/// The unchanged expected in-use verdict still comes from the vector.
pub fn python_in_use_vector_matches_linux(ctx: &Ctx) -> TestResult {
    replay_model(ctx, python_in_use_probe_vector(probe_size(ctx)?)?, false)
}

/// A live exclusive Cargo lock must leave the target and its payload intact.
pub fn cargo_target_held_lock_matches_linux(ctx: &Ctx) -> TestResult {
    replay_model(ctx, cargo_target_held_lock_vector(probe_size(ctx)?)?, false)
}

pub fn cargo_target_shared_lock_matches_linux(ctx: &Ctx) -> TestResult {
    let mut vector = cargo_target_held_lock_vector(probe_size(ctx)?)?;
    vector.host.locks[0].mode = LockModeSpec::Shared;
    replay_model(ctx, vector, false)
}

/// The size of the copied probe (this runner's own binary), which a model
/// of it must state ([`model_materialize::SCRATCH_PROBE_EXE`]).
fn probe_size(ctx: &Ctx) -> Result<u64, String> {
    Ok(fs::metadata(&ctx.self_exe)
        .map_err(|error| format!("stat fixture probe {}: {error}", ctx.self_exe.display()))?
        .len())
}

/// `process` becomes the copied probe (TASK-190): readable, with the probe
/// as its executable and mapping and `PATH=/bin` as its environment, the
/// shape the materializer runs. A process with none of them reads as
/// unreadable in the model, which a scan reports as `in_use_unknown`: the
/// model's verdict would then not be the fd's, which Linux's is.
fn as_copied_probe(vector: &mut ProfileVector, pid: u32, probe_size: u64) -> Result<(), String> {
    let probe = PathBuf::from(model_materialize::SCRATCH_PROBE_EXE);
    let process = vector
        .host
        .processes
        .iter_mut()
        .find(|process| process.pid == pid)
        .ok_or_else(|| format!("process {pid} missing"))?;
    process.exe = Some(probe.clone());
    process.maps = vec![probe.clone()];
    process.environ = vec!["PATH=/bin".into()];
    process.cwd = None;
    vector.host.tree.insert(
        probe,
        NodeSpec::File(FileSpec {
            size: Some(Bytes(probe_size)),
            mode: Some(Mode(0o755)),
            ..FileSpec::default()
        }),
    );
    Ok(())
}

/// The materialized helper must release its kernel lock when its guard ends.
pub fn materialized_lock_releases_on_finish(ctx: &Ctx) -> TestResult {
    let mut vector = cargo_target_held_lock_vector(probe_size(ctx)?)?;
    adapt_home_mount(&mut vector);
    let materialized = projected_materialize(ctx, &vector)?;
    materialized.assert_holding()?;
    materialized.finish()?;
    let path = Path::new(CARGO_LOCK_PATH);
    ensure!(
        flock_free_within_release_bound(path)?,
        "lock {} remained held {RELEASE_BOUND:?} after helper exit",
        path.display()
    );
    Ok(())
}

/// A missing modelled lock file must not be created by the fixture helper.
pub fn missing_lock_probe_does_not_create_a_file(ctx: &Ctx) -> TestResult {
    let path = ctx.scratch.path("missing-model-lock");
    ensure!(
        fs::symlink_metadata(&path).is_err_and(|error| error.kind() == io::ErrorKind::NotFound),
        "missing-lock probe path already exists: {}",
        path.display()
    );
    let args = [
        "flock-hold-existing",
        path.to_str().ok_or("non-UTF-8 lock path")?,
    ];
    let result = Holder::tester(ctx, &args, &[]);
    ensure!(
        result.is_err(),
        "no-create lock helper accepted a missing file"
    );
    ensure!(
        fs::symlink_metadata(&path).is_err_and(|error| error.kind() == io::ErrorKind::NotFound),
        "no-create lock helper created {}",
        path.display()
    );
    Ok(())
}

/// A shared lock allows another shared holder; an exclusive lock does not.
/// Check that a wrong declared mode fails the verifier in both directions.
pub fn lock_mode_verifier_rejects_mismatch(ctx: &Ctx) -> TestResult {
    let path = ctx
        .scratch
        .write_file("lock-mode-probe", b"", 0o600, TESTER)?;
    let path_text = path.to_str().ok_or("non-UTF-8 lock path")?;
    for (args, actual, incorrect) in [
        (
            vec!["flock-hold-existing", "--shared", path_text],
            LockModeSpec::Shared,
            LockModeSpec::Exclusive,
        ),
        (
            vec!["flock-hold-existing", path_text],
            LockModeSpec::Exclusive,
            LockModeSpec::Shared,
        ),
    ] {
        let holder = Holder::tester(ctx, &args, &[])?;
        model_materialize::verify_lock_mode(&path, actual)?;
        let error = model_materialize::verify_lock_mode(&path, incorrect)
            .expect_err("wrong declared lock mode must fail verification");
        ensure!(
            error.contains("disagrees"),
            "unexpected mode error: {error}"
        );
        holder.finish()?;
    }
    Ok(())
}

fn cargo_target_held_lock_vector(probe_size: u64) -> Result<ProfileVector, String> {
    let mut vector = ProfileVector::parse(CARGO_TARGET_VECTOR)?;
    let path = PathBuf::from(CARGO_LOCK_PATH);
    let process: ProcessSpec = serde_json::from_value(serde_json::json!({"pid": 42}))
        .map_err(|error| format!("lock holder model: {error}"))?;
    vector.host.processes.push(process);
    as_copied_probe(&mut vector, 42, probe_size)?;
    vector.host.locks.push(LockSpec {
        path: path.clone(),
        holder: 42,
        mode: LockModeSpec::Exclusive,
    });
    let plan = vector
        .expect
        .plan
        .iter_mut()
        .find(|plan| plan.candidate.as_deref() == Some(Path::new(CARGO_TARGET_PATH)))
        .ok_or("Cargo target plan missing")?;
    plan.reason = Some("in_use".into());
    vector.expect.actions.clear();
    vector.expect.removed.clear();
    vector.expect.skipped.push(SkipVerdict {
        candidate: Some(CARGO_TARGET_PATH.into()),
        reason: "locked".into(),
        process_blockers: None,
    });
    vector.expect.preserved.extend([
        "/home/tester/project/good/target/debug/.fingerprint/good-hash/bin-good".into(),
        "/home/tester/project/good/target/debug/deps/good".into(),
    ]);
    Ok(vector)
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

pub fn python_bind_root_matches_linux(ctx: &Ctx) -> TestResult {
    replay_model(ctx, python_bind_root_vector()?, false)
}

/// Compare the kernel's cross-mount rename refusal and the inode shared by
/// an unlink through the bind alias with the model's path operations.
pub fn bind_root_alias_mutations_match_linux(ctx: &Ctx) -> TestResult {
    let mut vector = python_bind_root_vector()?;
    adapt_home_mount(&mut vector);
    let (mut state, _) =
        diskgc_model::load::load(&vector.host).map_err(|error| error.to_string())?;
    let materialized = projected_materialize(ctx, &vector)?;
    let source = Path::new("/home/tester/visible");
    let alias_root = Path::new("/home/tester/project/app/__pycache__/mounted");
    let alias = alias_root.join("visible");
    ensure!(
        state.rename_path(source, &alias) == Err(Fault::Errno(Errno::EXDEV)),
        "model allowed a cross-mount rename"
    );
    let kernel_error =
        fs::rename(source, &alias).expect_err("kernel must refuse a rename across bind mount IDs");
    ensure!(
        kernel_error.kind() == io::ErrorKind::CrossesDevices,
        "kernel cross-bind rename: {kernel_error}"
    );
    let alias_dir = state
        .lookup(alias_root)
        .map_err(|error| format!("model alias: {error:?}"))?;
    state
        .unlinkat(alias_dir, std::ffi::OsStr::new("visible"), false)
        .map_err(|error| format!("model alias unlink: {error:?}"))?;
    let bound = Scratch::attach(alias_root.to_str().ok_or("non-UTF-8 bind root")?)?;
    bound.remove_file("visible")?;
    ensure!(
        state.lookup(source) == Err(Fault::Errno(Errno::ENOENT)),
        "model retained the source name after alias unlink"
    );
    ensure!(
        fs::symlink_metadata(source).is_err_and(|error| error.kind() == io::ErrorKind::NotFound),
        "kernel retained the source name after alias unlink"
    );
    materialized.finish()?;
    Ok(())
}

fn python_bind_root_vector() -> Result<ProfileVector, String> {
    let mut vector = python_mounted_cache_vector(false)?;
    let source = vector.host.mounts[1].clone();
    let mount = vector.host.mounts.last_mut().ok_or("missing bind mount")?;
    mount.bind_of = Some(2);
    mount.dev = source.dev;
    mount.fs = "tmpfs".into(); // replay_model adapts /home to this scratch filesystem
    mount.size = source.size;
    let at = mount.at.clone();
    vector.host.tree.remove(&at.join("visible"));
    vector.host.tree.insert(
        "/home/tester/visible".into(),
        NodeSpec::File(FileSpec {
            content: Some("shared filesystem root".into()),
            ..FileSpec::default()
        }),
    );
    vector.expect.preserved.push("/home/tester/visible".into());
    Ok(vector)
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
        bind_of: None,
        uuid: None,
        size: Bytes(1 << 20),
        other_used: Bytes(0),
        parent: Some(2),
        read_only: false,
        dead: false,
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
        .find(|entry| entry.candidate.as_ref() == Some(&cache))
        .ok_or("Python cache candidate missing from expectation")?;
    plan.reason = Some("contains_mount".into());
    vector.expect.actions.clear();
    vector.expect.removed.clear();
    vector.expect.skipped.push(SkipVerdict {
        candidate: Some(cache),
        reason: "contains_mount".into(),
        process_blockers: None,
    });
    vector.expect.preserved.push(at.join("visible"));
    Ok(vector)
}

fn python_in_use_probe_vector(probe_size: u64) -> Result<ProfileVector, String> {
    let mut vector = ProfileVector::parse(PYTHON_IN_USE_VECTOR)?;
    vector.host.tree.remove(Path::new("/usr/bin/python"));
    let [process] = vector.host.processes.as_slice() else {
        return Err("Python in-use vector must have exactly one process".into());
    };
    let pid = process.pid;
    as_copied_probe(&mut vector, pid, probe_size)?;
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

/// Vary the tagged Mypy cache independently of the Python bytecode cases.
/// The extra branches exercise the required tag, checkout refusal, and
/// similar names while each model keeps a distinct search root.
pub fn generated_mypy_models_match_linux(ctx: &Ctx) -> TestResult {
    for index in 0..100 {
        let vector = generated_mypy_vector(index)?;
        replay_model(ctx, vector, false)
            .map_err(|error| format!("generated Mypy model {index}: {error}"))?;
    }
    Ok(())
}

fn generated_mypy_vector(index: u32) -> Result<ProfileVector, String> {
    let root = format!("/home/tester/differential-mypy-{index:03}");
    let project = format!("{root}/project");
    let yaml = MYPY_VECTOR.replace("/home/tester/project", &project);
    let mut vector = ProfileVector::parse(&yaml)?;
    vector.path = root.clone().into();
    vector.roots = vec![root];

    let base_cache = PathBuf::from(format!("{project}/.mypy_cache"));
    let Some(NodeSpec::Dir(cache)) = vector.host.tree.get_mut(&base_cache) else {
        return Err("generated Mypy vector is missing its tagged base cache".into());
    };
    let Some(NodeSpec::Dir(version)) = cache.entries.get_mut("3.13") else {
        return Err("generated Mypy vector is missing its version directory".into());
    };
    version.entries.insert(
        format!("generated-{index}.json"),
        NodeSpec::File(FileSpec {
            size: Some(Bytes(1024 + u64::from(index) * 97)),
            ..FileSpec::default()
        }),
    );

    let extra = PathBuf::from(format!("{project}/variation/.mypy_cache"));
    let mut cache = DirSpec::default();
    cache.entries.insert(
        "payload.json".into(),
        NodeSpec::File(FileSpec {
            size: Some(Bytes(512 + u64::from(index) * 31)),
            ..FileSpec::default()
        }),
    );
    match index % 4 {
        0 => {} // Only the tagged base cache is eligible.
        1 | 3 => {
            cache.entries.insert(
                "CACHEDIR.TAG".into(),
                NodeSpec::File(FileSpec {
                    cachedir_tag: true,
                    ..FileSpec::default()
                }),
            );
            if index % 4 == 3 {
                let mut git = DirSpec::default();
                git.entries
                    .insert("HEAD".into(), NodeSpec::File(FileSpec::default()));
                cache.entries.insert(".git".into(), NodeSpec::Dir(git));
                vector.expect.plan.push(PlanVerdict {
                    candidate: Some(extra.clone()),
                    reason: Some("contains_checkout".into()),
                    source: None,
                });
                vector.expect.skipped.push(SkipVerdict {
                    candidate: Some(extra.clone()),
                    reason: "contains_checkout".into(),
                    process_blockers: None,
                });
                vector.expect.preserved.push(extra.join(".git/HEAD"));
            } else {
                vector.expect.plan.push(PlanVerdict {
                    candidate: Some(extra.clone()),
                    reason: None,
                    source: None,
                });
                vector.expect.actions.push(extra.clone());
                vector.expect.removed.push(extra.clone());
            }
        }
        2 => {
            // The path matches, but an absent tag keeps it out of the plan.
            vector.expect.preserved.push(extra.join("payload.json"));
        }
        _ => unreachable!(),
    }
    if !index.is_multiple_of(4) {
        vector.host.tree.insert(extra, NodeSpec::Dir(cache));
    }
    Ok(vector)
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
                candidate: Some(extra.clone()),
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
                candidate: Some(extra.clone()),
                reason: Some("contains_checkout".into()),
                source: None,
            });
            vector.expect.skipped.push(SkipVerdict {
                candidate: Some(extra.clone()),
                reason: "contains_checkout".into(),
                process_blockers: None,
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

fn replay_model(ctx: &Ctx, vector: ProfileVector, with_dangling_link: bool) -> TestResult {
    replay_model_with_profile(
        ctx,
        vector,
        with_dangling_link,
        None,
        None,
        false,
        projected_materialize,
    )
}

/// A single old timestamp cohort can be recreated with a real tmpfs ctime:
/// shift its explicit mtimes to kernel-now, and advance only the sandboxed
/// binary's clock by the cohort's original age. Default-time nodes are not
/// selected by these vectors; the independent expectation checks that.
fn replay_single_cohort_vector(ctx: &Ctx, yaml: &str) -> TestResult {
    let mut vector = ProfileVector::parse(yaml)?;
    vector.check(&Library::builtin().map_err(|error| error.to_string())?)?;
    let offset_seconds = rebase_single_cohort(&mut vector)?;
    let _clock_offset = SandboxClockOffset::install(offset_seconds)?;
    replay_model_with_profile(ctx, vector, false, None, None, true, projected_materialize)
}

/// The development binary's test-only wall-clock advance
/// (`diskgc_linux::sandbox::CLOCK_OFFSET_PATH`), written in the disposable
/// container's overlay root and removed when dropped. Shared with the Owner
/// PID tests, whose candidates are made now and must clear a floor.
pub(crate) struct SandboxClockOffset;

impl SandboxClockOffset {
    pub(crate) fn install(seconds: u64) -> Result<Self, String> {
        let path = diskgc_linux::sandbox::CLOCK_OFFSET_PATH;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(path)
            .map_err(|error| format!("create {path} in disposable container: {error}"))?;
        let guard = Self;
        use std::io::Write;
        writeln!(file, "{seconds}").map_err(|error| format!("write {path}: {error}"))?;
        Ok(guard)
    }
}

impl Drop for SandboxClockOffset {
    fn drop(&mut self) {
        fs::remove_file(diskgc_linux::sandbox::CLOCK_OFFSET_PATH)
            .expect("remove the sandbox clock offset from the disposable container");
    }
}

fn rebase_single_cohort(vector: &mut ProfileVector) -> Result<u64, String> {
    if vector
        .expect
        .plan
        .iter()
        .any(|entry| entry.reason.as_deref() == Some("not_idle"))
    {
        return Err("single-cohort replay cannot preserve a fresh not_idle candidate".into());
    }
    let [candidate] = vector.expect.removed.as_slice() else {
        return Err("single-cohort replay requires one independently expected removal".into());
    };
    let node = vector.host.tree.get(candidate).ok_or_else(|| {
        format!(
            "single-cohort replay has no explicit candidate {}",
            candidate.display()
        )
    })?;
    let old = match node {
        NodeSpec::File(file) => file.mtime,
        NodeSpec::Dir(dir) => dir.attrs.mtime,
    }
    .ok_or_else(|| {
        format!(
            "single-cohort candidate {} has no old mtime",
            candidate.display()
        )
    })?;
    for node in vector.host.tree.values() {
        require_one_explicit_mtime(node, old)?;
    }
    let age_nanos = vector.host.clock.0.as_nanosecond() - old.0.as_nanosecond();
    const NANOS_PER_SECOND: i128 = 1_000_000_000;
    if age_nanos <= 0 || age_nanos % NANOS_PER_SECOND != 0 {
        return Err(format!(
            "single-cohort age must be positive whole seconds: {age_nanos}ns"
        ));
    }
    let seconds = u64::try_from(age_nanos / NANOS_PER_SECOND)
        .map_err(|error| format!("single-cohort age overflows u64: {error}"))?;
    if seconds > 366 * 24 * 60 * 60 {
        return Err(format!(
            "single-cohort age {seconds}s exceeds sandbox clock offset bound"
        ));
    }
    let shift = jiff::Timestamp::now().as_nanosecond() - old.0.as_nanosecond();
    vector.host.clock = shifted_wall_time(vector.host.clock, shift)?;
    for node in vector.host.tree.values_mut() {
        shift_node_mtime(node, shift)?;
    }
    Ok(seconds)
}

fn require_one_explicit_mtime(node: &NodeSpec, old: WallTime) -> TestResult {
    match node {
        NodeSpec::File(file) if file.mtime.is_some_and(|time| time != old) => {
            Err("single-cohort replay contains a second explicit age".into())
        }
        NodeSpec::Dir(dir) => {
            if dir.attrs.mtime.is_some_and(|time| time != old) {
                return Err("single-cohort replay contains a second explicit age".into());
            }
            for child in dir.entries.values() {
                require_one_explicit_mtime(child, old)?;
            }
            Ok(())
        }
        NodeSpec::File(_) => Ok(()),
    }
}

fn shifted_wall_time(time: WallTime, nanos: i128) -> Result<WallTime, String> {
    let shifted = time
        .0
        .as_nanosecond()
        .checked_add(nanos)
        .ok_or("single-cohort timestamp shift overflowed")?;
    jiff::Timestamp::from_nanosecond(shifted)
        .map(WallTime)
        .map_err(|error| format!("single-cohort timestamp is out of range: {error}"))
}

fn shift_node_mtime(node: &mut NodeSpec, nanos: i128) -> TestResult {
    match node {
        NodeSpec::File(file) => {
            if let Some(time) = file.mtime.as_mut() {
                *time = shifted_wall_time(*time, nanos)?;
            }
        }
        NodeSpec::Dir(dir) => {
            if let Some(time) = dir.attrs.mtime.as_mut() {
                *time = shifted_wall_time(*time, nanos)?;
            }
            for child in dir.entries.values_mut() {
                shift_node_mtime(child, nanos)?;
            }
        }
    }
    Ok(())
}

/// The CLI config a replay runs with, from the vector's user profiles: none
/// without them; a hand-written `profile_yaml` must parse to exactly the
/// vector's typed profiles; otherwise the profiles as `profiles show` prints
/// them, which must load back to the same profiles (a probe profile of a
/// withdrawn built-in needs no hand-kept copy).
fn cli_config(
    vector: &ProfileVector,
    profile_yaml: Option<&str>,
) -> Result<Option<String>, String> {
    let indent = |yaml: &str| -> String {
        format!(
            "version: 1\nprofiles:\n{}",
            yaml.lines()
                .map(|line| format!("  {line}\n"))
                .collect::<String>()
        )
    };
    if let Some(profile_yaml) = profile_yaml {
        ensure!(
            parse_profiles(profile_yaml)? == vector.user_profiles,
            "real CLI profile differs from the model vector's typed profile"
        );
        return Ok(Some(indent(profile_yaml)));
    }
    if vector.user_profiles.is_empty() {
        return Ok(None);
    }
    let library = Library::with_user_profiles(&vector.user_profiles).map_err(|e| e.to_string())?;
    let mut yaml = String::new();
    for spec in &vector.user_profiles {
        let profile = library
            .get(&spec.name)
            .ok_or_else(|| format!("user profile `{}` did not load", spec.name))?;
        yaml.push_str(&describe::show_yaml(profile, None));
    }
    let reloaded =
        Library::with_user_profiles(&parse_profiles(&yaml)?).map_err(|e| e.to_string())?;
    for spec in &vector.user_profiles {
        ensure!(
            reloaded.get(&spec.name) == library.get(&spec.name),
            "generated CLI profile `{}` does not load back to the vector's profile",
            spec.name
        );
    }
    Ok(Some(indent(&yaml)))
}

fn replay_model_with_profile(
    ctx: &Ctx,
    mut vector: ProfileVector,
    with_dangling_link: bool,
    profile_yaml: Option<&str>,
    unrelated_to: Option<&Path>,
    sandbox_clock_offset: bool,
    materialize: fn(&Ctx, &ProfileVector) -> Result<model_materialize::Materialized, String>,
) -> TestResult {
    // Preflight the semantic contract before even writing the CLI config.
    // The model's old clock and ctime cannot be installed on a fresh tmpfs.
    adapt_home_mount(&mut vector);
    if vector.profile.as_deref() == Some(CONTAINERS_IMAGE_PROBE) {
        adapt_var_tmp_mount(&mut vector)?;
    }
    preflight_vector(&vector, sandbox_clock_offset)?;
    let config_path = match cli_config(&vector, profile_yaml)? {
        Some(config) => Some(ctx.home.write_file(
            "differential-user-profile.yaml",
            config.as_bytes(),
            0o644,
            TESTER,
        )?),
        None => None,
    };
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
    let materialized = materialize(ctx, &vector)?;
    if let Some(candidate) = unrelated_to {
        materialized.assert_no_reference_to(candidate)?;
    }
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
    let select = vector
        .select_expression()
        .ok_or("replay needs a profile or select")?;
    let path = vector.root_path().to_str().ok_or("non-UTF-8 vector path")?;
    let mut args = vec![
        "--json",
        "--no-index",
        "--min-free",
        vector.min_free.as_str(),
        "--select",
        &select,
    ];
    if let Some(config_path) = &config_path {
        args.push("--config");
        args.push(
            config_path
                .to_str()
                .ok_or("non-UTF-8 user profile config path")?,
        );
    }
    for root in &vector.roots {
        args.push("--roots");
        args.push(root);
    }
    args.push(path);
    let mut dry_args = vec!["--dry-run"];
    dry_args.extend(&args);
    let dry = ensure_run(ctx, &dry_args)?;
    materialized.assert_holding()?;
    if let Some(candidate) = unrelated_to {
        materialized.assert_no_reference_to(candidate)?;
    }
    let reclaim = ensure_run(ctx, &args)?;
    materialized.assert_holding()?;
    if let Some(candidate) = unrelated_to {
        materialized.assert_no_reference_to(candidate)?;
    }
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

fn projected_materialize(
    ctx: &Ctx,
    vector: &ProfileVector,
) -> Result<model_materialize::Materialized, String> {
    let mut stages = model_materialize::OutcomeStagePlan::checked(vector, vec![&vector.host])?;
    model_materialize::materialize_outcome_projection(ctx, stages.next_stage()?)
}

fn preflight_vector(vector: &ProfileVector, sandbox_clock_offset: bool) -> Result<(), String> {
    let library = if vector.user_profiles.is_empty() {
        Library::builtin().map_err(|error| error.to_string())?
    } else {
        Library::with_user_profiles(&vector.user_profiles)
            .map_err(|error| format!("vector user profiles: {error}"))?
    };
    let tier = Tier::select(
        &vector
            .select_expression()
            .ok_or("replay needs a profile or select")?,
        &library,
        (!vector.roots.is_empty()).then_some(vector.roots.as_slice()),
        vector.host.own_uid.unwrap_or(1000),
    )?;
    if !sandbox_clock_offset && (tier.idle_for.is_some() || tier.idle_before.is_some()) {
        return Err(format!(
            "materialize(model): selection {} idle_for is model-only: fresh tmpfs ctime cannot reproduce the model clock and historical inode changes",
            vector.select_expression().unwrap_or_default()
        ));
    }
    model_materialize::validate(&vector.host)
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

fn adapt_var_tmp_mount(vector: &mut ProfileVector) -> TestResult {
    ensure!(
        vector.host.mounts.len() == 1,
        "container spool vector must have only its original root mount"
    );
    let mut mount = vector.host.mounts[0].clone();
    mount.id = 2;
    mount.at = "/var/tmp".into();
    mount.fs = "tmpfs".into();
    mount.dev = DevSpec(Dev {
        major: 254,
        minor: 2,
    });
    mount.parent = Some(1);
    vector.host.mounts.push(mount);
    Ok(())
}

fn model_outcome(vector: &ProfileVector) -> Result<Outcome, String> {
    let library = if vector.user_profiles.is_empty() {
        Library::builtin().map_err(|e| e.to_string())?
    } else {
        Library::with_user_profiles(&vector.user_profiles).map_err(|e| e.to_string())?
    };
    let tier = Tier::select(
        &vector
            .select_expression()
            .ok_or("replay needs a profile or select")?,
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
                reason: entry.reason.map(|reason| reason.as_str().to_owned()),
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
                reason: entry.reason.as_str().to_owned(),
                process_blockers: None,
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
                    candidate: optional_path(&entry["candidate"])
                        .ok_or("plan candidate is neither a path nor null")?,
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
                    candidate: optional_path(&entry["candidate"])
                        .ok_or("skip candidate is neither a path nor null")?,
                    reason: entry["reason"]
                        .as_str()
                        .ok_or("skip reason missing")?
                        .to_owned(),
                    // A Linux replay compares candidates and reasons; vectors it
                    // replays leave process_blockers unset.
                    process_blockers: None,
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

/// A report's `candidate`: a path, or `null` for a row of the tier as a
/// whole (TASK-192); `None` for anything else.
fn optional_path(value: &Value) -> Option<Option<PathBuf>> {
    match value {
        Value::Null => Some(None),
        Value::String(path) => Some(Some(path.into())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Any size will do for the model: the copied probe's real size only
    /// matters where it is materialized.
    const UNIT_PROBE_SIZE: u64 = 1 << 20;

    /// Every checked-in model fixture with user profiles gets a CLI config
    /// that loads back to the same profiles, so a container replay never
    /// refuses a probe profile (TASK-122: six fixtures became probes).
    #[test]
    fn every_fixture_with_user_profiles_has_a_cli_config() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../crates/diskgc-model/tests/models/profiles");
        let mut with_profiles = 0;
        for entry in fs::read_dir(&dir).expect("fixture directory") {
            let path = entry.expect("fixture entry").path();
            if path.extension().is_none_or(|ext| ext != "yaml") {
                continue;
            }
            let yaml = fs::read_to_string(&path).expect("fixture");
            let vector = ProfileVector::parse(&yaml).expect("fixture parses");
            if vector.user_profiles.is_empty() {
                assert_eq!(cli_config(&vector, None), Ok(None), "{}", path.display());
                continue;
            }
            with_profiles += 1;
            let config = cli_config(&vector, None)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
                .expect("a config");
            assert!(config.starts_with("version: 1\nprofiles:\n"), "{config}");
        }
        assert!(
            with_profiles >= 6,
            "the six withdrawn built-ins' probes are fixtures"
        );
    }

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
    fn generated_mypy_models_have_independent_expectations() {
        let library = Library::builtin().expect("built-in library");
        let mut roots = std::collections::BTreeSet::new();
        for index in 0..100 {
            let vector = generated_mypy_vector(index).expect("generated model");
            assert!(
                roots.insert(vector.path.clone()),
                "generated roots must differ"
            );
            assert_eq!(vector.expect.actions.len(), 1 + usize::from(index % 4 == 1));
            vector
                .check(&library)
                .expect("independent generated Mypy expectation");
        }
        let mut broken = generated_mypy_vector(1).expect("generated model");
        let extra = broken.expect.actions[1].clone();
        let Some(NodeSpec::Dir(cache)) = broken.host.tree.get_mut(&extra) else {
            panic!("generated extra cache missing");
        };
        cache.entries.remove("CACHEDIR.TAG");
        assert!(
            broken.check(&library).is_err(),
            "an eligible expectation must fail without the required tag"
        );
    }

    #[test]
    fn projected_in_use_vector_keeps_its_independent_expected_verdict() {
        let mut vector =
            python_in_use_probe_vector(UNIT_PROBE_SIZE).expect("checked-in in-use vector");
        adapt_home_mount(&mut vector);
        model_materialize::validate(&vector.host).expect("probe-supported process shape");
        vector
            .check(&Library::builtin().expect("built-in library"))
            .expect("the checked-in in-use expectation");
    }

    #[test]
    fn held_cargo_lock_has_an_independent_refusal_expectation() {
        for mode in [LockModeSpec::Exclusive, LockModeSpec::Shared] {
            let mut vector =
                cargo_target_held_lock_vector(UNIT_PROBE_SIZE).expect("held lock vector");
            vector.host.locks[0].mode = mode;
            adapt_home_mount(&mut vector);
            vector
                .check(&Library::builtin().expect("built-in library"))
                .expect("checked independent locked verdict");
            model_materialize::validate(&vector.host).expect("supported lock holder");
        }
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
                    .contains("repeated devices require bind_of"),
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
    fn bind_root_has_independent_refusal_and_shared_inode() {
        let mut vector = python_bind_root_vector().expect("bind vector");
        adapt_home_mount(&mut vector);
        vector
            .check(&Library::builtin().expect("built-in library"))
            .expect("bind refusal expectation");
        model_materialize::validate(&vector.host).expect("supported root bind");
        let host = ModelHost::new(&vector.host).expect("valid bind model");
        let source = host
            .stat_path(Path::new("/home/tester/visible"))
            .expect("source stat");
        let alias = host
            .stat_path(Path::new(
                "/home/tester/project/app/__pycache__/mounted/visible",
            ))
            .expect("bind stat");
        assert_eq!(source.dev_ino(), alias.dev_ino());
        assert_ne!(source.mnt_id, alias.mnt_id);
    }

    #[test]
    fn projected_browser_vectors_keep_independent_expectations() {
        let library = Library::builtin().expect("built-in library");
        for yaml in BROWSER_VECTORS {
            let original = ProfileVector::parse(yaml).expect("checked-in browser vector");
            original.check(&library).expect("original expectation");
            let mut projected = project_browser_vector(original).expect("known browser shape");
            adapt_home_mount(&mut projected);
            model_materialize::validate(&projected.host).expect("projected scratch tree");
            projected.check(&library).expect("projected expectation");
            let removed = projected.expect.removed[0].clone();
            projected.host.tree.remove(&removed);
            assert!(
                projected.check(&library).is_err(),
                "missing eligible cache must break the independent expectation"
            );
        }
    }

    #[test]
    fn browser_projection_rejects_paths_outside_the_two_homes() {
        let mut vector = ProfileVector::parse(BRAVE_BROWSER_VECTOR).expect("browser vector");
        vector.host.tree.insert(
            "/home/other/unsafe".into(),
            NodeSpec::File(FileSpec::default()),
        );
        let error = project_browser_vector(vector).expect_err("unexpected home must fail");
        assert!(error.contains("/home/other/unsafe"), "{error}");

        let mut vector = ProfileVector::parse(BRAVE_BROWSER_VECTOR).expect("browser vector");
        vector.host.tree.insert(
            "/home/clear/absolute-link".into(),
            NodeSpec::File(FileSpec {
                symlink: Some("/home/locked/private".into()),
                ..FileSpec::default()
            }),
        );
        let error = project_browser_vector(vector).expect_err("absolute link must fail");
        assert!(error.contains("absolute-link"), "{error}");
    }

    #[test]
    fn pytest_process_projection_keeps_the_unsafe_action_and_unrelated_evidence() {
        let original = ProfileVector::parse(PYTEST_UNSAFE_VECTOR).expect("pytest vector");
        original
            .check(&Library::builtin().expect("built-in library"))
            .expect("original independent expectation");
        let mut vector =
            project_pytest_unrelated_process(original, 4096).expect("narrow process projection");
        assert_eq!(
            parse_profiles(PYTEST_UNSAFE_PROFILE).expect("CLI profile"),
            vector.user_profiles,
            "the real CLI config must select the same typed user profile"
        );
        vector
            .check(&Library::builtin().expect("built-in library"))
            .expect("projected unsafe action expectation");
        model_materialize::validate(&vector.host).expect("copied probe preflight");
        assert_eq!(vector.expect.actions, [PathBuf::from(PYTEST_ACTIVE)]);
        assert!(
            vector
                .expect
                .preserved
                .contains(&PathBuf::from(PYTEST_RETAINED))
        );
        assert!(
            !Path::new(PYTEST_RETAINED).starts_with("/tmp/pytest-of-tester"),
            "retained pressure file must be outside the pytest profile roots"
        );
        assert!(matches!(
            vector.host.tree.get(Path::new(PYTEST_RETAINED)),
            Some(NodeSpec::File(FileSpec {
                size: Some(Bytes(8192)),
                ..
            }))
        ));
        let process = &vector.host.processes[0];
        assert!(
            process
                .exe
                .as_deref()
                .is_some_and(|path| !path.starts_with(PYTEST_ACTIVE))
        );
        assert!(
            process
                .maps
                .iter()
                .all(|path| !path.starts_with(PYTEST_ACTIVE))
        );
        assert!(
            process
                .fds
                .iter()
                .all(|fd| !fd.path.starts_with(PYTEST_ACTIVE))
        );
        assert!(
            process
                .cwd
                .as_deref()
                .is_none_or(|path| !path.starts_with(PYTEST_ACTIVE))
        );
        assert!(
            process
                .environ
                .iter()
                .all(|entry| !entry.contains(PYTEST_ACTIVE))
        );

        vector.host.processes[0].maps = vec![PYTEST_ACTIVE.into()];
        let error = model_materialize::validate(&vector.host)
            .expect_err("a mapping inside the active candidate must fail preflight");
        assert!(error.contains("exe/maps/environ is model-only"), "{error}");
    }

    #[test]
    fn every_checked_in_profile_vector_has_an_explicit_replay_classification() {
        use std::collections::BTreeSet;

        let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../crates/diskgc-model/tests/models/profiles");
        let mut files: Vec<_> = fs::read_dir(&directory)
            .expect("profile fixture directory")
            .map(|entry| entry.expect("fixture directory entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "yaml"))
            .collect();
        files.sort();
        let listed: BTreeSet<_> = PROFILE_FIXTURES.iter().map(|entry| entry.file).collect();
        assert_eq!(
            listed.len(),
            PROFILE_FIXTURES.len(),
            "duplicate fixture classification"
        );
        let on_disk: BTreeSet<_> = files
            .iter()
            .map(|path| path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(
            listed, on_disk,
            "every checked-in vector needs its own classification"
        );
        for path in files {
            let yaml = fs::read_to_string(&path).expect("read profile fixture");
            let mut vector = ProfileVector::parse(&yaml).expect("parse profile fixture");
            adapt_home_mount(&mut vector);
            let name = path.file_name().expect("fixture name").to_string_lossy();
            let classification = PROFILE_FIXTURES
                .iter()
                .find(|entry| entry.file == name)
                .expect("listed fixture classification");
            match classification.replay {
                FixtureReplay::Direct { test, run } => {
                    preflight_vector(&vector, false).unwrap_or_else(|error| {
                        panic!("{name}: direct replay fails preflight: {error}")
                    });
                    let registered = crate::suite::TESTS
                        .iter()
                        .find(|entry| entry.name == test)
                        .unwrap_or_else(|| panic!("{name}: missing container replay {test}"));
                    assert_eq!(registered.tier, crate::suite::Tier::Fast, "{name}");
                    assert!(
                        std::ptr::fn_addr_eq(registered.run, run),
                        "{name}: wrong replay runner"
                    );
                }
                FixtureReplay::Projected { reason, test, run } => {
                    assert!(!reason.is_empty(), "{name}: projection reason is empty");
                    let error = preflight_vector(&vector, false)
                        .expect_err("original projected vector must refuse direct materialization");
                    assert!(
                        error.contains("model-only")
                            || error.contains("outside the container scratch mounts"),
                        "{name}: {error}"
                    );
                    let registered = crate::suite::TESTS
                        .iter()
                        .find(|entry| entry.name == test)
                        .unwrap_or_else(|| panic!("{name}: missing projected replay {test}"));
                    assert_eq!(registered.tier, crate::suite::Tier::Fast, "{name}");
                    assert!(
                        std::ptr::fn_addr_eq(registered.run, run),
                        "{name}: wrong projection runner"
                    );
                }
                FixtureReplay::Vm {
                    reason, test, run, ..
                } => {
                    assert!(reason.contains("historical ctime"), "{name}: {reason}");
                    let error = preflight_vector(&vector, false)
                        .expect_err("historical ctime must be rejected before writes");
                    assert!(
                        error.contains("ctime") && error.contains("model-only"),
                        "{name}: {error}"
                    );
                    let registered = crate::suite::TESTS
                        .iter()
                        .find(|entry| entry.name == test)
                        .unwrap_or_else(|| panic!("{name}: missing VM replay {test}"));
                    assert_eq!(registered.tier, crate::suite::Tier::Vm, "{name}");
                    assert!(
                        std::ptr::fn_addr_eq(registered.run, run),
                        "{name}: wrong VM runner"
                    );
                }
                FixtureReplay::KernelCounterpart {
                    reason,
                    test,
                    tier,
                    run,
                } => {
                    assert!(reason.contains("model-only"), "{name}: {reason}");
                    let error = preflight_vector(&vector, false)
                        .expect_err("a model-only interleaving must refuse materialization");
                    assert!(error.contains("events is model-only"), "{name}: {error}");
                    let registered = crate::suite::TESTS
                        .iter()
                        .find(|entry| entry.name == test)
                        .unwrap_or_else(|| panic!("{name}: missing kernel counterpart {test}"));
                    assert_eq!(registered.tier, tier, "{name}");
                    assert!(
                        std::ptr::fn_addr_eq(registered.run, run),
                        "{name}: wrong kernel counterpart runner"
                    );
                }
            }
        }
    }

    #[test]
    fn every_non_profile_host_model_has_a_model_only_classification() {
        fn yaml_below(root: &Path, base: &Path, found: &mut BTreeSet<PathBuf>) {
            for entry in fs::read_dir(root).expect("model fixture directory") {
                let path = entry.expect("model fixture entry").path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|name| name != "profiles") {
                        yaml_below(&path, base, found);
                    }
                } else if path
                    .extension()
                    .is_some_and(|extension| extension == "yaml")
                {
                    found.insert(path.strip_prefix(base).expect("relative model path").into());
                }
            }
        }
        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../crates/diskgc-model/tests/models");
        let mut on_disk = BTreeSet::new();
        yaml_below(&base, &base, &mut on_disk);
        let classified: BTreeSet<_> = NON_PROFILE_MODELS
            .iter()
            .map(|(name, _, _, _)| PathBuf::from(name))
            .collect();
        assert_eq!(
            classified.len(),
            NON_PROFILE_MODELS.len(),
            "duplicate classification"
        );
        assert_eq!(on_disk, classified, "unclassified non-profile host model");
        for (name, model, expected, second_expected) in
            non_profile_models().expect("parse original YAMLs")
        {
            let error = model_materialize::validate(&model)
                .expect_err("model-only original unexpectedly passed preflight");
            assert!(error.contains(expected), "{name}: {error}");
            if let Some(second_expected) = second_expected {
                let isolated_accounting = isolate_sketch_mount_accounting(&model);
                let error = model_materialize::validate(&isolated_accounting)
                    .expect_err("second model-only field unexpectedly passed preflight");
                assert!(error.contains(second_expected), "{name}: {error}");
            }
        }
    }

    #[test]
    fn model_only_field_table_bites_on_each_preflight_family() {
        for (name, model, expected) in model_only_field_cases().expect("field case table") {
            let error = model_materialize::validate(&model)
                .expect_err("model-only field unexpectedly passed preflight");
            assert!(error.contains(expected), "{name}: {error}");
        }
    }

    #[test]
    fn single_cohort_rebase_preserves_original_age_and_refuses_mixed_age() {
        let library = Library::builtin().expect("built-in profiles");
        for yaml in [WEST_VECTOR, XDG_VECTOR] {
            let mut vector = ProfileVector::parse(yaml).expect("checked-in vector");
            let original = vector.host.clock;
            vector.check(&library).expect("original expectation");
            let offset = rebase_single_cohort(&mut vector).expect("single age cohort");
            assert!(offset >= 30 * 24 * 60 * 60);
            assert_ne!(vector.host.clock, original);
            vector.check(&library).expect("rebased expectation");
            adapt_home_mount(&mut vector);
            preflight_vector(&vector, true).expect("sandbox clock replay preflight");
        }
        for yaml in [
            include_str!(
                "../../crates/diskgc-model/tests/models/profiles/autotools-source-anchor.yaml"
            ),
            include_str!(
                "../../crates/diskgc-model/tests/models/profiles/cmake-source-anchor.yaml"
            ),
            include_str!(
                "../../crates/diskgc-model/tests/models/profiles/meson-source-anchor.yaml"
            ),
            include_str!(
                "../../crates/diskgc-model/tests/models/profiles/containers-image-age.yaml"
            ),
        ] {
            let mut vector = ProfileVector::parse(yaml).expect("checked-in mixed-age vector");
            let error = rebase_single_cohort(&mut vector)
                .expect_err("a uniform clock offset cannot preserve fresh candidates");
            assert!(error.contains("fresh not_idle"), "{error}");
        }
    }
}
