//! `diskgc survey` (docs/reference.md, `survey`): the command line, and
//! the wiring of `diskgc_core::survey` to `LinuxHost`. Read-only: the host
//! is started without a state directory, so no index snapshot is read or
//! written, and the survey is given the host as a plain `Host` (it cannot
//! reach a deleting method).
//!
//! stdout carries only the result (`--json`: one object); stderr the
//! progress log and errors.

use std::ffi::OsString;
use std::path::{Component, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use diskgc_core::invocation::Platform;
use diskgc_core::survey::{self, Survey, SurveyOptions};
use diskgc_linux::LinuxHost;

use super::{
    Start, builtins, catalog, draw_seed, log, output, set, text, unknown_flag, zero_or_duration,
};
use diskgc_core::report::ErrorCause;

/// `--timeout`'s default: as `ensure`'s, a survey of a large tree is cut
/// short, never left running unbounded by a script (`0`: none).
const TIMEOUT_DEFAULT: Duration = Duration::from_secs(10 * 60);

/// `survey`'s synopsis as the `usage:` lines print it.
pub(super) const SURVEY_SYNOPSIS: &str = "\
diskgc survey [--select EXPR] [--roots DIR]... [--in-use] [--list] [--json]
                     [--timeout DUR] [--config FILE] PATH...";

/// The flags `survey` takes, for "did you mean" (the parser is the
/// authority; a test holds the list to it and to the usage).
pub(super) const SURVEY_FLAGS: [&str; 7] = [
    "--select",
    "--roots",
    "--in-use",
    "--list",
    "--json",
    "--timeout",
    "--config",
];

/// `survey`'s arguments, checked for syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SurveyArgs {
    pub(super) options: SurveyOptions,
    /// `--list`: the human text lists the candidates (the JSON always
    /// does).
    pub(super) list: bool,
    pub(super) config: Option<OsString>,
}

pub(super) fn parse_survey(args: &[OsString]) -> Result<SurveyArgs, String> {
    let (mut select, mut timeout, mut config) = (None, None, None);
    let mut roots = Vec::new();
    let (mut list, mut json, mut in_use) = (false, false, false);
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
            "--list" => switch(&mut list)?,
            "--in-use" => switch(&mut in_use)?,
            // Validated only: the output is `json_requested`'s.
            "--json" => switch(&mut json)?,
            "--select" | "--roots" | "--timeout" | "--config" => {
                let value = match inline {
                    Some(value) => value,
                    None => rest
                        .next()
                        .cloned()
                        .ok_or_else(|| format!("{name} needs a value"))?,
                };
                match name {
                    "--select" => set(&mut select, name, text(&value).map(str::to_owned))?,
                    "--roots" => {
                        roots.push(text(&value).map_err(|e| format!("{name}: {e}"))?.to_owned());
                    }
                    "--timeout" => {
                        set(&mut timeout, name, text(&value).and_then(zero_or_duration))?;
                    }
                    _ => set(&mut config, name, Ok(value))?,
                }
            }
            _ => return Err(unknown_flag(name, &SURVEY_FLAGS)),
        }
    }
    if paths.is_empty() {
        return Err("PATH is required: the directory to survey (one or more)".to_owned());
    }
    if let Some(path) = paths.iter().find(|p| !p.is_absolute()) {
        return Err(format!(
            "PATH {} is not absolute: name the directory exactly",
            path.display()
        ));
    }
    if let Some(path) = paths
        .iter()
        .find(|p| p.components().any(|c| c == Component::ParentDir))
    {
        return Err(format!(
            "PATH {} has a `..`: name the directory exactly",
            path.display()
        ));
    }
    if !roots.is_empty() && select.is_none() {
        return Err("--roots is only valid together with --select".to_owned());
    }
    Ok(SurveyArgs {
        options: SurveyOptions {
            // As written, less a trailing `/` and `.` parts (`~/` from tab
            // completion): each PATH is also the root of `roots: required`
            // profiles, and a root must be normalized.
            paths: paths.iter().map(|p| p.components().collect()).collect(),
            select,
            roots,
            timeout: match timeout {
                None => Some(TIMEOUT_DEFAULT),
                Some(Duration::ZERO) => None,
                Some(t) => Some(t),
            },
            in_use,
        },
        list,
        config,
    })
}

