//! Owner PID in the kernel's view (TASK-112 step 4; docs/safety.md,
//! Producer liveness, Owner PID): the `nix-shell-rc` built-in judged by the
//! real binary against real PID namespaces and a real `hidepid` `/proc`.
//!
//! These containers share the host's PID namespace
//! ([`crate::suite::HOST_PID_NAMESPACE_TESTS`]; what that exposes, and why
//! it is confined, is in docs/testing.md, Container setup). The harness
//! signals only processes it started, through pidfds, never by number.
//!
//! Nested namespaces are made with busybox `unshare` and entered with
//! `nsenter`, both untraced fixtures; the code under test runs traced, as
//! every run is, and the deletion oracle judges each one. Namespace-local
//! pids are moved into a range no host process carries at any `NSpid`
//! level (`ns_last_pid`): small namespace-local pids collide with other
//! namespaces' (pid 3 of another container), which only retains, and would
//! hide what the test is about.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::differential_suite::SandboxClockOffset;
use crate::ensure_suite::{ensure_run, report};
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, TestResult};
use crate::under_test::{Run, TESTER_ENV};
use crate::{OTHER, ROOT, TESTER, ensure, info};

const NIX_DEVELOP_SHELL_SCRIPT: &str = include_str!("lifecycle/nix_develop_shell.sh");
/// The pinned interactive shell (as in crate::lifecycle_suite).
const NIX_SHELL_BASH: &str = "/opt/nix-eval/bash";
/// The built-in's `min_idle_for` is 1h and every candidate here is made
/// now: the sandboxed binary's clock is advanced past it (kernel ctimes
/// cannot be set back).
const CLOCK_OFFSET_SECONDS: u64 = 2 * 3600;
/// Width of the namespace-local pid range a nested namespace allocates
/// from: room for Nix's helper processes and the shell.
const PID_RANGE_WIDTH: u32 = 1000;
/// Where the search for a free range starts, as a fraction of the host's
/// `pid_max`: away from the low pids namespaces and early boot use. The
/// range is checked against every host process, not assumed free; the
/// runner serialises these tests, so no other one picks it meanwhile.
const PID_RANGE_SEARCH_FROM_FRACTION: u32 = 2;
/// Bound for a fixture to reach its state (a nix develop start included).
const FIXTURE_TIMEOUT: Duration = Duration::from_secs(120);
pub(crate) const POLL: Duration = Duration::from_millis(100);
const NIX_SHELL_RC_SELECT: &str = "profile:nix-shell-rc";

/// The detail Owner PID gives when a precondition fails (crate
/// diskgc-core, liveness::owner_pid), for the owner pid `number`.
fn undecidable(number: u32, rule: &str) -> String {
    format!(
        "liveness_contract nix-shell: whether owner pid {number} ended cannot be decided: {rule}"
    )
}
const NOT_INITIAL: &str = "diskgc is not in the initial PID namespace";
const HIDEPID: &str = "the held /proc is mounted with hidepid";

