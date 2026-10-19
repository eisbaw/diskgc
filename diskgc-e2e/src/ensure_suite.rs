//! Container tests of `diskgc ensure` (task-10; docs/reference.md,
//! `ensure`, Exit codes, Output): the real binary, run as `tester` (or, for
//! the root refusal, as container root) under strace, judged by the
//! deletion oracle, whose eligible set the harness computes from the tree,
//! never from diskgc's output. Space is measured by the harness itself
//! (`statvfs` of the scratch tmpfs), so a goal the run must reach is set
//! from the harness's own numbers.
//!
//! The Cargo tests run once per Cargo toolchain under test
//! ([`crate::cargo_suite::CargoTest`]) on targets real builds made.

use std::collections::BTreeSet;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::cargo_harness::{deletable, lock_inodes, objects_below, profile_dirs, reclaimable};
use crate::cargo_under_test::{Cargo, SMOKE, Toolchain};
use crate::holder::Holder;
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, HOST_LOCK, TestResult};
use crate::under_test::Run;
use crate::{ROOT, TESTER, ensure, info};

/// Available bytes of the filesystem holding `path`, as an unprivileged
/// writer may use them (`f_bavail × f_frsize`): the harness's own measure.
fn avail(path: &Path) -> Result<u64, String> {
    let s = rustix::fs::statvfs(path).map_err(|e| format!("statvfs {path:?}: {e}"))?;
    Ok(s.f_bavail * s.f_frsize)
}

/// The allocated bytes of every object below the target's profile
/// directories' entries, each inode once (Cargo hard-links binaries from
/// `deps/`): what reclaiming the target frees on tmpfs.
fn freed_by(target: &Path) -> Result<u64, String> {
    let mut seen = BTreeSet::new();
    let mut bytes = 0;
    for profile in profile_dirs(target)? {
        for entry in deletable(&profile)? {
            for object in objects_below(&entry)? {
                let meta = std::fs::symlink_metadata(&object)
                    .map_err(|e| format!("lstat {object:?}: {e}"))?;
                if seen.insert(meta.ino()) {
                    bytes += meta.blocks() * 512;
                }
            }
        }
    }
    Ok(bytes)
}

/// `diskgc ensure ARGS` as tester under strace.
fn ensure_run(ctx: &Ctx, args: &[&str]) -> Result<Run, String> {
    let mut full = vec!["ensure"];
    full.extend_from_slice(args);
    let run = ctx.run_as_tester(&ctx.diskgc(), &full)?;
    info!(
        "diskgc {} -> {:?}\n--- stdout\n{}--- stderr\n{}",
        full.join(" "),
        run.code,
        run.stdout,
        run.stderr
    );
    Ok(run)
}

/// The one JSON object a `--json` run printed.
fn report(run: &Run) -> Result<Value, String> {
    let lines: Vec<&str> = run.stdout.lines().collect();
    ensure!(lines.len() == 1, "--json prints one line: {:?}", run.stdout);
    serde_json::from_str(lines[0]).map_err(|e| format!("{e}: {:?}", lines[0]))
}

/// `field` of every entry of the report's list `list`.
fn column<'a>(report: &'a Value, list: &str, field: &str) -> Vec<&'a Value> {
    report[list]
        .as_array()
        .map(|entries| entries.iter().map(|e| &e[field]).collect())
        .unwrap_or_default()
}

/// Removes the provisioned host lock (`/run/diskgc/lock`), as on a host
/// without the tmpfiles drop-in.
fn unprovision(ctx: &Ctx) -> TestResult {
    ctx.run_diskgc.remove_file("lock")?;
    ensure!(!Path::new(HOST_LOCK).exists(), "{HOST_LOCK} still there");
    Ok(())
}

