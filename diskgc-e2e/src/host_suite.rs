//! Container tests of the `Host` primitives (task-3): path traversal against
//! real symlinks and mounts, statx masks, statvfs, flock and the `/proc`
//! readers, and the minimum-kernel check. The code under test is
//! `LinuxHost` with the core's traversal, driven as `tester` (uid 1000, no
//! capabilities) by the host probes ([`crate::host_probe`]), and the
//! `diskgc` binary for the kernel check. Every traced run is judged by the
//! deletion oracle; none may delete anything except the unlink test's
//! eligible entries.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rustix::fs::{AtFlags, CWD, StatxFlags};

use crate::oracle::{Fixture, Oracle};
use crate::scratch::{self, Scratch};
use crate::suite::{Ctx, TestResult};
use crate::under_test::Run;
use crate::{ROOT, TESTER, ensure, info};

/// What a probe line for one argument must say.
#[derive(Debug, Clone, Copy)]
enum Expected {
    /// Resolved, on the mount with this id.
    On(u64),
    /// Refused with this skip reason and cause tag ([`crate::host_probe::tag`]).
    Refused(&'static str, &'static str),
}

fn to_str(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("non-UTF-8 path {}", path.display()))
}

/// The probe's `I rest...` lines by index.
fn indexed(stdout: &str) -> Result<BTreeMap<usize, String>, String> {
    stdout
        .lines()
        .map(|line| {
            let (i, rest) = line
                .split_once(' ')
                .ok_or_else(|| format!("unexpected probe line {line:?}"))?;
            let i = i
                .parse()
                .map_err(|_| format!("unexpected probe line {line:?}"))?;
            Ok((i, rest.to_owned()))
        })
        .collect()
}

/// Checks each case's line of a `host-resolve` or `host-beneath` run.
fn check_cases(run: &Run, cases: &[(String, Expected)]) -> TestResult {
    run.expect_code(0)?;
    let lines = indexed(&run.stdout)?;
    for (i, (what, expected)) in cases.iter().enumerate() {
        let got = lines
            .get(&i)
            .ok_or_else(|| format!("no result for case {i} {what:?}:\n{}", run.stdout))?;
        info!("{what:?}: {got}");
        let prefix = match expected {
            Expected::On(mnt) => format!("ok mnt={mnt} "),
            Expected::Refused(reason, tag) => format!("err {reason} {tag} "),
        };
        ensure!(
            got.starts_with(&prefix),
            "{what:?}: expected {expected:?}, got {got:?}"
        );
    }
    Ok(())
}

/// The `dev=` field of a probe line.
fn dev_field(line: &str) -> Option<&str> {
    line.split(' ').find_map(|f| f.strip_prefix("dev="))
}

/// A readable open of a FIFO with no writer returns promptly and carries
/// O_NONBLOCK. The probe has a two-second alarm, so removing that flag
/// fails this test on the real kernel without hanging the whole suite.
pub fn host_read_open_of_fifo_does_not_wait_for_writer(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.fifo("unopened-fifo", 0o600, TESTER)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let run = ctx.probe_as_tester(&["host-read-fifo", "/tmp", "unopened-fifo"])?;
    let verdict = oracle.judge(&run)?;
    run.expect_code(0)?;
    ensure!(
        run.stdout.trim() == "nonblock=true",
        "readable FIFO open lost its safe flags: {:?}",
        run.stdout
    );
    verdict.assert_clean()
}

/// A symlink anywhere in a configured path is refused as `unsafe_path`
/// (`ELOOP` from `RESOLVE_NO_SYMLINKS`): as the first component below the
/// mount, in the middle, at the end (dangling too), and in the
/// `/home -> /var/home` layout, where the symlink leads to another mount.
pub fn host_resolve_refuses_symlinks(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdirs("root/a/b/c", 0o755, TESTER)?;
    s.write_file("root/a/b/file", b"file", 0o644, TESTER)?;
    s.symlink("root/mid", Path::new("a"))?;
    s.symlink("root/a/last", Path::new("b"))?;
    s.symlink("root/a/b/dangling", Path::new("nowhere"))?;
    s.symlink("link-first", &s.path("root"))?;
    s.mkdir("var-home", 0o755, ROOT)?;
    let var_home = Scratch::mount(to_str(&s.path("var-home"))?, "0755", TESTER)?;
    var_home.write_file("x", b"x", 0o644, TESTER)?;
    s.symlink("home", var_home.root())?;

    let symlink = Expected::Refused("unsafe_path", "symlink");
    let cases: Vec<(String, Expected)> = [
        // Controls: the real paths resolve, each on its own mount.
        ("/tmp/root/a/b/c", Expected::On(s.mnt_id())),
        ("/tmp/root/a/b/file", Expected::On(s.mnt_id())),
        ("/tmp/var-home/x", Expected::On(var_home.mnt_id())),
        ("/tmp/root/mid/b", symlink),
        ("/tmp/root/a/last", symlink),
        ("/tmp/root/a/b/dangling", symlink),
        ("/tmp/link-first/a", symlink),
        ("/tmp/home/x", symlink),
        ("/tmp/home", symlink),
        (
            "/tmp/root/a/../a",
            Expected::Refused("unsafe_path", "not_normal"),
        ),
        ("/tmp/root/nope", Expected::Refused("missing", "missing")),
    ]
    .into_iter()
    .map(|(p, e)| (p.to_owned(), e))
    .collect();

    let oracle = Oracle::arm(Fixture::new())?;
    let mut args = vec!["host-resolve", "any"];
    args.extend(cases.iter().map(|(p, _)| p.as_str()));
    let run = ctx.probe_as_tester(&args)?;
    let verdict = oracle.judge(&run)?;
    check_cases(&run, &cases)?;
    verdict.assert_clean()
}

