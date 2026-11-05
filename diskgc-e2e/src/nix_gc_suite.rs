//! The Nix GC container scenarios (docs/testing.md, Nix GC; TASK-36): kind
//! `nix-store` against a real multi-user CppNix 2.34.8 whose daemon runs as
//! container root on the container's own store, with diskgc as tester
//! (uid 1000).
//!
//! The image (flake.nix, e2eNixGcImage) carries its own `/nix/store` and
//! database (its paths registered and rooted). Each scenario first checks
//! that the store holds the image's paths only (no host store is mounted),
//! then moves it onto a tmpfs of its own, so what a GC frees is measured on
//! a filesystem nothing else writes and nothing outside the container is
//! touched. The deletion oracle (crate::oracle) declares exactly the dead
//! store paths eligible: any rooted path, generation or image path that
//! disappears is a violation, whatever diskgc or Nix reported.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::ensure_suite::{not_forwarded_lines, report};
use crate::lock_release::{RELEASE_BOUND, flock_free_within_release_bound, flock_held};
use crate::oracle::{Fixture, Oracle};
use crate::slow_stderr::SlowStderr;
use crate::suite::{Ctx, HOST_LOCK, TestResult};
use crate::under_test::{self, Run, TESTER_ENV};
use crate::{ensure, info};

const NIX: &str = "/bin/nix";
const NIX_STORE: &str = "/bin/nix-store";
const NIX_ENV: &str = "/bin/nix-env";
const NIX_BUILD: &str = "/bin/nix-build";
const NIX_DAEMON: &str = "/bin/nix-daemon";
const SH: &str = "/bin/sh";
const STORE: &str = "/nix/store";
const STATE: &str = "/nix/var/nix";
const SOCKET: &str = "/nix/var/nix/daemon-socket/socket";
const PROFILES: &str = "/nix/var/nix/profiles";
const NIX_CONF: &str = "/etc/nix/nix.conf";
/// Where the store is copied before its tmpfs is moved onto `/nix/store`.
const STAGING: &str = "/nix/.diskgc-e2e-store";
/// The other CppNix client of the image (flake.nix, nixGcClients).
const OTHER_CLIENTS: &str = "/opt/nix-clients";
/// The marker plugin (flake.nix, nixMarkerPlugin).
const PLUGIN: &str = "/opt/nix-marker-plugin.so";
/// The group of a multi-user store (flake.nix: `nixbld`).
const NIXBLD: u32 = 30000;
/// Where the tester's source files for `nix-store --add` live.
const SOURCES: &str = "/tmp/nix-sources";

/// The worker protocol's opening magic numbers (CppNix).
const WORKER_MAGIC_1: u64 = 0x6e69_7863;
const WORKER_MAGIC_2: u64 = 0x6478_696f;

// ---- the store and the daemon ------------------------------------------

/// A shell command as container root; its stdout, or the error with both
/// streams.
fn root_sh(script: &str) -> Result<String, String> {
    let out = Command::new(SH)
        .args(["-c", script])
        .env_clear()
        .env("PATH", "/bin")
        .env("HOME", "/root")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("run as root {script:?}: {e}"))?;
    ensure!(
        out.status.success(),
        "as root {script:?}: {}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `program args...` as tester (through `as-tester`), with tester's
/// environment plus `env`; its stdout, or the error with both streams.
fn tester(ctx: &Ctx, env: &[(&str, &str)], program: &str, args: &[&str]) -> Result<String, String> {
    let out = tester_command(ctx, env, program, args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("run as tester {program} {args:?}: {e}"))?;
    ensure!(
        out.status.success(),
        "as tester {program} {args:?}: {}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn tester_command(ctx: &Ctx, env: &[(&str, &str)], program: &str, args: &[&str]) -> Command {
    let mut command = Command::new(&ctx.self_exe);
    command
        .args(["as-tester", "--", program])
        .args(args)
        .env_clear()
        .envs(TESTER_ENV.iter().copied())
        .envs(env.iter().copied())
        .current_dir("/");
    command
}

/// The pre-scenario check (docs/testing.md, Nix GC): `/nix/store` is the
/// image's own (no mount there, so no host store), and it holds exactly the
/// paths its database registers. Then the store moves onto a tmpfs of
/// `size_mib`, mode 1775 root:nixbld as a multi-user store is.
fn own_store(size_mib: u64) -> Result<(), String> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|e| format!("read mountinfo: {e}"))?;
    ensure!(
        !mountinfo
            .lines()
            .any(|line| line.split(' ').nth(4) == Some(STORE)),
        "{STORE} is a mount before the scenario: not the image's own store:\n{mountinfo}"
    );
    let valid: BTreeSet<String> = root_sh(&format!(
        "{NIX} --extra-experimental-features nix-command path-info --all"
    ))?
    .lines()
    .map(str::to_owned)
    .collect();
    let present = store_entries()?;
    ensure!(
        valid == present,
        "{STORE} is not exactly the image's registered paths: only present {:?}, only \
         registered {:?}",
        present.difference(&valid).collect::<Vec<_>>(),
        valid.difference(&present).collect::<Vec<_>>()
    );
    info!(
        "{STORE}: the image's {} registered paths, no mount",
        valid.len()
    );
    root_sh(&format!("mkdir -p {STAGING}"))?;
    let data =
        std::ffi::CString::new(format!("size={size_mib}m,mode=1775,gid={NIXBLD}")).expect("no NUL");
    rustix::mount::mount(
        "diskgc-e2e-nix-store",
        STAGING,
        "tmpfs",
        rustix::mount::MountFlags::NOSUID | rustix::mount::MountFlags::NODEV,
        data.as_c_str(),
    )
    .map_err(|e| format!("mount the store's tmpfs at {STAGING}: {e}"))?;
    let started = Instant::now();
    // `cp -a` of `.` gives the target directory the source's owner and
    // mode: set the multi-user store's again.
    root_sh(&format!(
        "cp -a {STORE}/. {STAGING}/ && chown 0:{NIXBLD} {STAGING} && chmod 1775 {STAGING}"
    ))?;
    rustix::mount::mount_move(STAGING, STORE)
        .map_err(|e| format!("move the store's tmpfs onto {STORE}: {e}"))?;
    let meta = std::fs::metadata(STORE).map_err(|e| format!("stat {STORE}: {e}"))?;
    ensure!(
        meta.uid() == 0 && meta.gid() == NIXBLD && meta.mode() & 0o7777 == 0o1775,
        "{STORE} on its tmpfs is {}:{} {:o}, not root:nixbld 1775",
        meta.uid(),
        meta.gid(),
        meta.mode() & 0o7777
    );
    ensure!(
        store_entries()? == valid,
        "the copy of {STORE} differs from the image's"
    );
    info!(
        "{STORE} copied to its own tmpfs ({size_mib} MiB) in {:?}",
        started.elapsed()
    );
    Ok(())
}

/// The store paths in `/nix/store` (not `.links`).
fn store_entries() -> Result<BTreeSet<String>, String> {
    let mut entries = BTreeSet::new();
    for entry in std::fs::read_dir(STORE).map_err(|e| format!("list {STORE}: {e}"))? {
        let name = entry
            .map_err(|e| format!("list {STORE}: {e}"))?
            .file_name()
            .to_string_lossy()
            .into_owned();
        if name != ".links" {
            entries.insert(format!("{STORE}/{name}"));
        }
    }
    Ok(entries)
}

/// Append `lines` to the system `nix.conf` (root's, read by the daemon and
/// by every client).
fn configure(lines: &[&str]) -> Result<(), String> {
    for line in lines {
        root_sh(&format!("printf '%s\\n' '{line}' >> {NIX_CONF}"))?;
    }
    Ok(())
}

/// The system Nix daemon, as container root, killed when dropped.
struct Daemon {
    child: Option<Child>,
    log: PathBuf,
}

