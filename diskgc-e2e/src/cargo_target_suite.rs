//! Container tests of the `cargo-target` kind up to its reclaim protocol
//! (task-9; docs/safety.md, `cargo-target`): discovery and eligibility of
//! targets that real Cargo builds made, through the test driver's
//! `cargo-targets` (diskgc-core `cargo_target` over `LinuxHost`), run as
//! `tester` under strace and judged by the deletion oracle (nothing may
//! go: the operation locks and deletes nothing). Each test runs once per
//! Cargo toolchain under test ([`crate::cargo_suite::CargoTest`]): both
//! layouts (classic on 1.90.0 and 1.99.1, per-unit on the nightly), the
//! split build directory where the toolchain has one, and the process
//! heuristics against real `cargo build`, `cargo run` and `cargo test`.

use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::cargo_under_test::{self, Cargo, Gate, RACE, REACH_TIMEOUT, SMOKE, Toolchain};
use crate::controlled::ControlLine;
use crate::holder::Holder;
use crate::oracle::{Fixture, Oracle};
use crate::proc_locks::{self, Watched};
use crate::suite::{Ctx, TestResult};
use crate::under_test::Run;
use crate::{TESTER, ensure, info};

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

/// Runs `cargo-targets` with `args` as tester; it must exit 0 (skips are
/// results, not failures).
fn cargo_targets(ctx: &Ctx, args: &[&str]) -> Result<(Run, Vec<String>), String> {
    let mut full = vec!["cargo-targets"];
    full.extend_from_slice(args);
    let run = ctx.run_as_tester(&ctx.driver(), &full)?;
    let lines = results(&run)?;
    info!(
        "cargo-targets {}:\n{}\n--- stderr\n{}",
        args.join(" "),
        lines.join("\n"),
        run.stderr
    );
    run.expect_code(0)?;
    Ok((run, lines))
}

/// The verdict line of the target at `target` (`eligible ...` or `skip
/// ...`), and its `target` line.
fn verdict<'a>(lines: &'a [String], target: &Path) -> Result<(&'a str, &'a str), String> {
    let quoted = format!("{target:?}");
    let at = lines
        .iter()
        .position(|l| l.starts_with("target ") && l.split(' ').nth(1) == Some(&quoted))
        .ok_or_else(|| format!("{} was not found: {lines:#?}", target.display()))?;
    let next = lines
        .get(at + 1)
        .ok_or_else(|| format!("no verdict after {}", lines[at]))?;
    Ok((&lines[at], next))
}

#[track_caller]
fn expect_verdict(lines: &[String], target: &Path, want: &str) -> Result<String, String> {
    let (_, got) = verdict(lines, target)?;
    let prefix = format!("{want} ");
    ensure!(
        got.starts_with(&prefix),
        "{}: expected {want}, got {got}",
        target.display()
    );
    Ok(got.to_owned())
}

/// The layout a toolchain's plain build makes (measured, docs/safety.md
/// Lock files): the new per-unit build-dir layout on the nightly, classic
/// on the stable toolchains.
fn layout_of(toolchain: &Toolchain) -> &'static str {
    if toolchain.name.starts_with("nightly") {
        "per-unit"
    } else {
        "classic"
    }
}

fn build(ctx: &Ctx, toolchain: &Toolchain, project: &Path, env: &[(&str, &str)]) -> TestResult {
    let mut cargo = Cargo::new(toolchain, project, &["build"]);
    for (k, v) in env {
        cargo = cargo.env(k, *v);
    }
    cargo.output(ctx)?.expect_success()?;
    Ok(())
}