/// A directory of tester's in the scratch tmpfs: PATH and the root. It
/// holds a 4 KiB file, so the tmpfs is never entirely free (an empty tmpfs
/// counts no used blocks, and `--min-free 100%` would be met).
fn work_dir(ctx: &Ctx) -> TestResult {
    ctx.scratch.mkdir("w", 0o755, TESTER)?;
    ctx.scratch
        .write_file("w/fill", &[0u8; 4096], 0o644, TESTER)?;
    let s = rustix::fs::statvfs("/tmp").map_err(|e| format!("statvfs /tmp: {e}"))?;
    ensure!(
        s.f_bavail < s.f_blocks,
        "the scratch tmpfs reports nothing used: {} of {} blocks available",
        s.f_bavail,
        s.f_blocks
    );
    Ok(())
}

/// AC #1 and exit 0: already above `--min-free`, `ensure` exits 0 without
/// taking the lock. The lock file is removed first, so a run that tried to
/// take it would exit 2; human mode prints nothing, `--json` says `met`
/// with the mount resolved.
pub fn ensure_above_min_free_exits_0_without_the_host_lock(ctx: &Ctx) -> TestResult {
    work_dir(ctx)?;
    unprovision(ctx)?;
    let args = [
        "--min-free",
        "1K",
        "--select",
        "profile:cargo-target",
        "--roots",
        "/tmp/w",
        "/tmp/w",
    ];
    let oracle = Oracle::arm(Fixture::new())?;
    let quiet = ensure_run(ctx, &args)?;
    let mut with_json = args.to_vec();
    with_json.insert(0, "--json");
    let json = ensure_run(ctx, &with_json)?;
    let judged = oracle.judge_runs(&[&quiet, &json])?;
    quiet.expect_code(0)?;
    ensure!(
        quiet.stdout.is_empty(),
        "nothing needed doing: silent, got {:?}",
        quiet.stdout
    );
    json.expect_code(0)?;
    let r = report(&json)?;
    ensure!(r["status"] == "met", "{r}");
    ensure!(r["mount"] == "/tmp" && r["path"] == "/tmp/w", "{r}");
    ensure!(r["target_met"] == true, "{r}");
    ensure!(r["avail_before_bytes"] == r["avail_after_bytes"], "{r}");
    ensure!(column(&r, "actions", "candidate").is_empty(), "{r}");
    judged.assert_clean()
}

/// AC #2 and exit 2: below `--min-free` without `/run/diskgc/lock`, exit 2
/// printing the two tmpfiles lines (docs/safety.md, Host lock); nothing is
/// deleted.
pub fn ensure_without_the_host_lock_file_exits_2_printing_the_tmpfiles_lines(
    ctx: &Ctx,
) -> TestResult {
    work_dir(ctx)?;
    unprovision(ctx)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let run = ensure_run(
        ctx,
        &[
            "--min-free",
            "100%",
            "--json",
            "--select",
            "profile:cargo-target",
            "--roots",
            "/tmp/w",
            "/tmp/w",
        ],
    )?;
    let judged = oracle.judge(&run)?;
    run.expect_code(2)?;
    for line in [
        "d /run/diskgc      0755 root root -",
        "f /run/diskgc/lock 0644 root root -",
    ] {
        ensure!(
            run.stderr.contains(line),
            "stderr lacks {line:?}: {}",
            run.stderr
        );
    }
    let r = report(&run)?;
    ensure!(r["status"] == "usage_error" && r["mount"] == "/tmp", "{r}");
    judged.assert_clean()
}