/// Runs `survey` and prints its result.
pub(super) fn survey(args: &SurveyArgs, json: bool) -> ExitCode {
    let result = survey_with(args, &Start::MACHINE);
    let body = if json {
        format!("{}\n", result.to_json())
    } else {
        result.to_text(args.list)
    };
    output(&body, result.exit_code())
}

/// [`survey`]'s result, with what it reads from the machine to start given
/// (the configuration file, then the host's startup checks).
fn survey_with(args: &SurveyArgs, start: &Start<'_>) -> Survey {
    let fail = |cause: ErrorCause, why: String| {
        log(&format!("error: {why}"));
        Survey::refused(args.options.in_use, cause, why)
    };
    if let Err(error) = LinuxHost::install_interrupt_handlers() {
        return fail(
            ErrorCause::Host,
            format!("install signal handlers: {error}"),
        );
    }
    let config = match (start.read_config)(args.config.as_deref(), false) {
        Ok(config) => config,
        Err((cause, why)) => return fail(cause, why),
    };
    let platform = Platform {
        catalog: catalog(),
        draw_seed: &draw_seed,
        builtins: &builtins,
    };
    let loaded = match survey::load(&args.options, &config, &platform, &mut log) {
        Ok(loaded) => loaded,
        Err((cause, why)) => return fail(cause, why),
    };
    // No state directory: a survey reads and writes no snapshot.
    let host = match (start.startup)(None) {
        Ok(host) => host,
        Err(failure) => return fail(failure.cause(), failure.message()),
    };
    survey::run(&host, &args.options, &loaded, &mut log, &mut log)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(list: &[&str]) -> Result<SurveyArgs, String> {
        parse_survey(&list.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn survey_parses_paths_and_flags_in_both_spellings() {
        let a = parse(&["/tmp", "--list", "--json", "/home/m", "--timeout=0"]).unwrap();
        assert_eq!(
            a.options.paths,
            [PathBuf::from("/tmp"), PathBuf::from("/home/m")]
        );
        assert!(a.list);
        assert!(!a.options.in_use, "the in-use check is opt-in");
        assert_eq!(a.options.timeout, None);
        assert!(parse(&["--in-use", "/tmp"]).unwrap().options.in_use);
        let a = parse(&["/home/m/", "/tmp/./x"]).unwrap();
        assert_eq!(
            a.options.paths,
            [PathBuf::from("/home/m"), PathBuf::from("/tmp/x")],
            "normalized as roots must be"
        );
        let a = parse(&[
            "--select",
            "profile:cargo-target",
            "--roots=/src",
            "--roots",
            "/w",
            "--config",
            "/c.yaml",
            "/",
        ])
        .unwrap();
        assert_eq!(a.options.select.as_deref(), Some("profile:cargo-target"));
        assert_eq!(a.options.roots, ["/src", "/w"]);
        assert_eq!(a.options.timeout, Some(TIMEOUT_DEFAULT));
        assert_eq!(a.config, Some(OsString::from("/c.yaml")));
        assert!(!a.list);
    }

    #[test]
    fn survey_usage_errors_say_why() {
        for (args, why) in [
            (&[][..], "PATH is required"),
            (&["tmp"][..], "not absolute"),
            (
                &["--roots", "/x", "/tmp"][..],
                "only valid together with --select",
            ),
            (&["--list", "--list", "/tmp"][..], "given twice"),
            (&["--in-use=yes", "/tmp"][..], "takes no value"),
            (&["--list=yes", "/tmp"][..], "takes no value"),
            (&["--timeout", "5", "/tmp"][..], "--timeout"),
            (&["--min-free", "5G", "/tmp"][..], "unknown flag --min-free"),
            (&["--lst", "/tmp"][..], "did you mean --list?"),
            (&["/tmp/../etc"][..], "has a `..`"),
            (&["/tmp", "--select"][..], "--select needs a value"),
        ] {
            let error = parse(args).expect_err(&format!("{args:?}"));
            assert!(error.contains(why), "{args:?}: {error}");
        }
    }
}
