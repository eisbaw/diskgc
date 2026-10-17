//! Starting the code under test: as the unprivileged `tester` (uid 1000),
//! traced by strace (docs/testing.md, Container setup).
//!
//! strace runs as container root and starts `diskgc-e2e as-tester -- PROGRAM
//! ARGS`, which is traced from its first instruction, drops to tester
//! ([`exec_as_tester`]) and execs the program. The trace files go to the
//! root-only records mount, where the code under test cannot touch them.
//!
//! strace runs with `--seccomp-bpf`: a seccomp filter installed before the
//! exec stops the tracee only at the traced syscalls, instead of at every
//! syscall, so a traced Cargo build costs little more than an untraced one.
//! A refused filter (strace prints why and carries on without it) fails the
//! run rather than silently making it slow.
//!
//! Every traced run is entered in the test's [`Ledger`]; the deletion oracle
//! marks the runs it judged, and a test that leaves a traced run unjudged
//! fails (docs/testing.md, Grounding, acceptance signal 1).
//!
//! Two kernel rules shape this:
//!
//! - `strace -u tester` changes the child's uid before strace attaches,
//!   which makes the child non-dumpable, and attaching to a non-dumpable
//!   process needs CAP_SYS_PTRACE. Attaching first and dropping privileges
//!   inside the trace avoids that and fixes the groups exactly (no NSS).
//! - strace's `-y` reads `/proc/<pid>/fd/<n>` of the traced process, which a
//!   root tracer may do for a tester process only with CAP_SYS_PTRACE in the
//!   container's user namespace. Without it, fd arguments come out bare
//!   (`unlinkat(3, "a", 0)`) and relative paths cannot be resolved. So the
//!   runner adds `--cap-add SYS_PTRACE`; it is in the bounding set of
//!   container root only, and the code under test loses every capability
//!   when it drops to uid 1000.

use std::ffi::OsStr;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rustix::process::{Gid, Uid};

use crate::scratch::Scratch;
use crate::strace::{self, Event};
use crate::{TESTER, TESTER_NAME, exit, info};

/// strace in the base image.
pub const STRACE: &str = "/bin/strace";

/// strace options, before `-e trace=...` and `-o`: follow forks (required by
/// `--seccomp-bpf`) into one file per process, no attach/exit noise, fd
/// paths, every string hex-escaped (see [`crate::strace`]).
pub const STRACE_OPTIONS: &[&str] = &["--seccomp-bpf", "-f", "-ff", "-qq", "-y", "-xx"];

/// What a traced run of the code under test produced.
#[derive(Debug)]
pub struct Run {
    pub argv: Vec<String>,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub events: Vec<Event>,
    /// When strace was started (the oracle refuses runs that started before
    /// it took its "before" snapshot).
    pub started: Instant,
    /// Wall time from starting strace to its exit.
    pub took: Duration,
    judged: Arc<AtomicBool>,
}

impl Run {
    /// Fails unless the code under test exited with `code`.
    pub fn expect_code(&self, code: i32) -> Result<&Self, String> {
        if self.code == Some(code) {
            Ok(self)
        } else {
            Err(format!(
                "{:?} exited with {:?}, expected {code}\n--- stdout\n{}--- stderr\n{}",
                self.argv, self.code, self.stdout, self.stderr
            ))
        }
    }

    /// Called by the deletion oracle once it judged this run.
    pub(crate) fn mark_judged(&self) {
        self.judged.store(true, Ordering::SeqCst);
    }
}

/// The traced runs of one test and whether the oracle judged each. Shared
/// by reference with every run; thread-safe so tests may run code under
/// test from several threads.
#[derive(Debug, Default)]
pub struct Ledger {
    runs: Mutex<Vec<(Vec<String>, Arc<AtomicBool>)>>,
}

impl Ledger {
    pub fn new() -> Self {
        Self::default()
    }

    fn enter(&self, argv: &[String]) -> Arc<AtomicBool> {
        let judged = Arc::new(AtomicBool::new(false));
        self.runs
            .lock()
            .expect("ledger lock")
            .push((argv.to_vec(), Arc::clone(&judged)));
        judged
    }

    /// The argv of every run the oracle has not judged.
    pub fn unjudged(&self) -> Vec<Vec<String>> {
        self.runs
            .lock()
            .expect("ledger lock")
            .iter()
            .filter(|(_, judged)| !judged.load(Ordering::SeqCst))
            .map(|(argv, _)| argv.clone())
            .collect()
    }

    /// Fails unless the oracle judged every traced run.
    pub fn require_all_judged(&self) -> Result<(), String> {
        let unjudged = self.unjudged();
        if unjudged.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "{} traced run(s) of code under test never judged by the deletion oracle \
                 (arm an Oracle before the run and judge it after): {unjudged:?}",
                unjudged.len()
            ))
        }
    }
}

static RUN_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Lines in which strace says it could not install its seccomp filter (it
/// then traces every syscall, much slower, and carries on).
fn seccomp_refusals(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|l| l.starts_with("strace: ") && l.contains("seccomp"))
        .collect()
}