/// AC #4 and exit 2: run as root (uid 0 in the container's user
/// namespace) without `--allow-root`, `ensure` exits 2 before measuring
/// anything; `--allow-root` is refused too in this build (it needs
/// `owners:`, task-21).
pub fn ensure_as_root_without_allow_root_exits_2(ctx: &Ctx) -> TestResult {
    work_dir(ctx)?;
    let args = [
        "ensure",
        "--min-free",
        "100%",
        "--json",
        "--select",
        "profile:cargo-target",
        "--roots",
        "/tmp/w",
        "/tmp/w",
    ];
    let oracle = Oracle::arm(Fixture::new())?;
    let root = ctx.run_as_root(&ctx.diskgc(), &args)?;
    let mut allow: Vec<&str> = args.to_vec();
    allow.insert(1, "--allow-root");
    let allowed = ctx.run_as_root(&ctx.diskgc(), &allow)?;
    let judged = oracle.judge_runs(&[&root, &allowed])?;
    info!(
        "as root: {}\nwith --allow-root: {}",
        root.stderr, allowed.stderr
    );
    root.expect_code(2)?;
    ensure!(
        root.stderr.contains("refuses to run as root"),
        "{}",
        root.stderr
    );
    let r = report(&root)?;
    ensure!(r["status"] == "usage_error" && r["mount"].is_null(), "{r}");
    allowed.expect_code(2)?;
    ensure!(allowed.stderr.contains("owners:"), "{}", allowed.stderr);
    judged.assert_clean()
}

/// The host lock path holding something other than a regular file (a
/// FIFO, a directory, a symlink to a regular file) is refused, exit 2, on a
/// real kernel: the FIFO is never opened (an open for reading would block
/// until a writer came), and nothing is locked or deleted.
pub fn ensure_refuses_a_host_lock_that_is_no_regular_file(ctx: &Ctx) -> TestResult {
    work_dir(ctx)?;
    ctx.scratch.write_file("w/elsewhere", b"", 0o644, TESTER)?;
    let r = &ctx.run_diskgc;
    for what in ["fifo", "directory", "symlink"] {
        r.remove_file("lock")?;
        match what {
            "fifo" => drop(r.fifo("lock", 0o644, ROOT)?),
            "directory" => drop(r.mkdir("lock", 0o755, ROOT)?),
            _ => drop(r.symlink("lock", Path::new("/tmp/w/elsewhere"))?),
        }
        let oracle = Oracle::arm(Fixture::new())?;
        let run = ensure_run(
            ctx,
            &[
                "--min-free",
                "100%",
                "--timeout",
                "10s",
                "--json",
                "--select",
                "profile:cargo-target",
                "--roots",
                "/tmp/w",
                "/tmp/w",
            ],
        )?;
        let judged = oracle.judge(&run)?;
        run.expect_code(2)?;
        ensure!(
            run.stderr
                .contains("host lock /run/diskgc/lock is unusable"),
            "{what}: {}",
            run.stderr
        );
        ensure!(
            report(&run)?["status"] == "usage_error",
            "{what}: {}",
            run.stdout
        );
        ensure!(
            run.took < Duration::from_secs(5),
            "{what}: took {:?} (blocked on the lock path?)",
            run.took
        );
        judged.assert_clean()?;
        // Back to a file, so the next case starts from the same place.
        let lock = Path::new(HOST_LOCK);
        if what == "directory" {
            std::fs::remove_dir(lock).map_err(|e| format!("rmdir {lock:?}: {e}"))?;
        } else {
            std::fs::remove_file(lock).map_err(|e| format!("rm {lock:?}: {e}"))?;
        }
        r.write_file("lock", b"", 0o644, ROOT)?;
    }
    Ok(())
}

/// Exit 75 (ahead of task-54's model tests, since the wait is here): the
/// host lock held by another process until `--timeout`, still below
/// `--min-free`: exit 75 after the deadline, nothing deleted.
pub fn ensure_waits_for_a_held_host_lock_until_the_deadline(ctx: &Ctx) -> TestResult {
    work_dir(ctx)?;
    // As root: the lock file is root's 0644, and flock-hold opens it for
    // writing; root holding it is the "any local user" of docs/safety.md.
    let holder = Holder::root(ctx, &["flock-hold", HOST_LOCK])?;
    let oracle = Oracle::arm(Fixture::new())?;
    let run = ensure_run(
        ctx,
        &[
            "--min-free",
            "100%",
            "--timeout",
            "2s",
            "--json",
            "--select",
            "profile:cargo-target",
            "--roots",
            "/tmp/w",
            "/tmp/w",
        ],
    )?;
    let judged = oracle.judge(&run)?;
    run.expect_code(75)?;
    ensure!(report(&run)?["status"] == "lock_timeout", "{}", run.stdout);
    ensure!(
        run.took >= Duration::from_secs(2),
        "returned after {:?}, before its deadline",
        run.took
    );
    holder.finish()?;
    judged.assert_clean()
}

