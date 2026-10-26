//! Container tests of the `fs` kind's reclaim (task-18; docs/safety.md,
//! Filesystem candidates): the real binary's reclaiming `ensure` over user
//! profiles from a config file of the test's own (no built-in names), run
//! as `tester` under strace and judged by the deletion oracle, whose
//! eligible set the harness computes from the fixture it built, never from
//! diskgc's output. Where these tests need the kernel's view: real
//! sockets and FIFOs, a real mount, a process's working directory, a
//! `systemd-private-*` directory, a `/tmp` of 100,000 entries, and a FUSE
//! mount whose server never answers.

use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::fs::{Mode, OFlags};
use rustix::mount::{MountFlags, UnmountFlags};
use serde_json::Value;

use crate::ensure_suite::{ensure_run, report};
use crate::holder::Holder;
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, TestResult};
use crate::under_test::Run;
use crate::{OTHER, ROOT, TESTER, ensure, info};

/// Where each test writes its configuration (tester's home tmpfs).
const CONFIG: &str = "/home/tester/diskgc.yaml";

/// The same two temporary name classes as the built-in profile, rooted in
/// the container's scratch `/tmp`. Its short floor makes an old candidate
/// possible without changing the container clock; the built-in's exact 3d
/// floor is pinned by the profile model test.
const IMAGE_TEMP_TEST_PROFILE: &str = r#"version: 1
profiles:
  - name: image-temp-test
    find: {roots: [/tmp], paths: ["buildah*", "container_images_storage*"], type: any}
    min_idle_for: 10s
    action: delete
    examples: {match: [/tmp/buildah-1, /tmp/container_images_storage-1]}
"#;

const IDLE_WINDOW_TEST_PROFILE: &str = r#"version: 1
profiles:
  - name: idle-window-kernel
    find: {roots: [/tmp], paths: ["idle-window-*"], type: dir}
    action: delete
    examples: {match: [/tmp/idle-window-old, /tmp/idle-window-recent]}
  - name: idle-window-test
    where: {idle_for: 10s}
"#;

const ENTRY_NEGATION_TEST_PROFILE: &str = r#"version: 1
profiles:
  - name: entry-negation-kernel
    find: {roots: [/tmp], paths: ["entry-negation-*"], type: dir}
    action: delete
    examples: {match: [/tmp/entry-negation-plain, /tmp/entry-negation-tagged]}
  - name: entry-has-tag
    where: {has: CACHEDIR.TAG}
  - name: entry-has-not-tag
    where: {has_not: CACHEDIR.TAG}
"#;

const MIXED_ENTRY_FILTER_TEST_PROFILE: &str = r#"version: 1
profiles:
  - name: mixed-entry-kernel
    find: {roots: [/tmp], paths: ["entry-mixed-*"], type: dir}
    action: delete
    examples: {match: [/tmp/entry-mixed-good]}
  - name: mixed-tagged-idle-unkept
    where: {has: CACHEDIR.TAG, has_not: DO_NOT_DELETE, idle_for: 10s}
"#;

// The recent candidate is created only after this wait. Five seconds of
// margin keep container setup and CLI startup from crossing its 10s bound.
const MIXED_FILTER_AGE_SETTLE: Duration = Duration::from_secs(15);

const FS_UNION_TEST_PROFILE: &str = r#"version: 1
profiles:
  - name: union-allow-kernel
    find: {roots: [/tmp], paths: ["union-a*", "union-shared*"], type: dir}
    checkouts: allow
    action: delete
    examples: {match: [/tmp/union-a]}
  - name: union-refuse-kernel
    find: {roots: [/tmp], paths: ["union-b*", "union-shared*"], type: dir}
    action: delete
    examples: {match: [/tmp/union-b]}
  - name: union-has-tag
    where: {has: CACHEDIR.TAG}
"#;

const FS_UNION_ALIAS_TEST_PROFILE: &str = r#"version: 1
profiles:
  - name: alias-cache-allow-kernel
    find: {roots: [/tmp/alias-case], paths: [cache-file], type: file}
    checkouts: allow
    action: delete
    examples: {match: [/tmp/alias-case/cache-file]}
  - name: alias-marker-refuse-kernel
    find: {roots: [/tmp/alias-case], paths: [.git], type: file}
    action: delete
    examples: {match: [/tmp/alias-case/.git]}
  - name: mixed-root-allow-kernel
    find: {roots: [/tmp], paths: [mixed-case/.git/cache-file], type: file}
    checkouts: allow
    action: delete
    examples: {match: [/tmp/mixed-case/.git/cache-file]}
  - name: mixed-root-refuse-kernel
    find: {roots: [/tmp/mixed-case/.git], paths: [cache-file], type: file}
    action: delete
    examples: {match: [/tmp/mixed-case/.git/cache-file]}
"#;

const FS_UNION_RETENTION_TEST_PROFILE: &str = r#"version: 1
profiles:
  - name: union-retention-delete-open
    find: {roots: [/tmp], paths: [union-retention-delete/releases/*], type: dir}
    action: delete
    examples: {match: [/tmp/union-retention-delete/releases/v1]}
  - name: union-retention-delete-guard
    find: {roots: [/tmp], paths: [union-retention-delete/releases/*], type: dir}
    where: {has: CACHEDIR.TAG, protect_parent_link: current}
    action: delete
    examples: {match: [/tmp/union-retention-delete/releases/v1]}
  - name: union-retention-command-open
    find: {roots: [/tmp], paths: [union-retention-command/releases/*], type: dir}
    action: {command: [/home/tester/union-retention-cleaner, "{path}"]}
    examples: {match: [/tmp/union-retention-command/releases/v1]}
  - name: union-retention-command-guard
    find: {roots: [/tmp], paths: [union-retention-command/releases/*], type: dir}
    where: {has: CACHEDIR.TAG, protect_parent_link: current}
    action: {command: [/home/tester/union-retention-cleaner, "{path}"]}
    examples: {match: [/tmp/union-retention-command/releases/v1]}
"#;

const EXCLUDED_CACHE_TEST_PROFILE: &str = r#"version: 1
profiles:
  - name: cache-exclusion-test
    find: {roots: [/tmp], paths: ["huggingface", "nix-index"], exclude: ["huggingface"], type: dir}
    action: delete
    examples: {match: [/tmp/nix-index], no_match: [/tmp/huggingface]}
"#;

const NEGATED_PROFILE_TEST_PROFILE: &str = r#"version: 1
profiles:
  - name: neg-profile-source
    find: {roots: [/tmp], paths: [neg-profile-parent, neg-profile-free], type: dir}
    action: delete
    examples: {match: [/tmp/neg-profile-parent, /tmp/neg-profile-free]}
  - name: neg-profile-child
    find: {roots: [/tmp], paths: [neg-profile-parent/child], type: dir}
    where: {has: deliberately-absent}
    action: delete
    examples: {match: [/tmp/neg-profile-parent/child]}
"#;

/// The kernel discovery and dry-run planner use raw negated matches and
/// component-aware overlap. This deliberately makes no reclaiming call.
pub fn fs_negated_profile_dry_run_excludes_ancestor(ctx: &Ctx) -> TestResult {
    ctx.home.write_file(
        "diskgc.yaml",
        NEGATED_PROFILE_TEST_PROFILE.as_bytes(),
        0o644,
        TESTER,
    )?;
    ctx.scratch.mkdir("neg-profile-parent", 0o700, TESTER)?;
    ctx.scratch
        .mkdir("neg-profile-parent/child", 0o700, TESTER)?;
    ctx.scratch.mkdir("neg-profile-free", 0o700, TESTER)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let run = ensure_tmp(
        ctx,
        &[
            "--dry-run",
            "--min-free",
            "100%",
            "--select",
            "profile:neg-profile-source && !profile:neg-profile-child",
        ],
    )?;
    let verdict = oracle.judge(&run)?;
    run.expect_code(0)?;
    let planned = report(&run)?;
    let entries = planned["plan"].as_array().ok_or("plan is not an array")?;
    let reason = |path: &str| {
        entries
            .iter()
            .find(|entry| entry["candidate"].as_str() == Some(path))
            .map(|entry| &entry["reason"])
    };
    ensure!(
        reason("/tmp/neg-profile-parent") == Some(&Value::String("excluded".to_owned())),
        "ancestor was not excluded: {planned}"
    );
    ensure!(
        reason("/tmp/neg-profile-free") == Some(&Value::Null),
        "free sibling was excluded: {planned}"
    );
    verdict.assert_clean()
}

const CMAKE_GUARD_TEST_PROFILE: &str = r#"version: 1
profiles:
  - name: cmake-guard-kernel
    find: {roots: [/tmp], paths: ["cmake-*-build"], type: dir}
    where: {has: CMakeCache.txt, lacks_entry: CMakeLists.txt}
    action: delete
    examples: {match: [/tmp/cmake-free-build]}
"#;

/// The built-in's three-day floor cannot be reached by newly created
/// tmpfs fixtures (ctime is current). This exercises its exact anchor
/// predicate and checkout guard with no floor; the built-in dry run below
/// separately confirms the floor on the same real tree.
const CMAKE_ANCHOR_KERNEL_PROFILE: &str = r#"version: 1
profiles:
  - name: cmake-anchor-kernel
    find: {roots: required, paths: ["*", "*/*"], type: dir}
    where: {has: CMakeCache.txt, has_dir: CMakeFiles, lacks_entry: CMakeLists.txt, source_anchor: cmake}
    action: delete
    examples: {root: /tmp, match: [/tmp/cmake-app/odd-output]}
"#;

/// The real LinuxHost deletes an anchored out-of-source output while a
/// missing build file, in-source tree and nested checkout survive.
pub fn fs_reclaim_anchored_cmake_keeps_unrebuildable_outputs(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    ctx.home.write_file(
        "diskgc.yaml",
        CMAKE_ANCHOR_KERNEL_PROFILE.as_bytes(),
        0o644,
        TESTER,
    )?;
    for rel in [
        "cmake-app/odd-output/CMakeFiles",
        "cmake-missing/odd-output/CMakeFiles",
        "cmake-insource/CMakeFiles",
        "cmake-checkout/odd-output/CMakeFiles",
        "cmake-checkout/odd-output/.git",
    ] {
        s.mkdirs(rel, 0o700, TESTER)?;
    }
    s.write_file("cmake-app/CMakeLists.txt", b"project(app)\n", 0o600, TESTER)?;
    s.write_file(
        "cmake-insource/CMakeLists.txt",
        b"project(in)\n",
        0o600,
        TESTER,
    )?;
    s.write_file(
        "cmake-checkout/CMakeLists.txt",
        b"project(checkout)\n",
        0o600,
        TESTER,
    )?;
    for (rel, source) in [
        ("cmake-app/odd-output", "/tmp/cmake-app"),
        ("cmake-missing/odd-output", "/tmp/cmake-missing"),
        ("cmake-insource", "/tmp/cmake-insource"),
        ("cmake-checkout/odd-output", "/tmp/cmake-checkout"),
    ] {
        s.write_file(
            &format!("{rel}/CMakeCache.txt"),
            format!("CMAKE_HOME_DIRECTORY:INTERNAL={source}\n").as_bytes(),
            0o600,
            TESTER,
        )?;
    }
    s.write_file(
        "cmake-app/odd-output/object.o",
        b"rebuildable",
        0o600,
        TESTER,
    )?;
    s.write_file("cmake-missing/odd-output/precious", b"keep", 0o600, TESTER)?;
    s.write_file("cmake-insource/source.c", b"keep", 0o600, TESTER)?;
    s.write_file(
        "cmake-checkout/odd-output/.git/HEAD",
        b"keep",
        0o600,
        TESTER,
    )?;

    let eligible = s.path("cmake-app/odd-output");
    let missing = s.path("cmake-missing/odd-output");
    let insource = s.path("cmake-insource");
    let checkout = s.path("cmake-checkout/odd-output");
    let oracle = Oracle::arm(Fixture::new().eligible(eligible.clone()))?;
    let plan = ensure_run(
        ctx,
        &[
            "--json",
            "--dry-run",
            "--no-index",
            "--min-free",
            "100%",
            "--select",
            "profile:cmake-build-anchored",
            "--roots",
            "/tmp",
            "/tmp",
        ],
    )?;
    plan.expect_code(0)?;
    let built_in = report(&plan)?;
    ensure!(
        built_in["plan"]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|entry| {
                entry["candidate"].as_str() == eligible.to_str()
                    && entry["reason"].as_str() == Some("not_idle")
                    && entry["build_evidence"]["source"].as_str() == Some("/tmp/cmake-app")
            })),
        "built-in anchor or idle floor was not observed: {built_in}"
    );

    let run = ensure_tmp(
        ctx,
        &[
            "--no-index",
            "--min-free",
            "100%",
            "--select",
            "profile:cmake-anchor-kernel",
            "--roots",
            "/tmp",
        ],
    )?;
    let verdict = oracle.judge_runs(&[&plan, &run])?;
    run.expect_code(1)?;
    let (skipped, actions) = outcome(&report(&run)?);
    ensure!(actions == [eligible.display().to_string()], "{actions:?}");
    ensure!(
        skipped
            .get(&checkout.display().to_string())
            .map(String::as_str)
            == Some("contains_checkout"),
        "checkout was not refused: {skipped:?}"
    );
    ensure!(!eligible.exists(), "eligible CMake output remains");
    ensure!(
        missing.join("precious").exists(),
        "missing-source output was removed"
    );
    ensure!(
        insource.join("source.c").exists(),
        "in-source tree was removed"
    );
    ensure!(checkout.join(".git/HEAD").exists(), "checkout was removed");
    verdict.assert_clean()
}

