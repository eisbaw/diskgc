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
//! [`run_as_tester`] waits for the run to end. [`spawn_as_tester`] returns
//! at once with a [`Spawned`], for runs the harness must pause at a chosen
//! port call, change the fixture while they wait, resume, or see killed
//! there (the test driver and its controlled `Host` wrapper,
//! [`crate::controlled`]); both are the same traced run underneath.
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
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rustix::process::{Gid, Uid};

use crate::controlled::ControlLine;
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
    /// The signal that ended strace, which ends itself with the signal
    /// that ended the code under test (a controlled kill: `SIGKILL`).
    pub signal: Option<i32>,
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
                "{:?} exited with {:?} (signal {:?}), expected {code}\n--- stdout\n{}--- \
                 stderr\n{}",
                self.argv, self.code, self.signal, self.stdout, self.stderr
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
/// enters the run in `ledger`. Its stdin is at end of file.
pub fn run_as_tester<S: AsRef<OsStr>>(
    ledger: &Ledger,
    records: &Scratch,
    cwd: &Path,
    program: &Path,
    args: &[S],
) -> Result<Run, String> {
    spawn_as_tester(ledger, records, cwd, program, args)?.wait()
}

/// The environment of code under test run as `tester`: a minimal fixed set
/// (no variable of the harness's own environment leaks through). Callers
/// that need more (a Cargo under test's `PATH`) add to it, later entries
/// overriding earlier ones.
pub const TESTER_ENV: &[(&str, &str)] = &[
    ("PATH", "/bin"),
    ("HOME", "/home/tester"),
    ("USER", TESTER_NAME),
    ("LOGNAME", TESTER_NAME),
    ("LANG", "C.UTF-8"),
];

/// [`run_as_tester`] with `env` added to [`TESTER_ENV`].
pub fn run_as_tester_with_env<S: AsRef<OsStr>>(
    ledger: &Ledger,
    records: &Scratch,
    cwd: &Path,
    env: &[(String, String)],
    program: &Path,
    args: &[S],
) -> Result<Run, String> {
    spawn_as_tester_with_env(ledger, records, cwd, env, program, args)?.wait()
}

/// How long [`Spawned`] waits for the code under test to print a control
/// line or answer a check. Generous: a loaded machine runs many containers.
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(60);

/// x86_64 `read(2)`: the syscall a paused driver is blocked in, as the
/// first field of `/proc/<pid>/syscall` (the flake builds x86_64 only).
const SYS_READ: &str = "0";

/// A traced run of code under test that is still going, from
/// [`spawn_as_tester`]: the harness reads its stdout line by line, and can
/// wait until it pauses at a controlled point ([`Spawned::wait_paused`]),
/// resume it ([`Spawned::resume`]) and collect the finished [`Run`]
/// ([`Spawned::wait`]), which the oracle must judge like any other.
///
/// Its stdin is a pipe the harness holds: the controlled `Host` wrapper
/// (crate::controlled) blocks reading it at a pause point, and a line
/// written to it resumes the code under test. Closing it (as `wait` does
/// first) makes a paused driver exit with a failure rather than carry on.
pub struct Spawned {
    argv: Vec<String>,
    strace: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    stdout: JoinHandle<String>,
    stderr: JoinHandle<String>,
    record_dir: PathBuf,
    started: Instant,
    judged: Arc<AtomicBool>,
}

/// Where the code under test paused, as its control line says, after the
/// harness checked that the process really is blocked there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paused {
    /// The port call it paused before, from 1.
    pub call: u64,
    pub op: String,
    /// The call's position among calls of `op`, from 1.
    pub nth: u64,
    pub pid: u32,
}

/// Prefix of strace's per-process record files (`-ff` adds `.PID`).
const RECORD_PREFIX: &str = "trace";

/// Starts `program args...` as `tester` under strace (as [`run_as_tester`]
/// does) and returns at once. The run is entered in `ledger` and its start
/// time recorded before anything is spawned.
pub fn spawn_as_tester<S: AsRef<OsStr>>(
    ledger: &Ledger,
    records: &Scratch,
    cwd: &Path,
    program: &Path,
    args: &[S],
) -> Result<Spawned, String> {
    spawn_as_tester_with_env(ledger, records, cwd, &[], program, args)
}