/// Lock inheritance (docs/safety.md, Host lock): the host lock taken by
/// diskgc-core's `hostlock::acquire` (through the test driver) is
/// inherited by a child; after the driver closed its own descriptor and
/// exited, the child's copy still holds it, so nothing released it but
/// closing (`flock(LOCK_UN)` on any copy would have released every copy);
/// once the child exits, the lock is free.
pub fn ensure_host_lock_is_inherited_by_children_and_released_only_by_close(
    ctx: &Ctx,
) -> TestResult {
    let gate = ctx.scratch.path("go");
    let gate_text = gate.to_str().ok_or("gate path")?.to_owned();
    ensure!(!held_by_someone()?, "the host lock is held before the test");
    let oracle = Oracle::arm(Fixture::new())?;
    let mut spawned = ctx.spawn_as_tester(&ctx.driver(), &["host-lock", &gate_text])?;
    let child = spawned.next_line("child pid=")?;
    spawned.next_line(" closed")?;
    let pid: u32 = child
        .rsplit("pid=")
        .next()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| format!("no pid in {child:?}"))?;
    ensure!(
        Path::new(&format!("/proc/{pid}")).exists(),
        "the child {pid} is gone already"
    );
    let still = held_by_someone()?;
    ctx.scratch.write_file("go", b"", 0o644, TESTER)?;
    let run = spawned.wait()?;
    let judged = oracle.judge(&run)?;
    info!("host-lock:\n{}--- stderr\n{}", run.stdout, run.stderr);
    run.expect_code(0)?;
    ensure!(
        still,
        "the host lock was free once the driver closed its copy: the child did not inherit it"
    );
    ensure!(
        !held_by_someone()?,
        "the host lock is still held after the child exited"
    );
    judged.assert_clean()
}

/// Whether someone holds the host lock: a non-blocking exclusive `flock`
/// by the harness (as root, on its own open file description) is refused.
fn held_by_someone() -> Result<bool, String> {
    let fd = rustix::fs::open(
        HOST_LOCK,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|e| format!("open {HOST_LOCK}: {e}"))?;
    match rustix::fs::flock(&fd, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(false),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(true),
        Err(e) => Err(format!("flock {HOST_LOCK}: {e}")),
    }
}

fn build(ctx: &Ctx, toolchain: &Toolchain, project: &Path) -> TestResult {
    Cargo::new(toolchain, project, &["build"])
        .output(ctx)?
        .expect_success()?;
    Ok(())
}

/// A Git checkout in `old`'s target, where a build script would clone a
/// vendored source (its `OUT_DIR`), relative to the scratch root.
const VENDORED_CHECKOUT: &str = "w/old/target/debug/build/vendored-0000/out/repo/.git";

/// Two projects built in turn, `old` first: `old`'s last build is older
/// (the clock moves on by at least a second between them, so their times
/// differ even on a coarse clock). `old`'s target also holds a Git
/// checkout ([`VENDORED_CHECKOUT`], made before `new` is built, so `old`
/// stays the older): the built-in `cargo-target` says `checkouts: allow`
/// (task-80), so it is part of the target like any other entry.
fn two_targets(ctx: &Ctx, toolchain: &Toolchain) -> Result<(), String> {
    work_dir(ctx)?;
    let old = SMOKE.write(&ctx.scratch, "w/old")?;
    build(ctx, toolchain, &old)?;
    ctx.scratch.mkdirs(VENDORED_CHECKOUT, 0o755, TESTER)?;
    ctx.scratch.write_file(
        &format!("{VENDORED_CHECKOUT}/HEAD"),
        b"ref: refs/heads/main\n",
        0o644,
        TESTER,
    )?;
    std::thread::sleep(Duration::from_millis(1100));
    let new = SMOKE.write(&ctx.scratch, "w/new")?;
    build(ctx, toolchain, &new)
}