impl Daemon {
    /// Starts `nix-daemon ARGS` and waits until it answers the worker
    /// protocol's magic number on its socket.
    fn start(ctx: &Ctx, args: &[&str]) -> Result<Self, String> {
        static STARTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let serial = STARTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let log =
            ctx.records
                .write_file(&format!("nix-daemon-{serial}.log"), b"", 0o600, crate::ROOT)?;
        let file = std::fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .map_err(|e| format!("open {}: {e}", log.display()))?;
        let child = Command::new(NIX_DAEMON)
            .args(args)
            .env_clear()
            .env("PATH", "/bin")
            .env("HOME", "/root")
            .stdin(Stdio::null())
            .stdout(file.try_clone().map_err(|e| format!("daemon log: {e}"))?)
            .stderr(file)
            .spawn()
            .map_err(|e| format!("start {NIX_DAEMON} {args:?}: {e}"))?;
        let mut daemon = Self {
            child: Some(child),
            log,
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(child) = daemon.child.as_mut()
                && let Some(status) = child.try_wait().map_err(|e| format!("daemon: {e}"))?
            {
                return Err(format!(
                    "nix-daemon {args:?} exited {status} before serving:\n{}",
                    daemon.log_text()
                ));
            }
            if handshake(Path::new(SOCKET)) == Ok(true) {
                info!("nix-daemon {args:?} serves {SOCKET}");
                return Ok(daemon);
            }
            ensure!(
                Instant::now() < deadline,
                "nix-daemon {args:?} did not serve {SOCKET} within 20 s:\n{}",
                daemon.log_text()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// SIGKILL and reap: the daemon is gone, its socket file stays.
    fn stop(&mut self) -> Result<(), String> {
        if let Some(mut child) = self.child.take() {
            child.kill().map_err(|e| format!("kill nix-daemon: {e}"))?;
            child.wait().map_err(|e| format!("reap nix-daemon: {e}"))?;
        }
        Ok(())
    }

    fn log_text(&self) -> String {
        let text = std::fs::read_to_string(&self.log).unwrap_or_default();
        let start = text.len().saturating_sub(8 * 1024);
        text.get(start..).unwrap_or(&text).to_owned()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        info!("nix-daemon log:\n{}", self.log_text());
    }
}

/// Whether a daemon at `socket` answers the client's magic number.
fn handshake(socket: &Path) -> Result<bool, String> {
    let mut stream = UnixStream::connect(socket).map_err(|e| format!("connect: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(&WORKER_MAGIC_1.to_le_bytes())
        .map_err(|e| e.to_string())?;
    let mut answer = [0u8; 8];
    stream.read_exact(&mut answer).map_err(|e| e.to_string())?;
    Ok(u64::from_le_bytes(answer) == WORKER_MAGIC_2)
}

/// Store paths `nix-store --add`ed by tester through the daemon, one per
/// `(name, size, fill)`: a file of `size` bytes of `fill`. Not rooted:
/// dead until something roots them.
fn add_files(ctx: &Ctx, files: &[(&str, usize, u8)]) -> Result<Vec<PathBuf>, String> {
    let rel = SOURCES.trim_start_matches("/tmp/");
    if !Path::new(SOURCES).exists() {
        ctx.scratch.mkdir(rel, 0o755, crate::TESTER)?;
    }
    let mut args = Vec::new();
    for (name, size, fill) in files {
        ctx.scratch.write_file(
            &format!("{rel}/{name}"),
            &vec![*fill; *size],
            0o644,
            crate::TESTER,
        )?;
        args.push(format!("{SOURCES}/{name}"));
    }
    let mut added = Vec::new();
    for chunk in args.chunks(2000) {
        let chunk: Vec<&str> = chunk.iter().map(String::as_str).collect();
        let mut all = vec!["--add"];
        all.extend(chunk);
        added.extend(
            tester(ctx, &[], NIX_STORE, &all)?
                .lines()
                .map(PathBuf::from),
        );
    }
    ensure!(
        added.len() == files.len(),
        "nix-store --add printed {} paths for {} files",
        added.len(),
        files.len()
    );
    Ok(added)
}

/// `diskgc ensure --json --no-index --min-free MIN --select
/// profile:nix-garbage /nix/store` as tester under strace, with `env` added.
fn ensure_nix(ctx: &Ctx, env: &[(&str, &str)], min_free: &str) -> Result<Run, String> {
    ensure_nix_in(ctx, Path::new("/"), env, &ensure_args(min_free))
}

/// [`ensure_nix`] from `cwd`, with these arguments.
fn ensure_nix_in(
    ctx: &Ctx,
    cwd: &Path,
    env: &[(&str, &str)],
    args: &[String],
) -> Result<Run, String> {
    run_binary_in(ctx, &ctx.diskgc(), cwd, env, args)
}

/// `binary args...` as tester under strace, from `cwd`, with `env` added.
fn run_binary_in(
    ctx: &Ctx,
    binary: &Path,
    cwd: &Path,
    env: &[(&str, &str)],
    args: &[String],
) -> Result<Run, String> {
    let env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    let run =
        under_test::run_as_tester_with_env(&ctx.ledger, &ctx.records, cwd, &env, binary, args)?;
    info!(
        "{} {args:?} in {} -> {:?}\n--- stdout\n{}--- stderr\n{}",
        binary.display(),
        cwd.display(),
        run.code,
        run.stdout,
        run.stderr
    );
    Ok(run)
}

fn ensure_args(min_free: &str) -> Vec<String> {
    [
        "ensure",
        "--json",
        "--no-index",
        "--min-free",
        min_free,
        "--select",
        "profile:nix-garbage",
        STORE,
    ]
    .map(str::to_owned)
    .to_vec()
}

/// The report's one skip of the Nix tier: its reason and candidate.
fn one_skip(report: &Value) -> Result<(String, Value), String> {
    let skipped = report["skipped"].as_array().ok_or("no skipped array")?;
    ensure!(skipped.len() == 1, "expected one skip, got {skipped:?}");
    Ok((
        skipped[0]["reason"].as_str().unwrap_or_default().to_owned(),
        skipped[0]["candidate"].clone(),
    ))
}

/// A run skipped as a whole with `reason`: exit 1 (space not there, no
/// error), no action, nothing deleted (the oracle, with nothing eligible).
fn assert_tier_skipped(run: &Run, reason: &str) -> Result<(), String> {
    let report = report(run)?;
    ensure!(
        run.code == Some(1),
        "exit {:?}, expected 1: {report}",
        run.code
    );
    ensure!(
        report["actions"].as_array().is_some_and(Vec::is_empty),
        "actions: {}",
        report["actions"]
    );
    ensure!(
        report["errors"].as_array().is_some_and(Vec::is_empty),
        "errors: {}",
        report["errors"]
    );
    let (got, _) = one_skip(&report)?;
    ensure!(got == reason, "skip {got}, expected {reason}: {report}");
    ensure!(
        !run.stderr.contains("Nix GC through"),
        "a GC was started: {}",
        run.stderr
    );
    Ok(())
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Processes of uid 1000 whose command line holds every word of `words`.
fn tester_processes_with(words: &[&str]) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            let Ok(meta) = std::fs::metadata(format!("/proc/{pid}")) else {
                return false;
            };
            let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
                return false;
            };
            let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
            meta.uid() == crate::TESTER.uid && words.iter().all(|w| args.contains(&w.as_bytes()))
        })
        .collect()
}

/// Whether `pid` runs: a zombie (exited, not yet reaped by the harness,
/// which inherits orphans as the container's init) does not.
fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(") ")
            .and_then(|(_, rest)| rest.chars().next())
            .is_some_and(|state| state != 'Z' && state != 'X')
    })
}

/// Generations: two of the system profile (root's, through its local store
/// as `nix-env` does) and two of tester's per-user profile (through the
/// daemon), each a store path from `paths`.
fn make_generations(ctx: &Ctx, system: [&Path; 2], user: [&Path; 2]) -> Result<(), String> {
    for path in system {
        root_sh(&format!(
            "{NIX_ENV} -p {PROFILES}/system --set {}",
            path.display()
        ))?;
    }
    root_sh(&format!(
        "mkdir -p {PROFILES}/per-user/tester && chown 1000:1000 {PROFILES}/per-user/tester"
    ))?;
    for path in user {
        tester(
            ctx,
            &[],
            NIX_ENV,
            &[
                "-p",
                &format!("{PROFILES}/per-user/tester/profile"),
                "--set",
                &path.display().to_string(),
            ],
        )?;
    }
    for link in [
        "system-1-link",
        "system-2-link",
        "per-user/tester/profile-1-link",
        "per-user/tester/profile-2-link",
    ] {
        ensure!(
            exists(&Path::new(PROFILES).join(link)),
            "generation {link} was not made"
        );
    }
    Ok(())
}

/// The Nix tier's action of `report`: one, candidate `/nix/store`.
fn nix_action(report: &Value) -> Result<Value, String> {
    let actions = report["actions"].as_array().ok_or("no actions")?;
    ensure!(
        actions.len() == 1,
        "expected one action (one GC), got {actions:?}"
    );
    ensure!(
        actions[0]["candidate"] == STORE,
        "action candidate {}",
        actions[0]["candidate"]
    );
    Ok(actions[0].clone())
}

/// The `--max-freed` diskgc ran the GC with, from its log line.
fn max_freed(stderr: &str) -> Result<u64, String> {
    stderr
        .lines()
        .find_map(|line| {
            line.contains("Nix GC through")
                .then(|| line.rsplit("--max-freed ").next()?.trim().parse().ok())
                .flatten()
        })
        .ok_or_else(|| format!("no GC line with --max-freed in stderr:\n{stderr}"))
}

/// The root check's duration from diskgc's log line, in ms.
fn root_check_ms(stderr: &str) -> Option<u64> {
    stderr.lines().find_map(|line| {
        let rest = line.split("Nix root check:").nth(1)?;
        let ms = rest.rsplit('(').next()?.split(' ').next()?;
        ms.parse().ok()
    })
}

/// What the Nix daemon itself writes below its state directory during a
/// query, a root check or a GC (its database, temporary roots, the GC lock
/// and socket); every other object there, the profiles, generation links
/// and GC root registrations above all, is judged by the oracle.
fn nix_state_writers(fixture: Fixture) -> Fixture {
    ["db", "temproots", "gc.lock", "gc-socket"]
        .into_iter()
        .fold(fixture, |fixture, name| {
            fixture.concurrent_writer(format!("{STATE}/{name}"))
        })
}

/// Every symlink that roots something or is a generation (below the
/// profiles and GC roots directories, and `extra` out-links): its identity
/// (device, inode) and its text.
type Links = std::collections::BTreeMap<PathBuf, (u64, u64, PathBuf)>;