/// [`spawn_as_tester`] with `env` added to [`TESTER_ENV`].
pub fn spawn_as_tester_with_env<S: AsRef<OsStr>>(
    ledger: &Ledger,
    records: &Scratch,
    cwd: &Path,
    env: &[(String, String)],
    program: &Path,
    args: &[S],
) -> Result<Spawned, String> {
    let n = RUN_COUNTER.fetch_add(1, Ordering::Relaxed);
    let record_dir: PathBuf = records.mkdir(&format!("strace-{n}"), 0o700, crate::ROOT)?;
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
    let mut strace = Command::new(STRACE)
        .args(STRACE_OPTIONS)
        .arg("-e")
        .arg(format!("trace={}", strace::TRACED_SYSCALLS))
        .arg("-o")
        .arg(record_dir.join(RECORD_PREFIX))
        .arg("--")
        .arg(&self_exe)
        .args(["as-tester", "--"])
        .arg(program)
        .args(args)
        .env_clear()
        .envs(TESTER_ENV.iter().copied())
        .envs(env.iter().map(|(k, v)| (k, v)))
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn {STRACE}: {e}"))?;
    let stdin = strace.stdin.take();
    let out = strace.stdout.take().ok_or("strace has no stdout pipe")?;
    let err = strace.stderr.take().ok_or("strace has no stderr pipe")?;
    let (sender, lines) = mpsc::channel();
    let stdout = std::thread::spawn(move || {
        let mut reader = BufReader::new(out);
        let mut all = String::new();
        let mut raw = Vec::new();
        loop {
            raw.clear();
            match reader.read_until(b'\n', &mut raw) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    all.push_str(&format!("[harness: reading stdout failed: {e}]\n"));
                    break;
                }
            }
            let line = String::from_utf8_lossy(&raw);
            let line = line.strip_suffix('\n').unwrap_or(&line).to_owned();
            all.push_str(&line);
            all.push('\n');
            // The receiver may be gone (nobody waits for lines); the text
            // is kept in `all` either way.
            let _ = sender.send(line);
        }
        all
    });
    let stderr = std::thread::spawn(move || {
        let mut all = Vec::new();
        let _ = BufReader::new(err).read_to_end(&mut all);
        String::from_utf8_lossy(&all).into_owned()
    });
    Ok(Spawned {
        argv,
        strace,
        stdin,
        lines,
        stdout,
        stderr,
        record_dir,
        started,
        judged,
    })
}

impl Spawned {
    /// The pid of strace, the parent of the code under test.
    fn strace_pid(&self) -> u32 {
        self.strace.id()
    }