/// Exit 0 after a real reclaim: two real targets, a `--min-free` the
/// harness sets halfway into what the older one frees: `ensure` takes the
/// host lock, reclaims `old` first (the tier's `oldest` order), observes
/// the goal and stops, so `new` is untouched. The oracle allows only
/// `old`'s profile entries to go; lock files keep their inodes; the next
/// build of `old` succeeds. The checkout in `old`'s target goes with it
/// (task-80: the built-in profile allows checkouts; no code exempts
/// Cargo).
pub fn ensure_reclaims_the_oldest_real_target_and_stops_at_min_free(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    two_targets(ctx, toolchain)?;
    let old = Path::new("/tmp/w/old/target");
    let freed = freed_by(old)?;
    let before = avail(Path::new("/tmp/w"))?;
    ensure!(freed > 64 << 10, "the old target frees only {freed} bytes");
    let min_k = (before + freed / 2).div_ceil(1024);
    let min = format!("{min_k}K");
    let locks = lock_inodes(old)?;
    let oracle = Oracle::arm(reclaimable(old)?)?;
    let run = ensure_run(
        ctx,
        &[
            "--min-free",
            &min,
            "--json",
            "--select",
            "profile:cargo-target",
            "--roots",
            "/tmp/w",
            "/tmp/w",
        ],
    )?;
    let judged = oracle.judge(&run)?;
    run.expect_code(0)?;
    let r = report(&run)?;
    ensure!(r["status"] == "met", "{r}");
    let actions = column(&r, "actions", "candidate");
    ensure!(
        actions == [&Value::from("/tmp/w/old/target")],
        "actions {actions:?}"
    );
    let delta = r["actions"][0]["avail_delta_bytes"]
        .as_i64()
        .ok_or_else(|| format!("the observed change is not a number: {r}"))?;
    info!(
        "{}: freed {} bytes by the harness's count, observed change {delta}, before {before}, \
         min-free {min}",
        toolchain.name, freed
    );
    ensure!(delta > 0, "the observed change is {delta}");
    ensure!(
        r["avail_after_bytes"].as_u64().unwrap_or(0) >= min_k * 1024,
        "{r}"
    );
    ensure!(lock_inodes(old)? == locks, "lock files gone or replaced");
    ensure!(
        !ctx.scratch.path(VENDORED_CHECKOUT).exists(),
        "the checkout in the old target was not reclaimed with it"
    );
    ensure!(
        Path::new("/tmp/w/new/target/debug")
            .join(SMOKE.binary)
            .exists(),
        "the new target was touched"
    );
    judged.assert_clean()?;
    build(ctx, toolchain, Path::new("/tmp/w/old"))
}

/// Exit 1, twice: (a) every target too recent for `filter:idle-1d` is
/// skipped `not_idle` and nothing is deleted; (b) without a filter,
/// everything is reclaimed and the goal (100% free) still not met. Both:
/// every eligible candidate processed without errors, still below
/// `--min-free`.
pub fn ensure_exits_1_when_every_candidate_was_processed_below_min_free(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    two_targets(ctx, toolchain)?;
    let run = |select: &str| {
        ensure_run(
            ctx,
            &[
                "--min-free",
                "100%",
                "--json",
                "--select",
                select,
                "--roots",
                "/tmp/w",
                "/tmp/w",
            ],
        )
    };
    let oracle = Oracle::arm(Fixture::new())?;
    let recent = run("profile:cargo-target && filter:idle-1d")?;
    let judged = oracle.judge(&recent)?;
    recent.expect_code(1)?;
    let r = report(&recent)?;
    ensure!(r["status"] == "exhausted", "{r}");
    let reasons = column(&r, "skipped", "reason");
    ensure!(
        reasons == [&Value::from("not_idle"), &Value::from("not_idle")],
        "{reasons:?}"
    );
    judged.assert_clean()?;

    let mut fixture = reclaimable(Path::new("/tmp/w/old/target"))?;
    for path in reclaimable_paths(Path::new("/tmp/w/new/target"))? {
        fixture = fixture.eligible(path);
    }
    let oracle = Oracle::arm(fixture)?;
    let all = run("profile:cargo-target")?;
    let judged = oracle.judge(&all)?;
    all.expect_code(1)?;
    let r = report(&all)?;
    ensure!(r["status"] == "exhausted", "{r}");
    let actions = column(&r, "actions", "candidate");
    ensure!(
        actions
            == [
                &Value::from("/tmp/w/old/target"),
                &Value::from("/tmp/w/new/target")
            ],
        "oldest first: {actions:?}"
    );
    ensure!(r["errors"].as_array().is_some_and(Vec::is_empty), "{r}");
    judged.assert_clean()
}