/// No mount is crossed below a resolved directory: a bind mount of the same
/// filesystem (same device, so a device comparison would miss it) is
/// refused (`EXDEV` from `RESOLVE_NO_XDEV`). A configured path *on* that
/// bind mount resolves on it; stacked mounts resolve to the topmost; a
/// mount point shadowed by a later mount over its parent is refused by the
/// statx confirmation; escaped mount points (`\040`, `\011`, `\134` in
/// mountinfo) are found.
pub fn host_resolve_refuses_mount_crossings(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdirs("root/plain", 0o755, TESTER)?;
    s.write_file("root/plain/f", b"f", 0o644, TESTER)?;
    s.mkdir("root/sub", 0o755, TESTER)?;
    s.mkdir("src", 0o755, TESTER)?;
    s.write_file("src/f", b"f", 0o644, TESTER)?;
    s.bind_mount("src", "root/sub")?;
    let bind_id = scratch::mount_id(&s.path("root/sub"), false)?;
    ensure!(
        bind_id != s.mnt_id(),
        "the bind mount kept the scratch mount id"
    );

    s.mkdir("stack", 0o755, TESTER)?;
    let lower = Scratch::mount(to_str(&s.path("stack"))?, "0755", TESTER)?;
    lower.write_file("only-lower", b"lower", 0o644, TESTER)?;
    let upper = Scratch::mount(to_str(&s.path("stack"))?, "0755", TESTER)?;
    upper.write_file("only-upper", b"upper", 0o644, TESTER)?;

    s.mkdirs("sh/inner", 0o755, TESTER)?;
    let inner = Scratch::mount(to_str(&s.path("sh/inner"))?, "0755", TESTER)?;
    inner.write_file("x", b"inner", 0o644, TESTER)?;
    let over = Scratch::mount(to_str(&s.path("sh"))?, "0755", TESTER)?;
    over.mkdir("inner", 0o755, TESTER)?;
    over.write_file("inner/x", b"over", 0o644, TESTER)?;

    let odd = "with space\tand\\back";
    s.mkdir(odd, 0o755, TESTER)?;
    let odd_mount = Scratch::mount(to_str(&s.path(odd))?, "0755", TESTER)?;
    odd_mount.write_file("x", b"odd", 0o644, TESTER)?;
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|e| format!("read mountinfo: {e}"))?;
    ensure!(
        mountinfo.contains("/tmp/with\\040space\\011and\\134back "),
        "mountinfo does not escape the odd mount point as expected:\n{mountinfo}"
    );

    let absolute: Vec<(String, Expected)> = vec![
        ("/tmp/root/plain/f".into(), Expected::On(s.mnt_id())),
        ("/tmp/root/sub/f".into(), Expected::On(bind_id)),
        ("/tmp/stack/only-upper".into(), Expected::On(upper.mnt_id())),
        (
            "/tmp/stack/only-lower".into(),
            Expected::Refused("missing", "missing"),
        ),
        ("/tmp/stack".into(), Expected::On(upper.mnt_id())),
        (
            "/tmp/sh/inner/x".into(),
            Expected::Refused("unsafe_path", "mount_mismatch"),
        ),
        (format!("/tmp/{odd}/x"), Expected::On(odd_mount.mnt_id())),
        (format!("/tmp/{odd}"), Expected::On(odd_mount.mnt_id())),
    ];
    let crosses = Expected::Refused("unsafe_path", "crosses_mount");
    let below_root: Vec<(String, Expected)> = vec![
        ("plain/f".into(), Expected::On(s.mnt_id())),
        ("sub/f".into(), crosses),
        ("sub".into(), crosses),
        (
            "plain/../sub".into(),
            Expected::Refused("unsafe_path", "not_normal"),
        ),
    ];

    let oracle = Oracle::arm(Fixture::new())?;
    let mut args = vec!["host-resolve", "any"];
    args.extend(absolute.iter().map(|(p, _)| p.as_str()));
    let resolved = ctx.probe_as_tester(&args)?;
    let mut args = vec!["host-beneath", "/tmp/root", "any"];
    args.extend(below_root.iter().map(|(p, _)| p.as_str()));
    let beneath = ctx.probe_as_tester(&args)?;
    let verdict = oracle.judge_runs(&[&resolved, &beneath])?;

    check_cases(&resolved, &absolute)?;
    check_cases(&beneath, &below_root)?;
    let lines = indexed(&resolved.stdout)?;
    let (plain_dev, bind_dev) = (
        lines.get(&0).and_then(|l| dev_field(l)),
        lines.get(&1).and_then(|l| dev_field(l)),
    );
    ensure!(
        plain_dev.is_some() && plain_dev == bind_dev,
        "the bind mount should be the same filesystem (device) as the scratch mount: \
         {plain_dev:?} vs {bind_dev:?}"
    );
    verdict.assert_clean()
}