    /// The next control line on stdout (result lines are skipped; they stay
    /// in the run's stdout), within [`CONTROL_TIMEOUT`].
    fn next_control(&self, waiting_for: &str) -> Result<ControlLine, String> {
        let deadline = Instant::now() + CONTROL_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    if let Some(control) = ControlLine::parse(&line)? {
                        return Ok(control);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(format!(
                        "{:?}: no {waiting_for} line within {CONTROL_TIMEOUT:?}",
                        self.argv
                    ));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(format!(
                        "{:?} closed its stdout before printing a {waiting_for} line",
                        self.argv
                    ));
                }
            }
        }
    }

    /// Waits until the code under test pauses at a controlled point and
    /// checks that it is blocked there: its parent is this run's strace, it
    /// sleeps (`S`), and it is inside `read(0, ...)` on the stdin pipe
    /// (`/proc/<pid>/syscall`), so nothing after the paused call can run
    /// until [`Spawned::resume`].
    pub fn wait_paused(&mut self) -> Result<Paused, String> {
        let paused = match self.next_control("pause")? {
            ControlLine::Pause { call, op, nth, pid } => Paused { call, op, nth, pid },
            other => return Err(format!("{:?}: expected a pause, got {other}", self.argv)),
        };
        let not_blocked = |e: String| {
            format!(
                "pid {} printed a pause before port call {} but is not blocked reading its \
                 stdin: {e}",
                paused.pid, paused.call
            )
        };
        let parent = proc_field(paused.pid, "status", |text| {
            text.lines()
                .find_map(|l| l.strip_prefix("PPid:"))
                .map(|v| v.trim().to_owned())
        })
        .map_err(not_blocked)?;
        if parent != self.strace_pid().to_string() {
            return Err(format!(
                "pid {} named by the pause line has parent {parent}, not this run's strace ({})",
                paused.pid,
                self.strace_pid()
            ));
        }
        let deadline = Instant::now() + CONTROL_TIMEOUT;
        loop {
            let state = proc_field(paused.pid, "stat", |text| {
                text.rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().next())
                    .map(str::to_owned)
            })
            .map_err(not_blocked)?;
            let syscall = proc_field(paused.pid, "syscall", |text| {
                let mut fields = text.split_whitespace();
                Some(format!(
                    "{} {}",
                    fields.next().unwrap_or(""),
                    fields.next().unwrap_or("")
                ))
            })
            .map_err(not_blocked)?;
            if state == "S" && syscall == format!("{SYS_READ} 0x0") {
                info!(
                    "{:?} paused before port call {} ({} #{}): pid {} blocked in read(0)",
                    self.argv, paused.call, paused.op, paused.nth, paused.pid
                );
                return Ok(paused);
            }
            if Instant::now() >= deadline {
                return Err(not_blocked(format!(
                    "after {CONTROL_TIMEOUT:?} it is in state {state}, syscall {syscall:?}"
                )));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Resumes code under test paused at `paused` and waits until it says
    /// so (its next step is the paused call).
    pub fn resume(&mut self, paused: &Paused) -> Result<(), String> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or("the code under test's stdin is already closed")?;
        stdin
            .write_all(b"resume\n")
            .and_then(|()| stdin.flush())
            .map_err(|e| format!("write to {:?}'s stdin: {e}", self.argv))?;
        match self.next_control("resume")? {
            ControlLine::Resume { call } if call == paused.call => Ok(()),
            other => Err(format!(
                "{:?}: expected the resume of call {}, got {other}",
                self.argv, paused.call
            )),
        }
    }

    /// Closes stdin, waits for strace (and so the code under test) to exit,
    /// and collects the record. A run that ended by a signal (a controlled
    /// kill) has `code` `None` and the signal in `signal`.
    pub fn wait(mut self) -> Result<Run, String> {
        drop(self.stdin.take());
        let status = self
            .strace
            .wait()
            .map_err(|e| format!("wait for {STRACE}: {e}"))?;
        let took = self.started.elapsed();
        let stdout = self
            .stdout
            .join()
            .map_err(|_| "the stdout reader panicked".to_owned())?;
        let stderr = self
            .stderr
            .join()
            .map_err(|_| "the stderr reader panicked".to_owned())?;
        let argv = self.argv;
        let refusals = seccomp_refusals(&stderr);
        if !refusals.is_empty() {
            return Err(format!(
                "strace ran {argv:?} without its seccomp filter (every syscall would stop the \
                 tracee): {refusals:?}"
            ));
        }
        let events = strace::read_record(&self.record_dir, RECORD_PREFIX)
            .map_err(|e| format!("{e}\n--- strace exit {status:?}, stderr\n{stderr}"))?;
        Ok(Run {
            argv,
            code: status.code(),
            signal: status.signal(),
            stdout,
            stderr,
            events,
            started: self.started,
            took,
            judged: self.judged,
        })
    }
}

/// Reads `/proc/<pid>/<file>` (as container root, with CAP_SYS_PTRACE) and
/// extracts a value from it.
fn proc_field(
    pid: u32,
    file: &str,
    extract: impl FnOnce(&str) -> Option<String>,
) -> Result<String, String> {
    let path = format!("/proc/{pid}/{file}");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("read {path}: {e}"))?;
    extract(&text).ok_or_else(|| format!("unexpected {path}: {text:?}"))
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

/// `personality(2)` flag that makes `uname` report a 2.6 release
/// (`UNAME26` in linux/personality.h). Podman's default seccomp profile
/// allows exactly this value.
const UNAME26: libc::c_ulong = 0x0020000;

/// `diskgc-e2e uname-2.6 -- PROGRAM ARGS...`: sets the `UNAME26`
/// personality, which survives `exec`, and execs PROGRAM. The kernel then
/// reports release `2.6.<60 + minor>` to it: an old kernel, simulated by the
/// kernel, with no hook in the code under test. Returns only on failure.
pub fn exec_with_uname26(argv: &[String]) -> i32 {
    let [program, args @ ..] = argv else {
        eprintln!("diskgc-e2e uname-2.6: no program given");
        return exit::USAGE;
    };
    // SAFETY: personality(2) takes a plain integer and touches no memory.
    let previous = unsafe { libc::personality(UNAME26) };
    if previous == -1 {
        eprintln!(
            "diskgc-e2e uname-2.6: personality(UNAME26): {}",
            std::io::Error::last_os_error()
        );
        return exit::FAIL;
    }
    let error = Command::new(program).args(args).exec();
    eprintln!("diskgc-e2e uname-2.6: exec {program}: {error}");
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