/// Linux must report even a dangling CMakeLists.txt symlink as present.
/// A following stat would miss it and permit deletion of this source tree.
pub fn fs_reclaim_cmake_guard_keeps_a_dangling_source_symlink(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    ctx.home.write_file(
        "diskgc.yaml",
        CMAKE_GUARD_TEST_PROFILE.as_bytes(),
        0o644,
        TESTER,
    )?;
    let protected = s.mkdir("cmake-linked-build", 0o700, TESTER)?;
    s.write_file("cmake-linked-build/CMakeCache.txt", b"cache", 0o600, TESTER)?;
    s.symlink(
        "cmake-linked-build/CMakeLists.txt",
        Path::new("/tmp/no-such-source-file"),
    )?;
    let free = s.mkdir("cmake-free-build", 0o700, TESTER)?;
    s.write_file("cmake-free-build/CMakeCache.txt", b"cache", 0o600, TESTER)?;
    let oracle = Oracle::arm(Fixture::new().eligible(free.clone()))?;
    let run = ensure_tmp(
        ctx,
        &[
            "--min-free",
            "100%",
            "--select",
            "profile:cmake-guard-kernel",
        ],
    )?;
    let verdict = oracle.judge_runs(&[&run])?;
    run.expect_code(1)?;
    let (_, actions) = outcome(&report(&run)?);
    ensure!(actions == [free.display().to_string()], "{actions:?}");
    ensure!(
        exists(&protected.join("CMakeCache.txt")),
        "source was deleted"
    );
    ensure!(!exists(&free), "free build was retained");
    verdict.assert_clean()
}

/// The kernel discovery path must leave a named cache holding user data
/// untouched while reclaiming its eligible sibling.
pub fn fs_reclaim_exact_exclusion_keeps_valuable_cache(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    ctx.home.write_file(
        "diskgc.yaml",
        EXCLUDED_CACHE_TEST_PROFILE.as_bytes(),
        0o644,
        TESTER,
    )?;
    s.mkdir("huggingface", 0o700, TESTER)?;
    s.write_file("huggingface/model", b"valuable", 0o600, TESTER)?;
    let free = s.mkdir("nix-index", 0o700, TESTER)?;
    s.write_file("nix-index/index", b"rebuildable", 0o600, TESTER)?;
    let oracle = Oracle::arm(Fixture::new().eligible(free.clone()))?;
    let run = ensure_tmp(
        ctx,
        &[
            "--min-free",
            "100%",
            "--select",
            "profile:cache-exclusion-test",
        ],
    )?;
    let verdict = oracle.judge_runs(&[&run])?;
    run.expect_code(1)?;
    let (skipped, actions) = outcome(&report(&run)?);
    ensure!(actions == [free.display().to_string()], "{actions:?}");
    ensure!(
        !skipped.contains_key("/tmp/huggingface"),
        "excluded entry was judged: {skipped:?}"
    );
    ensure!(
        exists(&s.path("huggingface/model")),
        "valuable data was removed"
    );
    ensure!(!exists(&free), "eligible cache was kept");
    verdict.assert_clean()
}

/// A held Buildah-shaped file is `in_use`, a new entry cannot bypass a
/// source's mandatory idle floor, and an old free image spool is removed.
/// The oracle checks that the real binary unlinks only the latter.
pub fn fs_reclaim_image_temp_minimum_idle_and_open_file(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    ctx.home.write_file(
        "diskgc.yaml",
        IMAGE_TEMP_TEST_PROFILE.as_bytes(),
        0o644,
        TESTER,
    )?;
    s.mkdir("buildah-held", 0o700, TESTER)?;
    s.write_file("buildah-held/image", b"held", 0o600, TESTER)?;
    let free = s.write_file("container_images_storage-free", b"free", 0o600, TESTER)?;
    let held_path = s.path("buildah-held/image");
    let holder = Holder::tester(ctx, &["hold", held_path.to_str().ok_or("path")?, "/"], &[])?;
    std::thread::sleep(Duration::from_secs(11));
    s.mkdir("buildah-fresh", 0o700, TESTER)?;
    let fresh_since = Instant::now();
    let oracle = Oracle::arm(Fixture::new().eligible(free.clone()))?;
    let args = ["--min-free", "100%", "--select", "profile:image-temp-test"];
    let plan = ensure_tmp(ctx, &["--dry-run", args[0], args[1], args[2], args[3]])?;
    let run = ensure_tmp(ctx, &args)?;
    let verdict = oracle.judge_runs(&[&plan, &run])?;
    holder.finish()?;
    ensure!(
        fresh_since.elapsed() < Duration::from_secs(10),
        "inconclusive: the fresh spool aged past the 10s minimum while judging"
    );
    plan.expect_code(0)?;
    run.expect_code(1)?;
    let planned = report(&plan)?;
    let plan_reasons: BTreeMap<String, Option<String>> = planned["plan"]
        .as_array()
        .ok_or("plan is not an array")?
        .iter()
        .map(|entry| {
            (
                entry["candidate"].as_str().unwrap_or_default().to_owned(),
                entry["reason"].as_str().map(str::to_owned),
            )
        })
        .collect();
    ensure!(
        plan_reasons.get("/tmp/buildah-held") == Some(&Some("in_use".to_owned())),
        "held image spool: {plan_reasons:?}"
    );
    ensure!(
        plan_reasons.get("/tmp/buildah-fresh") == Some(&Some("not_idle".to_owned())),
        "fresh image spool: {plan_reasons:?}"
    );
    ensure!(
        plan_reasons.get("/tmp/container_images_storage-free") == Some(&None),
        "free image spool: {plan_reasons:?}"
    );
    let (skipped, actions) = outcome(&report(&run)?);
    ensure!(actions == [free.display().to_string()], "{actions:?}");
    ensure!(
        skipped.get("/tmp/buildah-held").map(String::as_str) == Some("in_use"),
        "held image spool: {skipped:?}"
    );
    ensure!(
        skipped.get("/tmp/buildah-fresh").map(String::as_str) == Some("not_idle"),
        "fresh image spool: {skipped:?}"
    );
    ensure!(
        exists(&s.path("buildah-held/image")),
        "held file was deleted"
    );
    ensure!(exists(&s.path("buildah-fresh")), "fresh spool was deleted");
    verdict.assert_clean()
}

