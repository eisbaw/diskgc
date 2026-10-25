//! Helpers shared by the Cargo container suites (the reclaim protocol's
//! tests, the race tests, the `ensure` tests): the driver's `cargo-reclaim`
//! and its result lines, the harness's own reading of a target (profile
//! directories, lock files, the parts of the protocol's step 3 and their
//! unlink positions, a tree's state), `/proc/locks` usage of a profile's
//! lock files, and Cargo builds as fixtures. None of it asks diskgc.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cargo_under_test::{self, Cargo, CargoChild, RACE, REACH_TIMEOUT, Toolchain};
use crate::controlled::ControlLine;
use crate::oracle::{Fixture, Oracle};
use crate::proc_locks::{self, Usage, Watched};
use crate::suite::{Ctx, TestResult};
use crate::under_test::{Paused, Run, Spawned};
use crate::{ensure, info};
use diskgc_core::inuse::FRESHNESS;

/// SIGKILL's number on Linux.
pub(crate) const SIGKILL: i32 = 9;

/// The host's target triple (the container is x86_64; each toolchain
/// carries the host's rust-std).
pub(crate) const HOST_TRIPLE: &str = "x86_64-unknown-linux-gnu";

/// The driver's result lines (control lines left out), without their index.
pub(crate) fn results(run: &Run) -> Result<Vec<String>, String> {
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
pub(crate) fn verdict(lines: &[String], target: &Path) -> Result<String, String> {
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
pub(crate) fn cargo_reclaim(
    ctx: &Ctx,
    driver_flags: &[&str],
    args: &[&str],
) -> Result<Run, String> {
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
pub(crate) fn profile_dirs(target: &Path) -> Result<Vec<PathBuf>, String> {
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
pub(crate) fn deletable(dir: &Path) -> Result<Vec<PathBuf>, String> {
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
pub(crate) fn objects_below(path: &Path) -> Result<Vec<PathBuf>, String> {
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

/// The lock files of the target's profile directories, by inode.
pub(crate) fn lock_inodes(target: &Path) -> Result<BTreeMap<PathBuf, u64>, String> {
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
pub(crate) fn reclaimable(target: &Path) -> Result<Fixture, String> {
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
pub(crate) fn layout_of(profile: &Path) -> Result<&'static str, String> {
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
pub(crate) fn unlinked(run: &Run) -> Vec<PathBuf> {
    run.events
        .iter()
        .filter(|e| e.syscall == "unlinkat" && e.took_effect())
        .flat_map(|e| e.removed.iter().cloned())
        .collect()
}

pub(crate) fn build(ctx: &Ctx, toolchain: &Toolchain, project: &Path, args: &[&str]) -> TestResult {
    Cargo::new(toolchain, project, args)
        .output(ctx)?
        .expect_success()?;
    Ok(())
}

/// After a reclaim (complete or not): `cargo build PROFILE...` succeeds
/// and the binary `cargo run PROFILE...` builds runs; with `recompiles`
/// (every fingerprint of that profile was gone), it compiled the package
/// again. `profile`: no arguments for debug, `--release` for release.
pub(crate) fn rebuilds(
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
        !recompiles || built.stderr.contains(&format!("Compiling {}", RACE.binary)),
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

pub(crate) fn path_str(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("non-UTF-8 path {}", path.display()))
}

/// How many further `/proc/locks` samples a paused or blocked state must
/// hold in, after it was first seen: a count, not a time.
pub(crate) const STEADY_SAMPLES: usize = 25;

/// Pause between two of those samples.
pub(crate) const SAMPLE_GAP: Duration = Duration::from_millis(20);

/// What the harness sees of one object: enough to tell a write (a new
/// inode, a new size, a new mtime or ctime: a new link or mode moves only
/// the ctime) without diskgc's help.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Seen {
    ino: u64,
    is_dir: bool,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

/// Every object below `dir` (and `dir` itself) by path, symlinks not
/// followed: the harness's own walk.
pub(crate) fn tree_state(dir: &Path) -> Result<BTreeMap<PathBuf, Seen>, String> {
    let mut seen = BTreeMap::new();
    for path in objects_below(dir)? {
        let meta = std::fs::symlink_metadata(&path).map_err(|e| format!("lstat {path:?}: {e}"))?;
        seen.insert(
            path,
            Seen {
                ino: meta.ino(),
                is_dir: meta.is_dir(),
                size: meta.size(),
                mtime: (meta.mtime(), meta.mtime_nsec()),
                ctime: (meta.ctime(), meta.ctime_nsec()),
            },
        );
    }
    Ok(seen)
}

/// The differences between two [`tree_state`]s, one line each.
pub(crate) fn tree_changes(
    before: &BTreeMap<PathBuf, Seen>,
    after: &BTreeMap<PathBuf, Seen>,
) -> Vec<(PathBuf, String)> {
    let mut changes = Vec::new();
    for (path, was) in before {
        match after.get(path) {
            None => changes.push((path.clone(), "removed".to_owned())),
            Some(now) if now != was => {
                changes.push((path.clone(), format!("changed: {was:?} -> {now:?}")));
            }
            Some(_) => {}
        }
    }
    for (path, now) in after {
        if !before.contains_key(path) {
            changes.push((path.clone(), format!("created: {now:?}")));
        }
    }
    changes
}

/// What `pid` does with the lock files of `profile` in one `/proc/locks`
/// sample, the lock files listed again first (a Cargo may just have
/// created one).
pub(crate) fn lock_usage(profile: &Path, pid: u32) -> Result<Usage, String> {
    let watched = Watched::files(&cargo_under_test::lock_files(profile)?)?;
    Ok(watched.usage(&proc_locks::sample()?, pid))
}

/// Samples until `pid` waits for a lock on one of `profile`'s lock files
/// (at most [`REACH_TIMEOUT`]), then [`STEADY_SAMPLES`] more: in every one
/// it still waits for the same file and holds the same files, and Cargo
/// still runs. Returns that usage.
pub(crate) fn steadily_blocked(
    profile: &Path,
    cargo: &mut CargoChild,
    what: &str,
) -> Result<Usage, String> {
    let pid = cargo.pid();
    let deadline = std::time::Instant::now() + REACH_TIMEOUT;
    loop {
        let usage = lock_usage(profile, pid)?;
        if !usage.waits.is_empty() {
            break;
        }
        ensure!(
            std::time::Instant::now() < deadline && cargo.is_running()?,
            "{what} (pid {pid}) never waited for a lock file of {} (it ended, or \
             {REACH_TIMEOUT:?} passed): it {usage}",
            profile.display()
        );
        std::thread::sleep(SAMPLE_GAP);
    }
    // Sampled again with the lock files listed after it began to wait, so
    // that one it created just before is watched too.
    let first = lock_usage(profile, pid)?;
    let held = |u: &Usage| u.holds.iter().cloned().collect::<BTreeSet<_>>();
    for n in 0..STEADY_SAMPLES {
        std::thread::sleep(SAMPLE_GAP);
        let now = lock_usage(profile, pid)?;
        ensure!(
            cargo.is_running()? && now.waits == first.waits && held(&now) == held(&first),
            "{what} (pid {pid}) did not stay blocked (sample {n} of {STEADY_SAMPLES}): it {now}, \
             first it {first}"
        );
    }
    info!("{what} (pid {pid}) stays blocked over {STEADY_SAMPLES} samples: it {first}");
    Ok(first)
}

pub(crate) fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// One part of the protocol's step 3 (docs/safety.md, Reclaim protocol)
/// as the harness computes it from its own walk: the object whose
/// subtree is deleted as one, and how many unlinks it takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedPart {
    pub(crate) path: PathBuf,
    pub(crate) objects: usize,
    pub(crate) fingerprint: bool,
}

/// Rank of an entry among the last parts of its profile directory: the
/// layout's directories go last, `build/` then `deps/`.
fn marker_rank(name: &str) -> u8 {
    match name {
        "build" => 1,
        "deps" => 2,
        _ => 0,
    }
}

/// The parts of a reclaim of `profiles` (in the protocol's profile order,
/// [`profile_dirs`]'s), in the order step 3 deletes them: the
/// fingerprints of every profile directory first (`.fingerprint/`, then
/// each `build/<package>/<hash>/fingerprint`, names in order), then per
/// profile directory every other entry in name order with `build/` and
/// `deps/` last (`build/` without the fingerprints split off it). Each
/// object below a part is one `unlinkat`, so a part's positions follow
/// from the counts.
pub(crate) fn planned_parts(profiles: &[PathBuf]) -> Result<Vec<PlannedPart>, String> {
    let mut fingerprint_parts = Vec::new();
    let mut rest = Vec::new();
    for profile in profiles {
        let mut entries = deletable(profile)?;
        if let Some(at) = entries.iter().position(|e| name_of(e) == ".fingerprint") {
            let entry = entries.remove(at);
            fingerprint_parts.push(PlannedPart {
                objects: objects_below(&entry)?.len(),
                path: entry,
                fingerprint: true,
            });
        }
        let build_dir = profile.join("build");
        let mut split_off = 0;
        if build_dir.is_dir() && !build_dir.is_symlink() {
            for package in sorted_children(&build_dir)? {
                for unit in sorted_children(&package)? {
                    let fingerprint = unit.join("fingerprint");
                    if std::fs::symlink_metadata(&fingerprint).is_ok() {
                        let objects = objects_below(&fingerprint)?.len();
                        split_off += objects;
                        fingerprint_parts.push(PlannedPart {
                            path: fingerprint,
                            objects,
                            fingerprint: true,
                        });
                    }
                }
            }
        }
        entries.sort_by_key(|e| marker_rank(&name_of(e)));
        for entry in entries {
            let mut objects = objects_below(&entry)?.len();
            if entry == build_dir {
                objects -= split_off;
            }
            rest.push(PlannedPart {
                path: entry,
                objects,
                fingerprint: false,
            });
        }
    }
    fingerprint_parts.extend(rest);
    Ok(fingerprint_parts)
}

/// The directories directly in `dir`, in name order (as the walk records
/// them); empty for anything that is no directory.
fn sorted_children(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let meta = std::fs::symlink_metadata(dir).map_err(|e| format!("lstat {dir:?}: {e}"))?;
    if !meta.is_dir() {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| format!("read_dir {dir:?}: {e}"))? {
        found.push(entry.map_err(|e| format!("read_dir {dir:?}: {e}"))?.path());
    }
    found.sort();
    Ok(found)
}

/// The `unlinkat` position (from 1) of the first unlink of part `at` of
/// `parts`.
pub(crate) fn first_unlink_of(parts: &[PlannedPart], at: usize) -> usize {
    parts[..at].iter().map(|p| p.objects).sum::<usize>() + 1
}

/// Spawns `cargo-reclaim ARGS` paused at `point` and waits until it is
/// blocked there.
pub(crate) fn spawn_paused(
    ctx: &Ctx,
    point: &str,
    args: &[&str],
) -> Result<(Spawned, Paused), String> {
    let mut full = vec!["--pause-at", point, "cargo-reclaim"];
    full.extend_from_slice(args);
    let mut spawned = ctx.spawn_as_tester(&ctx.driver(), &full)?;
    let paused = spawned.wait_paused()?;
    Ok((spawned, paused))
}

/// Resumes the driver paused inside a target's deletion, but not before
/// [`FRESHNESS`] has passed since the pause was seen. The scan that cleared
/// the part in progress started before the pause, so the next part's check
/// must then make a new scan, which sees what started during the pause. A
/// shorter pause leaves the next part to that scan (one scan serves a
/// target's parts while it is younger than FRESHNESS, TASK-173), which
/// cannot see it.
pub(crate) fn resume_past_freshness(spawned: &mut Spawned, paused: &Paused) -> TestResult {
    if let Some(rest) = FRESHNESS.checked_sub(paused.seen.elapsed()) {
        std::thread::sleep(rest);
    }
    spawned.resume(paused)
}

/// The verdict line of `target` in the driver's run, which must exit 0.
pub(crate) fn verdict_of(run: &Run, target: &Path) -> Result<String, String> {
    run.expect_code(0)?;
    verdict(&results(run)?, target)
}

/// Fails unless `got` is a skip for `reason` of `target` with nothing
/// deleted, whose detail holds every `(pid, text)` piece of evidence.
pub(crate) fn expect_skip(
    got: &str,
    reason: &str,
    target: &Path,
    needles: &[(u32, &str)],
) -> TestResult {
    ensure!(
        got.starts_with(&format!("skip {reason} {target:?} entries=0 ")),
        "expected {} skipped as {reason} with nothing deleted: {got}",
        target.display()
    );
    for (pid, needle) in needles {
        ensure!(
            got.split("; ")
                .any(|e| e.contains(&format!("pid {pid} (")) && e.contains(needle)),
            "no evidence that pid {pid} {needle:?} in: {got}"
        );
    }
    Ok(())
}

/// Every lock file of every profile directory of `target`, watched.
pub(crate) fn watch_target_locks(target: &Path) -> Result<Watched, String> {
    let mut files = Vec::new();
    for profile in profile_dirs(target)? {
        files.extend(cargo_under_test::lock_files(&profile)?);
    }
    Watched::files(&files)
}

/// The objects (paths) each part of `parts` deletes, by the harness's
/// walk: everything below the part, without what a part nested in it
/// (a per-unit fingerprint split off `build/`) deletes itself.
pub(crate) fn part_objects(parts: &[PlannedPart]) -> Result<Vec<BTreeSet<PathBuf>>, String> {
    let mut sets = Vec::new();
    for part in parts {
        let mut objects: BTreeSet<PathBuf> = objects_below(&part.path)?.into_iter().collect();
        for nested in parts
            .iter()
            .filter(|n| n.path != part.path && n.path.starts_with(&part.path))
        {
            for object in objects_below(&nested.path)? {
                objects.remove(&object);
            }
        }
        ensure!(
            objects.len() == part.objects,
            "{}: {} objects, planned {}",
            part.path.display(),
            objects.len(),
            part.objects
        );
        sets.push(objects);
    }
    Ok(sets)
}

/// The index of the part of `parts` that deletes `path`: the one whose
/// path is the longest prefix of it (a per-unit fingerprint inside
/// `build/` is its own part).
pub(crate) fn part_of(parts: &[PlannedPart], path: &Path) -> Option<usize> {
    parts
        .iter()
        .enumerate()
        .filter(|(_, p)| path.starts_with(&p.path))
        .max_by_key(|(_, p)| p.path.components().count())
        .map(|(i, _)| i)
}

/// Appends a comment to the project's `src/main.rs`, so the next build
/// compiles it again (a newer build than any before).
pub(crate) fn edit_main(project: &Path) -> TestResult {
    let main = project.join("src/main.rs");
    let text = std::fs::read_to_string(&main).map_err(|e| format!("read {main:?}: {e}"))?;
    std::fs::write(&main, format!("{text}\n// edit\n")).map_err(|e| format!("write {main:?}: {e}"))
}

/// Runs `cargo-reclaim ARGS` judged by an oracle with nothing eligible:
/// nothing may be deleted or replaced anywhere, and the strace record must
/// hold no unlink at all. Returns the run (exit 0) for its verdicts.
pub(crate) fn reclaim_touching_nothing(ctx: &Ctx, args: &[&str]) -> Result<Run, String> {
    let oracle = Oracle::arm(Fixture::new())?;
    let run = cargo_reclaim(ctx, &[], args)?;
    let judged = oracle.judge(&run)?;
    run.expect_code(0)?;
    ensure!(
        unlinked(&run).is_empty(),
        "the driver unlinked {:?}",
        unlinked(&run)
    );
    judged.assert_clean()?;
    Ok(run)
}