/// statx on a real kernel: the mount id and mount-root attribute are
/// answered on tmpfs and procfs; procfs does not answer the birth time
/// (checked independently by the harness), and `LinuxHost` reports it as
/// absent rather than as a zero timestamp. statvfs gives `f_bavail ×
/// f_frsize`, as the harness computes it.
pub fn host_statx_and_statvfs_on_a_real_kernel(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.write_file("f", b"f", 0o644, TESTER)?;
    let raw_proc = rustix::fs::statx(CWD, "/proc", AtFlags::SYMLINK_NOFOLLOW, StatxFlags::BTIME)
        .map_err(|e| format!("statx /proc: {e}"))?;
    ensure!(
        raw_proc.stx_mask & StatxFlags::BTIME.bits() == 0,
        "this kernel answers a birth time on procfs; the absent-field case needs another \
         filesystem"
    );
    let proc_id = scratch::mount_id(Path::new("/proc"), false)?;

    let oracle = Oracle::arm(Fixture::new())?;
    let stat = ctx.probe_as_tester(&["host-stat", "/tmp", "/tmp/f", "/proc"])?;
    let space = ctx.probe_as_tester(&["host-statvfs", "/tmp"])?;
    let vfs = rustix::fs::statvfs("/tmp").map_err(|e| format!("statvfs /tmp: {e}"))?;
    let verdict = oracle.judge_runs(&[&stat, &space])?;

    stat.expect_code(0)?;
    let lines = indexed(&stat.stdout)?;
    let tmp = s.mnt_id();
    for (i, want) in [
        format!("mnt_id={tmp} mount_root=true btime=some"),
        format!("mnt_id={tmp} mount_root=false btime=some"),
        format!("mnt_id={proc_id} mount_root=true btime=none"),
    ]
    .iter()
    .enumerate()
    {
        let got = lines.get(&i).map(String::as_str);
        info!("host-stat {i}: {got:?}");
        ensure!(
            got == Some(want.as_str()),
            "case {i}: expected {want:?}, got {got:?}"
        );
    }

    space.expect_code(0)?;
    let want = format!(
        "avail={} size={}",
        vfs.f_bavail * vfs.f_frsize,
        vfs.f_blocks * vfs.f_frsize
    );
    info!(
        "host-statvfs /tmp: {:?}, harness: {want:?}",
        space.stdout.trim()
    );
    ensure!(
        space.stdout.trim() == want,
        "statvfs: probe said {:?}, harness computed {want:?}",
        space.stdout.trim()
    );
    verdict.assert_clean()
}

