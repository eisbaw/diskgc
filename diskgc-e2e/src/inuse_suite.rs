//! Container tests of the in-use check and the deletion step around it
//! (task-6; docs/safety.md, In use and Deletion), through the test driver's
//! `reclaim` (diskgc-core `inuse::delete_unused` over `LinuxHost`: lock,
//! walk, scan of every visible process, delete) and `held`, run as `tester`
//! under strace. Real processes hold the candidates' files: fixtures from
//! [`crate::inuse_probe`], started untraced. The deletion oracle judges
//! every run: only what a test declares eligible may disappear.

use std::path::Path;

use crate::controlled::ControlLine;
use crate::holder::Holder;
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, TestResult};
use crate::under_test::Run;
use crate::{TESTER, ensure, info};

/// The driver's single result line (control lines left out).
fn outcome(run: &Run) -> Result<String, String> {
    let mut lines = Vec::new();
    for line in run.stdout.lines() {
        if ControlLine::parse(line)?.is_none() {
            lines.push(line.to_owned());
        }
    }
    match lines.as_slice() {
        [line] => Ok(line.clone()),
        other => Err(format!(
            "expected one result line, got {other:?}\n--- stderr\n{}",
            run.stderr
        )),
    }
}

/// Runs the driver's `reclaim` (with `flags` first) on `/tmp/<rel>`.
fn reclaim(ctx: &Ctx, flags: &[&str], rel: &str) -> Result<Run, String> {
    let target = ctx.scratch.path(rel);
    let target = target.to_str().ok_or("non-UTF-8 path")?;
    let mut args = vec!["reclaim"];
    args.extend_from_slice(flags);
    args.push(target);
    ctx.run_as_tester(&ctx.driver(), &args)
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Checks a reclaim's line starts with `want` and its exit code (0 when
/// removed, 1 otherwise).
fn expect_line(run: &Run, want: &str) -> TestResult {
    let line = outcome(run)?;
    info!("{line}");
    ensure!(
        line.starts_with(want),
        "expected {want:?}..., got {line:?}\n--- stderr\n{}",
        run.stderr
    );
    run.expect_code(if want.starts_with("0 removed") { 0 } else { 1 })?;
    Ok(())
}

/// A tester directory `rel` holding the tester file `rel/file`.
fn dir_with_file(ctx: &Ctx, rel: &str) -> TestResult {
    ctx.scratch.mkdir(rel, 0o755, TESTER)?;
    ctx.scratch
        .write_file(&format!("{rel}/file"), b"in use?", 0o644, TESTER)?;
    Ok(())
}

/// AC #1: an open file, a working directory, a memory mapping and a path
/// in the initial environment (`TMPDIR`) each keep their candidate
/// (`in_use`, naming the process); a candidate none of them uses is
/// deleted.
pub fn inuse_a_held_file_cwd_mapping_or_environment_keeps_the_candidate(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    for rel in ["fd", "cwd", "map", "env", "free"] {
        dir_with_file(ctx, rel)?;
    }
    s.write_file("outside", b"outside", 0o644, TESTER)?;
    let outside = s.path("outside");
    let outside = outside.to_str().ok_or("path")?;
    let fd = Holder::tester(ctx, &["hold", "/tmp/fd/file", "/"], &[])?;
    let cwd = Holder::tester(ctx, &["hold", outside, "/tmp/cwd"], &[])?;
    let map = Holder::tester(ctx, &["hold-map", "/tmp/map/file"], &[])?;
    let env = Holder::tester(ctx, &["hold", outside, "/"], &[("TMPDIR", "/tmp/env")])?;

    let oracle = Oracle::arm(Fixture::new().eligible(s.path("free")))?;
    let runs: Vec<Run> = ["fd", "cwd", "map", "env", "free"]
        .iter()
        .map(|rel| reclaim(ctx, &[], rel))
        .collect::<Result<_, _>>()?;
    let verdict = oracle.judge_runs(&runs.iter().collect::<Vec<_>>())?;
    let pid = |h: &Holder| h.pid();
    expect_line(
        &runs[0],
        &format!("0 skip in_use \"/tmp/fd/file\" -- pid {} (", pid(&fd)),
    )?;
    expect_line(
        &runs[1],
        &format!("0 skip in_use \"/tmp/cwd\" -- pid {} (", pid(&cwd)),
    )?;
    expect_line(
        &runs[2],
        &format!("0 skip in_use \"/tmp/map/file\" -- pid {} (", pid(&map)),
    )?;
    ensure!(
        outcome(&runs[2])?.contains("mapped"),
        "the mapping, not something else, keeps map"
    );
    expect_line(
        &runs[3],
        &format!("0 skip in_use \"/tmp/env\" -- pid {} (", pid(&env)),
    )?;
    ensure!(
        outcome(&runs[3])?.contains("initial environment (TMPDIR=/tmp/env)"),
        "the environment, not something else, keeps env"
    );
    expect_line(&runs[4], "0 removed entries=2 ")?;
    for h in [fd, cwd, map, env] {
        h.finish()?;
    }
    ensure!(
        exists(&s.path("fd/file")) && !exists(&s.path("free")),
        "files"
    );
    verdict.assert_clean()
}

/// AC #1: a directory another process holds `flock`ed (shared, as a
/// cooperating tool would) is `locked` and untouched; once released, the
/// same step deletes it. A directory the user cannot open to lock (its own,
/// mode 000) is the walk's `unreadable` skip, not an error of the run.
pub fn inuse_a_flocked_directory_is_locked(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    dir_with_file(ctx, "lk")?;
    dir_with_file(ctx, "noread")?;
    s.chmod("noread", 0o000)?;
    let dir = std::fs::File::open(s.path("lk")).map_err(|e| format!("open lk: {e}"))?;
    rustix::fs::flock(&dir, rustix::fs::FlockOperation::NonBlockingLockShared)
        .map_err(|e| format!("flock lk: {e}"))?;
    let oracle = Oracle::arm(Fixture::new().eligible(s.path("lk")))?;
    let locked = reclaim(ctx, &[], "lk")?;
    let still_there = exists(&s.path("lk/file"));
    drop(dir);
    let freed = reclaim(ctx, &[], "lk")?;
    let noread = reclaim(ctx, &[], "noread")?;
    let verdict = oracle.judge_runs(&[&locked, &freed, &noread])?;
    expect_line(&noread, "0 skip unreadable \"/tmp/noread\" -- ")?;
    expect_line(&locked, "0 skip locked \"/tmp/lk\" -- ")?;
    ensure!(
        still_there && locked.events.is_empty(),
        "a locked candidate was touched"
    );
    expect_line(&freed, "0 removed entries=2 ")?;
    verdict.assert_clean()
}

/// AC #2, the heuristic's known limit: a variable set after `exec` (what
/// `export` does) is not in `/proc/<pid>/environ`, so it does not keep the
/// candidate it names, while the same variable in the initial environment
/// does.
pub fn inuse_an_export_after_exec_is_invisible(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    dir_with_file(ctx, "exported")?;
    dir_with_file(ctx, "initial")?;
    let exported = Holder::tester(ctx, &["hold-export", "TMPDIR", "/tmp/exported"], &[])?;
    let initial = Holder::tester(
        ctx,
        &["hold-export", "UNRELATED", "x"],
        &[("TMPDIR", "/tmp/initial")],
    )?;
    let oracle = Oracle::arm(Fixture::new().eligible(s.path("exported")))?;
    let gone = reclaim(ctx, &[], "exported")?;
    let kept = reclaim(ctx, &[], "initial")?;
    let verdict = oracle.judge_runs(&[&gone, &kept])?;
    expect_line(&gone, "0 removed entries=2 ")?;
    expect_line(
        &kept,
        &format!("0 skip in_use \"/tmp/initial\" -- pid {} (", initial.pid()),
    )?;
    exported.finish()?;
    initial.finish()?;
    verdict.assert_clean()
}

/// AC #3: a process in another mount namespace, which sees the candidate
/// through a bind mount at another path, is matched by device and inode:
/// its fd's path text names a path that does not exist for diskgc.
pub fn inuse_a_process_in_another_mount_namespace_is_matched(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    dir_with_file(ctx, "src")?;
    s.mkdir("elsewhere", 0o755, TESTER)?;
    let holder = Holder::root(ctx, &["ns-hold", "/tmp/src", "/tmp/elsewhere", "file"])?;
    let text = std::fs::read_dir(format!("/proc/{}/fd", holder.pid()))
        .map_err(|e| format!("list the holder's fds: {e}"))?
        .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .find(|t| t.ends_with("file"))
        .ok_or("the holder has no fd on the file")?;
    info!("the holder's fd reads {}", text.display());
    ensure!(
        text == Path::new("/tmp/elsewhere/file") && !exists(&text),
        "the fixture should name the file by a path diskgc cannot see, got {}",
        text.display()
    );
    let oracle = Oracle::arm(Fixture::new())?;
    let run = reclaim(ctx, &[], "src")?;
    let verdict = oracle.judge(&run)?;
    expect_line(&run, "0 skip in_use \"/tmp/src")?;
    ensure!(
        outcome(&run)?.contains(&format!("pid {} (", holder.pid())),
        "not matched to the holder: {}",
        outcome(&run)?
    );
    holder.finish()?;
    verdict.assert_clean()
}

/// AC #4 as narrowed by the user decision (2026-10-17, docs/safety.md, In
/// use): a non-dumpable process of the user and a setuid process are noted
/// and do not keep anything (their files are deleted: the accepted cost);
/// a dumpable process of the user that the kernel still refuses (it runs
/// with another group) keeps every candidate while it runs.
pub fn inuse_unreadable_processes_block_unless_their_status_explains_them(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    for rel in ["nd", "grp", "suid"] {
        dir_with_file(ctx, rel)?;
    }
    let me = ctx.self_exe.to_str().ok_or("path")?;
    let oracle = Oracle::arm(
        Fixture::new()
            .eligible(s.path("nd"))
            .eligible(s.path("suid")),
    )?;
    let nd = Holder::tester(ctx, &["hold-nondumpable", "/tmp/nd/file", "/tmp/nd"], &[])?;
    let nd_run = reclaim(ctx, &[], "nd")?;
    nd.finish()?;
    let grp = Holder::root(
        ctx,
        &[
            "exec-as",
            "1000",
            "1000",
            "1001",
            "--",
            me,
            "probe",
            "hold",
            "/tmp/grp/file",
            "/",
        ],
    )?;
    let grp_run = reclaim(ctx, &[], "suid")?;
    grp.finish()?;
    let suid = Holder::root(
        ctx,
        &[
            "exec-as",
            "1000",
            "0",
            "1000",
            "--",
            me,
            "probe",
            "hold",
            "/tmp/suid/file",
            "/",
        ],
    )?;
    let suid_run = reclaim(ctx, &[], "suid")?;
    let suid_pid = suid.pid();
    suid.finish()?;
    let verdict = oracle.judge_runs(&[&nd_run, &grp_run, &suid_run])?;

    expect_line(&nd_run, "0 removed entries=2 ")?;
    ensure!(
        nd_run.stderr.contains("non-dumpable process of the user"),
        "the non-dumpable process is not noted:\n{}",
        nd_run.stderr
    );
    expect_line(&grp_run, "0 skip in_use \"/tmp/suid\" -- pid ")?;
    ensure!(
        outcome(&grp_run)?.contains("cannot be inspected"),
        "{}",
        outcome(&grp_run)?
    );
    expect_line(&suid_run, "0 removed entries=2 ")?;
    ensure!(
        suid_run
            .stderr
            .contains(&format!("pid {suid_pid} (diskgc-e2e, uid 1000 0 0 0")),
        "the setuid process is not noted:\n{}",
        suid_run.stderr
    );
    verdict.assert_clean()
}

/// Under the "live only" IPC policy (`--ipc-live`): a FIFO a process holds
/// open keeps its candidate (`in_use`), a socket bound in diskgc's network
/// namespace keeps its (`contains_ipc`, found by the kernel's socket
/// diagnostics), stale ones are deleted with their candidate; the default
/// policy keeps any.
pub fn inuse_only_live_sockets_and_fifos_keep_their_candidate(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    for rel in ["fifo", "sock", "stale", "default"] {
        dir_with_file(ctx, rel)?;
    }
    s.fifo("fifo/p", 0o600, TESTER)?;
    for rel in ["stale", "default"] {
        s.fifo(&format!("{rel}/p"), 0o600, TESTER)?;
        s.socket(&format!("{rel}/s"), TESTER)?;
    }
    let fifo = Holder::tester(ctx, &["hold-fifo", "/tmp/fifo/p"], &[])?;
    let sock = Holder::tester(ctx, &["bind-socket", "/tmp/sock/s"], &[])?;
    let oracle = Oracle::arm(Fixture::new().eligible(s.path("stale")))?;
    let runs = [
        reclaim(ctx, &["--ipc-live"], "fifo")?,
        reclaim(ctx, &["--ipc-live"], "sock")?,
        reclaim(ctx, &["--ipc-live"], "stale")?,
        reclaim(ctx, &[], "default")?,
    ];
    let verdict = oracle.judge_runs(&runs.iter().collect::<Vec<_>>())?;
    expect_line(
        &runs[0],
        &format!("0 skip in_use \"/tmp/fifo/p\" -- pid {} (", fifo.pid()),
    )?;
    expect_line(
        &runs[1],
        "0 skip contains_ipc \"/tmp/sock/s\" -- a socket bound",
    )?;
    expect_line(&runs[2], "0 removed entries=4 ")?;
    expect_line(&runs[3], "0 skip contains_ipc \"/tmp/default/")?;
    fifo.finish()?;
    sock.finish()?;
    verdict.assert_clean()
}

/// The process-level handoff (task-3 review): the driver lists the
/// processes and pauses before reading the first; meanwhile the holder
/// forks a child that inherits its fd on the candidate's file and closes
/// its own. Read after that, the holder shows nothing, but the child, born
/// before the read, is in the next listing: `in_use` by the child.
pub fn inuse_a_child_forked_before_its_parent_was_read_is_found(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    dir_with_file(ctx, "fk")?;
    let mut holder = Holder::tester(ctx, &["hold-fork", "/tmp/fk/file"], &[])?;
    let oracle = Oracle::arm(Fixture::new())?;
    let mut run = ctx.spawn_as_tester(
        &ctx.driver(),
        &["--pause-at", "proc_fds:1", "reclaim", "/tmp/fk"],
    )?;
    let paused = run.wait_paused()?;
    holder.send("fork")?;
    let forked = holder.line()?;
    let child: u32 = forked
        .strip_prefix("forked ")
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| format!("holder said {forked:?}"))?;
    run.resume(&paused)?;
    let run = run.wait()?;
    let verdict = oracle.judge(&run)?;
    expect_line(
        &run,
        &format!("0 skip in_use \"/tmp/fk/file\" -- pid {child} ("),
    )?;
    holder.finish()?;
    ensure!(exists(&s.path("fk/file")), "the held file went");
    verdict.assert_clean()
}

