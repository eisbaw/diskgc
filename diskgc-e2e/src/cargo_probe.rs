//! The Cargo helper processes (docs/testing.md, Cargo race tests), run as
//! `diskgc-e2e probe NAME ...`: fixtures, not code under test. They make a
//! Cargo race deterministic by stopping Cargo at a known point:
//!
//! - `flock-hold [--shared] FILE...`: takes an exclusive (or shared) `flock`
//!   on each FILE without waiting (fails if it cannot), prints `ready` and
//!   holds them until its stdin closes. Holding `<profile>/.cargo-lock`
//!   makes a Cargo build block there; a Cargo that takes
//!   `.cargo-build-lock` first blocks holding it.
//! - `flock-hold-existing [--shared] FILE...`: the same lock, but refuses
//!   missing files. Model materialization uses it so an absent fixture
//!   cannot be silently created by the helper.
//! - `cargo-runner BINARY ARGS...`: a Cargo `runner` (set through
//!   [`RUNNER_ENV`]): Cargo starts it after compiling, with the binary and
//!   its arguments; it waits at the gate named by [`RUNNER_GATE_ENV`] (the
//!   gate protocol of [`crate::cargo_under_test::Gate`]) and then execs the
//!   binary, so `cargo run` / `cargo test` pause between compilation and
//!   execution.
//! - `rusage -- PROGRAM ARGS...`: runs PROGRAM as its child, then prints
//!   the voluntary context switches of itself and every waited-for
//!   descendant (one per ptrace stop when traced) and its seccomp filter
//!   count on stderr, and exits as PROGRAM did. Used to bound strace's cost
//!   on a traced Cargo build.
//!
//! The third helper, a build script that waits at a gate, is fixture source
//! compiled by the Cargo under test (`fixtures/cargo/race/build.rs`).

use std::io::{BufRead, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use rustix::fs::FlockOperation;

use crate::cargo_under_test::{RUNNER_GATE_ENV, wait_at_gate};
use crate::exit;

/// The probe names this module handles.
pub const PROBES: &[&str] = &[
    "flock-hold",
    "flock-hold-existing",
    "cargo-runner",
    "rusage",
];

/// The variable Cargo reads the host target's runner from (an alternative
/// to `target.<triple>.runner` in `.cargo/config.toml`): program and
/// arguments, separated by spaces.
pub const RUNNER_ENV: &str = "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER";

/// Prefix of the line `rusage` prints on stderr.
pub const RUSAGE_PREFIX: &str = "diskgc-e2e-rusage:";

/// Runs probe `cmd` with `args`; returns the exit code.
pub fn main(cmd: &str, args: &[String]) -> i32 {
    let result = match (cmd, args) {
        ("flock-hold", [flag, files @ ..]) if flag == "--shared" && !files.is_empty() => {
            flock_hold(files, FlockOperation::NonBlockingLockShared, true)
        }
        ("flock-hold", files) if !files.is_empty() => {
            flock_hold(files, FlockOperation::NonBlockingLockExclusive, true)
        }
        ("flock-hold-existing", [flag, files @ ..]) if flag == "--shared" && !files.is_empty() => {
            flock_hold(files, FlockOperation::NonBlockingLockShared, false)
        }
        ("flock-hold-existing", files) if !files.is_empty() => {
            flock_hold(files, FlockOperation::NonBlockingLockExclusive, false)
        }
        ("cargo-runner", [binary, rest @ ..]) => cargo_runner(Path::new(binary), rest),
        ("rusage", [sep, program, rest @ ..]) if sep == "--" => return rusage(program, rest),
        _ => {
            eprintln!("diskgc-e2e probe {cmd}: bad arguments {args:?}");
            return exit::USAGE;
        }
    };
    match result {
        Ok(()) => exit::PASS,
        Err(e) => {
            eprintln!("diskgc-e2e probe {cmd} {args:?}: {e}");
            exit::FAIL
        }
    }
}

/// Locks every file without waiting, says `ready`, holds until stdin closes.
/// `create` is only for the original Cargo race fixture probe.
fn flock_hold(files: &[String], op: FlockOperation, create: bool) -> Result<(), String> {
    let mut held = Vec::new();
    for file in files.iter().map(PathBuf::from) {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .truncate(false)
            .open(&file)
            .map_err(|e| format!("open {}: {e}", file.display()))?;
        rustix::fs::flock(&f, op)
            .map_err(|e| format!("flock {op:?} {} without waiting: {e}", file.display()))?;
        held.push(f);
    }
    let mut out = std::io::stdout();
    writeln!(out, "ready")
        .and_then(|()| out.flush())
        .map_err(|e| format!("write stdout: {e}"))?;
    let mut line = String::new();
    while std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("read stdin: {e}"))?
        > 0
    {
        line.clear();
    }
    drop(held);
    Ok(())
}

