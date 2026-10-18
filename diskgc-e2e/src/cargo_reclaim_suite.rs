//! Container tests of the `cargo-target` reclaim protocol (task-53;
//! docs/safety.md, `cargo-target`: Reclaim protocol) on targets that real
//! Cargo builds made, through the test driver's `cargo-reclaim`
//! (diskgc-core `cargo_target::reclaim` over `LinuxHost`), run as `tester`
//! under strace and judged by the deletion oracle, whose eligible set the
//! harness computes from the tree (every entry of every profile directory
//! but its lock files), never from diskgc's output. Each test runs once
//! per Cargo toolchain under test ([`crate::cargo_suite::CargoTest`]): the
//! classic layout on 1.90.0 and 1.99.1, the per-unit layout on the
//! nightly.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::cargo_under_test::{self, Cargo, Gate, RACE, REACH_TIMEOUT, SMOKE, Toolchain};
use crate::controlled::ControlLine;
use crate::holder::Holder;
use crate::oracle::{Fixture, Oracle};
use crate::proc_locks::{self, Watched};
use crate::suite::{Ctx, TestResult};
use crate::under_test::Run;
use crate::{TESTER, ensure, info};

/// SIGKILL's number on Linux.
const SIGKILL: i32 = 9;

/// The host's target triple (the container is x86_64; each toolchain
/// carries the host's rust-std).
const HOST_TRIPLE: &str = "x86_64-unknown-linux-gnu";

/// The driver's result lines (control lines left out), without their index.
fn results(run: &Run) -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    for line in run.stdout.lines() {
        if ControlLine::parse(line)?.is_none() {
            let (_, text) = line
                .split_once(' ')
                .ok_or_else(|| format!("a result line without an index: {line:?}"))?;
            lines.push(text.to_owned());
        }
    }
    Ok(lines)
}

/// The verdict line that follows the `target` line of `target`.
fn verdict(lines: &[String], target: &Path) -> Result<String, String> {
    let quoted = format!("{target:?}");
    let at = lines
        .iter()
        .position(|l| l.starts_with("target ") && l.split(' ').nth(1) == Some(&quoted))
        .ok_or_else(|| format!("{} was not found: {lines:#?}", target.display()))?;
    lines
        .get(at + 1)
        .cloned()
        .ok_or_else(|| format!("no verdict after {}", lines[at]))
}

/// Runs `cargo-reclaim ARGS` as tester under strace (driver flags such as
/// `--kill-at` go in `driver_flags`), already judged by nobody: the caller
/// arms the oracle first and judges the run.
fn cargo_reclaim(ctx: &Ctx, driver_flags: &[&str], args: &[&str]) -> Result<Run, String> {
    let mut full: Vec<&str> = driver_flags.to_vec();
    full.push("cargo-reclaim");
    full.extend_from_slice(args);
    let run = ctx.run_as_tester(&ctx.driver(), &full)?;
    info!(
        "cargo-reclaim {}:\n{}\n--- stderr\n{}",
        full.join(" "),
        run.stdout,
        run.stderr
    );
    Ok(run)
}

fn is_lock_name(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    name.starts_with(".cargo-") && name.contains("lock")
}

/// The profile directories of the target at `target`, by the harness's own
/// reading: `<t>/<p>/` and `<t>/<x>/<p>/` holding a `.cargo-*lock*` file.
fn profile_dirs(target: &Path) -> Result<Vec<PathBuf>, String> {
    let mut found = Vec::new();
    let subdirs = |dir: &Path| -> Result<Vec<PathBuf>, String> {
        let mut dirs = Vec::new();
        for entry in std::fs::read_dir(dir).map_err(|e| format!("read_dir {dir:?}: {e}"))? {
            let entry = entry.map_err(|e| format!("read_dir {dir:?}: {e}"))?;
            if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
                dirs.push(entry.path());
            }
        }
        dirs.sort();
        Ok(dirs)
    };
    for dir in subdirs(target)? {
        if !cargo_under_test::lock_files(&dir)?.is_empty() {
            found.push(dir);
            continue;
        }
        for inner in subdirs(&dir)? {
            if !cargo_under_test::lock_files(&inner)?.is_empty() {
                found.push(inner);
            }
        }
    }
    Ok(found)
}

