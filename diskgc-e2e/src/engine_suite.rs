//! Container tests of the deletion engine (task-5; docs/safety.md,
//! Filesystem candidates), run through the test driver's `delete` (the
//! engine's eligibility walk and identity-checked delete over `LinuxHost`)
//! as `tester` under strace. The deletion oracle judges every run: nothing
//! outside the candidate may disappear, whatever the engine reports.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Instant;

use crate::controlled::ControlLine;
use crate::oracle::{Fixture, Oracle, Unmatched};
use crate::suite::{Ctx, TestResult};
use crate::under_test::Run;
use crate::{OTHER, TESTER, ensure, info};

/// The driver's result lines (control lines left out).
fn results(run: &Run) -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    for line in run.stdout.lines() {
        if ControlLine::parse(line)?.is_none() {
            lines.push(line.to_owned());
        }
    }
    Ok(lines)
}

/// The single result line of a driver `delete`.
fn outcome(run: &Run) -> Result<String, String> {
    match results(run)?.as_slice() {
        [line] => Ok(line.clone()),
        other => Err(format!(
            "expected one result line, got {other:?}\n--- stderr\n{}",
            run.stderr
        )),
    }
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Runs the driver's `delete` of `/tmp/<rel>` (with the driver flags
/// `extra` first).
fn delete(ctx: &Ctx, extra: &[&str], rel: &str) -> Result<Run, String> {
    let target = ctx.scratch.path(rel);
    let target = target.to_str().ok_or("non-UTF-8 path")?;
    let mut args: Vec<&str> = extra.to_vec();
    args.extend(["delete", target]);
    ctx.run_as_tester(&ctx.driver(), &args)
}

/// A candidate with files, nested directories and a symlink pointing out of
/// it: tester's, at `rel`.
fn small_tree(ctx: &Ctx, rel: &str) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir(rel, 0o755, TESTER)?;
    s.write_file(&format!("{rel}/a"), b"a", 0o644, TESTER)?;
    s.mkdirs(&format!("{rel}/sub/deeper"), 0o755, TESTER)?;
    s.write_file(&format!("{rel}/sub/b"), b"b", 0o644, TESTER)?;
    s.write_file(&format!("{rel}/sub/deeper/c"), b"c", 0o600, TESTER)?;
    s.symlink(&format!("{rel}/sub/out"), &s.path("bystander/kept"))?;
    // Made by root: give it to tester, or it makes the tree not_owner.
    s.chown(&format!("{rel}/sub/out"), TESTER)?;
    Ok(())
}

/// A bystander next to the candidates, which must survive every test.
fn bystander(ctx: &Ctx) -> TestResult {
    ctx.scratch.mkdir("bystander", 0o755, TESTER)?;
    ctx.scratch
        .write_file("bystander/kept", b"keep", 0o644, TESTER)?;
    Ok(())
}

/// The engine deletes an eligible tree completely, the symlink in it as a
/// link (its target outside the candidate stays), and nothing else.
pub fn engine_deletes_an_eligible_tree_and_nothing_else(ctx: &Ctx) -> TestResult {
    bystander(ctx)?;
    small_tree(ctx, "c")?;
    let oracle = Oracle::arm(Fixture::new().eligible(ctx.scratch.path("c")))?;
    let run = delete(ctx, &[], "c")?;
    let verdict = oracle.judge(&run)?;
    run.expect_code(0)?;
    let line = outcome(&run)?;
    ensure!(
        line.starts_with("0 removed entries=7 "),
        "expected the 7 entries removed, got {line:?}"
    );
    ensure!(
        !exists(&ctx.scratch.path("c")),
        "the candidate is still there"
    );
    ensure!(
        exists(&ctx.scratch.path("bystander/kept")),
        "the symlink's target went"
    );
    verdict.assert_clean()
}

/// AC #1: one file of another user deep inside a tester-owned tree makes
/// the whole candidate `not_owner`; nothing at all is deleted.
pub fn engine_skips_a_mixed_owner_tree_as_not_owner(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    small_tree(ctx, "mixed")?;
    s.write_file("mixed/sub/deeper/theirs", b"x", 0o644, OTHER)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let run = delete(ctx, &[], "mixed")?;
    let verdict = oracle.judge(&run)?;
    run.expect_code(1)?;
    let line = outcome(&run)?;
    ensure!(
        line.starts_with("0 skip not_owner \"/tmp/mixed/sub/deeper/theirs\" -- "),
        "expected not_owner at the other user's file, got {line:?}"
    );
    ensure!(
        run.events.is_empty(),
        "a refused candidate saw unlink calls: {:?}",
        run.events
    );
    verdict.assert_clean()
}