/// A root probe gets its own mount namespace, constructs LinuxHost, covers
/// /proc with tmpfs, then lazily detaches the cover and its own procfs.
/// The held procfs must keep serving its readers after it is detached.
pub fn host_procfs_stays_pinned_after_overmount(ctx: &Ctx) -> TestResult {
    let out = ctx.probe_as_root(&["host-proc-replaced"])?;
    ensure!(
        out.status.success(),
        "procfs replacement probe failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    ensure!(
        String::from_utf8_lossy(&out.stdout).trim()
            == "held_procfs=true overmount_refused=true detached_procfs_readable=true",
        "unexpected procfs probe evidence: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    Ok(())
}

/// `major:minor:inode` of `path` (not following it), as the probes print.
fn dev_ino(path: &Path) -> Result<String, String> {
    let x = rustix::fs::statx(CWD, path, AtFlags::SYMLINK_NOFOLLOW, StatxFlags::INO)
        .map_err(|e| format!("statx {}: {e}", path.display()))?;
    Ok(format!(
        "{}:{}:{}",
        x.stx_dev_major, x.stx_dev_minor, x.stx_ino
    ))
}

/// The `key=value` lines of a `host-lock close-semantics` run.
fn lock_facts(stdout: &str) -> BTreeMap<&str, &str> {
    stdout.lines().filter_map(|l| l.split_once('=')).collect()
}

/// `flock` through `LinuxHost` on the host lock file (opened read-only by
/// tester, as diskgc will): a lock is released only by closing its fd, so
/// a child that inherited the fd keeps it after the parent closes its copy
/// (a `flock(LOCK_UN)` would have dropped it for both); a held lock makes
/// `try` busy and `wait` give up at its deadline; a lock released during a
/// wait is acquired.
pub fn host_flock_release_only_by_close(ctx: &Ctx) -> TestResult {
    const LOCK: &str = crate::suite::HOST_LOCK;
    let oracle = Oracle::arm(Fixture::new())?;

    let semantics = ctx.probe_as_tester(&["host-lock", "close-semantics", LOCK])?;
    semantics.expect_code(0)?;
    let facts = lock_facts(&semantics.stdout);
    info!("close semantics: {facts:?}");
    for fact in [
        "held_while_open",
        "released_on_close",
        "held_after_parent_close",
        "released_after_last_close",
    ] {
        ensure!(
            facts.get(fact) == Some(&"yes"),
            "{fact}: {:?}\n{}",
            facts.get(fact),
            semantics.stdout
        );
    }

    // The harness holds the lock (its own fd, untraced).
    let holder = std::fs::File::open(LOCK).map_err(|e| format!("open {LOCK}: {e}"))?;
    rustix::fs::flock(
        &holder,
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    )
    .map_err(|e| format!("harness flock {LOCK}: {e}"))?;
    let busy = ctx.probe_as_tester(&["host-lock", "try", LOCK])?;
    let timed_out = ctx.probe_as_tester(&["host-lock", "wait", LOCK, "300"])?;
    // The probe writes this marker only after its own fd observed Busy.
    // Waiting for the marker prevents a slow strace startup from turning
    // this into an uncontended acquire after the holder has gone away.
    let busy_marker = ctx.scratch.path("host-lock-wait-busy");
    let release = std::thread::spawn({
        let busy_marker = busy_marker.clone();
        move || -> TestResult {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !busy_marker.exists() {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "wait probe did not report a Busy lock at {}",
                        busy_marker.display()
                    ));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            std::thread::sleep(Duration::from_millis(300));
            drop(holder);
            Ok(())
        }
    });
    let acquired =
        ctx.probe_as_tester(&["host-lock", "wait", LOCK, "5000", to_str(&busy_marker)?])?;
    release
        .join()
        .map_err(|_| "the releasing thread panicked".to_owned())??;
    ensure!(
        std::fs::read(&busy_marker)
            .map_err(|e| format!("read busy marker {}: {e}", busy_marker.display()))?
            == b"busy\n",
        "the wait probe did not report Busy"
    );
    let verdict = oracle.judge_runs(&[&semantics, &busy, &timed_out, &acquired])?;

    busy.expect_code(0)?;
    ensure!(
        busy.stdout.trim() == "busy",
        "try while held: {:?}",
        busy.stdout
    );
    let after_ms = |run: &Run, outcome: &str| -> Result<u64, String> {
        run.expect_code(0)?;
        run.stdout
            .trim()
            .strip_prefix(outcome)
            .and_then(|rest| rest.trim().strip_prefix("after_ms="))
            .and_then(|ms| ms.parse().ok())
            .ok_or_else(|| format!("expected {outcome}, got {:?}", run.stdout))
    };
    let waited = after_ms(&timed_out, "DeadlinePassed")?;
    info!("wait with a 300 ms deadline gave up after {waited} ms");
    ensure!(
        (300..2000).contains(&waited),
        "deadline 300 ms, gave up after {waited} ms"
    );
    let waited = after_ms(&acquired, "Acquired")?;
    info!("lock released after a Busy probe and ~300 ms was acquired after {waited} ms");
    ensure!(
        (250..5000).contains(&waited),
        "acquired after {waited} ms; release was scheduled 300 ms after Busy"
    );
    verdict.assert_clean()
}

/// TASK-64 G: measure a real in-use scan with 0, 1, 4 and 8 bounded fork
/// workers in a disposable container. The probe samples until a scan holds
/// a whole child's life, or fails as `not exercised` (TASK-197). The exact
/// counts and times are diagnostic; after all workers are reaped, an unused
/// candidate must clear.
pub fn host_inuse_scan_settles_after_fork_churn(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("fork-churn-free", 0o755, TESTER)?;
    s.write_file("fork-churn-free/file", b"unused", 0o644, TESTER)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let run = ctx.probe_as_tester(&["host-inuse-fork-churn", "/tmp/fork-churn-free"])?;
    let verdict = oracle.judge(&run)?;
    run.expect_code(0)?;
    let lines: Vec<_> = run.stdout.lines().collect();
    ensure!(
        lines.len() == 5,
        "expected four measurements and post-storm result:\n{}",
        run.stdout
    );
    for (line, workers) in lines[..4].iter().zip([0, 1, 4, 8]) {
        ensure!(
            line.starts_with(&format!("workers={workers} ")),
            "missing {workers}-worker measurement: {line:?}"
        );
        info!("fork churn: {line}");
    }
    ensure!(
        lines[4].starts_with("post_storm ") && lines[4].ends_with(" clear=true"),
        "post-storm unused candidate did not clear: {:?}",
        lines[4]
    );
    info!("fork churn: {}", lines[4]);
    verdict.assert_clean()
}

