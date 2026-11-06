//! The host-side runner behind `just e2e`, `just e2e-full` and
//! `just e2e-one NAME` (docs/testing.md, Commands).
//!
//! It runs no destructive code itself. In order:
//!
//! 1. Runs the do-nothing interlock canary on the host and requires it to
//!    refuse (exit [`exit::INTERLOCK`]); anything else stops the run.
//! 2. Requires Podman's native overlay storage driver (`podman info`); with
//!    fuse-overlayfs or another driver the snapshot's key rules are not
//!    validated, so the run stops with a clear message.
//! 3. Makes sure the base image is loaded: `podman image exists` on its tag
//!    (the tag is the Nix hash of the image, so it changes only when the
//!    image's inputs change); only when missing is the Nix stream script
//!    piped into `podman load`.
//! 4. Lists the tests from inside a container (`diskgc-e2e list`).
//! 5. Runs one container per selected test, in parallel, with
//!    `--security-opt no-new-privileges --cap-add SYS_ADMIN
//!    --cap-add SYS_PTRACE --network=none`, the binaries
//!    under test mounted read-only, and records each test's duration and log.
//! 6. Reports, and fails when a test failed or the run exceeded its budget.
//!    The budget is also a deadline: tests still running at it are killed.
//!
//! Steps 1 to 4 share the run's deadline too (the suite budget, or
//! [`ONE_SETUP_LIMIT`] for `one`): a hung `podman` or image stream is killed
//! with its process group, and the run fails naming the step ([`output_by`]).
//!
//! The Nix flake's `e2e-runner` package wraps this binary with the image and
//! under-test paths, so `nix run .#e2e-runner -- run --suite fast` is all a
//! recipe needs.

use std::collections::VecDeque;
use std::fs::File;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

use crate::suite::Tier;
use crate::{UNDER_TEST_BIN_DIR, exit, info};

const USAGE: &str = "\
usage: diskgc-e2e-runner --image-stream PATH --image-ref REF --under-test DIR
                         [--runtime standard|dind]
                         [--log-dir DIR] COMMAND
commands:
  run --suite fast|full|release [--budget-secs N] [--jobs N]
      fast: the fast tests; full: every test; release: candidate and Cargo CLI probes
  one NAME
      one test; its container is kept when it fails";

/// Paths the flake provides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Nix `streamLayeredImage` script that writes the base image to stdout.
    pub image_stream: PathBuf,
    /// `name:tag` of the base image, the tag being its Nix hash.
    pub image_ref: String,
    /// Directory with `bin/diskgc`, `bin/diskgc-e2e`, `bin/diskgc-e2e-canary`
    /// and `bin/diskgc-e2e-driver`.
    pub under_test: PathBuf,
    /// Where per-test logs go (on the host).
    pub log_dir: PathBuf,
    /// The dind image has an entrypoint and needs extra isolation for its
    /// daemon. The normal images keep the existing Podman invocation.
    pub runtime: Runtime,
    /// The checkout (worktree) the runner was started from, its working
    /// directory: every container it creates carries it in
    /// [`CHECKOUT_LABEL`], so the cleanup tools find this harness's
    /// containers per checkout (tools/e2e_kept.py; TASK-311).
    pub checkout: PathBuf,
}

/// The Podman label every container of this runner carries, its value the
/// checkout ([`Config::checkout`]). The cleanup tools match a container as
/// the harness's only by its `diskgc-e2e-` name prefix and this label
/// (tools/e2e_kept.py mirrors it).
pub const CHECKOUT_LABEL: &str = "diskgc-e2e.checkout";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runtime {
    Standard,
    Dind,
}

/// Which tests to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// The named suite of tests.
    Suite {
        suite: SuiteKind,
        budget: Duration,
        jobs: usize,
    },
    /// One test by name, container kept on failure.
    One { name: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuiteKind {
    Fast,
    Full,
    Release,
}

impl SuiteKind {
    fn name(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Full => "full",
            Self::Release => "release",
        }
    }

    fn includes(self, test: &Listed) -> bool {
        match self {
            Self::Fast => test.tier == Tier::Fast,
            Self::Full => matches!(test.tier, Tier::Fast | Tier::Full),
            Self::Release => {
                test.tier == Tier::Fast
                    && (test.name.starts_with("release_candidate_")
                        || matches!(
                            test.name.split_once('@').map(|(name, _)| name),
                            Some(
                                "ensure_reclaims_the_oldest_real_target_and_stops_at_min_free"
                                    | "ensure_exits_1_when_every_candidate_was_processed_below_min_free"
                                    | "ensure_dry_run_plans_real_targets_and_deletes_nothing"
                            )
                        ))
            }
        }
    }
}

/// Parses the runner's command line (global flags may come first, as the
/// Nix wrapper adds them before the user's arguments).
pub fn parse_args(args: &[String]) -> Result<(Config, Selection), String> {
    let mut flags = std::collections::BTreeMap::new();
    let mut words = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(name) = arg.strip_prefix("--") {
            let value = iter
                .next()
                .ok_or_else(|| format!("--{name} needs a value"))?;
            if flags.insert(name.to_owned(), value.clone()).is_some() {
                return Err(format!("--{name} given twice"));
            }
        } else {
            words.push(arg.clone());
        }
    }
    let mut take = |name: &str| flags.remove(name);
    let required = |v: Option<String>, name: &str| v.ok_or_else(|| format!("--{name} is required"));
    let runtime = match take("runtime").as_deref() {
        None | Some("standard") => Runtime::Standard,
        Some("dind") => Runtime::Dind,
        other => return Err(format!("--runtime must be standard or dind, got {other:?}")),
    };
    let config = Config {
        image_stream: PathBuf::from(required(take("image-stream"), "image-stream")?),
        image_ref: required(take("image-ref"), "image-ref")?,
        under_test: PathBuf::from(required(take("under-test"), "under-test")?),
        log_dir: PathBuf::from(take("log-dir").unwrap_or_else(|| "tmp/e2e-logs".to_owned())),
        runtime,
        checkout: std::env::current_dir()
            .map_err(|e| format!("the working directory (the checkout) is unreadable: {e}"))?,
    };
    let selection = match words.as_slice() {
        [cmd] if cmd == "run" => {
            let suite = match take("suite").as_deref() {
                Some("fast") => SuiteKind::Fast,
                Some("full") => SuiteKind::Full,
                Some("release") => SuiteKind::Release,
                other => {
                    return Err(format!(
                        "--suite must be fast, full or release, got {other:?}"
                    ));
                }
            };
            let budget_secs: u64 = take("budget-secs")
                .unwrap_or_else(|| "300".to_owned())
                .parse()
                .map_err(|_| "--budget-secs must be a whole number of seconds".to_owned())?;
            let jobs = match take("jobs") {
                Some(j) => j
                    .parse()
                    .ok()
                    .filter(|j| *j > 0)
                    .ok_or("--jobs must be a positive number")?,
                None => std::thread::available_parallelism().map_or(4, |n| n.get()),
            };
            Selection::Suite {
                suite,
                budget: Duration::from_secs(budget_secs),
                jobs,
            }
        }
        [cmd, name] if cmd == "one" => Selection::One { name: name.clone() },
        _ => return Err(format!("expected a command, got {words:?}")),
    };
    if let Some(unknown) = flags.keys().next() {
        return Err(format!("unknown or misplaced flag --{unknown}"));
    }
    Ok((config, selection))
}

/// Entry point of `diskgc-e2e-runner`; returns the process exit code.
pub fn main(args: &[String]) -> i32 {
    let (config, selection) = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!("diskgc-e2e-runner: {e}\n{USAGE}");
            return exit::USAGE;
        }
    };
    match execute(&config, &selection) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("diskgc-e2e-runner: error: {e}");
            exit::FAIL
        }
    }
}

