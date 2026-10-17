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
//!    `--cap-add SYS_ADMIN --cap-add SYS_PTRACE --network=none`, the binaries
//!    under test mounted read-only, and records each test's duration and log.
//! 6. Reports, and fails when a test failed or the run exceeded its budget.
//!    The budget is also a deadline: tests still running at it are killed.
//!
//! The Nix flake's `e2e-runner` package wraps this binary with the image and
//! under-test paths, so `nix run .#e2e-runner -- run --suite fast` is all a
//! recipe needs.

use std::collections::VecDeque;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::suite::Tier;
use crate::{UNDER_TEST_BIN_DIR, exit, info};

const USAGE: &str = "\
usage: diskgc-e2e-runner --image-stream PATH --image-ref REF --under-test DIR
                         [--log-dir DIR] COMMAND
commands:
  run --suite fast|full [--budget-secs N] [--jobs N]
      fast: the fast tests (the `just e2e` gate); full: every test
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
}

/// Which tests to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// `fast` tests only, or every test with `full`.
    Suite {
        full: bool,
        budget: Duration,
        jobs: usize,
    },
    /// One test by name, container kept on failure.
    One { name: String },
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
    let config = Config {
        image_stream: PathBuf::from(required(take("image-stream"), "image-stream")?),
        image_ref: required(take("image-ref"), "image-ref")?,
        under_test: PathBuf::from(required(take("under-test"), "under-test")?),
        log_dir: PathBuf::from(take("log-dir").unwrap_or_else(|| "target/e2e-logs".to_owned())),
    };
    let selection = match words.as_slice() {
        [cmd] if cmd == "run" => {
            let full = match take("suite").as_deref() {
                Some("fast") => false,
                Some("full") => true,
                other => return Err(format!("--suite must be fast or full, got {other:?}")),
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
                full,
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

fn execute(config: &Config, selection: &Selection) -> Result<i32, String> {
    let started = Instant::now();
    check_canary_refuses_on_host(config)?;
    require_native_overlay_storage()?;
    ensure_base_image(config)?;
    std::fs::create_dir_all(&config.log_dir)
        .map_err(|e| format!("create {}: {e}", config.log_dir.display()))?;
    let listed = list_tests(config)?;

    match selection {
        Selection::Suite { full, budget, jobs } => {
            let selected: Vec<Listed> = listed
                .into_iter()
                .filter(|t| *full || t.tier == Tier::Fast)
                .collect();
            if selected.is_empty() {
                return Err("no tests selected".to_owned());
            }
            let suite = if *full { "full" } else { "fast" };
            info!(
                "running {} {suite}-suite tests, {jobs} at a time, budget {budget:?}",
                selected.len()
            );
            let deadline = started + *budget;
            let results = run_parallel(config, selected, *jobs, deadline);
            Ok(report(&results, started.elapsed(), Some(*budget)))
        }
        Selection::One { name } => {
            let Some(test) = listed.into_iter().find(|t| &t.name == name) else {
                return Err(format!(
                    "no test named {name:?}; `just e2e-full` lists every test it runs"
                ));
            };
            // A day: e2e-one is interactive; the deadline only guards hangs.
            let deadline = started + Duration::from_secs(24 * 3600);
            let result = run_container(config, &test, deadline, true);
            Ok(report(&[result], started.elapsed(), None))
        }
    }
}

/// Interlock proof on every run: the do-nothing canary, run here on the
/// host, must refuse. If it runs, the interlock is broken and nothing else
/// is started.
fn check_canary_refuses_on_host(config: &Config) -> Result<(), String> {
    let canary = config.under_test.join("bin/diskgc-e2e-canary");
    let out = Command::new(&canary)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("spawn interlock canary {}: {e}", canary.display()))?;
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
fn require_native_overlay_storage() -> Result<(), String> {
    let out = Command::new("podman")
        .args([
            "info",
            "--format",
            "{{.Store.GraphDriverName}}\n{{json .Store.GraphOptions}}",
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("spawn podman (rootless Podman is a host requirement): {e}"))?;
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
fn ensure_base_image(config: &Config) -> Result<(), String> {
    let probe_started = Instant::now();
    if image_exists(&config.image_ref)? {
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
    let mut stream = Command::new(&config.image_stream)
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", config.image_stream.display()))?;
    let image_tar = stream.stdout.take().expect("stdout is piped");
    let load = Command::new("podman")
        .args(["load", "--quiet"])
        .stdin(image_tar)
        .output()
        .map_err(|e| format!("spawn podman load: {e}"))?;
    let streamed = stream
        .wait()
        .map_err(|e| format!("wait for {}: {e}", config.image_stream.display()))?;
    if !streamed.success() || !load.status.success() {
        return Err(format!(
            "loading the base image failed: stream {streamed}, podman load {}: {}",
            load.status,
            String::from_utf8_lossy(&load.stderr)
        ));
    }
    if !image_exists(&config.image_ref)? {
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

fn image_exists(image_ref: &str) -> Result<bool, String> {
    let status = Command::new("podman")
        .args(["image", "exists", image_ref])
        .status()
        .map_err(|e| format!("spawn podman (rootless Podman is a host requirement): {e}"))?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(format!("podman image exists {image_ref}: {status}")),
    }
}

/// `podman run` arguments shared by every container of a run.
fn podman_run_args(config: &Config, name: &str, keep: bool) -> Vec<String> {
    let mut args: Vec<String> = [
        "run",
        "--name",
        name,
        "--pull=never",
        "--network=none",
        "--cap-add",
        "SYS_ADMIN",
        "--cap-add",
        "SYS_PTRACE",
        "--volume",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    args.push(format!(
        "{}:{UNDER_TEST_BIN_DIR}:ro",
        config.under_test.join("bin").display()
    ));
    if !keep {
        args.push("--rm".to_owned());
    }
    args.push(config.image_ref.clone());
    args.push(format!("{UNDER_TEST_BIN_DIR}/diskgc-e2e"));
    args
}

/// A test as listed by `diskgc-e2e list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub tier: Tier,
}

/// Parses the `name<TAB>tier` lines of `diskgc-e2e list`.
pub fn parse_listing(text: &str) -> Result<Vec<Listed>, String> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
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

fn list_tests(config: &Config) -> Result<Vec<Listed>, String> {
    let name = format!("diskgc-e2e-{}-list", std::process::id());
    let out = Command::new("podman")
        .args(podman_run_args(config, &name, false))
        .arg("list")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("spawn podman: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "listing the tests in a container failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let listed = parse_listing(&String::from_utf8_lossy(&out.stdout))?;
    if listed.is_empty() {
        return Err("the container listed no tests".to_owned());
    }
    Ok(listed)
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

fn run_parallel(
    config: &Config,
    tests: Vec<Listed>,
    jobs: usize,
    deadline: Instant,
) -> Vec<TestRun> {
    let queue = Mutex::new(VecDeque::from(tests));
    let results = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                loop {
                    let next = queue.lock().expect("queue lock").pop_front();
                    let Some(test) = next else { break };
                    let result = run_container(config, &test, deadline, false);
                    results.lock().expect("results lock").push(result);
                }
            });
        }
    });
    results.into_inner().expect("results lock")
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
    let container = format!(
        "diskgc-e2e-{}-{}",
        std::process::id(),
        test.name.replace('_', "-")
    );
    let spawned = File::create(&log)
        .and_then(|f| Ok((f.try_clone()?, f)))
        .map_err(|e| format!("create log {}: {e}", log.display()))
        .and_then(|(out, err)| {
            Command::new("podman")
                .args(podman_run_args(config, &container, keep))
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
            let _ = std::fs::write(&log, format!("runner: {e}\n"));
            return finish(Outcome::Fail(None));
        }
    };
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break Outcome::Pass,
            Ok(Some(status)) => break Outcome::Fail(status.code()),
            Ok(None) if Instant::now() >= deadline => {
                // The budget is a deadline: stop the container, not only the
                // podman client.
                let _ = Command::new("podman")
                    .args(["kill", &container])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                let _ = child.kill();
                let _ = child.wait();
                // `podman kill` can race the container's creation; make sure
                // nothing of it keeps running.
                let _ = Command::new("podman")
                    .args(["rm", "--force", "--ignore", "--time", "0", &container])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
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
            let _ = Command::new("podman")
                .args(["rm", "--force", &container])
                .stdout(Stdio::null())
                .status();
        } else {
            eprintln!(
                "container {container} kept for inspection: podman logs {container}; \
                 podman diff {container}; podman cp {container}:/PATH .; podman rm {container}"
            );
        }
    }
    finish(outcome)
}

fn outcome_label(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Pass => "PASS".to_owned(),
        Outcome::Fail(Some(code)) if *code == exit::INTERLOCK => "INTERLOCK".to_owned(),
        Outcome::Fail(_) => "FAIL".to_owned(),
        Outcome::TimedOut => "TIMEOUT".to_owned(),
        Outcome::NotRun => "NOT-RUN".to_owned(),
    }
}

/// Exit code of a run: failure when any test did not pass or the run took
/// longer than its budget.
pub fn verdict(results: &[TestRun], elapsed: Duration, budget: Option<Duration>) -> i32 {
    let all_passed = !results.is_empty() && results.iter().all(|r| r.outcome == Outcome::Pass);
    let within_budget = budget.is_none_or(|b| elapsed <= b);
    if all_passed && within_budget {
        exit::PASS
    } else {
        exit::FAIL
    }
}

/// Prints the per-test table, failing logs and the summary; returns the
/// exit code.
fn report(results: &[TestRun], elapsed: Duration, budget: Option<Duration>) -> i32 {
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
    let code = verdict(results, elapsed, budget);
    let budget_text = match budget {
        Some(b) if elapsed > b => format!("; BUDGET EXCEEDED: {elapsed:.1?} > {b:?}"),
        Some(b) => format!(" (budget {b:?})"),
        None => String::new(),
    };
    eprintln!(
        "\ne2e: {passed} of {} tests passed in {elapsed:.1?}{budget_text}; logs in {}",
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
        assert_eq!(config.log_dir, PathBuf::from("target/e2e-logs"));
        assert_eq!(
            selection,
            Selection::Suite {
                full: false,
                budget: Duration::from_secs(300),
                jobs: 3
            }
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
        let listed = parse_listing("a\tfast\nb\tfull\n").unwrap();
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
        assert_eq!(verdict(&ok, Duration::from_secs(299), budget), exit::PASS);
        assert_eq!(verdict(&ok, Duration::from_secs(301), budget), exit::FAIL);
        assert_eq!(verdict(&ok, Duration::from_secs(9999), None), exit::PASS);
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
                verdict(&[result(Outcome::Pass), result(bad)], d, None),
                exit::FAIL
            );
        }
        assert_eq!(verdict(&[], d, None), exit::FAIL);
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