/// An idle complement excludes the old sibling in the real Linux path;
/// only the recent candidate reaches deletion, as checked by the oracle.
pub fn fs_reclaim_negated_idle_filter_keeps_old_candidate(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    ctx.home.write_file(
        "diskgc.yaml",
        IDLE_WINDOW_TEST_PROFILE.as_bytes(),
        0o644,
        TESTER,
    )?;
    let old = s.mkdir("idle-window-old", 0o700, TESTER)?;
    s.write_file("idle-window-old/payload", b"keep", 0o600, TESTER)?;
    std::thread::sleep(Duration::from_secs(11));
    let recent = s.mkdir("idle-window-recent", 0o700, TESTER)?;
    s.write_file("idle-window-recent/payload", b"delete", 0o600, TESTER)?;
    let recent_since = Instant::now();
    let oracle = Oracle::arm(Fixture::new().eligible(recent.clone()))?;
    let expr = "profile:idle-window-kernel && !filter:idle-window-test";
    let plan = ensure_tmp(ctx, &["--dry-run", "--min-free", "100%", "--select", expr])?;
    let run = ensure_tmp(ctx, &["--min-free", "100%", "--select", expr])?;
    let verdict = oracle.judge_runs(&[&plan, &run])?;
    ensure!(
        recent_since.elapsed() < Duration::from_secs(10),
        "inconclusive: the recent candidate aged past the 10s upper bound"
    );
    plan.expect_code(0)?;
    run.expect_code(1)?;
    let planned = report(&plan)?;
    let reasons: BTreeMap<String, Option<String>> = planned["plan"]
        .as_array()
        .ok_or("plan is not an array")?
        .iter()
        .map(|entry| {
            (
                entry["candidate"].as_str().unwrap_or_default().to_owned(),
                entry["reason"].as_str().map(str::to_owned),
            )
        })
        .collect();
    ensure!(
        reasons.get(old.to_str().ok_or("old path")?) == Some(&Some("too_idle".to_owned())),
        "old candidate was not excluded: {reasons:?}"
    );
    ensure!(
        reasons.get(recent.to_str().ok_or("recent path")?) == Some(&None),
        "recent candidate was not eligible: {reasons:?}"
    );
    let (skipped, actions) = outcome(&report(&run)?);
    ensure!(actions == [recent.display().to_string()], "{actions:?}");
    ensure!(
        skipped
            .get(old.to_str().ok_or("old path")?)
            .map(String::as_str)
            == Some("too_idle"),
        "old candidate was not skipped: {skipped:?}"
    );
    ensure!(old.join("payload").exists(), "old payload was deleted");
    ensure!(!recent.exists(), "recent candidate remains");
    verdict.assert_clean()
}

/// Both entry complements use the shipped binary and the scratch tmpfs.
/// The deletion oracle permits only the candidate selected by each run.
pub fn fs_reclaim_negated_entry_filters(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    ctx.home.write_file(
        "diskgc.yaml",
        ENTRY_NEGATION_TEST_PROFILE.as_bytes(),
        0o644,
        TESTER,
    )?;
    let plain = s.mkdir("entry-negation-plain", 0o700, TESTER)?;
    s.write_file("entry-negation-plain/payload", b"delete", 0o600, TESTER)?;
    let tagged = s.mkdir("entry-negation-tagged", 0o700, TESTER)?;
    s.write_file("entry-negation-tagged/CACHEDIR.TAG", b"tag", 0o600, TESTER)?;
    let oracle = Oracle::arm(
        Fixture::new()
            .eligible(plain.clone())
            .eligible(tagged.clone()),
    )?;
    let no_has = "profile:entry-negation-kernel && !filter:entry-has-tag";
    let no_has_not = "profile:entry-negation-kernel && !filter:entry-has-not-tag";
    let first = ensure_tmp(ctx, &["--min-free", "100%", "--select", no_has])?;
    let second = ensure_tmp(ctx, &["--min-free", "100%", "--select", no_has_not])?;
    let verdict = oracle.judge_runs(&[&first, &second])?;
    first.expect_code(1)?;
    second.expect_code(0)?;
    let (first_skips, first_actions) = outcome(&report(&first)?);
    ensure!(
        first_actions == [plain.display().to_string()],
        "{first_actions:?}"
    );
    ensure!(
        first_skips
            .get(tagged.to_str().ok_or("tagged path")?)
            .map(String::as_str)
            == Some("filtered"),
        "{first_skips:?}"
    );
    let (_, second_actions) = outcome(&report(&second)?);
    ensure!(
        second_actions == [tagged.display().to_string()],
        "{second_actions:?}"
    );
    ensure!(
        !plain.exists() && !tagged.exists(),
        "selected candidates remain"
    );
    verdict.assert_clean()
}

/// Positive `has` and `has_not` select opposite real directories. The
/// independent deletion oracle permits only those two selected candidates.
pub fn fs_reclaim_positive_entry_filters(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    ctx.home.write_file(
        "diskgc.yaml",
        ENTRY_NEGATION_TEST_PROFILE.as_bytes(),
        0o644,
        TESTER,
    )?;
    let plain = s.mkdir("entry-negation-plain", 0o700, TESTER)?;
    s.write_file("entry-negation-plain/payload", b"delete", 0o600, TESTER)?;
    let tagged = s.mkdir("entry-negation-tagged", 0o700, TESTER)?;
    s.write_file("entry-negation-tagged/CACHEDIR.TAG", b"tag", 0o600, TESTER)?;
    let oracle = Oracle::arm(
        Fixture::new()
            .eligible(plain.clone())
            .eligible(tagged.clone()),
    )?;
    let has = "profile:entry-negation-kernel && filter:entry-has-tag";
    let has_not = "profile:entry-negation-kernel && filter:entry-has-not-tag";
    let first = ensure_tmp(ctx, &["--min-free", "100%", "--select", has])?;
    let second = ensure_tmp(ctx, &["--min-free", "100%", "--select", has_not])?;
    let verdict = oracle.judge_runs(&[&first, &second])?;
    first.expect_code(1)?;
    second.expect_code(0)?;
    let (first_skips, first_actions) = outcome(&report(&first)?);
    ensure!(
        first_actions == [tagged.display().to_string()],
        "{first_actions:?}"
    );
    ensure!(
        first_skips
            .get(plain.to_str().ok_or("plain path")?)
            .map(String::as_str)
            == Some("filtered"),
        "{first_skips:?}"
    );
    let (_, second_actions) = outcome(&report(&second)?);
    ensure!(
        second_actions == [plain.display().to_string()],
        "{second_actions:?}"
    );
    ensure!(
        !plain.exists() && !tagged.exists(),
        "selected candidates remain"
    );
    verdict.assert_clean()
}

/// A real positive mixed filter must require both entry predicates and
/// the idle bound. The oracle independently permits only the admitted dir.
pub fn fs_reclaim_mixed_entry_and_idle_filter(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    ctx.home.write_file(
        "diskgc.yaml",
        MIXED_ENTRY_FILTER_TEST_PROFILE.as_bytes(),
        0o644,
        TESTER,
    )?;
    let good = s.mkdir("entry-mixed-good", 0o700, TESTER)?;
    s.write_file("entry-mixed-good/CACHEDIR.TAG", b"tag", 0o600, TESTER)?;
    s.write_file("entry-mixed-good/payload", b"delete", 0o600, TESTER)?;
    let untagged = s.mkdir("entry-mixed-untagged", 0o700, TESTER)?;
    s.write_file("entry-mixed-untagged/payload", b"keep", 0o600, TESTER)?;
    let marked = s.mkdir("entry-mixed-marked", 0o700, TESTER)?;
    s.write_file("entry-mixed-marked/CACHEDIR.TAG", b"tag", 0o600, TESTER)?;
    s.write_file("entry-mixed-marked/DO_NOT_DELETE", b"keep", 0o600, TESTER)?;
    std::thread::sleep(MIXED_FILTER_AGE_SETTLE);
    let recent = s.mkdir("entry-mixed-recent", 0o700, TESTER)?;
    s.write_file("entry-mixed-recent/CACHEDIR.TAG", b"tag", 0o600, TESTER)?;

    let oracle = Oracle::arm(Fixture::new().eligible(good.clone()))?;
    let selection = "profile:mixed-entry-kernel && filter:mixed-tagged-idle-unkept";
    let run = ensure_tmp(ctx, &["--min-free", "100%", "--select", selection])?;
    let verdict = oracle.judge_runs(&[&run])?;
    run.expect_code(1)?;
    let (skips, actions) = outcome(&report(&run)?);
    ensure!(actions == [good.display().to_string()], "{actions:?}");
    for (path, reason) in [
        (&untagged, "filtered"),
        (&marked, "filtered"),
        (&recent, "not_idle"),
    ] {
        ensure!(
            skips
                .get(path.to_str().ok_or("candidate path")?)
                .map(String::as_str)
                == Some(reason),
            "{}: {skips:?}",
            path.display()
        );
        ensure!(path.exists(), "{} was removed", path.display());
    }
    ensure!(!good.exists(), "admitted candidate remains");
    verdict.assert_clean()
}

/// The real filesystem complements the entire mixed conjunction. An old
/// tagged unmarked directory is excluded; each single failed predicate
/// admits the other directories. The trace oracle constrains every unlink.
pub fn fs_reclaim_negated_mixed_entry_and_idle_filter(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    ctx.home.write_file(
        "diskgc.yaml",
        MIXED_ENTRY_FILTER_TEST_PROFILE.as_bytes(),
        0o644,
        TESTER,
    )?;
    let matched = s.mkdir("entry-mixed-good", 0o700, TESTER)?;
    s.write_file("entry-mixed-good/CACHEDIR.TAG", b"tag", 0o600, TESTER)?;
    s.write_file("entry-mixed-good/payload", b"preserve", 0o600, TESTER)?;
    let untagged = s.mkdir("entry-mixed-untagged", 0o700, TESTER)?;
    s.write_file("entry-mixed-untagged/payload", b"delete", 0o600, TESTER)?;
    let marked = s.mkdir("entry-mixed-marked", 0o700, TESTER)?;
    s.write_file("entry-mixed-marked/CACHEDIR.TAG", b"tag", 0o600, TESTER)?;
    s.write_file("entry-mixed-marked/DO_NOT_DELETE", b"marker", 0o600, TESTER)?;
    std::thread::sleep(MIXED_FILTER_AGE_SETTLE);
    let recent = s.mkdir("entry-mixed-recent", 0o700, TESTER)?;
    s.write_file("entry-mixed-recent/CACHEDIR.TAG", b"tag", 0o600, TESTER)?;

    let oracle = Oracle::arm(
        Fixture::new()
            .eligible(untagged.clone())
            .eligible(marked.clone())
            .eligible(recent.clone()),
    )?;
    let selection = "profile:mixed-entry-kernel && !filter:mixed-tagged-idle-unkept";
    let run = ensure_tmp(ctx, &["--min-free", "100%", "--select", selection])?;
    let verdict = oracle.judge_runs(&[&run])?;
    run.expect_code(1)?;
    let (skips, mut actions) = outcome(&report(&run)?);
    actions.sort();
    let mut expected = [&untagged, &marked, &recent]
        .into_iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    expected.sort();
    ensure!(actions == expected, "{actions:?}");
    ensure!(
        skips
            .get(matched.to_str().ok_or("matched path")?)
            .map(String::as_str)
            == Some("filtered"),
        "{skips:?}"
    );
    ensure!(matched.exists(), "matched conjunction was deleted");
    ensure!(
        !untagged.exists() && !marked.exists() && !recent.exists(),
        "admitted candidate remains"
    );
    verdict.assert_clean()
}