/// How long `one`'s setup (canary, Podman checks, image load, listing) may
/// take: a cold load of the multi-gigabyte Nix image takes a few minutes.
pub const ONE_SETUP_LIMIT: Duration = Duration::from_secs(15 * 60);

/// How long `one`'s test may run: it is interactive, so this only guards
/// against a hang.
const ONE_TEST_LIMIT: Duration = Duration::from_secs(24 * 3600);

/// How long a cleanup `podman kill` or `podman rm` may take.
const CLEANUP_LIMIT: Duration = Duration::from_secs(30);

/// The deadline of the steps before the tests: a suite's budget covers
/// them, so they share its deadline.
pub fn setup_deadline(selection: &Selection, started: Instant) -> Instant {
    match selection {
        Selection::Suite { budget, .. } => started + *budget,
        Selection::One { .. } => started + ONE_SETUP_LIMIT,
    }
}

fn execute(config: &Config, selection: &Selection) -> Result<i32, String> {
    let started = Instant::now();
    let setup_by = setup_deadline(selection, started);
    check_canary_refuses_on_host(config, setup_by)?;
    require_native_overlay_storage(setup_by)?;
    require_rootless(setup_by)?;
    ensure_base_image(config, setup_by)?;
    #[expect(
        clippy::disallowed_methods,
        reason = "the host-side runner's own log directory, not a fixture"
    )]
    std::fs::create_dir_all(&config.log_dir)
        .map_err(|e| format!("create {}: {e}", config.log_dir.display()))?;
    let (listed, image) = list_tests(config, setup_by)?;

    match selection {
        Selection::Suite {
            suite,
            budget,
            jobs,
        } => {
            let selected: Vec<Listed> = listed.into_iter().filter(|t| suite.includes(t)).collect();
            if selected.is_empty() {
                return Err("no tests selected".to_owned());
            }
            let suite = suite.name();
            info!(
                "running {} {suite}-suite tests, {jobs} at a time, budget {budget:?}",
                selected.len()
            );
            let deadline = started + *budget;
            let release = release_under_test(config);
            let (results, phases) = run_parallel(
                started,
                selected,
                *jobs,
                deadline,
                |test| shares_host_pid_namespace(release, &test.name),
                |test, deadline| run_container(config, test, deadline, false),
            );
            Ok(report(&image, &results, &phases))
        }
        Selection::One { name } => {
            let Some(test) = listed.into_iter().find(|t| &t.name == name) else {
                return Err(format!(
                    "no test named {name:?}; `just e2e-full` lists every test it runs"
                ));
            };
            let deadline = Instant::now() + ONE_TEST_LIMIT;
            let result = run_container(config, &test, deadline, true);
            let phases = [Phase {
                name: "the run",
                took: started.elapsed(),
                bound: None,
            }];
            Ok(report(&image, &[result], &phases))
        }
    }
}

/// Runs `command` like [`Command::output`], but in its own process group
/// that is killed with `SIGKILL` once `deadline` passes, so a hung step
/// fails the run instead of hanging it. `what` names the step in errors.
/// Stdin is the caller's choice (default: inherited).
pub fn output_by(command: &mut Command, deadline: Instant, what: &str) -> Result<Output, String> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("spawn {what}: {e}"))?;
    // Pipes are drained on threads so a chatty step cannot block on a full
    // pipe; the channel lets the wait give up at the deadline even if a
    // helper outside the group keeps a pipe open.
    let (sender, received) = mpsc::channel();
    for (index, pipe) in [
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .enumerate()
    {
        let mut pipe = pipe.expect("stdout and stderr are piped");
        let sender = sender.clone();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let read = pipe.read_to_end(&mut bytes).map(|_| bytes);
            // The receiver is gone only when the step already failed.
            let _ = sender.send((index, read));
        });
    }
    let status = wait_by(&mut child, deadline, what)?;
    let mut pipes = [Vec::new(), Vec::new()];
    for _ in 0..2 {
        let left = deadline.saturating_duration_since(Instant::now());
        let (index, read) = received.recv_timeout(left).map_err(|_| {
            // The leader is reaped, but a group member still holds the
            // pipe, so the group id is still in use and cannot name an
            // unrelated process.
            kill_group(&child);
            format!("{what} exited ({status}) but its output was still open at the deadline")
        })?;
        pipes[index] = read.map_err(|e| format!("read the output of {what}: {e}"))?;
    }
    let [stdout, stderr] = pipes;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Waits for `child` (the leader of its own process group) until
/// `deadline`; then kills the group, reaps the leader and fails.
fn wait_by(child: &mut Child, deadline: Instant, what: &str) -> Result<ExitStatus, String> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                kill_group(child);
                // Reaping after SIGKILL returns at once unless the leader
                // sleeps uninterruptibly in the kernel.
                let _ = child.wait();
                return Err(format!(
                    "{what} was still running at the run's deadline; killed it"
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                kill_group(child);
                let _ = child.wait();
                return Err(format!("wait for {what}: {e}"));
            }
        }
    }
}

/// `SIGKILL` to the process group `child` leads (spawned with
/// `process_group(0)`): a step such as the image stream script can start
/// helpers that would outlive their leader.
fn kill_group(child: &Child) {
    let pid = rustix::process::Pid::from_raw(child.id().cast_signed())
        .expect("a child's pid is positive");
    if let Err(e) = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL) {
        // ESRCH: the group is already gone, which is what we wanted.
        if e != rustix::io::Errno::SRCH {
            eprintln!("runner: kill process group {}: {e}", child.id());
        }
    }
}