/// A shell of `nix develop` in its own PID namespace, sharing `/tmp`
/// (the TASK-39 counterexample's shape): its rc directory is named after
/// its namespace-local pid, which no host process carries at the first
/// `NSpid` level. While it lives the directory is `in_use` (`owner_pid`,
/// reported as the shell's host pid); once it is killed (and reaped) the
/// directory, which `nix develop` always leaves, is reclaimed.
pub fn nix_shell_rc_shell_in_private_pid_namespace_is_kept(ctx: &Ctx) -> TestResult {
    below_the_goal(ctx)?;
    let base = free_pid_range()?;
    let namespace = Namespace::start(false)?;
    namespace.set_last_pid(base)?;
    let pidfile = "/home/tester/lc/shellpid";
    let shell = Command::new("/bin/nsenter")
        .args(namespace.enter_args())
        .arg(&ctx.self_exe)
        .args(["as-tester", "--", "/bin/sh", "-c", NIX_DEVELOP_SHELL_SCRIPT])
        .args(["nix-develop-shell", NIX_SHELL_BASH, pidfile])
        .env_clear()
        .envs(TESTER_ENV.iter().copied())
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("start nix develop in the namespace: {e}"))?;
    let mut shell = KillOnDrop(shell);
    let inner: u32 = wait_for(FIXTURE_TIMEOUT, "the nix develop shell's pid file", || {
        if let Some(status) = shell.0.try_wait().map_err(|e| e.to_string())? {
            return Err(format!("nix develop exited early: {status}"));
        }
        Ok(std::fs::read_to_string(pidfile)
            .ok()
            .and_then(|text| text.trim().parse().ok()))
    })?;
    ensure!(
        (base..base + PID_RANGE_WIDTH).contains(&inner),
        "the shell's namespace pid {inner} is outside the reserved range from {base}"
    );
    let rc_dir = only_rc_dir(ctx.scratch.root(), inner)?;
    let host = namespace.host_pid_of(inner)?;
    ensure!(
        host != inner,
        "the shell's host pid equals its namespace pid {inner}"
    );
    carriers_are_only(inner, host)?;
    info!(
        "nix develop shell: namespace pid {inner}, host pid {host}, rc directory {}",
        rc_dir.display()
    );

    let clock = SandboxClockOffset::install(CLOCK_OFFSET_SECONDS)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let live_plan = ensure_run(ctx, &dry_run_args())?;
    let live_run = ensure_run(ctx, &reclaim_args())?;
    oracle
        .judge_runs(&[&live_plan, &live_run])?
        .assert_clean()?;
    live_plan.expect_code(0)?;
    live_run.expect_code(1)?;
    expect_plan(&live_plan, &[(&rc_dir, Some("in_use"))])?;
    expect_skipped(&live_run, &rc_dir, "in_use", &[(host, "owner_pid")], None)?;
    ensure!(rc_dir.exists(), "the live shell's rc directory is gone");

    // SIGKILL to the namespace's init kills every process in it; the
    // shell's parent (nsenter, outside) reaps it. Until then a zombie keeps
    // the pid, which retains.
    namespace.kill()?;
    let status = shell
        .0
        .wait()
        .map_err(|e| format!("reap the killed nix develop: {e}"))?;
    info!("killed nix develop: {status}");
    wait_for(FIXTURE_TIMEOUT, "the killed shell to leave /proc", || {
        Ok((!Path::new(&format!("/proc/{host}")).exists()).then_some(()))
    })?;
    ensure!(
        rc_dir.exists(),
        "nix develop removed its rc directory when killed"
    );
    carriers_are_only(inner, 0)?;
    let oracle = Oracle::arm(Fixture::new().eligible(&rc_dir))?;
    let dead_run = ensure_run(ctx, &reclaim_args())?;
    oracle.judge_runs(&[&dead_run])?.assert_clean()?;
    dead_run.expect_code(1)?;
    expect_action(&dead_run, &rc_dir)?;
    ensure!(
        !rc_dir.exists(),
        "the dead shell's rc directory was not reclaimed"
    );
    drop(clock);
    Ok(())
}

/// diskgc in a PID namespace of its own cannot see sibling namespaces, so
/// no PID answer permits: a dead owner's and a live owner's directory are
/// both `producer_unknown` naming the namespace rule, in the plan and in
/// a reclaim, and nothing is deleted. The same directories judged from the
/// initial namespace afterwards (the positive control): the dead owner's
/// is reclaimed, the live owner's is `in_use` (`owner_pid`).
pub fn owner_pid_candidates_are_kept_when_diskgc_has_its_own_pid_namespace(
    ctx: &Ctx,
) -> TestResult {
    let s = &ctx.scratch;
    let base = free_pid_range()?;
    // A number no process carries at any level (the whole range is free;
    // this test's namespace allocates small pids, far below it).
    let dead = base + PID_RANGE_WIDTH - 1;
    let dead_dir = s.mkdir(&format!("nix-develop-{dead}-1"), 0o700, TESTER)?;
    let live = Command::new(&ctx.self_exe)
        .args(["as-tester", "--", "/bin/sleep", "600"])
        .env_clear()
        .envs(TESTER_ENV.iter().copied())
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("start the live owner: {e}"))?;
    let live = KillOnDrop(live);
    let live_pid = live.0.id();
    let live_dir = s.mkdir(&format!("nix-shell-{live_pid}-1"), 0o700, TESTER)?;
    s.write_file(
        &format!("nix-shell-{live_pid}-1/rc"),
        b"rc\n",
        0o600,
        TESTER,
    )?;
    let namespace = Namespace::start(false)?;

    let clock = SandboxClockOffset::install(CLOCK_OFFSET_SECONDS)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let nested_plan = namespace.diskgc_as_tester(ctx, &dry_run_args())?;
    let nested_run = namespace.diskgc_as_tester(ctx, &reclaim_args())?;
    oracle
        .judge_runs(&[&nested_plan, &nested_run])?
        .assert_clean()?;
    nested_plan.expect_code(0)?;
    nested_run.expect_code(1)?;
    expect_plan(
        &nested_plan,
        &[
            (&dead_dir, Some("producer_unknown")),
            (&live_dir, Some("producer_unknown")),
        ],
    )?;
    for (dir, number) in [(&dead_dir, dead), (&live_dir, live_pid)] {
        expect_skipped(
            &nested_run,
            dir,
            "producer_unknown",
            &[],
            Some(&undecidable(number, NOT_INITIAL)),
        )?;
        ensure!(dir.exists(), "{} was deleted", dir.display());
    }

    let oracle = Oracle::arm(Fixture::new().eligible(&dead_dir))?;
    let initial_run = ensure_run(ctx, &reclaim_args())?;
    oracle.judge_runs(&[&initial_run])?.assert_clean()?;
    initial_run.expect_code(1)?;
    expect_action(&initial_run, &dead_dir)?;
    expect_skipped(
        &initial_run,
        &live_dir,
        "in_use",
        &[(live_pid, "owner_pid")],
        None,
    )?;
    ensure!(!dead_dir.exists(), "the dead owner's directory remains");
    ensure!(live_dir.exists(), "the live owner's directory is gone");
    drop(clock);
    drop(live);
    Ok(())
}