/// `held_by_deleted_files`: a file deleted while a process keeps it open
/// is listed with that process and its allocated bytes.
pub fn inuse_held_by_deleted_files_lists_open_deleted_files(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.write_file("big", &vec![7u8; 1 << 20], 0o644, TESTER)?;
    let holder = Holder::tester(ctx, &["hold", "/tmp/big", "/"], &[])?;
    s.remove_file("big")?;
    let oracle = Oracle::arm(Fixture::new())?;
    let run = ctx.run_as_tester(&ctx.driver(), &["held", "/tmp"])?;
    let verdict = oracle.judge(&run)?;
    run.expect_code(0)?;
    let want = format!(
        "0 held pid={} command=diskgc-e2e bytes=1048576",
        holder.pid()
    );
    ensure!(
        run.stdout.lines().any(|l| l == want),
        "missing {want:?} in:\n{}",
        run.stdout
    );
    holder.finish()?;
    verdict.assert_clean()
}

/// AC #5: a running process mapping a library from an overlayfs mount is
/// seen. Memory maps can show the layer's device (and on older kernels
/// inode) instead of what `statx` of the overlay file shows; the harness
/// logs which this kernel does, and the candidate is `in_use` either way.
pub fn inuse_a_mapping_through_overlayfs_is_matched(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdirs("ov/lower/c", 0o755, TESTER)?;
    s.write_file("ov/lower/c/lib.so", &[0x7f; 8192], 0o755, TESTER)?;
    for rel in ["ov/upper", "ov/work", "ov/merged"] {
        s.mkdir(rel, 0o755, TESTER)?;
    }
    s.mount_overlay("ov/lower", "ov/upper", "ov/work", "ov/merged")?;
    let lib = s.path("ov/merged/c/lib.so");
    let holder = Holder::tester(ctx, &["hold-map", lib.to_str().ok_or("path")?], &[])?;
    let stat = rustix::fs::statx(
        rustix::fs::CWD,
        &lib,
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        rustix::fs::StatxFlags::INO,
    )
    .map_err(|e| format!("statx {}: {e}", lib.display()))?;
    let stat_id = format!(
        "{:x}:{:x} {}",
        stat.stx_dev_major, stat.stx_dev_minor, stat.stx_ino
    );
    let maps = std::fs::read_to_string(format!("/proc/{}/maps", holder.pid()))
        .map_err(|e| format!("read the holder's maps: {e}"))?;
    let line = maps
        .lines()
        .find(|l| l.ends_with("lib.so"))
        .ok_or("the holder does not map lib.so")?;
    info!("statx of the overlay file: {stat_id}; the holder's maps line: {line}");
    let oracle = Oracle::arm(Fixture::new())?;
    let run = reclaim(ctx, &[], "ov/merged/c")?;
    let verdict = oracle.judge(&run)?;
    expect_line(
        &run,
        &format!(
            "0 skip in_use \"/tmp/ov/merged/c/lib.so\" -- pid {} (",
            holder.pid()
        ),
    )?;
    holder.finish()?;
    verdict.assert_clean()
}