/// A source excluded by its positive filter cannot impose its checkout
/// refusal on the source that still admits the candidate.
pub fn fs_union_checkout_policy_uses_admitted_terms(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    ctx.home.write_file(
        "diskgc.yaml",
        FS_UNION_TEST_PROFILE.as_bytes(),
        0o644,
        TESTER,
    )?;
    let clear = s.mkdir("union-shared-clear", 0o700, TESTER)?;
    s.mkdirs("union-shared-clear/.git", 0o700, TESTER)?;
    s.write_file("union-shared-clear/.git/HEAD", b"ref: x\n", 0o600, TESTER)?;
    let tagged = s.mkdir("union-shared-tagged", 0o700, TESTER)?;
    s.mkdirs("union-shared-tagged/.git", 0o700, TESTER)?;
    s.write_file("union-shared-tagged/.git/HEAD", b"ref: y\n", 0o600, TESTER)?;
    s.write_file("union-shared-tagged/CACHEDIR.TAG", b"tag", 0o600, TESTER)?;
    let oracle = Oracle::arm(Fixture::new().eligible(clear.clone()))?;
    let expression =
        "profile:union-allow-kernel || (profile:union-refuse-kernel && filter:union-has-tag)";
    let run = ensure_tmp(ctx, &["--min-free", "100%", "--select", expression])?;
    let verdict = oracle.judge_runs(&[&run])?;
    run.expect_code(1)?;
    let (skipped, actions) = outcome(&report(&run)?);
    ensure!(actions == [clear.display().to_string()], "{actions:?}");
    ensure!(
        skipped
            .get(tagged.to_str().ok_or("tagged path")?)
            .map(String::as_str)
            == Some("contains_checkout"),
        "{skipped:?}"
    );
    ensure!(
        !clear.exists() && tagged.exists(),
        "wrong checkout candidate removed"
    );
    verdict.assert_clean()
}

/// A hard-linked `.git` name must veto its allowing alias, while a source
/// rooted inside `.git` may still delete a file whose other admitted source
/// explicitly allows that metadata path. The oracle catches every unlink
/// outside the one eligible file independently of the reported outcome.
pub fn fs_union_checkout_aliases_and_mixed_roots(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    ctx.home.write_file(
        "diskgc.yaml",
        FS_UNION_ALIAS_TEST_PROFILE.as_bytes(),
        0o644,
        TESTER,
    )?;
    s.mkdir("alias-case", 0o700, TESTER)?;
    let cache_alias = s.write_file("alias-case/cache-file", b"linked", 0o600, TESTER)?;
    let marker_alias = s.hard_link("alias-case/cache-file", "alias-case/.git")?;
    s.mkdirs("mixed-case/.git", 0o700, TESTER)?;
    let selections = [
        "profile:alias-cache-allow-kernel || profile:alias-marker-refuse-kernel || profile:mixed-root-allow-kernel || profile:mixed-root-refuse-kernel",
        "profile:mixed-root-refuse-kernel || profile:mixed-root-allow-kernel || profile:alias-marker-refuse-kernel || profile:alias-cache-allow-kernel",
    ];
    for (order, expression) in selections.into_iter().enumerate() {
        let mixed = s.write_file(
            "mixed-case/.git/cache-file",
            format!("payload-{order}").as_bytes(),
            0o600,
            TESTER,
        )?;
        let oracle = Oracle::arm(Fixture::new().eligible(mixed.clone()))?;
        let plan = ensure_tmp(
            ctx,
            &["--dry-run", "--min-free", "100%", "--select", expression],
        )?;
        let run = ensure_tmp(ctx, &["--min-free", "100%", "--select", expression])?;
        let verdict = oracle.judge_runs(&[&plan, &run])?;
        plan.expect_code(0)?;
        run.expect_code(1)?;
        let planned = report(&plan)?;
        let entries = planned["plan"].as_array().ok_or("plan is not an array")?;
        ensure!(entries.len() == 2, "order {order}: {planned}");
        ensure!(
            entries.iter().any(|entry| {
                (entry["candidate"] == cache_alias.display().to_string()
                    || entry["candidate"] == marker_alias.display().to_string())
                    && entry["reason"] == "contains_checkout"
            }),
            "hardlink alias was not refused in plan, order {order}: {planned}"
        );
        ensure!(
            entries.iter().any(|entry| {
                entry["candidate"] == mixed.display().to_string() && entry["reason"].is_null()
            }),
            "mixed-root file was not eligible in plan, order {order}: {planned}"
        );
        let (skipped, actions) = outcome(&report(&run)?);
        ensure!(
            actions == [mixed.display().to_string()],
            "wrong actions in order {order}: {actions:?}"
        );
        ensure!(
            [cache_alias.as_path(), marker_alias.as_path()]
                .iter()
                .any(|path| path
                    .to_str()
                    .and_then(|text| skipped.get(text))
                    .map(String::as_str)
                    == Some("contains_checkout")),
            "hardlink alias was not refused in reclaim, order {order}: {skipped:?}"
        );
        ensure!(
            cache_alias.exists() && marker_alias.exists() && !mixed.exists(),
            "wrong file survived in order {order}"
        );
        verdict.assert_clean()?;
    }
    Ok(())
}

/// The real binary unions two user sources, removes an overlapping inode
/// once, and applies the stricter checkout setting only to shared matches.
pub fn fs_reclaim_positive_union_dedupes_and_refuses_shared_checkout(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    ctx.home.write_file(
        "diskgc.yaml",
        FS_UNION_TEST_PROFILE.as_bytes(),
        0o644,
        TESTER,
    )?;
    let only_allow = s.mkdir("union-a", 0o700, TESTER)?;
    s.mkdirs("union-a/.git", 0o700, TESTER)?;
    s.write_file("union-a/.git/HEAD", b"ref: x\n", 0o600, TESTER)?;
    let only_refuse = s.mkdir("union-b", 0o700, TESTER)?;
    s.write_file("union-b/payload", b"delete", 0o600, TESTER)?;
    let shared = s.mkdir("union-shared-clear", 0o700, TESTER)?;
    s.write_file("union-shared-clear/payload", b"delete", 0o600, TESTER)?;
    let protected = s.mkdir("union-shared-checkout", 0o700, TESTER)?;
    s.mkdirs("union-shared-checkout/.git", 0o700, TESTER)?;
    s.write_file(
        "union-shared-checkout/.git/HEAD",
        b"ref: x\n",
        0o600,
        TESTER,
    )?;
    let oracle = Oracle::arm(
        Fixture::new()
            .eligible(only_allow.clone())
            .eligible(only_refuse.clone())
            .eligible(shared.clone()),
    )?;
    let expr = "profile:union-allow-kernel || profile:union-refuse-kernel";
    let plan = ensure_tmp(ctx, &["--dry-run", "--min-free", "100%", "--select", expr])?;
    let run = ensure_tmp(ctx, &["--min-free", "100%", "--select", expr])?;
    let verdict = oracle.judge_runs(&[&plan, &run])?;
    plan.expect_code(0)?;
    run.expect_code(1)?;
    let planned = report(&plan)?;
    let shared_entries: Vec<_> = planned["plan"]
        .as_array()
        .ok_or("plan is not an array")?
        .iter()
        .filter(|entry| entry["candidate"] == shared.display().to_string())
        .collect();
    ensure!(
        shared_entries.len() == 1,
        "shared inode was planned more than once: {planned}"
    );
    ensure!(shared_entries[0]["checkouts"] == "refuse", "{planned}");
    let (skipped, mut actions) = outcome(&report(&run)?);
    actions.sort();
    let mut expected = [only_allow, only_refuse, shared]
        .into_iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    expected.sort();
    ensure!(
        actions == expected,
        "union deleted {actions:?}, expected {expected:?}"
    );
    ensure!(
        skipped
            .get(protected.to_str().ok_or("protected path")?)
            .map(String::as_str)
            == Some("contains_checkout"),
        "shared checkout was not refused: {skipped:?}"
    );
    ensure!(
        protected.join(".git/HEAD").exists(),
        "shared checkout was deleted"
    );
    verdict.assert_clean()
}

/// Both delete and command unions protect an installed release even when
/// source-owned `where.has` rejects that source for action admission.
pub fn fs_union_retention_guards_veto_shared_candidates(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    ctx.home.write_file(
        "diskgc.yaml",
        FS_UNION_RETENTION_TEST_PROFILE.as_bytes(),
        0o644,
        TESTER,
    )?;
    ctx.home.write_file(
        "union-retention-cleaner",
        b"#!/bin/sh\nset -eu\n/bin/rm -rf -- \"$DISKGC_PATH\"\n",
        0o755,
        TESTER,
    )?;
    for action in ["delete", "command"] {
        let root = format!("union-retention-{action}");
        let retained = s.mkdirs(&format!("{root}/releases/v1"), 0o700, TESTER)?;
        s.write_file(
            &format!("{root}/releases/v1/payload"),
            b"installed",
            0o600,
            TESTER,
        )?;
        s.symlink(&format!("{root}/current"), Path::new("releases/v1"))?;
        for guarded_first in [false, true] {
            let eligible = s.mkdir(&format!("{root}/releases/v2"), 0o700, TESTER)?;
            s.write_file(
                &format!("{root}/releases/v2/payload"),
                b"obsolete",
                0o600,
                TESTER,
            )?;
            let open = format!("profile:union-retention-{action}-open");
            let guarded = format!("profile:union-retention-{action}-guard");
            let selection = if guarded_first {
                format!("{guarded} || {open}")
            } else {
                format!("{open} || {guarded}")
            };
            let oracle = Oracle::arm(Fixture::new().eligible(eligible.clone()))?;
            let plan = ensure_tmp(
                ctx,
                &["--dry-run", "--min-free", "100%", "--select", &selection],
            )?;
            let run = ensure_tmp(ctx, &["--min-free", "100%", "--select", &selection])?;
            let verdict = oracle.judge_runs(&[&plan, &run])?;
            plan.expect_code(0)?;
            run.expect_code(1)?;
            let planned = report(&plan)?;
            ensure!(
                planned["plan"]
                    .as_array()
                    .is_some_and(|entries| entries.iter().any(|entry| {
                        entry["candidate"] == retained.display().to_string()
                            && entry["reason"] == "linked_current"
                    })),
                "{action}, guarded first {guarded_first}: {planned}"
            );
            let (skipped, actions) = outcome(&report(&run)?);
            ensure!(
                skipped
                    .get(retained.to_str().ok_or("retained path")?)
                    .map(String::as_str)
                    == Some("linked_current"),
                "{action}, guarded first {guarded_first}: {skipped:?}"
            );
            ensure!(
                actions == [eligible.display().to_string()],
                "{action}, guarded first {guarded_first}: {actions:?}"
            );
            ensure!(
                retained.join("payload").exists(),
                "installed version removed"
            );
            ensure!(!eligible.exists(), "obsolete version remains");
            verdict.assert_clean()?;
        }
    }
    Ok(())
}

