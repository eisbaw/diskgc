//! `diskgc watch` (docs/reference.md, `watch`): the command line, and the
//! wiring of `diskgc_core::watch` to `LinuxHost` with the background tally
//! on a thread of its own ([`diskgc_core::watch::run_threaded`]).
//!
//! stdout carries only the cycle records (`--json`), one flushed line
//! each; a failed write exits 3 at once. stderr carries the log; a failed
//! stderr write is ignored (the log is best effort; the process keeps
//! protecting the disk).

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use diskgc_core::invocation::{EnsureOptions, Platform};
use diskgc_core::report::{ErrorCause, ErrorEntry, Report, Status};
use diskgc_core::units::{Size, parse_duration, parse_size};
use diskgc_core::watch::{self, Ended, Mode};
use diskgc_linux::LinuxHost;

use super::{
    CATALOG, EXIT_OUTPUT_ERROR, builtins, draw_seed, read_config, set, startup, text, unknown_flag,
};

/// `--interval`'s default and bounds (docs/reference.md, Watch flags).
const INTERVAL_DEFAULT: Duration = Duration::from_secs(10);
const INTERVAL_MIN: Duration = Duration::from_secs(1);
const INTERVAL_MAX: Duration = Duration::from_secs(60 * 60);
/// `--timeout`'s default and bounds.
const TIMEOUT_DEFAULT: Duration = Duration::from_secs(10 * 60);
const TIMEOUT_MIN: Duration = Duration::from_secs(60);
const TIMEOUT_MAX: Duration = Duration::from_secs(24 * 60 * 60);

pub(super) const WATCH_SYNOPSIS: &str = "\
diskgc watch --min-free SIZE --target-free SIZE [--interval DUR] [--timeout DUR]
                    [--select EXPR] [--roots DIR]... [--json] [--config FILE] [--allow-root] PATH
       diskgc watch --tally-only [--min-free SIZE]
                    [--select EXPR] [--roots DIR]... [--json] [--config FILE] [--allow-root] PATH";

/// The flags `watch` takes, for "did you mean" (the parser is the
/// authority; a test holds the list to it and to the usage).
pub(super) const WATCH_FLAGS: [&str; 10] = [
    "--min-free",
    "--target-free",
    "--interval",
    "--timeout",
    "--tally-only",
    "--select",
    "--roots",
    "--json",
    "--config",
    "--allow-root",
];

/// `watch`'s arguments, checked for syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WatchArgs {
    pub(super) mode: Mode,
    /// `PATH`, `--select`, `--roots` and `--allow-root` as for `ensure`; the
    /// goal fields are not used (the mode holds the goal).
    pub(super) options: EnsureOptions,
    pub(super) config: Option<OsString>,
}

fn duration_in(text: &str, flag: &str, min: Duration, max: Duration) -> Result<Duration, String> {
    let d = parse_duration(text)?;
    if d < min || d > max {
        return Err(format!(
            "{text} is out of range: {flag} is from {} s to {} s",
            min.as_secs(),
            max.as_secs()
        ));
    }
    Ok(d)
}

