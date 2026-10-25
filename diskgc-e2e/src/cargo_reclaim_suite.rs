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

use std::path::Path;

use crate::cargo_harness::{
    HOST_TRIPLE, build, cargo_reclaim, deletable, edit_main, lock_inodes, part_of, path_str,
    planned_parts, profile_dirs, rebuilds, reclaimable, results, unlinked, verdict,
};
use crate::cargo_under_test::{self, Cargo, Gate, RACE, REACH_TIMEOUT, SMOKE, Toolchain};
use crate::holder::Holder;
use crate::oracle::{Fixture, Oracle};
use crate::proc_locks::{self, Watched};
use crate::suite::{Ctx, TestResult};
use crate::{TESTER, ensure, info};

/// AC #1 and AC #2, and the order of step 3, on targets real builds made
/// (`build`, `test --no-run`, `build --release`, `build --target`): the
/// protocol deletes every entry of every profile directory but the lock
/// files; the oracle (snapshot by inode and birth time, and the strace
/// record) finds no lock file unlinked or replaced and nothing outside the
/// profile directories touched: not `CACHEDIR.TAG`, `.rustc_info.json`,
/// `doc/`, `tmp/`, `criterion/` or the triple directory. The strace record
/// shows the unlinks part by part in the harness's planned order of step 3
/// ([`planned_parts`]: every fingerprint first, `build/` and `deps/` last
/// in each profile), each part with its planned count; the next build
/// compiles again and runs.
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
    let parts = planned_parts(&profiles)?;
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

    // Step 3's order, from the strace record against the harness's own plan
    // of the parts: each part's unlinks together, the parts in plan order
    // (every fingerprint first, each profile's layout directories last), and
    // each part's unlink count as planned.
    let order: Vec<usize> = unlinked(&run)
        .iter()
        .map(|path| {
            part_of(&parts, path).ok_or_else(|| format!("{} is in no planned part", path.display()))
        })
        .collect::<Result<_, String>>()?;
    ensure!(
        order.windows(2).all(|w| w[0] <= w[1]),
        "the unlinks are not in the planned order of parts: {:?}",
        order
            .windows(2)
            .find(|w| w[0] > w[1])
            .map(|w| (&parts[w[0]].path, &parts[w[1]].path))
    );
    for (i, part) in parts.iter().enumerate() {
        let made = order.iter().filter(|&&n| n == i).count();
        ensure!(
            made == part.objects,
            "{}: {made} unlinks, planned {}",
            part.path.display(),
            part.objects
        );
    }
    info!(
        "{}: {} unlinks in the planned order of {} parts",
        toolchain.name,
        order.len(),
        parts.len()
    );
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

/// AC #3 on real Cargo, and race test 5 (docs/testing.md): the driver
/// paused right before its first lock (`flock_try:1`, after discovery;
/// `/proc/locks`: it holds none yet), while (a) a `cargo build --release`
/// creates a new profile directory, (a') a `cargo build --target TRIPLE`
/// creates a new triple's profile directory, or (b) a rebuild after an
/// edit makes a newer build than the one the eligibility check judged
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
    /// A change made while the driver is paused before its first lock.
    struct Case {
        name: &'static str,
        flags: &'static [&'static str],
        /// The directory below the target the build writes.
        written: &'static str,
        build: &'static [&'static str],
        /// Whether the source is edited first.
        edit: bool,
    }
    let cases = [
        Case {
            name: "a new profile directory",
            flags: &[],
            written: "release",
            build: &["build", "--release"],
            edit: false,
        },
        Case {
            name: "a new triple's profile directory",
            flags: &[],
            written: HOST_TRIPLE,
            build: &["build", "--target", HOST_TRIPLE],
            edit: false,
        },
        Case {
            name: "a newer build",
            flags: &["--after-eligibility"],
            written: "debug",
            build: &["build"],
            edit: true,
        },
    ];
    for Case {
        name: case,
        flags,
        written,
        build: build_args,
        edit,
    } in cases
    {
        let locks = lock_inodes(&target)?;
        let watched = Watched::files(&locks.keys().cloned().collect::<Vec<_>>())?;
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
        // Between discovery and locking: diskgc holds no lock yet.
        let usage = watched.usage(&proc_locks::sample()?, paused.pid);
        ensure!(
            usage.is_empty(),
            "{case}: paused before its first lock, diskgc already {usage}"
        );
        if edit {
            edit_main(&p)?;
        }
        build(ctx, toolchain, &p, build_args)?;
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
    // TASK-165: the project renamed under a keep marker while the driver
    // is paused before its first lock. The locks and walks go through fds
    // that stay valid, so only the path reached again from the root under
    // the locks sees that the target is no longer below its root as found:
    // changed, nothing unlinked. Moved back afterwards for the control.
    s.mkdir("w/kept", 0o755, TESTER)?;
    s.write_file("w/kept/.diskgc-keep", b"", 0o644, TESTER)?;
    let locks = lock_inodes(&target)?;
    // Nothing is eligible; judged after the project is moved back, so the
    // harness's own renames leave every object where it was armed.
    let oracle = Oracle::arm(Fixture::new())?;
    let mut spawned = ctx.spawn_as_tester(
        &ctx.driver(),
        &["--pause-at", "flock_try:1", "cargo-reclaim", "/tmp/w"],
    )?;
    let paused = spawned.wait_paused()?;
    ensure!(paused.op == "flock_try" && paused.nth == 1, "{paused:?}");
    s.rename("w/p", "w/kept/p")?;
    spawned.resume(&paused)?;
    let run = spawned.wait()?;
    info!(
        "ancestor renamed under a keep marker:\n{}\n--- stderr\n{}",
        run.stdout, run.stderr
    );
    s.rename("w/kept/p", "w/p")?;
    let judged = oracle.judge(&run)?;
    run.expect_code(0)?;
    let got = verdict(&results(&run)?, &target)?;
    ensure!(
        got.starts_with(&format!("skip changed {target:?} entries=0 ")),
        "ancestor renamed under a keep marker: expected changed, got {got}"
    );
    ensure!(
        unlinked(&run).is_empty(),
        "ancestor renamed under a keep marker: the driver unlinked {:?}",
        unlinked(&run)
    );
    ensure!(
        lock_inodes(&target)? == locks,
        "ancestor renamed under a keep marker: a lock file gone or replaced"
    );
    judged.assert_clean()?;
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
