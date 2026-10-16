//! The `diskgc` binary: CLI and output, wiring `diskgc-core` to `LinuxHost`.
//!
//! Scaffold only: `--version` and a usage message. The real commands
//! (`ensure`, `watch`, `check`, `survey`, `profiles`) arrive with their tasks;
//! the CLI contract is docs/reference.md.

use std::process::ExitCode;

/// Exit code for usage errors (docs/reference.md, Exit codes).
const EXIT_USAGE: u8 = 2;

const USAGE: &str = "\
usage: diskgc --version
       diskgc --help

No commands are implemented yet in this build.
The CLI contract is docs/reference.md.
";

/// What the command line asks for. Kept separate from printing so the
/// decision is a pure function of the arguments.
#[derive(Debug, PartialEq, Eq)]
enum Invocation {
    Version,
    Help,
    UsageError(String),
}

fn parse(args: &[String]) -> Invocation {
    match args {
        [flag] if flag == "--version" || flag == "-V" => Invocation::Version,
        [flag] if flag == "--help" || flag == "-h" => Invocation::Help,
        [] => Invocation::UsageError("no command given".to_owned()),
        _ => Invocation::UsageError(format!("unsupported arguments: {}", args.join(" "))),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse(&args) {
        Invocation::Version => {
            println!("diskgc {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Invocation::Help => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Invocation::UsageError(reason) => {
            eprint!("diskgc: {reason}\n{USAGE}");
            ExitCode::from(EXIT_USAGE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
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
    }
}