/// A real command union uses one checked child per inode, carries the
/// selected source into its environment, and honors term-local exclusion,
/// protection, and the stricter admitted checkout policy.
pub fn fs_command_union_invokes_once_with_selected_source(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    ctx.home.write_file(
        "command-union-cleaner",
        b"#!/bin/sh\nset -eu\ntest \"$DISKGC_PATH\" = \"$1\"\nprintf '%s:%s\\n' \"$DISKGC_PROFILE\" \"$DISKGC_PATH\" >> /home/tester/command-union.trace\n/bin/rm -rf -- \"$DISKGC_PATH\"\n",
        0o755,
        TESTER,
    )?;
    let config = r#"version: 1
profiles:
  - name: command-union-allow
    find: {roots: [/tmp], paths: ["command-union-a*", "command-union-shared*", "command-union-protected"], type: dir}
    checkouts: allow
    action: {command: [/home/tester/command-union-cleaner, "{path}"]}
    examples: {match: [/tmp/command-union-a]}
  - name: command-union-refuse
    find: {roots: [/tmp], paths: ["command-union-b*", "command-union-shared*", "command-union-protected"], type: dir}
    action: {command: [/home/tester/command-union-cleaner, "{path}"]}
    examples: {match: [/tmp/command-union-b]}
  - name: command-union-exclude
    find: {roots: [/tmp], paths: [command-union-shared-clear], type: dir}
    action: delete
    examples: {match: [/tmp/command-union-shared-clear]}
  - name: command-union-protect
    find: {roots: [/tmp], paths: [command-union-protected], type: dir}
    action: delete
    examples: {match: [/tmp/command-union-protected]}
protect: [command-union-protect]
"#;
    ctx.home
        .write_file("diskgc.yaml", config.as_bytes(), 0o644, TESTER)?;
    let only_allow = s.mkdir("command-union-a", 0o700, TESTER)?;
    s.mkdirs("command-union-a/.git", 0o700, TESTER)?;
    s.write_file("command-union-a/.git/HEAD", b"ref: x\n", 0o600, TESTER)?;
    let only_refuse = s.mkdir("command-union-b", 0o700, TESTER)?;
    s.write_file("command-union-b/payload", b"delete", 0o600, TESTER)?;
    let shared = s.mkdir("command-union-shared-clear", 0o700, TESTER)?;
    s.write_file(
        "command-union-shared-clear/payload",
        b"delete",
        0o600,
        TESTER,
    )?;
    let checkout = s.mkdir("command-union-shared-checkout", 0o700, TESTER)?;
    s.mkdirs("command-union-shared-checkout/.git", 0o700, TESTER)?;
    s.write_file(
        "command-union-shared-checkout/.git/HEAD",
        b"ref: y\n",
        0o600,
        TESTER,
    )?;
    let protected = s.mkdir("command-union-protected", 0o700, TESTER)?;
    s.write_file("command-union-protected/payload", b"keep", 0o600, TESTER)?;
    let expr = "(profile:command-union-allow && !profile:command-union-exclude) || profile:command-union-refuse";
    let oracle = Oracle::arm(
        Fixture::new()
            .eligible(only_allow.clone())
            .eligible(only_refuse.clone())
            .eligible(shared.clone()),
    )?;
    let plan = ensure_tmp(ctx, &["--dry-run", "--min-free", "100%", "--select", expr])?;
    let run = ensure_tmp(ctx, &["--min-free", "100%", "--select", expr])?;
    let judged = oracle.judge_runs(&[&plan, &run])?;
    plan.expect_code(0)?;
    run.expect_code(1)?;
    let planned = report(&plan)?;
    let entries = planned["plan"].as_array().ok_or("plan is not an array")?;
    ensure!(
        entries.len() == 5,
        "command union planned unexpected candidates: {planned}"
    );
    for (candidate, reason) in [
        (&only_allow, None),
        (&only_refuse, None),
        (&shared, None),
        (&checkout, Some("contains_checkout")),
        (&protected, Some("protected")),
    ] {
        let entry = entries
            .iter()
            .find(|entry| entry["candidate"] == candidate.to_string_lossy().as_ref())
            .ok_or_else(|| format!("plan omitted {}: {planned}", candidate.display()))?;
        ensure!(
            entry["reason"].as_str() == reason,
            "{}: expected {reason:?}: {planned}",
            candidate.display()
        );
    }
    ensure!(
        entries
            .iter()
            .filter(|entry| entry["candidate"] == shared.to_string_lossy().as_ref())
            .count()
            == 1,
        "command union planned shared inode more than once: {planned}"
    );
    let actual = report(&run)?;
    let (skipped, mut actions) = outcome(&actual);
    actions.sort();
    let mut expected = [only_allow.clone(), only_refuse.clone(), shared.clone()]
        .into_iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    expected.sort();
    ensure!(
        actions == expected,
        "command union actions {actions:?}, expected {expected:?}: {actual}"
    );
    ensure!(
        skipped
            .get(checkout.to_str().ok_or("checkout path")?)
            .map(String::as_str)
            == Some("contains_checkout"),
        "checkout not refused: {actual}"
    );
    ensure!(
        skipped
            .get(protected.to_str().ok_or("protected path")?)
            .map(String::as_str)
            == Some("protected"),
        "protection not enforced: {actual}"
    );
    let trace = std::fs::read_to_string("/home/tester/command-union.trace")
        .map_err(|error| format!("read command trace: {error}"))?;
    let mut invocations = trace.lines().collect::<Vec<_>>();
    invocations.sort();
    let mut expected_invocations = vec![
        format!("command-union-allow:{}", only_allow.display()),
        format!("command-union-refuse:{}", only_refuse.display()),
        format!("command-union-refuse:{}", shared.display()),
    ];
    expected_invocations.sort();
    ensure!(
        invocations
            == expected_invocations
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        "command invocations {invocations:?}, expected {expected_invocations:?}"
    );
    ensure!(
        checkout.join(".git/HEAD").exists() && protected.join("payload").exists(),
        "retained candidate changed"
    );
    judged.assert_clean()
}

/// The user profiles of these tests (docs/reference.md, Profiles), and a
/// filter `idle-test` whose `idle_for` the test chooses (`{IDLE}`): what a
/// fixture made before the run is old enough for, and what it makes right
/// before the run is not.
const PROFILES: &str = r#"version: 1
profiles:
  - name: pytest-dirs
    category: test
    find:
      roots: [/tmp]
      paths: ["pytest-of-*/pytest-*/*", "*/pytest-of-*/pytest-*/*"]
      type: dir
    action: delete
    examples: {match: [/tmp/pytest-of-tester/pytest-0/test_a0]}
  - name: rc-dirs
    category: nix
    find: {roots: [/tmp], paths: ["nix-shell-*-*", "nix-develop-*-*"], type: dir}
    action: delete
    examples: {match: [/tmp/nix-shell-1-2]}
  - name: rc-like
    category: test
    find: {roots: [/tmp], paths: ["rc-*", "pipe-*", "sock-*"], type: any}
    action: delete
    examples: {match: [/tmp/rc-1]}
  - name: tmp-any
    category: test
    find: {roots: [/tmp], paths: ["*"], type: any}
    action: delete
    examples: {match: [/tmp/x]}
  - name: idle-test
    where: {idle_for: {IDLE}}
"#;

/// Writes the configuration with `idle-test` at `idle`.
fn write_config(ctx: &Ctx, idle: Duration) -> TestResult {
    let text = PROFILES.replace("{IDLE}", &format!("{}s", idle.as_secs()));
    ctx.home
        .write_file("diskgc.yaml", text.as_bytes(), 0o644, TESTER)?;
    Ok(())
}

/// `diskgc ensure --json --config CONFIG ARGS... /tmp` as tester under
/// strace; logs only the head of a long output.
fn ensure_tmp(ctx: &Ctx, args: &[&str]) -> Result<Run, String> {
    let mut full = vec!["ensure", "--json", "--config", CONFIG];
    full.extend_from_slice(args);
    full.push("/tmp");
    let started = Instant::now();
    let run = ctx.run_as_tester(&ctx.diskgc(), &full)?;
    let head = |text: &str| text.lines().take(40).collect::<Vec<_>>().join("\n");
    info!(
        "diskgc {} -> {:?} in {:?}\n--- stderr (head)\n{}",
        full.join(" "),
        run.code,
        started.elapsed(),
        head(&run.stderr)
    );
    Ok(run)
}