/// Discovery finds every target that real builds made below the roots,
/// also in a hidden directory (an agent worktree), once each when the
/// roots overlap, and none reachable only through a symlink or a bind
/// mount (AC #2); every target's layout is the toolchain's; an untouched
/// target is eligible without an idle requirement and `not_idle` with
/// `--idle-for 1d`; a profile directory with an unknown lock file name is
/// `unsupported_layout` (AC #1), and so is a `.cargo-lock` that is a FIFO
/// or a directory; a future file time is `future_time` (an unknown age is
/// never idle). Nothing is deleted.
pub fn cargo_targets_discovered_and_judged(ctx: &Ctx, toolchain: &Toolchain) -> TestResult {
    let s = &ctx.scratch;
    s.mkdirs("w/.claude/worktrees/agent", 0o755, TESTER)?;
    let p = RACE.write(s, "w/p")?;
    let hidden = SMOKE.write(s, "w/.claude/worktrees/agent/q")?;
    let unknown = SMOKE.write(s, "w/u")?;
    let future = SMOKE.write(s, "w/f")?;
    let fifo_lock = SMOKE.write(s, "w/lf")?;
    let dir_lock = SMOKE.write(s, "w/ld")?;
    for project in [&p, &hidden, &unknown, &future, &fifo_lock, &dir_lock] {
        build(ctx, toolchain, project, &[])?;
    }
    // A lock-like name on what is not a regular file: the .cargo-lock of a
    // real build replaced by a FIFO, and by a directory.
    for (project, fifo) in [(&fifo_lock, true), (&dir_lock, false)] {
        let lock = project.join("target/debug/.cargo-lock");
        std::fs::remove_file(&lock).map_err(|e| format!("rm {lock:?}: {e}"))?;
        if fifo {
            rustix::fs::mknodat(
                rustix::fs::CWD,
                &lock,
                rustix::fs::FileType::Fifo,
                rustix::fs::Mode::from_raw_mode(0o644),
                0,
            )
            .map_err(|e| format!("mkfifo {lock:?}: {e}"))?;
        } else {
            std::fs::create_dir(&lock).map_err(|e| format!("mkdir {lock:?}: {e}"))?;
        }
        rustix::fs::chown(
            &lock,
            Some(rustix::fs::Uid::from_raw(TESTER.uid)),
            Some(rustix::fs::Gid::from_raw(TESTER.gid)),
        )
        .map_err(|e| format!("chown {lock:?}: {e}"))?;
    }
    s.write_file("w/u/target/debug/.cargo-future-lock", b"", 0o644, TESTER)?;
    let in_a_year = SystemTime::now() + Duration::from_secs(365 * 86_400);
    std::fs::OpenOptions::new()
        .read(true)
        .open(SMOKE.debug_binary(&future))
        .and_then(|f| f.set_modified(in_a_year))
        .map_err(|e| format!("set a future mtime: {e}"))?;
    // A target outside the roots, reachable from inside them only through
    // a symlink and through a bind mount of the same filesystem (which a
    // device comparison would not tell apart): never found.
    s.mkdir("elsewhere", 0o755, TESTER)?;
    let outside = SMOKE.write(s, "elsewhere/m")?;
    build(ctx, toolchain, &outside, &[])?;
    s.symlink("w/link", &s.path("elsewhere"))?;
    s.mkdir("w/mnt", 0o755, TESTER)?;
    s.bind_mount("elsewhere", "w/mnt")?;

    // The idle path on real targets: wait until every profile directory
    // has been unchanged for more than a second, so `--idle-for 1s` must
    // find the untouched targets idle (a ctime is the time of the build,
    // not of the check).
    let last_change = [&p, &hidden]
        .iter()
        .map(|project| newest_change_below(&project.join("target/debug")))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
        .ok_or("no projects")?;
    let idle_from = last_change + Duration::from_millis(1_200);
    while SystemTime::now() < idle_from {
        std::thread::sleep(Duration::from_millis(50));
    }

    let oracle = Oracle::arm(Fixture::new())?;
    let (plain, lines) = cargo_targets(ctx, &["/tmp/w", "/tmp/w/.claude"])?;
    let (idle, idle_lines) = cargo_targets(ctx, &["--idle-for", "1d", "/tmp/w"])?;
    let (second, second_lines) = cargo_targets(ctx, &["--idle-for", "1s", "/tmp/w"])?;
    let verdict_of_runs = oracle.judge_runs(&[&plain, &idle, &second])?;

    let t = |project: &Path| project.join("target");
    let targets: Vec<&String> = lines.iter().filter(|l| l.starts_with("target ")).collect();
    ensure!(
        targets.len() == 6,
        "expected the 6 targets, found {}: {lines:#?}",
        targets.len()
    );
    for project in [&fifo_lock, &dir_lock] {
        let got = expect_verdict(&lines, &t(project), "skip unsupported_layout")?;
        ensure!(
            got.contains(".cargo-lock: a Cargo lock file name on a"),
            "{got}"
        );
        ensure!(got.contains("not a regular file"), "{got}");
    }
    ensure!(
        !lines
            .iter()
            .any(|l| l.contains("/tmp/w/mnt") || l.contains("/tmp/w/link")),
        "discovery followed a symlink or crossed a bind mount: {lines:#?}"
    );
    ensure!(
        lines.contains(&format!("duplicate {:?}", t(&hidden))),
        "the target found again through the second root is not a duplicate: {lines:#?}"
    );
    let layout = layout_of(toolchain);
    for project in [&p, &hidden, &future] {
        let (target_line, _) = verdict(&lines, &t(project))?;
        ensure!(
            target_line.contains(&format!(" debug={layout}:")),
            "{target_line}: expected the {layout} layout of {}",
            toolchain.name
        );
        expect_verdict(&lines, &t(project), "eligible")?;
    }
    for project in [&p, &hidden] {
        expect_verdict(&idle_lines, &t(project), "skip not_idle")?;
        expect_verdict(&second_lines, &t(project), "eligible")
            .map_err(|e| format!("idle for a second: {e}"))?;
    }
    expect_verdict(&idle_lines, &t(&future), "skip future_time")?;
    expect_verdict(&second_lines, &t(&future), "skip future_time")?;
    // The last build each eligible line reports is the newest mtime or
    // ctime of everything in the profile directory, the directory itself
    // included, as an independent walk of it (std, not diskgc) finds it.
    for project in [&p, &hidden, &future] {
        let got = expect_verdict(&lines, &t(project), "eligible")?;
        let reported: u64 = got
            .split(' ')
            .find_map(|w| w.strip_prefix("last_build="))
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| format!("no last_build in {got}"))?;
        let newest = newest_change_below(&t(project).join("debug"))?
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|e| format!("before the epoch: {e}"))?
            .as_secs();
        ensure!(
            reported == newest,
            "{}: last_build={reported}, the independent walk's newest change is {newest}",
            t(project).display()
        );
    }
    for at in [&lines, &idle_lines] {
        let got = expect_verdict(at, &t(&unknown), "skip unsupported_layout")?;
        ensure!(
            got.contains(".cargo-future-lock: an unknown Cargo lock file name"),
            "{got}"
        );
    }
    verdict_of_runs.assert_clean()
}