/// TASK-64 G: a 30-second load measurement of the actual Linux checker.
/// The probe is read-only and untraced to keep 8 workers' fork logs out of
/// the harness record store. It runs only inside the disposable container.
pub fn host_inuse_scan_under_sustained_fork_churn(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("sustained-churn-free", 0o755, ROOT)?;
    let file = s.write_file("sustained-churn-free/file", b"unused", 0o644, ROOT)?;
    let run = ctx.probe_as_root(&["host-inuse-sustained-churn", "/tmp/sustained-churn-free"])?;
    ensure!(
        run.status.success(),
        "sustained fork churn failed: status={} stdout={} stderr={}",
        run.status,
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    let report = String::from_utf8(run.stdout).map_err(|e| format!("probe output UTF-8: {e}"))?;
    ensure!(
        report.starts_with("elapsed_ms=") && report.trim_end().ends_with("post_load_clear=true"),
        "missing sustained load measurement: {report:?}"
    );
    ensure!(
        std::fs::read(&file).map_err(|e| format!("read {}: {e}", file.display()))? == b"unused",
        "read-only load probe changed its candidate fixture"
    );
    info!("sustained fork churn: {}", report.trim());
    Ok(())
}

/// Pin a live child through LinuxHost, then prove that LinuxHost recognizes
/// its release after reap. In the pinned 6.6 VM, the independent syscall
/// probe establishes POLLIN without HUP, so the final answer bites the
/// pidfd_send_signal(0) fallback itself.
pub fn host_pidfd_release_after_reap(ctx: &Ctx) -> TestResult {
    let oracle = Oracle::arm(Fixture::new())?;
    let run = ctx.probe_as_tester(&["host-pidfd-release"])?;
    let verdict = oracle.judge(&run)?;
    run.expect_code(0)?;
    ensure!(
        run.stdout.trim() == "live_released=false reaped_released=true",
        "LinuxHost pidfd release result: {:?}",
        run.stdout
    );
    info!("LinuxHost pidfd: {}", run.stdout.trim());
    verdict.assert_clean()
}

/// The `/proc` readers on a process using scratch objects: an open file
/// (fd), a working directory, its executable (exe and a mapping) and its
/// initial environment, matched by (device, inode) against the harness's
/// own statx. Another user's process is `denied`, an exited one `gone`.
pub fn host_proc_readers(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("held", 0o755, TESTER)?;
    let held = s.write_file("held/file", b"held", 0o644, TESTER)?;
    let cwd = s.mkdir("cwd-dir", 0o755, TESTER)?;
    s.mkdir("bin", 0o755, TESTER)?;
    let exe_bytes = std::fs::read(&ctx.self_exe)
        .map_err(|e| format!("read {}: {e}", ctx.self_exe.display()))?;
    let holder_exe = s.write_file("bin/holder", &exe_bytes, 0o755, TESTER)?;

    let id = dev_ino;

    // The process using them: a fixture, untraced, running as tester.
    let mut holder = Command::new(&ctx.self_exe)
        .args(["as-tester", "--"])
        .arg(&holder_exe)
        .args(["probe", "hold"])
        .arg(&held)
        .arg(&cwd)
        .env_clear()
        .env("PATH", "/bin")
        .env("TMPDIR", "/tmp/envdir")
        .env("DISKGC_E2E_EQ", "a=b")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn holder: {e}"))?;
    let mut ready = String::new();
    BufReader::new(holder.stdout.take().ok_or("holder stdout")?)
        .read_line(&mut ready)
        .map_err(|e| format!("read holder: {e}"))?;
    ensure!(ready.trim() == "ready", "holder said {ready:?}");
    let holder_pid = holder.id().to_string();

    let gone_pid = {
        let mut child = Command::new("/bin/true")
            .spawn()
            .map_err(|e| format!("spawn /bin/true: {e}"))?;
        child.wait().map_err(|e| format!("wait /bin/true: {e}"))?;
        child.id().to_string()
    };
    let root_pid = std::process::id().to_string();

    let oracle = Oracle::arm(Fixture::new())?;
    let using = ctx.probe_as_tester(&["host-proc", &holder_pid])?;
    let denied = ctx.probe_as_tester(&["host-proc", &root_pid])?;
    let gone = ctx.probe_as_tester(&["host-proc", &gone_pid])?;
    drop(holder.stdin.take());
    let status = holder.wait().map_err(|e| format!("wait holder: {e}"))?;
    let verdict = oracle.judge_runs(&[&using, &denied, &gone])?;
    ensure!(status.success(), "holder exited with {status}");

    using.expect_code(0)?;
    info!("host-proc {holder_pid} (holder):\n{}", using.stdout);
    let lines: Vec<&str> = using.stdout.lines().collect();
    let has = |line: &str| lines.contains(&line);
    let held_id = id(&held)?;
    ensure!(
        lines
            .iter()
            .any(|l| l.starts_with("fd ") && l.ends_with(&format!(" {held_id}"))),
        "no fd on {} ({held_id}):\n{}",
        held.display(),
        using.stdout
    );
    for line in [
        format!("cwd {}", id(&cwd)?),
        format!("root {}", id(Path::new("/"))?),
        format!("exe {}", id(&holder_exe)?),
        format!("map {}", id(&holder_exe)?),
        "env TMPDIR=/tmp/envdir".to_owned(),
        "env DISKGC_E2E_EQ=a=b".to_owned(),
    ] {
        ensure!(has(&line), "missing {line:?} in:\n{}", using.stdout);
    }

    for (run, pid, what) in [(&denied, &root_pid, "denied"), (&gone, &gone_pid, "gone")] {
        run.expect_code(0)?;
        info!("host-proc {pid} ({what}):\n{}", run.stdout);
        for reader in ["fds", "cwd", "maps", "environ"] {
            let line = format!("{reader} {what}");
            ensure!(
                run.stdout.lines().any(|l| l == line),
                "missing {line:?} in:\n{}",
                run.stdout
            );
        }
    }
    verdict.assert_clean()
}

/// A kernel older than 5.8 exits 2 at startup. Simulated by the kernel
/// itself: the `UNAME26` personality makes uname report `2.6.x`. `--version`
/// stays exempt, and without the personality the same command line reaches
/// the (scaffold) usage error instead.
pub fn host_kernel_older_than_5_8_exits_2(ctx: &Ctx) -> TestResult {
    let diskgc = ctx.diskgc();
    let diskgc = to_str(&diskgc)?;
    let self_exe = to_str(&ctx.self_exe)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let seen = ctx.run_as_tester(
        &ctx.self_exe,
        &["uname-2.6", "--", self_exe, "probe", "host-kernel"],
    )?;
    let old = ctx.run_as_tester(&ctx.self_exe, &["uname-2.6", "--", diskgc, "ensure"])?;
    let version = ctx.run_as_tester(&ctx.self_exe, &["uname-2.6", "--", diskgc, "--version"])?;
    let current = ctx.run_as_tester(&ctx.diskgc(), &["ensure"])?;
    let verdict = oracle.judge_runs(&[&seen, &old, &version, &current])?;

    seen.expect_code(0)?;
    info!("under UNAME26: {}", seen.stdout.trim());
    ensure!(
        seen.stdout.starts_with("release=2.6.") && seen.stdout.contains(" refused -- "),
        "the personality did not make the kernel look old: {:?}",
        seen.stdout
    );
    old.expect_code(2)?;
    info!("diskgc on a 2.6 kernel: {}", old.stderr.trim());
    ensure!(
        old.stderr.contains("needs Linux 5.8 or newer"),
        "exit 2 but not for the kernel: {:?}",
        old.stderr
    );
    version.expect_code(0)?;
    current.expect_code(2)?;
    ensure!(
        !current.stderr.contains("Linux 5.8") && current.stderr.contains("usage: diskgc"),
        "on the real kernel, expected the usage error: {:?}",
        current.stderr
    );
    verdict.assert_clean()
}

/// Runs the `hold-thread` fixture in `mode` as tester, checks (as root)
/// that the thread-group leader's own `/proc/<pid>` entries do NOT show the
/// held file and working directory, then that the `/proc` readers, run as
/// tester, find both through the worker thread.
fn thread_holder_is_seen(ctx: &Ctx, mode: &str) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("held", 0o755, TESTER)?;
    let held = s.write_file("held/file", b"held", 0o644, TESTER)?;
    let cwd = s.mkdir("cwd-dir", 0o755, TESTER)?;
    let (held_id, cwd_id) = (dev_ino(&held)?, dev_ino(&cwd)?);

    let mut holder = Command::new(&ctx.self_exe)
        .args(["as-tester", "--"])
        .arg(&ctx.self_exe)
        .args(["probe", "hold-thread"])
        .arg(&held)
        .arg(&cwd)
        .arg(mode)
        .env_clear()
        .env("PATH", "/bin")
        .env("TMPDIR", "/tmp/envdir")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn holder: {e}"))?;
    let mut ready = String::new();
    BufReader::new(holder.stdout.take().ok_or("holder stdout")?)
        .read_line(&mut ready)
        .map_err(|e| format!("read holder: {e}"))?;
    ensure!(ready.trim() == "ready", "holder ({mode}) said {ready:?}");
    let pid = holder.id();

    // The scenario is real: the leader's entries alone miss the use.
    let leader_fds: Vec<String> = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .map(|dir| {
            dir.filter_map(|e| dev_ino(&std::fs::canonicalize(e.ok()?.path()).ok()?).ok())
                .collect()
        })
        .unwrap_or_default();
    let leader_cwd = std::fs::canonicalize(format!("/proc/{pid}/cwd"))
        .map_err(|e| e.to_string())
        .and_then(|p| dev_ino(&p));
    info!("{mode}: leader fds {leader_fds:?}, leader cwd {leader_cwd:?}");
    ensure!(
        !leader_fds.contains(&held_id),
        "{mode}: /proc/{pid}/fd shows the held file; the fixture does not hide it"
    );
    ensure!(
        leader_cwd.as_ref() != Ok(&cwd_id),
        "{mode}: /proc/{pid}/cwd is the held cwd; the fixture does not hide it"
    );

    let oracle = Oracle::arm(Fixture::new())?;
    let probe = ctx.probe_as_tester(&["host-proc", &pid.to_string()]);
    drop(holder.stdin.take());
    let status = holder.wait().map_err(|e| format!("wait holder: {e}"))?;
    let probe = probe?;
    let verdict = oracle.judge(&probe)?;
    ensure!(status.success(), "holder ({mode}) exited with {status}");

    probe.expect_code(0)?;
    info!("host-proc {pid} ({mode}):\n{}", probe.stdout);
    let lines: Vec<&str> = probe.stdout.lines().collect();
    ensure!(
        lines
            .iter()
            .any(|l| l.starts_with("fd ") && l.ends_with(&format!(" {held_id}"))),
        "{mode}: the worker's open file {held_id} was not found:\n{}",
        probe.stdout
    );
    ensure!(
        lines.contains(&format!("cwd {cwd_id}").as_str()),
        "{mode}: the worker's cwd {cwd_id} was not found:\n{}",
        probe.stdout
    );
    for prefix in ["map ", "exe ", "env TMPDIR=/tmp/envdir"] {
        ensure!(
            lines.iter().any(|l| l.starts_with(prefix)),
            "{mode}: no {prefix:?} line (read through a live task):\n{}",
            probe.stdout
        );
    }
    verdict.assert_clean()
}