pub(super) fn parse_watch(args: &[OsString]) -> Result<WatchArgs, String> {
    let (mut min_free, mut target_free, mut interval, mut timeout) = (None, None, None, None);
    let mut select = None;
    let mut roots = Vec::new();
    let mut config = None;
    let (mut json, mut allow_root, mut tally_only) = (false, false, false);
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
            "--json" => switch(&mut json)?,
            "--allow-root" => switch(&mut allow_root)?,
            "--tally-only" => switch(&mut tally_only)?,
            "--dry-run" => {
                return Err(
                    "--dry-run is not a watch flag: preview a cycle with `ensure \
                            --dry-run` and the same thresholds"
                        .to_owned(),
                );
            }
            "--tally-budget" | "--no-index" => {
                return Err(format!(
                    "{name} is not a watch flag: watch's live index replaces the synchronous \
                     tally"
                ));
            }
            "--min-free" | "--target-free" | "--interval" | "--timeout" | "--select"
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
                    "--interval" => set(
                        &mut interval,
                        name,
                        text(value).and_then(|t| duration_in(t, name, INTERVAL_MIN, INTERVAL_MAX)),
                    )?,
                    "--timeout" => set(
                        &mut timeout,
                        name,
                        text(value).and_then(|t| {
                            if t == "0" {
                                Err("0: a cycle needs a deadline (an unbounded cycle would \
                                     leave the service waiting forever on a lock an orphaned \
                                     child holds)"
                                    .to_owned())
                            } else {
                                duration_in(t, name, TIMEOUT_MIN, TIMEOUT_MAX)
                            }
                        }),
                    )?,
                    "--select" => set(&mut select, name, text(value).map(str::to_owned))?,
                    "--roots" => {
                        roots.push(text(value).map_err(|e| format!("{name}: {e}"))?.to_owned());
                    }
                    _ => set(&mut config, name, Ok(value.clone()))?,
                }
            }
            _ => return Err(unknown_flag(name, &WATCH_FLAGS)),
        }
    }
    let mode = if tally_only {
        for (given, flag) in [
            (interval.is_some(), "--interval"),
            (timeout.is_some(), "--timeout"),
            (target_free.is_some(), "--target-free"),
        ] {
            if given {
                return Err(format!(
                    "{flag} is not valid with --tally-only: nothing samples for a goal or \
                     reclaims"
                ));
            }
        }
        Mode::TallyOnly { min_free }
    } else {
        let min_free = min_free.ok_or("--min-free SIZE is required (or --tally-only)")?;
        let target_free = target_free.ok_or(
            "--target-free SIZE is required: a watcher without two thresholds would reclaim a \
             sliver at every poll (equal to --min-free is allowed)",
        )?;
        Mode::Reclaim {
            min_free,
            target_free,
            interval: interval.unwrap_or(INTERVAL_DEFAULT),
            timeout: timeout.unwrap_or(TIMEOUT_DEFAULT),
        }
    };
    let path = match paths.as_slice() {
        [path] => path.clone(),
        [] => return Err("PATH is required: a directory on the filesystem to watch".to_owned()),
        more => return Err(format!("one PATH, not {}", more.len())),
    };
    if !path.is_absolute() {
        return Err(format!("PATH {} is not absolute", path.display()));
    }
    if !roots.is_empty() && select.is_none() {
        return Err("--roots is only valid together with --select".to_owned());
    }
    Ok(WatchArgs {
        mode,
        options: EnsureOptions {
            // Unused by watch (the mode holds the goal); placeholders.
            min_free: Size::Bytes(0),
            target_free: None,
            timeout: None,
            tally_budget: None,
            select,
            roots,
            dry_run: false,
            allow_root,
            no_index: false,
            path,
            test_entry_budget: None,
        },
        config,
    })
}

/// One log line on stderr; a failed write is ignored.
fn log(line: &str) {
    let _ = writeln!(io::stderr().lock(), "diskgc: {line}");
}

/// A start failure: one JSON object under `--json`, and the exit code.
fn refused(report: Option<Report>, exit_code: u8, json: bool) -> ExitCode {
    if let (true, Some(report)) = (json, report) {
        let mut out = io::stdout().lock();
        if writeln!(out, "{}", report.to_json())
            .and_then(|()| out.flush())
            .is_err()
        {
            return ExitCode::from(EXIT_OUTPUT_ERROR);
        }
    }
    ExitCode::from(exit_code)
}