/// Every entry of the profile directory `dir` but its lock files: what the
/// protocol may delete (each with everything below it).
fn deletable(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| format!("read_dir {dir:?}: {e}"))? {
        let entry = entry.map_err(|e| format!("read_dir {dir:?}: {e}"))?;
        if !is_lock_name(&entry.file_name()) {
            found.push(entry.path());
        }
    }
    found.sort();
    Ok(found)
}

/// `path` and every object below it (symlinks not followed).
fn objects_below(path: &Path) -> Result<Vec<PathBuf>, String> {
    let mut found = Vec::new();
    let mut todo = vec![path.to_owned()];
    while let Some(at) = todo.pop() {
        let meta = std::fs::symlink_metadata(&at).map_err(|e| format!("lstat {at:?}: {e}"))?;
        if meta.is_dir() {
            for entry in std::fs::read_dir(&at).map_err(|e| format!("read_dir {at:?}: {e}"))? {
                todo.push(entry.map_err(|e| e.to_string())?.path());
            }
        }
        found.push(at);
    }
    Ok(found)
}

/// Whether `path` (below a profile directory) is a fingerprint, in either
/// layout: inside `.fingerprint/`, or `build/<package>/<hash>/fingerprint`
/// and below.
fn is_fingerprint(profile: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(profile) else {
        return false;
    };
    let names: Vec<&OsStr> = rel.components().map(|c| c.as_os_str()).collect();
    names.first() == Some(&OsStr::new(".fingerprint"))
        || (names.len() >= 4 && names[0] == "build" && names[3] == "fingerprint")
}

/// Every fingerprint object in the profile directory `profile`, by the
/// harness's walk.
fn fingerprints(profile: &Path) -> Result<Vec<PathBuf>, String> {
    let mut found = Vec::new();
    for entry in deletable(profile)? {
        for object in objects_below(&entry)? {
            if is_fingerprint(profile, &object) {
                found.push(object);
            }
        }
    }
    Ok(found)
}

/// The lock files of the target's profile directories, by inode.
fn lock_inodes(target: &Path) -> Result<BTreeMap<PathBuf, u64>, String> {
    let mut found = BTreeMap::new();
    for profile in profile_dirs(target)? {
        for lock in cargo_under_test::lock_files(&profile)? {
            let ino = std::fs::symlink_metadata(&lock)
                .map_err(|e| format!("lstat {lock:?}: {e}"))?
                .ino();
            found.insert(lock, ino);
        }
    }
    Ok(found)
}

/// The oracle's fixture of a reclaim of `target`: every entry of every
/// profile directory but the lock files may go.
fn reclaimable(target: &Path) -> Result<Fixture, String> {
    let mut fixture = Fixture::new();
    for profile in profile_dirs(target)? {
        for entry in deletable(&profile)? {
            fixture = fixture.eligible(entry);
        }
    }
    Ok(fixture)
}

/// The layout the harness reads in a profile directory (docs/safety.md,
/// Layouts): `classic` (a directory `.fingerprint/` or `deps/`), `per-unit`
/// (neither, a directory `build/`), `empty` (lock files only), or
/// `unknown`.
fn layout_of(profile: &Path) -> Result<&'static str, String> {
    let dir = |name: &str| profile.join(name).is_dir() && !profile.join(name).is_symlink();
    Ok(if dir(".fingerprint") || dir("deps") {
        "classic"
    } else if dir("build") {
        "per-unit"
    } else if deletable(profile)?.is_empty() {
        "empty"
    } else {
        "unknown"
    })
}

/// The paths the run's `unlinkat` calls removed, in the order made.
fn unlinked(run: &Run) -> Vec<PathBuf> {
    run.events
        .iter()
        .filter(|e| e.syscall == "unlinkat" && e.took_effect())
        .flat_map(|e| e.removed.iter().cloned())
        .collect()
}

fn build(ctx: &Ctx, toolchain: &Toolchain, project: &Path, args: &[&str]) -> TestResult {
    Cargo::new(toolchain, project, args)
        .output(ctx)?
        .expect_success()?;
    Ok(())
}

