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

use diskgc_core::config::{self, Config, Env, IpcPolicy, Lookup, MAX_CONFIG_BYTES, Owner, Ranking};
use diskgc_core::executor::{self, Request, SelectionFailure, Tier};
use diskgc_core::host::Errno;
use diskgc_core::kernel::KernelError;
use diskgc_core::profile::{Body, Finds, Library, Roots, Source, describe};
use diskgc_core::report::json::Json;
use diskgc_core::report::{ErrorCause, ErrorEntry, Report, Status};
use diskgc_core::select;
use diskgc_core::units::{Size, parse_duration, parse_size};
use diskgc_linux::LinuxHost;
use diskgc_linux::host::{read_config_file, read_root_config_file, resolve_owner};

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
    config: Option<OsString>,
    allow_root: bool,
    path: PathBuf,
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

/// The configuration file (docs/reference.md, Configuration file), given
/// by `flag` (`--config`), `$DISKGC_CONFIG` or found by the search: the
/// lookup, and the configuration unless the search found no file (a
/// given file that is missing is an error, never a fallback). Whether a
/// missing configuration is an error is the command's choice.
fn load_config(
    flag: Option<&OsStr>,
    for_profile_test: bool,
    strict_modes: bool,
) -> Result<(Lookup, Option<Config>), String> {
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
                let mut config = if for_profile_test {
                    config::parse_for_profile_test(path, &bytes)
                } else {
                    config::parse(path, &bytes)
                }
                .map_err(|e| e.to_string())?;
                if cfg!(feature = "proposed-release-01-catalog") {
                    enforce_proposed_release_01_config(&mut config)?;
                }
                return Ok((lookup, Some(config)));
            }
            Err(e) if matches!(lookup, Lookup::Search(_)) && is_absent(&e) => continue,
            Err(e) => return Err(format!("config {}: {e}", path.display())),
        }
    }
    Ok((lookup, None))
}

/// Recheck every configured tier against the narrowed catalog. Config
/// parsing checked these expressions against the full embedded library;
/// simply replacing that library would leave a previously checked Docker
/// or command selection in a later tier. `--select` is checked separately
/// against the same narrowed library by `Tier::select`.
fn enforce_proposed_release_01_config(config: &mut Config) -> Result<(), String> {
    let library = config.library.clone().proposed_release_01()?;
    for policy in &config.profile_policies {
        if library.get(&policy.profile).is_none() {
            return Err(format!(
                "proposed 0.1 catalog refuses checkout policy for excluded profile `{}`",
                policy.profile
            ));
        }
    }
    for group in &config.groups {
        for tier in &group.tiers {
            select::check(tier.select.expr().clone(), &library).map_err(|e| {
                format!(
                    "proposed 0.1 catalog refuses group `{}` tier `{}`: {e}",
                    group.name, tier.name
                )
            })?;
        }
    }
    for protected in &config.protect {
        if library.get(&protected.profile).is_none() {
            return Err(format!(
                "proposed 0.1 catalog refuses protect profile `{}`",
                protected.profile
            ));
        }
    }
    config.library = library;
    Ok(())
}