/// `podman ARGS` with a deadline [`CLEANUP_LIMIT`] from now, for cleanup:
/// a failure is printed (never silent) and returned.
fn podman_cleanup(args: &[&str]) -> Result<(), String> {
    let what = format!("podman {}", args.join(" "));
    let out = output_by(
        Command::new("podman").args(args).stdin(Stdio::null()),
        Instant::now() + CLEANUP_LIMIT,
        &what,
    )
    .and_then(|out| {
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "{what} failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    });
    if let Err(e) = &out {
        eprintln!("runner: {e}");
    }
    out
}

/// Interlock proof on every run: the do-nothing canary, run here on the
/// host, must refuse. If it runs, the interlock is broken and nothing else
/// is started.
fn check_canary_refuses_on_host(config: &Config, deadline: Instant) -> Result<(), String> {
    let canary = config.under_test.join("bin/diskgc-e2e-canary");
    let out = output_by(
        Command::new(&canary).stdin(Stdio::null()),
        deadline,
        &format!("interlock canary {}", canary.display()),
    )?;
    if out.status.code() == Some(exit::INTERLOCK) {
        info!(
            "interlock canary refused to run on the host (exit {}), as it must: {}",
            exit::INTERLOCK,
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .next()
                .unwrap_or("")
        );
        Ok(())
    } else {
        Err(format!(
            "INTERLOCK BROKEN: the do-nothing canary {} exited with {:?} on the host instead of \
             refusing with {}; no destructive test is started.\n--- stdout\n{}--- stderr\n{}",
            canary.display(),
            out.status.code(),
            exit::INTERLOCK,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

/// Fails fast unless Podman stores containers with its native overlay
/// driver. Under fuse-overlayfs (overlay with a `mount_program`) or another
/// driver (vfs, btrfs, ...) the container root is a different filesystem
/// whose inode numbers and birth times the deletion oracle's snapshot keys
/// were not validated on (false REPLACED, or replacements missed).
fn require_native_overlay_storage(deadline: Instant) -> Result<(), String> {
    let out = output_by(
        Command::new("podman")
            .args([
                "info",
                "--format",
                "{{.Store.GraphDriverName}}\n{{json .Store.GraphOptions}}",
            ])
            .stdin(Stdio::null()),
        deadline,
        "podman info (rootless Podman is a host requirement)",
    )?;
    if !out.status.success() {
        return Err(format!(
            "podman info failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (driver, options) = text.split_once('\n').unwrap_or((text.trim(), ""));
    check_storage_driver(driver.trim(), options.trim())?;
    info!("podman storage driver: native {driver}");
    Ok(())
}

/// Fails fast unless Podman runs rootless: the containers get
/// `CAP_SYS_ADMIN` and `CAP_SYS_PTRACE`, which only a user namespace
/// confines, and [`HOST_PID_NAMESPACE_ARGS`] would hand a rootful container
/// the host's processes.
fn require_rootless(deadline: Instant) -> Result<(), String> {
    let out = output_by(
        Command::new("podman")
            .args(["info", "--format", "{{.Host.Security.Rootless}}"])
            .stdin(Stdio::null()),
        deadline,
        "podman info (rootless Podman is a host requirement)",
    )?;
    let answer = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() || answer.trim() != "true" {
        return Err(format!(
            "the e2e harness needs rootless Podman; podman info says rootless={:?} ({}): {}",
            answer.trim(),
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

/// What the Owner PID tests ([`crate::suite::HOST_PID_NAMESPACE_TESTS`])
/// add: the host's PID namespace, with container uids and gids shifted by
/// one into the subordinate range, so that no container uid (container
/// root included) is the invoking user, whose host processes then show as
/// another user's (docs/testing.md, Container setup).
pub const HOST_PID_NAMESPACE_ARGS: &[&str] = &[
    "--pid=host",
    "--uidmap",
    "0:1:65536",
    "--gidmap",
    "0:1:65536",
];

/// The decision of [`require_native_overlay_storage`] on `podman info`'s
/// driver name and its graph options as JSON.
pub fn check_storage_driver(driver: &str, options_json: &str) -> Result<(), String> {
    let fix = "the e2e harness needs Podman's native overlay driver (rootless needs Linux \
               5.13+): driver = \"overlay\" without mount_program in storage.conf \
               (changing the driver of an existing store needs `podman system reset`, \
               which deletes all its images and containers; docs/testing.md, Commands)";
    if driver != "overlay" {
        return Err(format!(
            "podman storage driver is {driver:?}, not native overlay; {fix}"
        ));
    }
    if options_json.contains("mount_program") {
        return Err(format!(
            "podman storage driver is overlay through a mount program (fuse-overlayfs?): \
             {options_json}; {fix}"
        ));
    }
    Ok(())
}

/// Loads the base image unless an image with its tag (its Nix hash) exists.
fn ensure_base_image(config: &Config, deadline: Instant) -> Result<(), String> {
    let probe_started = Instant::now();
    if image_exists(&config.image_ref, deadline)? {
        info!(
            "base image {} already loaded: reused (checked in {:.2?})",
            config.image_ref,
            probe_started.elapsed()
        );
        return Ok(());
    }
    info!(
        "base image {} not present: loading it from {}",
        config.image_ref,
        config.image_stream.display()
    );
    let load_started = Instant::now();
    let stream_what = format!("image stream {}", config.image_stream.display());
    let mut stream = Command::new(&config.image_stream)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("spawn {stream_what}: {e}"))?;
    let image_tar = stream.stdout.take().expect("stdout is piped");
    let load = output_by(
        Command::new("podman")
            .args(["load", "--quiet"])
            .stdin(image_tar),
        deadline,
        "podman load of the base image",
    )
    .inspect_err(|_| {
        kill_group(&stream);
        let _ = stream.wait();
    })?;
    let streamed = wait_by(&mut stream, deadline, &stream_what)?;
    if !streamed.success() || !load.status.success() {
        return Err(format!(
            "loading the base image failed: stream {streamed}, podman load {}: {}",
            load.status,
            String::from_utf8_lossy(&load.stderr)
        ));
    }
    if !image_exists(&config.image_ref, deadline)? {
        return Err(format!(
            "podman load succeeded but {} does not exist; loaded: {}",
            config.image_ref,
            String::from_utf8_lossy(&load.stdout)
        ));
    }
    info!(
        "base image {} loaded in {:.2?}",
        config.image_ref,
        load_started.elapsed()
    );
    Ok(())
}

fn image_exists(image_ref: &str, deadline: Instant) -> Result<bool, String> {
    let out = output_by(
        Command::new("podman")
            .args(["image", "exists", image_ref])
            .stdin(Stdio::null()),
        deadline,
        "podman image exists (rootless Podman is a host requirement)",
    )?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(format!(
            "podman image exists {image_ref}: {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

/// The FUSE device, passed into the containers of
/// [`crate::suite::FUSE_TESTS`] when the host has it.
pub const FUSE_DEVICE: &str = "/dev/fuse";

/// The `podman run` arguments that give test `name` the FUSE device: only a
/// test that mounts FUSE gets it, and only when the host has one (a host
/// without runs it without, and the test fails saying so, never skipped).
fn fuse_device(name: &str) -> Vec<String> {
    let base = name.split('@').next().unwrap_or(name);
    if crate::suite::FUSE_TESTS.contains(&base) && Path::new(FUSE_DEVICE).exists() {
        vec!["--device".to_owned(), FUSE_DEVICE.to_owned()]
    } else {
        Vec::new()
    }
}

/// The real Buildah fixture uses pinned executables reached through the
/// under-test mount. Expose their Nix closure read-only for this one test,
/// without changing or reloading the multi-gigabyte base image.
/// The host-wide lock of host-PID-namespace containers: an exclusive
/// `flock` on `$XDG_RUNTIME_DIR/diskgc-e2e-host-pid.lock` (the user's
/// runtime directory, shared by every checkout and worktree; created if
/// missing, never written or removed), held while the container runs.
fn host_pid_lock(deadline: Instant) -> Result<File, String> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|d| !d.is_empty())
        .ok_or(
            "XDG_RUNTIME_DIR is not set: the host-PID-namespace containers need the user's \
                runtime directory for their host-wide lock",
        )?;
    host_pid_lock_at(&Path::new(&dir).join("diskgc-e2e-host-pid.lock"), deadline)
}

/// [`host_pid_lock`] on the lock file `path`.
fn host_pid_lock_at(path: &Path, deadline: Instant) -> Result<File, String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut announced = false;
    // Bounded by `deadline` (the host-PID tail's allowance, TASK-363): a
    // lock another runner holds past it fails this test, naming the holder.
    loop {
        if rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok() {
            return Ok(file);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the host-wide lock {} was still held at this phase's deadline, by {}",
                path.display(),
                lock_holder(path)
            ));
        }
        if !announced {
            info!(
                "waiting for {} (held by {}: another runner's host-PID-namespace container)",
                path.display(),
                lock_holder(path)
            );
            announced = true;
        }
        std::thread::sleep(HOST_PID_LOCK_POLL);
    }
}

/// How often [`host_pid_lock`] tries the lock again while another runner
/// holds it.
const HOST_PID_LOCK_POLL: Duration = Duration::from_millis(200);

/// Who holds the `flock` on `path`, from `/proc/locks`: its pid, working
/// directory and command line, or `unknown` when that cannot be read. A
/// best-effort diagnostic: the host's `/proc/locks` is often more than one
/// read, so it is read whole and each line parsed on its own (a line torn
/// between reads is skipped), not through the consistent
/// [`crate::proc_locks::sample`], which gives up on such a host.
fn lock_holder(path: &Path) -> String {
    let holder = crate::proc_locks::FileId::of(path).and_then(|id| {
        let text =
            std::fs::read_to_string("/proc/locks").map_err(|e| format!("read /proc/locks: {e}"))?;
        text.lines()
            .filter_map(|line| crate::proc_locks::parse(line).ok())
            .flatten()
            .find(|lock| lock.file == id && !lock.waiting)
            .map(|lock| lock.pid)
            .ok_or_else(|| "no granted lock on it listed in /proc/locks".to_owned())
    });
    match holder {
        Ok(pid) if pid > 0 => {
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline"))
                .map(|b| {
                    String::from_utf8_lossy(&b)
                        .replace('\0', " ")
                        .trim()
                        .to_owned()
                })
                .unwrap_or_else(|_| "?".to_owned());
            let cwd = std::fs::read_link(format!("/proc/{pid}/cwd"))
                .map_or_else(|_| "?".to_owned(), |p| p.display().to_string());
            format!("pid {pid} in {cwd} ({cmdline})")
        }
        Ok(pid) => format!("unknown (pid {pid} in /proc/locks)"),
        Err(e) => format!("unknown ({e})"),
    }
}

/// Whether the container of test `name` shares the host's PID namespace
/// ([`HOST_PID_NAMESPACE_ARGS`]): a test listed in
/// [`crate::suite::HOST_PID_NAMESPACE_TESTS`], or every test when the
/// release-candidate package is the binary under test
/// ([`crate::RELEASE_DEFAULT_MARKER`]): that build refuses to plan or
/// reclaim in a PID namespace of its own (docs/safety.md, In use;
/// TASK-263). Such containers run one at a time.
fn shares_host_pid_namespace(release_under_test: bool, name: &str) -> bool {
    release_under_test || crate::suite::HOST_PID_NAMESPACE_TESTS.contains(&name)
}

/// Whether the release runner's under-test directory is mounted
/// ([`crate::RELEASE_DEFAULT_MARKER`]).
fn release_under_test(config: &Config) -> bool {
    config
        .under_test
        .join("bin")
        .join(crate::RELEASE_DEFAULT_MARKER)
        .exists()
}

fn test_mounts(name: &str, host_pid_namespace: bool) -> Vec<String> {
    let mut extra = fuse_device(name);
    if name == crate::suite::BUILDAH_SPOOL_TEST
        || name == crate::suite::MESON_ANCHOR_TEST
        || name == crate::suite::AUTOTOOLS_ANCHOR_TEST
        || name == crate::suite::WEST_REAL_BUILD_TEST
        || name == crate::suite::PACKAGING_TEST
        || crate::suite::MYPY_TESTS.contains(&name)
    {
        extra.extend(["--volume".to_owned(), "/nix/store:/nix/store:ro".to_owned()]);
    }
    if host_pid_namespace {
        extra.extend(HOST_PID_NAMESPACE_ARGS.iter().map(|a| (*a).to_owned()));
    }
    if crate::suite::DOCKER_ONLY_TESTS
        .iter()
        .any(|(test, _)| *test == name)
    {
        extra.extend([
            "--privileged".to_owned(),
            "--tmpfs".to_owned(),
            "/var/lib/docker:rw,mode=0700,size=512m".to_owned(),
            "--tmpfs".to_owned(),
            "/var/lib/containerd:rw,mode=0700,size=512m".to_owned(),
        ]);
    }
    extra
}

/// `podman run` arguments shared by every container of a run, with `extra`
/// options for this one (before the image).
fn podman_run_args(config: &Config, name: &str, keep: bool, extra: &[String]) -> Vec<String> {
    let mut args: Vec<String> = [
        "run",
        "--name",
        name,
        "--pull=never",
        "--network=none",
        // The code under test drops to tester with no capabilities; with
        // no-new-privileges no setuid binary or file capability can give
        // any back (the credential test checks `NoNewPrivs`).
        "--security-opt",
        "no-new-privileges",
        "--cap-add",
        "SYS_ADMIN",
        "--cap-add",
        "SYS_PTRACE",
        "--label",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    args.push(format!("{CHECKOUT_LABEL}={}", config.checkout.display()));
    args.push("--volume".to_owned());
    args.push(format!(
        "{}:{UNDER_TEST_BIN_DIR}:ro",
        config.under_test.join("bin").display()
    ));
    if !keep {
        args.push("--rm".to_owned());
    }
    args.extend_from_slice(extra);
    if config.runtime == Runtime::Dind {
        // The official dind image starts dockerd as its entrypoint. The
        // harness starts it only inside the smoke test, after its interlock
        // and private scratch mounts have been checked.
        args.extend([
            "--entrypoint".to_owned(),
            format!("{UNDER_TEST_BIN_DIR}/diskgc-e2e"),
        ]);
    }
    args.push(config.image_ref.clone());
    if config.runtime == Runtime::Standard {
        args.push(format!("{UNDER_TEST_BIN_DIR}/diskgc-e2e"));
    }
    args
}

/// A test as listed by `diskgc-e2e list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub tier: Tier,
}

/// The prefix of the listing's line naming the image (its descriptor's
/// `name`, `crate::image`).
pub const IMAGE_LINE: &str = "# image ";

/// The image `diskgc-e2e list` names, if it names one.
pub fn listed_image(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix(IMAGE_LINE))
        .map(|name| name.trim().to_owned())
}

/// Parses the `name<TAB>tier` lines of `diskgc-e2e list` (`#` lines, such
/// as the image's name, are not tests).
pub fn parse_listing(text: &str) -> Result<Vec<Listed>, String> {
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|line| {
            let (name, tier) = line
                .split_once('\t')
                .ok_or_else(|| format!("bad listing line {line:?}"))?;
            let tier = Tier::parse(tier)
                .ok_or_else(|| format!("unknown tier in listing line {line:?}"))?;
            Ok(Listed {
                name: name.to_owned(),
                tier,
            })
        })
        .collect()
}

/// The tests the image lists, and the image's name.
fn list_tests(config: &Config, deadline: Instant) -> Result<(Vec<Listed>, String), String> {
    let name = format!("diskgc-e2e-{}-list", std::process::id());
    let out = output_by(
        Command::new("podman")
            .args(podman_run_args(config, &name, false, &[]))
            .arg("list")
            .stdin(Stdio::null()),
        deadline,
        "the test listing container",
    )
    .inspect_err(|_| {
        // Killing the podman client leaves the container to conmon.
        let _ = podman_cleanup(&["rm", "--force", "--ignore", "--time", "0", &name]);
    })?;
    if !out.status.success() {
        return Err(format!(
            "listing the tests in a container failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let listed = parse_listing(&text)?;
    if listed.is_empty() {
        return Err("the container listed no tests".to_owned());
    }
    let image = listed_image(&text).ok_or("the listing names no image")?;
    info!(
        "image {image} ({}) lists {} tests",
        config.image_ref,
        listed.len()
    );
    Ok((listed, image))
}

/// How one test ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    /// The test ran and failed (or podman failed) with this exit code.
    Fail(Option<i32>),
    /// Killed at the budget deadline.
    TimedOut,
    /// Not started: the deadline had already passed.
    NotRun,
}

/// One test's result.
#[derive(Debug, Clone)]
pub struct TestRun {
    pub name: String,
    pub tier: Tier,
    pub outcome: Outcome,
    pub took: Duration,
    pub log: PathBuf,
}

/// The order a suite's `tests` run in ([`run_parallel`]): the tests in a
/// PID namespace of their own, `jobs` at a time, then the
/// host-PID-namespace tests ([`shares_host_pid_namespace`]) one at a time,
/// with no other container of the suite running (TASK-363).
///
/// A host-PID-namespace test's process scan sees every process on the
/// host. Run beside the suite's other containers (their podman, strace,
/// compilers and tools start processes all the time), its scans did not
/// settle and its candidates were kept `in_use_unknown`: the suite
/// disturbing its own tests, a test-isolation defect, not product
/// behaviour. (Latent too: through the shifted uid map
/// ([`HOST_PID_NAMESPACE_ARGS`]) its tester is the same host uid as
/// another container's uid 1001, whose processes diskgc would read as the
/// user's own; no test starts one today, and none can run beside it now.)
/// Load from other work on the host, another worktree's suite included,
/// is not covered by this: the product's handling of an unsettled scan
/// is.
fn schedule(tests: Vec<Listed>, host_pid: impl Fn(&Listed) -> bool) -> (Vec<Listed>, Vec<Listed>) {
    let (serial, parallel) = tests.into_iter().partition(|test| host_pid(test));
    (parallel, serial)
}

/// What each host-PID-namespace test adds to the serial tail's allowance
/// ([`run_parallel`]): its own bound, separate from the parallel phase's
/// budget. The slowest of them took 12.5 s in a fast suite on a busy host
/// (2026-11-06); about 1.5 times that, and the allowance is their sum, so a
/// slow one borrows from the others. It guards a hang and a lock held by
/// another runner; it is not a speed target.
pub const HOST_PID_TEST_ALLOWANCE: Duration = Duration::from_secs(20);

/// One phase of a suite run: how long it took and its own bound
/// ([`verdict`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Phase {
    pub name: &'static str,
    pub took: Duration,
    pub bound: Option<Duration>,
}

/// Runs a suite's `tests` ([`schedule`]): the tests in a PID namespace of
/// their own `jobs` at a time, by `deadline` (the suite's budget from
/// `started`, setup included); then the host-PID-namespace tests
/// (`host_pid`) one at a time, by a deadline of their own:
/// [`HOST_PID_TEST_ALLOWANCE`] for each, from the end of the parallel
/// phase. Each phase is bounded on its own, so neither can use up the
/// other's time: under load from other work the parallel phase may end
/// late, and the tail still gets its whole allowance (TASK-363). `run`
/// runs one test by the deadline it is given. Returns the results and
/// both phases.
fn run_parallel(
    started: Instant,
    tests: Vec<Listed>,
    jobs: usize,
    deadline: Instant,
    host_pid: impl Fn(&Listed) -> bool,
    run: impl Fn(&Listed, Instant) -> TestRun + Sync,
) -> (Vec<TestRun>, Vec<Phase>) {
    let (parallel, serial) = schedule(tests, host_pid);
    let queue = Mutex::new(VecDeque::from(parallel));
    let results = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                loop {
                    let next = queue.lock().expect("queue lock").pop_front();
                    let Some(test) = next else { break };
                    let result = run(&test, deadline);
                    results.lock().expect("results lock").push(result);
                }
            });
        }
    });
    let mut results = results.into_inner().expect("results lock");
    let mut phases = vec![Phase {
        name: "setup and parallel phase",
        took: started.elapsed(),
        bound: Some(deadline.saturating_duration_since(started)),
    }];
    if !serial.is_empty() {
        let allowance =
            HOST_PID_TEST_ALLOWANCE.saturating_mul(u32::try_from(serial.len()).unwrap_or(u32::MAX));
        info!(
            "running {} host-PID-namespace tests one at a time, with no other container of \
             this suite running, within {allowance:?}",
            serial.len()
        );
        let tail_started = Instant::now();
        let tail_deadline = tail_started + allowance;
        for test in &serial {
            results.push(run(test, tail_deadline));
        }
        phases.push(Phase {
            name: "host-PID tail",
            took: tail_started.elapsed(),
            bound: Some(allowance),
        });
    }
    (results, phases)
}

/// Runs one test in its own container, logging its output to a file.
fn run_container(config: &Config, test: &Listed, deadline: Instant, keep: bool) -> TestRun {
    let log = config.log_dir.join(format!("{}.log", test.name));
    let started = Instant::now();
    let finish = |outcome: Outcome| {
        let run = TestRun {
            name: test.name.clone(),
            tier: test.tier,
            outcome,
            took: started.elapsed(),
            log: log.clone(),
        };
        eprintln!(
            "{:<9} {:<50} {:>8.2?}",
            outcome_label(&run.outcome),
            run.name,
            run.took
        );
        run
    };
    if Instant::now() >= deadline {
        return finish(Outcome::NotRun);
    }
    // Host-PID-namespace containers see every host process, other
    // runners' containers included: one at a time on the whole host, not
    // only in this runner (TASK-263 review, mped S1).
    let _host_pid_lock = if shares_host_pid_namespace(release_under_test(config), &test.name) {
        match host_pid_lock(deadline) {
            Ok(lock) => Some(lock),
            Err(e) => {
                eprintln!("runner: {}: {e}", test.name);
                return finish(Outcome::Fail(None));
            }
        }
    } else {
        None
    };
    let container = container_name(std::process::id(), &test.name);
    #[expect(
        clippy::disallowed_methods,
        reason = "the host-side runner's per-test log, not a fixture"
    )]
    let spawned = File::create(&log)
        .and_then(|f| Ok((f.try_clone()?, f)))
        .map_err(|e| format!("create log {}: {e}", log.display()))
        .and_then(|(out, err)| {
            let mut podman = Command::new("podman");
            if shares_host_pid_namespace(release_under_test(config), &test.name) {
                // With a shifted uid map, crun runs as a subordinate uid in
                // Podman's TMPDIR, which the dev shell points below the
                // checkout in a 0700 home: "openat /proc/self/cwd:
                // Permission denied". Podman's default works.
                podman.env_remove("TMPDIR");
            }
            podman
                .args(podman_run_args(
                    config,
                    &container,
                    keep,
                    &test_mounts(
                        &test.name,
                        shares_host_pid_namespace(release_under_test(config), &test.name),
                    ),
                ))
                .args(["run", &test.name])
                .stdin(Stdio::null())
                .stdout(out)
                .stderr(err)
                .spawn()
                .map_err(|e| format!("spawn podman: {e}"))
        });
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            eprintln!("runner: {}: {e}", test.name);
            #[expect(
                clippy::disallowed_methods,
                reason = "the host-side runner's own log file, not a fixture"
            )]
            let logged = std::fs::write(&log, format!("runner: {e}\n"));
            if let Err(write) = logged {
                eprintln!("runner: write {}: {write}", log.display());
            }
            return finish(Outcome::Fail(None));
        }
    };
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break Outcome::Pass,
            Ok(Some(status)) => break Outcome::Fail(status.code()),
            Ok(None) if Instant::now() >= deadline => {
                // The budget is a deadline: stop the container, not only the
                // podman client. Cleanup failures are printed by
                // podman_cleanup; the outcome is TIMEOUT either way.
                let _ = podman_cleanup(&["kill", &container]);
                let _ = child.kill();
                let _ = child.wait();
                // `podman kill` can race the container's creation; make sure
                // nothing of it keeps running.
                let _ = podman_cleanup(&["rm", "--force", "--ignore", "--time", "0", &container]);
                break Outcome::TimedOut;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                eprintln!("runner: waiting for {container}: {e}");
                break Outcome::Fail(None);
            }
        }
    };
    if keep {
        if outcome == Outcome::Pass {
            // --volumes: an image's anonymous volumes go with it, as with
            // the suite runs' `--rm`.
            if let Err(e) = podman_cleanup(&["rm", "--force", "--volumes", &container]) {
                // A passing `one` must not leave its container behind.
                eprintln!("runner: the test passed but its container was not removed: {e}");
                return finish(Outcome::Fail(None));
            }
        } else {
            eprintln!(
                "container {container} kept for inspection: podman logs {container}; \
                 podman diff {container}; podman cp {container}:/PATH .; podman rm {container}"
            );
        }
    }
    finish(outcome)
}