/// After a reclaim (complete or not): `cargo build PROFILE...` succeeds
/// and the binary `cargo run PROFILE...` builds runs; with `recompiles`
/// (every fingerprint of that profile was gone), it compiled the package
/// again. `profile`: no arguments for debug, `--release` for release.
fn rebuilds(
    ctx: &Ctx,
    toolchain: &Toolchain,
    project: &Path,
    profile: &[&str],
    recompiles: bool,
    what: &str,
) -> TestResult {
    let with = |command: &'static str| -> Vec<&str> {
        std::iter::once(command)
            .chain(profile.iter().copied())
            .collect()
    };
    let built = Cargo::new(toolchain, project, &with("build")).output(ctx)?;
    built.expect_success()?;
    ensure!(
        !recompiles || built.stderr.contains("Compiling diskgc-e2e-race"),
        "{what} ({profile:?}): the next build did not compile the package again (a \
         fingerprint survived?):\n{}",
        built.stderr
    );
    let ran = Cargo::new(toolchain, project, &with("run")).output(ctx)?;
    ran.expect_success()?;
    ensure!(
        ran.stdout.contains(RACE.output),
        "{what} ({profile:?}): the rebuilt binary printed {:?}",
        ran.stdout
    );
    Ok(())
}

fn path_str(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("non-UTF-8 path {}", path.display()))
}

/// AC #1 and AC #2, and the order of step 3, on targets real builds made
/// (`build`, `test --no-run`, `build --release`, `build --target`): the
/// protocol deletes every entry of every profile directory but the lock
/// files; the oracle (snapshot by inode and birth time, and the strace
/// record) finds no lock file unlinked or replaced and nothing outside the
/// profile directories touched: not `CACHEDIR.TAG`, `.rustc_info.json`,
/// `doc/`, `tmp/`, `criterion/` or the triple directory. The strace record
/// shows every fingerprint unlinked before any other entry, and `build/`
/// or `deps/` last in each profile; the next build compiles again and
/// runs.
pub fn cargo_reclaim_deletes_only_inside_locked_profile_dirs(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("w", 0o755, TESTER)?;
    let p = RACE.write(s, "w/p")?;
    for args in [
        &["build"][..],
        &["test", "--no-run"],
        &["build", "--release"],
        &["build", "--target", HOST_TRIPLE],
    ] {
        build(ctx, toolchain, &p, args)?;
    }
    let target = p.join("target");
    for (dir, file) in [
        ("w/p/target/doc/p", "index.html"),
        ("w/p/target/tmp/t", "scratch"),
        ("w/p/target/criterion/base", "estimates.json"),
    ] {
        s.mkdirs(dir, 0o755, TESTER)?;
        s.write_file(&format!("{dir}/{file}"), b"data", 0o644, TESTER)?;
    }
    let profiles = profile_dirs(&target)?;
    ensure!(
        profiles.len() >= 3,
        "expected debug, release and the triple's debug: {profiles:?}"
    );
    let locks = lock_inodes(&target)?;
    let oracle = Oracle::arm(reclaimable(&target)?)?;
    let run = cargo_reclaim(ctx, &[], &["/tmp/w"])?;
    let judged = oracle.judge(&run)?;
    run.expect_code(0)?;
    let lines = results(&run)?;
    let got = verdict(&lines, &target)?;
    ensure!(
        got.starts_with(&format!("reclaimed {target:?} ")),
        "expected the target reclaimed: {got}"
    );
    info!(
        "{}: locks held {} µs while walking, checking and deleting",
        toolchain.name,
        got.rsplit("held_us=").next().unwrap_or("?")
    );
    // AC #1 and AC #2: judged by the oracle (snapshot and strace), from the
    // fixture's eligible set only.
    judged.assert_clean()?;
    ensure!(
        lock_inodes(&target)? == locks,
        "lock files gone or replaced: before {locks:?}"
    );
    for kept in [
        "CACHEDIR.TAG",
        "doc/p/index.html",
        "tmp/t/scratch",
        "criterion/base/estimates.json",
        HOST_TRIPLE,
    ] {
        ensure!(target.join(kept).exists(), "{kept} was touched");
    }
    for profile in &profiles {
        let left = deletable(profile)?;
        ensure!(
            left.is_empty(),
            "{} still holds {left:?}",
            profile.display()
        );
    }

    // Step 3's order, from the strace record: every fingerprint before any
    // other entry, and in each profile the layout's directories last.
    let order = unlinked(&run);
    let in_profile = |path: &Path| profiles.iter().find(|p| path.starts_with(p));
    let first_other = order
        .iter()
        .position(|path| !in_profile(path).is_some_and(|p| is_fingerprint(p, path)))
        .ok_or("nothing but fingerprints was unlinked")?;
    ensure!(
        first_other > 0,
        "the first unlink was no fingerprint: {:?}",
        order[0]
    );
    if let Some(late) = order[first_other..]
        .iter()
        .find(|path| in_profile(path).is_some_and(|p| is_fingerprint(p, path)))
    {
        return Err(format!(
            "a fingerprint ({}) was unlinked after another entry ({})",
            late.display(),
            order[first_other].display()
        ));
    }
    let marker = if toolchain.name.starts_with("nightly") {
        "build"
    } else {
        "deps"
    };
    for profile in &profiles {
        let last_top = order
            .iter()
            .rfind(|path| path.parent() == Some(profile.as_path()))
            .ok_or_else(|| format!("nothing unlinked in {}", profile.display()))?;
        ensure!(
            last_top.file_name() == Some(OsStr::new(marker)),
            "{}: the last entry deleted is {}, not {marker}/",
            profile.display(),
            last_top.display()
        );
    }
    rebuilds(ctx, toolchain, &p, &[], true, "after a complete reclaim")
}