/// With a separate build directory (`CARGO_BUILD_BUILD_DIR` outside the
/// project), 1.99.1 and the nightly split the lock set: both halves are
/// found (each has a tag and a lock file) and both are
/// `unsupported_layout`, since nothing pairs them and they would have to be
/// locked together. 1.90.0 ignores the setting: there it makes no build
/// directory, the target is whole and eligible, and the split case is not
/// applicable (logged as such, checked: no build directory exists).
pub fn cargo_split_build_dir_halves_are_unsupported_layout(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("w", 0o755, TESTER)?;
    let p = RACE.write(s, "w/p")?;
    let build_dir = s.path("bd");
    build(
        ctx,
        toolchain,
        &p,
        &[("CARGO_BUILD_BUILD_DIR", path_str(&build_dir)?)],
    )?;
    let splits = toolchain.release() != cargo_under_test::IGNORES_BUILD_DIR;
    ensure!(
        build_dir.exists() == splits,
        "toolchain {}: build directory {} exists: {}, expected {splits} (measured, \
         docs/safety.md Lock files)",
        toolchain.name,
        build_dir.display(),
        build_dir.exists()
    );
    let oracle = Oracle::arm(Fixture::new())?;
    let roots: Vec<&str> = if splits {
        vec!["/tmp/w", "/tmp/bd"]
    } else {
        vec!["/tmp/w"]
    };
    let (run, lines) = cargo_targets(ctx, &roots)?;
    let judged = oracle.judge(&run)?;
    let target = p.join("target");
    if !splits {
        info!(
            "toolchain {} ignores CARGO_BUILD_BUILD_DIR: the split build directory case is not \
             applicable here (vacuous, not passed); the target is whole",
            toolchain.name
        );
        expect_verdict(&lines, &target, "eligible")?;
        return judged.assert_clean();
    }
    let (artifact_line, _) = verdict(&lines, &target)?;
    ensure!(
        artifact_line.ends_with("debug=unknown:.cargo-artifact-lock,.cargo-lock"),
        "the target directory's half: {artifact_line}"
    );
    let got = expect_verdict(&lines, &target, "skip unsupported_layout")?;
    ensure!(
        got.contains("target directory's half of a split build directory"),
        "{got}"
    );
    let (build_line, _) = verdict(&lines, &build_dir)?;
    ensure!(
        build_line.contains("debug=") && build_line.ends_with(":.cargo-build-lock"),
        "the build directory's half: {build_line}"
    );
    let got = expect_verdict(&lines, &build_dir, "skip unsupported_layout")?;
    ensure!(
        got.contains("build directory's half of a split build directory"),
        "{got}"
    );
    judged.assert_clean()
}