/// A `/proc` mounted with `hidepid=invisible` hides a process from every
/// reader that may not ptrace it, the user's own included: a shell whose
/// group differs (as after `newgrp`) is unlisted for tester. No PID answer
/// permits under `hidepid`, for tester and for root (`--allow-root`, which
/// sees the shell), and the detail names the `hidepid` rule. Rootless
/// Podman cannot mount a `hidepid` `/proc` for the initial namespace, so
/// diskgc runs in a nested one here and the rule is observed by its
/// detail (Owner PID names `hidepid` before the namespace). Positive
/// control: from the initial namespace without `hidepid` the shell is
/// listed, and the same directory is `in_use_unknown`; In use decides it
/// before the contract, since the shell's fd table is denied to tester
/// (ptrace access, like `hidepid`, also compares groups), so evidence
/// `unreadable` (a live process that cannot be inspected blocks every
/// candidate, as unknown rather than seen use: TASK-190).
pub fn owner_pid_is_unknown_under_hidepid(ctx: &Ctx) -> TestResult {
    let base = free_pid_range()?;
    let namespace = Namespace::start(true)?;
    namespace.set_last_pid(base)?;
    // The "newgrp" shell: tester's uid, another user's group.
    let shell = Command::new("/bin/nsenter")
        .args(namespace.enter_args_as(TESTER.uid, OTHER.gid))
        .args([
            "/bin/sh",
            "-c",
            "mkdir /tmp/nix-develop-$$-1 && echo rc > /tmp/nix-develop-$$-1/rc && exec sleep 600",
        ])
        .env_clear()
        .env("PATH", "/bin")
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("start the hidden shell: {e}"))?;
    let _shell = KillOnDrop(shell);
    let scratch_root = ctx.scratch.root().to_owned();
    let (rc_dir, inner) = wait_for(FIXTURE_TIMEOUT, "the hidden shell's directory", || {
        let found = rc_dirs(&scratch_root)?;
        Ok(match found.as_slice() {
            [] => None,
            [(dir, pid)] => Some((dir.clone(), *pid)),
            more => return Err(format!("more than one rc directory: {more:?}")),
        })
    })?;
    ensure!(
        (base..base + PID_RANGE_WIDTH).contains(&inner),
        "the hidden shell's namespace pid {inner} is outside the reserved range from {base}"
    );
    // The kernel premise of the rule, observed: tester cannot list the
    // shell, root can.
    ensure!(
        namespace.status(TESTER.uid, TESTER.gid, &format!("test ! -e /proc/{inner}"))?,
        "the shell of another group is visible to tester under hidepid=invisible"
    );
    ensure!(
        namespace.status(ROOT.uid, ROOT.gid, &format!("test -e /proc/{inner}"))?,
        "root cannot see the shell under hidepid=invisible"
    );
    let host = namespace.host_pid_of(inner)?;
    carriers_are_only(inner, host)?;
    ctx.run_diskgc.write_file(
        "hidepid-root.yaml",
        b"version: 1\nowners: [tester]\n",
        0o600,
        ROOT,
    )?;

    let clock = SandboxClockOffset::install(CLOCK_OFFSET_SECONDS)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let tester_plan = namespace.diskgc_as_tester(ctx, &dry_run_args())?;
    let tester_run = namespace.diskgc_as_tester(ctx, &reclaim_args())?;
    let root_run = namespace.diskgc_as_root(
        ctx,
        &[
            "ensure",
            "--allow-root",
            "--json",
            "--no-index",
            "--config",
            "/run/diskgc/hidepid-root.yaml",
            "--min-free",
            "100%",
            "--select",
            NIX_SHELL_RC_SELECT,
            "/tmp",
        ],
    )?;
    let initial_run = ensure_run(ctx, &reclaim_args())?;
    oracle
        .judge_runs(&[&tester_plan, &tester_run, &root_run, &initial_run])?
        .assert_clean()?;
    tester_plan.expect_code(0)?;
    expect_plan(&tester_plan, &[(&rc_dir, Some("producer_unknown"))])?;
    for run in [&tester_run, &root_run] {
        run.expect_code(1)?;
        expect_skipped(
            run,
            &rc_dir,
            "producer_unknown",
            &[],
            Some(&undecidable(inner, HIDEPID)),
        )?;
    }
    initial_run.expect_code(1)?;
    expect_skipped(
        &initial_run,
        &rc_dir,
        "in_use_unknown",
        &[(host, "unreadable")],
        None,
    )?;
    ensure!(rc_dir.exists(), "the hidden shell's directory is gone");
    drop(clock);
    Ok(())
}

