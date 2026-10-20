//! Container tests of the `fs` kind's reclaim (task-18; docs/safety.md,
//! Filesystem candidates): the real binary's reclaiming `ensure` over user
//! profiles from a config file of the test's own (no built-in names), run
//! as `tester` under strace and judged by the deletion oracle, whose
//! eligible set the harness computes from the fixture it built, never from
//! diskgc's output. Where these tests need the kernel's view: real
//! sockets and FIFOs, a real mount, a process's working directory, a
//! `systemd-private-*` directory, a `/tmp` of 100,000 entries, and a FUSE
//! mount whose server never answers.

use std::collections::BTreeMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::fs::{Mode, OFlags};
use rustix::mount::{MountFlags, UnmountFlags};
use serde_json::Value;

use crate::ensure_suite::report;
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

const EXCLUDED_CACHE_TEST_PROFILE: &str = r#"version: 1
profiles:
  - name: cache-exclusion-test
    find: {roots: [/tmp], paths: ["huggingface", "nix-index"], exclude: ["huggingface"], type: dir}
    action: delete
    examples: {match: [/tmp/nix-index], no_match: [/tmp/huggingface]}
"#;

const CMAKE_GUARD_TEST_PROFILE: &str = r#"version: 1
profiles:
  - name: cmake-guard-kernel
    find: {roots: [/tmp], paths: ["cmake-*-build"], type: dir}
    where: {has: CMakeCache.txt, lacks_entry: CMakeLists.txt}
    action: delete
    examples: {match: [/tmp/cmake-free-build]}
"#;

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
/// another user (`not_owner`), a session with a keep marker (`kept`), a
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
    kept.push(("pytest-of-tester/pytest-kept/test_k".to_owned(), "kept"));
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
    let mut actions = Vec::new();
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
        let (s, a) = outcome(&r);
        skipped.extend(s);
        actions.extend(a);
    }
    ensure!(
        actions.len() == eligible.len(),
        "{} actions for {} eligible candidates",
        actions.len(),
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

/// AC #5, the kernel's sockets and FIFOs: under the default policy no
/// socket or FIFO is ever deleted. A candidate holding a stale one (its
/// server gone, nothing holding the FIFO) is `contains_ipc`; a socket or
/// FIFO a pattern matches directly is no candidate at all (`type: any` is
/// a directory or a regular file). The plain candidate next to them goes.
pub fn fs_reclaim_never_deletes_a_socket_or_fifo(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    write_config(ctx, Duration::from_secs(0))?;
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
    write_config(ctx, Duration::from_secs(0))?;
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

/// AC #6: a dead FUSE mount in `/tmp`, as a candidate's name and inside a
/// candidate, is never touched, so it does not hang discovery or the
/// judgement (mountinfo first: a mount point is never `statx`ed, opened or
/// listed). The in-use scan's own limit with dead mounts is another matter
/// (docs/safety.md, In use). The dry run and the reclaim both
/// finish; both candidates holding the mounts are `contains_mount`, the
/// old directory next to them is deleted, and nothing else.
pub fn fs_reclaim_a_dead_fuse_mount_is_never_touched(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    write_config(ctx, Duration::from_secs(0))?;
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