/// AC #3 on real Cargo processes, each case on its own (the target is
/// eligible before and after, so the process is what keeps it):
///
/// - a `cargo build` in the project directory, blocked on `.cargo-lock`
///   (a lock holder holds it): a Cargo process working in the project
///   directory;
/// - `cargo run` paused in its runner (Cargo execed the runner, so no
///   process named cargo is left): `CARGO_MANIFEST_DIR` of the project;
/// - `cargo test` paused in its runner: the parent Cargo in the project
///   directory, and the runner's `CARGO_MANIFEST_DIR`;
/// - a running test binary (paused at the fixture test's gate): its
///   executable inside the target;
/// - a process with `CARGO_TARGET_DIR` naming the target in its initial
///   environment.
pub fn cargo_running_cargo_keeps_its_target_in_use(ctx: &Ctx, toolchain: &Toolchain) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("w", 0o755, TESTER)?;
    let p = RACE.write(s, "w/p")?;
    build(ctx, toolchain, &p, &[])?;
    Cargo::new(toolchain, &p, &["test", "--no-run"])
        .output(ctx)?
        .expect_success()?;
    let target = p.join("target");
    let fixture = || Fixture::new().concurrent_writer(p.clone());
    let judge_eligible = |when: &str| -> TestResult {
        let oracle = Oracle::arm(fixture())?;
        let (run, lines) = cargo_targets(ctx, &["/tmp/w"])?;
        let judged = oracle.judge(&run)?;
        expect_verdict(&lines, &target, "eligible")
            .map_err(|e| format!("{when}: the control: {e}"))?;
        judged.assert_clean()
    };
    // `needles`: (pid, text) pairs the in-use evidence must hold.
    let in_use = |case: &str, needles: &[(u32, &str)]| -> TestResult {
        let oracle = Oracle::arm(fixture())?;
        let (run, lines) = cargo_targets(ctx, &["/tmp/w"])?;
        let judged = oracle.judge(&run)?;
        let got =
            expect_verdict(&lines, &target, "skip in_use").map_err(|e| format!("{case}: {e}"))?;
        for (pid, needle) in needles {
            let evidence = got
                .split("; ")
                .find(|e| e.contains(&format!("pid {pid} (")) && e.contains(needle));
            ensure!(
                evidence.is_some(),
                "{case}: no evidence that pid {pid} {needle:?} in: {got}"
            );
        }
        info!("{case}: in use as expected");
        judged.assert_clean()
    };

    judge_eligible("before")?;

    // A build blocked on .cargo-lock, in the project directory.
    let cargo_lock = target.join("debug/.cargo-lock");
    let watched = Watched::files(std::slice::from_ref(&cargo_lock))?;
    let holder = Holder::tester(ctx, &["flock-hold", path_str(&cargo_lock)?], &[])?;
    let blocked = Cargo::new(toolchain, &p, &["build"]).spawn(ctx)?;
    watched.wait_for(
        blocked.pid(),
        REACH_TIMEOUT,
        "blocked on .cargo-lock",
        |u| u.waits(&cargo_lock),
    )?;
    in_use(
        "a cargo build blocked in the project directory",
        &[(
            blocked.pid(),
            "is a Cargo process working in the project directory",
        )],
    )?;
    holder.finish()?;
    blocked.wait()?.expect_success()?;

    // cargo run and cargo test, paused in their runner.
    for (command, execs) in [(&["run"][..], true), (&["test"][..], false)] {
        let gate = Gate::new(&ctx.scratch, &format!("runner-{}", command[0]))?;
        let cargo = Cargo::new(toolchain, &p, command)
            .runner_gate(ctx, &gate)
            .spawn(ctx)?;
        let runner = gate.wait_until_waiting(REACH_TIMEOUT)?;
        let manifest = "was started by Cargo for a package of the project directory";
        let mut needles = vec![(runner, manifest)];
        if !execs {
            needles.push((
                cargo.pid(),
                "is a Cargo process working in the project directory",
            ));
        }
        in_use(
            &format!("cargo {} paused in its runner", command[0]),
            &needles,
        )?;
        gate.open(&ctx.scratch)?;
        cargo.wait()?.expect_success()?;
    }

    // A test binary still running.
    let gate = Gate::new(&ctx.scratch, "test-binary")?;
    let cargo = Cargo::new(toolchain, &p, &["test"])
        .test_gate(&gate)
        .spawn(ctx)?;
    let test_binary = gate.wait_until_waiting(REACH_TIMEOUT)?;
    in_use(
        "a running test binary",
        &[(test_binary, "runs an executable inside the target")],
    )?;
    gate.open(&ctx.scratch)?;
    cargo.wait()?.expect_success()?;

    // CARGO_TARGET_DIR in a process's initial environment.
    let target_dir = path_str(&target)?.to_owned();
    s.write_file("unrelated", b"", 0o644, TESTER)?;
    let holder = Holder::tester(
        ctx,
        &["flock-hold", path_str(&s.path("unrelated"))?],
        &[("CARGO_TARGET_DIR", &target_dir)],
    )?;
    in_use(
        "CARGO_TARGET_DIR in the initial environment",
        &[(
            holder.pid(),
            "names a path inside the target in its initial environment (CARGO_TARGET_DIR=",
        )],
    )?;
    holder.finish()?;

    judge_eligible("after")?;
    let locks = proc_locks::sample()?;
    ensure!(
        watched.locks_on(&locks).is_empty(),
        "locks left on .cargo-lock: {:?}",
        watched.locks_on(&locks)
    );
    Ok(())
}