/// Owner PID's filesystem precondition through the real binary (AC #4 of
/// TASK-112): a pid written on a filesystem off the local list may name a
/// process of another kernel, so even an owner no process carries is
/// `producer_unknown` there, never eligible. The candidate lies on a kernel
/// overlayfs (upper and lower on the scratch tmpfs) and is judged by a user
/// profile with `liveness_contract: nix-shell`, since the built-in's root
/// is `/tmp` itself. Positive control in the same initial PID namespace:
/// the same dead number's directory on the tmpfs `/tmp` is reclaimed by
/// the built-in, so the overlay one is kept by the filesystem rule alone.
pub fn owner_pid_is_unknown_on_an_unlisted_filesystem(ctx: &Ctx) -> TestResult {
    below_the_goal(ctx)?;
    let base = free_pid_range()?;
    let dead = base + PID_RANGE_WIDTH - 1;
    // The upper directory's mode is the overlay root's: sticky and
    // world-writable like /tmp, so tester could delete the candidate.
    for (dir, mode) in [
        ("ovl-lower", 0o755),
        ("ovl-upper", 0o1777),
        ("ovl-work", 0o700),
        ("ovl", 0o755),
    ] {
        ctx.scratch.mkdir(dir, mode, ROOT)?;
    }
    let overlay = OverlayMount::mount(ctx.scratch.root())?;
    let overlay_dir = overlay.at.join(format!("nix-develop-{dead}-1"));
    std::fs::create_dir(&overlay_dir)
        .map_err(|e| format!("mkdir {}: {e}", overlay_dir.display()))?;
    std::fs::write(overlay_dir.join("rc"), b"rc\n")
        .map_err(|e| format!("write rc in {}: {e}", overlay_dir.display()))?;
    for path in [overlay_dir.join("rc"), overlay_dir.clone()] {
        rustix::fs::chown(
            &path,
            Some(rustix::fs::Uid::from_raw(TESTER.uid)),
            Some(rustix::fs::Gid::from_raw(TESTER.gid)),
        )
        .map_err(|e| format!("chown {}: {e}", path.display()))?;
    }
    let magic = rustix::fs::statfs(&overlay_dir)
        .map_err(|e| format!("statfs {}: {e}", overlay_dir.display()))?
        .f_type;
    ensure!(
        u64::try_from(magic).ok() == Some(OVERLAYFS_SUPER_MAGIC),
        "precondition: {} is on filesystem type {magic:#x}, not overlayfs",
        overlay_dir.display()
    );
    let tmpfs_dir = ctx
        .scratch
        .mkdir(&format!("nix-develop-{dead}-2"), 0o700, TESTER)?;
    ctx.scratch
        .write_file(&format!("nix-develop-{dead}-2/rc"), b"rc\n", 0o600, TESTER)?;
    carriers_are_only(dead, 0)?;
    ctx.home.write_file(
        "overlay-probe.yaml",
        format!(
            "version: 1\nprofiles:\n  - name: nix-shell-overlay-probe\n    \
             find: {{roots: [{at}], paths: [\"nix-develop-*-*\"], type: dir}}\n    \
             where: {{liveness_contract: nix-shell}}\n    min_idle_for: 1h\n    \
             action: delete\n    examples: {{match: [{at}/nix-develop-4242-1]}}\n",
            at = overlay.at.display()
        )
        .as_bytes(),
        0o600,
        TESTER,
    )?;
    let at = overlay
        .at
        .to_str()
        .ok_or("non-UTF-8 overlay path")?
        .to_owned();
    let overlay_args = |dry_run: bool| {
        let mut args = Vec::new();
        if dry_run {
            args.push("--dry-run");
        }
        args.extend([
            "--json",
            "--no-index",
            "--config",
            "/home/tester/overlay-probe.yaml",
            "--min-free",
            "100%",
            "--select",
            "profile:nix-shell-overlay-probe",
            at.as_str(),
        ]);
        args
    };

    let clock = SandboxClockOffset::install(CLOCK_OFFSET_SECONDS)?;
    let oracle = Oracle::arm(Fixture::new())?;
    let overlay_plan = ensure_run(ctx, &overlay_args(true))?;
    let overlay_run = ensure_run(ctx, &overlay_args(false))?;
    oracle
        .judge_runs(&[&overlay_plan, &overlay_run])?
        .assert_clean()?;
    overlay_plan.expect_code(0)?;
    expect_plan(&overlay_plan, &[(&overlay_dir, Some("producer_unknown"))])?;
    overlay_run.expect_code(1)?;
    let rule = format!("filesystem type {OVERLAYFS_SUPER_MAGIC:#x} is not on the local list");
    expect_skipped(
        &overlay_run,
        &overlay_dir,
        "producer_unknown",
        &[],
        Some(&undecidable(dead, &rule)),
    )?;
    ensure!(overlay_dir.exists(), "the overlay candidate was deleted");

    let oracle = Oracle::arm(Fixture::new().eligible(&tmpfs_dir))?;
    let initial_run = ensure_run(ctx, &reclaim_args())?;
    oracle.judge_runs(&[&initial_run])?.assert_clean()?;
    initial_run.expect_code(1)?;
    expect_action(&initial_run, &tmpfs_dir)?;
    ensure!(!tmpfs_dir.exists(), "the tmpfs control directory remains");
    ensure!(overlay_dir.exists(), "the overlay candidate was deleted");
    drop(clock);
    drop(overlay);
    Ok(())
}