fn protected_links(extra: &[&str]) -> Result<Links, String> {
    fn walk(dir: &Path, links: &mut Links, depth: usize) -> Result<(), String> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(());
        };
        for entry in entries {
            let path = entry
                .map_err(|e| format!("list {}: {e}", dir.display()))?
                .path();
            let meta = std::fs::symlink_metadata(&path)
                .map_err(|e| format!("lstat {}: {e}", path.display()))?;
            if meta.file_type().is_symlink() {
                let text = std::fs::read_link(&path)
                    .map_err(|e| format!("readlink {}: {e}", path.display()))?;
                links.insert(path, (meta.dev(), meta.ino(), text));
            } else if meta.is_dir() && depth < 3 {
                walk(&path, links, depth + 1)?;
            }
        }
        Ok(())
    }
    let mut links = Links::new();
    for dir in [format!("{STATE}/profiles"), format!("{STATE}/gcroots")] {
        walk(Path::new(&dir), &mut links, 0)?;
    }
    for path in extra {
        let meta = std::fs::symlink_metadata(path).map_err(|e| format!("lstat {path}: {e}"))?;
        let text = std::fs::read_link(path).map_err(|e| format!("readlink {path}: {e}"))?;
        links.insert(PathBuf::from(path), (meta.dev(), meta.ino(), text));
    }
    Ok(links)
}

/// The protected links are all still there, the same objects with the same
/// text.
fn assert_links_kept(before: &Links, after: &Links) -> Result<(), String> {
    let changed: Vec<_> = before
        .iter()
        .filter(|(path, id)| after.get(*path) != Some(id))
        .map(|(path, _)| path)
        .collect();
    ensure!(
        changed.is_empty(),
        "protected links removed, replaced or rewritten: {changed:?} (before {} links, after {})",
        before.len(),
        after.len()
    );
    Ok(())
}

fn avail(path: &str) -> Result<u64, String> {
    let st = rustix::fs::statvfs(path).map_err(|e| format!("statvfs {path}: {e}"))?;
    Ok(st.f_bavail * st.f_frsize)
}

// ---- the scenarios --------------------------------------------------------

/// docs/testing.md `nix_gc_keeps_every_kind_of_root` (AC1): a direct root,
/// an indirect out-link root, two generations of the system profile and of
/// a per-user profile, a temporary root held by a paused build and a
/// runtime root (a process with a store file open) all survive the GC;
/// the dead paths go. diskgc's user is not trusted.
pub fn nix_gc_keeps_every_kind_of_root(ctx: &Ctx) -> TestResult {
    keeps_every_kind_of_root(ctx, false, Build::Development)
}

/// docs/testing.md `nix_gc_trusted_and_untrusted_user`: the same scenario
/// with diskgc's user in `trusted-users` (the untrusted run is
/// [`nix_gc_keeps_every_kind_of_root`]); the same paths survive.
pub fn nix_gc_trusted_and_untrusted_user(ctx: &Ctx) -> TestResult {
    keeps_every_kind_of_root(ctx, true, Build::Development)
}

/// docs/testing.md `release_candidate_nix_garbage_dry_run_then_bounded_gc`
/// (TASK-335): the keep-every-root scenario through the gate-lifted
/// `release-candidate` package, whose release row's catalog holds
/// `nix-garbage`. A dry run first (one plan row, size unknown, no GC, no
/// root check, nothing deleted), then a reclaim asked for about 1 MiB more
/// than is free: one GC bounded by that need, not by the filesystem, every
/// root kept and every dead path gone, judged by the deletion oracle.
pub fn release_candidate_nix_garbage_dry_run_then_bounded_gc(ctx: &Ctx) -> TestResult {
    keeps_every_kind_of_root(ctx, false, Build::ReleaseCandidate)
}

/// Which build a scenario runs, and so how much it asks to free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Build {
    /// The e2e development build (`diskgc`), asked for `--min-free 100%`.
    Development,
    /// The `release-candidate` package (`diskgc-release`, the default
    /// `diskgc` of the release runner), dry run first, then a bounded need.
    ReleaseCandidate,
}

/// The need the release-candidate reclaim asks for beyond what is free:
/// more than the scenario's dead paths (120 KiB), far less than the store's
/// 1 GiB tmpfs, so a `--max-freed` near it shows the bound is the need.
const BOUNDED_NEED_KIB: u64 = 1024;

/// How far diskgc's `--max-freed` may lie from the need the test measured
/// just before the run: one tmpfs page either way for rounding, as nothing
/// writes the store's tmpfs in between.
const NEED_SLACK: u64 = 4096;

