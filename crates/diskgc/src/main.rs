//! The `diskgc` binary: CLI and output, wiring `diskgc-core` to `LinuxHost`
//! (docs/reference.md is the contract).
//!
//! So far: `ensure` with `--select` (one `cargo-target` tier), `--dry-run`
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
//!    the `--select` tier, checked against it (static
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
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use diskgc_core::config::{self, Config, Env, IpcPolicy, Lookup, MAX_CONFIG_BYTES};
use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::host::Errno;
use diskgc_core::profile::{Library, describe};
use diskgc_core::report::json::Json;
use diskgc_core::report::{Report, Status};
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
                     [--tally-budget DUR] [--no-index] --select EXPR [--roots DIR]...
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

This build runs `ensure --select` with one source profile term and
supported positive filters: `profile:cargo-target`, or a filesystem
profile whose action is `delete` or `command`. `watch`, `survey`, and
config tiers in ensure arrive with later tasks. PATH and every DIR are
absolute.
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
fn startup() -> Result<LinuxHost, String> {
    let host = LinuxHost::new().map_err(|e| e.to_string())?;
    diskgc_core::kernel::require_supported(&host).map_err(|e| e.to_string())?;
    Ok(host)
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
                let config = if for_profile_test {
                    config::parse_for_profile_test(path, &bytes)
                } else {
                    config::parse(path, &bytes)
                }
                .map_err(|e| e.to_string())?;
                return Ok((lookup, Some(config)));
            }
            Err(e) if matches!(lookup, Lookup::Search(_)) && is_absent(&e) => continue,
            Err(e) => return Err(format!("config {}: {e}", path.display())),
        }
    }
    Ok((lookup, None))
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

/// Translate the configured names once before any planning or deletion.
/// Two aliases for the same uid would hide a policy error, so refuse them.
fn resolved_owners(config: &Config) -> Result<Vec<u32>, String> {
    let names = config
        .owners
        .as_ref()
        .ok_or("--allow-root requires an owners: list")?;
    let mut ids = Vec::with_capacity(names.len());
    for name in names {
        let uid = resolve_owner(name)?;
        if ids.contains(&uid) {
            return Err(format!(
                "owners: user {name:?} resolves to duplicate uid {uid}"
            ));
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
            builtin = builtin_library()?;
            &builtin
        }
    };
    let roots = (!args.roots.is_empty()).then_some(args.roots.as_slice());
    Tier::select(expr, library, roots, uid)
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
    let fail = |why: String| {
        log(&format!("error: {why}"));
        Report::new(Status::UsageError, Some(&args.path)).usage_error(why)
    };
    let host = match startup() {
        Ok(host) => host,
        Err(why) => return fail(why),
    };
    let config = match ensure_config(args) {
        Ok(config) => config,
        Err(why) => return fail(why),
    };
    if let Some(config) = &config {
        log(&format!("config {}", config.path.display()));
        for warning in &config.warnings {
            log(&format!("warning: {warning}"));
        }
        if let Err(why) = config.validate_mode(args.allow_root) {
            return fail(why);
        }
        if !config.protect.is_empty() {
            return fail("protect: is statically valid, but ensure cannot enforce it in this build yet (task-16)".to_owned());
        }
        if config.ipc == IpcPolicy::LiveOnly {
            return fail("ipc: live-only is statically valid, but ensure cannot enforce it in this build yet (task-21)".to_owned());
        }
    }
    let uid = match executor::privileges(&host, args.allow_root) {
        Ok(uid) => uid,
        Err(why) => return fail(why),
    };
    let owners = if args.allow_root {
        match resolved_owners(config.as_ref().expect("root mode requires config")) {
            Ok(owners) => owners,
            Err(why) => return fail(why),
        }
    } else {
        vec![uid]
    };
    let tier = match tier(args, config.as_ref(), uid) {
        Ok(tier) => tier,
        Err(why) => return fail(why),
    };
    if let Err(why) = validate_selected_root_action(args.allow_root, &tier) {
        return fail(why);
    }
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
        executor::dry_run(&host, &request, &owners, &mut progress)
    } else {
        executor::ensure(&host, &request, &owners, &mut progress)
    }
}

/// The built-in profiles alone; an error is a bug in the shipped YAML
/// (the tests load it).
fn builtin_library() -> Result<Library, String> {
    Library::builtin().map_err(|e| format!("the built-in profiles do not load (a bug): {e}"))
}

/// `profiles list | show NAME | test`: the text or JSON for stdout, or why not
/// (exit 2). The library is the built-ins plus the configuration's user
/// profiles; without a configuration file (none found by the search), the
/// built-ins alone. A given file that is missing, or any file that does
/// not load, is an error, as for `ensure`.
fn profiles(args: &ProfilesArgs, json: bool) -> Result<(String, bool), String> {
    let (_, config) = load_config(
        args.config.as_deref(),
        matches!(&args.command, ProfilesCommand::Test),
        false,
    )?;
    let config_path = config.as_ref().map(|c| c.path.as_path());
    if let Some(path) = config_path {
        log(&format!("config {}", path.display()));
    }
    let builtin;
    let library = match &config {
        Some(config) => &config.library,
        None => {
            builtin = builtin_library()?;
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
            let profile = library
                .get(name)
                .ok_or_else(|| describe::unknown_name(library, name))?;
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
fn error_json(why: &str) -> String {
    Json::Obj(vec![
        ("status", Json::str(Status::UsageError.as_str())),
        ("exit_code", Json::U64(EXIT_USAGE.into())),
        ("error", Json::str(why)),
    ])
    .to_text()
}

fn log(line: &str) {
    eprintln!("diskgc: {line}");
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
                    output(&format!("{}\n", error_json(&why)), EXIT_USAGE)
                } else {
                    ExitCode::from(EXIT_USAGE)
                }
            }
        },
        Invocation::Profiles(args) => match profiles(&args, json) {
            Ok((out, failed)) => output(&out, u8::from(failed)),
            Err(why) => {
                log(&format!("error: {why}"));
                if json {
                    output(&format!("{}\n", error_json(&why)), EXIT_USAGE)
                } else {
                    ExitCode::from(EXIT_USAGE)
                }
            }
        },
        Invocation::NotYet { command, task } => {
            let why = format!("{command} is not implemented in this build yet ({task})");
            eprintln!("diskgc: {why}");
            if json {
                output(&format!("{}\n", error_json(&why)), EXIT_USAGE)
            } else {
                ExitCode::from(EXIT_USAGE)
            }
        }
        Invocation::UsageError(reason) => {
            // The kernel check comes first, as for every command that may
            // touch the machine: an old kernel is the reason to report.
            let reason = if args
                .first()
                .is_some_and(|a| a == "check" || a == "profiles")
            {
                reason
            } else {
                match startup() {
                    Err(why) => why,
                    Ok(_) => reason,
                }
            };
            match (json, report) {
                (true, true) => {
                    eprintln!("diskgc: error: {reason}");
                    output(
                        &format!(
                            "{}\n",
                            Report::new(Status::UsageError, None)
                                .usage_error(reason)
                                .to_json()
                        ),
                        EXIT_USAGE,
                    )
                }
                (true, false) => {
                    eprintln!("diskgc: error: {reason}");
                    output(&format!("{}\n", error_json(&reason)), EXIT_USAGE)
                }
                (false, _) => {
                    eprint!("diskgc: {reason}\n{USAGE}");
                    ExitCode::from(EXIT_USAGE)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