/// AC #1: a `.diskgc-keep` deep inside makes the candidate `kept`, and a
/// Cargo target inside (`CACHEDIR.TAG` with a lock file in a profile
/// directory) makes it `contains_special`; nothing is deleted.
pub fn engine_skips_a_nested_keep_marker_or_cargo_target(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    small_tree(ctx, "kept")?;
    s.write_file("kept/sub/deeper/.diskgc-keep", b"", 0o644, TESTER)?;
    small_tree(ctx, "cargo")?;
    s.mkdirs("cargo/sub/target/debug", 0o755, TESTER)?;
    s.write_file(
        "cargo/sub/target/CACHEDIR.TAG",
        b"Signature: 8a477f597d28d172789f06886806bc55\n",
        0o644,
        TESTER,
    )?;
    s.write_file("cargo/sub/target/debug/.cargo-lock", b"", 0o644, TESTER)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let kept = delete(ctx, &[], "kept")?;
    let cargo = delete(ctx, &[], "cargo")?;
    let verdict = oracle.judge_runs(&[&kept, &cargo])?;
    for (run, want) in [
        (
            &kept,
            "0 skip kept \"/tmp/kept/sub/deeper/.diskgc-keep\" -- ",
        ),
        (
            &cargo,
            "0 skip contains_special \"/tmp/cargo/sub/target/debug/.cargo-lock\" -- ",
        ),
    ] {
        run.expect_code(1)?;
        let line = outcome(run)?;
        ensure!(line.starts_with(want), "expected {want:?}..., got {line:?}");
    }
    verdict.assert_clean()
}

/// task-71: a VCS checkout inside a plain-delete candidate makes it
/// `contains_checkout`, on a real filesystem: a `.git` directory, a
/// worktree's `.git` file deep inside, a `.git` symlink (never followed:
/// its target, the bystander, is untouched), `.hg` and `.svn`
/// directories. Nothing is deleted.
pub fn engine_skips_a_source_checkout(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    bystander(ctx)?;
    small_tree(ctx, "gitdir")?;
    s.mkdir("gitdir/sub/.git", 0o755, TESTER)?;
    s.write_file(
        "gitdir/sub/.git/HEAD",
        b"ref: refs/heads/main\n",
        0o644,
        TESTER,
    )?;
    small_tree(ctx, "worktree")?;
    s.write_file(
        "worktree/sub/deeper/.git",
        b"gitdir: /home/tester/repo/.git/worktrees/w\n",
        0o644,
        TESTER,
    )?;
    small_tree(ctx, "gitlink")?;
    s.symlink("gitlink/.git", &s.path("bystander"))?;
    s.chown("gitlink/.git", TESTER)?;
    small_tree(ctx, "hg")?;
    s.mkdirs("hg/sub/.hg/store", 0o755, TESTER)?;
    small_tree(ctx, "svn")?;
    s.mkdir("svn/.svn", 0o755, TESTER)?;
    s.write_file("svn/.svn/wc.db", b"", 0o644, TESTER)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let cases = [
        ("gitdir", "/tmp/gitdir/sub/.git"),
        ("worktree", "/tmp/worktree/sub/deeper/.git"),
        ("gitlink", "/tmp/gitlink/.git"),
        ("hg", "/tmp/hg/sub/.hg"),
        ("svn", "/tmp/svn/.svn"),
    ];
    let mut runs = Vec::new();
    for (rel, _) in cases {
        runs.push(delete(ctx, &[], rel)?);
    }
    let verdict = oracle.judge_runs(&runs.iter().collect::<Vec<_>>())?;
    for (run, (_, at)) in runs.iter().zip(cases) {
        run.expect_code(1)?;
        let line = outcome(run)?;
        let want = format!("0 skip contains_checkout \"{at}\" -- ");
        ensure!(
            line.starts_with(&want),
            "expected {want:?}..., got {line:?}"
        );
        ensure!(
            run.events.is_empty(),
            "a refused candidate saw unlink calls: {:?}",
            run.events
        );
    }
    ensure!(
        exists(&ctx.scratch.path("bystander/kept")),
        "the .git symlink's target went"
    );
    verdict.assert_clean()
}

