//! The `diskgc` binary: CLI and output, wiring `diskgc-core` to `LinuxHost`
//! (docs/reference.md is the contract).
//!
//! So far: `ensure` with `--select` (one `cargo-target` tier), `--dry-run`
//! and `--json` (task-10), the dry run of one kind `fs` tier (task-68),
//! plus `--version` and `--help`. The other
//! commands exit 2 saying which task brings them.
//!
//! The flow of `ensure`, every step before the next:
//!
//! 1. the command line ([`parse`]: pure; sizes and durations checked);
//! 2. startup: `LinuxHost` (its `/proc` checks) and the minimum kernel;
//! 3. privileges (root refused without `--allow-root`);
//! 4. the configuration (lookup, read, the part of `version: 1` loaded so
//!    far) and the `--select` tier, checked against it (static
//!    validation: nothing on the machine was touched yet);
//! 5. the executor: `dry_run` (given the host as a plain `Host`) or
//!    `ensure` (as a `Delete`, which asks the sandbox gate first).
//!
//! stdout carries only the result (`--json`: exactly one object, for every
//! exit code); stderr carries progress logs and errors.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use diskgc_core::config::{self, Config, Env, Lookup, MAX_CONFIG_BYTES};
use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::host::Errno;
use diskgc_core::profile::Library;
use diskgc_core::report::{Report, Status};
use diskgc_core::units::{Size, parse_duration, parse_size};
use diskgc_linux::LinuxHost;

/// Exit code for usage, config and provisioning errors, including a kernel
/// older than the minimum (docs/reference.md, Exit codes).
const EXIT_USAGE: u8 = 2;

/// `--timeout`'s default (docs/reference.md, `ensure`).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

const USAGE: &str = "\
usage: diskgc ensure --min-free SIZE [--target-free SIZE] [--timeout DUR]
                     [--tally-budget DUR] [--no-index] --select EXPR [--roots DIR]...
                     [--dry-run] [--json] [--config FILE] [--allow-root] PATH
       diskgc --version
       diskgc --help

This build runs `ensure --select` with one source profile term
(optionally `&& filter:idle-...`): `profile:cargo-target`, or, with
--dry-run only, a kind fs profile from the config (its delete is task-18);
`watch`, `check`, `survey`, `profiles` and config tiers arrive with later
tasks. PATH and every DIR are absolute.
SIZE: 50G, 80GiB, 500MB, 10%. DUR: 30s, 10m, 2h, 7d, 1w (m is minutes).
The CLI contract is docs/reference.md.
";

/// What the command line asks for. Kept separate from acting on it so the
/// decision is a pure function of the arguments.
#[derive(Debug, PartialEq, Eq)]
enum Invocation {
    Version,
    Help,
    Ensure(EnsureArgs),
    /// A command of the contract that this build does not have yet.
    NotYet {
        command: String,
        task: &'static str,
    },
    UsageError(String),
}

/// `ensure`'s arguments, checked for syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EnsureArgs {
    min_free: Size,
    target_free: Option<Size>,
    /// `None`: `--timeout 0`, no deadline.
    timeout: Option<Duration>,
    /// `None`: `--tally-budget 0`, no limit (the default is derived from
    /// the timeout: `executor::default_tally_budget`).
    tally_budget: Option<Duration>,
    select: Option<String>,
    roots: Vec<String>,
    dry_run: bool,
    json: bool,
    config: Option<OsString>,
    allow_root: bool,
    path: PathBuf,
}

/// The commands of docs/reference.md this build does not run yet, and
/// the task that brings each.
const NOT_YET: [(&str, &str); 4] = [
    ("watch", "task-32"),
    ("check", "task-15"),
    ("survey", "task-38"),
    ("profiles", "task-17"),
];