/// The newest mtime or ctime of `dir` and everything below it, symlinks
/// not followed: the harness's own walk, independent of diskgc's.
fn newest_change_below(dir: &Path) -> Result<SystemTime, String> {
    use std::os::unix::fs::MetadataExt;
    let mut newest = SystemTime::UNIX_EPOCH;
    let mut todo = vec![dir.to_owned()];
    while let Some(at) = todo.pop() {
        let meta =
            std::fs::symlink_metadata(&at).map_err(|e| format!("lstat {}: {e}", at.display()))?;
        for (secs, nanos) in [
            (meta.mtime(), meta.mtime_nsec()),
            (meta.ctime(), meta.ctime_nsec()),
        ] {
            let t = SystemTime::UNIX_EPOCH
                + Duration::new(
                    u64::try_from(secs).map_err(|_| "a time before the epoch")?,
                    u32::try_from(nanos).map_err(|_| "bad nanoseconds")?,
                );
            newest = newest.max(t);
        }
        if meta.is_dir() {
            for entry in
                std::fs::read_dir(&at).map_err(|e| format!("read_dir {}: {e}", at.display()))?
            {
                todo.push(
                    entry
                        .map_err(|e| format!("entry of {}: {e}", at.display()))?
                        .path(),
                );
            }
        }
    }
    Ok(newest)
}

