//! The `diskgc` binary: CLI and output, wiring `diskgc-core` to `LinuxHost`
//! (docs/reference.md is the contract).
//!
//! So far: `ensure` with `--select` or configured groups, `--dry-run`
//! and `--json` (task-10), the dry run of one kind `fs` tier (task-68),
//! `profiles list`, `show NAME` and `test`, `check`, `watch` and `survey`
//! (their own modules), plus `--version` and `--help`, whose Profiles
//! section is rendered from the built-in library. A command the build's
//! row of the release descriptor leaves out exits 2.
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
//! host startup (the kernel check) and print the library's profiles or
//! checked tiers; `check` also builds a `LinuxHost` for its advisory root
//! warnings when one can start.
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
use diskgc_core::release::{self, Build, Command};
use diskgc_core::report::json::Json;
use diskgc_core::report::{self, ErrorCause, ErrorEntry, Report, Status};
use diskgc_core::units::{parse_duration, parse_size};
use diskgc_core::watch::Mode;
use diskgc_linux::LinuxHost;
use diskgc_linux::host::{ConfigReadError, read_config_file, read_root_config_file};

mod survey;
mod watch;

/// Exit code for usage, config and provisioning errors, including a kernel
/// older than the minimum (docs/reference.md, Exit codes).
const EXIT_USAGE: u8 = 2;

/// A result that could not be written is a run error even if the operation
/// itself had another result: callers cannot rely on a missing report.
const EXIT_OUTPUT_ERROR: u8 = 3;

/// `--timeout`'s default (docs/reference.md, `ensure`).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The commit this build comes from, set at compile time by the flake's
/// packages from `self.shortRev` or `self.dirtyShortRev` (flake.nix,
/// `gitRev`). A plain `cargo build` leaves it unset unless the caller
/// exports it.
const GIT_REV: Option<&str> = option_env!("DISKGC_GIT_REV");

/// The `--version` line (docs/reference.md, `diskgc --version`). A release
/// version (no pre-release suffix such as `-dev`) is a tagged release and
/// prints alone; any other version names the commit, or `unknown` when the
/// build did not say, so that evidence from development builds can be told
/// apart from a release and from each other.
fn version_line(version: &str, rev: Option<&str>) -> String {
    if version.contains('-') {
        let rev = rev.filter(|r| !r.is_empty()).unwrap_or("unknown");
        format!("diskgc {version} ({rev})\n")
    } else {
        format!("diskgc {version}\n")
    }
}

/// Each command's synopsis as the `usage:` lines print it: continuation
/// lines are indented for the 7 columns of `usage: `.
const ENSURE_SYNOPSIS: &str = "\
diskgc ensure --min-free SIZE [--target-free SIZE] [--timeout DUR]
                     [--tally-budget DUR] [--no-index] [--select EXPR] [--roots DIR]...
                     [--dry-run] [--json] [--config FILE] [--allow-root] PATH";
const PROFILES_SYNOPSIS: &str = "\
diskgc profiles list [--json] [--config FILE]
       diskgc profiles show NAME [--json] [--config FILE]
       diskgc profiles test [--json] [--config FILE]";
const CHECK_SYNOPSIS: &str = "diskgc check [--config FILE]";
const INFO_SYNOPSIS: &str = "\
diskgc --version
       diskgc --help";

/// Every synopsis with the command it belongs to (`None`: every build's).
fn synopses() -> [(Option<Command>, &'static str, &'static str); 7] {
    [
        (Some(Command::Ensure), "ensure", ENSURE_SYNOPSIS),
        (Some(Command::Watch), "watch", watch::WATCH_RECLAIM_SYNOPSIS),
        (
            Some(Command::WatchTallyOnly),
            "watch",
            watch::WATCH_TALLY_SYNOPSIS,
        ),
        (Some(Command::Profiles), "profiles", PROFILES_SYNOPSIS),
        (Some(Command::Check), "check", CHECK_SYNOPSIS),
        (Some(Command::Survey), "survey", survey::SURVEY_SYNOPSIS),
        (None, "", INFO_SYNOPSIS),
    ]
}

/// The usage lines `build` shows: of `command` when it runs it, else of
/// everything it runs.
fn usage(build: &Build, command: Option<&str>) -> String {
    let runs: Vec<(&str, &str)> = synopses()
        .into_iter()
        .filter(|(needs, ..)| needs.is_none_or(|c| build.allows(c)))
        .map(|(_, word, synopsis)| (word, synopsis))
        .collect();
    let of_command: Vec<&str> = runs
        .iter()
        .filter(|(word, _)| !word.is_empty() && Some(*word) == command)
        .map(|(_, synopsis)| *synopsis)
        .collect();
    let shown = if of_command.is_empty() {
        runs.iter().map(|(_, synopsis)| *synopsis).collect()
    } else {
        of_command
    };
    format!("usage: {}\n", shown.join("\n       "))
}

/// What follows a usage error's usage lines: one line, so the error stays
/// on screen (the full help is ~40 lines).
const SEE_HELP: &str = "Run `diskgc --help` for examples, sizes, durations and exit codes.\n";

/// The help's first part, about `ensure`, which every build runs (a rule
/// of the release descriptor: `release::EVERY_ROW_RUNS`).
const HELP_ENSURE: &str = "
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
";

/// The configuration paragraph, up to what the package installs.
const HELP_CONFIG: &str = "
Configuration: --config FILE, else $DISKGC_CONFIG, else the first of
  $XDG_CONFIG_HOME/diskgc/config.yaml (default ~/.config/diskgc/config.yaml)
  and /etc/diskgc/config.yaml that exists. `ensure --select` needs none;
  configured groups do. `diskgc check` validates a configuration.
";

/// `watch` in a build that runs a reclaiming one.
const HELP_WATCH: &str = "
watch runs as a service: it reclaims as ensure does whenever available
  space falls below --min-free, up to --target-free, and deletes nothing
  in between; --tally-only only keeps the index snapshot warm. It stops on
  SIGTERM, SIGINT or SIGHUP with exit 0 (no reload: restart it to apply an
  edited configuration); 2 refused at start, 3 stdout or the host failed.
";

/// `watch` in a build that runs only `watch --tally-only`.
const HELP_WATCH_TALLY_ONLY: &str = "
watch --tally-only runs as a service that keeps the index snapshot warm
  and never deletes. It stops on SIGTERM, SIGINT or SIGHUP with exit 0 (no
  reload: restart it to apply an edited configuration); 2 refused at start,
  3 stdout or the host failed.
";

/// `survey` in a build that runs it.
const HELP_SURVEY: &str = "
survey reads only: per PATH, what each profile finds (count, size, age,
  what keeps it), and the top-level entries no profile matches, grouped by
  name family; --in-use also checks whether candidates are in use
  (process scans), --list shows them, --json one object of it all.
";