fn parse(args: &[OsString]) -> Invocation {
    match args {
        [flag] if flag == "--version" || flag == "-V" => Invocation::Version,
        [flag] if flag == "--help" || flag == "-h" => Invocation::Help,
        [] => Invocation::UsageError("no command given".to_owned()),
        [command, rest @ ..] if command == "ensure" => match parse_ensure(rest) {
            Ok(args) => Invocation::Ensure(args),
            Err(why) => Invocation::UsageError(format!("ensure: {why}")),
        },
        [command, ..] => match NOT_YET.iter().find(|(name, _)| command == name) {
            Some((name, task)) => Invocation::NotYet {
                command: (*name).to_owned(),
                task,
            },
            None => Invocation::UsageError(format!(
                "unknown command or arguments: {}",
                args.iter()
                    .map(|a| a.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" ")
            )),
        },
    }
}

/// Text of a flag's value; sizes, durations and expressions are text.
fn text(value: &OsStr) -> Result<&str, String> {
    value
        .to_str()
        .ok_or_else(|| "the value is not UTF-8".to_owned())
}

fn parse_ensure(args: &[OsString]) -> Result<EnsureArgs, String> {
    let mut min_free = None;
    let mut target_free = None;
    let mut timeout = None;
    let mut tally_budget = None;
    let mut select = None;
    let mut roots = Vec::new();
    let mut config = None;
    let (mut dry_run, mut json, mut allow_root, mut no_index) = (false, false, false, false);
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let Some(flag) = arg.to_str().filter(|a| a.starts_with('-')) else {
            paths.push(PathBuf::from(arg));
            continue;
        };
        let (name, inline) = match flag.split_once('=') {
            Some((name, value)) => (name, Some(OsString::from(value))),
            None => (flag, None),
        };
        let switch = |set: &mut bool| -> Result<(), String> {
            if inline.is_some() {
                return Err(format!("{name} takes no value"));
            }
            if std::mem::replace(set, true) {
                return Err(format!("{name} is given twice"));
            }
            Ok(())
        };
        match name {
            "--dry-run" => switch(&mut dry_run)?,
            "--json" => switch(&mut json)?,
            "--allow-root" => switch(&mut allow_root)?,
            "--no-index" => switch(&mut no_index)?,
            "--min-free" | "--target-free" | "--timeout" | "--tally-budget" | "--select"
            | "--roots" | "--config" => {
                let value = match inline {
                    Some(value) => value,
                    None => rest
                        .next()
                        .cloned()
                        .ok_or_else(|| format!("{name} needs a value"))?,
                };
                let value = &value;
                match name {
                    "--min-free" => set(&mut min_free, name, text(value).and_then(parse_size))?,
                    "--target-free" => {
                        set(&mut target_free, name, text(value).and_then(parse_size))?;
                    }
                    "--timeout" => set(&mut timeout, name, text(value).and_then(zero_or_duration))?,
                    "--tally-budget" => {
                        set(
                            &mut tally_budget,
                            name,
                            text(value).and_then(zero_or_duration),
                        )?;
                    }
                    "--select" => set(&mut select, name, text(value).map(str::to_owned))?,
                    "--roots" => {
                        roots.push(text(value).map_err(|e| format!("{name}: {e}"))?.to_owned());
                    }
                    _ => set(&mut config, name, Ok(value.clone()))?,
                }
            }
            _ => return Err(format!("unknown flag {name}")),
        }
    }
    let min_free = min_free.ok_or("--min-free SIZE is required")?;
    let path = match paths.as_slice() {
        [path] => path.clone(),
        [] => return Err("PATH is required: the directory the next job writes to".to_owned()),
        more => {
            return Err(format!(
                "one PATH, not {}: {}",
                more.len(),
                more.iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
    };
    if !path.is_absolute() {
        return Err(format!(
            "PATH {} is not absolute: name the directory exactly (there is no default and no \
             current directory, so a hook started elsewhere cannot free the wrong disk)",
            path.display()
        ));
    }
    if !roots.is_empty() && select.is_none() {
        return Err("--roots is only valid together with --select".to_owned());
    }
    let timeout = match timeout {
        None => Some(DEFAULT_TIMEOUT),
        Some(Duration::ZERO) => None,
        Some(t) => Some(t),
    };
    Ok(EnsureArgs {
        min_free,
        target_free,
        timeout,
        tally_budget: match tally_budget {
            None => Some(executor::default_tally_budget(timeout)),
            Some(Duration::ZERO) => None,
            Some(t) => Some(t),
        },
        select,
        roots,
        dry_run,
        json,
        config,
        allow_root,
        path,
    })
}

/// A duration flag whose `0` means "none" (`--timeout 0`, `--tally-budget
/// 0`, docs/reference.md): exactly `0` is accepted bare; every other value
/// is a duration with its unit (a bare `5` stays an error).
fn zero_or_duration(text: &str) -> Result<Duration, String> {
    if text == "0" {
        Ok(Duration::ZERO)
    } else {
        parse_duration(text)
    }
}

/// Fills a flag's slot with its parsed value: a second occurrence or a
/// malformed value is a usage error naming the flag.
fn set<T>(slot: &mut Option<T>, name: &str, value: Result<T, String>) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("{name} is given twice"));
    }
    *slot = Some(value.map_err(|e| format!("{name}: {e}"))?);
    Ok(())
}