/// Runs `program args...` as `tester` under strace, with a minimal fixed
/// environment and `cwd` as working directory, collects the record and
/// enters the run in `ledger`.
pub fn run_as_tester<S: AsRef<OsStr>>(
    ledger: &Ledger,
    records: &Scratch,
    cwd: &Path,
    program: &Path,
    args: &[S],
) -> Result<Run, String> {
    let n = RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir: PathBuf = records.mkdir(&format!("strace-{n}"), 0o700, crate::ROOT)?;
    let prefix = "trace";
    let argv: Vec<String> = std::iter::once(program.display().to_string())
        .chain(
            args.iter()
                .map(|a| a.as_ref().to_string_lossy().into_owned()),
        )
        .collect();
    let self_exe =
        std::env::current_exe().map_err(|e| format!("cannot find own executable: {e}"))?;
    info!("running as {TESTER_NAME} under strace: {argv:?}");
    let judged = ledger.enter(&argv);

    let started = Instant::now();
    let output = Command::new(STRACE)
        .args(STRACE_OPTIONS)
        .arg("-e")
        .arg(format!("trace={}", strace::TRACED_SYSCALLS))
        .arg("-o")
        .arg(dir.join(prefix))
        .arg("--")
        .arg(&self_exe)
        .args(["as-tester", "--"])
        .arg(program)
        .args(args)
        .env_clear()
        .env("PATH", "/bin")
        .env("HOME", "/home/tester")
        .env("USER", TESTER_NAME)
        .env("LOGNAME", TESTER_NAME)
        .env("LANG", "C.UTF-8")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("spawn {STRACE}: {e}"))?;
    let took = started.elapsed();

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let refusals = seccomp_refusals(&stderr);
    if !refusals.is_empty() {
        return Err(format!(
            "strace ran {argv:?} without its seccomp filter (every syscall would stop the \
             tracee): {refusals:?}"
        ));
    }
    let events = strace::read_record(&dir, prefix)
        .map_err(|e| format!("{e}\n--- strace exit {:?}, stderr\n{stderr}", output.status))?;
    Ok(Run {
        argv,
        code: output.status.code(),
        stdout,
        stderr,
        events,
        started,
        took,
        judged,
    })
}

/// `diskgc-e2e as-tester -- PROGRAM ARGS...`: drops from container root to
/// tester (supplementary groups, then real/effective/saved gid, then uid,
/// so the capabilities go with the uid) and execs PROGRAM. Returns only on
/// failure.
///
/// Raw per-thread syscalls are correct here because this process is
/// single-threaded: nothing has spawned a thread before this point.
pub fn exec_as_tester(argv: &[String]) -> i32 {
    let [program, args @ ..] = argv else {
        eprintln!("diskgc-e2e as-tester: no program given");
        return exit::USAGE;
    };
    let gid = Gid::from_raw(TESTER.gid);
    let uid = Uid::from_raw(TESTER.uid);
    let dropped = rustix::thread::set_thread_groups(&[gid])
        .and_then(|()| rustix::thread::set_thread_res_gid(gid, gid, gid))
        .and_then(|()| rustix::thread::set_thread_res_uid(uid, uid, uid));
    if let Err(e) = dropped {
        eprintln!("diskgc-e2e as-tester: dropping to uid {}: {e}", TESTER.uid);
        return exit::FAIL;
    }
    if rustix::process::geteuid() != uid || rustix::process::getuid() != uid {
        eprintln!(
            "diskgc-e2e as-tester: still not uid {} after setresuid",
            TESTER.uid
        );
        return exit::FAIL;
    }
    let error = Command::new(program).args(args).exec();
    eprintln!("diskgc-e2e as-tester: exec {program}: {error}");
    exit::FAIL
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ledger_names_every_run_the_oracle_did_not_judge() {
        let ledger = Ledger::new();
        assert!(ledger.require_all_judged().is_ok(), "no runs, nothing owed");
        let first = ledger.enter(&["/bin/diskgc".into(), "--version".into()]);
        let _second = ledger.enter(&["/bin/diskgc".into(), "survey".into()]);
        first.store(true, Ordering::SeqCst);
        assert_eq!(
            ledger.unjudged(),
            vec![vec!["/bin/diskgc".to_owned(), "survey".to_owned()]]
        );
        let err = ledger.require_all_judged().unwrap_err();
        assert!(err.contains("never judged by the deletion oracle"), "{err}");
        assert!(err.contains("survey"), "{err}");
    }

    #[test]
    fn a_refused_seccomp_filter_is_detected_in_strace_stderr() {
        let stderr = "\
strace: --seccomp-bpf cannot be used without -f/--follow-forks, disabling
diskgc: seccomp in a program's own message is not strace's
";
        assert_eq!(
            seccomp_refusals(stderr),
            vec!["strace: --seccomp-bpf cannot be used without -f/--follow-forks, disabling"]
        );
        assert!(seccomp_refusals("diskgc 0.1.0\n").is_empty());
    }
}