/// Step 1: a target whose lock another process holds is `locked`, and
/// nothing is deleted: a real `cargo build` paused in its build script
/// (it holds every lock of the profile, `.cargo-lock` shared on 1.99.1 and
/// the nightly), and the lock-holder helper holding `.cargo-lock`
/// exclusively or shared. The build then succeeds.
pub fn cargo_reclaim_skips_a_target_whose_lock_is_held(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("w", 0o755, TESTER)?;
    let p = RACE.write(s, "w/p")?;
    build(ctx, toolchain, &p, &["build"])?;
    let target = p.join("target");
    let debug = target.join("debug");
    let watched = Watched::files(&cargo_under_test::lock_files(&debug)?)?;
    let expect_locked = |case: &str| -> TestResult {
        let oracle = Oracle::arm(Fixture::new())?;
        let run = cargo_reclaim(ctx, &[], &["/tmp/w"])?;
        let judged = oracle.judge(&run)?;
        run.expect_code(0)?;
        let got = verdict(&results(&run)?, &target)?;
        ensure!(
            got.starts_with(&format!("skip locked {target:?} entries=0 ")),
            "{case}: expected locked, got {got}"
        );
        info!("{case}: locked as expected");
        judged.assert_clean()
    };

    let gate = Gate::new(&ctx.scratch, "build")?;
    let cargo = Cargo::new(toolchain, &p, &["build"])
        .build_gate(&gate)
        .spawn(ctx)?;
    gate.wait_until_waiting(REACH_TIMEOUT)?;
    // The paused build holds the profile's locks (checked in /proc/locks,
    // not by what Cargo prints).
    let usage = watched.usage(&proc_locks::sample()?, cargo.pid());
    ensure!(
        usage.holds(&debug.join(".cargo-lock")),
        "the paused build does not hold .cargo-lock: {usage:?}"
    );
    expect_locked("a cargo build paused in its build script")?;
    gate.open(&ctx.scratch)?;
    cargo.wait()?.expect_success()?;

    let cargo_lock = debug.join(".cargo-lock");
    for shared in [false, true] {
        let mut args = vec!["flock-hold"];
        if shared {
            args.push("--shared");
        }
        args.push(path_str(&cargo_lock)?);
        let holder = Holder::tester(ctx, &args, &[])?;
        expect_locked(if shared {
            ".cargo-lock held shared"
        } else {
            ".cargo-lock held exclusively"
        })?;
        holder.finish()?;
    }
    build(ctx, toolchain, &p, &["build"])
}