/// `ensure`'s configuration: `None` when none is found and `--select`
/// makes one optional.
fn ensure_config(args: &EnsureArgs) -> Result<Option<Config>, String> {
    match load_config(args.config.as_deref(), false, args.allow_root)? {
        (_, Some(config)) => Ok(Some(config)),
        (_, None) if args.allow_root => {
            Err("--allow-root requires a configuration file with an owners: list".to_owned())
        }
        (_, None) if args.select.is_some() => Ok(None),
        (lookup, None) => Err(format!(
            "no configuration file found (tried {}); ensure without --select needs one",
            lookup
                .paths()
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Translate configured owners once before any planning or deletion. Two
/// names or entries resolving to the same uid hide a policy error.
fn resolved_owners(config: &Config) -> Result<Vec<u32>, String> {
    let entries = config
        .owners
        .as_ref()
        .ok_or("--allow-root requires an owners: list")?;
    let mut ids = Vec::with_capacity(entries.len());
    for owner in entries {
        let uid = match owner {
            Owner::Name(name) => resolve_owner(name)?,
            Owner::Uid(explicit) => explicit.uid,
        };
        if ids.contains(&uid) {
            return Err(format!("owners: entries resolve to duplicate uid {uid}"));
        }
        ids.push(uid);
    }
    Ok(ids)
}

/// A configuration file that is not there (as opposed to one that cannot
/// be read, which is an error, never a reason to try the next file).
fn is_absent(e: &diskgc_core::host::HostError) -> bool {
    e.is(Errno::ENOENT) || e.is(Errno::ENOTDIR)
}

/// The tier `--select` makes, against the configuration's library or the
/// built-ins alone.
fn tier(
    args: &EnsureArgs,
    config: Option<&Config>,
    uid: u32,
    allow_root: bool,
) -> Result<Tier, (ErrorCause, String)> {
    let Some(expr) = &args.select else {
        return Err((
            ErrorCause::Usage,
            "an explicit tier requires --select".to_owned(),
        ));
    };
    let builtin;
    let library = match config {
        Some(config) => &config.library,
        None => {
            builtin = builtin_library().map_err(|message| (ErrorCause::Config, message))?;
            &builtin
        }
    };
    let roots = (!args.roots.is_empty()).then_some(args.roots.as_slice());
    let (selected, warnings) =
        Tier::select_with_warnings(expr, library, roots, uid).map_err(|failure| match failure {
            SelectionFailure::Parse(message) => (ErrorCause::Parse, message),
            SelectionFailure::Usage(message) => (ErrorCause::Usage, message),
        })?;
    for warning in warnings {
        log(&format!("warning: --select {expr:?}: {warning}"));
    }
    if allow_root {
        reject_root_uid_selected(&selected, library, &args.roots)
            .map_err(|message| (ErrorCause::Safety, message))?;
    }
    Ok(selected)
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

/// Draw one seed for all random groups so the report reproduces their order.
fn configured_seed(config: &Config) -> Result<Option<u64>, String> {
    if !config
        .groups
        .iter()
        .any(|group| group.ranking == Ranking::Random)
    {
        return Ok(None);
    }
    if let Some(value) = std::env::var_os("DISKGC_SEED") {
        return parse_configured_seed(&value).map(Some);
    }
    let mut bytes = [0; 8];
    getrandom::fill(&mut bytes).map_err(|error| format!("could not draw random seed: {error}"))?;
    Ok(Some(u64::from_le_bytes(bytes)))
}

/// A stable, small PRNG for Fisher-Yates tier shuffles. The seed is printed
/// so a run can be reproduced without relying on a library RNG version.
fn next_shuffle_word(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut word = *state;
    word = (word ^ (word >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    word = (word ^ (word >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    word ^ (word >> 31)
}

/// Compile every configured tier before any action. Unsupported execution
/// options are errors rather than silently changing policy order.
fn configured_tiers(
    config: &Config,
    uid: u32,
    allow_root: bool,
    seed: Option<u64>,
) -> Result<(Vec<Tier>, Vec<Duration>), String> {
    let mut tiers = Vec::new();
    let mut timeouts = Vec::new();
    let mut state = seed.unwrap_or_default();
    for group in &config.groups {
        let mut settings = group.tiers.iter().collect::<Vec<_>>();
        if group.ranking == Ranking::Random {
            if seed.is_none() {
                return Err(format!("group `{}` needs a random seed", group.name));
            }
            for end in (1..settings.len()).rev() {
                let pick = (next_shuffle_word(&mut state) % (end as u64 + 1)) as usize;
                settings.swap(end, pick);
            }
        }
        for setting in settings {
            let tier = Tier::configured(&group.name, setting, &config.library, uid)?;
            if allow_root {
                let supplied: Vec<String> = setting.roots.values().flatten().cloned().collect();
                reject_root_uid_selected(&tier, &config.library, &supplied)?;
                validate_selected_root_action(true, &tier)?;
            }
            tiers.push(tier);
            timeouts.push(setting.timeout);
        }
    }
    if tiers.is_empty() {
        return Err(format!(
            "config {}: groups contain no tiers",
            config.path.display()
        ));
    }
    Ok((tiers, timeouts))
}

/// Root mode must validate every source of a positive union. Checking only
/// the first would let a later term bind `{uid}` to root's uid 0.
fn reject_root_uid_selected(
    selected: &Tier,
    library: &Library,
    supplied_roots: &[String],
) -> Result<(), String> {
    let profiles: Vec<&str> = std::iter::once(selected.profile.as_str())
        .chain(
            selected
                .negated
                .iter()
                .map(|source| source.profile.as_str()),
        )
        .chain(selected.fs_union.iter().flat_map(|source| {
            std::iter::once(source.profile.as_str()).chain(
                source
                    .negated
                    .iter()
                    .map(|negated| negated.profile.as_str()),
            )
        }))
        .collect();
    for profile in profiles {
        let Some(Body::Source(source)) = library.get(profile).map(|p| &p.body) else {
            return Err(format!(
                "--select `{}`: selected source `{profile}` is missing",
                selected.select
            ));
        };
        reject_root_uid_placeholder(profile, source, supplied_roots)?;
    }
    Ok(())
}

/// In root mode `{uid}` expands to uid 0, not to each configured owner. A
/// root-owned namespace can therefore select a different user's files.
/// Refuse the source before its substituted find can plan or reclaim.
fn reject_root_uid_placeholder(
    profile: &str,
    source: &Source,
    supplied_roots: &[String],
) -> Result<(), String> {
    let in_field = if supplied_roots.iter().any(|root| root.contains("{uid}")) {
        Some("--roots")
    } else if matches!(&source.roots, Roots::Fixed(roots) if roots.iter().any(|root| root.contains("{uid}")))
    {
        Some("find.roots")
    } else if matches!(&source.finds, Finds::Paths { patterns, .. } if patterns.iter().any(|path| path.contains("{uid}")))
    {
        Some("find.paths")
    } else if matches!(&source.finds, Finds::Paths { exclude, .. } if exclude.iter().any(|path| path.contains("{uid}")))
    {
        Some("find.exclude")
    } else if source
        .where_
        .app_not_running
        .as_deref()
        .is_some_and(|path| path.contains("{uid}"))
    {
        Some("where.app_not_running")
    } else {
        None
    };
    if let Some(field) = in_field {
        return Err(format!(
            "--allow-root refuses `{{uid}}` in {field} of selected profile `{profile}`: \
             it would expand to root (uid 0), not to an owners: entry"
        ));
    }
    Ok(())
}

/// Check the action that this invocation actually selected. Static config
/// checks cover configured tiers; `--select` can choose another built-in or
/// user profile from the same library.
fn validate_selected_root_action(allow_root: bool, tier: &Tier) -> Result<(), String> {
    if allow_root && let Some(error) = tier.action.root_mode_error() {
        return Err(error.to_owned());
    }
    Ok(())
}

/// Runs `ensure`; every error is already in the report.
fn ensure(args: &EnsureArgs) -> Report {
    // Every error is logged where it is recorded: here for those found
    // before the executor runs, by the executor for its own.
    let fail = |cause: ErrorCause, why: String| {
        log(&format!("error: {why}"));
        Report::new(Status::UsageError, Some(&args.path)).usage_error_with_cause(cause, why)
    };
    let fail_host = |why: String| {
        log(&format!("error: {why}"));
        failed_report(Status::Error, ErrorCause::Host, why, Some(&args.path))
    };
    if let Err(error) = LinuxHost::install_interrupt_handlers() {
        return fail_host(format!("install signal handlers: {error}"));
    }
    let host = match startup() {
        Ok(host) => host,
        Err(StartupFailure::Host(why)) => return fail_host(why),
        Err(StartupFailure::UnsupportedKernel(why)) => return fail(ErrorCause::Usage, why),
    };
    let config = match ensure_config(args) {
        Ok(config) => config,
        Err(why) => return fail(ErrorCause::Config, why),
    };
    if let Some(config) = &config {
        log(&format!("config {}", config.path.display()));
        for warning in &config.warnings {
            log(&format!("warning: {warning}"));
        }
        if let Err(why) = config.validate_mode(args.allow_root) {
            return fail(ErrorCause::Safety, why);
        }
        if config.ipc == IpcPolicy::LiveOnly {
            return fail(ErrorCause::Safety, "ipc: live-only is statically valid, but ensure cannot enforce it in this build yet (task-21)".to_owned());
        }
    }
    let uid = match executor::privileges(&host, args.allow_root) {
        Ok(uid) => uid,
        Err(why) => return fail(ErrorCause::Safety, why),
    };
    let owners = if args.allow_root {
        match resolved_owners(config.as_ref().expect("root mode requires config")) {
            Ok(owners) => owners,
            Err(why) => return fail(ErrorCause::Config, why),
        }
    } else {
        vec![uid]
    };
    let protections = match config.as_ref() {
        Some(config) => match executor::bind_protections(config, uid, args.allow_root) {
            Ok(protections) => protections,
            Err(why) => return fail(ErrorCause::Config, why),
        },
        None => Vec::new(),
    };
    let mut configured = if args.select.is_none() {
        let config = config.as_ref().expect("unselected ensure requires config");
        let seed = match configured_seed(config) {
            Ok(seed) => seed,
            Err(why) => return fail(ErrorCause::Config, why),
        };
        match configured_tiers(config, uid, args.allow_root, seed) {
            Ok((tiers, timeouts)) => Some((tiers, timeouts, seed)),
            Err(why) => return fail(ErrorCause::Config, why),
        }
    } else {
        None
    };
    if let Some((tiers, _, _)) = &mut configured {
        for tier in tiers {
            tier.protections = protections.clone();
        }
    }
    let mut tier = if let Some((tiers, _, _)) = &configured {
        tiers[0].clone()
    } else {
        let tier = match tier(args, config.as_ref(), uid, args.allow_root) {
            Ok(tier) => tier,
            Err((cause, why)) => return fail(cause, why),
        };
        if let Err(why) = validate_selected_root_action(args.allow_root, &tier) {
            return fail(ErrorCause::Safety, why);
        }
        tier
    };
    tier.protections = protections;
    let request = Request {
        path: args.path.clone(),
        min_free: args.min_free,
        target_free: args.target_free,
        timeout: args.timeout,
        tally_budget: args.tally_budget,
        tier,
    };
    let mut progress = |line: &str| log(&operator_progress(line));
    let mut report = if let Some((tiers, timeouts, _)) = &configured {
        if args.dry_run {
            executor::dry_run_grouped(&host, &request, tiers, timeouts, &owners, &mut progress)
        } else {
            executor::ensure_grouped(&host, &request, tiers, timeouts, &owners, &mut progress)
        }
    } else if args.dry_run {
        // A plain Host: the dry run has no way to reach a deleting method.
        executor::dry_run(&host, &request, &owners, &mut progress)
    } else {
        executor::ensure(&host, &request, &owners, &mut progress)
    };
    report.seed = configured.as_ref().and_then(|(_, _, seed)| *seed);
    report
}

/// The built-in profiles alone; an error is a bug in the shipped YAML
/// (the tests load it).
fn builtin_library() -> Result<Library, String> {
    let library = Library::builtin()
        .map_err(|e| format!("the built-in profiles do not load (a bug): {e}"))?;
    if cfg!(feature = "proposed-release-01-catalog") {
        library.proposed_release_01()
    } else {
        Ok(library)
    }
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
    let (_, config) = load_config(
        args.config.as_deref(),
        matches!(&args.command, ProfilesCommand::Test),
        false,
    )
    .map_err(ProfilesFailure::Config)?;
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
    let (lookup, config) = load_config(flag, false, false)?;
    let config = config.ok_or_else(|| {
        format!(
            "no configuration file found (tried {})",
            lookup
                .paths()
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
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
    fn root_mode_checks_the_final_cli_selection() {
        let library = Library::builtin().expect("built-ins");
        let roots = ["/home/mark".to_owned()];
        let command = Tier::select("profile:pip-cache", &library, Some(&roots), 1000)
            .expect("command source");
        let deletion = Tier::select("profile:go-mod-cache", &library, Some(&roots), 1000)
            .expect("deletion source");
        assert!(validate_selected_root_action(false, &command).is_ok());
        assert!(validate_selected_root_action(true, &deletion).is_ok());
        assert!(
            validate_selected_root_action(true, &command)
                .unwrap_err()
                .contains("command actions are refused")
        );
        let docker = Tier::select("profile:docker-build-cache", &library, None, 1000)
            .expect("Docker source");
        assert!(
            validate_selected_root_action(true, &docker)
                .unwrap_err()
                .contains("owners: cannot bound")
        );
    }

    #[test]
    fn root_mode_refuses_each_uid_placeholder_input() {
        let library = Library::builtin().expect("built-ins");
        let Some(Body::Source(source)) = library.get("claude-code-tmp").map(|p| &p.body) else {
            panic!("missing built-in")
        };
        let mut source = (**source).clone();
        source.roots = Roots::Required;
        let Finds::Paths {
            patterns, exclude, ..
        } = &mut source.finds
        else {
            panic!("expected fs source")
        };
        *patterns = vec!["cache".to_owned()];
        exclude.clear();
        let check = |source: &Source, roots: &[String]| {
            reject_root_uid_placeholder("fixture", source, roots)
        };
        assert!(check(&source, &[]).is_ok());
        assert!(
            check(&source, &["/tmp/u-{uid}".to_owned()])
                .unwrap_err()
                .contains("--roots")
        );
        source.roots = Roots::Fixed(vec!["/tmp/u-{uid}".to_owned()]);
        assert!(check(&source, &[]).unwrap_err().contains("find.roots"));
        source.roots = Roots::Required;
        if let Finds::Paths { patterns, .. } = &mut source.finds {
            *patterns = vec!["u-{uid}".to_owned()];
        }
        assert!(check(&source, &[]).unwrap_err().contains("find.paths"));
        if let Finds::Paths {
            patterns, exclude, ..
        } = &mut source.finds
        {
            *patterns = vec!["cache".to_owned()];
            *exclude = vec!["u-{uid}".to_owned()];
        }
        assert!(check(&source, &[]).unwrap_err().contains("find.exclude"));
        if let Finds::Paths { exclude, .. } = &mut source.finds {
            exclude.clear();
        }
        source.where_.app_not_running = Some("/tmp/u-{uid}/SingletonLock".to_owned());
        assert!(
            check(&source, &[])
                .unwrap_err()
                .contains("where.app_not_running")
        );
    }

    #[test]
    fn root_mode_checks_uid_placeholder_in_second_union_source() {
        let specs = diskgc_core::profile::parse_profiles(concat!(
            "- name: plain-union\n",
            "  find: {roots: [/tmp], paths: [plain-*], type: dir}\n",
            "  action: delete\n",
            "  examples: {match: [/tmp/plain-x]}\n",
            "- name: uid-union\n",
            "  find: {roots: [/tmp], paths: [\"uid-{uid}\"], type: dir}\n",
            "  action: delete\n",
            "  examples: {match: [/tmp/uid-1000]}\n",
        ))
        .expect("fixture profiles");
        let library = Library::with_user_profiles(&specs).expect("fixture library");
        let selected = Tier::select(
            "profile:plain-union || profile:uid-union",
            &library,
            None,
            0,
        )
        .expect("filesystem union");
        let error = reject_root_uid_selected(&selected, &library, &[])
            .expect_err("second source's uid placeholder must be refused");
        assert!(error.contains("selected profile `uid-union`"), "{error}");
        assert!(error.contains("find.paths"), "{error}");
    }

    #[test]
    fn root_mode_checks_uid_placeholder_in_negated_source() {
        let specs = diskgc_core::profile::parse_profiles(concat!(
            "- name: plain-source\n",
            "  find: {roots: [/tmp], paths: [plain-*], type: dir}\n",
            "  action: delete\n",
            "  examples: {match: [/tmp/plain-x]}\n",
            "- name: uid-exclusion\n",
            "  find: {roots: [/tmp], paths: [\"uid-{uid}\"], type: dir}\n",
            "  action: delete\n",
            "  examples: {match: [/tmp/uid-1000]}\n",
        ))
        .expect("fixture profiles");
        let library = Library::with_user_profiles(&specs).expect("fixture library");
        let selected = Tier::select(
            "profile:plain-source && !profile:uid-exclusion",
            &library,
            None,
            0,
        )
        .expect("negated filesystem source");
        let error = reject_root_uid_selected(&selected, &library, &[])
            .expect_err("negated source's uid placeholder must be refused");
        assert!(
            error.contains("selected profile `uid-exclusion`"),
            "{error}"
        );
        assert!(error.contains("find.paths"), "{error}");
    }

    #[test]
    fn configured_random_ranking_is_reproducible_and_preserves_group_order() {
        let source = "version: 1\ngroups:\n  - name: stale\n    ranking: random\n    tiers:\n      - name: first\n        select: profile:cargo-target\n        roots: [/tmp]\n      - name: second\n        select: profile:cargo-target\n        roots: [/tmp]\n      - name: third\n        select: profile:cargo-target\n        roots: [/tmp]\n  - name: pressure\n    tiers:\n      - name: last\n        select: profile:cargo-target\n        roots: [/tmp]\n";
        let config = config::parse(std::path::Path::new("/tmp/policy.yaml"), source.as_bytes())
            .expect("valid static policy");
        let (first, first_timeouts) = configured_tiers(&config, 1000, false, Some(42)).unwrap();
        let (again, again_timeouts) = configured_tiers(&config, 1000, false, Some(42)).unwrap();
        assert_eq!(first, again);
        assert_eq!(first_timeouts, again_timeouts);
        assert_eq!(
            first
                .iter()
                .map(|tier| tier.name.as_str())
                .collect::<Vec<_>>(),
            ["first", "third", "second", "last"]
        );
        assert_eq!(first.last().expect("pressure tier").name, "last");
        assert!(first.iter().take(3).all(|tier| tier.group == "stale"));
        let error = configured_tiers(&config, 1000, false, None).expect_err("seed required");
        assert!(error.contains("needs a random seed"), "{error}");
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

    #[test]
    fn configured_execution_accepts_best_fit() {
        let source = "version: 1\ngroups:\n  - name: stale\n    tiers:\n      - name: cargo\n        select: profile:cargo-target\n        roots: [/tmp]\n        order: best-fit\n";
        let config = config::parse(std::path::Path::new("/tmp/policy.yaml"), source.as_bytes())
            .expect("valid static policy");
        let (tiers, _) =
            configured_tiers(&config, 1000, false, None).expect("best-fit is supported");
        assert_eq!(tiers[0].order, diskgc_core::config::Order::BestFit);
    }

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

    #[test]
    fn proposed_catalog_refuses_excluded_config_tiers_even_when_later() {
        for excluded in ["docker-build-cache", "pip-cache", "pytest-cache"] {
            let roots = if excluded == "docker-build-cache" {
                ""
            } else {
                "        roots: [/tmp]\n"
            };
            let source = format!(
                "version: 1\ngroups:\n  - name: first\n    tiers:\n      - name: safe\n        select: profile:cargo-target\n        roots: [/tmp]\n  - name: later\n    tiers:\n      - name: excluded\n        select: profile:{excluded}\n{roots}"
            );
            let mut config =
                config::parse(std::path::Path::new("/tmp/policy.yaml"), source.as_bytes())
                    .expect("valid full catalog policy");
            let error = enforce_proposed_release_01_config(&mut config)
                .expect_err("later tier must fail before execution");
            assert!(
                error.contains("later") && error.contains(excluded),
                "{error}"
            );
        }
    }

    #[test]
    fn proposed_catalog_refuses_operator_override_of_an_allowed_name() {
        let source = concat!(
            "version: 1\nprofiles:\n",
            "  - name: cargo-target\n    override: true\n",
            "    kind: cargo-target\n    find: {roots: required}\n",
            "groups:\n  - name: first\n    tiers:\n",
            "      - name: cargo\n        select: profile:cargo-target\n        roots: [/tmp]\n",
        );
        let mut config = config::parse(std::path::Path::new("/tmp/policy.yaml"), source.as_bytes())
            .expect("valid full catalog override");
        let error = enforce_proposed_release_01_config(&mut config)
            .expect_err("operator override must not impersonate approved Cargo");
        assert!(
            error.contains("operator-authored") && error.contains("cargo-target"),
            "{error}"
        );
    }

    #[test]
    fn proposed_catalog_accepts_only_checkout_policy_for_an_admitted_builtin() {
        let source =
            "version: 1\nprofile_policies:\n  - profile: cargo-target\n    checkouts: refuse\n";
        let mut config = config::parse(std::path::Path::new("/tmp/policy.yaml"), source.as_bytes())
            .expect("narrow policy parses");
        enforce_proposed_release_01_config(&mut config).expect("Cargo remains admitted");
        assert_eq!(
            config.library.checkouts("cargo-target"),
            Some(diskgc_core::engine::Checkouts::Refuse)
        );
        assert_eq!(
            config.library.get("cargo-target").unwrap().origin,
            diskgc_core::profile::Origin::BuiltIn
        );

        for excluded in ["npm-cache", "uv-cache"] {
            let source = format!(
                "version: 1\nprofile_policies:\n  - profile: {excluded}\n    checkouts: allow\n"
            );
            let mut config =
                config::parse(std::path::Path::new("/tmp/policy.yaml"), source.as_bytes())
                    .expect("full library accepts policy");
            let error = enforce_proposed_release_01_config(&mut config)
                .expect_err("excluded profile policy must not pass release gate");
            assert!(
                error.contains(excluded) && error.contains("excluded profile"),
                "{error}"
            );
        }
    }

    #[test]
    fn proposed_catalog_refuses_user_command_action_before_selection() {
        let source = concat!(
            "version: 1\nprofiles:\n",
            "  - name: custom-cleaner\n",
            "    find: {roots: required, paths: [.cache/tool], type: dir}\n",
            "    action: {command: [tool, clean, '{path}']}\n",
            "    examples: {match: [/tmp/.cache/tool]}\n",
            "groups:\n  - name: first\n    tiers:\n",
            "      - name: cleaner\n        select: profile:custom-cleaner\n        roots: [/tmp]\n",
        );
        let mut config = config::parse(std::path::Path::new("/tmp/policy.yaml"), source.as_bytes())
            .expect("valid full catalog command policy");
        let error = enforce_proposed_release_01_config(&mut config)
            .expect_err("user command action must be refused before any backend");
        assert!(
            error.contains("operator-authored") && error.contains("custom-cleaner"),
            "{error}"
        );
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
    fn configured_root_mode_checks_later_groups_before_execution() {
        let source = concat!(
            "version: 1\nprofiles:\n",
            "  - name: uid-in-later-group\n",
            "    find: {roots: [/tmp], paths: [\"uid-{uid}\"], type: dir}\n",
            "    action: delete\n",
            "    examples: {match: [/tmp/uid-1000]}\n",
            "groups:\n",
            "  - name: first\n    tiers:\n",
            "      - name: cargo\n        select: profile:cargo-target\n        roots: [/tmp]\n",
            "  - name: later\n    tiers:\n",
            "      - name: uid\n        select: profile:uid-in-later-group\n",
        );
        let config = config::parse(std::path::Path::new("/tmp/policy.yaml"), source.as_bytes())
            .expect("valid static policy");
        let error = configured_tiers(&config, 0, true, None)
            .expect_err("later uid placeholder must be refused");
        assert!(
            error.contains("uid-in-later-group") && error.contains("find.paths"),
            "{error}"
        );
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
        assert!(a.dry_run && !a.allow_root);
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