/// The help after the usage lines, for `build`: what it runs, from the
/// release descriptor. The Profiles section that ends `--help` is
/// rendered from the build's library ([`describe::help_section`]) and
/// appended by the caller. No task numbers or source-tree paths: the
/// release package ships neither.
fn help(build: &Build) -> String {
    let mut out = String::from(HELP_ENSURE);
    out.push_str(HELP_CONFIG);
    out.push_str(&wrap(&package_sentence(build)));
    if build.allows(Command::Watch) {
        out.push_str(HELP_WATCH);
    } else if build.allows(Command::WatchTallyOnly) {
        out.push_str(HELP_WATCH_TALLY_ONLY);
    }
    if build.allows(Command::Survey) {
        out.push_str(HELP_SURVEY);
    }
    out.push('\n');
    out.push_str(&wrap(&commands_sentence(build)));
    out.push_str("SIZE: 50G, 80GiB, 500MB, 10%. DUR: 30s, 10m, 2h, 7d, 1w (m is minutes).\n");
    out
}

/// What the build's package installs that a user reads (the example, and
/// the README and release notes where it ships them), as the end of the
/// configuration paragraph.
fn package_sentence(build: &Build) -> String {
    let ships = |path: &str| build.ships.iter().any(|s| s == path);
    let mut sentence = String::from(" The package installs an example");
    if ships("share/doc/diskgc/examples/example.yaml") {
        sentence.push_str(" (share/doc/diskgc/examples/example.yaml)");
    }
    if ships("share/doc/diskgc/RELEASE-NOTES.md") {
        sentence
            .push_str(", a README and the release notes with the known limits (share/doc/diskgc/)");
    }
    sentence.push('.');
    sentence
}

/// The build's commands, and what of the documented ones it leaves out.
fn commands_sentence(build: &Build) -> String {
    let mut missing: Vec<&str> = Vec::new();
    if !build.allows(Command::Watch) {
        missing.push(if build.allows(Command::WatchTallyOnly) {
            "a reclaiming `watch`"
        } else {
            "`watch`"
        });
    }
    if !build.allows(Command::Survey) {
        missing.push("`survey`");
    }
    let missing = match missing.as_slice() {
        [] => String::new(),
        [one] => format!(" ({one} is not in it yet)"),
        many => format!(" ({} are not in it yet)", many.join(" and ")),
    };
    format!(
        "Commands in this version: {}{missing}. ensure runs \
         `--select` or configured groups with ordered or random tier ranking and \
         oldest or best-fit candidate order. PATH and every DIR are absolute.",
        build.commands_phrase()
    )
}