/// AC #3 on real Cargo: the driver paused right before its first lock
/// (`flock_try:1`, after discovery), while (a) a `cargo build --release`
/// creates a new profile directory, or (b) a rebuild after an edit makes a
/// newer build than the one the eligibility check judged
/// (`--after-eligibility`): resumed, the target is `changed` and nothing
/// is deleted (no unlink at all in the strace record; the lock files keep
/// their inodes), and the builds succeed. The control: with nothing in
/// between, the same `--after-eligibility` run reclaims the target.
pub fn cargo_reclaim_changed_between_discovery_and_locking(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("w", 0o755, TESTER)?;
    let p = RACE.write(s, "w/p")?;
    build(ctx, toolchain, &p, &["build"])?;
    let target = p.join("target");
    let cases: [(&str, &[&str], &str); 2] = [
        ("a new profile directory", &[], "release"),
        ("a newer build", &["--after-eligibility"], "debug"),
    ];
    for (case, flags, written) in cases {
        let locks = lock_inodes(&target)?;
        // Cargo's own writes, which happen while diskgc is paused, are the
        // fixture's concurrent writers; diskgc's unlinks are checked from
        // the strace record below.
        let oracle = Oracle::arm(
            Fixture::new()
                .concurrent_writer(target.join(written))
                .concurrent_writer(target.join(".rustc_info.json")),
        )?;
        let mut args = vec!["--pause-at", "flock_try:1", "cargo-reclaim"];
        args.extend_from_slice(flags);
        args.push("/tmp/w");
        let mut spawned = ctx.spawn_as_tester(&ctx.driver(), &args)?;
        let paused = spawned.wait_paused()?;
        ensure!(paused.op == "flock_try" && paused.nth == 1, "{paused:?}");
        if written == "release" {
            build(ctx, toolchain, &p, &["build", "--release"])?;
        } else {
            let main = p.join("src/main.rs");
            let text = std::fs::read_to_string(&main).map_err(|e| e.to_string())?;
            std::fs::write(&main, format!("{text}\n// edit\n")).map_err(|e| e.to_string())?;
            build(ctx, toolchain, &p, &["build"])?;
        }
        spawned.resume(&paused)?;
        let run = spawned.wait()?;
        let judged = oracle.judge(&run)?;
        info!("{case}:\n{}\n--- stderr\n{}", run.stdout, run.stderr);
        run.expect_code(0)?;
        let got = verdict(&results(&run)?, &target)?;
        ensure!(
            got.starts_with(&format!("skip changed {target:?} entries=0 ")),
            "{case}: expected changed, got {got}"
        );
        ensure!(
            unlinked(&run).is_empty(),
            "{case}: the driver unlinked {:?}",
            unlinked(&run)
        );
        let now = lock_inodes(&target)?;
        for (lock, ino) in &locks {
            ensure!(
                now.get(lock) == Some(ino),
                "{case}: {} gone or replaced",
                lock.display()
            );
        }
        judged.assert_clean()?;
    }
    // The control: judged and reclaimed with nothing in between, the same
    // run deletes (the eligibility check and the locks change no time the
    // last build measures).
    let oracle = Oracle::arm(reclaimable(&target)?)?;
    let run = cargo_reclaim(ctx, &[], &["--after-eligibility", "/tmp/w"])?;
    let judged = oracle.judge(&run)?;
    run.expect_code(0)?;
    let got = verdict(&results(&run)?, &target)?;
    ensure!(
        got.starts_with(&format!("reclaimed {target:?} ")),
        "the control: expected the target reclaimed, got {got}"
    );
    judged.assert_clean()?;
    rebuilds(ctx, toolchain, &p, &[], true, "after the control")
}