/// AC #5: a tester-owned directory with mode 000 inside the candidate
/// cannot be listed by tester (no `CAP_DAC_OVERRIDE`): the candidate is
/// ineligible (`unreadable`, fail closed), never treated as empty.
pub fn engine_fails_closed_on_a_mode_000_directory(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    small_tree(ctx, "locked")?;
    s.mkdir("locked/sub/sealed", 0o755, TESTER)?;
    s.write_file("locked/sub/sealed/inside", b"x", 0o644, TESTER)?;
    s.chmod("locked/sub/sealed", 0o000)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let run = delete(ctx, &[], "locked")?;
    let verdict = oracle.judge(&run)?;
    run.expect_code(1)?;
    let line = outcome(&run)?;
    ensure!(
        line.starts_with("0 skip unreadable \"/tmp/locked/sub/sealed\" -- ")
            && line.contains("EACCES"),
        "expected unreadable (EACCES) at the mode-000 directory, got {line:?}"
    );
    verdict.assert_clean()
}

/// A socket or FIFO inside keeps the candidate (`contains_ipc`, the default
/// policy); nothing is deleted.
pub fn engine_skips_sockets_and_fifos(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    small_tree(ctx, "fifo")?;
    s.fifo("fifo/sub/deeper/pipe", 0o600, TESTER)?;
    small_tree(ctx, "sock")?;
    s.socket("sock/sub/server.sock", TESTER)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let fifo = delete(ctx, &[], "fifo")?;
    let sock = delete(ctx, &[], "sock")?;
    let verdict = oracle.judge_runs(&[&fifo, &sock])?;
    for (run, want) in [
        (
            &fifo,
            "0 skip contains_ipc \"/tmp/fifo/sub/deeper/pipe\" -- ",
        ),
        (
            &sock,
            "0 skip contains_ipc \"/tmp/sock/sub/server.sock\" -- ",
        ),
    ] {
        run.expect_code(1)?;
        let line = outcome(run)?;
        ensure!(line.starts_with(want), "expected {want:?}..., got {line:?}");
    }
    verdict.assert_clean()
}

/// AC #4: paused at the boundary between the eligibility walk and the
/// delete (the delete's first port call: reopening the candidate, the
/// fourth `openat2` after resolve's two and the walk's one), the harness
/// replaces `b`; resumed, the engine deletes `a`, finds `b` is not the
/// object it checked, and stops: `b`'s replacement and everything after it
/// stay, and the candidate is `changed`.
pub fn engine_never_deletes_an_entry_replaced_after_the_walk(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("r", 0o755, TESTER)?;
    for name in ["a", "b", "c"] {
        s.write_file(&format!("r/{name}"), name.as_bytes(), 0o644, TESTER)?;
    }
    let oracle = Oracle::arm(Fixture::new().eligible(s.path("r")))?;
    let mut run = ctx.spawn_as_tester(
        &ctx.driver(),
        &["--pause-at", "openat2:4", "delete", "/tmp/r"],
    )?;
    let paused = run.wait_paused()?;
    s.write_file("r/b.new", b"replacement", 0o644, TESTER)?;
    s.rename("r/b.new", "r/b")?;
    run.resume(&paused)?;
    let run = run.wait()?;
    let verdict = oracle.judge(&run)?;

    run.expect_code(1)?;
    let line = outcome(&run)?;
    ensure!(
        line.starts_with("0 changed \"/tmp/r/b\" entries=1 -- "),
        "expected changed at b after removing a, got {line:?}"
    );
    ensure!(
        std::fs::read(s.path("r/b")).ok().as_deref() == Some(&b"replacement"[..]),
        "b's replacement was deleted or changed"
    );
    ensure!(
        !exists(&s.path("r/a")) && exists(&s.path("r/c")),
        "expected a gone and c kept"
    );
    info!(
        "paused at port call {} (the walk/delete boundary), b replaced: never unlinked",
        paused.call
    );
    verdict.assert_clean()
}