/// Runs `watch` until a signal (exit 0), a start refusal (2 or 3) or a
/// failed stdout write (3).
pub(super) fn watch(args: &WatchArgs, json: bool) -> ExitCode {
    let path = args.options.path.as_path();
    let fail = |status: Status, cause: ErrorCause, why: String| {
        log(&format!("error: {why}"));
        let mut report = Report::new(status, Some(path), false);
        report.errors.push(ErrorEntry::run(cause, why));
        let code = report.exit_code();
        refused(Some(report), code, json)
    };
    match LinuxHost::install_watch_handlers() {
        Ok(true) => {}
        Ok(false) => log("SIGHUP was ignored at start (nohup): it stays ignored"),
        Err(e) => {
            return fail(
                Status::Error,
                ErrorCause::Host,
                format!("install signal handlers: {e}"),
            );
        }
    }
    let host = match startup() {
        Ok(host) => host,
        Err(failure) => return fail(failure.status(), failure.cause(), failure.message()),
    };
    let config = match read_config(args.config.as_deref(), args.options.allow_root) {
        Ok(config) => config,
        Err(why) => return fail(Status::UsageError, ErrorCause::Config, why),
    };
    let platform = Platform {
        catalog: CATALOG,
        draw_seed: &draw_seed,
        builtins: &builtins,
    };
    let state_dir = diskgc_core::snapshot::state_dir(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    );
    let mut log_mut = log;
    let started = match watch::start(
        &host,
        &args.mode,
        &args.options,
        &config,
        &platform,
        state_dir.as_deref(),
        &mut log_mut,
    ) {
        Ok(started) => started,
        Err(refusal) => {
            if let Some(report) = &refusal.report {
                for error in &report.errors {
                    log(&format!("error: {}", error.message));
                }
            }
            return refused(refusal.report.map(|r| *r), refusal.exit_code, json);
        }
    };
    let mut write_record = |line: &str| -> Result<(), String> {
        let mut out = io::stdout().lock();
        writeln!(out, "{line}")
            .and_then(|()| out.flush())
            .map_err(|e| e.to_string())
    };
    let record: Option<watch::Record<'_>> = if json { Some(&mut write_record) } else { None };
    match watch::run_threaded(&host, started, record, &log) {
        Ended::Stopped(_) => {
            log("stopped");
            ExitCode::SUCCESS
        }
        Ended::OutputFailed(why) => {
            log(&format!(
                "error: could not write a record to stdout: {why}; exiting (the stream's \
                 consumer would miss deletions)"
            ));
            ExitCode::from(EXIT_OUTPUT_ERROR)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn parse(list: &[&str]) -> Result<WatchArgs, String> {
        parse_watch(&args(list))
    }

    #[test]
    fn watch_parses_both_modes_with_their_defaults() {
        let w = parse(&["--min-free", "30G", "--target-free", "60G", "/home"]).unwrap();
        assert_eq!(
            w.mode,
            Mode::Reclaim {
                min_free: parse_size("30G").unwrap(),
                target_free: parse_size("60G").unwrap(),
                interval: INTERVAL_DEFAULT,
                timeout: TIMEOUT_DEFAULT,
            }
        );
        let t = parse(&[
            "--tally-only",
            "--select",
            "profile:x",
            "--roots=/r",
            "/home",
        ])
        .unwrap();
        assert_eq!(t.mode, Mode::TallyOnly { min_free: None });
        assert_eq!(t.options.roots, ["/r"]);
        let t = parse(&["--tally-only", "--min-free=5G", "/home"]).unwrap();
        assert_eq!(
            t.mode,
            Mode::TallyOnly {
                min_free: Some(parse_size("5G").unwrap())
            }
        );
    }

    #[test]
    fn watch_usage_errors_say_why() {
        let e = parse(&["--min-free", "30G", "/home"]).unwrap_err();
        assert!(e.contains("--target-free SIZE is required"), "{e}");
        assert!(e.contains("sliver"), "{e}");
        for flag in ["--interval", "--timeout", "--target-free"] {
            let value = if flag == "--target-free" {
                "60G"
            } else {
                "10m"
            };
            let e = parse(&["--tally-only", flag, value, "/home"]).unwrap_err();
            assert!(e.contains("not valid with --tally-only"), "{flag}: {e}");
        }
        for (flag, value) in [
            ("--interval", "0"),
            ("--interval", "500ms"),
            ("--interval", "2h"),
            ("--timeout", "0"),
            ("--timeout", "30s"),
            ("--timeout", "25h"),
        ] {
            let e = parse(&["--min-free", "1G", "--target-free", "2G", flag, value, "/h"]);
            assert!(e.is_err(), "{flag} {value}");
        }
        for flag in ["--dry-run", "--no-index"] {
            let e = parse(&["--tally-only", flag, "/home"]).unwrap_err();
            assert!(e.contains("not a watch flag"), "{e}");
        }
        let e = parse(&["--tally-only", "--tally-budget", "5s", "/home"]).unwrap_err();
        assert!(e.contains("not a watch flag"), "{e}");
        let e = parse(&["--tally-only", "home"]).unwrap_err();
        assert!(e.contains("not absolute"), "{e}");
        let e = parse(&["--tally-only", "--roots", "/r", "/home"]).unwrap_err();
        assert!(e.contains("only valid together with --select"), "{e}");
        let e = parse(&["--tally-only", "--intervall", "1s", "/home"]).unwrap_err();
        assert!(e.contains("did you mean --interval"), "{e}");
        // The bounds themselves are accepted.
        for (flag, value) in [
            ("--interval", "1s"),
            ("--interval", "1h"),
            ("--timeout", "1m"),
            ("--timeout", "24h"),
        ] {
            parse(&["--min-free", "1G", "--target-free", "1G", flag, value, "/h"])
                .unwrap_or_else(|e| panic!("{flag} {value}: {e}"));
        }
    }
}