/// B2 of task-6's review: a process of tester's in a user namespace of its
/// own (`unshare -r`), root there with every capability, is still readable
/// for tester, who owns that namespace; its open file keeps the candidate.
/// (Were it ever refused, its different `uid_map` keeps it from being
/// excused as more capable.)
pub fn inuse_a_process_in_its_own_user_namespace_is_read(ctx: &Ctx) -> TestResult {
    dir_with_file(ctx, "uns")?;
    let holder = Holder::tester(ctx, &["hold-userns", "/tmp/uns/file"], &[])?;
    let map = std::fs::read_to_string(format!("/proc/{}/uid_map", holder.pid()))
        .map_err(|e| format!("read the holder's uid_map: {e}"))?;
    info!("the holder's uid_map: {}", map.trim());
    let oracle = Oracle::arm(Fixture::new())?;
    let run = reclaim(ctx, &[], "uns")?;
    let verdict = oracle.judge(&run)?;
    expect_line(
        &run,
        &format!("0 skip in_use \"/tmp/uns/file\" -- pid {} (", holder.pid()),
    )?;
    holder.finish()?;
    verdict.assert_clean()
}

/// The capability excuse in a rootless container (task-6 review round 2,
/// C2): a process with all of tester's ids, dumpable, holding one
/// permitted capability (`CAP_KILL`) that tester lacks, is refused to
/// tester by the kernel. diskgc here runs in the container's user
/// namespace, whose `uid_map` is not the full identity map, so it cannot
/// tell that namespace from a sibling and does not excuse the process: it
/// blocks (`in_use`, "cannot be inspected"). On a host's initial user
/// namespace the same process would be excused (model tests; the live
/// read-only scan of diskgc-linux notes `systemd --user` that way).
pub fn inuse_a_more_capable_process_blocks_outside_the_initial_namespace(ctx: &Ctx) -> TestResult {
    dir_with_file(ctx, "capable")?;
    let holder = Holder::root(ctx, &["hold-capable", "/tmp/capable/file"])?;
    let own_map = std::fs::read_to_string("/proc/self/uid_map")
        .map_err(|e| format!("read the container's uid_map: {e}"))?;
    info!("the container's uid_map: {}", own_map.trim());
    let oracle = Oracle::arm(Fixture::new())?;
    let run = reclaim(ctx, &[], "capable")?;
    let verdict = oracle.judge(&run)?;
    expect_line(
        &run,
        &format!(
            "0 skip in_use \"/tmp/capable\" -- pid {} (diskgc-e2e) cannot be inspected",
            holder.pid()
        ),
    )?;
    holder.finish()?;
    verdict.assert_clean()
}
