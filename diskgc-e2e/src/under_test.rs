//! Starting the code under test: as the unprivileged `tester` (uid 1000),
//! traced by strace (docs/testing.md, Container setup).
//!
//! strace runs as container root and starts `diskgc-e2e as-tester -- PROGRAM
//! ARGS`, which is traced from its first instruction, drops to tester
//! ([`exec_as_tester`]) and execs the program. The trace files go to the
//! root-only records mount, where the code under test cannot touch them.
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
use std::sync::atomic::{AtomicUsize, Ordering};

use rustix::process::{Gid, Uid};

use crate::scratch::Scratch;
use crate::strace::{self, Event};
use crate::{TESTER, TESTER_NAME, exit, info};

/// strace in the base image.
pub const STRACE: &str = "/bin/strace";

/// What a traced run of the code under test produced.
#[derive(Debug)]
pub struct Run {
    pub argv: Vec<String>,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub events: Vec<Event>,
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
}

static RUN_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Runs `program args...` as `tester` under strace, with a minimal fixed
/// environment and `cwd` as working directory, and collects the record.
pub fn run_as_tester<S: AsRef<OsStr>>(
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

    let output = Command::new(STRACE)
        .args(["-f", "-ff", "-qq", "-y", "-xx"])
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

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let events = strace::read_record(&dir, prefix)
        .map_err(|e| format!("{e}\n--- strace exit {:?}, stderr\n{stderr}", output.status))?;
    Ok(Run {
        argv,
        code: output.status.code(),
        stdout,
        stderr,
        events,
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