/// Startup checks for every invocation that may touch the machine:
/// `--version` and `--help` are exempt (they only print), and so must be
/// `check`, which is machine-independent (docs/reference.md, check).
fn startup() -> Result<LinuxHost, String> {
    let host = LinuxHost::new().map_err(|e| e.to_string())?;
    diskgc_core::kernel::require_supported(&host).map_err(|e| e.to_string())?;
    Ok(host)
}

/// The configuration this run uses (docs/reference.md, Configuration
/// file): `None` when none is found and `--select` makes one optional.
fn load_config(host: &LinuxHost, args: &EnsureArgs) -> Result<Option<Config>, String> {
    let env = Env {
        diskgc_config: std::env::var_os("DISKGC_CONFIG"),
        xdg_config_home: std::env::var_os("XDG_CONFIG_HOME"),
        home: std::env::var_os("HOME"),
    };
    let lookup = config::lookup(args.config.as_deref(), &env)?;
    for path in lookup.paths() {
        match host.read_config_file(path, MAX_CONFIG_BYTES) {
            Ok(bytes) => {
                return config::parse(path, &bytes)
                    .map(Some)
                    .map_err(|e| e.to_string());
            }
            Err(e) if matches!(lookup, Lookup::Search(_)) && is_absent(&e) => continue,
            Err(e) => return Err(format!("config {}: {e}", path.display())),
        }
    }
    if args.select.is_some() {
        return Ok(None);
    }
    Err(format!(
        "no configuration file found (tried {}); ensure without --select needs one",
        lookup
            .paths()
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// A configuration file that is not there (as opposed to one that cannot
/// be read, which is an error, never a reason to try the next file).
fn is_absent(e: &diskgc_core::host::HostError) -> bool {
    e.is(Errno::ENOENT) || e.is(Errno::ENOTDIR)
}

/// The tier `--select` makes, against the configuration's library or the
/// built-ins alone.
fn tier(args: &EnsureArgs, config: Option<&Config>, uid: u32) -> Result<Tier, String> {
    let Some(expr) = &args.select else {
        return Err(format!(
            "config {}: ensure without --select runs the config's groups, which this build \
             does not do yet (task-15, task-20); give --select",
            config.map_or_else(|| "?".to_owned(), |c| c.path.display().to_string())
        ));
    };
    let builtin;
    let library = match config {
        Some(config) => &config.library,
        None => {
            builtin = Library::builtin()
                .map_err(|e| format!("the built-in profiles do not load (a bug): {e}"))?;
            &builtin
        }
    };
    let roots = (!args.roots.is_empty()).then_some(args.roots.as_slice());
    Tier::select(expr, library, roots, uid)
}

/// Runs `ensure`; every error is already in the report.
fn ensure(args: &EnsureArgs) -> Report {
    // Every error is logged where it is recorded: here for those found
    // before the executor runs, by the executor for its own.
    let fail = |why: String| {
        log(&format!("error: {why}"));
        Report::new(Status::UsageError, Some(&args.path)).usage_error(why)
    };
    let host = match startup() {
        Ok(host) => host,
        Err(why) => return fail(why),
    };
    let uid = match executor::privileges(&host, args.allow_root) {
        Ok(uid) => uid,
        Err(why) => return fail(why),
    };
    let config = match load_config(&host, args) {
        Ok(config) => config,
        Err(why) => return fail(why),
    };
    if let Some(config) = &config {
        log(&format!("config {}", config.path.display()));
        if config.has_groups {
            log("its groups are replaced by --select (and not validated by this build)");
        }
    }
    let tier = match tier(args, config.as_ref(), uid) {
        Ok(tier) => tier,
        Err(why) => return fail(why),
    };
    let request = Request {
        path: args.path.clone(),
        min_free: args.min_free,
        target_free: args.target_free,
        timeout: args.timeout,
        tally_budget: args.tally_budget,
        tier,
    };
    let mut progress = |line: &str| log(line);
    if args.dry_run {
        // A plain Host: the dry run has no way to reach a deleting method.
        executor::dry_run(&host, &request, uid, &mut progress)
    } else {
        executor::ensure(&host, &request, uid, &mut progress)
    }
}

fn log(line: &str) {
    eprintln!("diskgc: {line}");
}

/// Prints the result and returns the exit code (errors were logged as
/// they were recorded).
fn finish(report: &Report, json: bool) -> ExitCode {
    if json {
        println!("{}", report.to_json());
    } else if let Some(summary) = report.summary() {
        println!("{summary}");
    }
    ExitCode::from(report.exit_code())
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    // `--json` holds for every exit code, usage errors included
    // (docs/reference.md, `ensure`): decided before the command line is
    // understood.
    let json = args.first().is_some_and(|c| c == "ensure") && args.iter().any(|a| a == "--json");
    match parse(&args) {
        Invocation::Version => {
            println!("diskgc {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Invocation::Help => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Invocation::Ensure(args) => finish(&ensure(&args), args.json),
        Invocation::NotYet { command, task } => {
            eprintln!("diskgc: {command} is not implemented in this build yet ({task})");
            ExitCode::from(EXIT_USAGE)
        }
        Invocation::UsageError(reason) => {
            // The kernel check comes first, as for every command that may
            // touch the machine: an old kernel is the reason to report.
            let reason = match startup() {
                Err(why) => why,
                Ok(_) => reason,
            };
            if json {
                eprintln!("diskgc: error: {reason}");
                println!(
                    "{}",
                    Report::new(Status::UsageError, None)
                        .usage_error(reason)
                        .to_json()
                );
            } else {
                eprint!("diskgc: {reason}\n{USAGE}");
            }
            ExitCode::from(EXIT_USAGE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn ensure_args(list: &[&str]) -> Result<EnsureArgs, String> {
        match parse(&args(&[&["ensure"], list].concat())) {
            Invocation::Ensure(a) => Ok(a),
            Invocation::UsageError(e) => Err(e),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn version_flags_are_recognised() {
        assert_eq!(parse(&args(&["--version"])), Invocation::Version);
        assert_eq!(parse(&args(&["-V"])), Invocation::Version);
    }

    #[test]
    fn extra_arguments_are_a_usage_error() {
        assert!(matches!(
            parse(&args(&["--version", "ensure"])),
            Invocation::UsageError(_)
        ));
        assert!(matches!(parse(&args(&[])), Invocation::UsageError(_)));
        assert!(matches!(
            parse(&args(&["prune"])),
            Invocation::UsageError(_)
        ));
    }

    #[test]
    fn commands_of_later_tasks_say_so() {
        assert_eq!(
            parse(&args(&["check"])),
            Invocation::NotYet {
                command: "check".into(),
                task: "task-15"
            }
        );
    }

    #[test]
    fn ensure_parses_every_flag_in_both_spellings() {
        let a = ensure_args(&[
            "--min-free",
            "50G",
            "--target-free=80GiB",
            "--timeout",
            "2m",
            "--tally-budget=10s",
            "--no-index",
            "--select",
            "profile:cargo-target && filter:idle-1d",
            "--roots",
            "/b",
            "--roots=/c",
            "--dry-run",
            "--json",
            "--config",
            "/etc/x.yaml",
            "/b",
        ])
        .expect("parses");
        assert_eq!(a.min_free, Size::Bytes(50 << 30));
        assert_eq!(a.target_free, Some(Size::Bytes(80 << 30)));
        assert_eq!(a.timeout, Some(Duration::from_secs(120)));
        assert_eq!(a.roots, ["/b", "/c"]);
        assert_eq!(a.config.as_deref(), Some(OsStr::new("/etc/x.yaml")));
        assert!(a.dry_run && a.json && !a.allow_root);
        assert_eq!(a.path, PathBuf::from("/b"));
        let d = ensure_args(&["--min-free", "10%", "/p"]).expect("parses");
        assert_eq!(d.timeout, Some(DEFAULT_TIMEOUT));
        assert_eq!(d.min_free, Size::Percent(10));
        let none = ensure_args(&["--min-free", "1G", "--timeout", "0s", "/p"]).expect("parses");
        assert_eq!(none.timeout, None);
        assert_eq!(none.tally_budget, Some(Duration::from_secs(30)));
        assert_eq!(d.tally_budget, Some(Duration::from_secs(30)));
        let quarter = ensure_args(&["--min-free", "1G", "--timeout", "1m", "/p"]).expect("parses");
        assert_eq!(quarter.tally_budget, Some(Duration::from_secs(15)));
    }

    /// The doc spells "no deadline" and "no limit" as a bare `0`
    /// (docs/reference.md, `ensure`): accepted for exactly `0`, while any
    /// other bare number stays a usage error.
    #[test]
    fn a_bare_zero_means_none_for_timeout_and_tally_budget() {
        let a = ensure_args(&[
            "--min-free",
            "1G",
            "--timeout",
            "0",
            "--tally-budget",
            "0",
            "/p",
        ])
        .expect("the documented spelling parses");
        assert_eq!(a.timeout, None);
        assert_eq!(a.tally_budget, None);
        let b = ensure_args(&["--min-free", "1G", "--timeout=0", "--tally-budget=0", "/p"])
            .expect("parses");
        assert_eq!((b.timeout, b.tally_budget), (None, None));
        for bare in ["00", "5", "0 ", "-0"] {
            let error =
                ensure_args(&["--min-free", "1G", "--timeout", bare, "/p"]).expect_err(bare);
            assert!(
                error.starts_with("ensure: --timeout: "),
                "{bare:?}: {error}"
            );
        }
    }

    /// AC #4 of task-10 (command-line half): a bare number, a month
    /// duration, and every other malformed or missing argument is a usage
    /// error (exit 2).
    #[test]
    fn bare_numbers_months_and_malformed_arguments_are_usage_errors() {
        for (list, needle) in [
            (&["--min-free", "50", "/p"][..], "bare number"),
            (
                &["--min-free", "50G", "--target-free", "80", "/p"],
                "bare number",
            ),
            (&["--min-free", "50G", "--timeout", "1M", "/p"], "no months"),
            (
                &["--min-free", "50G", "--timeout", "1mo", "/p"],
                "no months",
            ),
            (
                &["--min-free", "50G", "--tally-budget", "30", "/p"],
                "bare number",
            ),
            (
                &["--min-free", "50G", "--timeout", "5", "/p"],
                "bare number",
            ),
            (&["/p"], "--min-free SIZE is required"),
            (&["--min-free", "50G"], "PATH is required"),
            (&["--min-free", "50G", "/a", "/b"], "one PATH, not 2"),
            (&["--min-free", "50G", "rel"], "not absolute"),
            (
                &["--min-free", "50G", "--roots", "/r", "/p"],
                "only valid together with --select",
            ),
            (
                &["--min-free", "50G", "--min-free", "60G", "/p"],
                "given twice",
            ),
            (
                &["--min-free", "50G", "--json", "--json", "/p"],
                "given twice",
            ),
            (
                &["--min-free", "50G", "--dry-run=yes", "/p"],
                "takes no value",
            ),
            (
                &["--min-free", "50G", "--frobnicate", "/p"],
                "unknown flag --frobnicate",
            ),
            (&["--min-free"], "needs a value"),
        ] {
            let error = ensure_args(list).expect_err(&format!("{list:?}"));
            assert!(error.contains(needle), "{list:?}: {error}");
            assert!(error.starts_with("ensure: "), "{error}");
        }
    }
}
