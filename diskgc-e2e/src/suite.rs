//! The container tests and their per-test context.
//!
//! Every test runs in its own container, as container root, after the
//! interlock passed: [`Ctx::setup`] mounts a fresh scratch tmpfs at `/tmp`
//! and a private records tmpfs, the test builds its fixture through
//! [`Scratch`], runs the code under test as `tester` (uid 1000) under strace,
//! and judges the result with the deletion oracle.
//!
//! Tiers: `fast` tests run in `just e2e` (the commit gate, 5-minute budget);
//! `full` tests run only in `just e2e-full`.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Instant;

use crate::oracle::{Change, Fixture, Oracle};
use crate::probe::{ESCAPE_CASES, ESCAPE_TARGET_DIR, ESCAPED_NAME};
use crate::scratch::{self, Scratch};
use crate::under_test::{self, Run};
use crate::{OTHER, TESTER, UNDER_TEST_BIN_DIR, ensure, exit, info};

/// Which gate runs a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// `just e2e` and `just e2e-full`.
    Fast,
    /// `just e2e-full` only.
    Full,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Fast => "fast",
            Tier::Full => "full",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "fast" => Some(Tier::Fast),
            "full" => Some(Tier::Full),
            _ => None,
        }
    }
}

pub type TestResult = Result<(), String>;

/// One container test.
pub struct Test {
    pub name: &'static str,
    pub tier: Tier,
    pub run: fn(&Ctx) -> TestResult,
}

/// The registry. Names are stable: `just e2e-one NAME` uses them.
pub static TESTS: &[Test] = &[
    Test {
        name: "sandbox_trivial",
        tier: Tier::Fast,
        run: sandbox_trivial,
    },
    Test {
        name: "tester_cannot_read_other_users_mode000_dir",
        tier: Tier::Fast,
        run: tester_cannot_read_other_users_mode000_dir,
    },
    Test {
        name: "interlock_canary_passes_in_container",
        tier: Tier::Fast,
        run: interlock_canary_passes_in_container,
    },
    Test {
        name: "interlock_aborts_outside_scratch",
        tier: Tier::Fast,
        run: interlock_aborts_outside_scratch,
    },
    Test {
        name: "oracle_clean_when_only_eligible_objects_go",
        tier: Tier::Fast,
        run: oracle_clean_when_only_eligible_objects_go,
    },
    Test {
        name: "oracle_catches_deletion_outside_eligible_set",
        tier: Tier::Fast,
        run: oracle_catches_deletion_outside_eligible_set,
    },
    Test {
        name: "oracle_catches_replaced_objects",
        tier: Tier::Fast,
        run: oracle_catches_replaced_objects,
    },
    Test {
        name: "oracle_large_tree",
        tier: Tier::Full,
        run: oracle_large_tree,
    },
];

/// Prints `name<TAB>tier` per test (read by the runner).
pub fn list() -> i32 {
    for test in TESTS {
        println!("{}\t{}", test.name, test.tier.as_str());
    }
    exit::PASS
}

/// Sets up the context, runs test `name` and prints its result.
pub fn run(name: &str) -> i32 {
    let Some(test) = TESTS.iter().find(|t| t.name == name) else {
        eprintln!("diskgc-e2e: no test named {name:?}");
        return exit::USAGE;
    };
    let started = Instant::now();
    let result = Ctx::setup().and_then(|ctx| (test.run)(&ctx));
    let took = started.elapsed();
    match result {
        Ok(()) => {
            println!("PASS {name} ({took:.2?} in container)");
            exit::PASS
        }
        Err(why) => {
            println!("FAIL {name} ({took:.2?} in container): {why}");
            exit::FAIL
        }
    }
}

/// What every test gets: its scratch mount, the harness's records mount and
/// the binaries under test.
pub struct Ctx {
    pub scratch: Scratch,
    pub records: Scratch,
    pub bin_dir: PathBuf,
    pub self_exe: PathBuf,
}