/// Every entry of every profile directory of `target` but its lock files.
fn reclaimable_paths(target: &Path) -> Result<Vec<std::path::PathBuf>, String> {
    let mut paths = Vec::new();
    for profile in profile_dirs(target)? {
        paths.extend(deletable(&profile)?);
    }
    Ok(paths)
}

/// `--dry-run` on real targets: it plans and deletes nothing, takes no
/// lock (the host lock file is removed, which a lock attempt would turn
/// into exit 2), and prints even with the goal met: exit 0, status
/// `dry_run`, the plan listing `old` eligible (sized) before `new`, which a
/// keep marker in its target makes `kept`. The oracle allows nothing to
/// go; the strace record holds no unlink.
pub fn ensure_dry_run_plans_real_targets_and_deletes_nothing(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    two_targets(ctx, toolchain)?;
    ctx.scratch
        .write_file("w/new/target/.diskgc-keep", b"", 0o644, TESTER)?;
    unprovision(ctx)?;
    let args = [
        "--dry-run",
        "--min-free",
        "1K",
        "--select",
        "profile:cargo-target",
        "--roots",
        "/tmp/w",
        "/tmp/w",
    ];
    let oracle = Oracle::arm(Fixture::new())?;
    let human = ensure_run(ctx, &args)?;
    let mut with_json = args.to_vec();
    with_json.insert(0, "--json");
    let json = ensure_run(ctx, &with_json)?;
    let judged = oracle.judge_runs(&[&human, &json])?;
    human.expect_code(0)?;
    ensure!(
        human.stdout.starts_with("dry run: ") && human.stdout.contains("/tmp/w/old/target"),
        "a dry run prints its plan on stdout, got {:?}",
        human.stdout
    );
    json.expect_code(0)?;
    let r = report(&json)?;
    ensure!(r["status"] == "dry_run", "{r}");
    let plan: Vec<(&Value, &Value)> = r["plan"]
        .as_array()
        .map(|p| p.iter().map(|e| (&e["candidate"], &e["reason"])).collect())
        .unwrap_or_default();
    ensure!(
        plan == [
            (&Value::from("/tmp/w/old/target"), &Value::Null),
            (&Value::from("/tmp/w/new/target"), &Value::from("kept"))
        ],
        "plan {plan:?}"
    );
    ensure!(
        r["plan"][0]["estimated_bytes"].as_u64().unwrap_or(0) > 0,
        "{r}"
    );
    ensure!(
        r["plan"][0]["eligible"] == true && r["plan"][1]["eligible"] == false,
        "{r}"
    );
    // The built-in cargo-target allows checkouts (task-80): every plan
    // entry says so.
    ensure!(
        r["plan"]
            .as_array()
            .is_some_and(|p| p.iter().all(|e| e["checkouts"] == "allow")),
        "{r}"
    );
    for run in [&human, &json] {
        let unlinks = run
            .events
            .iter()
            .filter(|e| matches!(e.syscall.as_str(), "unlink" | "unlinkat" | "rmdir"))
            .count();
        ensure!(unlinks == 0, "a dry run made {unlinks} unlink calls");
    }
    judged.assert_clean()
}