/// A Podman container name for `test` (names allow `[a-zA-Z0-9_.-]` only;
/// Cargo tests are listed as `NAME@TOOLCHAIN`).
pub fn container_name(runner_pid: u32, test: &str) -> String {
    let safe: String = test
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("diskgc-e2e-{runner_pid}-{safe}")
}

fn outcome_label(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Pass => "PASS".to_owned(),
        Outcome::Fail(Some(code)) if *code == exit::INTERLOCK => "INTERLOCK".to_owned(),
        // The test's Cargo toolchain under test failed its smoke check.
        Outcome::Fail(Some(code)) if *code == exit::TOOLCHAIN => "TOOLCHAIN".to_owned(),
        Outcome::Fail(_) => "FAIL".to_owned(),
        Outcome::TimedOut => "TIMEOUT".to_owned(),
        Outcome::NotRun => "NOT-RUN".to_owned(),
    }
}

/// Exit code of a run: failure when any test did not pass or the run took
/// longer than its budget.
pub fn verdict(results: &[TestRun], phases: &[Phase]) -> i32 {
    let all_passed = !results.is_empty() && results.iter().all(|r| r.outcome == Outcome::Pass);
    let within_budget = phases
        .iter()
        .all(|phase| phase.bound.is_none_or(|b| phase.took <= b));
    if all_passed && within_budget {
        exit::PASS
    } else {
        exit::FAIL
    }
}