impl Ctx {
    /// Mounts the records tmpfs (root only) and the scratch tmpfs at `/tmp`.
    pub fn setup() -> Result<Self, String> {
        let uid = rustix::process::getuid().as_raw();
        ensure!(
            uid == 0,
            "the harness must run as container root to build fixtures, runs as uid {uid}"
        );
        let records = Scratch::mount("/run/diskgc-e2e", "0700")?;
        let scratch = Scratch::mount("/tmp", "1777")?;
        let self_exe =
            std::env::current_exe().map_err(|e| format!("cannot find own executable: {e}"))?;
        Ok(Self {
            scratch,
            records,
            bin_dir: PathBuf::from(UNDER_TEST_BIN_DIR),
            self_exe,
        })
    }

    /// The diskgc binary under test.
    pub fn diskgc(&self) -> PathBuf {
        self.bin_dir.join("diskgc")
    }

    /// Runs `program args...` as `tester` under strace, from `/`.
    pub fn run_as_tester(&self, program: &Path, args: &[&str]) -> Result<Run, String> {
        under_test::run_as_tester(&self.records, Path::new("/"), program, args)
    }

    /// Runs a probe as the code under test (`tester`, traced).
    pub fn probe_as_tester(&self, args: &[&str]) -> Result<Run, String> {
        let mut full = vec!["probe"];
        full.extend_from_slice(args);
        self.run_as_tester(&self.self_exe, &full)
    }

    /// Runs a probe as container root (untraced), for interlock checks.
    pub fn probe_as_root(&self, args: &[&str]) -> Result<Output, String> {
        Command::new(&self.self_exe)
            .arg("probe")
            .args(args)
            .output()
            .map_err(|e| format!("spawn probe {args:?}: {e}"))
    }
}

/// Fails unless `path` has this file type, permission bits and owner.
fn expect_meta(path: &str, dir: bool, mode: u32, uid: u32, gid: u32) -> TestResult {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("lstat {path}: {e}"))?;
    ensure!(
        meta.is_dir() == dir,
        "{path}: is_dir {} expected {dir}",
        meta.is_dir()
    );
    ensure!(
        meta.mode() & 0o7777 == mode && meta.uid() == uid && meta.gid() == gid,
        "{path}: mode {:o} owner {}:{}, expected mode {mode:o} owner {uid}:{gid}",
        meta.mode() & 0o7777,
        meta.uid(),
        meta.gid()
    );
    Ok(())
}

fn to_str(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("non-UTF-8 path {}", path.display()))
}

/// The harness works end to end: host lock provisioned, scratch mounted,
/// diskgc runs as tester under strace, and the oracle sees no change.
fn sandbox_trivial(ctx: &Ctx) -> TestResult {
    // The lines of docs/safety.md (Host lock):
    //   d /run/diskgc      0755 root root -
    //   f /run/diskgc/lock 0644 root root -
    expect_meta("/run/diskgc", true, 0o755, 0, 0)?;
    expect_meta("/run/diskgc/lock", false, 0o644, 0, 0)?;

    let root_mount = scratch::mount_id(Path::new("/"), false)?;
    ensure!(
        ctx.scratch.mnt_id() != root_mount,
        "scratch /tmp is on the root mount"
    );
    ctx.scratch.write_file("hello", b"hello\n", 0o644, TESTER)?;

    let oracle = Oracle::arm(Fixture::new())?;
    let run = ctx.run_as_tester(&ctx.diskgc(), &["--version"])?;
    run.expect_code(0)?;
    ensure!(
        run.stdout.starts_with("diskgc "),
        "unexpected diskgc --version output {:?}",
        run.stdout
    );
    oracle.judge(&run)?.assert_clean()
}

/// Parses the whitespace-separated values of a `Name:` line of
/// /proc/<pid>/status as printed by the whoami probe.
fn status_values<'a>(status: &'a str, name: &str) -> Result<Vec<&'a str>, String> {
    status
        .lines()
        .find_map(|l| l.strip_prefix(name))
        .map(|rest| rest.split_whitespace().collect())
        .ok_or_else(|| format!("no {name} line in {status:?}"))
}