/// A name moved out after the walk is gone at its checked parent. Deletion
/// reports changed, leaves the moved object outside the candidate intact,
/// and does not touch later entries.
pub fn engine_keeps_an_entry_renamed_out_after_the_walk(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("r", 0o755, TESTER)?;
    for name in ["a", "b", "c"] {
        s.write_file(&format!("r/{name}"), name.as_bytes(), 0o644, TESTER)?;
    }
    let oracle = Oracle::arm(Fixture::new().eligible(s.path("r")))?;
    let mut run = ctx.spawn_as_tester(
        &ctx.driver(),
        &["--pause-at", "openat2:4", "delete", "/tmp/r"],
    )?;
    let paused = run.wait_paused()?;
    s.rename("r/b", "escaped")?;
    run.resume(&paused)?;
    let run = run.wait()?;
    let verdict = oracle.judge(&run)?;
    run.expect_code(1)?;
    ensure!(
        outcome(&run)?.starts_with("0 changed \"/tmp/r/b\" entries=1 -- "),
        "renamed entry was not classified changed: {}",
        run.stdout
    );
    ensure!(
        exists(&s.path("escaped")) && exists(&s.path("r/c")) && !exists(&s.path("r/a")),
        "renamed entry or later entry changed unexpectedly"
    );
    verdict.assert_clean()
}

/// A same-filesystem bind mount of a regular file can arrive after the
/// identity statx and before unlinkat. The kernel must refuse unlinking
/// the mount point with EBUSY; the engine reports changed and stops.
pub fn engine_stops_at_file_bind_mount_after_identity_check(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("r", 0o755, TESTER)?;
    for name in ["a", "b", "c"] {
        s.write_file(&format!("r/{name}"), name.as_bytes(), 0o644, TESTER)?;
    }
    s.write_file("source", b"keep", 0o644, TESTER)?;
    let oracle = Oracle::arm(Fixture::new().eligible(s.path("r")))?;
    let mut run = ctx.spawn_as_tester(
        &ctx.driver(),
        &["--pause-at", "unlinkat:2", "delete", "/tmp/r"],
    )?;
    let paused = run.wait_paused()?;
    s.bind_mount("source", "r/b")?;
    run.resume(&paused)?;
    let run = run.wait()?;
    s.unmount("r/b")?;
    let verdict = oracle.judge(&run)?;
    run.expect_code(1)?;
    let line = outcome(&run)?;
    ensure!(
        line.starts_with("0 changed \"/tmp/r/b\" entries=1 -- ") && line.contains("EBUSY"),
        "mounted file was not classified changed (EBUSY): {line:?}"
    );
    ensure!(
        exists(&s.path("source")) && exists(&s.path("r/b")) && exists(&s.path("r/c")),
        "mounted source, underlying target or later entry was deleted"
    );
    verdict.assert_clean()
}

/// The mode used by fchmod must be read from the held fd after a concurrent
/// chmod, so a stale walk mode cannot restore bits another process removed.
pub fn engine_make_writable_preserves_a_concurrent_mode_change(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    small_tree(ctx, "writable")?;
    s.chmod("writable/sub/deeper", 0o555)?;
    let oracle = Oracle::arm(Fixture::new().eligible(s.path("writable")))?;
    let mut run = ctx.spawn_as_tester(
        &ctx.driver(),
        &[
            "--pause-at",
            "fchmod:1",
            "--pause-at",
            "unlinkat:3",
            "delete",
            "--make-writable",
            "/tmp/writable",
        ],
    )?;
    let paused = run.wait_paused()?;
    s.chmod("writable/sub/deeper", 0o550)?;
    run.resume(&paused)?;
    let paused = run.wait_paused()?;
    let mode = std::fs::metadata(s.path("writable/sub/deeper"))
        .map_err(|e| format!("stat after fchmod: {e}"))?
        .permissions()
        .mode()
        & 0o7777;
    ensure!(
        mode == 0o750,
        "fchmod restored stale permission bits: {mode:#o}"
    );
    run.resume(&paused)?;
    let run = run.wait()?;
    let verdict = oracle.judge(&run)?;
    run.expect_code(0)?;
    ensure!(
        outcome(&run)?.starts_with("0 removed entries=7 "),
        "writable tree was not deleted: {}",
        run.stdout
    );
    verdict.assert_clean()
}