/// The main thread exited (`pthread_exit`) while a worker holds a file and
/// a working directory: the leader is a zombie whose `fd` is empty and
/// whose `cwd` is gone, yet the process is using both, and the readers say
/// so (through `/proc/<pid>/task/<tid>`), not `gone`.
pub fn host_proc_readers_see_threads_after_main_exits(ctx: &Ctx) -> TestResult {
    thread_holder_is_seen(ctx, "main-exits")
}

/// A thread with its own fd table and filesystem state
/// (`unshare(CLONE_FILES | CLONE_FS)`) holds a file and a working
/// directory that the leader's entries never show; the readers find both.
pub fn host_proc_readers_see_threads_with_own_fd_table(ctx: &Ctx) -> TestResult {
    thread_holder_is_seen(ctx, "unshare")
}

/// How long the watch probe calls `proc_maps` while the leader exits.
const EXIT_WATCH_MS: u64 = 4000;

/// Regression (exit cleanup): the thread-group leader exits slowly (closing
/// hundreds of thousands of files, `PF_EXITING` set, its memory map already
/// released, state still `R`/`D`) while a worker keeps the process and its
/// mappings alive. `proc_maps` must never answer from the exiting leader:
/// every call either contains the worker-visible mapping of the holder's
/// executable or is a conservative error, never `Ok` without it and never
/// `Gone`. The probe also counts how often it saw the leader exiting, so
/// the window is proven to have been exercised.
pub fn host_proc_maps_while_the_leader_exits_slowly(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("bin", 0o755, TESTER)?;
    let exe_bytes = std::fs::read(&ctx.self_exe)
        .map_err(|e| format!("read {}: {e}", ctx.self_exe.display()))?;
    let holder_exe = s.write_file("bin/holder", &exe_bytes, 0o755, TESTER)?;
    let exe_id = dev_ino(&holder_exe)?;

    let mut holder = Command::new(&ctx.self_exe)
        .args(["as-tester", "--"])
        .arg(&holder_exe)
        .args(["probe", "hold-exit-slowly"])
        .env_clear()
        .env("PATH", "/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn holder: {e}"))?;
    let mut ready = String::new();
    BufReader::new(holder.stdout.take().ok_or("holder stdout")?)
        .read_line(&mut ready)
        .map_err(|e| format!("read holder: {e}"))?;
    let opened = ready
        .trim()
        .strip_prefix("ready ")
        .ok_or_else(|| format!("holder said {ready:?}"))?
        .to_owned();
    let pid = holder.id().to_string();
    info!("holder {pid} holds {opened} open files in the leader's own table");

    let oracle = Oracle::arm(Fixture::new())?;
    let mut stdin = holder.stdin.take().ok_or("holder stdin")?;
    let watch = std::thread::scope(|scope| {
        let watcher = scope.spawn(|| {
            ctx.probe_as_tester(&["host-maps-watch", &pid, &exe_id, &EXIT_WATCH_MS.to_string()])
        });
        // Let the watch start, then end the main thread.
        std::thread::sleep(Duration::from_millis(500));
        let go = std::io::Write::write_all(&mut stdin, b"go\n")
            .map_err(|e| format!("tell the holder to go: {e}"));
        let watched = watcher
            .join()
            .map_err(|_| "the watch thread panicked".to_owned());
        go.and(watched)
    })?;
    drop(stdin);
    let status = holder.wait().map_err(|e| format!("wait holder: {e}"))?;
    let watch = watch?;
    let verdict = oracle.judge(&watch)?;
    ensure!(status.success(), "holder exited with {status}");

    watch.expect_code(0)?;
    info!("maps watch: {}", watch.stdout.trim());
    let counts: BTreeMap<&str, u64> = watch
        .stdout
        .split_whitespace()
        .filter_map(|kv| kv.split_once('='))
        .filter_map(|(k, v)| Some((k, v.parse().ok()?)))
        .collect();
    let count = |k: &str| counts.get(k).copied().unwrap_or(0);
    ensure!(
        count("leader_exiting_seen") > 0,
        "the watch never saw the leader exiting; the window was not exercised: {}",
        watch.stdout
    );
    ensure!(
        count("without") == 0 && count("gone") == 0,
        "proc_maps answered without the worker's mapping (or gone) while the leader exited: {}",
        watch.stdout
    );
    ensure!(
        count("with") > 0,
        "proc_maps never found the mapping: {}",
        watch.stdout
    );
    verdict.assert_clean()
}