/// Race test 6's shape at three chosen points, on a target with a debug
/// and a release profile: the driver SIGKILLed before its second unlink
/// (inside the fingerprints), before the first unlink after every
/// fingerprint of both profiles (counted by the harness), and before its
/// last unlink (the layout's directory). Each time no lock file is unlinked or
/// replaced and nothing outside the profile directories' entries goes (the
/// oracle); past the fingerprints none is left; the profile directory is
/// still in a known layout (or holds only lock files); the next build
/// succeeds and its binary runs, and once the fingerprints are gone it
/// compiled the package again.
pub fn cargo_reclaim_killed_leaves_lock_files_and_a_recognisable_profile(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("w", 0o755, TESTER)?;
    let p = RACE.write(s, "w/p")?;
    // Two profile directories: the fingerprints of both go before anything
    // else of either (in one classic profile, name order alone would put
    // `.fingerprint/` first).
    let builds: [&[&str]; 2] = [&["build"], &["build", "--release"]];
    for args in builds {
        build(ctx, toolchain, &p, args)?;
    }
    let target = p.join("target");
    for point in ["inside-fingerprints", "after-fingerprints", "last"] {
        let profiles = profile_dirs(&target)?;
        ensure!(profiles.len() == 2, "debug and release: {profiles:?}");
        let mut fingerprint_objects = 0;
        let mut all = 0;
        for profile in &profiles {
            fingerprint_objects += fingerprints(profile)?.len();
            for entry in deletable(profile)? {
                all += objects_below(&entry)?.len();
            }
        }
        let k = match point {
            "inside-fingerprints" => 2,
            "after-fingerprints" => fingerprint_objects + 1,
            _ => all,
        };
        ensure!(
            fingerprint_objects > 0 && k <= all,
            "{point}: k {k} of {all}"
        );
        let locks = lock_inodes(&target)?;
        let oracle = Oracle::arm(reclaimable(&target)?)?;
        let at = format!("unlinkat:{k}");
        let run = cargo_reclaim(ctx, &["--kill-at", &at], &["/tmp/w"])?;
        let judged = oracle.judge(&run)?;
        ensure!(
            run.signal == Some(SIGKILL),
            "{point}: the driver was not killed at {at}: {:?} {:?}",
            run.code,
            run.signal
        );
        judged.assert_clean()?;
        ensure!(
            lock_inodes(&target)? == locks,
            "{point}: lock files gone or replaced"
        );
        let gone = unlinked(&run);
        ensure!(
            gone.len() == k - 1,
            "{point}: {} unlinks before the kill, want {}",
            gone.len(),
            k - 1
        );
        let mut layouts = Vec::new();
        for profile in &profiles {
            if point != "inside-fingerprints" {
                let left = fingerprints(profile)?;
                ensure!(
                    left.is_empty(),
                    "{point}: fingerprints left after other entries started to go: {left:?}"
                );
            }
            let layout = layout_of(profile)?;
            ensure!(
                layout != "unknown",
                "{point}: {} is in no known layout: {:?}",
                profile.display(),
                deletable(profile)?
            );
            layouts.push(layout);
        }
        info!(
            "{point} (killed at {at} of {all}, {fingerprint_objects} fingerprint objects): \
             layouts {layouts:?}, {} unlinked",
            gone.len()
        );
        // Once past the fingerprints, both profiles had lost all of
        // theirs, so both compile again.
        let recompiles = point != "inside-fingerprints";
        for profile in [&[][..], &["--release"]] {
            rebuilds(ctx, toolchain, &p, profile, recompiles, point)?;
        }
    }
    Ok(())
}

/// mped H6 (c): how long the protocol holds a target's locks (every Cargo
/// of the target waits that long) while it walks, checks and deletes a
/// large target: a real build of the smoke fixture plus 100,000 files in
/// `deps/`, on the fast toolchain. The time is reported (`held_us`); the
/// test fails only on a hang (over 120 s) or a reclaim that does not
/// complete. Under strace, which stops the driver at every unlink, so the
/// time is an upper bound of an untraced run.
pub fn cargo_reclaim_lock_hold_time_on_a_large_target(ctx: &Ctx) -> TestResult {
    const FILES: usize = 100_000;
    let toolchain = Toolchain::fast()?;
    let s = &ctx.scratch;
    s.mkdir("w", 0o755, TESTER)?;
    let p = SMOKE.write(s, "w/p")?;
    build(ctx, &toolchain, &p, &["build"])?;
    let bulk = p.join("target/debug/deps/bulk");
    std::fs::create_dir(&bulk).map_err(|e| format!("mkdir {bulk:?}: {e}"))?;
    let chown = |path: &Path| {
        rustix::fs::chown(
            path,
            Some(rustix::fs::Uid::from_raw(TESTER.uid)),
            Some(rustix::fs::Gid::from_raw(TESTER.gid)),
        )
        .map_err(|e| format!("chown {path:?}: {e}"))
    };
    chown(&bulk)?;
    for i in 0..FILES {
        let file = bulk.join(format!("f{i:06}"));
        std::fs::write(&file, b"x").map_err(|e| format!("write {file:?}: {e}"))?;
        chown(&file)?;
    }
    let target = p.join("target");
    let oracle = Oracle::arm(reclaimable(&target)?)?;
    let run = cargo_reclaim(ctx, &[], &["/tmp/w"])?;
    let judged = oracle.judge(&run)?;
    run.expect_code(0)?;
    let got = verdict(&results(&run)?, &target)?;
    ensure!(got.starts_with(&format!("reclaimed {target:?} ")), "{got}");
    let held_us: u64 = got
        .rsplit("held_us=")
        .next()
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| format!("no held_us in {got}"))?;
    info!(
        "{FILES} extra files: the locks were held {:.2} s (run took {:.2} s under strace)",
        held_us as f64 / 1e6,
        run.took.as_secs_f64()
    );
    ensure!(held_us < 120_000_000, "the locks were held {held_us} µs");
    judged.assert_clean()
}