/// AC #7: a mount appearing inside a candidate mid-delete. The driver is
/// paused before its first unlink (of `a`); the harness, as container root
/// (mounting needs `CAP_SYS_ADMIN` in the container's user namespace, which
/// tester lacks), mounts a tmpfs on `m/sub`; resumed, the engine's reopening
/// of `sub` meets the mount (`EXDEV` from `RESOLVE_NO_XDEV`) and the delete
/// stops there, `changed`. The harness unmounts before the oracle's
/// snapshot, so what lies under the mount point is judged too: intact.
pub fn engine_stops_at_a_mount_that_appears_mid_delete(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    s.mkdir("m", 0o755, TESTER)?;
    s.write_file("m/a", b"a", 0o644, TESTER)?;
    s.mkdir("m/sub", 0o755, TESTER)?;
    s.write_file("m/sub/under", b"u", 0o644, TESTER)?;
    s.write_file("m/z", b"z", 0o644, TESTER)?;
    let oracle = Oracle::arm(Fixture::new().eligible(s.path("m")))?;
    let mut run = ctx.spawn_as_tester(
        &ctx.driver(),
        &["--pause-at", "unlinkat:1", "delete", "/tmp/m"],
    )?;
    let paused = run.wait_paused()?;
    s.mount_tmpfs("m/sub")?;
    run.resume(&paused)?;
    let run = run.wait()?;
    s.unmount("m/sub")?;
    let verdict = oracle.judge(&run)?;

    run.expect_code(1)?;
    let line = outcome(&run)?;
    ensure!(
        line.starts_with("0 changed \"/tmp/m/sub\" entries=1 -- ") && line.contains("EXDEV"),
        "expected changed at sub (EXDEV) after removing a, got {line:?}"
    );
    ensure!(
        !exists(&s.path("m/a")) && exists(&s.path("m/sub/under")) && exists(&s.path("m/z")),
        "expected a gone, sub's content and z kept"
    );
    verdict.assert_clean()
}

/// `make_writable`: a read-only directory (0555) inside the candidate
/// cannot be emptied by its owner (`EACCES`, an error for the candidate;
/// the delete stops there); with `--make-writable` the engine adds owner
/// write permission after its checks and deletes it all.
pub fn engine_make_writable_deletes_a_read_only_tree(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    for rel in ["plain", "writable"] {
        small_tree(ctx, rel)?;
        s.chmod(&format!("{rel}/sub/deeper"), 0o555)?;
    }
    let oracle = Oracle::arm(
        Fixture::new()
            .eligible(s.path("plain"))
            .eligible(s.path("writable")),
    )?;
    let plain = delete(ctx, &[], "plain")?;
    let writable = ctx.run_as_tester(
        &ctx.driver(),
        &["delete", "--make-writable", "/tmp/writable"],
    )?;
    let verdict = oracle.judge_runs(&[&plain, &writable])?;
    plain.expect_code(1)?;
    let line = outcome(&plain)?;
    ensure!(
        line.starts_with("0 err EACCES \"/tmp/plain/sub/deeper/c\" "),
        "without make_writable, expected EACCES at deeper/c, got {line:?}"
    );
    ensure!(
        exists(&s.path("plain/sub/deeper/c")),
        "the read-only directory was emptied without make_writable"
    );
    writable.expect_code(0)?;
    let line = outcome(&writable)?;
    ensure!(line.starts_with("0 removed entries=7 "), "got {line:?}");
    ensure!(!exists(&s.path("writable")), "writable is still there");
    verdict.assert_clean()
}

/// Fixture size for the large trees (AC #2).
const LEVELS: usize = 30;
const FILES: usize = 100_000;