/// The code under test runs as uid 1000 without capabilities or root's
/// groups, and cannot read a mode-000 directory of another mapped uid.
fn tester_cannot_read_other_users_mode000_dir(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("other-private", 0o700, OTHER)?;
    s.write_file("other-private/secret", b"secret\n", 0o600, OTHER)?;
    s.chmod("other-private", 0o000)?;
    s.mkdir("tester-own", 0o700, TESTER)?;
    s.write_file("tester-own/mine", b"mine\n", 0o600, TESTER)?;
    expect_meta(
        to_str(&s.path("other-private"))?,
        true,
        0o000,
        OTHER.uid,
        OTHER.gid,
    )?;

    let who = ctx.probe_as_tester(&["whoami"])?;
    who.expect_code(0)?;
    info!("code under test credentials:\n{}", who.stdout);
    ensure!(
        status_values(&who.stdout, "Uid:")? == ["1000"; 4],
        "code under test is not uid 1000 (real, effective, saved, fs): {}",
        who.stdout
    );
    ensure!(
        status_values(&who.stdout, "Gid:")? == ["1000"; 4],
        "code under test is not gid 1000: {}",
        who.stdout
    );
    ensure!(
        status_values(&who.stdout, "Groups:")? == ["1000"],
        "code under test kept supplementary groups other than tester's: {}",
        who.stdout
    );
    for caps in ["CapEff:", "CapPrm:"] {
        ensure!(
            status_values(&who.stdout, caps)? == ["0000000000000000"],
            "code under test has capabilities ({caps}): {}",
            who.stdout
        );
    }

    let other = s.path("other-private");
    let denied = ctx.probe_as_tester(&["read-dir", to_str(&other)?])?;
    denied.expect_code(0)?;
    ensure!(
        denied.stdout.starts_with("err 13 "),
        "tester read {} (mode 000, owner uid {}): {:?}, expected EACCES (13)",
        other.display(),
        OTHER.uid,
        denied.stdout
    );

    // Controls: tester reads its own private dir, and container root (with
    // CAP_DAC_OVERRIDE over mapped uids) reads the other user's dir, so the
    // denial above comes from tester's identity, not from the mount.
    let own = ctx.probe_as_tester(&["read-dir", to_str(&s.path("tester-own"))?])?;
    ensure!(
        own.stdout.trim() == "ok 1",
        "tester cannot read its own dir: {:?}",
        own.stdout
    );
    let as_root = std::fs::read_dir(&other)
        .map_err(|e| format!("root cannot read {}: {e}", other.display()))?
        .count();
    ensure!(
        as_root == 1,
        "root sees {as_root} entries in {}",
        other.display()
    );
    Ok(())
}