fn keeps_every_kind_of_root(ctx: &Ctx, trusted: bool, build_kind: Build) -> TestResult {
    let binary = match build_kind {
        Build::Development => ctx.diskgc(),
        Build::ReleaseCandidate => {
            crate::release_suite::release_candidate_with(ctx, "nix-garbage")?
        }
    };
    own_store(1024)?;
    if trusted {
        configure(&["trusted-users = root tester"])?;
    }
    let _daemon = Daemon::start(ctx, &[])?;
    let names = [
        "sys-1", "sys-2", "user-1", "user-2", "direct", "indirect", "temp", "runtime", "dead-1",
        "dead-2", "dead-3",
    ];
    let files: Vec<(&str, usize, u8)> = names
        .iter()
        .enumerate()
        .map(|(i, name)| (*name, 4096 * (i + 1), b'a' + i as u8))
        .collect();
    let paths = add_files(ctx, &files)?;
    let path = |name: &str| paths[names.iter().position(|n| *n == name).expect("named")].clone();
    make_generations(
        ctx,
        [&path("sys-1"), &path("sys-2")],
        [&path("user-1"), &path("user-2")],
    )?;
    root_sh(&format!(
        "ln -s {} {STATE}/gcroots/diskgc-e2e-direct",
        path("direct").display()
    ))?;
    tester(
        ctx,
        &[],
        NIX_STORE,
        &[
            "--realise",
            "--add-root",
            "/home/tester/result",
            &path("indirect").display().to_string(),
        ],
    )?;
    // A temporary root: the input of a build that is running (paused).
    let started = "/tmp/nix-build-paused";
    let expression = format!(
        "derivation {{ name = \"paused\"; system = \"x86_64-linux\"; builder = \"/bin/sh\"; \
         args = [ \"-c\" \"echo $input > {started}; exec /bin/sleep 600\" ]; \
         input = builtins.storePath \"{}\"; }}",
        path("temp").display()
    );
    let mut build = tester_command(ctx, &[], NIX_BUILD, &["--no-out-link", "-E", &expression])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("start the paused build: {e}"))?;
    // A runtime root: a process holding a store file open.
    let mut holder = tester_command(
        ctx,
        &[],
        SH,
        &[
            "-c",
            &format!("exec 3< {}; exec sleep 600", path("runtime").display()),
        ],
    )
    .stdin(Stdio::null())
    .spawn()
    .map_err(|e| format!("start the runtime-root holder: {e}"))?;
    let result = (|| -> TestResult {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !exists(Path::new(started)) {
            if let Some(status) = build.try_wait().map_err(|e| e.to_string())? {
                let mut err = String::new();
                if let Some(mut stderr) = build.stderr.take() {
                    let _ = stderr.read_to_string(&mut err);
                }
                return Err(format!(
                    "the paused build ended {status} before it ran: {err}"
                ));
            }
            ensure!(
                Instant::now() < deadline,
                "the paused build did not start within 60 s"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let held = format!("/proc/{}/fd/3", holder.id());
        let deadline = Instant::now() + Duration::from_secs(20);
        while std::fs::read_link(&held).ok().as_deref() != Some(path("runtime").as_path()) {
            ensure!(
                Instant::now() < deadline,
                "the holder did not open the runtime root"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let before = store_entries()?;
        let dead: Vec<PathBuf> = ["dead-1", "dead-2", "dead-3"].map(path).to_vec();
        let links = protected_links(&["/home/tester/result"])?;
        let mut fixture = nix_state_writers(Fixture::new()).concurrent_writer("/tmp");
        for path in &dead {
            fixture = fixture.eligible(path.clone());
        }
        ensure!(
            build.try_wait().map_err(|e| e.to_string())?.is_none(),
            "the paused build ended before the GC: no temporary root to keep"
        );
        let (min_free, goal) = match build_kind {
            Build::Development => ("100%".to_owned(), None),
            Build::ReleaseCandidate => {
                dry_run_plans_one_gc(ctx, &binary, &dead, &links)?;
                let goal_kib = avail(STORE)? / 1024 + BOUNDED_NEED_KIB;
                (format!("{goal_kib}KiB"), Some(goal_kib * 1024))
            }
        };
        let oracle = Oracle::arm(fixture)?;
        // The need diskgc should compute from its own fresh sample: nothing
        // else writes the store's tmpfs between this one and its.
        let need = goal.map(|goal| avail(STORE).map(|now| goal.saturating_sub(now)));
        let run = run_binary_in(ctx, &binary, Path::new("/"), &[], &ensure_args(&min_free))?;
        let build_ran_through = build.try_wait().map_err(|e| e.to_string())?.is_none();
        oracle.judge(&run)?.assert_clean()?;
        assert_links_kept(&links, &protected_links(&["/home/tester/result"])?)?;
        ensure!(build_ran_through, "the paused build ended during the GC");
        let after = store_entries()?;
        info!(
            "store before ({} paths) minus after ({} paths): {:?}",
            before.len(),
            after.len(),
            before.difference(&after).collect::<Vec<_>>()
        );
        let report = report(&run)?;
        ensure!(
            run.code == Some(1),
            "exit {:?} ({min_free} stays unmet): {report}",
            run.code
        );
        ensure!(
            report["errors"].as_array().is_some_and(Vec::is_empty)
                && report["skipped"].as_array().is_some_and(Vec::is_empty),
            "errors or skips: {report}"
        );
        let action = nix_action(&report)?;
        for path in &dead {
            ensure!(!exists(path), "dead {} survived the GC", path.display());
        }
        for name in [
            "sys-1", "sys-2", "user-1", "user-2", "direct", "indirect", "temp", "runtime",
        ] {
            ensure!(
                exists(&path(name)),
                "{name} root {} was removed",
                path(name).display()
            );
        }
        ensure!(
            run.stderr
                .contains("the daemon counts all 4 generation link(s) below /nix/var/nix/profiles"),
            "no root check line for the 4 generations:\n{}",
            run.stderr
        );
        if let Some(need) = need {
            let need = need?;
            let bound = max_freed(&run.stderr)?;
            ensure!(
                need > 0 && bound.abs_diff(need) <= NEED_SLACK,
                "--max-freed {bound} is not the need {need} measured before the run (within \
                 {NEED_SLACK} bytes)"
            );
            let removed: BTreeSet<String> = before.difference(&after).cloned().collect();
            let dead_names: BTreeSet<String> =
                dead.iter().map(|p| p.display().to_string()).collect();
            ensure!(
                removed == dead_names,
                "the GC removed {removed:?}, not exactly the dead paths {dead_names:?}"
            );
        }
        info!(
            "{build_kind:?}, trusted {trusted}: root check {:?} ms, GC action {} ms, \
             --max-freed {:?}, avail delta {}",
            root_check_ms(&run.stderr),
            action["duration_ms"],
            max_freed(&run.stderr).ok(),
            action["avail_delta_bytes"]
        );
        Ok(())
    })();
    let _ = build.kill();
    let _ = build.wait();
    let _ = holder.kill();
    let _ = holder.wait();
    result
}

/// The release candidate's dry run of the Nix tier (exit 0): one plan row,
/// `/nix/store`, eligible, size unknown; no GC and no root check (which
/// unlinks stale registrations, so a dry run must not run it); every dead
/// path and protected link still there, and nothing deleted (the oracle,
/// with nothing eligible).
fn dry_run_plans_one_gc(ctx: &Ctx, binary: &Path, dead: &[PathBuf], links: &Links) -> TestResult {
    let mut args = ensure_args("100%");
    args.insert(1, "--dry-run".to_owned());
    let oracle = Oracle::arm(nix_state_writers(Fixture::new()).concurrent_writer("/tmp"))?;
    let plan = run_binary_in(ctx, binary, Path::new("/"), &[], &args)?;
    oracle.judge(&plan)?.assert_clean()?;
    let report = report(&plan)?;
    ensure!(
        plan.code == Some(0) && report["status"] == "dry_run",
        "dry run exit {:?}: {report}",
        plan.code
    );
    let rows = report["plan"].as_array().ok_or("no plan array")?;
    ensure!(
        rows.len() == 1
            && rows[0]["candidate"] == STORE
            && rows[0]["eligible"] == true
            && rows[0]["estimated_bytes"].is_null(),
        "expected one eligible /nix/store row of unknown size: {report}"
    );
    ensure!(
        report["actions"].as_array().is_some_and(Vec::is_empty)
            && report["errors"].as_array().is_some_and(Vec::is_empty),
        "dry run acted or failed: {report}"
    );
    ensure!(
        !plan.stderr.contains("Nix GC through") && !plan.stderr.contains("Nix root check:"),
        "the dry run ran a GC or the root check:\n{}",
        plan.stderr
    );
    for path in dead {
        ensure!(exists(path), "the dry run removed {}", path.display());
    }
    assert_links_kept(links, &protected_links(&["/home/tester/result"])?)
}

/// docs/testing.md `nix_gc_refuses_a_daemon_with_another_root_database`:
/// the daemon restarted on another state directory over the same store
/// (its database copied from A, so it registers every path valid, but its
/// roots are its own): the root check finds the system's generations not
/// counted (`unsupported_backend`), and a path rooted only in A survives.
pub fn nix_gc_refuses_a_daemon_with_another_root_database(ctx: &Ctx) -> TestResult {
    own_store(1024)?;
    let mut daemon_a = Daemon::start(ctx, &[])?;
    let paths = add_files(
        ctx,
        &[
            ("sys-1", 4096, b's'),
            ("sys-2", 8192, b't'),
            ("rooted-in-a", 4096, b'r'),
        ],
    )?;
    make_generations(ctx, [&paths[0], &paths[1]], [&paths[0], &paths[1]])?;
    root_sh(&format!(
        "ln -s {} {STATE}/gcroots/diskgc-e2e-rooted-in-a",
        paths[2].display()
    ))?;
    daemon_a.stop()?;
    drop(daemon_a);
    root_sh(&format!(
        "mkdir -p /nix/var/nix-b && {NIX_STORE} --dump-db | {NIX_STORE} --store \
         'local?state=/nix/var/nix-b' --load-db"
    ))?;
    let _daemon_b = Daemon::start(ctx, &["--store", "local?state=/nix/var/nix-b"])?;
    let roots_in_b = tester(
        ctx,
        &[],
        NIX_STORE,
        &["--query", "--roots", &paths[2].display().to_string()],
    )?;
    let valid_in_b = tester(
        ctx,
        &[],
        NIX_STORE,
        &["--query", "--hash", &paths[2].display().to_string()],
    )?;
    info!(
        "daemon B: rooted-in-a valid ({}), its roots there: {roots_in_b:?}",
        valid_in_b.trim()
    );
    let oracle =
        Oracle::arm(nix_state_writers(Fixture::new()).concurrent_writer("/nix/var/nix-b"))?;
    let run = ensure_nix(ctx, &[], "100%")?;
    oracle.judge(&run)?.assert_clean()?;
    let report = report(&run)?;
    ensure!(run.code == Some(1), "exit {:?}: {report}", run.code);
    let (reason, candidate) = one_skip(&report)?;
    ensure!(
        reason == "unsupported_backend" && candidate == STORE,
        "skip {reason} {candidate}, expected unsupported_backend /nix/store: {report}"
    );
    ensure!(
        run.stderr.contains("does not count generation"),
        "the skip is not the root check's:\n{}",
        run.stderr
    );
    ensure!(
        report["actions"].as_array().is_some_and(Vec::is_empty),
        "a GC ran: {report}"
    );
    ensure!(exists(&paths[2]), "the path rooted only in A was collected");
    Ok(())
}

/// docs/testing.md `nix_gc_refuses_unlisted_versions`: root's default
/// profile (the client anchor) names a second pinned CppNix (this flake's
/// nixpkgs, not 2.34.8): `unsupported_backend`, no GC. Lix and other
/// variants are refused by the same allowlist; they are covered by the
/// model vector `nix-lix-client-refused` only (Mark, 2026-11-04).
pub fn nix_gc_refuses_unlisted_versions(ctx: &Ctx) -> TestResult {
    own_store(1024)?;
    let _daemon = Daemon::start(ctx, &[])?;
    let dead = add_files(ctx, &[("dead", 4096, b'd')])?;
    let mut clients = std::fs::read_dir(OTHER_CLIENTS)
        .map_err(|e| format!("list {OTHER_CLIENTS}: {e}"))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect::<Vec<_>>();
    clients.sort();
    ensure!(clients.len() == 1, "expected one other client: {clients:?}");
    let other = std::fs::canonicalize(&clients[0]).map_err(|e| e.to_string())?;
    let version = tester(
        ctx,
        &[],
        &other.join("bin/nix-store").display().to_string(),
        &["--version"],
    )?;
    ensure!(
        version.starts_with("nix-store (Nix) ") && version.trim_end() != "nix-store (Nix) 2.34.8",
        "the other client is {version:?}"
    );
    set_default_profile(&other)?;
    let oracle = Oracle::arm(nix_state_writers(Fixture::new()))?;
    let run = ensure_nix(ctx, &[], "100%")?;
    oracle.judge(&run)?.assert_clean()?;
    assert_tier_skipped(&run, "unsupported_backend")?;
    let version = version.trim_end().trim_start_matches("nix-store (Nix) ");
    ensure!(
        run.stderr.contains(version),
        "the skip does not name client {version}:\n{}",
        run.stderr
    );
    ensure!(exists(&dead[0]), "a dead path went without a GC");
    Ok(())
}

/// Point root's default profile, the client anchor of this image, at
/// `store_path` (as root, as an administrator's install would).
fn set_default_profile(store_path: &Path) -> Result<(), String> {
    root_sh(&format!(
        "ln -sfn {} {PROFILES}/default",
        store_path.display()
    ))?;
    Ok(())
}

/// TASK-36 review (Codex 1): a wrapper is never the client. (1) A
/// `nix-store` wrapper first on diskgc's `PATH`, which would mark that it
/// ran: the client is the installation's (the anchor), the GC runs, the
/// wrapper never does. (2) Root's default profile names a store path laid
/// out like a Nix 2.34.8 (`<hash>-…-nix-2.34.8/bin/nix` plus `nix-store ->
/// nix`) whose `nix` is a script: refused `unsupported_backend` (not an
/// ELF), and it never runs either.
pub fn nix_gc_never_runs_a_wrapper_client(ctx: &Ctx) -> TestResult {
    own_store(1024)?;
    let _daemon = Daemon::start(ctx, &[])?;
    let rooted = add_files(ctx, &[("sys-1", 4096, b's'), ("sys-2", 8192, b't')])?;
    make_generations(ctx, [&rooted[0], &rooted[1]], [&rooted[0], &rooted[1]])?;
    let marker = Path::new("/tmp/wrapper-ran");
    let script = format!(
        "#!/bin/sh\n: > {}\nexec /bin/nix-store \"$@\"\n",
        marker.display()
    );
    ctx.scratch.mkdirs("path-wrapper", 0o755, crate::TESTER)?;
    ctx.scratch.write_file(
        "path-wrapper/nix-store",
        script.as_bytes(),
        0o755,
        crate::TESTER,
    )?;
    let dead = add_files(ctx, &[("dead", 4096, b'd')])?;
    let links = protected_links(&[])?;
    let oracle = Oracle::arm(nix_state_writers(Fixture::new().eligible(dead[0].clone())))?;
    let path = "/tmp/path-wrapper:/bin";
    let first = ensure_nix(ctx, &[("PATH", path)], "100%")?;
    oracle.judge(&first)?.assert_clean()?;
    let report1 = report(&first)?;
    nix_action(&report1)?;
    ensure!(
        !exists(&dead[0]),
        "(1) the GC did not run with the installation's client"
    );
    ensure!(!exists(marker), "(1) the PATH wrapper ran");
    // (2) A wrapper store path, added by root, named like Nix 2.34.8
    // (`nix-store --add` names it after its source directory).
    root_sh(&format!(
        "mkdir -p /root/wrapper-nix-2.34.8/bin && printf '%s' '{script}' > \
         /root/wrapper-nix-2.34.8/bin/nix && chmod 0555 /root/wrapper-nix-2.34.8/bin/nix && \
         ln -s nix /root/wrapper-nix-2.34.8/bin/nix-store"
    ))?;
    let wrapped =
        PathBuf::from(root_sh(&format!("{NIX_STORE} --add /root/wrapper-nix-2.34.8"))?.trim());
    ensure!(
        wrapped.to_string_lossy().ends_with("-wrapper-nix-2.34.8"),
        "the wrapper store path is {}",
        wrapped.display()
    );
    root_sh(&format!(
        "ln -s {} {STATE}/gcroots/diskgc-e2e-wrapper",
        wrapped.display()
    ))?;
    set_default_profile(&wrapped)?;
    let oracle = Oracle::arm(nix_state_writers(Fixture::new()))?;
    let second = ensure_nix(ctx, &[("PATH", path)], "100%")?;
    oracle.judge(&second)?.assert_clean()?;
    assert_tier_skipped(&second, "unsupported_backend")?;
    ensure!(
        second.stderr.contains("not an ELF executable"),
        "(2) not refused as a script:\n{}",
        second.stderr
    );
    ensure!(!exists(marker), "(2) the wrapper client ran");
    let mut after = protected_links(&[])?;
    after.remove(&PathBuf::from(format!(
        "{STATE}/gcroots/diskgc-e2e-wrapper"
    )));
    let mut before = links;
    before.remove(&PathBuf::from(format!("{PROFILES}/default")));
    after.remove(&PathBuf::from(format!("{PROFILES}/default")));
    assert_links_kept(&before, &after)
}

/// TASK-36 review (Codex 2): no configured Nix plugin runs in a diskgc
/// child. The marker plugin (flake.nix: its constructor creates
/// /tmp/nix-plugin-loaded) loads into the caller's own `nix config show
/// store` from `NIX_CONFIG` (the control); diskgc's dry run and reclaim,
/// with the same `NIX_CONFIG` and then with a user `nix.conf` naming it,
/// never load it.
pub fn nix_gc_never_loads_a_configured_plugin(ctx: &Ctx) -> TestResult {
    own_store(1024)?;
    let _daemon = Daemon::start(ctx, &[])?;
    let rooted = add_files(ctx, &[("sys-1", 4096, b's'), ("sys-2", 8192, b't')])?;
    make_generations(ctx, [&rooted[0], &rooted[1]], [&rooted[0], &rooted[1]])?;
    let plugin = std::fs::canonicalize(PLUGIN).map_err(|e| format!("{PLUGIN}: {e}"))?;
    let config = format!("plugin-files = {}", plugin.display());
    let marker = Path::new("/tmp/nix-plugin-loaded");
    tester(
        ctx,
        &[("NIX_CONFIG", &config)],
        NIX,
        &[
            "--extra-experimental-features",
            "nix-command",
            "config",
            "show",
            "store",
        ],
    )?;
    ensure!(
        exists(marker),
        "inconclusive: the marker plugin did not load into a plain `nix config show store`"
    );
    ctx.scratch.remove_file("nix-plugin-loaded")?;
    let dead = add_files(ctx, &[("dead", 4096, b'd')])?;
    let oracle = Oracle::arm(nix_state_writers(
        Fixture::new()
            .eligible(dead[0].clone())
            .concurrent_writer("/home/tester"),
    ))?;
    let mut runs = Vec::new();
    let mut dry = ensure_args("100%");
    dry.insert(1, "--dry-run".to_owned());
    runs.push(ensure_nix_in(
        ctx,
        Path::new("/"),
        &[("NIX_CONFIG", &config)],
        &dry,
    )?);
    ensure!(!exists(marker), "the plugin loaded in diskgc's dry run");
    ctx.home.mkdirs(".config/nix", 0o755, crate::TESTER)?;
    ctx.home.write_file(
        ".config/nix/nix.conf",
        format!("{config}\n").as_bytes(),
        0o644,
        crate::TESTER,
    )?;
    runs.push(ensure_nix(ctx, &[("NIX_CONFIG", &config)], "100%")?);
    oracle
        .judge_runs(&runs.iter().collect::<Vec<_>>())?
        .assert_clean()?;
    nix_action(&report(&runs[1])?)?;
    ensure!(!exists(&dead[0]), "the reclaim did not collect");
    ensure!(!exists(marker), "the plugin loaded in diskgc's reclaim");
    Ok(())
}
/// docs/testing.md `nix_gc_refuses_single_user_layout`: no daemon, the
/// store owned by uid 1000 without the sticky bit: `unsupported_backend`.
pub fn nix_gc_refuses_single_user_layout(ctx: &Ctx) -> TestResult {
    own_store(1024)?;
    root_sh(&format!("chown 1000:1000 {STORE} && chmod 0755 {STORE}"))?;
    let oracle = Oracle::arm(Fixture::new())?;
    let run = ensure_nix(ctx, &[], "100%")?;
    oracle.judge(&run)?.assert_clean()?;
    assert_tier_skipped(&run, "unsupported_backend")?;
    ensure!(
        run.stderr.contains("single-user"),
        "the skip is not the layout's:\n{}",
        run.stderr
    );
    Ok(())
}

/// docs/testing.md `nix_gc_refuses_redirected_store`: the caller's Nix
/// selects another store: `NIX_REMOTE=local?state=...`, a user `nix.conf`
/// `store` setting (both `unsupported_backend`) and `NIX_REMOTE=ssh-ng://x`
/// (`remote_backend`); nothing is deleted in either state.
pub fn nix_gc_refuses_redirected_store(ctx: &Ctx) -> TestResult {
    own_store(1024)?;
    let _daemon = Daemon::start(ctx, &[])?;
    let dead = add_files(ctx, &[("dead", 4096, b'd')])?;
    ctx.home.mkdirs(".config/nix", 0o755, crate::TESTER)?;
    ctx.home.mkdirs("project", 0o755, crate::TESTER)?;
    ctx.home.write_file(
        "project/nix.conf",
        b"store = ssh-ng://relative\n",
        0o644,
        crate::TESTER,
    )?;
    let oracle = Oracle::arm(nix_state_writers(
        Fixture::new().concurrent_writer("/home/tester"),
    ))?;
    let mut runs = Vec::new();
    let local = ensure_nix(
        ctx,
        &[("NIX_REMOTE", "local?state=/tmp/nix-state-b")],
        "100%",
    )?;
    assert_tier_skipped(&local, "unsupported_backend")?;
    runs.push(local);
    ctx.home.write_file(
        ".config/nix/nix.conf",
        b"store = local?root=/tmp/nix-chroot\n",
        0o644,
        crate::TESTER,
    )?;
    let configured = ensure_nix(ctx, &[], "100%")?;
    assert_tier_skipped(&configured, "unsupported_backend")?;
    ensure!(
        configured.stderr.contains("local://?root=/tmp/nix-chroot")
            || configured.stderr.contains("local?root=/tmp/nix-chroot"),
        "the skip does not name the configured store:\n{}",
        configured.stderr
    );
    runs.push(configured);
    ctx.home.remove_file(".config/nix/nix.conf")?;
    let remote = ensure_nix(ctx, &[("NIX_REMOTE", "ssh-ng://x")], "100%")?;
    assert_tier_skipped(&remote, "remote_backend")?;
    runs.push(remote);
    // TASK-36 review (Codex 3): a relative NIX_USER_CONF_FILES names a
    // file by diskgc's own working directory, which the query keeps.
    let relative = ensure_nix_in(
        ctx,
        Path::new("/home/tester/project"),
        &[("NIX_USER_CONF_FILES", "nix.conf")],
        &ensure_args("100%"),
    )?;
    assert_tier_skipped(&relative, "remote_backend")?;
    ensure!(
        relative.stderr.contains("ssh-ng://relative"),
        "the skip does not name the relatively configured store:\n{}",
        relative.stderr
    );
    runs.push(relative);
    oracle
        .judge_runs(&runs.iter().collect::<Vec<_>>())?
        .assert_clean()?;
    ensure!(exists(&dead[0]), "a dead path went without a GC");
    ensure!(
        !exists(Path::new("/tmp/nix-state-b")) && !exists(Path::new("/tmp/nix-chroot")),
        "a query opened the selected local store"
    );
    Ok(())
}

/// docs/testing.md `nix_gc_daemon_down_is_a_skip`: the daemon stopped
/// with its socket kept (refused), a socket-activated daemon that fails (a
/// listener that closes every connection at once: end of file before the
/// magic answer), and a daemon whose `allowed-users` excludes diskgc's
/// user: each `backend_unavailable`, exit 1 with no other tier.
pub fn nix_gc_daemon_down_is_a_skip(ctx: &Ctx) -> TestResult {
    own_store(1024)?;
    let oracle = Oracle::arm(nix_state_writers(Fixture::new()))?;
    let mut runs = Vec::new();
    {
        let mut daemon = Daemon::start(ctx, &[])?;
        daemon.stop()?;
        ensure!(
            exists(Path::new(SOCKET)),
            "the stopped daemon's socket is gone"
        );
        let run = ensure_nix(ctx, &[], "100%")?;
        assert_tier_skipped(&run, "backend_unavailable")?;
        ensure!(
            run.stderr.contains("ECONNREFUSED") || run.stderr.contains("refused"),
            "not the refused connection:\n{}",
            run.stderr
        );
        runs.push(run);
    }
    {
        root_sh(&format!("rm -f {SOCKET}"))?;
        let listener =
            UnixListener::bind(SOCKET).map_err(|e| format!("bind the failing listener: {e}"))?;
        root_sh(&format!("chmod 0666 {SOCKET}"))?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopping = stop.clone();
        let failing = std::thread::spawn(move || {
            let mut accepted = 0;
            while !stopping.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((connection, _)) => {
                        accepted += 1;
                        drop(connection);
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
            accepted
        });
        let run = ensure_nix(ctx, &[], "100%");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let accepted = failing.join().map_err(|_| "listener thread panicked")?;
        let run = run?;
        assert_tier_skipped(&run, "backend_unavailable")?;
        ensure!(accepted >= 1, "the failing listener was never connected to");
        runs.push(run);
        root_sh(&format!("rm -f {SOCKET}"))?;
    }
    {
        configure(&["allowed-users = root"])?;
        let _daemon = Daemon::start(ctx, &[])?;
        let run = ensure_nix(ctx, &[], "100%")?;
        assert_tier_skipped(&run, "backend_unavailable")?;
        ensure!(
            run.stderr.contains("closed the connection"),
            "not the end of file before the answer:\n{}",
            run.stderr
        );
        runs.push(run);
    }
    oracle
        .judge_runs(&runs.iter().collect::<Vec<_>>())?
        .assert_clean()
}

/// How a GC is interrupted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Interrupt {
    Term,
    Kill,
}

/// docs/testing.md `nix_gc_sigterm_mid_gc`: 10,000 small dead paths;
/// SIGTERM to diskgc once the GC is deleting: exit 143, the host lock held
/// until the client exits, every rooted path present, and a second,
/// uninterrupted GC completes.
pub fn nix_gc_sigterm_mid_gc(ctx: &Ctx) -> TestResult {
    interrupted_gc(ctx, Interrupt::Term)
}

/// The same with SIGKILL to diskgc (TASK-36 AC2): the GC client it started
/// keeps the host lock it inherited until it exits.
pub fn nix_gc_sigkill_leaves_the_host_lock_with_the_client(ctx: &Ctx) -> TestResult {
    interrupted_gc(ctx, Interrupt::Kill)
}

fn interrupted_gc(ctx: &Ctx, interrupt: Interrupt) -> TestResult {
    own_store(2048)?;
    let _daemon = Daemon::start(ctx, &[])?;
    let rooted = add_files(ctx, &[("sys-1", 4096, b's'), ("sys-2", 8192, b't')])?;
    make_generations(ctx, [&rooted[0], &rooted[1]], [&rooted[0], &rooted[1]])?;
    let names: Vec<String> = (0..10_000).map(|i| format!("dead-{i:05}")).collect();
    let files: Vec<(&str, usize, u8)> = names.iter().map(|n| (n.as_str(), 64, b'x')).collect();
    // Distinct contents, so 10,000 distinct paths.
    let mut dead = Vec::new();
    for chunk in files.chunks(2000) {
        let distinct: Vec<(String, usize)> = chunk
            .iter()
            .map(|(name, size, _)| ((*name).to_owned(), *size))
            .collect();
        dead.extend(add_distinct(ctx, &distinct)?);
    }
    let initial = store_entries()?.len();
    let links = protected_links(&[])?;
    let mut fixture = nix_state_writers(Fixture::new());
    for path in &dead {
        fixture = fixture.eligible(path.clone());
    }
    let oracle = Oracle::arm(fixture)?;
    let mut spawned = under_test::spawn_as_tester_with_env(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        &[],
        &ctx.diskgc(),
        &ensure_args("100%"),
    )?;
    let deadline = Instant::now() + Duration::from_secs(120);
    let client = loop {
        if let Some(pid) = tester_processes_with(&["--gc", "--max-freed"]).first() {
            break *pid;
        }
        ensure!(
            !spawned.finished()?,
            "diskgc ended before its GC client was seen"
        );
        ensure!(Instant::now() < deadline, "no GC client within 120 s");
        std::thread::sleep(Duration::from_millis(5));
    };
    // SIGTERM once it deletes (the store shrinks); SIGKILL at once, while
    // the client waits for the daemon's root discovery and writes nothing
    // (a client that outlives diskgc ends at its next line on the closed
    // stderr pipe).
    if interrupt == Interrupt::Term {
        let deadline = Instant::now() + Duration::from_secs(60);
        while store_entries()?.len() >= initial {
            ensure!(
                alive(client),
                "the GC client ended before it deleted a path"
            );
            ensure!(
                Instant::now() < deadline,
                "the GC deleted nothing within 60 s"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    let diskgc = tester_processes_with(&["ensure", "profile:nix-garbage"])
        .into_iter()
        .find(|pid| {
            std::fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|exe| exe == ctx.diskgc())
        })
        .ok_or("no diskgc process to interrupt")?;
    let signal = match interrupt {
        Interrupt::Term => rustix::process::Signal::TERM,
        Interrupt::Kill => rustix::process::Signal::KILL,
    };
    let at = Instant::now();
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(diskgc.cast_signed()).ok_or("pid")?,
        signal,
    )
    .map_err(|e| format!("signal diskgc {diskgc}: {e}"))?;
    // While the client lives, the host lock is held (it inherited it).
    let mut held_while_client = 0;
    let mut free_while_client = 0;
    while alive(client) {
        if flock_held(Path::new(HOST_LOCK))? {
            held_while_client += 1;
        } else if alive(client) {
            free_while_client += 1;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let client_lived = at.elapsed();
    let run = spawned.wait()?;
    info!(
        "{interrupt:?}: client lived {client_lived:?} after the signal; lock held in \
         {held_while_client} samples, free in {free_while_client}\n--- stdout\n{}--- stderr\n{}",
        run.stdout, run.stderr
    );
    ensure!(
        free_while_client == 0,
        "the host lock was free while the GC client still ran"
    );
    ensure!(
        flock_free_within_release_bound(Path::new(HOST_LOCK))?,
        "the host lock is still held {RELEASE_BOUND:?} after the client exited"
    );
    match interrupt {
        Interrupt::Term => {
            ensure!(
                run.code == Some(143),
                "SIGTERM: exit {:?}, expected 143",
                run.code
            );
            let report = report(&run)?;
            ensure!(
                report["status"] == "interrupted",
                "status {}",
                report["status"]
            );
            nix_action(&report)?;
        }
        Interrupt::Kill => {
            ensure!(
                run.signal == Some(9),
                "diskgc did not die by SIGKILL: {run:?}"
            );
            ensure!(
                held_while_client > 0,
                "inconclusive: the client ended before the lock could be observed held"
            );
        }
    }
    for path in &rooted {
        ensure!(exists(path), "rooted {} was removed", path.display());
    }
    let left = dead.iter().filter(|p| exists(p)).count();
    info!(
        "{left} of {} dead paths left after the interrupted GC",
        dead.len()
    );
    let second = ensure_nix(ctx, &[], "100%")?;
    oracle.judge_runs(&[&run, &second])?.assert_clean()?;
    assert_links_kept(&links, &protected_links(&[])?)?;
    let report = report(&second)?;
    ensure!(
        second.code == Some(1),
        "second GC: exit {:?}: {report}",
        second.code
    );
    nix_action(&report)?;
    let left = dead.iter().filter(|p| exists(p)).count();
    ensure!(
        left == 0,
        "{left} dead paths left after the uninterrupted GC"
    );
    for path in &rooted {
        ensure!(exists(path), "rooted {} was removed", path.display());
    }
    Ok(())
}

/// docs/testing.md `nix_gc_on_a_slow_stderr_completes_and_counts_unforwarded_output`
/// (TASK-342; dogfood 2026-11-04, where one slow `deleting` line failed a
/// real GC with exit 3): 10,000 dead paths, so `nix-store --gc` writes
/// 10,000 `deleting` lines while diskgc's stderr is a pipe read 4 KiB every
/// 250 ms ([`SlowStderr`]). The GC runs to its end: every dead path gone,
/// every root kept, the action error-free (exit 1, `exhausted`: nothing
/// meets `100%`), and every `deleting` line forwarded or counted in the
/// action's log line as not forwarded.
pub fn nix_gc_on_a_slow_stderr_completes_and_counts_unforwarded_output(ctx: &Ctx) -> TestResult {
    own_store(2048)?;
    let _daemon = Daemon::start(ctx, &[])?;
    let rooted = add_files(ctx, &[("sys-1", 4096, b's'), ("sys-2", 8192, b't')])?;
    make_generations(ctx, [&rooted[0], &rooted[1]], [&rooted[0], &rooted[1]])?;
    let names: Vec<String> = (0..10_000).map(|i| format!("chatty-{i:05}")).collect();
    let mut dead = Vec::new();
    for chunk in names.chunks(2000) {
        let distinct: Vec<(String, usize)> = chunk.iter().map(|name| (name.clone(), 64)).collect();
        dead.extend(add_distinct(ctx, &distinct)?);
    }
    let links = protected_links(&[])?;
    let mut fixture = nix_state_writers(Fixture::new());
    for path in &dead {
        fixture = fixture.eligible(path.clone());
    }
    let oracle = Oracle::arm(fixture)?;
    let (slow, stderr) = SlowStderr::start()?;
    let started = Instant::now();
    let spawned = under_test::spawn_as_tester_with_stderr(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        &ctx.diskgc(),
        &ensure_args("100%"),
        stderr,
    )?;
    let run = spawned.wait()?;
    let took = started.elapsed();
    let stderr = slow.finish()?;
    let deleting = stderr
        .lines()
        .filter(|line| line.contains(": deleting '/nix/store/"))
        .count();
    let not_forwarded = not_forwarded_lines(&stderr)?;
    info!(
        "GC on a slow stderr: exit {:?} in {took:?}; {deleting} deleting lines forwarded, \
         {not_forwarded} lines counted as not forwarded\n--- stdout\n{}--- stderr (without \
         deleting lines)\n{}",
        run.code,
        run.stdout,
        stderr
            .lines()
            .filter(|line| !line.contains(": deleting '/nix/store/"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    oracle.judge(&run)?.assert_clean()?;
    assert_links_kept(&links, &protected_links(&[])?)?;
    let report = report(&run)?;
    ensure!(
        run.code == Some(1)
            && report["status"] == "exhausted"
            && report["errors"].as_array().is_some_and(Vec::is_empty),
        "a slow stderr changed the GC's outcome: exit {:?}: {report}",
        run.code
    );
    nix_action(&report)?;
    let left = dead.iter().filter(|p| exists(p)).count();
    ensure!(left == 0, "{left} dead paths left after the GC");
    for path in &rooted {
        ensure!(exists(path), "rooted {} was removed", path.display());
    }
    // The GC takes seconds; forwarding 10,000 lines at 16 KiB/s would take
    // a minute, so the drop path must have run.
    ensure!(
        not_forwarded > 0,
        "the slow pipe took every line, so nothing tested the drop path"
    );
    // The count covers every `deleting` line not seen, and exceeds them by
    // at most Nix's few other lines (roots, links, the summary) and the
    // segment being written at the end.
    let accounted = u64::try_from(deleting).map_err(|e| e.to_string())? + not_forwarded;
    let dead_paths = u64::try_from(dead.len()).map_err(|e| e.to_string())?;
    ensure!(
        (dead_paths..=dead_paths + NIX_GC_OTHER_LINES).contains(&accounted),
        "{deleting} deleting lines forwarded + {not_forwarded} counted as not forwarded is not \
         within {NIX_GC_OTHER_LINES} lines above the {dead_paths} dead paths"
    );
    Ok(())
}

/// An allowance for the lines `nix-store --gc` writes besides one
/// `deleting` line per path (finding roots, deleting unused links, the
/// summary, an occasional warning), plus the one segment the not-forwarded
/// count may include twice.
const NIX_GC_OTHER_LINES: u64 = 20;

/// Files of distinct contents (their names repeated to `size`), added.
fn add_distinct(ctx: &Ctx, files: &[(String, usize)]) -> Result<Vec<PathBuf>, String> {
    let rel = SOURCES.trim_start_matches("/tmp/");
    if !Path::new(SOURCES).exists() {
        ctx.scratch.mkdir(rel, 0o755, crate::TESTER)?;
    }
    let mut args = vec!["--add".to_owned()];
    for (name, size) in files {
        let body: Vec<u8> = name.bytes().cycle().take(*size).collect();
        ctx.scratch
            .write_file(&format!("{rel}/{name}"), &body, 0o644, crate::TESTER)?;
        args.push(format!("{SOURCES}/{name}"));
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let added: Vec<PathBuf> = tester(ctx, &[], NIX_STORE, &args)?
        .lines()
        .map(PathBuf::from)
        .collect();
    ensure!(
        added.len() == files.len(),
        "added {} of {}",
        added.len(),
        files.len()
    );
    Ok(added)
}

/// docs/testing.md `nix_gc_overshoot_is_bounded_by_the_last_path`: one
/// 64 MiB dead path, need 1 MiB: Nix deletes the whole path, at least
/// 64 MiB freed, one GC, `met`.
pub fn nix_gc_overshoot_is_bounded_by_the_last_path(ctx: &Ctx) -> TestResult {
    own_store(1024)?;
    let _daemon = Daemon::start(ctx, &[])?;
    let rooted = add_files(ctx, &[("sys-1", 4096, b's'), ("sys-2", 8192, b't')])?;
    make_generations(ctx, [&rooted[0], &rooted[1]], [&rooted[0], &rooted[1]])?;
    let big = add_files(ctx, &[("big", 64 << 20, b'b')])?;
    let oracle = Oracle::arm(
        Fixture::new()
            .eligible(big[0].clone())
            .concurrent_writer(format!("{STATE}/db"))
            .concurrent_writer(format!("{STATE}/temproots"))
            .concurrent_writer(format!("{STATE}/gc.lock"))
            .concurrent_writer(format!("{STATE}/gc-socket")),
    )?;
    let before = avail(STORE)?;
    let min_free = format!("{}KiB", before / 1024 + 1024);
    // `--timeout 0`: no deadline (TASK-36 review, mped B1: the root check
    // once overflowed computing one).
    let mut args = ensure_args(&min_free);
    args.splice(1..1, ["--timeout".to_owned(), "0".to_owned()]);
    let run = ensure_nix_in(ctx, Path::new("/"), &[], &args)?;
    oracle.judge(&run)?.assert_clean()?;
    let report = report(&run)?;
    ensure!(run.code == Some(0), "exit {:?}: {report}", run.code);
    ensure!(report["status"] == "met", "status {}", report["status"]);
    let action = nix_action(&report)?;
    let delta = action["avail_delta_bytes"].as_i64().ok_or("no delta")?;
    let bound = max_freed(&run.stderr)?;
    info!("need {bound} bytes, freed {delta} bytes, avail before {before}");
    ensure!(
        (1..64 << 20).contains(&bound),
        "--max-freed {bound} is not the small need"
    );
    ensure!(
        delta >= 64 << 20,
        "freed {delta} bytes, less than the 64 MiB path"
    );
    ensure!(!exists(&big[0]), "the 64 MiB dead path survived");
    Ok(())
}

/// docs/testing.md `nix_gc_optimised_store_frees_at_the_end`:
/// `auto-optimise-store = true`, two dead paths sharing a 16 MiB file
/// through `.links`: the space appears with the links phase. Measured and
/// reported, not asserted against a timing.
pub fn nix_gc_optimised_store_frees_at_the_end(ctx: &Ctx) -> TestResult {
    own_store(1024)?;
    configure(&["auto-optimise-store = true"])?;
    let _daemon = Daemon::start(ctx, &[])?;
    let rooted = add_files(ctx, &[("sys-1", 4096, b's'), ("sys-2", 8192, b't')])?;
    make_generations(ctx, [&rooted[0], &rooted[1]], [&rooted[0], &rooted[1]])?;
    let rel = SOURCES.trim_start_matches("/tmp/");
    for dir in ["shared-a", "shared-b"] {
        ctx.scratch
            .mkdir(&format!("{rel}/{dir}"), 0o755, crate::TESTER)?;
        ctx.scratch.write_file(
            &format!("{rel}/{dir}/blob"),
            &vec![b'z'; 16 << 20],
            0o644,
            crate::TESTER,
        )?;
        ctx.scratch.write_file(
            &format!("{rel}/{dir}/own"),
            dir.as_bytes(),
            0o644,
            crate::TESTER,
        )?;
    }
    let dead: Vec<PathBuf> = tester(
        ctx,
        &[],
        NIX_STORE,
        &[
            "--add",
            &format!("{SOURCES}/shared-a"),
            &format!("{SOURCES}/shared-b"),
        ],
    )?
    .lines()
    .map(PathBuf::from)
    .collect();
    ensure!(dead.len() == 2, "added {dead:?}");
    let links = || std::fs::read_dir(format!("{STORE}/.links")).map_or(0, Iterator::count);
    let nlink = std::fs::metadata(dead[0].join("blob"))
        .map_err(|e| e.to_string())?
        .nlink();
    info!(
        "optimised: blob nlink {nlink}, {} entries in .links, avail {}",
        links(),
        avail(STORE)?
    );
    ensure!(
        nlink >= 3,
        "the shared blob is not linked through .links (nlink {nlink})"
    );
    let mut fixture =
        nix_state_writers(Fixture::new()).concurrent_writer(format!("{STORE}/.links"));
    for path in &dead {
        fixture = fixture.eligible(path.clone());
    }
    let oracle = Oracle::arm(fixture)?;
    let before = avail(STORE)?;
    let run = ensure_nix(ctx, &[], "100%")?;
    oracle.judge(&run)?.assert_clean()?;
    let after = avail(STORE)?;
    let report = report(&run)?;
    let action = nix_action(&report)?;
    info!(
        "optimised store: avail {before} -> {after} ({} bytes), action delta {}, {} entries in \
         .links after; GC stderr:\n{}",
        after.saturating_sub(before),
        action["avail_delta_bytes"],
        links(),
        run.stderr
    );
    for path in &dead {
        ensure!(!exists(path), "dead {} survived", path.display());
    }
    ensure!(
        after.saturating_sub(before) >= 16 << 20,
        "the shared 16 MiB was not freed by the end of the GC"
    );
    Ok(())
}

/// docs/testing.md `nix_gc_pinned_socket_survives_replacement`: container
/// root replaces the socket pathname with a decoy listener (1) after the
/// preflight: the recheck skips `changed`; (2) after the recheck, while the
/// root check runs: the GC still reaches the original daemon through the
/// pinned descriptor. The decoy receives no connection either time.
pub fn nix_gc_pinned_socket_survives_replacement(ctx: &Ctx) -> TestResult {
    own_store(1024)?;
    let mut daemon = Daemon::start(ctx, &[])?;
    let rooted = add_files(ctx, &[("sys-1", 4096, b's'), ("sys-2", 8192, b't')])?;
    make_generations(ctx, [&rooted[0], &rooted[1]], [&rooted[0], &rooted[1]])?;
    // (1) After the preflight: two Nix tiers; the preflight of both
    // precedes the first's GC (10,000 dead paths, seconds); the socket is
    // replaced while that GC deletes (its client connected before), so
    // the second tier's recheck finds another inode.
    let names: Vec<(String, usize)> = (0..10_000).map(|i| (format!("first-{i:05}"), 64)).collect();
    let mut first_dead = Vec::new();
    for chunk in names.chunks(2000) {
        first_dead.extend(add_distinct(ctx, chunk)?);
    }
    let config = ctx.home.write_file(
        "diskgc.yaml",
        b"version: 1\ngroups:\n  - name: g\n    tiers:\n      - {name: first, select: profile:nix-garbage}\n      - {name: second, select: profile:nix-garbage}\n",
        0o644,
        crate::TESTER,
    )?;
    let mut fixture =
        nix_state_writers(Fixture::new()).concurrent_writer("/nix/var/nix/daemon-socket");
    for path in &first_dead {
        fixture = fixture.eligible(path.clone());
    }
    let initial = store_entries()?.len();
    let oracle = Oracle::arm(fixture)?;
    let mut spawned = under_test::spawn_as_tester_with_env(
        &ctx.ledger,
        &ctx.records,
        Path::new("/"),
        &[],
        &ctx.diskgc(),
        &[
            "ensure".to_owned(),
            "--json".to_owned(),
            "--no-index".to_owned(),
            "--config".to_owned(),
            config.display().to_string(),
            "--min-free".to_owned(),
            "100%".to_owned(),
            STORE.to_owned(),
        ],
    )?;
    let deadline = Instant::now() + Duration::from_secs(120);
    while store_entries()?.len() >= initial {
        ensure!(
            !spawned.finished()?,
            "diskgc ended before the first tier's GC deleted"
        );
        ensure!(
            Instant::now() < deadline,
            "the first tier's GC deleted nothing within 120 s"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let original = rustix::fs::open(
        SOCKET,
        rustix::fs::OFlags::PATH | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|e| format!("pin the original socket: {e}"))?;
    let decoy = Decoy::replace()?;
    let gc_running_at_swap = !tester_processes_with(&["--gc", "--max-freed"]).is_empty();
    let first = spawned.wait()?;
    let decoy_contacts = decoy.restore(&original)?;
    info!(
        "(1) replaced during the first tier's GC (still running at the swap: \
         {gc_running_at_swap}): exit {:?}\n--- stdout\n{}--- stderr\n{}",
        first.code, first.stdout, first.stderr
    );
    ensure!(
        gc_running_at_swap,
        "inconclusive: the first GC had ended before the swap"
    );
    let report1 = report(&first)?;
    let actions = report1["actions"].as_array().ok_or("no actions")?;
    ensure!(
        actions.len() == 1 && actions[0]["tier"] == "first",
        "(1) expected the first tier's GC only: {report1}"
    );
    let (reason, candidate) = one_skip(&report1)?;
    ensure!(
        reason == "changed" && candidate == STORE,
        "(1) second tier: skip {reason} {candidate}, expected changed /nix/store: {report1}"
    );
    ensure!(
        decoy_contacts == 0,
        "(1) the decoy was connected to {decoy_contacts} times"
    );
    ensure!(
        handshake(Path::new(SOCKET)) == Ok(true),
        "the original daemon is gone"
    );
    let dead = add_files(ctx, &[("dead", 4096, b'd')])?;

    // (2) After the recheck: swap while the root check runs. Many
    // processes make the daemon's runtime-root scan take a while.
    let mut sleepers = Vec::new();
    for _ in 0..400 {
        sleepers.push(
            tester_command(ctx, &[], "/bin/sleep", &["600"])
                .stdin(Stdio::null())
                .spawn()
                .map_err(|e| format!("start a sleeper: {e}"))?,
        );
    }
    let second_oracle = Oracle::arm(
        nix_state_writers(Fixture::new().eligible(dead[0].clone()))
            .concurrent_writer("/nix/var/nix/daemon-socket"),
    )?;
    let outcome = (|| -> Result<(Run, usize, bool), String> {
        let mut spawned = under_test::spawn_as_tester_with_env(
            &ctx.ledger,
            &ctx.records,
            Path::new("/"),
            &[],
            &ctx.diskgc(),
            &ensure_args("100%"),
        )?;
        let deadline = Instant::now() + Duration::from_secs(60);
        let roots = loop {
            if let Some(pid) = tester_processes_with(&["--query", "--roots"]).first() {
                break *pid;
            }
            ensure!(
                !spawned.finished()?,
                "diskgc ended before its root check was seen"
            );
            ensure!(Instant::now() < deadline, "no root check within 60 s");
            std::thread::sleep(Duration::from_millis(1));
        };
        let decoy = Decoy::replace()?;
        let in_window = alive(roots);
        let run = spawned.wait()?;
        let contacts = decoy.restore(&original)?;
        Ok((run, contacts, in_window))
    })();
    for mut sleeper in sleepers {
        let _ = sleeper.kill();
        let _ = sleeper.wait();
    }
    let (second, contacts, in_window) = outcome?;
    info!(
        "(2) replaced during the root check (still running at the swap: {in_window}): exit \
         {:?}\n--- stdout\n{}--- stderr\n{}",
        second.code, second.stdout, second.stderr
    );
    ensure!(
        in_window,
        "inconclusive: the root check had ended before the socket was replaced"
    );
    oracle.judge(&first)?.assert_clean()?;
    second_oracle.judge(&second)?.assert_clean()?;
    let report2 = report(&second)?;
    nix_action(&report2)?;
    ensure!(
        contacts == 0,
        "(2) the GC connected to the decoy {contacts} times"
    );
    ensure!(
        !exists(&dead[0]),
        "(2) the GC did not reach the original daemon"
    );
    for path in &rooted {
        ensure!(exists(path), "rooted {} was removed", path.display());
    }
    daemon.stop()?;
    Ok(())
}

/// A decoy listener put at the socket's pathname by container root,
/// counting connections, until restored.
struct Decoy {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: std::thread::JoinHandle<usize>,
}

impl Decoy {
    fn replace() -> Result<Self, String> {
        let moved = format!("{SOCKET}.original");
        rustix::fs::rename(SOCKET, &moved).map_err(|e| format!("move the socket away: {e}"))?;
        let listener = UnixListener::bind(SOCKET).map_err(|e| format!("bind the decoy: {e}"))?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = std::thread::spawn(move || {
            let mut contacts = 0;
            while !stopping.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok(_) => contacts += 1,
                    Err(_) => std::thread::sleep(Duration::from_millis(2)),
                }
            }
            contacts
        });
        Ok(Self { stop, worker })
    }

    /// Stop the decoy and put the original socket (still the inode
    /// `original` pins) back at its pathname; how often the decoy was
    /// connected to.
    fn restore(self, original: &std::os::fd::OwnedFd) -> Result<usize, String> {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let contacts = self.worker.join().map_err(|_| "decoy thread panicked")?;
        let moved = format!("{SOCKET}.original");
        rustix::fs::rename(&moved, SOCKET).map_err(|e| format!("restore the socket: {e}"))?;
        let at_path = rustix::fs::stat(SOCKET).map_err(|e| e.to_string())?;
        let pinned = rustix::fs::fstat(original).map_err(|e| e.to_string())?;
        ensure!(
            at_path.st_ino == pinned.st_ino,
            "the restored socket is not the original inode"
        );
        Ok(contacts)
    }
}