/// The reason each candidate was skipped with, and the actions' candidates.
fn outcome(report: &Value) -> (BTreeMap<String, String>, Vec<String>) {
    let skipped = report["skipped"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .map(|e| {
                    (
                        e["candidate"].as_str().unwrap_or_default().to_owned(),
                        e["reason"].as_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let actions = report["actions"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .map(|e| e["candidate"].as_str().unwrap_or_default().to_owned())
                .collect()
        })
        .unwrap_or_default();
    (skipped, actions)
}

/// How many candidates a report says it reclaimed, summing each action
/// row's `count`. The report keeps at most 1,000 detail rows (one candidate
/// each, every one in `eligible` and none twice) and folds the rest into a
/// single last row with `null` tier, profile and candidate
/// (docs/reference.md, Output); any other shape is an error.
fn reclaimed_count(report: &Value, eligible: &BTreeSet<String>) -> Result<u64, String> {
    const DETAIL_LIMIT: usize = 1_000;
    let rows = report["actions"].as_array().ok_or("no actions array")?;
    let mut seen = BTreeSet::new();
    let mut total: u64 = 0;
    for (index, row) in rows.iter().enumerate() {
        let count = row["count"]
            .as_u64()
            .ok_or_else(|| format!("action row {index} has no count: {row}"))?;
        match row["candidate"].as_str() {
            Some(candidate) => {
                ensure!(count == 1, "detail row {index} counts {count}: {row}");
                ensure!(
                    index < DETAIL_LIMIT,
                    "detail row {index} past the limit: {row}"
                );
                ensure!(eligible.contains(candidate), "{candidate} is not eligible");
                ensure!(seen.insert(candidate), "{candidate} reclaimed twice");
            }
            None => {
                ensure!(
                    row["candidate"].is_null() && row["tier"].is_null() && row["profile"].is_null(),
                    "malformed summary row {index}: {row}"
                );
                ensure!(
                    index == DETAIL_LIMIT && index + 1 == rows.len(),
                    "summary row at {index} of {} rows, expected last after {DETAIL_LIMIT}",
                    rows.len()
                );
                ensure!(count >= 1, "empty summary row: {row}");
            }
        }
        total += count;
    }
    Ok(total)
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// How big the `/tmp` fixture is.
#[derive(Debug, Clone, Copy)]
struct Scale {
    /// pytest sessions directly in `/tmp/pytest-of-tester`, and test dirs
    /// per session.
    sessions: usize,
    tests: usize,
    /// nix-shell TMPDIRs, each holding one pytest session of `tests` dirs.
    shells: usize,
    /// rc dirs (`nix-shell-*-*`, `nix-develop-*-*`), two files each.
    rc_dirs: usize,
    /// Top-level files no profile matches (the bulk of a real `/tmp`).
    filler: usize,
    /// The idle filter: longer than the run takes, shorter than the time
    /// between building the fixture and the run.
    idle: Duration,
}

impl Scale {
    /// Entries the fixture holds in `/tmp` (about).
    fn entries(&self) -> usize {
        let test_dirs = (self.sessions + self.shells) * self.tests;
        test_dirs * 3 + self.rc_dirs * 3 + self.filler
    }
}

/// The commit gate's size: about 12,000 entries, a run of a few seconds.
const FAST: Scale = Scale {
    sessions: 20,
    tests: 40,
    shells: 5,
    rc_dirs: 1000,
    filler: 6000,
    idle: Duration::from_secs(30),
};

/// The PRD's 100,000-entry `/tmp`.
const FULL: Scale = Scale {
    sessions: 50,
    tests: 100,
    shells: 10,
    rc_dirs: 10_000,
    filler: 52_001,
    idle: Duration::from_secs(120),
};

/// AC #1 at the commit gate's size (see [`tmp_fixture`]).
pub fn fs_reclaim_tmp_fixture_deletes_every_eligible_candidate_and_nothing_else(
    ctx: &Ctx,
) -> TestResult {
    tmp_fixture(ctx, FAST)
}

/// AC #1 on the PRD's 100,000-entry `/tmp` (the full tier).
pub fn fs_reclaim_100k_entry_tmp_deletes_every_eligible_candidate_and_nothing_else(
    ctx: &Ctx,
) -> TestResult {
    ensure!(
        FULL.entries() >= 100_000,
        "full fixture fell below 100k entries"
    );
    tmp_fixture(ctx, FULL)
}

/// AC #1: a `/tmp` laid out as on the development machine (pytest test
/// dirs in sessions directly in `/tmp` and inside nix-shell TMPDIRs, rc
/// dirs, and many unrelated top-level files), reclaimed by two user
/// profiles in turn (one term each: this build has no unions). Every
/// eligible test dir and rc dir is deleted, and nothing else, by the
/// deletion oracle; every decoy is kept with its reason: a test dir
/// holding a `.git` (`contains_checkout`), one with a socket and one with a
/// FIFO (`contains_ipc`), one holding another user's file and an rc dir of
/// another user (`not_owner`), a session with a keep marker (`keep_marker`), a
/// test dir that is a mount point (`contains_mount`), one holding a file a
/// process has open (`in_use`), a fresh one of each profile (`not_idle`);
/// a pytest
/// session inside `systemd-private-*` (which a pattern matches) is never
/// looked at, and neither are symlinks, plain files or the filler.
fn tmp_fixture(ctx: &Ctx, scale: Scale) -> TestResult {
    let s = &ctx.scratch;
    let built = Instant::now();
    let mut eligible: Vec<PathBuf> = Vec::new();
    let test_dir = |rel: &str, eligible: &mut Vec<PathBuf>| -> TestResult {
        let dir = s.mkdir(rel, 0o755, TESTER)?;
        s.write_file(&format!("{rel}/out.txt"), &[1u8; 512], 0o644, TESTER)?;
        s.write_file(&format!("{rel}/data.bin"), &[2u8; 4096], 0o644, TESTER)?;
        eligible.push(dir);
        Ok(())
    };
    s.mkdir("pytest-of-tester", 0o700, TESTER)?;
    for session in 0..scale.sessions {
        let rel = format!("pytest-of-tester/pytest-{session}");
        s.mkdir(&rel, 0o700, TESTER)?;
        for test in 0..scale.tests {
            test_dir(&format!("{rel}/test_{test}"), &mut eligible)?;
        }
        // pytest's own files around them: never candidates.
        s.symlink(
            &format!("{rel}/test_0current"),
            &s.path(&format!("{rel}/test_0")),
        )?;
        s.write_file(&format!("{rel}/.lock"), b"4242", 0o644, TESTER)?;
    }
    s.symlink(
        "pytest-of-tester/pytest-current",
        &s.path("pytest-of-tester/pytest-0"),
    )?;
    for shell in 0..scale.shells {
        let rel = format!("nix-shell.Ab{shell:04}/pytest-of-tester/pytest-0");
        s.mkdirs(&rel, 0o700, TESTER)?;
        for test in 0..scale.tests {
            test_dir(&format!("{rel}/test_{test}"), &mut eligible)?;
        }
    }
    for rc in 0..scale.rc_dirs {
        let kind = if rc % 4 == 0 {
            "nix-develop"
        } else {
            "nix-shell"
        };
        let rel = format!("{kind}-{}-{}", 100_000 + rc, 3_000_000_000u64 + rc as u64);
        let dir = s.mkdir(&rel, 0o700, TESTER)?;
        s.write_file(&format!("{rel}/rc"), b"export PATH=...\n", 0o644, TESTER)?;
        s.write_file(&format!("{rel}/env-vars"), b"x=1\n", 0o644, TESTER)?;
        eligible.push(dir);
    }
    // The last eligible entry was made now: its age is counted from here.
    let last_eligible = Instant::now();
    for f in 0..scale.filler {
        s.write_file(&format!("x.{f:06}"), b"", 0o644, TESTER)?;
    }
    // The decoys, each with the reason it is kept.
    let session = "pytest-of-tester/pytest-0";
    let mut kept: Vec<(String, &str)> = Vec::new();
    let decoy = |name: &str| format!("{session}/{name}");
    s.mkdirs(&decoy("test_git/.git/refs"), 0o755, TESTER)?;
    s.write_file(&decoy("test_git/.git/HEAD"), b"ref: x\n", 0o644, TESTER)?;
    kept.push((decoy("test_git"), "contains_checkout"));
    s.mkdir(&decoy("test_sock"), 0o755, TESTER)?;
    s.socket(&decoy("test_sock/server.sock"), TESTER)?;
    kept.push((decoy("test_sock"), "contains_ipc"));
    s.mkdir(&decoy("test_fifo"), 0o755, TESTER)?;
    s.fifo(&decoy("test_fifo/pipe"), 0o600, TESTER)?;
    kept.push((decoy("test_fifo"), "contains_ipc"));
    s.mkdir(&decoy("test_theirs"), 0o755, TESTER)?;
    s.write_file(&decoy("test_theirs/f"), b"theirs", 0o644, OTHER)?;
    kept.push((decoy("test_theirs"), "not_owner"));
    s.mkdir(&decoy("test_busy"), 0o755, TESTER)?;
    s.write_file(&decoy("test_busy/f"), b"busy", 0o644, TESTER)?;
    kept.push((decoy("test_busy"), "in_use"));
    s.mkdir(&decoy("test_mnt"), 0o755, TESTER)?;
    kept.push((decoy("test_mnt"), "contains_mount"));
    s.mkdirs("pytest-of-tester/pytest-kept/test_k", 0o700, TESTER)?;
    s.write_file(
        "pytest-of-tester/pytest-kept/.diskgc-keep",
        b"",
        0o644,
        TESTER,
    )?;
    kept.push((
        "pytest-of-tester/pytest-kept/test_k".to_owned(),
        "keep_marker",
    ));
    s.mkdir("nix-shell-1-theirs", 0o755, OTHER)?;
    s.write_file("nix-shell-1-theirs/rc", b"theirs", 0o644, OTHER)?;
    kept.push(("nix-shell-1-theirs".to_owned(), "not_owner"));
    s.mkdir("nix-shell-2-pipe", 0o700, TESTER)?;
    s.fifo("nix-shell-2-pipe/fifo", 0o600, TESTER)?;
    kept.push(("nix-shell-2-pipe".to_owned(), "contains_ipc"));
    s.write_file("nix-shell-3-file", b"a file, no dir", 0o644, TESTER)?;
    let private = "systemd-private-0123abcd-cups.service-Xy9";
    s.mkdirs(
        &format!("{private}/pytest-of-tester/pytest-0/test_p"),
        0o700,
        TESTER,
    )?;
    s.write_file(
        &format!("{private}/pytest-of-tester/pytest-0/test_p/f"),
        b"private",
        0o644,
        TESTER,
    )?;
    info!(
        "built a /tmp of about {} entries ({} eligible candidates) in {:?}",
        scale.entries(),
        eligible.len(),
        built.elapsed()
    );
    write_config(ctx, scale.idle)?;
    // Old enough: every eligible entry, once the idle time has passed since
    // the last one was made (task-18 round 2, QA 3).
    let waited = last_eligible.elapsed();
    let wait = scale.idle + Duration::from_secs(1);
    if waited < wait {
        std::thread::sleep(wait - waited);
    }
    // The fresh ones, made now; the mount and the busy process too.
    s.mkdir(&decoy("test_fresh"), 0o755, TESTER)?;
    s.write_file(&decoy("test_fresh/f"), b"fresh", 0o644, TESTER)?;
    kept.push((decoy("test_fresh"), "not_idle"));
    s.mkdir("nix-shell-4-fresh", 0o700, TESTER)?;
    s.write_file("nix-shell-4-fresh/rc", b"fresh", 0o644, TESTER)?;
    kept.push(("nix-shell-4-fresh".to_owned(), "not_idle"));
    s.mount_tmpfs(&decoy("test_mnt"))?;
    let busy_file = s.path(&decoy("test_busy/f"));
    let busy = Holder::tester(ctx, &["hold", busy_file.to_str().ok_or("path")?, "/"], &[])?;
    let fixture = eligible
        .iter()
        .fold(Fixture::new(), |f, path| f.eligible(path.clone()));
    let oracle = Oracle::arm(fixture)?;
    let idle = "&& filter:idle-test";
    let started = Instant::now();
    let pytest = ensure_tmp(
        ctx,
        &[
            "--min-free",
            "100%",
            "--select",
            &format!("profile:pytest-dirs {idle}"),
        ],
    )?;
    let rc = ensure_tmp(
        ctx,
        &[
            "--min-free",
            "100%",
            "--select",
            &format!("profile:rc-dirs {idle}"),
        ],
    )?;
    let took = started.elapsed();
    let verdict = oracle.judge_runs(&[&pytest, &rc])?;
    drop(busy);
    s.unmount(&decoy("test_mnt"))?;
    info!(
        "two reclaiming runs over {} candidates in {took:?}",
        eligible.len()
    );
    ensure!(
        took < scale.idle - Duration::from_secs(5),
        "inconclusive: the runs took {took:?}, so the fresh decoys may have aged past the idle \
         filter of {:?}; raise it",
        scale.idle
    );
    let mut skipped = BTreeMap::new();
    let mut reclaimed: u64 = 0;
    let eligible_set: BTreeSet<String> = eligible.iter().map(|p| p.display().to_string()).collect();
    for run in [&pytest, &rc] {
        // Exhausted: every eligible candidate processed, 100% never met.
        run.expect_code(1)?;
        let r = report(run)?;
        ensure!(r["status"] == "exhausted", "{}", r["status"]);
        ensure!(
            r["errors"].as_array().is_some_and(Vec::is_empty),
            "{}",
            r["errors"]
        );
        let (s, _) = outcome(&r);
        skipped.extend(s);
        reclaimed += reclaimed_count(&r, &eligible_set)?;
    }
    // Candidates, not rows: the report folds every action past its first
    // 1,000 into one summary row with a `count` (TASK-192, TASK-207).
    ensure!(
        reclaimed == eligible.len() as u64,
        "{reclaimed} candidates reclaimed for {} eligible candidates",
        eligible.len()
    );
    for path in &eligible {
        ensure!(!exists(path), "{} was not deleted", path.display());
    }
    for (rel, reason) in &kept {
        let path = s.path(rel);
        ensure!(exists(&path), "{} was deleted", path.display());
        let got = skipped.get(&path.display().to_string());
        ensure!(
            got.map(String::as_str) == Some(*reason),
            "{}: skipped as {got:?}, expected {reason}",
            path.display()
        );
    }
    let private_test = s.path(&format!("{private}/pytest-of-tester/pytest-0/test_p"));
    ensure!(
        !skipped.contains_key(&private_test.display().to_string()),
        "a systemd-private-* entry was a candidate"
    );
    for kept_too in [
        "x.000000",
        "nix-shell-3-file",
        "pytest-of-tester/pytest-current",
        "pytest-of-tester/pytest-0/test_0current",
        "pytest-of-tester/pytest-0/.lock",
    ] {
        ensure!(exists(&s.path(kept_too)), "{kept_too} was deleted");
    }
    verdict.assert_clean()
}

/// TASK-31 AC #4 at the commit gate's size: about 8,000 entries.
pub fn fs_reclaim_time_to_first_deletion_is_measured_cold_and_warm(ctx: &Ctx) -> TestResult {
    first_deletion(ctx, 2_000, 2_000)
}

/// TASK-31 AC #4 on the PRD's 100,000-entry `/tmp` (the full tier).
pub fn fs_reclaim_100k_entry_time_to_first_deletion_is_measured_cold_and_warm(
    ctx: &Ctx,
) -> TestResult {
    first_deletion(ctx, 10_000, 70_001)
}

/// The value of `<what> <number> ms` in the first line of `stderr` that
/// holds `line`.
fn logged_ms(stderr: &str, line: &str, what: &str) -> Result<u64, String> {
    let text = stderr
        .lines()
        .find(|l| l.contains(line))
        .ok_or_else(|| format!("no `{line}` line in:\n{stderr}"))?;
    let after = text
        .split(what)
        .nth(1)
        .ok_or_else(|| format!("no `{what}` in `{text}`"))?;
    let digits: String = after
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits
        .parse()
        .map_err(|e| format!("`{text}`: no number after `{what}`: {e}"))
}

/// TASK-31 AC #4 (docs/testing.md, Definition of Done: Freeing works;
/// docs/architecture.md, The reclaim walk: the time to the first
/// deletion): a `/tmp` of `rc_dirs` nix-shell rc directories (each two
/// 4 KiB files) among `filler` empty files (three entries per rc dir), and
/// two `ensure` runs, each with a `--min-free` a quarter of the candidates'
/// bytes above what is available before it: the first from nothing (no
/// snapshot exists before TASK-30, so its synchronous tally lists and
/// sizes everything), the second right after it ("warm": the kernel's
/// caches, never a saved index). Both must reach `--min-free` and exit 0,
/// judged by the deletion oracle. The time to the first deletion and the
/// synchronous tally's time, as diskgc logs them, are logged here: a
/// measurement, not a pass criterion (no latency target). The runs are
/// under the harness's `strace -f`, which slows every system call: the
/// numbers are an upper bound of an untraced run's.
fn first_deletion(ctx: &Ctx, rc_dirs: usize, filler: usize) -> TestResult {
    let s = &ctx.scratch;
    write_config(ctx, Duration::from_secs(1))?;
    let built = Instant::now();
    let mut eligible = Vec::with_capacity(rc_dirs);
    let mut bytes: u64 = 0;
    for rc in 0..rc_dirs {
        let rel = format!(
            "nix-shell-{}-{}",
            100_000 + rc,
            3_000_000_000u64 + rc as u64
        );
        let dir = s.mkdir(&rel, 0o700, TESTER)?;
        for file in ["rc", "env-vars"] {
            let path = s.write_file(&format!("{rel}/{file}"), &[7u8; 4096], 0o644, TESTER)?;
            let meta = std::fs::symlink_metadata(&path).map_err(|e| format!("{path:?}: {e}"))?;
            bytes += std::os::unix::fs::MetadataExt::blocks(&meta) * 512;
        }
        eligible.push(dir);
    }
    for f in 0..filler {
        s.write_file(&format!("x.{f:06}"), b"", 0o644, TESTER)?;
    }
    let entries = rc_dirs * 3 + filler;
    info!(
        "built a /tmp of {entries} entries ({rc_dirs} candidates, {bytes} bytes in them) in {:?}",
        built.elapsed()
    );
    let fixture = eligible
        .iter()
        .fold(Fixture::new(), |f, path| f.eligible(path.clone()));
    let oracle = Oracle::arm(fixture)?;
    let quarter = bytes / 4;
    let mut runs = Vec::new();
    for name in ["cold", "warm"] {
        let before = crate::ensure_suite::avail(Path::new("/tmp"))?;
        let goal = before + quarter;
        let min_free = format!("{}K", goal.div_ceil(1024));
        let run = ensure_tmp(
            ctx,
            &["--min-free", &min_free, "--select", "profile:rc-dirs"],
        )?;
        run.expect_code(0)?;
        let r = report(&run)?;
        ensure!(r["status"] == "met", "{name}: {}", r["status"]);
        let after = crate::ensure_suite::avail(Path::new("/tmp"))?;
        ensure!(
            after >= goal.div_ceil(1024) * 1024,
            "{name}: {after} bytes available after an exit 0 for --min-free {min_free}"
        );
        let first = logged_ms(&run.stderr, "first deletion:", "first deletion:")?;
        let tally = logged_ms(&run.stderr, "index: synchronous tally", " in")?;
        info!(
            "time to first deletion ({name}, {entries} entries, under strace): {first} ms; \
             synchronous tally {tally} ms; the run {:?}; {} -> {after} bytes available",
            run.took, before
        );
        runs.push(run);
    }
    let verdict = oracle.judge_runs(&runs.iter().collect::<Vec<_>>())?;
    verdict.assert_clean()
}

/// AC #5, the kernel's sockets and FIFOs: under the default policy no
/// socket or FIFO is ever deleted. A candidate holding a stale one (its
/// server gone, nothing holding the FIFO) is `contains_ipc`; a socket or
/// FIFO a pattern matches directly is no candidate at all (`type: any` is
/// a directory or a regular file). The plain candidate next to them goes.
pub fn fs_reclaim_never_deletes_a_socket_or_fifo(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    write_config(ctx, Duration::from_secs(1))?;
    s.mkdirs("rc-sock/run", 0o755, TESTER)?;
    s.socket("rc-sock/run/agent.sock", TESTER)?;
    s.write_file("rc-sock/f", b"x", 0o644, TESTER)?;
    s.mkdirs("rc-fifo/a/b", 0o755, TESTER)?;
    s.fifo("rc-fifo/a/b/pipe", 0o600, TESTER)?;
    s.fifo("pipe-top", 0o600, TESTER)?;
    s.socket("sock-top", TESTER)?;
    s.mkdir("rc-plain", 0o755, TESTER)?;
    s.write_file("rc-plain/f", &[0u8; 8192], 0o644, TESTER)?;
    let oracle = Oracle::arm(Fixture::new().eligible(s.path("rc-plain")))?;
    let run = ensure_tmp(ctx, &["--min-free", "100%", "--select", "profile:rc-like"])?;
    let verdict = oracle.judge(&run)?;
    run.expect_code(1)?;
    let (skipped, actions) = outcome(&report(&run)?);
    ensure!(actions == ["/tmp/rc-plain"], "{actions:?}");
    for candidate in ["/tmp/rc-sock", "/tmp/rc-fifo"] {
        ensure!(
            skipped.get(candidate).map(String::as_str) == Some("contains_ipc"),
            "{candidate}: {skipped:?}"
        );
    }
    for path in [
        "rc-sock/run/agent.sock",
        "rc-fifo/a/b/pipe",
        "pipe-top",
        "sock-top",
    ] {
        ensure!(exists(&s.path(path)), "{path} was deleted");
    }
    ensure!(
        !skipped.contains_key("/tmp/pipe-top") && !skipped.contains_key("/tmp/sock-top"),
        "a socket or FIFO was a candidate: {skipped:?}"
    );
    verdict.assert_clean()
}

/// A broad user profile selects a directory enclosing systemd PrivateTmp.
/// Its contents are owned by tester, so only the structural guard keeps it.
pub fn fs_reclaim_never_enters_private_tmp_inside_a_candidate(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    write_config(ctx, Duration::from_secs(1))?;
    let private = "rc-private/systemd-private-abc-cups.service-x1/tmp";
    s.mkdirs(private, 0o700, TESTER)?;
    s.write_file(&format!("{private}/f"), b"keep", 0o600, TESTER)?;
    s.mkdir("rc-plain", 0o755, TESTER)?;
    s.write_file("rc-plain/f", b"delete", 0o644, TESTER)?;
    let oracle = Oracle::arm(Fixture::new().eligible(s.path("rc-plain")))?;
    let run = ensure_tmp(ctx, &["--min-free", "100%", "--select", "profile:rc-like"])?;
    let verdict = oracle.judge(&run)?;
    run.expect_code(1)?;
    let (skipped, actions) = outcome(&report(&run)?);
    ensure!(actions == ["/tmp/rc-plain"], "{actions:?}");
    ensure!(
        skipped.get("/tmp/rc-private").map(String::as_str) == Some("contains_special"),
        "{skipped:?}"
    );
    ensure!(
        exists(&s.path(&format!("{private}/f"))),
        "PrivateTmp was deleted"
    );
    verdict.assert_clean()
}

/// A FUSE mount whose server never answers: the harness opens `/dev/fuse`,
/// mounts it and never reads it, so every request on the mount (a `statx`
/// of its root, an `open`) waits for an answer that never comes. Unmounted
/// lazily and the device closed (which aborts the connection) when dropped.
struct DeadFuse {
    at: PathBuf,
    device: Option<OwnedFd>,
}

impl DeadFuse {
    fn mount(at: &Path) -> Result<Self, String> {
        let device = rustix::fs::open("/dev/fuse", OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
            .map_err(|e| {
            format!(
                "/dev/fuse is unavailable in the container ({e}): the dead FUSE mount test \
                     cannot run here (the runner passes the device only when the host has it)"
            )
        })?;
        let options = format!(
            "fd={},rootmode=40000,user_id=0,group_id=0,allow_other",
            device.as_raw_fd()
        );
        let options = std::ffi::CString::new(options).map_err(|_| "NUL in options")?;
        rustix::mount::mount(
            "diskgc-e2e-dead",
            at,
            "fuse",
            MountFlags::NOSUID | MountFlags::NODEV,
            options.as_c_str(),
        )
        .map_err(|e| {
            format!(
                "mounting a FUSE filesystem at {} failed ({e}): a rootless container may not \
                 allow it on this host",
                at.display()
            )
        })?;
        Ok(Self {
            at: at.to_owned(),
            device: Some(device),
        })
    }

    /// Proves the mount is dead: a `statx` of its root, in a thread of the
    /// harness, has no answer after `wait`. The thread stays blocked until
    /// the connection is aborted ([`Drop`]).
    fn proven_dead(&self, wait: Duration) -> Result<std::thread::JoinHandle<()>, String> {
        let at = self.at.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let blocked = std::thread::spawn(move || {
            let answer = std::fs::symlink_metadata(&at).map(|_| ());
            let _ = tx.send(answer.map_err(|e| e.to_string()));
        });
        match rx.recv_timeout(wait) {
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(blocked),
            Ok(answer) => Err(format!(
                "the FUSE mount at {} answered a stat ({answer:?}): not dead, the test would \
                 prove nothing",
                self.at.display()
            )),
            Err(e) => Err(format!("control thread: {e}")),
        }
    }
}

impl Drop for DeadFuse {
    fn drop(&mut self) {
        if let Err(e) = rustix::mount::unmount(&self.at, UnmountFlags::DETACH) {
            info!("unmount {}: {e}", self.at.display());
        }
        // Closing the only device fd aborts the connection: every waiting
        // request fails (ENOTCONN), the control thread included.
        drop(self.device.take());
    }
}

/// A protected root on a dead FUSE mount must be refused before the root
/// is opened. Protection discovery must fail closed quickly, and no sibling
/// candidate may be deleted after that incomplete scan. `check` takes the
/// same decision (TASK-194): it says the root is not looked up, at once,
/// and exits 0.
pub fn fs_protected_dead_fuse_root_is_refused_without_opening_it(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    ctx.home.write_file(
        "diskgc.yaml",
        b"version: 1\nprofiles:\n  - name: reclaim-test\n    find: {roots: [/tmp], paths: [rc-plain], type: dir}\n    action: delete\n    examples: {match: [/tmp/rc-plain]}\n  - name: guard\n    find: {roots: required, paths: ['*'], type: dir}\n    action: delete\n    examples: {root: /tmp, match: [/tmp/guarded]}\nprotect: [{profile: guard, roots: [/tmp/fuse-dead]}]\n",
        0o644,
        TESTER,
    )?;
    s.mkdir("fuse-dead", 0o755, ROOT)?;
    s.mkdir("rc-plain", 0o755, TESTER)?;
    s.write_file("rc-plain/payload", b"keep", 0o600, TESTER)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let fuse = DeadFuse::mount(&s.path("fuse-dead"))?;
    let blocked = fuse.proven_dead(Duration::from_secs(2))?;
    let checked = ctx.run_as_tester(&ctx.diskgc(), &["check", "--config", CONFIG])?;
    checked.expect_code(0)?;
    ensure!(
        checked.took < Duration::from_secs(5),
        "check stalled on the dead FUSE root for {:?}",
        checked.took
    );
    ensure!(
        checked
            .stderr
            .contains("warning: root /tmp/fuse-dead is not looked up: ")
            && !checked.stderr.contains("does not exist"),
        "{}",
        checked.stderr
    );
    let run = ensure_tmp(
        ctx,
        &["--min-free", "100%", "--select", "profile:reclaim-test"],
    )?;
    drop(fuse);
    blocked
        .join()
        .map_err(|_| "FUSE control thread panicked".to_owned())?;
    let verdict = oracle.judge_runs(&[&checked, &run])?;
    run.expect_code(3)?;
    ensure!(
        run.took < Duration::from_secs(5),
        "protected dead FUSE root stalled for {:?}",
        run.took
    );
    let output = report(&run)?;
    ensure!(
        output["actions"].as_array().is_some_and(Vec::is_empty),
        "{output}"
    );
    ensure!(
        output["errors"]
            .as_array()
            .is_some_and(|errors| errors.iter().any(|error| error["message"]
                .as_str()
                .is_some_and(
                    |message| message.contains("protect `guard`") && message.contains("fuse")
                ))),
        "{output}"
    );
    ensure!(exists(&s.path("rc-plain/payload")), "sibling was deleted");
    verdict.assert_clean()
}

/// AC #6: a dead FUSE mount in `/tmp`, as a candidate's name and inside a
/// candidate, is never touched, so it does not hang discovery or the
/// judgement (mountinfo first: a mount point is never `statx`ed, opened or
/// listed). The in-use scan's own limit with dead mounts is another matter
/// (docs/safety.md, In use). The dry run and the reclaim both
/// finish; both candidates holding the mounts are `contains_mount`, the
/// old directory next to them is deleted, and nothing else.
pub fn fs_reclaim_a_dead_fuse_mount_is_never_touched(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    write_config(ctx, Duration::from_secs(1))?;
    s.mkdir("fuse-dead", 0o755, ROOT)?;
    s.mkdirs("holder/mnt", 0o755, TESTER)?;
    s.write_file("holder/f", b"x", 0o644, TESTER)?;
    s.mkdir("old", 0o755, TESTER)?;
    s.write_file("old/f", &[0u8; 8192], 0o644, TESTER)?;
    let oracle = Oracle::arm(Fixture::new().eligible(s.path("old")))?;
    let outer = DeadFuse::mount(&s.path("fuse-dead"))?;
    let inner = DeadFuse::mount(&s.path("holder/mnt"))?;
    let blocked = [
        outer.proven_dead(Duration::from_secs(2))?,
        inner.proven_dead(Duration::from_secs(2))?,
    ];
    let started = Instant::now();
    let plan = ensure_tmp(
        ctx,
        &[
            "--dry-run",
            "--min-free",
            "100%",
            "--select",
            "profile:tmp-any",
        ],
    )?;
    let run = ensure_tmp(ctx, &["--min-free", "100%", "--select", "profile:tmp-any"])?;
    let took = started.elapsed();
    drop(inner);
    drop(outer);
    for thread in blocked {
        thread
            .join()
            .map_err(|_| "a control thread panicked".to_owned())?;
    }
    let verdict = oracle.judge_runs(&[&plan, &run])?;
    info!("dry run and reclaim with two dead FUSE mounts in /tmp: {took:?}");
    plan.expect_code(0)?;
    let planned = report(&plan)?;
    let reasons: BTreeMap<String, Option<String>> = planned["plan"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .map(|e| {
                    (
                        e["candidate"].as_str().unwrap_or_default().to_owned(),
                        e["reason"].as_str().map(str::to_owned),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    for candidate in ["/tmp/fuse-dead", "/tmp/holder"] {
        ensure!(
            reasons.get(candidate) == Some(&Some("contains_mount".to_owned())),
            "plan of {candidate}: {reasons:?}"
        );
    }
    run.expect_code(1)?;
    let (skipped, actions) = outcome(&report(&run)?);
    ensure!(actions == ["/tmp/old"], "{actions:?}");
    for candidate in ["/tmp/fuse-dead", "/tmp/holder"] {
        ensure!(
            skipped.get(candidate).map(String::as_str) == Some("contains_mount"),
            "{candidate}: {skipped:?}"
        );
    }
    ensure!(exists(&s.path("holder/f")), "the holder's file went");
    verdict.assert_clean()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn detail(candidate: &str) -> Value {
        json!({"tier": "t", "profile": "p", "candidate": candidate, "count": 1})
    }

    fn summary(count: u64) -> Value {
        json!({"tier": null, "profile": null, "candidate": null, "count": count})
    }

    fn eligible(n: usize) -> BTreeSet<String> {
        (0..n).map(|i| format!("/tmp/c{i}")).collect()
    }

    fn details(n: usize) -> Vec<Value> {
        (0..n).map(|i| detail(&format!("/tmp/c{i}"))).collect()
    }

    #[test]
    fn counts_candidates_in_the_folded_summary_row_not_rows() {
        let mut rows = details(1_000);
        rows.push(summary(7_000));
        let report = json!({ "actions": rows });
        assert_eq!(reclaimed_count(&report, &eligible(1_000)), Ok(8_000));
        let short = json!({ "actions": details(3) });
        assert_eq!(reclaimed_count(&short, &eligible(3)), Ok(3));
    }

    #[test]
    fn rejects_a_report_whose_rows_break_the_fold_contract() {
        let set = eligible(1_000);
        let early_summary = json!({ "actions": [detail("/tmp/c0"), summary(5)] });
        let mut after_summary = details(1_000);
        after_summary.push(summary(5));
        after_summary.push(summary(5));
        let twice = json!({ "actions": [detail("/tmp/c0"), detail("/tmp/c0")] });
        let stranger = json!({ "actions": [detail("/tmp/other")] });
        let fat_detail = json!({ "actions": [{"tier": "t", "profile": "p", "candidate": "/tmp/c0", "count": 2}] });
        let no_count = json!({ "actions": [{"candidate": "/tmp/c0"}] });
        for bad in [
            early_summary,
            json!({ "actions": after_summary }),
            twice,
            stranger,
            fat_detail,
            no_count,
            json!({}),
        ] {
            assert!(reclaimed_count(&bad, &set).is_err(), "{bad}");
        }
    }
}
