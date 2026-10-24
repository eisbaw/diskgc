//! The `diskgc` binary: CLI and output, wiring `diskgc-core` to `LinuxHost`
//! (docs/reference.md is the contract).
//!
//! So far: `ensure` with `--select` or configured groups, `--dry-run`
//! and `--json` (task-10), the dry run of one kind `fs` tier (task-68),
//! `profiles list`, `show NAME` and `test`, `check`, plus `--version`
//! and `--help`, whose Profiles section is rendered from the built-in
//! library. The other commands exit 2 saying which task brings them.
//!
//! The flow of `ensure`, every step before the next:
//!
//! 1. the command line ([`parse`]: pure; sizes and durations checked);
//! 2. startup: `LinuxHost` (its `/proc` checks) and the minimum kernel;
//! 3. the configuration (lookup, read, all of `version: 1` checked) and
//!    either the `--select` tier or all configured tiers, checked against it (static
//!    validation: nothing on the machine was touched yet);
//! 4. privileges (root refused without `--allow-root`);
//! 5. the executor: `dry_run` (given the host as a plain `Host`) or
//!    `ensure` (as a `Delete`, which asks the sandbox gate first).
//!
//! `profiles list | show | test` and `check` read the configuration without
//! host startup and print the library's profiles or checked tiers.
//!
//! stdout carries only the result (`--json`: exactly one object when stdout
//! accepts the write); stderr carries progress logs and errors.

use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use diskgc_core::config::{self, Config, Env, Lookup, MAX_CONFIG_BYTES};
use diskgc_core::executor;
use diskgc_core::host::Errno;
use diskgc_core::invocation::{self, Catalog, ConfigFile, ConfigUse, EnsureOptions, Platform};
use diskgc_core::kernel::KernelError;
use diskgc_core::profile::{Library, describe};
use diskgc_core::report::json::Json;
use diskgc_core::report::{ErrorCause, ErrorEntry, Report, Status};
use diskgc_core::units::{parse_duration, parse_size};
use diskgc_linux::LinuxHost;
use diskgc_linux::host::{read_config_file, read_root_config_file};

/// Exit code for usage, config and provisioning errors, including a kernel
/// older than the minimum (docs/reference.md, Exit codes).
const EXIT_USAGE: u8 = 2;

/// A result that could not be written is a run error even if the operation
/// itself had another result: callers cannot rely on a missing report.
const EXIT_OUTPUT_ERROR: u8 = 3;

/// `--timeout`'s default (docs/reference.md, `ensure`).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

const USAGE: &str = "\
usage: diskgc ensure --min-free SIZE [--target-free SIZE] [--timeout DUR]
                     [--tally-budget DUR] [--no-index] [--select EXPR] [--roots DIR]...
                     [--dry-run] [--json] [--config FILE] [--allow-root] PATH
       diskgc profiles list [--json] [--config FILE]
       diskgc profiles show NAME [--json] [--config FILE]
       diskgc profiles test [--json] [--config FILE]
       diskgc check [--config FILE]
       diskgc --version
       diskgc --help

Start here, with a dry run (it reads only and deletes nothing):
  diskgc ensure --dry-run --json --min-free 50G \\
      --select 'profile:cargo-target && filter:idle-3d' --roots DIR PATH
PATH: the directory whose filesystem must have --min-free available (that
  filesystem is measured and freed); `roots: required` profiles look only
  in --roots DIR. --min-free is required, also with --dry-run.
Without --dry-run, ensure DELETES what the selection finds and the safety
  checks allow, until the goal is met.
--json prints exactly one JSON object on stdout for every exit when stdout
  accepts the write; --help prints the help as text.
Exit codes (ensure): 0 met (--dry-run: 0 with status dry_run; read
  target_met), 1 exhausted, 2 usage/config, 3 error, 75 another reclaim
  holds the lock (retry later), 124 deadline, 130/143 signal.

This build runs `ensure --select` or configured groups with ordered or random
tier ranking and oldest or best-fit candidate order. `watch`
and `survey` arrive with later tasks. PATH and every DIR are absolute.
SIZE: 50G, 80GiB, 500MB, 10%. DUR: 30s, 10m, 2h, 7d, 1w (m is minutes).
The full contract is docs/reference.md in diskgc's source tree.
";

/// What the command line asks for. Kept separate from acting on it so the
/// decision is a pure function of the arguments.
#[derive(Debug, PartialEq, Eq)]
enum Invocation {
    Version,
    Help,
    Ensure(EnsureArgs),
    Profiles(ProfilesArgs),
    Check(Option<OsString>),
    /// A command of the contract that this build does not have yet.
    NotYet {
        command: String,
        task: &'static str,
    },
    UsageError(String),
}

/// `ensure`'s arguments, checked for syntax: the composition's options
/// plus where the configuration comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EnsureArgs {
    options: EnsureOptions,
    config: Option<OsString>,
}