/// `cargo-runner`: waits at the gate in [`RUNNER_GATE_ENV`], then execs the
/// binary Cargo built. Returns only on failure.
fn cargo_runner(binary: &Path, args: &[String]) -> Result<(), String> {
    let gate = std::env::var_os(RUNNER_GATE_ENV)
        .map(PathBuf::from)
        .ok_or_else(|| format!("{RUNNER_GATE_ENV} is not set: the runner has no gate"))?;
    wait_at_gate(&gate)?;
    // The gate variable is the runner's, not the program's.
    let error = Command::new(binary)
        .args(args)
        .env_remove(RUNNER_GATE_ENV)
        .exec();
    Err(format!("exec {}: {error}", binary.display()))
}

/// `rusage`: runs `program` and reports the context switches of the whole
/// process tree it waited for.
fn rusage(program: &str, args: &[String]) -> i32 {
    let status = match Command::new(program).args(args).status() {
        Ok(status) => status,
        Err(e) => {
            eprintln!("diskgc-e2e probe rusage: spawn {program}: {e}");
            return exit::FAIL;
        }
    };
    // u64::MAX (a failed getrusage) stays u64::MAX, which the parser refuses.
    let switches = voluntary_switches(libc::RUSAGE_SELF)
        .saturating_add(voluntary_switches(libc::RUSAGE_CHILDREN));
    let filters = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Seccomp_filters:"))
                .map(|v| v.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned());
    eprintln!("{RUSAGE_PREFIX} voluntary_ctxt_switches={switches} seccomp_filters={filters}");
    match (status.code(), status.signal()) {
        (Some(code), _) => code,
        (None, signal) => {
            eprintln!("diskgc-e2e probe rusage: {program} ended by signal {signal:?}");
            exit::FAIL
        }
    }
}

/// `ru_nvcsw` of `getrusage(who)`: summed over all threads; for
/// `RUSAGE_CHILDREN`, over every descendant waited for by its parent.
fn voluntary_switches(who: libc::c_int) -> u64 {
    // SAFETY: getrusage writes one plain-data struct we own; zeroed is a
    // valid value for it.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: a valid `who` and a pointer to a live rusage.
    let rc = unsafe { libc::getrusage(who, &mut usage) };
    if rc != 0 {
        eprintln!(
            "diskgc-e2e probe rusage: getrusage: {}",
            std::io::Error::last_os_error()
        );
        return u64::MAX;
    }
    u64::try_from(usage.ru_nvcsw).unwrap_or(u64::MAX)
}

/// The numbers a `rusage` probe printed on `stderr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rusage {
    pub voluntary_switches: u64,
    pub seccomp_filters: u64,
}

/// Finds the last [`RUSAGE_PREFIX`] line in `stderr`.
pub fn parse_rusage(stderr: &str) -> Result<Rusage, String> {
    let line = stderr
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix(RUSAGE_PREFIX))
        .ok_or_else(|| format!("no {RUSAGE_PREFIX} line in stderr:\n{stderr}"))?;
    let field = |name: &str| -> Result<u64, String> {
        line.split_whitespace()
            .find_map(|kv| kv.strip_prefix(name)?.strip_prefix('='))
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| format!("no numeric {name} in {RUSAGE_PREFIX}{line}"))
    };
    let parsed = Rusage {
        voluntary_switches: field("voluntary_ctxt_switches")?,
        seccomp_filters: field("seccomp_filters")?,
    };
    if parsed.voluntary_switches == u64::MAX {
        return Err(format!("getrusage failed in the probe: {line}"));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_rusage_line_is_parsed() {
        let stderr = "   Compiling x\ndiskgc-e2e-rusage: voluntary_ctxt_switches=1 seccomp_filters=0\n\
                      diskgc-e2e-rusage: voluntary_ctxt_switches=4321 seccomp_filters=3\n";
        assert_eq!(
            parse_rusage(stderr).unwrap(),
            Rusage {
                voluntary_switches: 4321,
                seccomp_filters: 3
            }
        );
        assert!(parse_rusage("nothing\n").is_err());
        assert!(
            parse_rusage("diskgc-e2e-rusage: voluntary_ctxt_switches=1 seccomp_filters=unknown\n")
                .is_err()
        );
        let failed = format!(
            "diskgc-e2e-rusage: voluntary_ctxt_switches={} seccomp_filters=1\n",
            u64::MAX
        );
        assert!(parse_rusage(&failed).is_err());
    }
}
