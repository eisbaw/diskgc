//! Fixture processes for the in-use container tests ([`crate::inuse_suite`]):
//! stand-ins for programs using files, started by the harness (untraced,
//! as `tester` through `as-tester` or `exec-as`, or as root where they set
//! up a namespace first). Not code under test. Each prints `ready` once its
//! state holds and lives until its stdin closes.

use std::io::{BufRead, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;

use rustix::process::{Gid, Uid};

/// Prints `ready` and blocks until stdin is closed.
fn ready_then_wait() -> Result<(), String> {
    say("ready")?;
    wait_for_eof()
}

fn say(line: &str) -> Result<(), String> {
    let mut out = std::io::stdout();
    writeln!(out, "{line}").map_err(|e| format!("write stdout: {e}"))?;
    out.flush().map_err(|e| format!("flush stdout: {e}"))
}

fn wait_for_eof() -> Result<(), String> {
    let mut line = String::new();
    while std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("read stdin: {e}"))?
        > 0
    {
        line.clear();
    }
    Ok(())
}

/// `hold-map FILE`: maps FILE (read-only, shared) and closes its fd, so
/// only the mapping uses it.
pub fn hold_map(file: &Path) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    let f = std::fs::File::open(file).map_err(|e| format!("open {}: {e}", file.display()))?;
    let len = f
        .metadata()
        .map_err(|e| format!("stat {}: {e}", file.display()))?
        .len()
        .max(1);
    let len = usize::try_from(len).map_err(|_| "file too large to map".to_owned())?;
    // SAFETY: a fresh read-only shared mapping of a file this process
    // opened, never dereferenced, kept until the process exits.
    let at = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            f.as_raw_fd(),
            0,
        )
    };
    if at == libc::MAP_FAILED {
        return Err(format!(
            "mmap {}: {}",
            file.display(),
            std::io::Error::last_os_error()
        ));
    }
    drop(f);
    std::env::set_current_dir("/").map_err(|e| format!("chdir /: {e}"))?;
    ready_then_wait()
}

/// `hold-export VAR VALUE`: sets VAR=VALUE in its own environment after it
/// started (what a shell's `export` does), so the initial environment in
/// `/proc/<pid>/environ` does not show it.
pub fn hold_export(var: &str, value: &str) -> Result<(), String> {
    // SAFETY: single-threaded at this point; nothing reads the environment
    // concurrently.
    unsafe { std::env::set_var(var, value) };
    std::env::set_current_dir("/").map_err(|e| format!("chdir /: {e}"))?;
    ready_then_wait()
}

/// `hold-fork FILE`: holds FILE open; on a line from stdin, forks a child
/// that inherits the fd, closes its own and prints `forked <child pid>`;
/// both live until stdin closes (the child then exits too).
pub fn hold_fork(file: &Path) -> Result<(), String> {
    let held = std::fs::File::open(file).map_err(|e| format!("open {}: {e}", file.display()))?;
    std::env::set_current_dir("/").map_err(|e| format!("chdir /: {e}"))?;
    say("ready")?;
    let mut go = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut go)
        .map_err(|e| format!("read stdin: {e}"))?;
    // SAFETY: this process is single-threaded; the child only reads stdin
    // and exits.
    let child = unsafe { libc::fork() };
    match child {
        -1 => Err(format!("fork: {}", std::io::Error::last_os_error())),
        0 => {
            // The child: keeps `held` until stdin closes.
            let _ = wait_for_eof();
            drop(held);
            std::process::exit(crate::exit::PASS);
        }
        pid => {
            drop(held);
            say(&format!("forked {pid}"))?;
            wait_for_eof()?;
            let mut status = 0;
            // SAFETY: waits for this process's own child.
            unsafe { libc::waitpid(pid, &mut status, 0) };
            Ok(())
        }
    }
}

/// `hold-fifo FIFO`: holds the FIFO open (read-write, which does not block
/// waiting for a peer).
pub fn hold_fifo(fifo: &Path) -> Result<(), String> {
    let _held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fifo)
        .map_err(|e| format!("open {}: {e}", fifo.display()))?;
    std::env::set_current_dir("/").map_err(|e| format!("chdir /: {e}"))?;
    ready_then_wait()
}

/// `bind-socket PATH`: binds a listening AF_UNIX socket at PATH (creating
/// the socket file) and keeps it.
pub fn bind_socket(path: &Path) -> Result<(), String> {
    let _listener = std::os::unix::net::UnixListener::bind(path)
        .map_err(|e| format!("bind {}: {e}", path.display()))?;
    std::env::set_current_dir("/").map_err(|e| format!("chdir /: {e}"))?;
    ready_then_wait()
}

/// `hold-nondumpable FILE CWD`: marks itself non-dumpable
/// (`prctl(PR_SET_DUMPABLE, 0)`, as `ssh-agent` does), then holds FILE
/// open with CWD as working directory.
pub fn hold_nondumpable(file: &Path, cwd: &Path) -> Result<(), String> {
    // SAFETY: prctl with integer arguments only.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(format!(
            "prctl(PR_SET_DUMPABLE, 0): {}",
            std::io::Error::last_os_error()
        ));
    }
    crate::host_probe::hold(file, cwd)
}