/// `OVERLAYFS_SUPER_MAGIC` (linux/magic.h).
const OVERLAYFS_SUPER_MAGIC: u64 = 0x794c_7630;

/// A kernel overlayfs at `<scratch>/ovl` over `ovl-lower`, `ovl-upper` and
/// `ovl-work` on the scratch tmpfs (`userxattr`: the container's user
/// namespace cannot set trusted xattrs). Detached when dropped, so the
/// scratch tmpfs can be unmounted after the test.
struct OverlayMount {
    at: PathBuf,
}

impl OverlayMount {
    fn mount(scratch: &Path) -> Result<Self, String> {
        let at = scratch.join("ovl");
        let data = format!(
            "lowerdir={0}/ovl-lower,upperdir={0}/ovl-upper,workdir={0}/ovl-work,userxattr",
            scratch.display()
        );
        let data = std::ffi::CString::new(data).map_err(|e| e.to_string())?;
        rustix::mount::mount(
            "diskgc-e2e-overlay",
            &at,
            "overlay",
            rustix::mount::MountFlags::NOSUID | rustix::mount::MountFlags::NODEV,
            data.as_c_str(),
        )
        .map_err(|e| format!("mount overlay at {}: {e}", at.display()))?;
        info!("overlayfs at {}", at.display());
        Ok(Self { at })
    }
}

impl Drop for OverlayMount {
    fn drop(&mut self) {
        let _ = rustix::mount::unmount(&self.at, rustix::mount::UnmountFlags::DETACH);
    }
}

fn dry_run_args() -> Vec<&'static str> {
    let mut args = vec!["--dry-run"];
    args.extend(reclaim_args());
    args
}

fn reclaim_args() -> Vec<&'static str> {
    vec![
        "--json",
        "--no-index",
        "--min-free",
        "100%",
        "--select",
        NIX_SHELL_RC_SELECT,
        "/tmp",
    ]
}

/// The plan has exactly these candidates with these reasons.
pub(crate) fn expect_plan(run: &Run, want: &[(&PathBuf, Option<&str>)]) -> TestResult {
    let report = report(run)?;
    let mut got: Vec<(String, Option<String>)> = report["plan"]
        .as_array()
        .ok_or_else(|| format!("no plan array: {report}"))?
        .iter()
        .map(|entry| {
            (
                entry["candidate"].as_str().unwrap_or_default().to_owned(),
                entry["reason"].as_str().map(str::to_owned),
            )
        })
        .collect();
    got.sort();
    let mut want: Vec<(String, Option<String>)> = want
        .iter()
        .map(|(path, reason)| (path.display().to_string(), reason.map(str::to_owned)))
        .collect();
    want.sort();
    ensure!(got == want, "plan {got:?}, expected {want:?}: {report}");
    Ok(())
}