/// `profiles list | show NAME | test`'s arguments. Whether the output is JSON is
/// [`json_requested`]'s, for every command.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProfilesArgs {
    command: ProfilesCommand,
    config: Option<OsString>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProfilesCommand {
    List,
    Show(String),
    Test,
}

/// The commands of docs/reference.md this build does not run yet, and
/// the task that brings each.
const NOT_YET: [(&str, &str); 2] = [("watch", "task-32"), ("survey", "task-38")];

/// Whether the command line asks for JSON: any argument `--json`, or
/// `--json=...` (then a usage error). Decided on the raw arguments, before
/// they are understood, so that every exit prints one JSON object, a usage
/// error included, also when `--json` ended up as another flag's value
/// (`--config --json`) (docs/reference.md, `ensure` and `profiles`).
fn json_requested(args: &[OsString]) -> bool {
    args.iter()
        .any(|a| a == "--json" || a.as_encoded_bytes().starts_with(b"--json="))
}

fn parse(args: &[OsString]) -> Invocation {
    // Help wins wherever it is asked for (`profiles list --help`).
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return Invocation::Help;
    }
    match args {
        [flag] if flag == "--version" || flag == "-V" => Invocation::Version,
        [first, second]
            if (first == "--version" || first == "-V") && second == "--json"
                || first == "--json" && (second == "--version" || second == "-V") =>
        {
            Invocation::Version
        }
        [] => Invocation::UsageError("no command given".to_owned()),
        [command, rest @ ..] if command == "ensure" => match parse_ensure(rest) {
            Ok(args) => Invocation::Ensure(args),
            Err(why) => Invocation::UsageError(format!("ensure: {why}")),
        },
        [command, rest @ ..] if command == "profiles" => parse_profiles(rest),
        [command, rest @ ..] if command == "check" => match parse_check(rest) {
            Ok(config) => Invocation::Check(config),
            Err(why) => Invocation::UsageError(format!("check: {why}")),
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

fn parse_check(args: &[OsString]) -> Result<Option<OsString>, String> {
    let mut config = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.to_str() {
            Some("--config") => set(
                &mut config,
                "--config",
                rest.next()
                    .cloned()
                    .ok_or("--config needs a value".to_owned()),
            )?,
            Some(s) if s.starts_with("--config=") => {
                set(&mut config, "--config", Ok(OsString::from(&s[9..])))?
            }
            _ => return Err(format!("unknown argument {}", arg.to_string_lossy())),
        }
    }
    Ok(config)
}

/// `profiles list | show NAME | test` (docs/reference.md, `profiles`
/// command), flags before or after the words.
fn parse_profiles(args: &[OsString]) -> Invocation {
    let usage = |why: String| Invocation::UsageError(format!("profiles: {why}"));
    let (mut json, mut config, mut words) = (false, None, Vec::new());
    let mut rest = args.iter();
    let mut flags = || -> Result<(), String> {
        while let Some(arg) = rest.next() {
            let Some(flag) = arg.to_str().filter(|a| a.starts_with('-')) else {
                words.push(text(arg)?.to_owned());
                continue;
            };
            match flag.split_once('=') {
                // Validated only: the output is `json_requested`'s.
                None if flag == "--json" => {
                    if std::mem::replace(&mut json, true) {
                        return Err("--json is given twice".to_owned());
                    }
                }
                Some(("--json", _)) => return Err("--json takes no value".to_owned()),
                None if flag == "--config" => {
                    let value = rest.next().ok_or("--config needs a value")?;
                    set(&mut config, flag, Ok(value.clone()))?;
                }
                Some(("--config", value)) => set(&mut config, "--config", Ok(value.into()))?,
                _ => return Err(format!("unknown flag {flag}")),
            }
        }
        Ok(())
    };
    if let Err(why) = flags() {
        return usage(why);
    }
    let Some((sub, names)) = words.split_first() else {
        return usage("list, show NAME or test is required".to_owned());
    };
    let command = match (sub.as_str(), names) {
        ("test", []) => ProfilesCommand::Test,
        ("test", more) => return usage(format!("test takes no NAME: {}", more.join(" "))),
        ("list", []) => ProfilesCommand::List,
        ("list", more) => return usage(format!("list takes no NAME: {}", more.join(" "))),
        ("show", [name]) => ProfilesCommand::Show(name.clone()),
        ("show", []) => return usage("show needs a NAME".to_owned()),
        ("show", more) => return usage(format!("show takes one NAME, not {}", more.len())),
        (other, _) => {
            return usage(format!(
                "unknown subcommand {other}: list, show NAME or test"
            ));
        }
    };
    Invocation::Profiles(ProfilesArgs { command, config })
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
        options: EnsureOptions {
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
            allow_root,
            path,
            // A model-test seam: never set from the command line or config.
            test_entry_budget: None,
        },
        config,
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
enum StartupFailure {
    Host(String),
    UnsupportedKernel(String),
}

impl StartupFailure {
    fn status(&self) -> Status {
        match self {
            Self::Host(_) => Status::Error,
            Self::UnsupportedKernel(_) => Status::UsageError,
        }
    }

    fn cause(&self) -> ErrorCause {
        match self {
            Self::Host(_) => ErrorCause::Host,
            Self::UnsupportedKernel(_) => ErrorCause::Usage,
        }
    }

    fn message(self) -> String {
        match self {
            Self::Host(message) | Self::UnsupportedKernel(message) => message,
        }
    }
}

fn startup() -> Result<LinuxHost, StartupFailure> {
    let host = LinuxHost::new().map_err(|e| StartupFailure::Host(e.to_string()))?;
    diskgc_core::kernel::require_supported(&host).map_err(kernel_failure)?;
    Ok(host)
}

fn kernel_failure(error: KernelError) -> StartupFailure {
    match error {
        KernelError::Host(_) => StartupFailure::Host(error.to_string()),
        KernelError::TooOld { .. } | KernelError::Unparseable { .. } => {
            StartupFailure::UnsupportedKernel(error.to_string())
        }
    }
}

fn failed_report(
    status: Status,
    cause: ErrorCause,
    message: String,
    path: Option<&Path>,
) -> Report {
    let mut report = Report::new(status, path);
    report.errors.push(ErrorEntry {
        tier: String::new(),
        profile: String::new(),
        candidate: String::new(),
        cause,
        message,
    });
    report
}

/// The catalog this build ships ([`Catalog`]): the opt-in feature builds
/// the proposed 0.1 release's.
const CATALOG: Catalog = if cfg!(feature = "proposed-release-01-catalog") {
    Catalog::ProposedRelease01
} else {
    Catalog::Development
};

/// The configuration file (docs/reference.md, Configuration file), given
/// by `flag` (`--config`), `$DISKGC_CONFIG` or found by the search: its
/// bytes, or the paths tried when the search found none (a given file that
/// is missing is an error, never a fallback). Parsing it, under
/// [`CATALOG`], and whether a missing one is an error, are the command's.
fn read_config(flag: Option<&OsStr>, strict_modes: bool) -> Result<ConfigFile, String> {
    let env = Env {
        diskgc_config: std::env::var_os("DISKGC_CONFIG"),
        xdg_config_home: std::env::var_os("XDG_CONFIG_HOME"),
        home: std::env::var_os("HOME"),
    };
    let lookup = config::lookup(flag, &env)?;
    let read = if strict_modes {
        read_root_config_file
    } else {
        read_config_file
    };
    for path in lookup.paths() {
        match read(path, MAX_CONFIG_BYTES) {
            Ok(bytes) => {
                return Ok(ConfigFile::Read {
                    path: path.to_path_buf(),
                    bytes,
                });
            }
            Err(e) if matches!(lookup, Lookup::Search(_)) && is_absent(&e) => continue,
            Err(e) => return Err(format!("config {}: {e}", path.display())),
        }
    }
    Ok(ConfigFile::NotFound {
        tried: lookup.paths().into_iter().map(Path::to_path_buf).collect(),
    })
}

/// [`read_config`], parsed under [`CATALOG`] for `profiles` and `check`:
/// the configuration, or the paths tried when none was found.
fn load_config(
    flag: Option<&OsStr>,
    use_: ConfigUse,
) -> Result<Result<Config, Vec<PathBuf>>, String> {
    match read_config(flag, false)? {
        ConfigFile::Read { path, bytes } => CATALOG.config(&path, &bytes, use_).map(Ok),
        ConfigFile::NotFound { tried } => Ok(Err(tried)),
    }
}

/// A configuration file that is not there (as opposed to one that cannot
/// be read, which is an error, never a reason to try the next file).
fn is_absent(e: &diskgc_core::host::HostError) -> bool {
    e.is(Errno::ENOENT) || e.is(Errno::ENOTDIR)
}

/// Parse the operator's reproducible seed without accepting signs or spaces.
fn parse_configured_seed(value: &OsStr) -> Result<u64, String> {
    let value = value
        .to_str()
        .ok_or("DISKGC_SEED must be an unsigned decimal integer")?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("DISKGC_SEED must be an unsigned decimal integer".to_owned());
    }
    value
        .parse::<u64>()
        .map_err(|_| "DISKGC_SEED is larger than u64".to_owned())
}

/// The seed of a `ranking: random` run: `DISKGC_SEED`, or a fresh random
/// one. Drawn once per run so the report reproduces every group's order.
fn draw_seed() -> Result<u64, String> {
    if let Some(value) = std::env::var_os("DISKGC_SEED") {
        return parse_configured_seed(&value);
    }
    let mut bytes = [0; 8];
    getrandom::fill(&mut bytes).map_err(|error| format!("could not draw random seed: {error}"))?;
    Ok(u64::from_le_bytes(bytes))
}

/// Runs `ensure`; every error is already in the report. The decisions are
/// `diskgc_core::invocation::ensure`'s; this wires it to `LinuxHost`, the
/// configuration file's bytes and the build's catalog.
fn ensure(args: &EnsureArgs) -> Report {
    // Every error is logged where it is recorded: here for those found
    // before the composition runs, by the composition for its own.
    let fail = |cause: ErrorCause, why: String| {
        log(&format!("error: {why}"));
        Report::new(Status::UsageError, Some(&args.options.path)).usage_error_with_cause(cause, why)
    };
    let fail_host = |why: String| {
        log(&format!("error: {why}"));
        failed_report(
            Status::Error,
            ErrorCause::Host,
            why,
            Some(&args.options.path),
        )
    };
    if let Err(error) = LinuxHost::install_interrupt_handlers() {
        return fail_host(format!("install signal handlers: {error}"));
    }
    let host = match startup() {
        Ok(host) => host,
        Err(StartupFailure::Host(why)) => return fail_host(why),
        Err(StartupFailure::UnsupportedKernel(why)) => return fail(ErrorCause::Usage, why),
    };
    let config = match read_config(args.config.as_deref(), args.options.allow_root) {
        Ok(config) => config,
        Err(why) => return fail(ErrorCause::Config, why),
    };
    let builtins = || {
        Library::builtin().map_err(|e| format!("the built-in profiles do not load (a bug): {e}"))
    };
    let platform = Platform {
        catalog: CATALOG,
        draw_seed: &draw_seed,
        builtins: &builtins,
    };
    invocation::ensure(
        &host,
        &args.options,
        &config,
        &platform,
        &mut log,
        &mut |line| log(&operator_progress(line)),
    )
}

/// The built-in profiles of [`CATALOG`] alone; an error is a bug in the
/// shipped YAML (the tests load it).
fn builtin_library() -> Result<Library, String> {
    let library = Library::builtin()
        .map_err(|e| format!("the built-in profiles do not load (a bug): {e}"))?;
    CATALOG.library(library)
}

/// `profiles list | show NAME | test`: the text or JSON for stdout, or why not
/// (exit 2). The library is the built-ins plus the configuration's user
/// profiles; without a configuration file (none found by the search), the
/// built-ins alone. A given file that is missing, or any file that does
/// not load, is an error, as for `ensure`.
enum ProfilesFailure {
    Config(String),
    UnknownName(String),
}

impl ProfilesFailure {
    fn cause(&self) -> ErrorCause {
        match self {
            Self::Config(_) => ErrorCause::Config,
            Self::UnknownName(_) => ErrorCause::Usage,
        }
    }

    fn message(self) -> String {
        match self {
            Self::Config(message) | Self::UnknownName(message) => message,
        }
    }
}

fn profiles(args: &ProfilesArgs, json: bool) -> Result<(String, bool), ProfilesFailure> {
    let use_ = if matches!(&args.command, ProfilesCommand::Test) {
        ConfigUse::ProfileTest
    } else {
        ConfigUse::Run
    };
    let config = load_config(args.config.as_deref(), use_)
        .map_err(ProfilesFailure::Config)?
        .ok();
    let config_path = config.as_ref().map(|c| c.path.as_path());
    if let Some(path) = config_path {
        log(&format!("config {}", path.display()));
    }
    let builtin;
    let library = match &config {
        Some(config) => &config.library,
        None => {
            builtin = builtin_library().map_err(ProfilesFailure::Config)?;
            &builtin
        }
    };
    Ok(match (&args.command, json) {
        (ProfilesCommand::List, false) => (describe::list_text(library), false),
        (ProfilesCommand::List, true) => (
            describe::list_json(library, config_path).to_text() + "\n",
            false,
        ),
        (ProfilesCommand::Show(name), json) => {
            let profile = library.get(name).ok_or_else(|| {
                ProfilesFailure::UnknownName(describe::unknown_name(library, name))
            })?;
            if json {
                (
                    describe::show_json(profile, config_path).to_text() + "\n",
                    false,
                )
            } else {
                (describe::show_yaml(profile, config_path), false)
            }
        }
        (ProfilesCommand::Test, json) => {
            let failures = diskgc_core::profile::examples::failures(library);
            let failed = !failures.is_empty();
            if json {
                let object = Json::Obj(vec![
                    (
                        "config",
                        Json::opt(config_path, |p| Json::str(p.to_string_lossy())),
                    ),
                    (
                        "status",
                        Json::str(if failed { "failed" } else { "passed" }),
                    ),
                    ("exit_code", Json::U64(u64::from(failed))),
                    ("profiles", Json::U64(library.profiles().count() as u64)),
                    (
                        "failures",
                        Json::Arr(failures.iter().map(Json::str).collect()),
                    ),
                ]);
                (object.to_text() + "\n", failed)
            } else if failed {
                (
                    format!(
                        "{} profile example failure(s):\n{}\n",
                        failures.len(),
                        failures.join("\n")
                    ),
                    true,
                )
            } else {
                (
                    format!("{} profiles: examples pass\n", library.profiles().count()),
                    false,
                )
            }
        }
    })
}

/// Machine-independent policy validation. No `LinuxHost`, kernel version
/// check, process scan, path traversal or backend invocation occurs here.
fn check(flag: Option<&OsStr>) -> Result<String, String> {
    let config =
        load_config(flag, ConfigUse::Run)?.map_err(|tried| invocation::not_found(&tried))?;
    for warning in &config.warnings {
        log(&format!("warning: {warning}"));
    }
    let mut out = format!("config {}\n", config.path.display());
    for group in &config.groups {
        for tier in &group.tiers {
            let profiles = tier
                .select
                .expr()
                .terms
                .iter()
                .map(|term| term.source.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!(
                "{}.{}: {} => [{}]\n",
                group.name,
                tier.name,
                tier.select.expr(),
                profiles
            ));
        }
    }
    Ok(out)
}

/// The one JSON object of a failed command other than `ensure` (whose
/// report says it): `status` as `ensure`'s usage-error report has it, the
/// exit code (which `ensure`'s report does not carry: there it is only the
/// process's), and the message (docs/reference.md, `profiles` command).
fn error_json(cause: ErrorCause, why: &str) -> String {
    error_json_status(Status::UsageError, cause, why)
}

fn error_json_status(status: Status, cause: ErrorCause, why: &str) -> String {
    Json::Obj(vec![
        ("status", Json::str(status.as_str())),
        ("exit_code", Json::U64(status.exit_code().into())),
        ("cause", Json::str(cause.as_str())),
        ("error", Json::str(why)),
    ])
    .to_text()
}

fn log(line: &str) {
    eprintln!("diskgc: {line}");
}

/// The JSON tier key stays `--select`; stderr names the work, so an operator
/// does not read every line as a complaint about the command-line flag.
fn operator_progress(line: &str) -> String {
    if let Some(rest) = line.strip_prefix("--select")
        && (rest.starts_with(':') || rest.starts_with(' '))
    {
        format!("selection{rest}")
    } else {
        line.to_owned()
    }
}

/// Write the entire result, including any trailing newline. A failed write
/// or flush overrides the operation's exit code because its report is lost.
/// Reporting the failure to stderr is best effort: that stream can fail too.
fn write_result<W: Write, E: Write>(
    stdout: &mut W,
    stderr: &mut E,
    body: &str,
    exit_code: u8,
) -> ExitCode {
    match stdout
        .write_all(body.as_bytes())
        .and_then(|_| stdout.flush())
    {
        Ok(()) => ExitCode::from(exit_code),
        Err(error) => {
            let _ = writeln!(
                stderr,
                "diskgc: error: could not write result to stdout: {error}"
            );
            ExitCode::from(EXIT_OUTPUT_ERROR)
        }
    }
}

fn output(body: &str, exit_code: u8) -> ExitCode {
    write_result(
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
        body,
        exit_code,
    )
}

/// Write the result and return the operation's exit code when it succeeds.
fn finish(report: &Report, json: bool) -> ExitCode {
    if json {
        output(&format!("{}\n", report.to_json()), report.exit_code())
    } else if let Some(summary) = report.summary() {
        output(&format!("{summary}\n"), report.exit_code())
    } else {
        ExitCode::from(report.exit_code())
    }
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    // `--json` holds for every exit code when stdout works, usage errors
    // included: decided before the command line is understood. `ensure`
    // prints its report; every other command prints `error_json`.
    let json = json_requested(&args);
    let report = args.first().is_some_and(|c| c == "ensure");
    match parse(&args) {
        Invocation::Version => output(&format!("diskgc {}\n", env!("CARGO_PKG_VERSION")), 0),
        Invocation::Help => match builtin_library() {
            Ok(library) => output(&format!("{USAGE}{}", describe::help_section(&library)), 0),
            Err(why) => {
                log(&format!("error: {why}"));
                output(USAGE, EXIT_USAGE)
            }
        },
        Invocation::Ensure(args) => finish(&ensure(&args), json),
        Invocation::Check(config) => match check(config.as_deref()) {
            Ok(out) => output(&out, 0),
            Err(why) => {
                log(&format!("error: {why}"));
                if json {
                    output(
                        &format!("{}\n", error_json(ErrorCause::Config, &why)),
                        EXIT_USAGE,
                    )
                } else {
                    ExitCode::from(EXIT_USAGE)
                }
            }
        },
        Invocation::Profiles(args) => match profiles(&args, json) {
            Ok((out, failed)) => output(&out, u8::from(failed)),
            Err(failure) => {
                let cause = failure.cause();
                let why = failure.message();
                log(&format!("error: {why}"));
                if json {
                    output(&format!("{}\n", error_json(cause, &why)), EXIT_USAGE)
                } else {
                    ExitCode::from(EXIT_USAGE)
                }
            }
        },
        Invocation::NotYet { command, task } => {
            let why = format!("{command} is not implemented in this build yet ({task})");
            eprintln!("diskgc: {why}");
            if json {
                output(
                    &format!("{}\n", error_json(ErrorCause::Usage, &why)),
                    EXIT_USAGE,
                )
            } else {
                ExitCode::from(EXIT_USAGE)
            }
        }
        Invocation::UsageError(reason) => {
            // The kernel check comes first, as for every command that may
            // touch the machine: an old kernel is the reason to report.
            let (reason, cause, status) = if args
                .first()
                .is_some_and(|a| a == "check" || a == "profiles")
            {
                (reason, ErrorCause::Parse, Status::UsageError)
            } else {
                match startup() {
                    Err(failure) => {
                        let status = failure.status();
                        let cause = failure.cause();
                        (failure.message(), cause, status)
                    }
                    Ok(_) => (reason, ErrorCause::Parse, Status::UsageError),
                }
            };
            match (json, report) {
                (true, true) => {
                    eprintln!("diskgc: error: {reason}");
                    output(
                        &format!("{}\n", failed_report(status, cause, reason, None).to_json()),
                        status.exit_code(),
                    )
                }
                (true, false) => {
                    eprintln!("diskgc: error: {reason}");
                    output(
                        &format!("{}\n", error_json_status(status, cause, &reason)),
                        status.exit_code(),
                    )
                }
                (false, _) => {
                    if status == Status::UsageError {
                        eprint!("diskgc: {reason}\n{USAGE}");
                    } else {
                        eprintln!("diskgc: error: {reason}");
                    }
                    ExitCode::from(status.exit_code())
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskgc_core::executor::Tier;
    use diskgc_core::units::Size;

    #[test]
    fn selected_tier_has_a_readable_progress_label() {
        assert_eq!(
            operator_progress("--select: skip in_use /tmp/target"),
            "selection: skip in_use /tmp/target"
        );
        assert_eq!(
            operator_progress("--select /tmp: checking"),
            "selection /tmp: checking"
        );
        assert_eq!(
            operator_progress("builds: checking /tmp/target"),
            "builds: checking /tmp/target"
        );
    }

    #[test]
    fn startup_classifies_kernel_io_separately_from_unsupported_versions() {
        let host = kernel_failure(KernelError::Host(diskgc_core::host::HostError::invalid(
            "kernel_release",
            "/proc/sys/kernel/osrelease",
            "EIO",
        )));
        assert_eq!(host.status(), Status::Error);
        assert_eq!(host.cause(), ErrorCause::Host);
        let old = kernel_failure(KernelError::TooOld {
            release: "5.4.0".to_owned(),
            version: diskgc_core::kernel::KernelVersion { major: 5, minor: 4 },
        });
        assert_eq!(old.status(), Status::UsageError);
        assert_eq!(old.cause(), ErrorCause::Usage);
        let unknown = kernel_failure(KernelError::Unparseable {
            release: "unknown".to_owned(),
        });
        assert_eq!(unknown.status(), Status::UsageError);
        assert_eq!(unknown.cause(), ErrorCause::Usage);
    }

    struct FailingWriter {
        errno: i32,
        on_flush: bool,
    }

    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.on_flush {
                Ok(bytes.len())
            } else {
                Err(io::Error::from_raw_os_error(self.errno))
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.on_flush {
                Err(io::Error::from_raw_os_error(self.errno))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn stdout_write_and_flush_errors_override_the_operation_exit_code() {
        const LINUX_ENOSPC: i32 = 28;
        const LINUX_EPIPE: i32 = 32;
        for (errno, label) in [
            (LINUX_ENOSPC, "No space left on device"),
            (LINUX_EPIPE, "Broken pipe"),
        ] {
            for on_flush in [false, true] {
                for operation_code in [0, EXIT_USAGE] {
                    let mut stdout = FailingWriter { errno, on_flush };
                    let mut stderr = Vec::new();
                    let exit = write_result(&mut stdout, &mut stderr, "result\n", operation_code);
                    assert_eq!(exit, ExitCode::from(EXIT_OUTPUT_ERROR));
                    let message = String::from_utf8(stderr).expect("UTF-8 error message");
                    assert!(
                        message.contains("diskgc: error: could not write result to stdout:"),
                        "{message}"
                    );
                    assert!(message.contains(label), "{message}");
                    assert_eq!(message.lines().count(), 1, "{message}");
                }
            }
        }
    }

    #[test]
    fn successful_stdout_write_preserves_result_and_operation_exit_code() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = write_result(&mut stdout, &mut stderr, "result\n", EXIT_USAGE);
        assert_eq!(exit, ExitCode::from(EXIT_USAGE));
        assert_eq!(stdout, b"result\n");
        assert!(stderr.is_empty());
    }

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
    fn configured_seed_rejects_invalid_operator_input() {
        assert_eq!(parse_configured_seed(OsStr::new("0")).unwrap(), 0);
        assert_eq!(
            parse_configured_seed(OsStr::new("18446744073709551615")).unwrap(),
            u64::MAX
        );
        for invalid in ["", "-1", "+1", " 1", "1 ", "0x2a", "1.5"] {
            assert!(
                parse_configured_seed(OsStr::new(invalid)).is_err(),
                "{invalid:?}"
            );
        }
        assert!(parse_configured_seed(OsStr::new("18446744073709551616")).is_err());
    }

    /// The literal names are deliberate: the catalog is derived from
    /// builtin.yaml's `releases:` (Library::proposed_release_01), and this
    /// list is the second key a widening of the 0.1 catalog must turn
    /// (with TASK-65's sign-off), not a stale copy to derive away.
    #[test]
    fn proposed_catalog_keeps_only_cargo_and_idle_filters_on_every_listing_surface() {
        let library = Library::builtin()
            .expect("built-ins")
            .proposed_release_01()
            .expect("proposed catalog");
        let names: Vec<_> = library
            .profiles()
            .map(|profile| profile.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "cargo-target",
                "idle-10d",
                "idle-1d",
                "idle-1h",
                "idle-2d",
                "idle-30d",
                "idle-3d",
                "idle-7d"
            ]
        );
        for surface in [
            describe::list_text(&library),
            describe::help_section(&library),
        ] {
            assert!(surface.contains("cargo-target"));
            assert!(!surface.contains("docker-build-cache"));
            assert!(!surface.contains("pip-cache"));
            assert!(!surface.contains("pytest-cache"));
        }
        assert!(library.get("docker-build-cache").is_none());
        assert!(library.get("pip-cache").is_none());
        assert!(library.get("pytest-cache").is_none());
    }

    #[test]
    fn proposed_catalog_refuses_excluded_select_before_a_tier_can_reach_a_backend() {
        let all = Library::builtin().expect("built-ins");
        let library = all.clone().proposed_release_01().expect("proposed catalog");
        let roots = ["/tmp".to_owned()];
        for profile in all
            .profiles()
            .filter(|profile| profile.role() == "source" && library.get(&profile.name).is_none())
        {
            let name = profile.name.as_str();
            let error = Tier::select(&format!("profile:{name}"), &library, Some(&roots), 1000)
                .expect_err(name);
            assert!(error.contains(name), "{error}");
        }
        Tier::select(
            "profile:cargo-target && filter:idle-3d",
            &library,
            Some(&roots),
            1000,
        )
        .expect("Cargo remains selectable");
    }

    #[cfg(feature = "proposed-release-01-catalog")]
    #[test]
    fn opt_in_binary_uses_the_narrowed_library_for_help_and_select() {
        let library = builtin_library().expect("binary catalog");
        assert_eq!(library.profiles().count(), 8);
        assert!(library.get("docker-build-cache").is_none());
        let error = Tier::select("profile:docker-build-cache", &library, None, 1000)
            .expect_err("excluded source");
        assert!(error.contains("docker-build-cache"), "{error}");
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
    fn check_parses_only_its_config_flag() {
        assert_eq!(parse(&args(&["check"])), Invocation::Check(None));
        assert_eq!(
            parse(&args(&["check", "--config=/c.yaml"])),
            Invocation::Check(Some("/c.yaml".into()))
        );
        assert!(matches!(
            parse(&args(&["check", "--json"])),
            Invocation::UsageError(_)
        ));
    }

    fn profiles_args(list: &[&str]) -> Result<ProfilesArgs, String> {
        match parse(&args(&[&["profiles"], list].concat())) {
            Invocation::Profiles(a) => Ok(a),
            Invocation::UsageError(e) => Err(e),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn profiles_parses_list_show_and_test_with_their_flags_anywhere() {
        assert_eq!(
            profiles_args(&["list"]),
            Ok(ProfilesArgs {
                command: ProfilesCommand::List,
                config: None
            })
        );
        let show =
            profiles_args(&["show", "--json", "cargo-target", "--config=/c.yaml"]).expect("parses");
        assert_eq!(show.command, ProfilesCommand::Show("cargo-target".into()));
        assert_eq!(show.config.as_deref(), Some(OsStr::new("/c.yaml")));
        let list = profiles_args(&["--json", "--config", "/c.yaml", "list"]).expect("parses");
        assert_eq!(list.command, ProfilesCommand::List);
        assert_eq!(list.config.as_deref(), Some(OsStr::new("/c.yaml")));
        assert_eq!(
            profiles_args(&["test", "--json"]),
            Ok(ProfilesArgs {
                command: ProfilesCommand::Test,
                config: None
            })
        );
    }

    /// Help is asked for anywhere on the line, also after a command.
    #[test]
    fn help_anywhere_is_help() {
        for list in [
            &["--help"][..],
            &["-h"],
            &["profiles", "--help"],
            &["profiles", "list", "--help"],
            &["profiles", "show", "-h"],
            &["ensure", "--help"],
            &["ensure", "--min-free", "50", "-h"],
            &["watch", "--help"],
        ] {
            assert_eq!(parse(&args(list)), Invocation::Help, "{list:?}");
        }
    }

    #[test]
    fn json_is_requested_by_any_json_argument() {
        for (list, want) in [
            (&["ensure", "--json"][..], true),
            (&["profiles", "list", "--json=yes"], true),
            (&["profiles", "list", "--config", "--json"], true),
            (&["--json"], true),
            (&["profiles", "list", "--jsonx"], false),
            (&["profiles", "list", "json"], false),
            (&["profiles", "list"], false),
        ] {
            assert_eq!(json_requested(&args(list)), want, "{list:?}");
        }
    }

    #[test]
    fn profiles_test_parses() {
        assert_eq!(
            parse(&args(&["profiles", "test"])),
            Invocation::Profiles(ProfilesArgs {
                command: ProfilesCommand::Test,
                config: None
            })
        );
    }

    #[test]
    fn malformed_profiles_arguments_are_usage_errors() {
        for (list, needle) in [
            (&[][..], "list, show NAME or test is required"),
            (&["--json"], "list, show NAME or test is required"),
            (&["frob"], "unknown subcommand frob"),
            (&["list", "x"], "list takes no NAME: x"),
            (&["show"], "show needs a NAME"),
            (&["show", "a", "b"], "show takes one NAME, not 2"),
            (&["test", "x"], "test takes no NAME: x"),
            (&["list", "--json", "--json"], "--json is given twice"),
            (&["list", "--json=yes"], "--json takes no value"),
            (&["list", "--config"], "--config needs a value"),
            (
                &["list", "--config", "/a", "--config=/b"],
                "--config is given twice",
            ),
            (&["list", "--all"], "unknown flag --all"),
        ] {
            let error = profiles_args(list).expect_err(&format!("{list:?}"));
            assert!(error.starts_with("profiles: "), "{error}");
            assert!(error.contains(needle), "{list:?}: {error}");
        }
    }

    /// The entry budget's test seam (`EnsureOptions::test_entry_budget`).
    /// The guarantee is structural: `EnsureOptions` is built only here
    /// (with `None`) and by the model's vector runner. This test catches
    /// the obvious ways to break that: a parse that sets it, an
    /// `--entry-budget` flag or an `entry_budget:` configuration key.
    #[test]
    fn the_binary_never_sets_the_test_entry_budget() {
        for line in [
            &["--min-free", "1G", "/b"][..],
            &[
                "--min-free",
                "1G",
                "--target-free",
                "2G",
                "--timeout",
                "0",
                "--tally-budget",
                "5s",
                "--select",
                "profile:cargo-target",
                "--roots",
                "/b",
                "--dry-run",
                "--json",
                "--config",
                "/etc/x.yaml",
                "--allow-root",
                "--no-index",
                "/b",
            ][..],
        ] {
            let parsed = ensure_args(line).expect("valid ensure command line");
            assert_eq!(parsed.options.test_entry_budget, None, "{line:?}");
        }
        assert!(ensure_args(&["--min-free", "1G", "--entry-budget", "5", "/b"]).is_err());
        let config = "version: 1\nentry_budget: 5\ngroups: []\n";
        let error = config::parse(Path::new("/etc/diskgc/config.yaml"), config.as_bytes())
            .expect_err("no configuration key sets the entry budget");
        assert!(error.to_string().contains("entry_budget"), "{error}");
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
        assert_eq!(a.options.min_free, Size::Bytes(50 << 30));
        assert_eq!(a.options.target_free, Some(Size::Bytes(80 << 30)));
        assert_eq!(a.options.timeout, Some(Duration::from_secs(120)));
        assert_eq!(a.options.roots, ["/b", "/c"]);
        assert_eq!(a.config.as_deref(), Some(OsStr::new("/etc/x.yaml")));
        assert!(a.options.dry_run && !a.options.allow_root);
        assert_eq!(a.options.path, PathBuf::from("/b"));
        let d = ensure_args(&["--min-free", "10%", "/p"]).expect("parses");
        assert_eq!(d.options.timeout, Some(DEFAULT_TIMEOUT));
        assert_eq!(d.options.min_free, Size::Percent(10));
        let none = ensure_args(&["--min-free", "1G", "--timeout", "0s", "/p"]).expect("parses");
        assert_eq!(none.options.timeout, None);
        assert_eq!(none.options.tally_budget, Some(Duration::from_secs(30)));
        assert_eq!(d.options.tally_budget, Some(Duration::from_secs(30)));
        let quarter = ensure_args(&["--min-free", "1G", "--timeout", "1m", "/p"]).expect("parses");
        assert_eq!(quarter.options.tally_budget, Some(Duration::from_secs(15)));
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
        assert_eq!(a.options.timeout, None);
        assert_eq!(a.options.tally_budget, None);
        let b = ensure_args(&["--min-free", "1G", "--timeout=0", "--tally-budget=0", "/p"])
            .expect("parses");
        assert_eq!((b.options.timeout, b.options.tally_budget), (None, None));
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