/// Positive control of the interlock canary: inside the container it runs.
/// (Its refusal outside a container is checked by the runner on the host
/// before every run.)
fn interlock_canary_passes_in_container(ctx: &Ctx) -> TestResult {
    let canary = ctx.bin_dir.join("diskgc-e2e-canary");
    let out = Command::new(&canary)
        .output()
        .map_err(|e| format!("spawn {}: {e}", canary.display()))?;
    ensure!(
        out.status.code() == Some(exit::PASS),
        "canary exited with {:?} inside the container: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    ensure!(
        String::from_utf8_lossy(&out.stdout).contains("interlock passed"),
        "canary output: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    Ok(())
}

/// Interlock layer 3 bites: a fixture operation aimed outside the scratch
/// mount (absolute path, `..`, a symlink out, a bind mount of the scratch
/// filesystem itself) aborts the process before touching anything.
fn interlock_aborts_outside_scratch(ctx: &Ctx) -> TestResult {
    for case in ESCAPE_CASES {
        let out = ctx.probe_as_root(&["escape", case])?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        info!(
            "escape case {case}: exit {:?}; {}",
            out.status.code(),
            stderr.trim()
        );
        if *case == "control" {
            ensure!(
                out.status.code() == Some(exit::PASS) && stdout.contains("NOT ABORTED"),
                "control case (inside scratch) did not run: {:?} {stdout} {stderr}",
                out.status.code()
            );
        } else {
            ensure!(
                out.status.code() == Some(exit::INTERLOCK) && stderr.contains("INTERLOCK"),
                "escape case {case} was not aborted by the interlock: exit {:?}\n{stdout}{stderr}",
                out.status.code()
            );
        }
    }
    let escaped = Path::new(ESCAPE_TARGET_DIR).join(ESCAPED_NAME);
    ensure!(!escaped.exists(), "{} was created", escaped.display());
    let through_bind = ctx.scratch.path("bind-src").join(ESCAPED_NAME);
    ensure!(
        !through_bind.exists(),
        "{} was created",
        through_bind.display()
    );
    let control = ctx.scratch.path("control").join(ESCAPED_NAME);
    ensure!(
        control.exists(),
        "control file {} missing",
        control.display()
    );
    Ok(())
}

/// Fixture shared by the oracle tests: a candidate tree (eligible), a
/// sibling that is not, and a concurrent writer's directory.
fn candidate_fixture(ctx: &Ctx) -> Result<Fixture, String> {
    let s = &ctx.scratch;
    s.mkdirs("cand/sub", 0o755, TESTER)?;
    s.write_file("cand/a", b"a", 0o644, TESTER)?;
    s.write_file("cand/sub/b", b"b", 0o644, TESTER)?;
    s.mkdir("keep", 0o755, TESTER)?;
    s.write_file("keep/file", b"keep", 0o644, TESTER)?;
    s.mkdir("writer", 0o755, TESTER)?;
    s.write_file("writer/log", b"log", 0o644, TESTER)?;
    Ok(Fixture::new()
        .eligible(s.path("cand"))
        .concurrent_writer(s.path("writer")))
}

/// Control for the two tests below: deleting only eligible and allowlisted
/// objects (and creating new ones) leaves the verdict clean.
fn oracle_clean_when_only_eligible_objects_go(ctx: &Ctx) -> TestResult {
    let fixture = candidate_fixture(ctx)?;
    let oracle = Oracle::arm(fixture)?;
    let s = &ctx.scratch;
    let run = ctx.probe_as_tester(&[
        "remove",
        to_str(&s.path("cand"))?,
        to_str(&s.path("writer/log"))?,
    ])?;
    run.expect_code(0)?;
    let verdict = oracle.judge(&run)?;
    ensure!(
        run.events.iter().filter(|e| e.took_effect()).count() >= 4,
        "strace recorded too few removals: {:?}",
        run.events
    );
    verdict.assert_clean()
}

/// Deliberately wrong fixture: the code under test also deletes an object
/// on the scratch mount and one on the image's root filesystem, neither
/// eligible. Both views of the oracle must flag exactly those two.
fn oracle_catches_deletion_outside_eligible_set(ctx: &Ctx) -> TestResult {
    let fixture = candidate_fixture(ctx)?;
    let s = &ctx.scratch;
    // Baked into the image, owned by tester, outside the scratch mount.
    let outside = PathBuf::from("/home/tester/.profile");
    let oracle = Oracle::arm(fixture)?;
    let run = ctx.probe_as_tester(&[
        "remove",
        to_str(&s.path("cand"))?,
        to_str(&s.path("keep/file"))?,
        to_str(&s.path("writer/log"))?,
        to_str(&outside)?,
    ])?;
    run.expect_code(0)?;
    let verdict = oracle.judge(&run)?;

    let expected = [s.path("keep/file"), outside].into_iter().collect();
    ensure!(
        verdict.snapshot_paths() == expected,
        "snapshot flagged {:?}, expected {expected:?}\n{}",
        verdict.snapshot_paths(),
        verdict.report()
    );
    ensure!(
        verdict
            .snapshot
            .iter()
            .all(|v| matches!(v.change, Change::Deleted { .. })),
        "expected deletions only:\n{}",
        verdict.report()
    );
    ensure!(
        verdict.traced_paths() == expected,
        "strace record flagged {:?}, snapshot flagged {expected:?}: they disagree\n{}",
        verdict.traced_paths(),
        verdict.report()
    );
    ensure!(verdict.assert_clean().is_err(), "verdict should fail");
    info!("oracle caught, as intended:\n{}", verdict.report());
    Ok(())
}

/// Deliberately wrong fixture: the code under test replaces two objects it
/// was not allowed to touch, keeping their paths: a `.cargo-lock` unlinked
/// and recreated (what Cargo does), and a file renamed over. Both views must
/// flag both paths as replaced.
fn oracle_catches_replaced_objects(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdirs("proj/target/debug/deps", 0o755, TESTER)?;
    s.write_file("proj/target/debug/deps/libx.rlib", b"rlib", 0o644, TESTER)?;
    s.write_file("proj/target/debug/.cargo-lock", b"", 0o644, TESTER)?;
    s.write_file("proj/Cargo.lock", b"version = 4\n", 0o644, TESTER)?;
    let lock = s.path("proj/target/debug/.cargo-lock");
    let manifest_lock = s.path("proj/Cargo.lock");
    let oracle = Oracle::arm(Fixture::new().eligible(s.path("proj/target/debug/deps")))?;

    let recreate = ctx.probe_as_tester(&["recreate", to_str(&lock)?])?;
    recreate.expect_code(0)?;
    let rename = ctx.probe_as_tester(&["rename-over", to_str(&manifest_lock)?])?;
    rename.expect_code(0)?;
    let verdict = oracle.judge_runs(&[&recreate, &rename])?;

    let expected = [lock, manifest_lock].into_iter().collect();
    ensure!(
        verdict.snapshot_paths() == expected,
        "snapshot flagged {:?}, expected {expected:?}\n{}",
        verdict.snapshot_paths(),
        verdict.report()
    );
    ensure!(
        verdict
            .snapshot
            .iter()
            .all(|v| matches!(v.change, Change::Replaced { .. })),
        "expected replacements only:\n{}",
        verdict.report()
    );
    ensure!(
        verdict.traced_paths() == expected,
        "strace record flagged {:?}, snapshot flagged {expected:?}: they disagree\n{}",
        verdict.traced_paths(),
        verdict.report()
    );
    info!("oracle caught, as intended:\n{}", verdict.report());
    Ok(())
}

/// The oracle at the scale of the 100,000-entry `/tmp` scenarios: it stays
/// clean when half of a large tree (all eligible) is deleted, and reports
/// what the snapshots cost.
fn oracle_large_tree(ctx: &Ctx) -> TestResult {
    const DIRS: usize = 100;
    const FILES_PER_DIR: usize = 1000;
    let s = &ctx.scratch;
    let built = Instant::now();
    let mut fixture = Fixture::new();
    let mut eligible = Vec::new();
    s.mkdir("big", 0o755, TESTER)?;
    for d in 0..DIRS {
        let dir = format!("big/d{d:03}");
        s.mkdir(&dir, 0o755, TESTER)?;
        for f in 0..FILES_PER_DIR {
            s.write_file(&format!("{dir}/f{f:04}"), b"", 0o644, TESTER)?;
        }
        if d % 2 == 0 {
            fixture = fixture.eligible(s.path(&dir));
            eligible.push(s.path(&dir).display().to_string());
        }
    }
    info!(
        "built {} files in {:?}",
        DIRS * FILES_PER_DIR,
        built.elapsed()
    );
    let oracle = Oracle::arm(fixture)?;
    let mut args = vec!["remove"];
    args.extend(eligible.iter().map(String::as_str));
    let run = ctx.probe_as_tester(&args)?;
    run.expect_code(0)?;
    let removed = run.events.iter().filter(|e| e.took_effect()).count();
    ensure!(
        removed >= DIRS / 2 * (FILES_PER_DIR + 1),
        "strace recorded {removed} removals"
    );
    oracle.judge(&run)?.assert_clean()
}