/// AC #2: a 100,000-entry tree 30 levels deep is deleted under
/// `RLIMIT_NOFILE` 32. The limit bites: the driver's plain `walk`, which
/// holds one fd per level, fails on the same tree under the same limit
/// (`EMFILE`); the engine holds a bounded window of fds.
pub fn engine_deletes_a_100k_entry_30_deep_tree_under_rlimit_nofile_32(ctx: &Ctx) -> TestResult {
    let s = &ctx.scratch;
    let built = Instant::now();
    let mut dir = String::from("big");
    s.mkdir(&dir, 0o755, TESTER)?;
    let per_level = FILES / LEVELS;
    let mut files = 0;
    for level in 0..LEVELS {
        if level > 0 {
            dir.push_str(&format!("/l{level:02}"));
            s.mkdir(&dir, 0o755, TESTER)?;
        }
        let here = if level + 1 == LEVELS {
            FILES - files
        } else {
            per_level
        };
        for f in 0..here {
            s.write_file(&format!("{dir}/f{f:05}"), b"", 0o644, TESTER)?;
        }
        files += here;
    }
    info!(
        "built {files} files in {LEVELS} levels in {:?}",
        built.elapsed()
    );
    let oracle = Oracle::arm(Fixture::new().eligible(s.path("big")))?;
    let control = ctx.run_as_tester(
        &ctx.driver(),
        &["--max-open-files", "32", "walk", "/tmp/big"],
    )?;
    let started = Instant::now();
    let run = delete(ctx, &["--max-open-files", "32"], "big")?;
    let took = started.elapsed();
    let verdict = oracle.judge_runs(&[&control, &run])?;

    ensure!(
        control.code == Some(1) && control.stdout.contains("EMFILE"),
        "the control walk (one fd per level) should hit the limit: exit {:?}\n{}",
        control.code,
        control
            .stdout
            .lines()
            .filter(|l| l.contains(" err "))
            .take(3)
            .collect::<Vec<_>>()
            .join("\n")
    );
    run.expect_code(0)?;
    let line = outcome(&run)?;
    let want = format!("0 removed entries={} ", FILES + LEVELS);
    ensure!(
        line.starts_with(&want),
        "expected {want:?}..., got {line:?}"
    );
    ensure!(!exists(&s.path("big")), "the tree is still there");
    info!(
        "engine walk and delete of {} entries: {took:?}",
        FILES + LEVELS
    );
    verdict.assert_clean()
}

/// AC #6: a single directory with 200,000 entries is deleted completely.
pub fn engine_deletes_a_200k_entry_directory(ctx: &Ctx) -> TestResult {
    const ENTRIES: usize = 200_000;
    let s = &ctx.scratch;
    s.mkdir("wide", 0o755, TESTER)?;
    let built = Instant::now();
    for f in 0..ENTRIES {
        s.write_file(&format!("wide/f{f:06}"), b"", 0o644, TESTER)?;
    }
    info!("built {ENTRIES} files in {:?}", built.elapsed());
    let oracle = Oracle::arm(Fixture::new().eligible(s.path("wide")))?;
    let started = Instant::now();
    let run = delete(ctx, &[], "wide")?;
    let took = started.elapsed();
    let verdict = oracle.judge(&run)?;
    run.expect_code(0)?;
    let line = outcome(&run)?;
    let want = format!("0 removed entries={} ", ENTRIES + 1);
    ensure!(
        line.starts_with(&want),
        "expected {want:?}..., got {line:?}"
    );
    ensure!(!exists(&s.path("wide")), "the directory is still there");
    info!(
        "engine walk and delete of {} entries: {took:?}",
        ENTRIES + 1
    );
    verdict.assert_clean()
}

/// AC #6 (and the fd bound of AC #2 at depth): a chain of 1,500 nested
/// directories (`dd/dd/...`, each with a file), whose deepest paths are
/// about 4,500 bytes, longer than `PATH_MAX`, is deleted completely under
/// `RLIMIT_NOFILE` 64. No full path ever reaches the kernel. strace cannot
/// name the deepest directory fds; those calls are logged as unmatched
/// (`UnnamedDirFd`), and the snapshot view judges the objects.
pub fn engine_deletes_a_tree_deeper_than_path_max(ctx: &Ctx) -> TestResult {
    const DEPTH: usize = 1500;
    let s = &ctx.scratch;
    s.mkdir("deep", 0o755, TESTER)?;
    s.deep_chain("deep", DEPTH, "dd", TESTER)?;
    let oracle = Oracle::arm(Fixture::new().eligible(s.path("deep")))?;
    let started = Instant::now();
    let run = delete(ctx, &["--max-open-files", "64"], "deep")?;
    let took = started.elapsed();
    let verdict = oracle.judge(&run)?;
    run.expect_code(0)?;
    let line = outcome(&run)?;
    // deep, then per level a file and a directory.
    let want = format!("0 removed entries={} ", 1 + 2 * DEPTH);
    ensure!(
        line.starts_with(&want),
        "expected {want:?}..., got {line:?}"
    );
    ensure!(!exists(&s.path("deep")), "the chain is still there");
    let unnamed = verdict
        .unmatched
        .iter()
        .filter(|u| u.why == Unmatched::UnnamedDirFd)
        .count();
    info!(
        "{} levels deleted in {took:?}; {unnamed} traced unlinks below directories strace could \
         not name",
        DEPTH
    );
    ensure!(
        unnamed > 0,
        "expected strace to lose the deepest directory paths (the tree is not deeper than a page?)"
    );
    verdict.assert_clean()
}