/// The reclaim skipped `candidate` for `reason` with exactly these
/// process blockers (`[]`: none reported), and, when given, this detail.
pub(crate) fn expect_skipped(
    run: &Run,
    candidate: &Path,
    reason: &str,
    blockers: &[(u32, &str)],
    detail: Option<&str>,
) -> TestResult {
    let report = report(run)?;
    let entry = report["skipped"]
        .as_array()
        .and_then(|entries| {
            entries
                .iter()
                .find(|entry| entry["candidate"].as_str() == candidate.to_str())
        })
        .ok_or_else(|| format!("{} not skipped: {report}", candidate.display()))?;
    ensure!(
        entry["reason"] == reason,
        "{} skipped as {}, expected {reason}: {entry}",
        candidate.display(),
        entry["reason"]
    );
    let got: Vec<(u64, String)> = match &entry["process_blockers"] {
        Value::Null => Vec::new(),
        Value::Array(list) => list
            .iter()
            .map(|b| {
                (
                    b["pid"].as_u64().unwrap_or(0),
                    b["evidence"].as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect(),
        other => return Err(format!("process_blockers is {other}")),
    };
    let want: Vec<(u64, String)> = blockers
        .iter()
        .map(|(pid, evidence)| (u64::from(*pid), (*evidence).to_owned()))
        .collect();
    ensure!(
        got == want,
        "{} blockers {got:?}, expected {want:?}: {entry}",
        candidate.display()
    );
    if let Some(detail) = detail {
        ensure!(
            entry["detail"] == detail,
            "{} detail {}, expected {detail:?}",
            candidate.display(),
            entry["detail"]
        );
    }
    Ok(())
}

/// The reclaim acted on `candidate`.
pub(crate) fn expect_action(run: &Run, candidate: &Path) -> TestResult {
    let report = report(run)?;
    ensure!(
        report["actions"].as_array().is_some_and(|actions| actions
            .iter()
            .any(|a| a["candidate"].as_str() == candidate.to_str())),
        "no action on {}: {report}",
        candidate.display()
    );
    Ok(())
}

/// Every rc-shaped directory directly in `tmp`, with the pid in its name.
fn rc_dirs(tmp: &Path) -> Result<Vec<(PathBuf, u32)>, String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(tmp).map_err(|e| format!("list {}: {e}", tmp.display()))? {
        let entry = entry.map_err(|e| format!("list {}: {e}", tmp.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rest = name
            .strip_prefix("nix-develop-")
            .or_else(|| name.strip_prefix("nix-shell-"));
        if let Some((pid, _)) = rest.and_then(|r| r.split_once('-'))
            && let Ok(pid) = pid.parse()
        {
            found.push((entry.path(), pid));
        }
    }
    found.sort();
    Ok(found)
}

/// The one rc directory in `tmp`, which must carry `pid`.
fn only_rc_dir(tmp: &Path, pid: u32) -> Result<PathBuf, String> {
    match rc_dirs(tmp)?.as_slice() {
        [(dir, named)] if *named == pid => Ok(dir.clone()),
        other => Err(format!(
            "expected one rc directory for pid {pid}: {other:?}"
        )),
    }
}

/// Every pid any listed process carries at any `NSpid` level, read from
/// the host's `/proc` (this container shares its PID namespace). A status
/// that cannot be read (the process exited) is skipped.
fn host_nspid_levels() -> Result<Vec<Vec<u32>>, String> {
    let mut all = Vec::new();
    for entry in std::fs::read_dir("/proc").map_err(|e| format!("list /proc: {e}"))? {
        let entry = entry.map_err(|e| format!("list /proc: {e}"))?;
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
            continue;
        };
        let levels: Vec<u32> = status
            .lines()
            .find_map(|l| l.strip_prefix("NSpid:"))
            .ok_or_else(|| format!("/proc/{pid}/status has no NSpid"))?
            .split_whitespace()
            .map(|n| n.parse().map_err(|e| format!("NSpid of {pid}: {e}")))
            .collect::<Result<_, _>>()?;
        all.push(levels);
    }
    Ok(all)
}

/// The first `base` from `pid_max / PID_RANGE_SEARCH_FROM_FRACTION` such
/// that no process carries a pid in `base..base + PID_RANGE_WIDTH` at any
/// level, all below the host's `pid_max` (a nested namespace cannot
/// allocate above it).
fn free_pid_range() -> Result<u32, String> {
    require_shifted_uid_map()?;
    let pid_max: u32 = std::fs::read_to_string("/proc/sys/kernel/pid_max")
        .map_err(|e| format!("read /proc/sys/kernel/pid_max: {e}"))?
        .trim()
        .parse()
        .map_err(|e| format!("parse /proc/sys/kernel/pid_max: {e}"))?;
    ensure!(
        pid_max >= 4 * PID_RANGE_WIDTH * PID_RANGE_SEARCH_FROM_FRACTION,
        "precondition: kernel.pid_max is {pid_max}, too small for a free namespace pid \
         range of {PID_RANGE_WIDTH} (systemd sets 4194304)"
    );
    let used: BTreeSet<u32> = host_nspid_levels()?.into_iter().flatten().collect();
    let mut base = pid_max / PID_RANGE_SEARCH_FROM_FRACTION;
    while base + PID_RANGE_WIDTH < pid_max {
        if used.range(base..base + PID_RANGE_WIDTH).next().is_none() {
            info!("free namespace pid range from {base}");
            return Ok(base);
        }
        base += PID_RANGE_WIDTH;
    }
    Err(format!(
        "no free namespace pid range below pid_max {pid_max}"
    ))
}

/// `--min-free 100%` is met on a tmpfs that holds no allocated bytes
/// (an empty directory takes none): `ensure` would then act on nothing. A
/// file outside every candidate keeps `/tmp` below the goal.
pub(crate) fn below_the_goal(ctx: &Ctx) -> TestResult {
    ctx.scratch
        .write_file("not-a-candidate", &[0x5a; 8192], 0o600, TESTER)?;
    Ok(())
}

/// The container's uid map must be the runner's shifted one (container 0
/// is the parent namespace's 1, not the invoking user): with the host's
/// PID namespace, an unshifted map would make container root the owner of
/// the user's host processes (docs/testing.md, Container setup).
pub(crate) fn require_shifted_uid_map() -> TestResult {
    let map = std::fs::read_to_string("/proc/self/uid_map")
        .map_err(|e| format!("read /proc/self/uid_map: {e}"))?;
    let fields: Vec<&str> = map.split_whitespace().collect();
    ensure!(
        fields == ["0", "1", "65536"],
        "precondition: the container's uid map is {map:?}, not the runner's shifted \
         0 1 65536 (HOST_PID_NAMESPACE_ARGS)"
    );
    Ok(())
}

/// No process but `host` (0: none) carries `number` at any level, so a
/// verdict about `number` is about that one process.
fn carriers_are_only(number: u32, host: u32) -> TestResult {
    let carriers: Vec<Vec<u32>> = host_nspid_levels()?
        .into_iter()
        .filter(|levels| levels.contains(&number))
        .collect();
    let expected: Vec<Vec<u32>> = if host == 0 {
        Vec::new()
    } else {
        vec![vec![host, number]]
    };
    ensure!(
        carriers == expected,
        "processes carrying {number}: {carriers:?}, expected {expected:?}"
    );
    Ok(())
}

/// A nested PID namespace with its own mount namespace and `/proc`
/// (optionally `hidepid=invisible`), held by a sleeping init. Dropped:
/// its init is killed, which kills every process in it.
///
/// Init is signalled only through a pidfd opened while `unshare` (its
/// parent, which reaps it) still waits for it: a pidfd names that process,
/// never a later holder of its number.
struct Namespace {
    /// Held for its drop, after init's: kills and reaps `unshare`.
    _unshare: KillOnDrop,
    init: u32,
    init_pidfd: std::os::fd::OwnedFd,
}

impl Namespace {
    fn start(hidepid: bool) -> Result<Self, String> {
        let options = if hidepid { "-o hidepid=invisible " } else { "" };
        let unshare = Command::new("/bin/unshare")
            .args(["-p", "-f", "-m", "--propagation", "private", "--"])
            .args([
                "/bin/sh",
                "-c",
                &format!("mount -t proc {options}proc /proc && exec sleep 600"),
            ])
            .env_clear()
            .env("PATH", "/bin")
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("start unshare: {e}"))?;
        // Until init is held by a pidfd, a failure kills only unshare (a
        // child of this process, so its pid cannot be reused before it is
        // reaped here), and with it nothing else.
        let mut unshare = KillOnDrop(unshare);
        let parent = unshare.0.id();
        let init = wait_for(FIXTURE_TIMEOUT, "the namespace's init", || {
            if let Some(status) = unshare.0.try_wait().map_err(|e| e.to_string())? {
                return Err(format!("unshare exited early: {status}"));
            }
            let children =
                std::fs::read_to_string(format!("/proc/{parent}/task/{parent}/children"))
                    .unwrap_or_default();
            let Some(init) = children
                .split_whitespace()
                .next()
                .and_then(|p| p.parse::<u32>().ok())
            else {
                return Ok(None);
            };
            // Ready once its /proc is mounted and it sleeps.
            let cmdline = std::fs::read(format!("/proc/{init}/cmdline")).unwrap_or_default();
            Ok((cmdline == b"sleep\x00600\x00").then_some(init))
        })?;
        let raw = rustix::process::Pid::from_raw(init.cast_signed())
            .ok_or_else(|| format!("init pid {init} is not a pid"))?;
        let init_pidfd = rustix::process::pidfd_open(raw, rustix::process::PidfdFlags::empty())
            .map_err(|e| format!("pidfd_open init {init}: {e}"))?;
        // Still unshare's child after the open: the pidfd names init, not a
        // later holder of its number (a reaped child leaves the list).
        let children = std::fs::read_to_string(format!("/proc/{parent}/task/{parent}/children"))
            .unwrap_or_default();
        ensure!(
            children.split_whitespace().next() == Some(init.to_string().as_str()),
            "init {init} left unshare before its pidfd was checked: {children:?}"
        );
        let namespace = Self {
            _unshare: unshare,
            init,
            init_pidfd,
        };
        let own = std::fs::read_link("/proc/self/ns/pid").map_err(|e| e.to_string())?;
        ensure!(
            namespace.pid_ns()? != own,
            "the namespace's init is in the harness's PID namespace"
        );
        info!(
            "nested PID namespace {:?}, init host pid {}, hidepid {hidepid}",
            namespace.pid_ns()?,
            namespace.init
        );
        Ok(namespace)
    }

    fn pid_ns(&self) -> Result<PathBuf, String> {
        std::fs::read_link(format!("/proc/{}/ns/pid", self.init))
            .map_err(|e| format!("read the namespace of {}: {e}", self.init))
    }

    /// `nsenter` arguments into the namespace (PID and mount), before the
    /// program.
    fn enter_args(&self) -> Vec<String> {
        ["-t", &self.init.to_string(), "-p", "-m", "--"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect()
    }

    /// As [`Self::enter_args`], running as `uid` / `gid` (no
    /// supplementary groups).
    fn enter_args_as(&self, uid: u32, gid: u32) -> Vec<String> {
        let init = self.init.to_string();
        let (uid, gid) = (uid.to_string(), gid.to_string());
        ["-t", &init, "-p", "-m", "-S", &uid, "-G", &gid, "--"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect()
    }

    /// The next process the namespace makes gets `base + 1` (its
    /// `ns_last_pid`, written by a process inside it).
    fn set_last_pid(&self, base: u32) -> TestResult {
        ensure!(
            self.status(
                ROOT.uid,
                ROOT.gid,
                &format!("echo {base} > /proc/sys/kernel/ns_last_pid")
            )?,
            "could not set ns_last_pid to {base}"
        );
        Ok(())
    }

    /// Whether `script` succeeds run by `/bin/sh` in the namespace as
    /// `uid` / `gid` (untraced: a fixture probe, it deletes nothing).
    fn status(&self, uid: u32, gid: u32, script: &str) -> Result<bool, String> {
        let output = Command::new("/bin/nsenter")
            .args(self.enter_args_as(uid, gid))
            .args(["/bin/sh", "-c", script])
            .env_clear()
            .env("PATH", "/bin")
            .current_dir("/")
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("nsenter {script:?}: {e}"))?;
        info!(
            "in the namespace as {uid}:{gid}: {script:?} -> {} {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(output.status.success())
    }

    /// The host pid of the process the namespace knows as `inner`.
    fn host_pid_of(&self, inner: u32) -> Result<u32, String> {
        let ns = self.pid_ns()?;
        let found: Vec<u32> = host_nspid_levels()?
            .into_iter()
            .filter(|levels| levels.len() >= 2 && levels.last() == Some(&inner))
            .map(|levels| levels[0])
            .filter(|host| {
                std::fs::read_link(format!("/proc/{host}/ns/pid")).is_ok_and(|n| n == ns)
            })
            .collect();
        match found.as_slice() {
            [host] => Ok(*host),
            other => Err(format!(
                "namespace pid {inner}: host pids {other:?}, expected one"
            )),
        }
    }

    /// `diskgc ARGS` in the namespace as tester, traced (strace follows
    /// nsenter's fork). `HOME` is tester's, as for every tester run.
    fn diskgc_as_tester(&self, ctx: &Ctx, args: &[&str]) -> Result<Run, String> {
        let diskgc = ctx.diskgc();
        let mut full: Vec<String> = self.enter_args();
        full.extend(["/bin/env".to_owned(), "HOME=/home/tester".to_owned()]);
        full.push(ctx.self_exe.display().to_string());
        full.extend(["as-tester".to_owned(), "--".to_owned()]);
        full.push(diskgc.display().to_string());
        full.push("ensure".to_owned());
        full.extend(args.iter().map(|a| (*a).to_owned()));
        self.traced(ctx, &full)
    }

    /// `diskgc ARGS` in the namespace as container root, traced.
    fn diskgc_as_root(&self, ctx: &Ctx, args: &[&str]) -> Result<Run, String> {
        let mut full: Vec<String> = self.enter_args();
        full.push(ctx.diskgc().display().to_string());
        full.extend(args.iter().map(|a| (*a).to_owned()));
        self.traced(ctx, &full)
    }

    fn traced(&self, ctx: &Ctx, nsenter_args: &[String]) -> Result<Run, String> {
        let refs: Vec<&str> = nsenter_args.iter().map(String::as_str).collect();
        let run = ctx.run_as_root(Path::new("/bin/nsenter"), &refs)?;
        info!(
            "nsenter {} -> {:?}\n--- stdout\n{}--- stderr\n{}",
            refs.join(" "),
            run.code,
            run.stdout,
            run.stderr
        );
        Ok(run)
    }

    /// Kills the namespace's init (and with it every process inside),
    /// through its pidfd; an init that already ended is not an error.
    fn kill(&self) -> TestResult {
        match rustix::process::pidfd_send_signal(&self.init_pidfd, rustix::process::Signal::KILL) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(e) => Err(format!("pidfd_send_signal init {}: {e}", self.init)),
        }
    }
}

impl Drop for Namespace {
    fn drop(&mut self) {
        // Init first (everything in the namespace dies with it); then the
        // `_unshare` field's own drop kills and reaps it.
        let _ = self.kill();
    }
}

/// A fixture child killed and reaped when dropped. `Child::kill` signals
/// by pid, which is safe: until this process reaps it the pid stays its
/// (a zombie holds it), and once reaped `Child` sends nothing.
pub(crate) struct KillOnDrop(pub(crate) Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Polls `probe` until it returns a value, bounded by `timeout`.
pub(crate) fn wait_for<T>(
    timeout: Duration,
    what: &str,
    mut probe: impl FnMut() -> Result<Option<T>, String>,
) -> Result<T, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = probe()? {
            return Ok(value);
        }
        ensure!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(POLL);
    }
}