/// Prints the per-test table, failing logs and the summary; returns the
/// exit code.
fn report(image: &str, results: &[TestRun], phases: &[Phase]) -> i32 {
    let mut sorted: Vec<&TestRun> = results.iter().collect();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.took));
    eprintln!("\ne2e per-test durations (slowest first):");
    for r in &sorted {
        eprintln!(
            "  {:<9} {:<4} {:<50} {:>8.2?}",
            outcome_label(&r.outcome),
            r.tier.as_str(),
            r.name,
            r.took
        );
    }
    for r in sorted.iter().filter(|r| r.outcome != Outcome::Pass) {
        let text = std::fs::read_to_string(&r.log).unwrap_or_default();
        let tail: Vec<&str> = text.lines().rev().take(80).collect();
        eprintln!(
            "\n--- {} {} (last {} lines of {})",
            outcome_label(&r.outcome),
            r.name,
            tail.len(),
            r.log.display()
        );
        for line in tail.iter().rev() {
            eprintln!("  {line}");
        }
    }
    let passed = results
        .iter()
        .filter(|r| r.outcome == Outcome::Pass)
        .count();
    let code = verdict(results, phases);
    let broken_toolchain = results
        .iter()
        .filter(|r| r.outcome == Outcome::Fail(Some(exit::TOOLCHAIN)))
        .count();
    if broken_toolchain > 0 {
        eprintln!(
            "\ne2e: {broken_toolchain} test(s) not run because their Cargo toolchain under test \
             failed its smoke check (TOOLCHAIN): a toolchain problem, not a race failure"
        );
    }
    let timing = phases
        .iter()
        .map(|phase| match phase.bound {
            Some(b) if phase.took > b => {
                format!(
                    "{} {:.1?}, BUDGET EXCEEDED: > {b:?}",
                    phase.name, phase.took
                )
            }
            Some(b) => format!("{} {:.1?} (budget {b:?})", phase.name, phase.took),
            None => format!("{} {:.1?}", phase.name, phase.took),
        })
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!(
        "\ne2e ({image} image): {passed} of {} tests passed: {timing}; logs in {}",
        results.len(),
        results
            .first()
            .and_then(|r| r.log.parent())
            .unwrap_or(Path::new("."))
            .display()
    );
    eprintln!("e2e: {}", if code == exit::PASS { "OK" } else { "FAILED" });
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    /// Only the listed tests get the FUSE device, and every listed name is
    /// a registered test (a renamed test would silently lose its device).
    #[test]
    fn only_the_fuse_tests_get_the_fuse_device() {
        for name in crate::suite::FUSE_TESTS {
            assert!(
                crate::suite::TESTS.iter().any(|t| t.name == *name),
                "{name} is not a registered test"
            );
        }
        assert!(fuse_device("fs_reclaim_never_deletes_a_socket_or_fifo").is_empty());
    }

    /// Only the listed tests share the host's PID namespace, and every
    /// listed name is a registered test, unless the release candidate is
    /// under test: then every test does (TASK-263).
    #[test]
    fn only_the_listed_tests_share_the_host_pid_namespace_unless_the_release_is_under_test() {
        let shared = |release: bool, name: &str| {
            let args = test_mounts(name, shares_host_pid_namespace(release, name));
            let host_pid = args.iter().any(|a| a == "--pid=host");
            assert_eq!(
                host_pid,
                args.windows(2).any(|w| w == ["--uidmap", "0:1:65536"]),
                "{name}: --pid=host always with the shifted uid map"
            );
            host_pid
        };
        for name in crate::suite::HOST_PID_NAMESPACE_TESTS {
            assert!(
                crate::suite::TESTS.iter().any(|t| t.name == *name),
                "{name} is not a registered test"
            );
            assert!(shared(false, name), "{name}");
        }
        assert!(!shared(false, "fs_reclaim_never_deletes_a_socket_or_fifo"));
        assert!(shared(true, "fs_reclaim_never_deletes_a_socket_or_fifo"));
        assert!(shared(
            true,
            "ensure_dry_run_plans_real_targets_and_deletes_nothing@1.99.1"
        ));
    }

    /// TASK-363: the host-PID-namespace tests run after every other test
    /// of the suite, one at a time; with the release candidate under test
    /// every test is one of them.
    #[test]
    fn host_pid_namespace_tests_run_after_the_parallel_ones() {
        let listed = |name: &str| Listed {
            name: name.to_owned(),
            tier: Tier::Fast,
        };
        let host_pid = crate::suite::HOST_PID_NAMESPACE_TESTS[0];
        let tests = vec![
            listed(host_pid),
            listed("fs_reclaim_never_deletes_a_socket_or_fifo"),
            listed("ensure_dry_run_plans_real_targets_and_deletes_nothing"),
        ];
        let names = |tests: &[Listed]| tests.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
        let shared = |release: bool| move |t: &Listed| shares_host_pid_namespace(release, &t.name);
        let (parallel, serial) = schedule(tests.clone(), shared(false));
        assert_eq!(
            names(&parallel),
            [
                "fs_reclaim_never_deletes_a_socket_or_fifo",
                "ensure_dry_run_plans_real_targets_and_deletes_nothing"
            ]
        );
        assert_eq!(names(&serial), [host_pid]);
        let (parallel, serial) = schedule(tests, shared(true));
        assert!(parallel.is_empty());
        assert_eq!(serial.len(), 3);
    }

    /// TASK-363: the tail starts only once every parallel test has ended,
    /// runs one test at a time, and is bounded by its own allowance
    /// ([`HOST_PID_TEST_ALLOWANCE`] for each), not by the parallel phase's
    /// deadline: with that deadline already past, the tail's tests still
    /// get the whole allowance, and each phase is reported with its bound.
    #[test]
    fn the_host_pid_tail_runs_after_the_parallel_phase_within_its_own_allowance() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listed = |name: &str| Listed {
            name: name.to_owned(),
            tier: Tier::Fast,
        };
        let tests: Vec<Listed> = ["p1", "h1", "p2", "h2", "p3"]
            .iter()
            .map(|n| listed(n))
            .collect();
        let started = Instant::now();
        let deadline = started; // the parallel phase's budget is used up
        let running = AtomicUsize::new(0);
        let log = Mutex::new(Vec::new());
        let (results, phases) = run_parallel(
            started,
            tests,
            3,
            deadline,
            |t| t.name.starts_with('h'),
            |test, by| {
                let overlap = running.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(20));
                log.lock()
                    .expect("log")
                    .push((test.name.clone(), overlap, by, Instant::now()));
                running.fetch_sub(1, Ordering::SeqCst);
                result(Outcome::Pass)
            },
        );
        assert_eq!(results.len(), 5);
        let log = log.into_inner().expect("log");
        let order: Vec<&str> = log.iter().map(|(n, ..)| n.as_str()).collect();
        let first_tail = order
            .iter()
            .position(|n| n.starts_with('h'))
            .expect("tail ran");
        assert!(
            order[..first_tail].iter().all(|n| n.starts_with('p'))
                && order[first_tail..].iter().all(|n| n.starts_with('h')),
            "the tail ran before a parallel test ended: {order:?}"
        );
        for (name, overlap, by, _) in &log {
            if name.starts_with('h') {
                assert_eq!(*overlap, 0, "{name} ran beside another test");
                assert!(
                    by.saturating_duration_since(started)
                        >= 2 * HOST_PID_TEST_ALLOWANCE - Duration::from_secs(1)
                        && *by > deadline,
                    "{name}: its deadline is not the tail's own allowance"
                );
            } else {
                assert_eq!(*by, deadline, "{name}: the parallel phase's deadline");
            }
        }
        assert_eq!(
            phases.iter().map(|p| (p.name, p.bound)).collect::<Vec<_>>(),
            [
                ("setup and parallel phase", Some(Duration::ZERO)),
                ("host-PID tail", Some(2 * HOST_PID_TEST_ALLOWANCE)),
            ]
        );
    }

    /// TASK-363: waiting for the host-wide lock ends at the deadline it is
    /// given, with an error naming the holder; a free lock is taken.
    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "a unit test's own temporary lock file"
    )]
    fn the_host_pid_lock_wait_ends_at_its_deadline_naming_the_holder() {
        let path =
            std::env::temp_dir().join(format!("diskgc-e2e-host-pid-{}.lock", std::process::id()));
        let held = host_pid_lock_at(&path, Instant::now()).expect("a free lock is taken");
        let started = Instant::now();
        let error = host_pid_lock_at(&path, started + Duration::from_millis(500))
            .expect_err("held by another open file description");
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(500) && waited < Duration::from_secs(5),
            "{waited:?}"
        );
        assert!(
            error.contains("was still held at this phase's deadline, by "),
            "{error}"
        );
        // The holder, read from /proc/locks: a line torn between two reads
        // of a busy host's list is skipped, so a few tries.
        let own = format!("pid {} ", std::process::id());
        assert!(
            (0..5).any(|_| lock_holder(&path).starts_with(&own)),
            "{own}not named: {}",
            lock_holder(&path)
        );
        drop(held);
        host_pid_lock_at(&path, Instant::now()).expect("taken once released");
        std::fs::remove_file(&path).unwrap();
    }

    const GLOBAL: &[&str] = &[
        "--image-stream",
        "/s",
        "--image-ref",
        "localhost/x:abc",
        "--under-test",
        "/u",
    ];

    #[test]
    fn global_flags_before_the_command_as_the_wrapper_adds_them() {
        let mut a = args(GLOBAL);
        a.extend(args(&[
            "run",
            "--suite",
            "fast",
            "--budget-secs",
            "300",
            "--jobs",
            "3",
        ]));
        let (config, selection) = parse_args(&a).unwrap();
        assert_eq!(config.image_ref, "localhost/x:abc");
        assert_eq!(config.log_dir, PathBuf::from("tmp/e2e-logs"));
        assert_eq!(
            selection,
            Selection::Suite {
                suite: SuiteKind::Fast,
                budget: Duration::from_secs(300),
                jobs: 3
            }
        );
    }

    #[test]
    fn release_suite_selects_candidate_and_actual_cargo_cli_probes() {
        let mut a = args(GLOBAL);
        a.extend(args(&["run", "--suite", "release"]));
        assert!(matches!(
            parse_args(&a),
            Ok((
                _,
                Selection::Suite {
                    suite: SuiteKind::Release,
                    ..
                }
            ))
        ));
        let selected: Vec<_> = crate::suite::TESTS
            .iter()
            .filter(|test| {
                SuiteKind::Release.includes(&Listed {
                    name: test.name.to_owned(),
                    tier: test.tier,
                })
            })
            .map(|test| test.name)
            .collect();
        let mut selected = selected;
        selected.sort_unstable();
        assert_eq!(
            selected,
            [
                "release_candidate_cargo_busy_then_reclaims_real_target",
                "release_candidate_catalog_refuses_every_excluded_builtin",
                // TASK-360: on the Nix image only.
                "release_candidate_ccache_reclaim_then_recompile_and_a_compile_in_flight_completes",
                "release_candidate_checkout_policy_accepts_only_admitted_builtin",
                "release_candidate_files_work_in_container",
                // TASK-359: on the Nix image only.
                "release_candidate_mypy_cache_reclaim_and_a_run_in_flight_is_kept",
                // TASK-335: on the Nix GC image only.
                "release_candidate_nix_garbage_dry_run_then_bounded_gc",
                // TASK-359: on the Nix image only.
                "release_candidate_node_compile_cache_reclaim_then_a_live_process_recompiles",
                // TASK-360: on the Nix image only.
                "release_candidate_pip_cache_reclaim_then_refetch_and_an_install_in_flight_completes",
                "release_candidate_pnpm_store_prune_keeps_referenced_packages_and_an_install_in_flight",
                "release_candidate_pytest_cache_reclaim_then_a_live_session_rewrites_it",
                "release_candidate_python_pycache_reclaim_then_a_live_importer_recompiles",
                "release_candidate_python_venv_keeps_projects_inside_the_30_day_floor",
                "release_candidate_refuses_a_reclaiming_watch",
                "release_candidate_refuses_in_its_own_pid_namespace",
                "release_candidate_rejects_docker_command_and_arbitrary_delete_policy",
                "release_candidate_runs_watch_tally_only",
                // TASK-360: on the Nix image only.
                "release_candidate_uv_cache_reclaim_then_refetch_and_an_install_in_flight_is_kept",
            ],
            "the candidate suite lost or gained a probe"
        );
        for name in [
            "ensure_reclaims_the_oldest_real_target_and_stops_at_min_free@1.99.1",
            "ensure_exits_1_when_every_candidate_was_processed_below_min_free@1.99.1",
            "ensure_dry_run_plans_real_targets_and_deletes_nothing@1.99.1",
        ] {
            assert!(SuiteKind::Release.includes(&Listed {
                name: name.to_owned(),
                tier: Tier::Fast,
            }));
        }
        for name in [
            "cargo_targets_discovered_and_judged@1.99.1",
            "journey_ci_operator_frees_space_with_ensure@1.99.1",
        ] {
            assert!(!SuiteKind::Release.includes(&Listed {
                name: name.to_owned(),
                tier: Tier::Fast,
            }));
        }
    }

    #[test]
    fn container_names_carry_only_allowed_characters() {
        assert_eq!(
            container_name(7, "cargo_smoke@nightly-2026-10-16"),
            "diskgc-e2e-7-cargo-smoke-nightly-2026-10-16"
        );
        assert_eq!(
            container_name(7, "cargo_smoke@1.90.0"),
            "diskgc-e2e-7-cargo-smoke-1.90.0"
        );
        assert_eq!(
            container_name(7, "sandbox_trivial"),
            "diskgc-e2e-7-sandbox-trivial"
        );
    }

    #[test]
    fn one_names_a_test() {
        let mut a = args(GLOBAL);
        a.extend(args(&["one", "sandbox_trivial"]));
        let (_, selection) = parse_args(&a).unwrap();
        assert_eq!(
            selection,
            Selection::One {
                name: "sandbox_trivial".into()
            }
        );
    }

    #[test]
    fn bad_command_lines_are_errors() {
        assert!(parse_args(&args(&["run", "--suite", "fast"])).is_err());
        let mut a = args(GLOBAL);
        a.extend(args(&["run", "--suite", "slow"]));
        assert!(parse_args(&a).is_err());
        let mut a = args(GLOBAL);
        a.extend(args(&["run", "--suite", "fast", "--bogus", "1"]));
        assert!(parse_args(&a).is_err());
        let mut a = args(GLOBAL);
        a.extend(args(&["one", "x", "--suite", "fast"]));
        assert!(parse_args(&a).is_err());
    }

    #[test]
    fn listing_lines_parse_and_unknown_tiers_are_errors() {
        let listed = parse_listing("# image nix\na\tfast\nb\tfull\n").unwrap();
        assert_eq!(
            listed_image("# image nix\na\tfast\n").as_deref(),
            Some("nix")
        );
        assert_eq!(listed_image("a\tfast\n"), None);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[1].tier, Tier::Full);
        assert!(parse_listing("a\tslow\n").is_err());
        assert!(parse_listing("a fast\n").is_err());
    }

    fn result(outcome: Outcome) -> TestRun {
        TestRun {
            name: "t".into(),
            tier: Tier::Fast,
            outcome,
            took: Duration::from_secs(1),
            log: PathBuf::from("t.log"),
        }
    }

    #[test]
    fn the_budget_fails_a_run_that_passed_every_test() {
        let ok = [result(Outcome::Pass)];
        let budget = Some(Duration::from_secs(300));
        let phase = |name, took, bound| Phase { name, took, bound };
        let tail = phase(
            "tail",
            Duration::from_secs(40),
            Some(Duration::from_secs(60)),
        );
        let secs = Duration::from_secs;
        assert_eq!(
            verdict(&ok, &[phase("p", secs(299), budget), tail]),
            exit::PASS
        );
        assert_eq!(
            verdict(&ok, &[phase("p", secs(301), budget), tail]),
            exit::FAIL
        );
        assert_eq!(verdict(&ok, &[phase("p", secs(9999), None)]), exit::PASS);
        // Each phase by its own bound: a tail over its allowance fails the
        // run although the whole run took less than the parallel budget.
        let over = phase("tail", secs(61), Some(secs(60)));
        assert_eq!(
            verdict(&ok, &[phase("p", secs(10), budget), over]),
            exit::FAIL
        );
    }

    #[test]
    fn any_non_passing_test_or_no_test_fails_the_run() {
        let d = Duration::from_secs(1);
        for bad in [
            Outcome::Fail(Some(1)),
            Outcome::TimedOut,
            Outcome::NotRun,
            Outcome::Fail(None),
        ] {
            assert_eq!(
                verdict(
                    &[result(Outcome::Pass), result(bad)],
                    &[Phase {
                        name: "p",
                        took: d,
                        bound: None
                    }]
                ),
                exit::FAIL
            );
        }
        let unbounded = [Phase {
            name: "p",
            took: d,
            bound: None,
        }];
        assert_eq!(verdict(&[], &unbounded), exit::FAIL);
    }

    /// The runner failure probe fails on purpose, so no suite may select it.
    #[test]
    fn the_runner_failure_probe_is_in_no_suite() {
        let probe = crate::suite::TESTS
            .iter()
            .find(|t| t.name == crate::suite::RUNNER_FAILURE_PROBE)
            .expect("the probe is registered, so `one` can run it");
        assert_eq!(probe.tier, Tier::Probe);
        for test in crate::suite::TESTS.iter().filter(|t| t.tier == Tier::Probe) {
            let listed = Listed {
                name: test.name.to_owned(),
                tier: test.tier,
            };
            for suite in [SuiteKind::Fast, SuiteKind::Full, SuiteKind::Release] {
                assert!(!suite.includes(&listed), "{} in {suite:?}", test.name);
            }
        }
        assert_eq!(Tier::parse(Tier::Probe.as_str()), Some(Tier::Probe));
    }

    #[test]
    fn every_test_container_gets_no_new_privileges() {
        for runtime in ["standard", "dind"] {
            let mut a = args(GLOBAL);
            a.extend(args(&["--runtime", runtime, "one", "sandbox_trivial"]));
            let (config, _) = parse_args(&a).unwrap();
            for keep in [false, true] {
                let run = podman_run_args(&config, "c", keep, &[]);
                let image = run.iter().position(|a| *a == config.image_ref).unwrap();
                assert!(
                    run[..image]
                        .windows(2)
                        .any(|w| w == ["--security-opt", "no-new-privileges"]),
                    "{runtime}: {run:?}"
                );
            }
        }
    }

    /// Every container carries the checkout label the cleanup tools match
    /// (tools/e2e_kept.py), kept or not, on every runtime.
    #[test]
    fn every_container_carries_the_checkout_label() {
        for runtime in ["standard", "dind"] {
            let mut a = args(GLOBAL);
            a.extend(args(&["--runtime", runtime, "one", "sandbox_trivial"]));
            let (config, _) = parse_args(&a).unwrap();
            assert!(config.checkout.is_absolute(), "{:?}", config.checkout);
            let label = format!("{CHECKOUT_LABEL}={}", config.checkout.display());
            for keep in [false, true] {
                let run = podman_run_args(&config, "c", keep, &[]);
                let image = run.iter().position(|a| *a == config.image_ref).unwrap();
                assert!(
                    run[..image].windows(2).any(|w| w == ["--label", &label]),
                    "{runtime}: {run:?}"
                );
            }
        }
    }

    #[test]
    fn setup_shares_the_suite_deadline_and_one_has_its_own_limit() {
        let started = Instant::now();
        let suite = Selection::Suite {
            suite: SuiteKind::Fast,
            budget: Duration::from_secs(300),
            jobs: 1,
        };
        assert_eq!(
            setup_deadline(&suite, started),
            started + Duration::from_secs(300)
        );
        let one = Selection::One { name: "t".into() };
        assert_eq!(setup_deadline(&one, started), started + ONE_SETUP_LIMIT);
    }

    #[test]
    fn a_setup_step_returns_its_output_before_the_deadline() {
        let out = output_by(
            Command::new("sh").args(["-c", "echo out; echo err >&2; exit 3"]),
            Instant::now() + Duration::from_secs(60),
            "probe",
        )
        .unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");
    }

    /// A hung step and its helpers are killed at the deadline, not waited
    /// for: `wait` would otherwise keep the shell alive for a minute, and
    /// a kill of the shell alone would leave its `sleep` running.
    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "a unit test's own temporary file, in the Nix build sandbox"
    )]
    fn a_hung_setup_step_is_killed_with_its_group_at_the_deadline() {
        let pid_file = std::env::temp_dir().join(format!("diskgc-e2e-hung-{}", std::process::id()));
        let started = Instant::now();
        let error = output_by(
            Command::new("sh")
                .args(["-c", r#"sleep 60 & echo $! > "$0"; wait"#])
                .arg(&pid_file),
            started + Duration::from_millis(300),
            "hung probe",
        )
        .unwrap_err();
        assert!(error.contains("hung probe was still running"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10));
        let helper = std::fs::read_to_string(&pid_file).unwrap();
        std::fs::remove_file(&pid_file).unwrap();
        // Killed: gone, or a zombie its new parent has not reaped yet.
        let stat = format!("/proc/{}/stat", helper.trim());
        let alive = || {
            std::fs::read_to_string(&stat)
                .ok()
                .and_then(|s| s.rsplit_once(") ").map(|(_, rest)| !rest.starts_with('Z')))
                .unwrap_or(false)
        };
        let waited = Instant::now();
        while alive() && waited.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(), "the step's helper {} survived", helper.trim());
    }

    /// A helper that outlives its leader and keeps the output pipe open
    /// fails the step at the deadline (the group kill then ends it).
    #[test]
    fn a_helper_holding_the_output_open_fails_the_step_at_the_deadline() {
        let started = Instant::now();
        let error = output_by(
            Command::new("sh").args(["-c", "sleep 60 &"]),
            started + Duration::from_millis(300),
            "leaky probe",
        )
        .unwrap_err();
        assert!(error.contains("output was still open"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn only_the_native_overlay_driver_is_accepted() {
        assert!(check_storage_driver("overlay", "{}").is_ok());
        let fuse = r#"{"overlay.mount_program":{"Executable":"/usr/bin/fuse-overlayfs","Package":"fuse-overlayfs-1.13","Version":"fusermount3 version: 3.16.2"}}"#;
        let err = check_storage_driver("overlay", fuse).unwrap_err();
        assert!(err.contains("mount program"), "{err}");
        assert!(err.contains("fuse-overlayfs"), "{err}");
        for other in ["vfs", "btrfs", "zfs", ""] {
            let err = check_storage_driver(other, "{}").unwrap_err();
            assert!(err.contains("not native overlay"), "{other}: {err}");
        }
    }
}