/// `text` as help lines of at most 76 columns, continuation lines indented
/// by two. A `text` that starts with a space continues a paragraph already
/// begun above it, so its first line is indented too (the package sentence
/// after HELP_CONFIG); otherwise its first line starts at column 0.
fn wrap(text: &str) -> String {
    let (mut out, mut line) = (String::new(), String::new());
    let continues = text.starts_with(' ');
    for word in text.split_whitespace() {
        let indent = if out.is_empty() && !continues {
            ""
        } else {
            "  "
        };
        if !line.is_empty() && indent.len() + line.len() + 1 + word.len() > 76 {
            out.push_str(indent);
            out.push_str(&line);
            out.push('\n');
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    let indent = if out.is_empty() && !continues {
        ""
    } else {
        "  "
    };
    out.push_str(indent);
    out.push_str(&line);
    out.push('\n');
    out
}

/// The flags each command takes, for "did you mean" (the parsers below
/// are the authority; a test holds each list to its parser and usage).
const ENSURE_FLAGS: [&str; 11] = [
    "--min-free",
    "--target-free",
    "--timeout",
    "--tally-budget",
    "--no-index",
    "--select",
    "--roots",
    "--dry-run",
    "--json",
    "--config",
    "--allow-root",
];
const PROFILES_FLAGS: [&str; 2] = ["--json", "--config"];
const CHECK_FLAGS: [&str; 1] = ["--config"];

/// The words a command line may start with, for "did you mean".
const FIRST_WORDS: [&str; 7] = [
    "ensure",
    "check",
    "profiles",
    "watch",
    "survey",
    "--help",
    "--version",
];

/// "unknown flag X", with the flag `known` most likely meant.
fn unknown_flag(written: &str, known: &[&str]) -> String {
    match diskgc_core::suggest::closest(written, known) {
        Some(near) => format!("unknown flag {written}; did you mean {near}?"),
        None => format!("unknown flag {written}"),
    }
}

/// The refusal of a command line whose first word is no command, naming
/// the commands `build` runs.
fn unknown_command(first: &str, build: &Build) -> String {
    let what = if first.starts_with('-') {
        "unknown flag before a command"
    } else {
        "unknown command"
    };
    match diskgc_core::suggest::closest(first, &FIRST_WORDS) {
        Some(near) => format!("{what} `{first}`; did you mean `{near}`?"),
        None => format!(
            "{what} `{first}`; the commands are {}",
            build.commands_phrase()
        ),
    }
}

/// What the command line asks for. Kept separate from acting on it so the
/// decision is a pure function of the arguments.
#[derive(Debug, PartialEq, Eq)]
enum Invocation {
    Version,
    Help,
    Ensure(EnsureArgs),
    Watch(watch::WatchArgs),
    Profiles(ProfilesArgs),
    Check(Option<OsString>),
    Survey(survey::SurveyArgs),
    /// A command this build does not run (the release descriptor's row,
    /// [`admit`]).
    NotInBuild(Command),
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

/// Whether the command line asks for JSON: any argument `--json`, or
/// `--json=...` (then a usage error). Decided on the raw arguments, before
/// they are understood, so that every exit prints one JSON object, a usage
/// error included, also when `--json` ended up as another flag's value
/// (`--config --json`) (docs/reference.md, `ensure` and `profiles`).
fn json_requested(args: &[OsString]) -> bool {
    args.iter()
        .any(|a| a == "--json" || a.as_encoded_bytes().starts_with(b"--json="))
}

/// Whether a command line that was not understood asked for a dry run, for
/// its report's `dry_run`: decided like [`json_requested`].
fn dry_run_requested(args: &[OsString]) -> bool {
    args.iter().any(|a| a == "--dry-run")
}

/// Whether `args` ask for help, which wins wherever it is asked for
/// (`profiles list --help`).
fn help_asked(args: &[OsString]) -> bool {
    args.iter().any(|a| a == "--help" || a == "-h")
}

/// Whether `args` are `--version` (or `-V`), alone or with `--json`, and
/// no help: answered before the build's row is read, so it works in any
/// build ([`begin`]).
fn version_asked(args: &[OsString]) -> bool {
    let version = |a: &OsString| a == "--version" || a == "-V";
    !help_asked(args)
        && match args {
            [flag] => version(flag),
            [first, second] => {
                version(first) && second == "--json" || first == "--json" && version(second)
            }
            _ => false,
        }
}

/// The command line's syntax, in `build` (whose commands a usage error
/// names): a pure function of both. What `build` does not run is
/// [`admit`]'s.
fn parse(args: &[OsString], build: &Build) -> Invocation {
    if help_asked(args) {
        return Invocation::Help;
    }
    if version_asked(args) {
        return Invocation::Version;
    }
    match args {
        [] => Invocation::UsageError("no command given".to_owned()),
        [command, rest @ ..] if command == "ensure" => match parse_ensure(rest) {
            Ok(args) => Invocation::Ensure(args),
            Err(why) => Invocation::UsageError(format!("ensure: {why}")),
        },
        [command, rest @ ..] if command == "watch" => match watch::parse_watch(rest) {
            Ok(args) => Invocation::Watch(args),
            Err(why) => Invocation::UsageError(format!("watch: {why}")),
        },
        [command, rest @ ..] if command == "profiles" => parse_profiles(rest),
        [command, rest @ ..] if command == "check" => match parse_check(rest) {
            Ok(config) => Invocation::Check(config),
            Err(why) => Invocation::UsageError(format!("check: {why}")),
        },
        [command, rest @ ..] if command == "survey" => match survey::parse_survey(rest) {
            Ok(args) => Invocation::Survey(args),
            Err(why) => Invocation::UsageError(format!("survey: {why}")),
        },
        [first, ..] if first == "--version" || first == "-V" => {
            Invocation::UsageError("--version takes no arguments (`--json` aside)".to_owned())
        }
        [command, ..] => Invocation::UsageError(unknown_command(&command.to_string_lossy(), build)),
    }
}

/// `invocation` held to what `build` runs: a command it does not run is
/// refused (exit 2) however it is written; a syntax error of one it runs
/// stays a usage error. `args` are the command line `invocation` was
/// parsed from.
fn admit(args: &[OsString], invocation: Invocation, build: &Build) -> Invocation {
    let first = args.first().and_then(|a| a.to_str());
    let needs = match &invocation {
        Invocation::Ensure(_) => Command::Ensure,
        Invocation::Watch(args) => match args.mode {
            Mode::Reclaim { .. } => Command::Watch,
            Mode::TallyOnly { .. } => Command::WatchTallyOnly,
        },
        Invocation::Profiles(_) => Command::Profiles,
        Invocation::Check(_) => Command::Check,
        Invocation::Survey(_) => Command::Survey,
        Invocation::UsageError(_) => match first {
            Some("ensure") => Command::Ensure,
            Some("watch") if !build.allows(Command::Watch) => Command::WatchTallyOnly,
            Some("watch") => Command::Watch,
            Some("profiles") => Command::Profiles,
            Some("check") => Command::Check,
            Some("survey") => Command::Survey,
            _ => return invocation,
        },
        _ => return invocation,
    };
    if build.allows(needs) {
        invocation
    } else if needs == Command::WatchTallyOnly && !build.allows(Command::Watch) {
        // No `watch` at all: the word is refused, whatever its flags.
        Invocation::NotInBuild(Command::Watch)
    } else {
        Invocation::NotInBuild(needs)
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
            Some(flag) if flag.starts_with('-') => {
                return Err(unknown_flag(
                    flag.split_once('=').map_or(flag, |(name, _)| name),
                    &CHECK_FLAGS,
                ));
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
                Some((name, _)) => return Err(unknown_flag(name, &PROFILES_FLAGS)),
                None => return Err(unknown_flag(flag, &PROFILES_FLAGS)),
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
            return usage(
                match diskgc_core::suggest::closest(other, &["list", "show", "test"]) {
                    Some(near) => format!("unknown subcommand {other}; did you mean {near}?"),
                    None => format!("unknown subcommand {other}: list, show NAME or test"),
                },
            );
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
            _ => return Err(unknown_flag(name, &ENSURE_FLAGS)),
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
            no_index,
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
/// `check`, which is machine-independent (docs/reference.md, check), and a
/// command line that is not understood (a usage error, TASK-291).
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

/// diskgc's state directory from the environment (docs/architecture.md,
/// Snapshot): `$XDG_STATE_HOME/diskgc` or `$HOME/.local/state/diskgc`;
/// `None` without an absolute one of them.
fn env_state_dir() -> Option<PathBuf> {
    diskgc_core::snapshot::state_dir(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// The host's startup checks and the host, with the state directory
/// already found ([`env_state_dir`]).
fn startup_in(state_dir: Option<PathBuf>) -> Result<LinuxHost, StartupFailure> {
    // The in-use scan holds about one descriptor per visible process; a
    // stock soft limit of 1024 is too low on a busy host (TASK-166,
    // diskgc_linux::nofile). Not raising it is not fatal: a scan that runs
    // out of descriptors is an error of the run.
    if let Err(error) = diskgc_linux::nofile::raise_to_hard() {
        log(&format!(
            "warning: could not raise the open-file limit: {error}; the in-use scan fails \
             if it runs out of descriptors on a busy host"
        ));
    }
    let mut host = LinuxHost::new().map_err(|e| StartupFailure::Host(e.to_string()))?;
    diskgc_core::kernel::require_supported(&host).map_err(kernel_failure)?;
    // Where the index snapshot lives (docs/architecture.md, Snapshot);
    // without an absolute $XDG_STATE_HOME or $HOME there is none, and a
    // run that would read or write it says so.
    // The per-user lock's home when the account database has none for this
    // user (docs/safety.md, Host lock); $XDG_* are never read for it.
    if let Some(home) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|h| h.is_absolute())
    {
        host = host.with_home_fallback(home);
    }
    if let Some(dir) = state_dir {
        host = host.with_state_dir(dir);
    }
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

/// This build's row of the release descriptor (`diskgc_core::release`):
/// the development build's, or with the `release-catalog` feature the
/// release this version belongs to (master's `0.2.0-dev`: 0.2). What it
/// runs, its help, its catalog and what its package ships follow from it.
fn this_build() -> Result<release::Entry, String> {
    if cfg!(feature = "release-catalog") {
        release::release_of_version(env!("CARGO_PKG_VERSION"))
    } else {
        release::development()
    }
    .map_err(|e| format!("the release descriptor does not load (a bug): {e}"))
}

/// [`this_build`], which [`begin`] checks before anything else reads it;
/// used only after that (and every test that asks for a build reads the
/// embedded descriptor).
fn build() -> release::Entry {
    this_build().expect("main checked the release descriptor first")
}

/// The catalog this build ships ([`Catalog`]), from [`build`].
fn catalog() -> Catalog {
    Catalog::from(build())
}

// TASK-195: removing the sandbox gate (diskgc-linux's
// `release-without-sandbox-gate`) is only for the release package, which
// also narrows the catalog. Built alone it would make an ungated binary with
// every development profile, so that build does not compile. The flake's
// release-gate-needs-catalog check proves the refusal.
const _: () = assert!(
    diskgc_linux::sandbox::SANDBOX_GATE || cfg!(feature = "release-catalog"),
    "diskgc-linux/release-without-sandbox-gate needs diskgc's release-catalog: \
     an ungated binary must ship the release catalog"
);

/// The configuration file (docs/reference.md, Configuration file), given
/// by `flag` (`--config`), `$DISKGC_CONFIG` or found by the search: its
/// bytes, or the paths tried when the search found none (a given file that
/// is missing is an error, never a fallback). Parsing it, under
/// [`catalog`], and whether a missing one is an error, are the command's.
/// The error's cause: `config` for the file (exit 2), `host` for a read
/// the host failed (`/proc`, I/O: exit 3, [`ConfigReadError`]).
fn read_config(flag: Option<&OsStr>, strict_modes: bool) -> ConfigRead {
    let read: ConfigReader = if strict_modes {
        &read_root_config_file
    } else {
        &read_config_file
    };
    read_config_with(flag, read)
}

/// The configuration file as found and read, or why not, with the cause
/// (`config` or `host`).
type ConfigRead = Result<ConfigFile, (ErrorCause, String)>;

/// How a configuration file's bytes are read: [`read_config_file`] or
/// [`read_root_config_file`] (a test gives its own).
type ConfigReader<'a> = &'a dyn Fn(&Path, usize) -> Result<Vec<u8>, ConfigReadError>;

/// [`read_config`] with its reader given.
fn read_config_with(flag: Option<&OsStr>, read: ConfigReader<'_>) -> ConfigRead {
    let env = Env {
        diskgc_config: std::env::var_os("DISKGC_CONFIG"),
        xdg_config_home: std::env::var_os("XDG_CONFIG_HOME"),
        home: std::env::var_os("HOME"),
    };
    let lookup = config::lookup(flag, &env).map_err(|why| (ErrorCause::Config, why))?;
    for path in lookup.paths() {
        match read(path, MAX_CONFIG_BYTES) {
            Ok(bytes) => {
                return Ok(ConfigFile::Read {
                    path: path.to_path_buf(),
                    bytes,
                });
            }
            Err(ConfigReadError::File(e))
                if matches!(lookup, Lookup::Search(_)) && is_absent(&e) =>
            {
                continue;
            }
            Err(ConfigReadError::File(e)) => {
                return Err((
                    ErrorCause::Config,
                    format!("config {}: {e}", path.display()),
                ));
            }
            Err(ConfigReadError::Host(e)) => {
                return Err((ErrorCause::Host, format!("config {}: {e}", path.display())));
            }
        }
    }
    Ok(ConfigFile::NotFound {
        tried: lookup.paths().into_iter().map(Path::to_path_buf).collect(),
    })
}

/// [`read_config`], parsed under [`catalog`] for `profiles` and `check`:
/// the configuration, or the paths tried when none was found.
fn load_config(
    flag: Option<&OsStr>,
    use_: ConfigUse,
) -> Result<Result<Config, Vec<PathBuf>>, String> {
    match read_config(flag, false).map_err(|(_, why)| why)? {
        ConfigFile::Read { path, bytes } => catalog().config(&path, &bytes, use_).map(Ok),
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
/// configuration file's bytes and the build's catalog. The configuration
/// and `--select` are checked ([`invocation::load`]) before the host
/// starts, so a bad configuration is exit 2 even where the kernel or
/// `/proc` checks would fail (docs/reference.md, Exit codes (`ensure`)).
fn ensure(args: &EnsureArgs) -> Report {
    ensure_with(args, &Start::MACHINE)
}

/// What `ensure` and `watch` read from the machine to start, in their
/// order: the configuration file, then the host's startup checks. Given
/// so the tests can hold that order and its classification.
struct Start<'a> {
    /// The configuration file ([`read_config`]).
    read_config: &'a dyn Fn(Option<&OsStr>, bool) -> ConfigRead,
    /// The host's startup ([`startup_in`]).
    startup: &'a dyn Fn(Option<PathBuf>) -> Result<LinuxHost, StartupFailure>,
}

impl Start<'_> {
    /// This machine's.
    const MACHINE: Start<'static> = Start {
        read_config: &read_config,
        startup: &startup_in,
    };
}

/// [`ensure`] with what it reads from the machine to start given.
fn ensure_with(args: &EnsureArgs, start: &Start<'_>) -> Report {
    // Every error is logged where it is recorded: here for those found
    // before the composition runs, by the composition for its own. Each
    // starts from the options' report, so it says what was asked for (the
    // goal, `--dry-run`) even though nothing was measured (TASK-191).
    let fail = |status: Status, cause: ErrorCause, why: String| {
        log(&format!("error: {why}"));
        let mut report = args.options.report(status);
        report.errors.push(ErrorEntry::run(cause, why));
        report
    };
    if let Err(error) = LinuxHost::install_interrupt_handlers() {
        return fail(
            Status::Error,
            ErrorCause::Host,
            format!("install signal handlers: {error}"),
        );
    }
    let config = match (start.read_config)(args.config.as_deref(), args.options.allow_root) {
        Ok(config) => config,
        Err((cause, why)) => return fail(invocation::refusal_status(cause), cause, why),
    };
    let platform = Platform {
        catalog: catalog(),
        draw_seed: &draw_seed,
        builtins: &builtins,
    };
    let loaded = match invocation::load(args.options.selection(), &config, &platform, &mut log) {
        Ok(loaded) => loaded,
        Err((cause, why)) => return invocation::refused(&args.options, cause, why, &mut log),
    };
    let host = match (start.startup)(env_state_dir()) {
        Ok(host) => host,
        Err(failure) => return fail(failure.status(), failure.cause(), failure.message()),
    };
    invocation::ensure(&host, &args.options, loaded, &platform, &mut log, &mut log)
}

/// The embedded built-in profiles, before [`catalog`] narrows them; an
/// error is a bug in the shipped YAML (the tests load it).
fn builtins() -> Result<Library, String> {
    Library::builtin().map_err(|e| format!("the built-in profiles do not load (a bug): {e}"))
}

/// The built-in profiles of [`catalog`] alone.
fn builtin_library() -> Result<Library, String> {
    catalog().library(builtins()?)
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
            let run = diskgc_core::profile::examples::run(library);
            let failures: Vec<String> = run.failures.iter().map(ToString::to_string).collect();
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
                let sources = library
                    .profiles()
                    .filter(|p| matches!(p.body, diskgc_core::profile::Body::Source(_)))
                    .count();
                (tested_text(&run, sources), false)
            }
        }
    })
}

/// `profiles test`'s line when nothing failed: what was checked, so a
/// library without examples is not reported as passing them.
/// Filters have no examples; only source profiles are counted.
fn tested_text(run: &diskgc_core::profile::examples::Run, sources: usize) -> String {
    if run.examples == 0 {
        format!("no examples to test: none of the {sources} source profiles has any\n")
    } else {
        format!(
            "{} examples of {} of the {sources} source profiles pass\n",
            run.examples, run.profiles
        )
    }
}

/// Machine-independent policy validation: its exit code depends on the
/// configuration alone. No kernel version check, process scan, path
/// traversal or backend invocation occurs here; the configured roots are
/// screened and looked up for advisory warnings only ([`warn_about_roots`]).
fn check(flag: Option<&OsStr>) -> Result<String, String> {
    let config =
        load_config(flag, ConfigUse::Run)?.map_err(|tried| invocation::not_found(&tried))?;
    for warning in &config.warnings {
        log(&format!("warning: {warning}"));
    }
    warn_about_roots(&config);
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

/// `check`'s root warnings ([`invocation::check_roots`], the decision
/// `ensure` refuses with). It needs a host for the lookup; where
/// `LinuxHost` cannot start (an old kernel, a sandbox without pidfds) the
/// roots are not checked, which is said, and the exit code stays.
fn warn_about_roots(config: &Config) {
    if config.written_roots.is_empty() {
        return;
    }
    let warnings = match LinuxHost::new() {
        Ok(host) => invocation::check_roots(&host, config),
        Err(why) => vec![format!(
            "cannot inspect this machine ({why}); configured roots not checked"
        )],
    };
    for warning in warnings {
        log(&format!("warning: {warning}"));
    }
}

/// The one JSON object of a failed command other than `ensure`
/// (docs/reference.md, Output, Errors): `ensure`'s envelope and `errors`
/// list with the one error.
fn error_json(status: Status, cause: ErrorCause, why: &str) -> String {
    report::error_json(status, &[ErrorEntry::run(cause, why)]).to_text()
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

/// What `build` says to `command`, which it does not run: a reclaiming
/// `watch` where only `watch --tally-only` runs (TASK-276), or any other.
fn not_in_build(command: Command, build: &Build) -> String {
    let what = if command == Command::Watch && build.allows(Command::WatchTallyOnly) {
        "a reclaiming `watch`".to_owned()
    } else {
        format!("`{}`", command.words())
    };
    format!(
        "{what} is not in diskgc {} yet; its commands are {}",
        env!("CARGO_PKG_VERSION"),
        build.commands_phrase()
    )
}

/// A command this build does not run: exit 2 with `why` on stderr, and
/// under `--json` the usage-error object every command fails with.
fn not_yet(why: &str, json: bool) -> ExitCode {
    log(&format!("error: {why}"));
    if json {
        output(
            &format!(
                "{}\n",
                error_json(Status::UsageError, ErrorCause::Usage, why)
            ),
            EXIT_USAGE,
        )
    } else {
        ExitCode::from(EXIT_USAGE)
    }
}

/// How a run starts: `--version` answers in every build; otherwise the
/// build's row is read first (TASK-289 review: a release-catalog build of a
/// version without a row must exit 2, never panic in the parser), then the
/// command line is parsed and held to it.
#[derive(Debug, PartialEq, Eq)]
enum Begin {
    Version,
    /// The release descriptor has no row for this build: exit 2.
    NoRow(String),
    Run(Invocation, release::Entry),
}

fn begin(args: &[OsString], row: Result<release::Entry, String>) -> Begin {
    if version_asked(args) {
        return Begin::Version;
    }
    match row {
        Err(why) => Begin::NoRow(why),
        Ok(row) => Begin::Run(admit(args, parse(args, &row), &row), row),
    }
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    // `--json` holds for every exit code when stdout works, usage errors
    // included: decided before the command line is understood. `ensure`
    // prints its report; every other command prints `error_json`.
    let json = json_requested(&args);
    let report = args.first().is_some_and(|c| c == "ensure");
    // Every later step reads the build's row ([`build`]), which `begin`
    // has checked.
    let (invocation, build) = match begin(&args, this_build()) {
        Begin::Version => return output(&version_line(env!("CARGO_PKG_VERSION"), GIT_REV), 0),
        Begin::NoRow(why) => return not_yet(&why, json),
        Begin::Run(invocation, build) => (invocation, build),
    };
    match invocation {
        Invocation::Version => unreachable!("answered by begin"),
        Invocation::Help => {
            let text = format!("{}{}", usage(&build, None), help(&build));
            match builtin_library() {
                Ok(library) => output(&format!("{text}{}", describe::help_section(&library)), 0),
                Err(why) => {
                    log(&format!("error: {why}"));
                    output(&text, EXIT_USAGE)
                }
            }
        }
        Invocation::Ensure(args) => finish(&ensure(&args), json),
        Invocation::Watch(args) => watch::watch(&args, json),
        Invocation::Check(config) => match check(config.as_deref()) {
            Ok(out) => output(&out, 0),
            Err(why) => {
                log(&format!("error: {why}"));
                // `check` has no `--json` flag (post-0.1 if a harness needs
                // one), but `check --config --json` parses with `--json`
                // as the file, and `json_requested` saw it (TASK-198).
                if json {
                    output(
                        &format!(
                            "{}\n",
                            error_json(Status::UsageError, ErrorCause::Config, &why)
                        ),
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
                    output(
                        &format!("{}\n", error_json(Status::UsageError, cause, &why)),
                        EXIT_USAGE,
                    )
                } else {
                    ExitCode::from(EXIT_USAGE)
                }
            }
        },
        Invocation::Survey(args) => survey::survey(&args, json),
        Invocation::NotInBuild(command) => not_yet(&not_in_build(command, &build), json),
        Invocation::UsageError(reason) => {
            // Reported before anything on the machine is read, as a bad
            // configuration is (docs/reference.md, Exit codes): a broken
            // `/proc` or an old kernel does not hide what was mistyped, nor
            // make it exit 3 (TASK-291). The command, once typed right,
            // meets the startup check.
            let (cause, status) = (ErrorCause::Parse, Status::UsageError);
            match (json, report) {
                (true, true) => {
                    eprintln!("diskgc: error: {reason}");
                    // The command line was not understood: no path or
                    // goal to report, only whether a dry run was asked for.
                    let mut report = Report::new(status, None, dry_run_requested(&args));
                    report.errors.push(ErrorEntry::run(cause, reason));
                    output(&format!("{}\n", report.to_json()), status.exit_code())
                }
                (true, false) => {
                    eprintln!("diskgc: error: {reason}");
                    output(
                        &format!("{}\n", error_json(status, cause, &reason)),
                        status.exit_code(),
                    )
                }
                (false, _) => {
                    // The error first and alone on its line, then only this
                    // command's usage and where the rest is: the full help
                    // would scroll the error away (TASK-148).
                    log(&format!("error: {reason}"));
                    if status == Status::UsageError {
                        let command = args.first().and_then(|a| a.to_str());
                        eprint!("{}{SEE_HELP}", usage(&build, command));
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

    /// [`super::parse`] in this test build's row.
    fn parse(args: &[OsString]) -> Invocation {
        super::parse(args, &build())
    }
    use diskgc_core::executor::Tier;
    use diskgc_core::units::Size;

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

    /// A configuration file of `text` in a fresh directory under the
    /// test's temporary directory (the dev shell's `tmp/system`), removed
    /// on drop.
    pub(super) struct ConfigFixture(PathBuf);

    impl ConfigFixture {
        pub(super) fn new(name: &str, text: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("t272-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("temporary directory");
            std::fs::write(dir.join("config.yaml"), text).expect("write the configuration");
            Self(dir)
        }

        pub(super) fn path(&self) -> String {
            self.0
                .join("config.yaml")
                .to_str()
                .expect("UTF-8")
                .to_owned()
        }
    }

    impl Drop for ConfigFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A configuration whose tier names a profile that does not exist:
    /// what `check` reports as invalid.
    pub(super) const BAD_CONFIG: &str = "version: 1\ngroups:\n  - name: g\n    tiers:\n      - name: t\n        select: profile:missing\n";

    /// TASK-272: the configuration is step 1, before the host starts (the
    /// kernel and `/proc` checks): a bad one is exit 2 (`config`) even
    /// where the host would fail (exit 3), and the host is not started.
    #[test]
    fn ensure_checks_the_configuration_before_the_host_starts() {
        let bad = ConfigFixture::new("ensure", BAD_CONFIG);
        let a = ensure_args(&[
            "--dry-run",
            "--config",
            &bad.path(),
            "--min-free",
            "1G",
            "/tmp",
        ])
        .expect("parses");
        let started = std::cell::Cell::new(0);
        let broken = |_: Option<PathBuf>| {
            started.set(started.get() + 1);
            Err(StartupFailure::Host(
                "open /proc: EACCES (a test)".to_owned(),
            ))
        };
        let start = Start {
            read_config: &read_config,
            startup: &broken,
        };
        let report = ensure_with(&a, &start);
        assert_eq!(report.status, Status::UsageError, "{}", report.to_json());
        assert_eq!(report.errors[0].cause, ErrorCause::Config);
        assert!(
            report.errors[0]
                .message
                .contains("invalid selection `profile:missing`"),
            "{}",
            report.to_json()
        );
        assert_eq!(
            started.get(),
            0,
            "the host started before the configuration"
        );
        // A configuration that loads: the host's failure is the run's.
        let good = ConfigFixture::new("ensure-good", "version: 1\n");
        let a = ensure_args(&[
            "--dry-run",
            "--config",
            &good.path(),
            "--min-free",
            "1G",
            "--select",
            "profile:cargo-target",
            "--roots",
            "/tmp",
            "/tmp",
        ])
        .expect("parses");
        let report = ensure_with(&a, &start);
        assert_eq!(report.status, Status::Error, "{}", report.to_json());
        assert_eq!(report.errors[0].cause, ErrorCause::Host);
        assert_eq!(started.get(), 1);
    }

    /// A reader of the configuration file as `/proc` unavailable makes it:
    /// the checked file's reopen fails (diskgc-linux's
    /// `a_config_read_that_fails_on_proc_is_the_hosts` pins that the real
    /// reader says so), through the real lookup.
    pub(super) fn read_config_without_proc(flag: Option<&OsStr>, _: bool) -> ConfigRead {
        read_config_with(flag, &|path, _| {
            Err(ConfigReadError::Host(diskgc_core::host::HostError::errno(
                "reopen through /proc",
                path.display(),
                Errno::ENOENT,
            )))
        })
    }

    /// TASK-272 review (Codex 1): a valid configuration that cannot be
    /// read because `/proc` is unavailable is the host's failure, exit 3
    /// (`error`, cause `host`) as before the configuration moved first; only
    /// a file read and found wanting is exit 2.
    #[test]
    fn ensure_with_a_valid_config_and_no_proc_is_exit_3_host() {
        let good = ConfigFixture::new("ensure-no-proc", "version: 1\n");
        let a = ensure_args(&[
            "--dry-run",
            "--config",
            &good.path(),
            "--min-free",
            "1G",
            "/tmp",
        ])
        .expect("parses");
        let start = Start {
            read_config: &read_config_without_proc,
            startup: &|_| panic!("the host starts after the configuration is read"),
        };
        let report = ensure_with(&a, &start);
        assert_eq!(report.status, Status::Error, "{}", report.to_json());
        assert_eq!(report.exit_code(), 3);
        assert_eq!(report.errors[0].cause, ErrorCause::Host);
        assert!(
            report.errors[0].message.contains("reopen through /proc"),
            "{}",
            report.to_json()
        );
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
    /// builtin.yaml's `releases:` (Library::release_catalog) through this
    /// version's row of the release descriptor, and this
    /// list is the second key a widening of the catalog must turn (with
    /// TASK-65's sign-off for `/tmp`, Mark's admission of `nix-garbage` for
    /// 0.2, TASK-335), not a stale copy to derive away.
    #[test]
    fn release_catalog_keeps_only_its_admitted_built_ins_on_every_listing_surface() {
        let row = release::release_of_version(env!("CARGO_PKG_VERSION")).expect("this release");
        let library = Catalog::from(row)
            .library(Library::builtin().expect("built-ins"))
            .expect("release catalog");
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
                "idle-7d",
                "nix-garbage",
                "nix-shell-rc",
                "pytest-tmp"
            ]
        );
        for surface in [
            describe::list_text(&library),
            describe::help_section(&library),
        ] {
            assert!(surface.contains("cargo-target"));
            assert!(surface.contains("nix-garbage"));
            assert!(surface.contains("nix-shell-rc"));
            assert!(surface.contains("pytest-tmp"));
            assert!(!surface.contains("docker-build-cache"));
            assert!(!surface.contains("pip-cache"));
            assert!(!surface.contains("pytest-cache"));
        }
        assert!(library.get("docker-build-cache").is_none());
        assert!(library.get("pip-cache").is_none());
        assert!(library.get("pytest-cache").is_none());
    }

    #[test]
    fn release_catalog_refuses_excluded_select_before_a_tier_can_reach_a_backend() {
        let all = Library::builtin().expect("built-ins");
        let row = release::release_of_version(env!("CARGO_PKG_VERSION")).expect("this release");
        let library = Catalog::from(row)
            .library(all.clone())
            .expect("release catalog");
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
        for name in ["pytest-tmp", "nix-shell-rc"] {
            Tier::select(
                &format!("profile:{name} && filter:idle-1d"),
                &library,
                None,
                1000,
            )
            .expect(name);
        }
        Tier::select("profile:nix-garbage", &library, None, 1000).expect("nix-garbage");
    }

    #[cfg(feature = "release-catalog")]
    #[test]
    fn opt_in_binary_uses_the_narrowed_library_for_help_and_select() {
        let library = builtin_library().expect("binary catalog");
        assert_eq!(library.profiles().count(), 11);
        assert!(library.get("nix-garbage").is_some());
        assert!(library.get("docker-build-cache").is_none());
        let error = Tier::select("profile:docker-build-cache", &library, None, 1000)
            .expect_err("excluded source");
        assert!(error.contains("docker-build-cache"), "{error}");
    }

    #[test]
    fn a_development_version_names_its_commit_and_a_release_does_not() {
        assert_eq!(
            version_line("0.2.0-dev", Some("9c1e331")),
            "diskgc 0.2.0-dev (9c1e331)\n"
        );
        assert_eq!(
            version_line("0.2.0-dev", Some("9c1e331-dirty")),
            "diskgc 0.2.0-dev (9c1e331-dirty)\n"
        );
        assert_eq!(
            version_line("0.2.0-dev", None),
            "diskgc 0.2.0-dev (unknown)\n"
        );
        assert_eq!(
            version_line("0.2.0-dev", Some("")),
            "diskgc 0.2.0-dev (unknown)\n"
        );
        assert_eq!(version_line("0.2.0", Some("9c1e331")), "diskgc 0.2.0\n");
        assert_eq!(version_line("0.1.0", None), "diskgc 0.1.0\n");
    }

    fn rows() -> [release::Entry; 3] {
        [
            release::development().expect("development"),
            release::release("0.1").expect("0.1"),
            release::release_of_version(env!("CARGO_PKG_VERSION")).expect("this release"),
        ]
    }

    /// TASK-289 review (Codex 1): a release-catalog build of a version with
    /// no row answers `--version` and refuses everything else with the
    /// descriptor's error (exit 2, a JSON object under `--json`), before
    /// the parser could ask for the row: `prune --json` panicked.
    #[test]
    fn a_build_without_a_row_refuses_before_parsing() {
        let missing = release::release_of_version("9.9.0-dev");
        assert!(missing.is_err(), "no row 9.9 in the descriptor");
        for line in [
            &["prune", "--json"][..],
            &["prune"],
            &[],
            &["ensure", "--min-free", "1G", "/p"],
            &["--help"],
            &["--version", "extra"],
        ] {
            match begin(&args(line), missing.clone()) {
                Begin::NoRow(why) => assert_eq!(Err(why), missing, "{line:?}"),
                other => panic!("{line:?}: {other:?}"),
            }
        }
        for line in [
            &["--version"][..],
            &["-V", "--json"],
            &["--json", "--version"],
        ] {
            assert_eq!(
                begin(&args(line), missing.clone()),
                Begin::Version,
                "{line:?}"
            );
        }
        // With a row, the same lines parse and are admitted.
        let row = build();
        assert_eq!(
            begin(&args(&["prune"]), Ok(row)),
            Begin::Run(super::parse(&args(&["prune"]), &row), row)
        );
        assert_eq!(
            begin(&args(&["--version", "--help"]), Ok(row)),
            Begin::Run(Invocation::Help, row)
        );
    }

    /// Each row runs what it lists and refuses the rest (exit 2), however
    /// it is written; a syntax error of a command it runs stays one.
    #[test]
    fn a_build_runs_only_its_rows_commands() {
        let reclaim = args(&["watch", "--min-free", "1G", "--target-free", "2G", "/p"]);
        let tally = args(&["watch", "--tally-only", "/p"]);
        let bad_watch = args(&["watch", "--min-free", "1G", "/p"]);
        for row in rows() {
            let admitted = |line: &[OsString]| admit(line, super::parse(line, &row), &row);
            match admitted(&reclaim) {
                Invocation::Watch(_) => assert!(row.allows(Command::Watch), "{row:?}"),
                Invocation::NotInBuild(Command::Watch) => {
                    assert!(!row.allows(Command::Watch), "{row:?}")
                }
                other => panic!("{row:?}: {other:?}"),
            }
            match admitted(&tally) {
                Invocation::Watch(_) => assert!(row.allows(Command::WatchTallyOnly)),
                Invocation::NotInBuild(Command::Watch) => {
                    assert!(!row.allows(Command::WatchTallyOnly))
                }
                other => panic!("{row:?}: {other:?}"),
            }
            // `survey`, read-only, runs where its row lists it; a syntax
            // error of it is refused the same way where it does not.
            for line in [&["survey", "/p"][..], &["survey"]] {
                let line = args(line);
                match admitted(&line) {
                    Invocation::Survey(_) | Invocation::UsageError(_) => {
                        assert!(row.allows(Command::Survey), "{row:?} {line:?}")
                    }
                    Invocation::NotInBuild(Command::Survey) => {
                        assert!(!row.allows(Command::Survey), "{row:?} {line:?}")
                    }
                    other => panic!("{row:?}: {other:?}"),
                }
            }
            let runs_a_watch = row.allows(Command::Watch) || row.allows(Command::WatchTallyOnly);
            match admitted(&bad_watch) {
                Invocation::UsageError(_) => assert!(runs_a_watch, "{row:?}"),
                Invocation::NotInBuild(Command::Watch) => assert!(!runs_a_watch, "{row:?}"),
                other => panic!("{row:?}: {other:?}"),
            }
            // Every row runs these.
            for line in [
                &["check"][..],
                &["profiles", "list"],
                &["ensure", "--min-free", "1G", "/p"],
            ] {
                let line = args(line);
                let parsed = || super::parse(&line, &row);
                assert_eq!(admit(&line, parsed(), &row), parsed(), "{line:?}");
            }
        }
    }

    /// What the binary prints for a refused command (the release suite's
    /// `release_candidate_refuses_a_reclaiming_watch` runs this build's, in
    /// a container: TASK-282 keeps reclaiming argv off the host).
    #[test]
    fn a_refusal_names_the_rows_commands() {
        let [development, v01, _] = rows();
        let mut tally_only = (*release::release("0.1").expect("0.1").build).clone();
        tally_only.commands = vec![
            Command::Ensure,
            Command::WatchTallyOnly,
            Command::Check,
            Command::Profiles,
        ];
        assert_eq!(
            not_in_build(Command::Watch, &tally_only),
            concat!(
                "a reclaiming `watch` is not in diskgc ",
                env!("CARGO_PKG_VERSION"),
                " yet; its commands are ensure, watch --tally-only, check and profiles"
            )
        );
        assert_eq!(
            not_in_build(Command::Watch, &v01),
            concat!(
                "`watch` is not in diskgc ",
                env!("CARGO_PKG_VERSION"),
                " yet; its commands are ensure, check and profiles"
            )
        );
        assert_eq!(
            development.commands_phrase(),
            "ensure, watch, check, profiles and survey"
        );
        assert_eq!(
            not_in_build(Command::Survey, &v01),
            concat!(
                "`survey` is not in diskgc ",
                env!("CARGO_PKG_VERSION"),
                " yet; its commands are ensure, check and profiles"
            )
        );
    }

    /// The help shows what its build runs (TASK-289: it advertised a
    /// reclaiming `watch` in every build): the usage lines, the `watch`
    /// paragraph, the commands sentence and what the package installs.
    #[test]
    fn the_help_follows_the_rows_commands_and_files() {
        let [development, v01, this_release] = rows();
        // Sentences are compared with their line breaks as spaces.
        let help_of = |row: &release::Entry| {
            let text = format!("{}{}", usage(row, None), help(row));
            let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
            (text, flat)
        };
        let reclaim_line = "diskgc watch --min-free SIZE --target-free SIZE";
        let tally_line = "diskgc watch --tally-only [--min-free SIZE]";

        let (_, dev) = help_of(&development);
        assert!(
            dev.contains(reclaim_line) && dev.contains(tally_line),
            "{dev}"
        );
        assert!(
            dev.contains("watch runs as a service: it reclaims"),
            "{dev}"
        );
        assert!(!dev.contains("not in it yet"), "{dev}");
        assert!(dev.contains("survey reads only"), "{dev}");
        assert!(
            dev.contains("Commands in this version: ensure, watch, check, profiles and survey"),
            "{dev}"
        );
        assert!(
            !dev.contains("release notes"),
            "the development package has none:\n{dev}"
        );

        let (_, v01) = help_of(&v01);
        assert!(!v01.contains("diskgc watch"), "{v01}");
        assert!(!v01.contains("watch --tally-only runs"), "{v01}");
        assert!(
            v01.contains("(`watch` and `survey` are not in it yet)"),
            "{v01}"
        );
        assert!(v01.contains("a README and the release notes"), "{v01}");

        // This version's release: its watch lines are exactly what its row
        // allows (TASK-276 decides whether a reclaiming one is added).
        let (_, rel) = help_of(&this_release);
        assert_eq!(
            rel.contains(reclaim_line),
            this_release.allows(Command::Watch),
            "{rel}"
        );
        assert_eq!(
            rel.contains(tally_line),
            this_release.allows(Command::WatchTallyOnly),
            "{rel}"
        );
        assert_eq!(
            rel.contains("watch runs as a service: it reclaims"),
            this_release.allows(Command::Watch),
            "{rel}"
        );
        assert!(
            rel.contains(&format!(
                "Commands in this version: {}",
                this_release.commands_phrase()
            )),
            "{rel}"
        );
        for row in rows() {
            let (text, _) = help_of(&row);
            // The prose; the usage lines are as wide as the synopses.
            for line in help(&row).lines() {
                assert!(line.len() <= 78, "{line:?} is wider than 78 columns");
            }
            assert!(
                text.contains("share/doc/diskgc/examples/example.yaml"),
                "{text}"
            );
        }
        // A usage error of `watch` shows only the lines its build runs.
        let usage_tally_only = {
            let mut row = (*this_release.build).clone();
            row.commands.retain(|c| *c != Command::Watch);
            if !row.commands.contains(&Command::WatchTallyOnly) {
                row.commands.push(Command::WatchTallyOnly);
            }
            usage(&row, Some("watch"))
        };
        assert!(usage_tally_only.contains(tally_line), "{usage_tally_only}");
        assert!(
            !usage_tally_only.contains(reclaim_line),
            "{usage_tally_only}"
        );
        assert!(!usage_tally_only.contains("ensure"), "{usage_tally_only}");
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

    /// TASK-148: a mistyped flag, subcommand or command names what was
    /// probably meant.
    #[test]
    fn mistyped_words_suggest_the_closest_known_one() {
        for (list, needle) in [
            (
                &["ensure", "--min-fre", "1G", "/p"][..],
                "ensure: unknown flag --min-fre; did you mean --min-free?",
            ),
            (
                &["ensure", "--min-free", "1G", "--dryrun", "/p"],
                "unknown flag --dryrun; did you mean --dry-run?",
            ),
            (
                &["ensure", "--min-free", "1G", "--frobnicate", "/p"],
                "unknown flag --frobnicate",
            ),
            (
                &["profiles", "list", "--jsno"],
                "unknown flag --jsno; did you mean --json?",
            ),
            (
                &["profiles", "list", "--confg=/c.yaml"],
                "unknown flag --confg; did you mean --config?",
            ),
            (
                &["profiles", "lst"],
                "unknown subcommand lst; did you mean list?",
            ),
            (
                &["check", "--conifg", "/c.yaml"],
                "check: unknown flag --conifg; did you mean --config?",
            ),
            (&["help"], "unknown command `help`; did you mean `--help`?"),
            (
                &["ensur"],
                "unknown command `ensur`; did you mean `ensure`?",
            ),
            (
                &["prune"],
                "unknown command `prune`; the commands are ensure, ",
            ),
            (&["--version", "ensure"], "--version takes no arguments"),
        ] {
            match parse(&args(list)) {
                Invocation::UsageError(error) => {
                    assert!(error.contains(needle), "{list:?}: {error}");
                }
                other => panic!("{list:?}: {other:?}"),
            }
        }
        // The row's own commands (the release descriptor's).
        for row in rows() {
            assert_eq!(
                super::parse(&args(&["prune"]), &row),
                Invocation::UsageError(format!(
                    "unknown command `prune`; the commands are {}",
                    row.commands_phrase()
                ))
            );
        }
    }

    /// The "did you mean" lists hold exactly what the parsers accept and
    /// the usage lines show.
    #[test]
    fn flag_lists_match_the_parsers_and_the_usage() {
        for flag in ENSURE_FLAGS {
            assert!(
                ENSURE_SYNOPSIS.contains(&format!("{flag} "))
                    || ENSURE_SYNOPSIS.contains(&format!("{flag}]")),
                "{flag}"
            );
            if let Err(error) = ensure_args(&[flag, "1G", "/p"]) {
                assert!(!error.contains("unknown flag"), "{flag}: {error}");
            }
        }
        for flag in PROFILES_FLAGS {
            assert!(PROFILES_SYNOPSIS.contains(flag), "{flag}");
            if let Err(error) = profiles_args(&["list", flag, "/c.yaml"]) {
                assert!(!error.contains("unknown flag"), "{flag}: {error}");
            }
        }
        for flag in watch::WATCH_FLAGS {
            assert!(
                watch::WATCH_RECLAIM_SYNOPSIS.contains(flag)
                    || watch::WATCH_TALLY_SYNOPSIS.contains(flag),
                "{flag}"
            );
            let error = watch::parse_watch(&args(&[flag, "1G", "/p"])).err();
            assert!(!error.is_some_and(|e| e.contains("unknown flag")), "{flag}");
        }
        for flag in survey::SURVEY_FLAGS {
            assert!(survey::SURVEY_SYNOPSIS.contains(flag), "{flag}");
            let error = survey::parse_survey(&args(&[flag, "x", "/p"])).err();
            assert!(!error.is_some_and(|e| e.contains("unknown flag")), "{flag}");
        }
        for flag in CHECK_FLAGS {
            assert!(CHECK_SYNOPSIS.contains(flag), "{flag}");
            assert!(matches!(
                parse(&args(&["check", flag, "/c.yaml"])),
                Invocation::Check(_)
            ));
        }
    }

    /// TASK-153: what a user reads has no task numbers or source-tree
    /// paths: the release package ships neither.
    #[test]
    fn help_and_refusals_name_no_internal_tasks_or_source_paths() {
        let library = builtin_library().expect("the build's catalog");
        let help = format!(
            "{}{}{}",
            usage(&build(), None),
            help(&build()),
            describe::help_section(&library)
        );
        for leak in ["task", "TASK", "docs/", "source tree", "later tasks"] {
            assert!(!help.contains(leak), "{leak:?} in --help:\n{help}");
        }
        assert!(help.contains("/etc/diskgc/config.yaml"), "{help}");
        assert!(
            help.contains("share/doc/diskgc/examples/example.yaml"),
            "{help}"
        );
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