/// Codex 2: a target below a directory whose name is not UTF-8, with a
/// process running an executable from a part of the target the walks do
/// not read (`target/tmp/sleep`, a copy of the image's `sleep`, working in `/` with
/// only `PATH` in its environment): only the executable's path text shows
/// the use, and it must be compared byte for byte, so the target is
/// `in_use`. It is eligible before and after (the control). The helper is
/// started through a shell from inside `target/tmp`, so no argument is
/// non-UTF-8 (the harness reads its arguments as UTF-8).
pub fn cargo_running_executable_below_a_non_utf8_name_keeps_its_target_in_use(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let s = &ctx.scratch;
    s.mkdirs("w/x", 0o755, TESTER)?;
    let built = SMOKE.write(s, "w/x/p")?;
    build(ctx, toolchain, &built, &[])?;
    let odd = s.path("w").join(OsStr::from_bytes(b"\xff"));
    std::fs::rename(s.path("w/x"), &odd).map_err(|e| format!("rename to {odd:?}: {e}"))?;
    let target = odd.join("p/target");
    let tmp = target.join("tmp");
    std::fs::create_dir(&tmp).map_err(|e| format!("mkdir {tmp:?}: {e}"))?;
    let helper = tmp.join("sleep");
    let sleep = std::fs::canonicalize(SLEEP).map_err(|e| format!("{SLEEP}: {e}"))?;
    std::fs::copy(&sleep, &helper).map_err(|e| format!("copy {}: {e}", sleep.display()))?;
    for path in [&tmp, &helper] {
        rustix::fs::chown(
            path.as_path(),
            Some(rustix::fs::Uid::from_raw(TESTER.uid)),
            Some(rustix::fs::Gid::from_raw(TESTER.gid)),
        )
        .map_err(|e| format!("chown {path:?}: {e}"))?;
    }
    let judge = |want: &str| -> Result<String, String> {
        let oracle = Oracle::arm(Fixture::new())?;
        let (run, lines) = cargo_targets(ctx, &["/tmp/w"])?;
        let judged = oracle.judge(&run)?;
        let got = expect_verdict(&lines, &target, want)?;
        judged.assert_clean()?;
        Ok(got)
    };
    judge("eligible").map_err(|e| format!("before: {e}"))?;
    let mut child = std::process::Command::new(&ctx.self_exe)
        .args([
            "as-tester",
            "--",
            "/bin/sh",
            "-c",
            "p=\"$PWD/sleep\"; cd / && exec env -i PATH=/bin \"$p\" 300",
        ])
        .env_clear()
        .env("PATH", "/bin")
        .current_dir(&tmp)
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn the helper: {e}"))?;
    let pid = child.id();
    let started = std::time::Instant::now();
    while std::fs::read_link(format!("/proc/{pid}/exe")).ok() != Some(helper.clone()) {
        if started.elapsed() > REACH_TIMEOUT {
            let _ = child.kill();
            return Err(format!("the helper (pid {pid}) never ran {helper:?}"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let got = judge("skip in_use");
    let _ = child.kill();
    let _ = child.wait();
    let got = got?;
    ensure!(
        got.contains(&format!("pid {pid} ("))
            && got.contains("runs an executable inside the target"),
        "the helper (pid {pid}) is not named as running from the target: {got}"
    );
    judge("eligible").map_err(|e| format!("after: {e}"))?;
    Ok(())
}

/// The image's `sleep`, followed to its file: busybox in the Nix image (a
/// copy named `sleep` is `sleep`), coreutils' in the Debian one.
const SLEEP: &str = "/bin/sleep";

/// mped S2: a detached target (`cargo build --target-dir out/target`, so
/// its parent holds no `Cargo.toml`) is `in_use` while a Cargo runs in the
/// project, although that Cargo works in neither the target's parent nor
/// below it (a gated build of the project's own `target/`); eligible before
/// and after.
pub fn cargo_detached_target_is_in_use_while_any_cargo_runs(
    ctx: &Ctx,
    toolchain: &Toolchain,
) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("w", 0o755, TESTER)?;
    let q = RACE.write(s, "w/q")?;
    Cargo::new(toolchain, &q, &["build", "--target-dir", "out/target"])
        .output(ctx)?
        .expect_success()?;
    let detached = q.join("out/target");
    let judge = |want: &str| -> Result<String, String> {
        let oracle = Oracle::arm(Fixture::new().concurrent_writer(q.clone()))?;
        let (run, lines) = cargo_targets(ctx, &["/tmp/w"])?;
        let judged = oracle.judge(&run)?;
        let got = expect_verdict(&lines, &detached, want)?;
        judged.assert_clean()?;
        Ok(got)
    };
    judge("eligible").map_err(|e| format!("before: {e}"))?;
    let gate = Gate::new(&ctx.scratch, "detached")?;
    let cargo = Cargo::new(toolchain, &q, &["build"])
        .build_gate(&gate)
        .spawn(ctx)?;
    gate.wait_until_waiting(REACH_TIMEOUT)?;
    let got = judge("skip in_use")?;
    ensure!(
        got.contains(&format!("pid {} (", cargo.pid()))
            && got.contains("is a Cargo process, and the target is detached"),
        "the running cargo (pid {}) is not named for the detached target: {got}",
        cargo.pid()
    );
    gate.open(&ctx.scratch)?;
    cargo.wait()?.expect_success()?;
    judge("eligible").map_err(|e| format!("after: {e}"))?;
    Ok(())
}

fn path_str(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("non-UTF-8 path {}", path.display()))
}