/// `exec-as RUID EUID GID -- PROGRAM ARGS...` (as container root): sets the
/// groups to GID, every gid to GID, the real uid to RUID and the effective
/// and saved uid to EUID, then execs PROGRAM. With RUID = EUID the exec
/// leaves it dumpable; with RUID != EUID it is non-dumpable afterwards, like
/// a setuid program.
pub fn exec_as(ruid: &str, euid: &str, gid: &str, argv: &[String]) -> Result<(), String> {
    let num = |s: &str| s.parse::<u32>().map_err(|_| format!("bad id {s:?}"));
    let (ruid, euid, gid) = (num(ruid)?, num(euid)?, num(gid)?);
    let [program, args @ ..] = argv else {
        return Err("exec-as: no program given".to_owned());
    };
    let g = Gid::from_raw(gid);
    rustix::thread::set_thread_groups(&[g])
        .and_then(|()| rustix::thread::set_thread_res_gid(g, g, g))
        .and_then(|()| {
            rustix::thread::set_thread_res_uid(
                Uid::from_raw(ruid),
                Uid::from_raw(euid),
                Uid::from_raw(euid),
            )
        })
        .map_err(|e| format!("set ids {ruid}/{euid}/{gid}: {e}"))?;
    let error = std::process::Command::new(program).args(args).exec();
    Err(format!("exec {program}: {error}"))
}

/// `ns-hold SRC DST FILE` (as container root): takes a mount namespace of
/// its own, bind-mounts SRC on DST there, then runs `probe hold DST/FILE
/// DST` as tester: a process using the scratch objects under a path that
/// does not exist in diskgc's mount namespace.
pub fn ns_hold(src: &Path, dst: &Path, file: &str) -> Result<(), String> {
    use rustix::mount::{MountPropagationFlags, mount_bind, mount_change};
    // SAFETY: a new mount namespace for this single-threaded process.
    unsafe { rustix::thread::unshare_unsafe(rustix::thread::UnshareFlags::NEWNS) }
        .map_err(|e| format!("unshare the mount namespace: {e}"))?;
    mount_change(
        "/",
        MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
    )
    .map_err(|e| format!("make the mounts private: {e}"))?;
    mount_bind(src, dst)
        .map_err(|e| format!("bind {} on {}: {e}", src.display(), dst.display()))?;
    let me = std::env::current_exe().map_err(|e| format!("own executable: {e}"))?;
    let tester = crate::TESTER;
    let argv: Vec<String> = [
        me.display().to_string(),
        "probe".into(),
        "hold".into(),
        dst.join(file).display().to_string(),
        dst.display().to_string(),
    ]
    .into();
    exec_as(
        &tester.uid.to_string(),
        &tester.uid.to_string(),
        &tester.gid.to_string(),
        &argv,
    )
}

/// `hold-userns FILE` (as tester): takes a user namespace of its own, in
/// which it is root (`unshare -r`: uid and gid 0 mapped to its own), then
/// holds FILE open. It holds every capability in that namespace, but the
/// namespace belongs to tester, so tester may read it.
pub fn hold_userns(file: &Path) -> Result<(), String> {
    let (uid, gid) = (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    );
    // SAFETY: a new user namespace for this single-threaded process.
    unsafe { rustix::thread::unshare_unsafe(rustix::thread::UnshareFlags::NEWUSER) }
        .map_err(|e| format!("unshare the user namespace: {e}"))?;
    let write = |name: &str, text: String| {
        std::fs::write(format!("/proc/self/{name}"), text)
            .map_err(|e| format!("write /proc/self/{name}: {e}"))
    };
    write("uid_map", format!("0 {uid} 1"))?;
    write("setgroups", "deny".to_owned())?;
    write("gid_map", format!("0 {gid} 1"))?;
    crate::host_probe::hold(file, Path::new("/"))
}

/// `hold-capable FILE` (as container root): becomes tester (every uid and
/// gid) while keeping one permitted capability (`CAP_KILL`), makes itself
/// dumpable again, then holds FILE open: a process of tester's that the
/// kernel refuses to show to a tester without that capability.
pub fn hold_capable(file: &Path) -> Result<(), String> {
    use rustix::thread::{CapabilitySet, CapabilitySets};
    let tester = crate::TESTER;
    let (u, g) = (Uid::from_raw(tester.uid), Gid::from_raw(tester.gid));
    rustix::thread::set_keep_capabilities(true).map_err(|e| format!("keepcaps: {e}"))?;
    rustix::thread::set_thread_groups(&[g])
        .and_then(|()| rustix::thread::set_thread_res_gid(g, g, g))
        .and_then(|()| rustix::thread::set_thread_res_uid(u, u, u))
        .map_err(|e| format!("become tester: {e}"))?;
    rustix::thread::set_capabilities(
        None,
        CapabilitySets {
            effective: CapabilitySet::empty(),
            permitted: CapabilitySet::KILL,
            inheritable: CapabilitySet::empty(),
        },
    )
    .map_err(|e| format!("keep only CAP_KILL permitted: {e}"))?;
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::Dumpable)
        .map_err(|e| format!("prctl(PR_SET_DUMPABLE, 1): {e}"))?;
    crate::host_probe::hold(file, Path::new("/"))
}