/// `LinuxHost::unlinkat` inside the sandbox (task-50): the pre-release
/// sandbox gate lets it through, a file and an empty directory are really
/// deleted, a non-empty directory is a kernel refusal (`ENOTEMPTY`, not the
/// gate), and nothing else changes. Two oracles judge the same run: one with
/// exactly the two entries eligible must be clean, one with nothing eligible
/// must flag exactly those two, in the snapshot and in the strace record.
pub fn host_unlinkat_deletes_inside_the_sandbox(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("del", 0o755, TESTER)?;
    let file = s.write_file("del/file", b"file", 0o644, TESTER)?;
    let empty_dir = s.mkdir("del/empty-dir", 0o755, TESTER)?;
    s.mkdir("del/full-dir", 0o755, TESTER)?;
    let inner = s.write_file("del/full-dir/inner", b"inner", 0o644, TESTER)?;
    let sibling = s.write_file("del/sibling", b"sibling", 0o644, TESTER)?;
    let deleted: BTreeSet<PathBuf> = [file.clone(), empty_dir.clone()].into();

    let oracle = Oracle::arm(
        Fixture::new()
            .eligible(file.clone())
            .eligible(empty_dir.clone()),
    )?;
    let strict = Oracle::arm(Fixture::new())?;
    let run = ctx.probe_as_tester(&[
        "host-unlink",
        "/tmp/del",
        "file:file",
        "empty-dir:dir",
        "full-dir:dir",
    ])?;
    let verdict = oracle.judge(&run)?;
    let strict_verdict = strict.judge(&run)?;

    run.expect_code(0)?;
    info!("host-unlink:\n{}", run.stdout);
    let lines = indexed(&run.stdout)?;
    for (i, want) in [(0, "ok"), (1, "ok"), (2, "err ENOTEMPTY ")] {
        let got = lines.get(&i).map(String::as_str).unwrap_or("");
        ensure!(
            got.starts_with(want),
            "entry {i}: expected {want:?}, got {got:?}"
        );
    }
    for gone in &deleted {
        ensure!(
            std::fs::symlink_metadata(gone).is_err(),
            "{} still exists",
            gone.display()
        );
    }
    for kept in [&inner, &sibling] {
        ensure!(kept.exists(), "{} was deleted", kept.display());
    }
    ensure!(
        strict_verdict.snapshot_paths() == deleted && strict_verdict.traced_paths() == deleted,
        "the oracle with nothing eligible should flag exactly {deleted:?}: snapshot {:?}, \
         traced {:?}\n{}",
        strict_verdict.snapshot_paths(),
        strict_verdict.traced_paths(),
        strict_verdict.report()
    );
    verdict.assert_clean()
}
