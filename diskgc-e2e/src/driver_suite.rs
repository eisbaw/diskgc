//! Container tests of the test driver and its controlled `Host` wrapper
//! (task-51; docs/testing.md, Controlled pause and kill points): the
//! harness pauses the driver at a chosen port call, changes the fixture
//! while it waits, resumes it, and kills it at a chosen call. The driver
//! runs as `tester` under strace like any code under test, and the deletion
//! oracle judges every run, the killed ones included.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::controlled::ControlLine;
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, TestResult};
use crate::under_test::Run;
use crate::{TESTER, ensure, info};

/// SIGKILL's number on Linux.
const SIGKILL: i32 = 9;

/// The result lines of a driver run (control lines left out).
fn results(run: &Run) -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    for line in run.stdout.lines() {
        if ControlLine::parse(line)?.is_none() {
            lines.push(line.to_owned());
        }
    }
    Ok(lines)
}

/// The control lines of a driver run.
fn controls(run: &Run) -> Result<Vec<ControlLine>, String> {
    run.stdout
        .lines()
        .filter_map(|line| ControlLine::parse(line).transpose())
        .collect()
}

fn ino(path: &Path) -> Result<u64, String> {
    std::fs::symlink_metadata(path)
        .map(|m| m.ino())
        .map_err(|e| format!("lstat {}: {e}", path.display()))
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Files `names` (tester's) in a new tester directory `dir` of the scratch.
fn files_in(ctx: &Ctx, dir: &str, names: &[&str]) -> TestResult {
    ctx.scratch.mkdir(dir, 0o755, TESTER)?;
    for name in names {
        ctx.scratch
            .write_file(&format!("{dir}/{name}"), name.as_bytes(), 0o644, TESTER)?;
    }
    Ok(())
}

/// A walk paused before its second directory listing (`read_dir:2`, of
/// `sub`) has listed the top level but not `sub`; a file the harness adds
/// to `sub` while it waits is in the listing after it resumes. A wrapper
/// that acted after the call, or not at all, would miss the file.
pub fn driver_walk_sees_a_change_made_while_paused(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    files_in(ctx, "w", &["a"])?;
    files_in(ctx, "w/sub", &["b"])?;
    s.symlink("w/link", Path::new("sub"))?;
    let oracle = Oracle::arm(Fixture::new())?;

    let mut run = ctx.spawn_as_tester(
        &ctx.driver(),
        &["--pause-at", "read_dir:2", "walk", "/tmp/w"],
    )?;
    let paused = run.wait_paused()?;
    ensure!(
        paused.op == "read_dir" && paused.nth == 2,
        "paused at the wrong call: {paused:?}"
    );
    s.write_file("w/sub/added-while-paused", b"new", 0o644, TESTER)?;
    run.resume(&paused)?;
    let run = run.wait()?;
    let verdict = oracle.judge(&run)?;

    run.expect_code(0)?;
    let got = results(&run)?;
    info!("walk:\n{}", got.join("\n"));
    let want = [
        "0 file \"a\"",
        "1 symlink \"link\"",
        "2 dir \"sub\"",
        "3 file \"sub/added-while-paused\"",
        "4 file \"sub/b\"",
    ];
    ensure!(got == want, "walk listed {got:?}, expected {want:?}");
    verdict.assert_clean()
}

/// Paused before the second `unlinkat` of a delete, the driver is blocked
/// (checked in `/proc`), the first entry is gone and the second still
/// exists, and stays so while it waits; resumed, the delete completes.
pub fn driver_delete_pauses_and_resumes_at_a_host_call(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    files_in(ctx, "d", &["a", "b", "c"])?;
    let oracle = Oracle::arm(Fixture::new().eligible(s.path("d")))?;

    let mut run = ctx.spawn_as_tester(
        &ctx.driver(),
        &["--pause-at", "unlinkat:2", "delete", "/tmp/d"],
    )?;
    let paused = run.wait_paused()?;
    ensure!(
        paused.op == "unlinkat" && paused.nth == 2,
        "paused at the wrong call: {paused:?}"
    );
    for _ in 0..2 {
        ensure!(!exists(&s.path("d/a")), "the first unlinkat did not happen");
        ensure!(
            exists(&s.path("d/b")) && exists(&s.path("d/c")),
            "an entry went while the driver was paused before call {}",
            paused.call
        );
        // Still nothing after a while: paused means paused.
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    run.resume(&paused)?;
    let run = run.wait()?;
    let verdict = oracle.judge(&run)?;

    run.expect_code(0)?;
    let got: Vec<String> = results(&run)?
        .iter()
        .map(|l| l.split(" ino=").next().unwrap_or("").to_owned())
        .collect();
    let want = [
        "0 unlinked \"d/a\"",
        "1 unlinked \"d/b\"",
        "2 unlinked \"d/c\"",
        "3 unlinked \"d\"",
    ];
    ensure!(got == want, "delete reported {got:?}, expected {want:?}");
    ensure!(!exists(&s.path("d")), "d still exists after the delete");
    verdict.assert_clean()
}

/// Killed at port call k, the driver dies before performing it: only the
/// entries before it are gone, and the oracle, whose eligible set is
/// exactly those entries, judges the partial run clean. k is an absolute
/// position, learned from a paused run on an identical tree (its third
/// `unlinkat`), so the test also shows that call positions are stable from
/// run to run.
pub fn driver_delete_killed_at_a_host_call(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    let names = ["a", "b", "c", "d"];
    files_in(ctx, "calib", &names)?;
    files_in(ctx, "victim", &names)?;
    let oracle = Oracle::arm(
        Fixture::new()
            .eligible(s.path("calib"))
            .eligible(s.path("victim/a"))
            .eligible(s.path("victim/b")),
    )?;

    let mut calib = ctx.spawn_as_tester(
        &ctx.driver(),
        &["--pause-at", "unlinkat:3", "delete", "/tmp/calib"],
    )?;
    let paused = calib.wait_paused()?;
    calib.resume(&paused)?;
    let calib = calib.wait()?;
    calib.expect_code(0)?;
    let k = paused.call;
    info!("the third unlinkat of a delete of a 4-file tree is port call {k}");

    let victim = ctx
        .spawn_as_tester(
            &ctx.driver(),
            &["--kill-at", &k.to_string(), "delete", "/tmp/victim"],
        )?
        .wait()?;
    let verdict = oracle.judge_runs(&[&calib, &victim])?;

    ensure!(
        victim.code.is_none() && victim.signal == Some(SIGKILL),
        "the driver was not killed: exit {:?}, signal {:?}\n{}{}",
        victim.code,
        victim.signal,
        victim.stdout,
        victim.stderr
    );
    let said = controls(&victim)?;
    ensure!(
        matches!(
            said.as_slice(),
            [ControlLine::Kill { call, op, nth: 3, .. }] if *call == k && op == "unlinkat"
        ),
        "expected one kill line at call {k} (the third unlinkat), got {said:?}"
    );
    let got: Vec<String> = results(&victim)?
        .iter()
        .map(|l| l.split(" ino=").next().unwrap_or("").to_owned())
        .collect();
    ensure!(
        got == ["0 unlinked \"victim/a\"", "1 unlinked \"victim/b\""],
        "the killed driver reported {got:?}"
    );
    for name in ["c", "d"] {
        ensure!(
            exists(&s.path(&format!("victim/{name}"))),
            "victim/{name} is gone: the kill came after its call"
        );
    }
    ensure!(
        !exists(&s.path("calib")),
        "the calibration delete did not finish"
    );
    verdict.assert_clean()
}

/// The replace-between-identity-check-and-unlink window (docs/safety.md,
/// Deletion: Linux has no unlink-by-inode), hit at an exact call: paused
/// before the second `unlinkat` (its identity `statx` was the call before),
/// the harness renames a new file over the entry; resumed, the driver
/// unlinks the replacement, not the object it checked. Task-11 builds the
/// real race test on this mechanism.
pub fn driver_delete_unlinks_what_replaced_an_entry_while_paused(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    files_in(ctx, "r", &["a", "b", "c"])?;
    let checked = ino(&s.path("r/b"))?;
    let oracle = Oracle::arm(Fixture::new().eligible(s.path("r")))?;

    let mut run = ctx.spawn_as_tester(
        &ctx.driver(),
        &["--pause-at", "unlinkat:2", "delete", "/tmp/r"],
    )?;
    let paused = run.wait_paused()?;
    s.write_file("r/b.new", b"replacement", 0o644, TESTER)?;
    let replacement = ino(&s.path("r/b.new"))?;
    s.rename("r/b.new", "r/b")?;
    ensure!(
        ino(&s.path("r/b"))? == replacement && replacement != checked,
        "the rename did not replace r/b"
    );
    run.resume(&paused)?;
    let run = run.wait()?;
    let verdict = oracle.judge(&run)?;

    run.expect_code(0)?;
    let got = results(&run)?;
    let want_b = format!("1 unlinked \"r/b\" ino={checked}");
    ensure!(
        got.get(1) == Some(&want_b),
        "expected {want_b:?} (the identity statx saw the original), got {got:?}"
    );
    ensure!(
        !exists(&s.path("r")),
        "r still exists: the replacement was not the entry unlinked"
    );
    info!(
        "identity check saw inode {checked}, the unlinkat right after it removed inode \
         {replacement}: the accepted window, reproduced at port call {}",
        paused.call
    );
    verdict.assert_clean()
}
